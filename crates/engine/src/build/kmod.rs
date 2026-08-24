//! Out-of-tree kernel-module build stage: for each kmod the board named, fetch the
//! pinned driver repo, apply its own quilt (plus any boot2deb-authored compat
//! patches), build the module against the freshly built kernel tree with `make M=`, and
//! package the resulting `.ko`s as a `<name>-modules-<kver>` `.deb` that installs them
//! into `/lib/modules/<kver>/updates/` and runs `depmod` on configure.
//!
//! This is the kernel-side analogue of the userspace-from-git nodes
//! ([`build_userspace`](crate::build::userspace::build_userspace)): the source is a
//! pinned git repo we fetch, not a series we author, and the compat patch is the tracked
//! fork's own quilt — updates are a pin bump. Unlike the userspace node it builds
//! host-side with the kernel's cross toolchain (not the target-arch rootless sandbox),
//! because a module links against the kernel's `Module.symvers` and must match its
//! vermagic. Engine side effects (git, make, dpkg-deb) live here; the pins it reads are
//! resolved in [`crate::pins`].

use crate::build::{self, BuildEnv};
use crate::error::EngineError;
use crate::event::{EventSink, Step};
use crate::signature::{Signature, SignatureBuilder, SignatureManifest};
use boot2deb_core::lock::{KmodPin, Lock};
use boot2deb_core::model::{ResolvedKmod, KmodFirmware};
use boot2deb_core::ResolvedBuild;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Stage-recipe version for a kmod **tree** signature (Tier-1 fetched+patched tree):
/// bump when the fetch/patch logic that shapes a reused driver tree changes.
const TREE_STAGE_VERSION: u32 = 1;

/// Stage-recipe version for a kmod **output** signature (Tier-2 `.deb`): bump when the
/// build/package logic changes the produced `.deb` in a way the folded inputs do not
/// already capture.
const OUTPUT_STAGE_VERSION: u32 = 1;

/// Stage-recipe version for a kmod **firmware** signature (Tier-2 `<name>-firmware.deb`):
/// bump when the firmware collect/package logic changes the produced `.deb`.
const FIRMWARE_STAGE_VERSION: u32 = 1;

/// Filesystem inputs for the kmod stage. The resolved build carries the descriptors
/// (`subdir`/patches/`make_args`/modules) and the lock the git pins; these are the
/// on-disk locations plus the resolved local-patch paths the CLI expanded.
pub struct KmodOptions<'a> {
    /// The kernel stage's options — reused to reach (and, if a `--stage kmod` run or a
    /// kernel cache hit left no tree, rebuild) the built kernel tree via
    /// [`kernel::ensure_module_tree`](crate::build::kernel::ensure_module_tree).
    pub kernel: &'a crate::build::kernel::KernelOptions<'a>,
    /// Per-name clone-source overrides (`(name, url-or-path)`) for co-development; a
    /// name absent here uses that kmod's locked `source`.
    pub sources: &'a [(String, String)],
    /// Per-name resolved local-patch paths (`(name, [abs path, …])`) — each kmod's
    /// `local_patches`, expanded from bare filenames to absolute paths by the CLI, in
    /// apply order. A name absent here applies no local patch.
    pub local_patches: &'a [(String, Vec<PathBuf>)],
    /// Scratch dir; each fetched+patched driver tree is `<work>/kmod/<name>` and the
    /// packaging stage lives beside it.
    pub work_dir: &'a Path,
    /// Directory the produced `.deb`s are staged into.
    pub out_dir: &'a Path,
    /// Root of the Tier-2 artifact store, or `None` to disable output caching. A hit
    /// restores the `.deb` without touching the kernel tree; a miss builds and stores it.
    pub store: Option<&'a Path>,
}

/// The kmod `.deb`s produced by [`build_kmods`], in declared order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KmodArtifacts {
    /// One `<name>-modules-<kver>` `.deb` per built device kmod.
    pub debs: Vec<PathBuf>,
}

/// Run the kmod stage, emitting its [`Event`](crate::event::Event)s to `sink`.
///
/// Reads the [`Lock`] for the `[[kmods]]` pins and the [`ResolvedBuild`] for the module
/// descriptors. Each kmod is Tier-2-cached on `(kernel tree, driver tree, kver, arch,
/// toolchain, module list, make args)`, so a no-change rebuild restores the `.deb`
/// without rebuilding either tree.
pub fn build_kmods(
    build: &ResolvedBuild,
    lock: &Lock,
    opts: &KmodOptions,
    env: &BuildEnv,
    sink: &dyn EventSink,
) -> Result<KmodArtifacts, EngineError> {
    let step = Step::start(sink, "kmod");
    let stage_root = opts.work_dir.join("kmod");

    // The kernel tree signature is folded into every kmod's output key: a kernel commit
    // or patch bump changes module vermagic, so a cached modules `.deb` must not survive
    // it. Computed from the lock (no tree needed), mirroring how ffmpeg folds its deps.
    let kernel_sig = kernel_tree_signature(lock, opts)?;

    let mut debs = Vec::new();
    for (idx, k) in build.device_kmods.iter().enumerate() {
        let pin = lock
            .kmods
            .iter()
            .find(|p| p.name == k.name)
            .ok_or_else(|| EngineError::ArtifactMissing {
                what: format!("lock pin for kmod '{}' (run `boot2deb update`)", k.name),
                location: "[[kmods]]".into(),
            })?;
        let locals = local_patches_for(opts, &k.name);
        // One kmod yields the per-kernel modules deb and, when it ships firmware, a
        // companion `<name>-firmware` deb.
        let produced = build_one(build, lock, opts, env, &stage_root, k, pin, locals, &kernel_sig, &step)?;
        debs.extend(produced);
        step.progress((100 * (idx + 1) / build.device_kmods.len().max(1)) as u8);
    }
    step.finish();
    Ok(KmodArtifacts { debs })
}

/// Build and package one device kmod, restoring from the Tier-2 cache on a hit. Returns
/// the per-kernel modules `.deb` and, when the kmod ships firmware, the companion
/// `<name>-firmware` `.deb` after it (declared install order).
#[allow(clippy::too_many_arguments)]
fn build_one(
    build: &ResolvedBuild,
    lock: &Lock,
    opts: &KmodOptions,
    env: &BuildEnv,
    stage_root: &Path,
    k: &ResolvedKmod,
    pin: &KmodPin,
    local_patches: &[PathBuf],
    kernel_sig: &Signature,
    step: &Step,
) -> Result<Vec<PathBuf>, EngineError> {
    // A local patch's *content* shapes both the module and the firmware bytes copied
    // from the patched tree, so fold each file's fingerprint into every signature — an
    // edited shim must restamp rather than reuse a stale build.
    let local_fps = local_patch_fingerprints(local_patches)?;

    // Tier-2 probes first, each keyed without a tree: the modules deb via the kernel
    // tree signature (which embeds the kver), the firmware deb via the driver commit.
    // Restoring is the whole point of not materializing a tree, so probe both up front.
    let mod_node = node_name(&k.name);
    let mod_man = output_manifest(k, pin, &local_fps, kernel_sig, build.arch.debian_arch(), &env.toolchain_id);
    let mod_cached = build::restore_stage_outputs(opts.store, &mod_node, &mod_man.signature(), opts.out_dir, &["deb"], step)?;

    let fw_node = firmware_node_name(&k.name);
    let fw_man = k.firmware.as_ref().map(|f| firmware_output_manifest(k, f, pin, &local_fps));
    let fw_cached = match &fw_man {
        Some(m) => build::restore_stage_outputs(opts.store, &fw_node, &m.signature(), opts.out_dir, &["deb"], step)?,
        None => None,
    };

    let need_modules = mod_cached.is_none();
    let need_firmware = k.firmware.is_some() && fw_cached.is_none();

    // Everything cached: return the restored debs without touching any tree.
    if !need_modules && !need_firmware {
        let mut debs = vec![mod_cached.expect("modules cache hit")[0].clone()];
        if let Some(hit) = fw_cached {
            debs.push(hit[0].clone());
        }
        return Ok(debs);
    }

    // At least one deb must be built, and both the module and the firmware come from the
    // fetched+patched driver tree — so materialize it once here (reused when the commit +
    // patch set are unchanged). The kernel tree is materialized only for a modules build.
    let driver_tree = stage_root.join(&k.name);
    let source = source_for(opts, &k.name).unwrap_or(&pin.source);
    let tree_man = tree_signature_manifest(k, pin, &local_fps);
    build::reuse_or_refresh_tree(&driver_tree, &tree_man, &format!("kmod {}", k.name), step, || {
        fetch_and_patch(source, pin, k, local_patches, &driver_tree, step)
    })?;
    let epoch = crate::git::commit_epoch(&driver_tree, &pin.commit).ok();

    let mut debs = Vec::new();

    // The modules deb: build against the kernel tree, or restore the cache hit.
    if need_modules {
        let ktree = crate::build::kernel::ensure_module_tree(build, lock, opts.kernel, env, step)?;
        let kver = kernel_release(&ktree)?;
        let subdir = driver_tree.join(&k.subdir);
        compile_module(build, env, &ktree, &subdir, k, epoch, step)?;
        let pkg_stage = stage_root.join(format!("{}-pkg", k.name));
        let _ = std::fs::remove_dir_all(&pkg_stage);
        install_modules(build, env, &ktree, &subdir, k, &kver, &pkg_stage, step)?;
        let deb = package_deb(k, pin, &kver, build.arch.debian_arch(), stage_root, &pkg_stage, epoch, step)?;
        let staged = build::stage_artifact(opts.out_dir, &deb)?;
        build::store_stage_outputs(opts.store, &mod_node, &mod_man.signature(), &[("deb", staged.as_path())], step)?;
        step.log(format!("staged {}", staged.file_name().and_then(|n| n.to_str()).unwrap_or("kmod deb")));
        debs.push(staged);
    } else {
        debs.push(mod_cached.expect("modules cache hit")[0].clone());
    }

    // The firmware deb: package from the driver tree, or restore the cache hit.
    if let (Some(fw), Some(man)) = (k.firmware.as_ref(), &fw_man) {
        if need_firmware {
            let deb = package_firmware_deb(k, fw, pin, &driver_tree, stage_root, epoch, step)?;
            let staged = build::stage_artifact(opts.out_dir, &deb)?;
            build::store_stage_outputs(opts.store, &fw_node, &man.signature(), &[("deb", staged.as_path())], step)?;
            step.log(format!("staged {}", staged.file_name().and_then(|n| n.to_str()).unwrap_or("firmware deb")));
            debs.push(staged);
        } else {
            debs.push(fw_cached.expect("firmware cache hit")[0].clone());
        }
    }

    Ok(debs)
}

/// Fetch the driver at its pinned commit and apply the in-repo quilt then the local
/// compat patches, all with `git apply -p1` (the patches are unified diffs, not `git am`
/// mbox). A patch that will not apply is a hard error naming it — never a silent skip,
/// which would ship an unpatched driver.
fn fetch_and_patch(
    source: &str,
    pin: &KmodPin,
    k: &ResolvedKmod,
    local_patches: &[PathBuf],
    tree: &Path,
    step: &Step,
) -> Result<(), EngineError> {
    build::fetch_commit(source, &pin.reference, &pin.commit, &format!("kmod {}", k.name), tree, step)?;
    for name in &k.repo_patches {
        let patch = tree.join(&k.patch_dir).join(name);
        git_apply(tree, &patch, step)?;
    }
    for patch in local_patches {
        git_apply(tree, patch, step)?;
    }
    Ok(())
}

/// `git -C <tree> apply -p1 <patch>`, erroring with the patch path on failure.
fn git_apply(tree: &Path, patch: &Path, step: &Step) -> Result<(), EngineError> {
    if !patch.exists() {
        return Err(EngineError::ArtifactMissing {
            what: format!("kmod patch {}", patch.display()),
            location: tree.display().to_string(),
        });
    }
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(tree).args(["apply", "-p1", "--verbose"]).arg(patch);
    build::run(cmd, "git", &format!("git apply {}", patch.display()), step)
}

/// `make -C <ktree> M=<subdir> [make_args] modules` (after a `clean`), with the kernel's
/// `ARCH`/`SOURCE_DATE_EPOCH` and cross toolchain. The `make_args` (bare `KEY=VALUE`,
/// validated at resolve) are command-line overrides — how a board selects a build knob
/// like the direct-probe SDIO path or drops a module that will not build.
fn compile_module(
    build: &ResolvedBuild,
    env: &BuildEnv,
    ktree: &Path,
    subdir: &Path,
    k: &ResolvedKmod,
    epoch: Option<u64>,
    step: &Step,
) -> Result<(), EngineError> {
    // A clean first, so a make_args change (which kbuild does not track per-object) never
    // links stale objects into the module.
    let mut clean = Command::new("make");
    clean
        .arg("-C")
        .arg(ktree)
        .arg(format!("M={}", subdir.display()))
        .arg("clean");
    apply_kbuild_env(&mut clean, build, env, epoch);
    build::run(clean, "make", &format!("make M= clean ({})", k.name), step)?;

    let mut make = Command::new("make");
    make.arg("-C")
        .arg(ktree)
        .arg(format!("-j{}", env.jobs()))
        .arg(format!("M={}", subdir.display()));
    for arg in &k.make_args {
        make.arg(arg);
    }
    make.arg("modules");
    apply_kbuild_env(&mut make, build, env, epoch);
    build::run(make, "make", &format!("make M= modules ({})", k.name), step)
}

/// `make … modules_install` into `pkg_stage`, landing the `.ko`s under
/// `/lib/modules/<kver>/updates/`. `DEPMOD=/bin/true` skips the in-place depmod (the deb
/// runs the real one on the installed kernel); `INSTALL_MOD_STRIP=1` strips debug info.
/// Then prunes everything under `lib/modules/<kver>/` except `updates/` (the stray
/// `modules.order`/`modules.builtin` the install writes would shadow the kernel deb's),
/// and, when the device names a module set, verifies each is present and drops extras.
fn install_modules(
    build: &ResolvedBuild,
    env: &BuildEnv,
    ktree: &Path,
    subdir: &Path,
    k: &ResolvedKmod,
    kver: &str,
    pkg_stage: &Path,
    step: &Step,
) -> Result<(), EngineError> {
    std::fs::create_dir_all(pkg_stage).map_err(|s| EngineError::io(pkg_stage, s))?;
    let mut make = Command::new("make");
    make.arg("-C")
        .arg(ktree)
        .arg(format!("M={}", subdir.display()))
        .arg(format!("INSTALL_MOD_PATH={}", pkg_stage.display()))
        .arg("INSTALL_MOD_DIR=updates")
        .arg("INSTALL_MOD_STRIP=1")
        .arg("DEPMOD=/bin/true");
    for arg in &k.make_args {
        make.arg(arg);
    }
    make.arg("modules_install");
    apply_kbuild_env(&mut make, build, env, None);
    build::run(make, "make", &format!("make M= modules_install ({})", k.name), step)?;

    let mods_dir = pkg_stage.join(format!("lib/modules/{kver}"));
    let updates = mods_dir.join("updates");
    if !updates.exists() {
        return Err(EngineError::ArtifactMissing {
            what: format!("built modules for kmod '{}' under updates/", k.name),
            location: updates.display().to_string(),
        });
    }
    // Keep only `updates/` under `lib/modules/<kver>/`.
    for entry in std::fs::read_dir(&mods_dir).map_err(|s| EngineError::io(&mods_dir, s))? {
        let entry = entry.map_err(|s| EngineError::io(&mods_dir, s))?;
        if entry.file_name() != std::ffi::OsStr::new("updates") {
            let p = entry.path();
            if p.is_dir() {
                std::fs::remove_dir_all(&p).map_err(|s| EngineError::io(&p, s))?;
            } else {
                std::fs::remove_file(&p).map_err(|s| EngineError::io(&p, s))?;
            }
        }
    }
    prune_and_verify_modules(&updates, k, step)
}

/// When the device names a `modules` set, verify each named `.ko` was built and remove
/// any the build produced that are not named (so the deb ships exactly the intended set,
/// e.g. Wi-Fi without a Bluetooth helper that is out of scope). An empty set ships all.
///
/// `modules_install` preserves each module's build-relative subdirectory, so a module
/// lands at `updates/<subdir>/<name>.ko` (e.g. `updates/aic8800_bsp/aic8800_bsp.ko`),
/// not flat under `updates/`. The `.ko` set is therefore collected recursively, matched
/// by basename; pruning an unlisted module also removes any subdirectory it emptied.
fn prune_and_verify_modules(updates: &Path, k: &ResolvedKmod, step: &Step) -> Result<(), EngineError> {
    if k.modules.is_empty() {
        return Ok(());
    }
    let want: Vec<String> = k
        .modules
        .iter()
        .map(|m| m.strip_suffix(".ko").unwrap_or(m).to_string())
        .collect();
    let mut kos = Vec::new();
    collect_ko(updates, &mut kos)?;
    // Present modules, by basename stem.
    let mut present: Vec<String> = Vec::new();
    for ko in &kos {
        let stem = ko
            .file_stem()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        if want.iter().any(|w| w == &stem) {
            present.push(stem);
        } else {
            std::fs::remove_file(ko).map_err(|s| EngineError::io(ko, s))?;
            step.log(format!("dropped unlisted module {}", ko.display()));
        }
    }
    for w in &want {
        if !present.contains(w) {
            return Err(EngineError::ArtifactMissing {
                what: format!("module '{w}.ko' named in device_kmods '{}' but not built", k.name),
                location: updates.display().to_string(),
            });
        }
    }
    prune_empty_dirs(updates)?;
    Ok(())
}

/// Collect every `.ko` under `dir`, recursing into subdirectories (modules install into
/// their build-relative subdir, not flat).
fn collect_ko(dir: &Path, out: &mut Vec<PathBuf>) -> Result<(), EngineError> {
    for entry in std::fs::read_dir(dir).map_err(|s| EngineError::io(dir, s))? {
        let path = entry.map_err(|s| EngineError::io(dir, s))?.path();
        if path.is_dir() {
            collect_ko(&path, out)?;
        } else if path.extension().and_then(|e| e.to_str()) == Some("ko") {
            out.push(path);
        }
    }
    Ok(())
}

/// Remove now-empty subdirectories under `root` (but not `root` itself) left after
/// pruning unlisted modules, so the deb ships no empty module directories.
fn prune_empty_dirs(root: &Path) -> Result<(), EngineError> {
    for entry in std::fs::read_dir(root).map_err(|s| EngineError::io(root, s))? {
        let path = entry.map_err(|s| EngineError::io(root, s))?.path();
        if path.is_dir() {
            prune_empty_dirs(&path)?;
            if std::fs::read_dir(&path).map_err(|s| EngineError::io(&path, s))?.next().is_none() {
                std::fs::remove_dir(&path).map_err(|s| EngineError::io(&path, s))?;
            }
        }
    }
    Ok(())
}

/// Package the staged module tree as `<name>-modules-<kver>_<version>_<arch>.deb`: a
/// `Depends: linux-image-<kver>` so apt configures the kernel first, and `postinst`/
/// `postrm` that `depmod -a <kver>` so `modprobe` and SDIO modalias autoprobe resolve
/// the `updates/` module. `fakeroot dpkg-deb` clamps member mtimes to the driver
/// commit's date via `SOURCE_DATE_EPOCH`.
#[allow(clippy::too_many_arguments)]
fn package_deb(
    k: &ResolvedKmod,
    pin: &KmodPin,
    kver: &str,
    arch: &str,
    stage_root: &Path,
    pkg_stage: &Path,
    epoch: Option<u64>,
    step: &Step,
) -> Result<PathBuf, EngineError> {
    let pkg = package_name(&k.name, kver);
    let version = deb_version(pin);

    // Data modes uniform (dirs 0755, `.ko` 0644) so the host umask does not leak in.
    build::normalize_data_tree(pkg_stage)?;
    let debian = pkg_stage.join("DEBIAN");
    std::fs::create_dir_all(&debian).map_err(|s| EngineError::io(&debian, s))?;
    std::fs::write(debian.join("control"), control_text(&pkg, &version, arch, kver, &k.name))
        .map_err(|s| EngineError::io(&debian.join("control"), s))?;
    let postinst = debian.join("postinst");
    std::fs::write(&postinst, maintainer_script(kver)).map_err(|s| EngineError::io(&postinst, s))?;
    let postrm = debian.join("postrm");
    std::fs::write(&postrm, maintainer_script(kver)).map_err(|s| EngineError::io(&postrm, s))?;
    // `normalize_data_tree` refuses trees with maintainer scripts, so the DEBIAN dir is
    // moded explicitly here after it has run on the data tree.
    build::set_mode(&debian, 0o755)?;
    build::set_mode(&debian.join("control"), 0o644)?;
    build::set_mode(&postinst, 0o755)?;
    build::set_mode(&postrm, 0o755)?;

    let deb_name = format!("{pkg}_{version}_{arch}.deb");
    let deb_out = stage_root.join(&deb_name);
    let mut cmd = Command::new("fakeroot");
    cmd.args(["dpkg-deb", "--build"]).arg(pkg_stage).arg(&deb_out);
    if let Some(e) = epoch {
        cmd.env("SOURCE_DATE_EPOCH", e.to_string());
    }
    build::run(cmd, "fakeroot", &format!("dpkg-deb --build {pkg}"), step)?;
    Ok(deb_out)
}

/// Package the firmware the kmod ships as `<name>-firmware_<version>_all.deb`: an
/// `Architecture: all` package with **no** kernel dependency, so it is not tied to a
/// kernel release and does not collide with a second kernel's modules deb over the same
/// firmware path (the reason firmware is its own deb, not folded into the per-kver one).
/// All regular files directly under `<driver_tree>/<subdir>` are staged at the declared
/// `install` path; subdirectories are not descended.
fn package_firmware_deb(
    k: &ResolvedKmod,
    fw: &KmodFirmware,
    pin: &KmodPin,
    driver_tree: &Path,
    stage_root: &Path,
    epoch: Option<u64>,
    step: &Step,
) -> Result<PathBuf, EngineError> {
    let src = driver_tree.join(&fw.subdir);
    if !src.is_dir() {
        return Err(EngineError::ArtifactMissing {
            what: format!("firmware source dir for kmod '{}' (subdir {})", k.name, fw.subdir),
            location: src.display().to_string(),
        });
    }
    let pkg_stage = stage_root.join(format!("{}-fw-pkg", k.name));
    let _ = std::fs::remove_dir_all(&pkg_stage);
    let dest = pkg_stage.join(&fw.install);
    std::fs::create_dir_all(&dest).map_err(|s| EngineError::io(&dest, s))?;
    let mut count = 0usize;
    for entry in std::fs::read_dir(&src).map_err(|s| EngineError::io(&src, s))? {
        let entry = entry.map_err(|s| EngineError::io(&src, s))?;
        let p = entry.path();
        if p.is_file() {
            let to = dest.join(entry.file_name());
            std::fs::copy(&p, &to).map_err(|s| EngineError::io(&to, s))?;
            count += 1;
        }
    }
    if count == 0 {
        return Err(EngineError::ArtifactMissing {
            what: format!("firmware files for kmod '{}' under {}", k.name, fw.subdir),
            location: src.display().to_string(),
        });
    }

    let pkg = firmware_package_name(&k.name);
    let version = deb_version(pin);
    // Uniform data modes (dirs 0755, files 0644) so the host umask does not leak in.
    build::normalize_data_tree(&pkg_stage)?;
    let debian = pkg_stage.join("DEBIAN");
    std::fs::create_dir_all(&debian).map_err(|s| EngineError::io(&debian, s))?;
    std::fs::write(debian.join("control"), firmware_control_text(&pkg, &version, &k.name, count))
        .map_err(|s| EngineError::io(&debian.join("control"), s))?;
    build::set_mode(&debian, 0o755)?;
    build::set_mode(&debian.join("control"), 0o644)?;

    let deb_name = format!("{pkg}_{version}_all.deb");
    let deb_out = stage_root.join(&deb_name);
    let mut cmd = Command::new("fakeroot");
    cmd.args(["dpkg-deb", "--build"]).arg(&pkg_stage).arg(&deb_out);
    if let Some(e) = epoch {
        cmd.env("SOURCE_DATE_EPOCH", e.to_string());
    }
    build::run(cmd, "fakeroot", &format!("dpkg-deb --build {pkg}"), step)?;
    Ok(deb_out)
}

/// Apply the kernel's kbuild env (`ARCH`, optional `SOURCE_DATE_EPOCH`) plus the cross
/// toolchain to a `make` command — shared by the module compile and install steps.
fn apply_kbuild_env(cmd: &mut Command, build: &ResolvedBuild, env: &BuildEnv, epoch: Option<u64>) {
    for (key, value) in crate::build::kernel::kbuild_env(build, epoch) {
        cmd.env(key, value);
    }
    if let Some(prefix) = &env.cross_compile {
        cmd.env("CROSS_COMPILE", prefix);
    }
}

/// The deb package name: `<name>-modules-<kver>` (embeds the kernel release, like
/// `linux-image-<kver>`, so it is tied to the exact kernel it links into).
fn package_name(name: &str, kver: &str) -> String {
    format!("{name}-modules-{kver}")
}

/// The firmware deb package name: `<name>-firmware` (no kver — firmware is not tied to a
/// kernel release, so one package serves every installed kernel).
fn firmware_package_name(name: &str) -> String {
    format!("{name}-firmware")
}

/// The deb version from the locked ref + short commit, guaranteed to start with a digit
/// (a branch ref like `main` is prefixed `0~` so dpkg accepts and orders it low).
fn deb_version(pin: &KmodPin) -> String {
    let short = &pin.commit[..pin.commit.len().min(12)];
    let base = format!("{}+g{}", pin.reference, short);
    let base = if base.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        base
    } else {
        format!("0~{base}")
    };
    build::sanitize_deb_version(&base)
}

/// The `DEBIAN/control` stanza. Pure, so the metadata is testable without `dpkg-deb`.
fn control_text(pkg: &str, version: &str, arch: &str, kver: &str, driver: &str) -> String {
    format!(
        "Package: {pkg}\n\
         Version: {version}\n\
         Architecture: {arch}\n\
         Maintainer: boot2deb <boot2deb@localhost>\n\
         Section: kernel\n\
         Priority: optional\n\
         Depends: linux-image-{kver}\n\
         Description: {driver} out-of-tree kernel module for {kver}\n\
        \x20Out-of-tree driver built against the boot2deb kernel and installed under\n\
        \x20/lib/modules/{kver}/updates/. Rebuilt whenever the kernel is repinned.\n"
    )
}

/// The firmware `DEBIAN/control` stanza: `Architecture: all`, no `Depends`, so it
/// installs independently of any kernel. Pure, so the metadata is testable without
/// `dpkg-deb`.
fn firmware_control_text(pkg: &str, version: &str, driver: &str, file_count: usize) -> String {
    format!(
        "Package: {pkg}\n\
         Version: {version}\n\
         Architecture: all\n\
         Maintainer: boot2deb <boot2deb@localhost>\n\
         Section: kernel\n\
         Priority: optional\n\
         Description: {driver} device firmware ({file_count} files)\n\
        \x20Firmware blobs the {driver} out-of-tree driver loads, taken from the same\n\
        \x20pinned upstream repo as the driver so the two move together. Rebuilt whenever\n\
        \x20the driver is repinned.\n"
    )
}

/// The `depmod` maintainer script, shared by `postinst` and `postrm` — after either an
/// install (module appears in `updates/`) or a removal (it disappears), the module
/// dependency graph for `<kver>` must be regenerated so `modprobe` stays correct.
fn maintainer_script(kver: &str) -> String {
    format!(
        "#!/bin/sh\n\
         set -e\n\
         depmod -a {kver} 2>/dev/null || true\n"
    )
}

/// The Tier-1 driver-tree signature: the repo commit (which content-addresses the repo
/// and its own quilt) plus the applied in-repo patch list, patch dir, and local-patch
/// fingerprints. The subdir/make_args do not shape the *tree*, only the build, so they
/// live in the output signature.
pub fn tree_signature_manifest(k: &ResolvedKmod, pin: &KmodPin, local_fps: &[String]) -> SignatureManifest {
    let mut b = SignatureBuilder::new(&node_name(&k.name), TREE_STAGE_VERSION);
    b.fold_scalar("commit", &pin.commit);
    b.fold_scalar("patch_dir", &k.patch_dir);
    b.fold_ordered("patches", &k.repo_patches);
    b.fold_ordered("local_patches", local_fps);
    b.manifest()
}

/// The Tier-2 output signature of the `<name>-modules-<kver>.deb`. Folds the kernel tree
/// signature (a kernel commit/patch bump changes module vermagic), the driver commit +
/// applied patches, the subdir, the make args, the module list, arch, and toolchain id —
/// every input that changes the produced `.ko`s.
pub fn output_manifest(
    k: &ResolvedKmod,
    pin: &KmodPin,
    local_fps: &[String],
    kernel_sig: &Signature,
    arch: &str,
    toolchain_id: &str,
) -> SignatureManifest {
    let mut b = SignatureBuilder::new(&node_name(&k.name), OUTPUT_STAGE_VERSION);
    b.fold_dep(kernel_sig);
    b.fold_scalar("commit", &pin.commit);
    b.fold_scalar("subdir", &k.subdir);
    b.fold_scalar("patch_dir", &k.patch_dir);
    b.fold_ordered("patches", &k.repo_patches);
    b.fold_ordered("local_patches", local_fps);
    b.fold_ordered("make_args", &k.make_args);
    b.fold_ordered("modules", &k.modules);
    b.fold_scalar("arch", arch);
    b.fold_scalar("toolchain", toolchain_id);
    b.manifest()
}

/// The `kmod:<name>` build/cache node label.
pub fn node_name(name: &str) -> String {
    format!("kmod:{name}")
}

/// The `kmod-fw:<name>` build/cache node label for the companion firmware deb.
pub fn firmware_node_name(name: &str) -> String {
    format!("kmod-fw:{name}")
}

/// The Tier-2 output signature of the `<name>-firmware.deb`. Folds the driver commit
/// (which content-addresses the repo and its quilt, hence the firmware bytes at that
/// commit), the applied patch list + local-patch fingerprints (a patch could touch the
/// firmware dir), and the firmware source/install paths. No arch or toolchain: the
/// package is `Architecture: all` and nothing is compiled.
pub fn firmware_output_manifest(
    k: &ResolvedKmod,
    fw: &KmodFirmware,
    pin: &KmodPin,
    local_fps: &[String],
) -> SignatureManifest {
    let mut b = SignatureBuilder::new(&firmware_node_name(&k.name), FIRMWARE_STAGE_VERSION);
    b.fold_scalar("commit", &pin.commit);
    b.fold_scalar("patch_dir", &k.patch_dir);
    b.fold_ordered("patches", &k.repo_patches);
    b.fold_ordered("local_patches", local_fps);
    b.fold_scalar("fw_subdir", &fw.subdir);
    b.fold_scalar("fw_install", &fw.install);
    b.manifest()
}

/// The kernel release the built tree names (`7.1.3-1-arm64`) — the module's vermagic and
/// the deb's package name and `Depends:` all key on it, read from the one tree the module
/// is built against so they cannot drift.
fn kernel_release(ktree: &Path) -> Result<String, EngineError> {
    let path = ktree.join("include/config/kernel.release");
    let text = std::fs::read_to_string(&path).map_err(|s| EngineError::io(&path, s))?;
    Ok(text.trim().to_string())
}

/// The lock-derived kernel tree signature, folded into every kmod output key so a kernel
/// repin invalidates the modules `.deb` without needing the tree present.
fn kernel_tree_signature(lock: &Lock, opts: &KmodOptions) -> Result<Signature, EngineError> {
    let series_fp = build::dev_series_fingerprint(opts.kernel.patches, crate::build::PatchScope::Kernel);
    let patches = build::series_identity(opts.kernel.patches, &series_fp);
    let dts_fp = build::device_dts_fingerprint(opts.kernel.device_dts);
    Ok(crate::build::kernel::clone_manifest(lock, patches, &dts_fp)?.signature())
}

/// Fingerprint each local patch file's content, so an edited compat shim restamps.
fn local_patch_fingerprints(patches: &[PathBuf]) -> Result<Vec<String>, EngineError> {
    patches.iter().map(|p| build::file_fingerprint(p)).collect()
}

/// The resolved local-patch paths for a named kmod (empty when it declares none).
fn local_patches_for<'a>(opts: &'a KmodOptions, name: &str) -> &'a [PathBuf] {
    opts.local_patches
        .iter()
        .find(|(n, _)| n == name)
        .map(|(_, v)| v.as_slice())
        .unwrap_or(&[])
}

/// A per-name clone-source override, when one was given.
fn source_for<'a>(opts: &'a KmodOptions, name: &str) -> Option<&'a str> {
    opts.sources.iter().find(|(n, _)| n == name).map(|(_, s)| s.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signature::SignatureBuilder;

    fn kmod() -> ResolvedKmod {
        ResolvedKmod {
            name: "aic8800".into(),
            description: "AIC8800 SDIO Wi-Fi".into(),
            git: "https://github.com/radxa-pkg/aic8800.git".into(),
            git_ref: "main".into(),
            subdir: "src/SDIO/driver_fw/driver/aic8800".into(),
            patch_dir: "debian/patches".into(),
            repo_patches: vec!["fix-sdio-firmware-path.patch".into()],
            local_patches: vec![],
            make_args: vec!["CONFIG_FDRV_NO_REG_SDIO=y".into()],
            modules: vec!["aic8800_bsp".into(), "aic8800_fdrv".into()],
            firmware: None,
        }
    }

    fn pin(reference: &str) -> KmodPin {
        KmodPin {
            name: "aic8800".into(),
            source: "https://github.com/radxa-pkg/aic8800.git".into(),
            reference: reference.into(),
            commit: "a".repeat(40),
        }
    }

    fn kernel_sig(commit: &str) -> Signature {
        let mut b = SignatureBuilder::new("kernel", 1);
        b.fold_scalar("commit", commit);
        b.manifest().signature()
    }

    #[test]
    fn deb_version_always_starts_with_a_digit() {
        // A branch ref is prefixed `0~`; a numeric tag keeps its digit. Either way dpkg
        // gets a version starting with a digit.
        for reference in ["main", "8.1", "v7.1"] {
            let v = deb_version(&pin(reference));
            assert!(
                v.chars().next().is_some_and(|c| c.is_ascii_digit()),
                "deb version {v} for ref {reference} must start with a digit"
            );
        }
        assert!(deb_version(&pin("main")).starts_with("0~main+g"));
        assert!(deb_version(&pin("8.1")).starts_with("8.1+g"));
    }

    #[test]
    fn control_text_declares_the_kernel_dep_and_updates_path() {
        let text = control_text("aic8800-modules-7.1.3-1-arm64", "0~main+gabc", "arm64", "7.1.3-1-arm64", "aic8800");
        assert!(text.contains("Package: aic8800-modules-7.1.3-1-arm64"));
        assert!(text.contains("Depends: linux-image-7.1.3-1-arm64"));
        assert!(text.contains("/lib/modules/7.1.3-1-arm64/updates/"));
    }

    #[test]
    fn maintainer_script_runs_depmod_for_the_kver() {
        assert!(maintainer_script("7.1.3-1-arm64").contains("depmod -a 7.1.3-1-arm64"));
    }

    #[test]
    fn firmware_control_is_arch_all_with_no_kernel_dep() {
        let text = firmware_control_text("aic8800-firmware", "0~main+gabc", "aic8800", 7);
        assert!(text.contains("Package: aic8800-firmware"));
        assert!(text.contains("Architecture: all"));
        assert!(!text.contains("Depends:"), "firmware deb must not depend on a kernel");
        assert_eq!(firmware_package_name("aic8800"), "aic8800-firmware");
        assert_eq!(firmware_node_name("aic8800"), "kmod-fw:aic8800");
    }

    #[test]
    fn firmware_signature_folds_the_fw_paths_and_driver_commit() {
        let k = ResolvedKmod {
            firmware: Some(KmodFirmware {
                subdir: "src/SDIO/driver_fw/fw/aic8800D80".into(),
                install: "usr/lib/firmware/aic8800_fw/SDIO/aic8800D80".into(),
            }),
            ..kmod()
        };
        let fw = k.firmware.clone().unwrap();
        let p = pin("main");
        let base = firmware_output_manifest(&k, &fw, &p, &[]).signature();

        // A different source dir or install path restamps the firmware deb.
        let other_subdir = KmodFirmware { subdir: "src/USB/driver_fw/fw".into(), ..fw.clone() };
        assert_ne!(base, firmware_output_manifest(&k, &other_subdir, &p, &[]).signature());
        let other_install = KmodFirmware { install: "usr/lib/firmware/aic8800".into(), ..fw.clone() };
        assert_ne!(base, firmware_output_manifest(&k, &other_install, &p, &[]).signature());
        // A driver commit bump restamps it (the firmware bytes at that commit changed).
        let bumped = KmodPin { commit: "b".repeat(40), ..pin("main") };
        assert_ne!(base, firmware_output_manifest(&k, &fw, &bumped, &[]).signature());
        // An edited local shim restamps it too (a patch could touch the firmware dir).
        assert_ne!(base, firmware_output_manifest(&k, &fw, &p, &["deadbeef".into()]).signature());
    }

    #[test]
    fn output_signature_folds_the_kernel_and_build_inputs_that_the_tree_does_not() {
        let k = kmod();
        let p = pin("main");
        let base_tree = tree_signature_manifest(&k, &p, &[]).signature();
        let base_out = output_manifest(&k, &p, &[], &kernel_sig("k1"), "arm64", "gcc-13").signature();

        // The kernel signature is an output-only input (module vermagic): a kernel bump
        // must restamp the deb but leaves the fetched+patched *tree* untouched.
        assert_ne!(
            base_out,
            output_manifest(&k, &p, &[], &kernel_sig("k2"), "arm64", "gcc-13").signature(),
            "kernel sig must move the output signature"
        );

        // The subdir/make_args shape the build, not the tree, so they live in the output
        // signature only.
        let other_subdir = ResolvedKmod { subdir: "src/USB/driver".into(), ..kmod() };
        assert_eq!(
            base_tree,
            tree_signature_manifest(&other_subdir, &p, &[]).signature(),
            "subdir must not move the tree signature"
        );
        assert_ne!(
            base_out,
            output_manifest(&other_subdir, &p, &[], &kernel_sig("k1"), "arm64", "gcc-13").signature(),
            "subdir must move the output signature"
        );

        // The applied in-repo patch list shapes the tree, so it moves both signatures.
        let other_patches = ResolvedKmod { repo_patches: vec!["fix-other.patch".into()], ..kmod() };
        assert_ne!(base_tree, tree_signature_manifest(&other_patches, &p, &[]).signature());
        assert_ne!(
            base_out,
            output_manifest(&other_patches, &p, &[], &kernel_sig("k1"), "arm64", "gcc-13").signature(),
        );

        // A local-patch content fingerprint change (an edited compat shim) restamps both.
        let fps = vec!["deadbeef".to_string()];
        assert_ne!(base_tree, tree_signature_manifest(&k, &p, &fps).signature());
        assert_ne!(
            base_out,
            output_manifest(&k, &p, &fps, &kernel_sig("k1"), "arm64", "gcc-13").signature(),
        );
    }

    #[test]
    fn prune_keeps_named_modules_drops_extras_and_requires_each_named() {
        let sink = |_e: crate::event::Event| {};
        let step = Step::start(&sink, "kmod");
        let dir = tempfile::tempdir().unwrap();
        let updates = dir.path().join("updates");
        // `modules_install` preserves each module's build subdir, so lay them out that
        // way (updates/<subdir>/<name>.ko), not flat.
        for m in ["aic8800_bsp", "aic8800_fdrv", "aic8800_btlpm"] {
            let sub = updates.join(m);
            std::fs::create_dir_all(&sub).unwrap();
            std::fs::write(sub.join(format!("{m}.ko")), b"ko").unwrap();
        }
        // Ship only bsp+fdrv: the unlisted btlpm is dropped (and its emptied subdir), both
        // named survive in their subdirs.
        prune_and_verify_modules(&updates, &kmod(), &step).unwrap();
        assert!(updates.join("aic8800_bsp/aic8800_bsp.ko").exists());
        assert!(updates.join("aic8800_fdrv/aic8800_fdrv.ko").exists());
        assert!(!updates.join("aic8800_btlpm").exists(), "unlisted module + its dir dropped");

        // A named module that was never built is a hard error, not a silent empty deb.
        std::fs::remove_dir_all(updates.join("aic8800_fdrv")).unwrap();
        assert!(prune_and_verify_modules(&updates, &kmod(), &step).is_err());
    }

    #[test]
    fn prune_ships_everything_when_no_modules_named() {
        let sink = |_e: crate::event::Event| {};
        let step = Step::start(&sink, "kmod");
        let dir = tempfile::tempdir().unwrap();
        let updates = dir.path().join("updates");
        std::fs::create_dir_all(&updates).unwrap();
        std::fs::write(updates.join("whatever.ko"), b"ko").unwrap();
        let all = ResolvedKmod { modules: vec![], ..kmod() };
        prune_and_verify_modules(&updates, &all, &step).unwrap();
        assert!(updates.join("whatever.ko").exists(), "empty module set ships all");
    }
}
