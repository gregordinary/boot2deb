//! u-boot compile stage: clone the pinned u-boot, apply any locked
//! u-boot patches (none for the RK1's pristine `v2026.04`), build the board
//! defconfig with the sha256-verified rkbin ATF/TPL blobs, and stage the raw-gap
//! payloads (`idbloader.img`, `u-boot.itb`).
//!
//! RK3588 u-boot builds with the aarch64 toolchain (`CROSS_COMPILE` on a
//! non-arm64 host) and `CONFIG_ARM64=y` from the defconfig, so no `ARCH=` is
//! passed — the defconfig carries it. The blobs are verified against the lock's
//! hashes ([`crate::blobs`]) before `make` consumes them.

use crate::build::{self, stage_artifact, BuildEnv, ClonePinned, CloneMode, PatchScope, PatchSeries};
use crate::blobs;
use crate::error::EngineError;
use crate::event::{EventSink, Step};
use boot2deb_core::lock::Lock;
use boot2deb_core::size::parse_size;
use boot2deb_core::ResolvedBuild;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Stage-recipe version for the u-boot tree signature: bump when the
/// clone/patch logic that shapes the reused tree changes.
const CLONE_STAGE_VERSION: u32 = 1;

/// Stage-recipe version for the u-boot **output** signature (Tier-2 artifact cache):
/// bump when the compile/package logic changes the produced payloads/deb in a
/// way the folded inputs do not already capture.
const OUTPUT_STAGE_VERSION: u32 = 1;

/// Filesystem inputs for the u-boot stage.
pub struct UbootOptions<'a> {
    /// Git URL or local path to clone u-boot from, at the locked ref. Defaults to
    /// the boot method's `uboot_source`; a local clone speeds the shallow clone.
    pub source: &'a str,
    /// Checkout of the `patches` repo at the locked commit.
    pub patches_root: &'a Path,
    /// Directory holding the vendored rkbin blobs, verified against the lock
    /// before use.
    pub blobs_dir: &'a Path,
    /// Scratch directory holding the u-boot clone (`<work>/u-boot`).
    pub work_dir: &'a Path,
    /// Directory the produced boot payloads are staged into.
    pub out_dir: &'a Path,
    /// The `patches_root` is an explicit `--patches-path` co-dev checkout: a
    /// patches-pin mismatch is a loud warning rather than a hard error.
    pub patches_dev: bool,
    /// Root of the Tier-2 artifact store ([`crate::artstore`]), or `None` to
    /// disable output caching. On a hit the payloads + deb are restored; on a miss
    /// they are stored after the build.
    pub store: Option<&'a Path>,
}

/// The raw-gap boot payloads produced by [`build_uboot`], plus the packaged deb.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UbootArtifacts {
    /// `idbloader.img` (SPL + TPL), written at the boot method's idbloader offset.
    pub idbloader: PathBuf,
    /// `u-boot.itb` (FIT: u-boot proper + ATF + DT), written at its offset.
    pub uboot_itb: PathBuf,
    /// The `u-boot-<device>` `.deb` staging the payloads under `/usr/lib/u-boot`.
    /// The image build still writes the raw payloads to the gap directly;
    /// this deb is the package-centric artifact + on-board reference, not the
    /// bootloader-write path (it never auto-flashes).
    pub deb: PathBuf,
}

/// Run the u-boot stage, emitting its [`Event`](crate::event::Event)s to `sink`.
///
/// Reads the [`Lock`] for the u-boot ref/commit and the blob hashes. A fresh
/// clone is verified against the locked commit before any patches; a reused tree
/// is `distclean`ed and reconfigured.
pub fn build_uboot(
    build: &ResolvedBuild,
    lock: &Lock,
    opts: &UbootOptions,
    env: &BuildEnv,
    sink: &dyn EventSink,
) -> Result<UbootArtifacts, EngineError> {
    let step = Step::start(sink, "uboot");
    let tree = opts.work_dir.join("u-boot");

    // The applied patch series' identity for the Tier-1/Tier-2 signatures:
    // pinned by `patches.commit`, or the live-series fingerprint in co-dev mode so an
    // edited u-boot patch restamps the tree (CACHE-1). `series_fp` outlives `patches`.
    let series_fp = if opts.patches_dev {
        build::patch_series_fingerprint(opts.patches_root, &lock.patches.profile, PatchScope::Uboot)
    } else {
        Vec::new()
    };
    let patches = if opts.patches_dev {
        PatchSeries::Dev(&series_fp)
    } else {
        PatchSeries::Pinned
    };

    // Tier-2 output cache: restore the payloads + deb and skip the whole
    // clone/blob-verify/configure/compile when the output signature is stored. The
    // signature folds the blob hashes, so a hit implies the same verified blobs.
    let out_man = output_manifest(build, lock, env, patches);
    if let Some(root) = opts.store {
        let store = crate::artstore::ArtifactStore::open(root)?;
        if let Some(files) = store.restore("uboot", out_man.signature().as_str(), opts.out_dir)? {
            if let (Some(idbloader), Some(uboot_itb), Some(deb)) = (
                crate::build::role_path(&files, "idbloader"),
                crate::build::role_path(&files, "uboot_itb"),
                crate::build::role_path(&files, "deb"),
            ) {
                step.log(format!(
                    "restored u-boot payloads + deb from the artifact cache (signature {})",
                    out_man.signature().short()
                ));
                step.progress(100);
                step.finish();
                return Ok(UbootArtifacts { idbloader, uboot_itb, deb });
            }
        }
    }

    // Tier-1 reuse: the cloned+patched tree is reused only when its stamp
    // matches the current input signature; a lock bump rebuilds it rather than
    // reusing a stale checkout (COR-1). configure() distcleans + reconfigures a
    // reused tree and compile() re-runs, so the signature covers only the
    // tree-shaping clone/patch inputs.
    let man = clone_manifest(lock, patches);
    let reused = crate::signature::is_fresh(&tree, &man);
    if reused {
        step.log(format!(
            "reusing u-boot tree at {} (signature {})",
            tree.display(),
            man.signature().short()
        ));
    } else {
        if tree.exists() {
            step.log(format!(
                "u-boot tree at {} is stale (inputs changed) — rebuilding",
                tree.display()
            ));
            std::fs::remove_dir_all(&tree).map_err(|s| EngineError::io(&tree, s))?;
        }
        clone_and_patch(lock, opts, &tree, &step)?;
        crate::signature::write_manifest(&tree, &man)?;
    }
    step.progress(20);

    // Verify blobs against the lock and stage the verified bytes into a private
    // dir the build consumes, so `make` reads exactly what was hashed (SEC-5).
    let blob_stage = opts.work_dir.join("blobs");
    let atf = absolute(blobs::verify_to(opts.blobs_dir, &lock.blobs.atf, &blob_stage)?)?;
    let tpl = absolute(blobs::verify_to(opts.blobs_dir, &lock.blobs.tpl, &blob_stage)?)?;
    step.log("verified rkbin ATF + TPL against the lock");
    step.progress(30);

    configure(build, env, &tree, reused, &step)?;
    step.progress(40);

    // Deterministic build timestamp from the locked commit, so `u-boot.itb` does
    // not embed wall-clock time (COR-9).
    let epoch = crate::git::commit_epoch(&tree, &lock.uboot.commit).ok();
    compile(env, &tree, &atf, &tpl, epoch, &step)?;

    let (idbloader, uboot_itb) = collect(opts, &tree, &step)?;
    step.progress(90);

    let deb = package_deb(build, &lock.uboot.reference, opts, epoch, &idbloader, &uboot_itb, &step)?;

    // Store the payloads + deb under the output signature.
    if let Some(root) = opts.store {
        let store = crate::artstore::ArtifactStore::open(root)?;
        store.put(
            "uboot",
            out_man.signature().as_str(),
            &[
                ("idbloader", idbloader.as_path()),
                ("uboot_itb", uboot_itb.as_path()),
                ("deb", deb.as_path()),
            ],
        )?;
        step.log("stored u-boot payloads + deb to the artifact cache");
    }
    step.progress(100);
    step.finish();
    Ok(UbootArtifacts {
        idbloader,
        uboot_itb,
        deb,
    })
}

/// The Tier-2 output signature manifest of the u-boot payloads + deb. It
/// folds the Tier-1 tree signature ([`clone_manifest`]) as a dependency, then every
/// other input that shapes the output: the board defconfig, the sha256-pinned rkbin
/// blob hashes (a blob change → new signature → rebuild, so a hit implies the same
/// verified blobs), the deb's packaged fields (device, description, SoC, arch, the
/// raw offsets, the u-boot ref that becomes the deb version), whether the build is
/// cross, and the host toolchain identity. On a signature hit the artifact store
/// restores the payloads + deb rather than rebuilding, so the key must cover
/// everything that can change them.
fn output_manifest(
    build: &ResolvedBuild,
    lock: &Lock,
    env: &BuildEnv,
    patches: PatchSeries,
) -> crate::signature::SignatureManifest {
    // Fold the Tier-1 tree signature (carrying the co-dev series fingerprint, if any),
    // so a co-dev build never shares an output entry with a pinned one and an edited
    // patch invalidates the cached deb (CACHE-1).
    let tree_sig = clone_manifest(lock, patches).signature();
    let mut b = crate::signature::SignatureBuilder::new("uboot:out", OUTPUT_STAGE_VERSION);
    b.fold_dep(&tree_sig)
        .fold_scalar("uboot_defconfig", &build.uboot_defconfig)
        .fold_scalar("blob.atf", &lock.blobs.atf)
        .fold_scalar("blob.tpl", &lock.blobs.tpl)
        .fold_scalar("device", &build.device)
        .fold_scalar("description", &build.description)
        .fold_scalar("soc", build.soc.as_str())
        .fold_scalar("boot_method", build.boot_method.as_str())
        .fold_scalar("arch", build.arch.as_str())
        .fold_scalar("offset.idbloader", &build.offsets.idbloader)
        .fold_scalar("offset.uboot_itb", &build.offsets.uboot_itb)
        .fold_scalar("uboot.reference", &lock.uboot.reference)
        .fold_scalar("cross", env.cross_compile.as_deref().unwrap_or(""))
        .fold_scalar("toolchain", &env.toolchain_id);
    b.manifest()
}

/// The Tier-1 signature manifest of the cloned+patched u-boot tree: the
/// pinned inputs that determine its content — the u-boot commit and the patch series
/// (`build::fold_patch_series`). The source URL is excluded (the commit
/// content-addresses the tree). The [`PatchSeries`] fold covers the pinned patch
/// commit and — in co-dev mode — the live-series fingerprint (CACHE-1), so a co-dev
/// build never shares a stamp with a pinned one and an edited patch restamps.
/// Blobs/defconfig are not folded here — they gate compile, which re-runs on every
/// invocation, not the tree reuse. Public so `why-rebuild` ([`crate::plan`])
/// recomputes the same signature it stamps here.
pub fn clone_manifest(lock: &Lock, patches: PatchSeries) -> crate::signature::SignatureManifest {
    let mut b = crate::signature::SignatureBuilder::new("uboot", CLONE_STAGE_VERSION);
    b.fold_scalar("uboot.commit", &lock.uboot.commit);
    build::fold_patch_series(&mut b, &lock.patches.profile, &lock.patches.commit, patches);
    b.manifest()
}

/// Shallow-clone the pinned u-boot, verify the commit, enforce the patches pin,
/// and apply any locked u-boot patches. A failure removes the partial tree so
/// a resume never reuses a half-patched u-boot (via [`build::clone_pinned`]).
fn clone_and_patch(
    lock: &Lock,
    opts: &UbootOptions,
    tree: &Path,
    step: &Step,
) -> Result<(), EngineError> {
    let target = format!("u-boot @ {}", lock.uboot.reference);
    let spec = ClonePinned {
        source: opts.source,
        reference: &lock.uboot.reference,
        commit: &lock.uboot.commit,
        mode: CloneMode::Shallow,
        tree,
        what: "u-boot",
        patches_root: opts.patches_root,
        patches_commit: &lock.patches.commit,
        patches_dev: opts.patches_dev,
        profile: &lock.patches.profile,
        scope: PatchScope::Uboot,
        target: &target,
        gate_reference: None,
    };
    let n = build::clone_pinned(&spec, step)?;
    if n > 0 {
        step.log(format!("applied {n} u-boot patches ({})", lock.patches.profile));
    }
    Ok(())
}

/// Configure the board defconfig (`make <defconfig>`), `distclean`ing first only
/// when reusing a tree. RK3588 u-boot takes no `ARCH=` (the defconfig sets it).
fn configure(
    build: &ResolvedBuild,
    env: &BuildEnv,
    tree: &Path,
    reused: bool,
    step: &Step,
) -> Result<(), EngineError> {
    if reused {
        let mut clean = Command::new("make");
        clean.arg("-C").arg(tree).arg("distclean");
        cross(&mut clean, env);
        build::run(clean, "make", "make distclean", step)?;
    }
    // The defconfig comes from config; validate it and pass it after `--` so make
    // cannot read it as an option or a `FOO=bar` variable assignment (SUB-1).
    build::reject_unsafe_make_target("uboot_defconfig", &build.uboot_defconfig)?;
    let mut defconfig = Command::new("make");
    defconfig.arg("-C").arg(tree).arg("--").arg(&build.uboot_defconfig);
    cross(&mut defconfig, env);
    build::run(
        defconfig,
        "make",
        &format!("make {}", build.uboot_defconfig),
        step,
    )
}

/// Build u-boot with the verified blobs passed as make variables.
/// `source_date_epoch` is the locked commit's committer date (COR-9).
fn compile(
    env: &BuildEnv,
    tree: &Path,
    atf: &Path,
    tpl: &Path,
    source_date_epoch: Option<u64>,
    step: &Step,
) -> Result<(), EngineError> {
    let mut make = Command::new("make");
    make.arg("-C")
        .arg(tree)
        .arg(format!("-j{}", env.jobs()))
        .arg(format!("BL31={}", atf.display()))
        .arg(format!("ROCKCHIP_TPL={}", tpl.display()));
    if let Some(epoch) = source_date_epoch {
        make.env("SOURCE_DATE_EPOCH", epoch.to_string());
    }
    cross(&mut make, env);
    build::run(make, "make", "make u-boot", step)
}

/// Stage the produced boot payloads out of the tree, returning
/// `(idbloader.img, u-boot.itb)`.
fn collect(opts: &UbootOptions, tree: &Path, step: &Step) -> Result<(PathBuf, PathBuf), EngineError> {
    let idb_src = tree.join("idbloader.img");
    let itb_src = tree.join("u-boot.itb");
    for (what, path) in [("idbloader.img", &idb_src), ("u-boot.itb", &itb_src)] {
        if !path.exists() {
            return Err(EngineError::ArtifactMissing {
                what: what.into(),
                location: tree.display().to_string(),
            });
        }
    }
    let idbloader = stage_artifact(opts.out_dir, &idb_src)?;
    let uboot_itb = stage_artifact(opts.out_dir, &itb_src)?;
    step.log("staged idbloader.img and u-boot.itb");
    Ok((idbloader, uboot_itb))
}

/// Add `CROSS_COMPILE` to a `make` invocation when cross-building.
fn cross(cmd: &mut Command, env: &BuildEnv) {
    if let Some(prefix) = &env.cross_compile {
        cmd.env("CROSS_COMPILE", prefix);
    }
}

/// Canonicalize a blob path so `make -C <tree>` (which changes directory) still
/// resolves it. The file exists (verify just read it), so this cannot 404.
fn absolute(path: PathBuf) -> Result<PathBuf, EngineError> {
    std::fs::canonicalize(&path).map_err(|source| EngineError::io(&path, source))
}

/// Debian package name for a device's u-boot payloads, e.g. `u-boot-turing-rk1`.
fn package_name(device: &str) -> String {
    format!("u-boot-{device}")
}

/// Package the staged raw payloads into the `u-boot-<device>` `.deb`.
///
/// The deb stages `idbloader.img` + `u-boot.itb` under `/usr/lib/u-boot/<device>/`
/// with an `install.conf` recording their raw byte offsets, and documents the
/// manual `dd` in `README.Debian`. It carries **no** maintainer script: the
/// bootloader lives in a raw gap outside any filesystem, so flashing is the image
/// build's job (or a documented manual step), never an `apt` side effect that
/// could brick a board by writing to the wrong device. Built host-side
/// with `fakeroot dpkg-deb` — a data-only archive needs no target-arch sandbox.
///
/// `source_date_epoch` (the locked commit's committer date) is exported so
/// `dpkg-deb` clamps every archive member's mtime to it, making the `.deb`
/// byte-reproducible rather than stamped with the build clock.
fn package_deb(
    build: &ResolvedBuild,
    uboot_ref: &str,
    opts: &UbootOptions,
    source_date_epoch: Option<u64>,
    idbloader: &Path,
    uboot_itb: &Path,
    step: &Step,
) -> Result<PathBuf, EngineError> {
    let pkg = package_name(&build.device);
    let version = deb_version(uboot_ref);
    let arch = build.arch.debian_arch();

    // Assemble under a clean pkg-stage (a stale tree would ship leftover files).
    let pkg_stage = opts.work_dir.join("uboot-deb");
    let _ = std::fs::remove_dir_all(&pkg_stage);
    stage_tree(&pkg_stage, build, &pkg, &version, arch, idbloader, uboot_itb)?;
    // Force uniform data modes (dirs 0755, files 0644) so the host umask does not leak
    // into the packaged tree — the u-boot deb is data-only, so this is byte-safe and
    // makes the .deb reproducible across hosts (DET-5).
    build::normalize_data_tree(&pkg_stage)?;

    let deb_name = format!("{pkg}_{version}_{arch}.deb");
    let deb_in_stage = opts.work_dir.join(&deb_name);
    let mut cmd = Command::new("fakeroot");
    cmd.args(["dpkg-deb", "--build"])
        .arg(&pkg_stage)
        .arg(&deb_in_stage);
    if let Some(epoch) = source_date_epoch {
        cmd.env("SOURCE_DATE_EPOCH", epoch.to_string());
    }
    build::run(cmd, "fakeroot", "dpkg-deb --build u-boot", step)?;

    let deb = stage_artifact(opts.out_dir, &deb_in_stage)?;
    step.log(format!("staged {deb_name}"));
    Ok(deb)
}

/// Lay out the u-boot deb's file tree under `pkg_stage`: the two payloads and
/// their `install.conf` under `/usr/lib/u-boot/<device>/`, the `README.Debian`
/// under `/usr/share/doc/<pkg>/`, and the `DEBIAN/control`. Split from
/// [`package_deb`] so the layout is testable without `dpkg-deb`. The offsets are
/// parsed from the build's authored strings, so a malformed offset is a
/// typed [`ConfigError`](boot2deb_core::ConfigError) here rather than a bad deb.
fn stage_tree(
    pkg_stage: &Path,
    build: &ResolvedBuild,
    pkg: &str,
    version: &str,
    arch: &str,
    idbloader: &Path,
    uboot_itb: &Path,
) -> Result<(), EngineError> {
    let idb_off = parse_size(&build.offsets.idbloader)?;
    let itb_off = parse_size(&build.offsets.uboot_itb)?;

    let lib_dir = pkg_stage.join(format!("usr/lib/u-boot/{}", build.device));
    std::fs::create_dir_all(&lib_dir).map_err(|s| EngineError::io(&lib_dir, s))?;
    copy_into(idbloader, &lib_dir.join("idbloader.img"))?;
    copy_into(uboot_itb, &lib_dir.join("u-boot.itb"))?;
    let conf = install_conf_text(&build.device, build.boot_method.as_str(), idb_off, itb_off);
    write_file(&lib_dir.join("install.conf"), &conf)?;

    let doc_dir = pkg_stage.join(format!("usr/share/doc/{pkg}"));
    std::fs::create_dir_all(&doc_dir).map_err(|s| EngineError::io(&doc_dir, s))?;
    let readme = readme_text(
        &build.device,
        &build.description,
        &build.offsets.idbloader,
        idb_off,
        &build.offsets.uboot_itb,
        itb_off,
    );
    write_file(&doc_dir.join("README.Debian"), &readme)?;

    let debian = pkg_stage.join("DEBIAN");
    std::fs::create_dir_all(&debian).map_err(|s| EngineError::io(&debian, s))?;
    let control = control_text(pkg, version, arch, &build.description, build.soc.as_str());
    write_file(&debian.join("control"), &control)?;
    Ok(())
}

/// Copy `src` to `dst` (its parent already created), mapping I/O errors.
fn copy_into(src: &Path, dst: &Path) -> Result<(), EngineError> {
    std::fs::copy(src, dst)
        .map(|_| ())
        .map_err(|s| EngineError::io(src, s))
}

/// Write `contents` to `path`, mapping I/O errors.
fn write_file(path: &Path, contents: &str) -> Result<(), EngineError> {
    std::fs::write(path, contents).map_err(|s| EngineError::io(path, s))
}

/// Debian version from the u-boot ref: drop a leading `v` (`v2026.04` →
/// `2026.04`), then sanitize ([`build::sanitize_deb_version`]).
fn deb_version(reference: &str) -> String {
    build::sanitize_deb_version(reference.strip_prefix('v').unwrap_or(reference))
}

/// The `DEBIAN/control` stanza. Pure, so it is testable. No `Depends:` — the
/// package ships only data files — and no maintainer script.
fn control_text(pkg: &str, version: &str, arch: &str, description: &str, soc: &str) -> String {
    format!(
        "Package: {pkg}\n\
         Version: {version}\n\
         Section: admin\n\
         Priority: optional\n\
         Architecture: {arch}\n\
         Maintainer: boot2deb <build@boot2deb>\n\
         Description: U-Boot bootloader payloads for {description}\n\
        \x20Stages the SPL (idbloader.img) and U-Boot FIT (u-boot.itb) for the\n\
        \x20{soc} under /usr/lib/u-boot, with the raw offsets recorded in\n\
        \x20install.conf. It does NOT flash the bootloader: it lives in a raw gap\n\
        \x20outside any filesystem, so writing it is left to the image build or the\n\
        \x20documented manual dd (see /usr/share/doc/{pkg}/README.Debian).\n"
    )
}

/// The `install.conf` recording the payloads' raw byte offsets, so a future
/// on-device updater (Phase D+) can read where each is written. Pure/testable.
fn install_conf_text(device: &str, boot_method: &str, idb_off: u64, itb_off: u64) -> String {
    format!(
        "# boot2deb u-boot install offsets for {device}\n\
         # raw byte offsets from the start of the boot medium (outside any filesystem)\n\
         device={device}\n\
         boot_method={boot_method}\n\
         idbloader=/usr/lib/u-boot/{device}/idbloader.img\n\
         idbloader_offset={idb_off}\n\
         uboot_itb=/usr/lib/u-boot/{device}/u-boot.itb\n\
         uboot_itb_offset={itb_off}\n"
    )
}

/// The `README.Debian` documenting the manual flash. Pure/testable. Shows each
/// payload's offset both in bytes (from the parsed value) and in the authored
/// unit string, plus a ready-to-run `dd` per payload.
fn readme_text(
    device: &str,
    description: &str,
    idb_str: &str,
    idb_off: u64,
    itb_str: &str,
    itb_off: u64,
) -> String {
    let title = package_name(device);
    let underline = "=".repeat(title.len());
    format!(
        "{title}\n{underline}\n\n\
         This package stages the U-Boot bootloader for {description} under\n\
         /usr/lib/u-boot/{device}/. It deliberately does not write the bootloader\n\
         to any device: the payloads live in a raw gap outside any filesystem, so\n\
         an automatic flash on apt install/upgrade could brick a board by writing\n\
         to the wrong disk. The boot2deb image build writes them for you; to flash\n\
         a device by hand, write each payload to its fixed byte offset from the\n\
         start of the medium:\n\n\
        \x20 idbloader.img  ->  offset {idb_off} bytes ({idb_str})\n\
        \x20 u-boot.itb     ->  offset {itb_off} bytes ({itb_str})\n\n\
        \x20 {}\n\
        \x20 {}\n\
        \x20 sync\n\n\
         Replace /dev/DISK with the target boot medium (e.g. /dev/mmcblk0 or\n\
         /dev/sdX). Writing to the wrong device will destroy its contents --\n\
         double-check the device name first.\n",
        dd_command(&format!("/usr/lib/u-boot/{device}/idbloader.img"), idb_off),
        dd_command(&format!("/usr/lib/u-boot/{device}/u-boot.itb"), itb_off),
    )
}

/// A `dd` command writing `payload` to `/dev/DISK` at `offset` bytes, choosing
/// the largest of 4096 / 512 / 1 that divides the offset as the block size (RK
/// raw-gap offsets are 4 KiB-aligned, so this uses `bs=4K`). Pure, so the
/// block-size choice is testable.
fn dd_command(payload: &str, offset: u64) -> String {
    let (bs, seek) = if offset.is_multiple_of(4096) {
        (4096, offset / 4096)
    } else if offset.is_multiple_of(512) {
        (512, offset / 512)
    } else {
        (1, offset)
    };
    format!("dd if={payload} of=/dev/DISK bs={bs} seek={seek} conv=notrunc")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::rk1_build;
    use boot2deb_core::lock::{
        BlobsPin, FfmpegPins, GitPin, KernelPin, PatchesPin, RootfsPin, UbootPin, UserspacePins,
    };

    fn lock_with(uboot_commit: &str, patches_commit: &str) -> Lock {
        let git = |c: &str| GitPin { reference: "r".into(), commit: c.into() };
        Lock {
            kernel: KernelPin { id: "k".into(), reference: "v".into(), commit: "kc".into() },
            patches: PatchesPin { profile: "rk3588-accel".into(), commit: patches_commit.into() },
            uboot: UbootPin { reference: "v2026.04".into(), commit: uboot_commit.into() },
            userspace: Some(UserspacePins { mpp: git("m"), librga: git("r"), libmali: git("l") }),
            ffmpeg: Some(FfmpegPins { base: git("b"), rockchip: git("rk") }),
            rootfs: RootfsPin { suite: "forky".into(), manifest: "m".into(), manifest_sha256: None },
            blobs: BlobsPin { atf: "a".into(), tpl: "t".into() },
            extra_debs: vec![],
            snapshot: None,
        }
    }

    #[test]
    fn clone_manifest_tracks_pin_and_dev_inputs() {
        let sig = |uc, pc, patches| clone_manifest(&lock_with(uc, pc), patches).signature;
        let base = sig("uc1", "pc1", PatchSeries::Pinned);
        assert_eq!(base, sig("uc1", "pc1", PatchSeries::Pinned));
        // A u-boot bump or a patches-pin bump each invalidate the reused tree.
        assert_ne!(base, sig("uc2", "pc1", PatchSeries::Pinned));
        assert_ne!(base, sig("uc1", "pc2", PatchSeries::Pinned));
        // Co-dev mode splits the key; a co-dev content change restamps (CACHE-1).
        let empty: Vec<String> = vec![];
        assert_ne!(base, sig("uc1", "pc1", PatchSeries::Dev(&empty)));
        let fp1 = vec!["uboot/010.patch=aaa".to_string()];
        let fp2 = vec!["uboot/010.patch=bbb".to_string()];
        assert_ne!(
            sig("uc1", "pc1", PatchSeries::Dev(&fp1)),
            sig("uc1", "pc1", PatchSeries::Dev(&fp2))
        );
    }

    #[test]
    fn output_manifest_covers_blobs_defconfig_and_toolchain() {
        let build = rk1_build();
        let env = |tc: &str| BuildEnv {
            cross_compile: None,
            jobs: None,
            toolchain_id: tc.to_string(),
        };
        let man = |lock: &Lock, env: &BuildEnv, patches| {
            output_manifest(&build, lock, env, patches).signature
        };
        let base = man(&lock_with("uc1", "pc1"), &env("gcc-1"), PatchSeries::Pinned);
        // Stable under identical inputs.
        assert_eq!(base, man(&lock_with("uc1", "pc1"), &env("gcc-1"), PatchSeries::Pinned));
        // A u-boot pin bump reaches the output signature through the tree dependency.
        assert_ne!(base, man(&lock_with("uc2", "pc1"), &env("gcc-1"), PatchSeries::Pinned));
        // A blob change → new signature (a hit must imply the same verified blobs).
        let mut lock_blob = lock_with("uc1", "pc1");
        lock_blob.blobs.atf = "different-atf-hash".into();
        assert_ne!(base, man(&lock_blob, &env("gcc-1"), PatchSeries::Pinned));
        // Toolchain and co-dev mode each split the key.
        assert_ne!(base, man(&lock_with("uc1", "pc1"), &env("gcc-2"), PatchSeries::Pinned));
        let empty: Vec<String> = vec![];
        assert_ne!(base, man(&lock_with("uc1", "pc1"), &env("gcc-1"), PatchSeries::Dev(&empty)));
    }

    #[test]
    fn deb_version_strips_v_and_sanitizes() {
        assert_eq!(deb_version("v2026.04"), "2026.04");
        assert_eq!(deb_version("2026.04-rc1"), "2026.04-rc1");
        assert_eq!(deb_version("v2025.10+dfsg"), "2025.10+dfsg");
        assert_eq!(deb_version(""), "0");
    }

    #[test]
    fn dd_command_picks_block_size_by_alignment() {
        // 4 KiB-aligned RK offsets use bs=4K seeks.
        assert_eq!(
            dd_command("/p/idbloader.img", 32768),
            "dd if=/p/idbloader.img of=/dev/DISK bs=4096 seek=8 conv=notrunc"
        );
        assert!(dd_command("/p/u-boot.itb", 8 * 1024 * 1024).contains("bs=4096 seek=2048"));
        // 512-aligned but not 4 KiB → bs=512; unaligned → bs=1.
        assert!(dd_command("x", 512).contains("bs=512 seek=1"));
        assert!(dd_command("x", 513).contains("bs=1 seek=513"));
    }

    #[test]
    fn control_text_has_fields_and_no_depends() {
        let c = control_text(
            "u-boot-turing-rk1",
            "2026.04",
            "arm64",
            "Turing RK1 (RK3588)",
            "rk3588",
        );
        assert!(c.contains("Package: u-boot-turing-rk1"));
        assert!(c.contains("Version: 2026.04"));
        assert!(c.contains("Architecture: arm64"));
        // Data-only package: no runtime deps, and it must never auto-flash.
        assert!(!c.contains("Depends:"));
        assert!(c.contains("does NOT flash"));
        // Description continuation lines are space-prefixed per deb-control.
        assert!(c.lines().any(|l| l.starts_with(" Stages the SPL")));
    }

    #[test]
    fn install_conf_records_offsets() {
        let conf = install_conf_text("turing-rk1", "rockchip-rkbin", 32768, 8_388_608);
        assert!(conf.contains("device=turing-rk1"));
        assert!(conf.contains("boot_method=rockchip-rkbin"));
        assert!(conf.contains("idbloader_offset=32768"));
        assert!(conf.contains("uboot_itb_offset=8388608"));
    }

    #[test]
    fn readme_documents_offsets_and_dd() {
        let r = readme_text("turing-rk1", "Turing RK1 (RK3588)", "32KiB", 32768, "8MiB", 8_388_608);
        assert!(r.contains("offset 32768 bytes (32KiB)"));
        assert!(r.contains("offset 8388608 bytes (8MiB)"));
        assert!(r.contains("bs=4096 seek=8"));
        assert!(r.contains("bs=4096 seek=2048"));
        assert!(r.contains("/dev/DISK"));
    }

    #[test]
    fn stage_tree_lays_out_the_package() {
        let tmp = tempfile::tempdir().unwrap();
        let build = rk1_build();
        let payloads = tmp.path().join("payloads");
        std::fs::create_dir_all(&payloads).unwrap();
        let idb = payloads.join("idbloader.img");
        let itb = payloads.join("u-boot.itb");
        std::fs::write(&idb, b"IDBLOADER").unwrap();
        std::fs::write(&itb, b"UBOOTITB").unwrap();

        let pkg_stage = tmp.path().join("pkg-stage");
        stage_tree(&pkg_stage, &build, "u-boot-turing-rk1", "2026.04", "arm64", &idb, &itb).unwrap();

        // Payloads + install.conf land under /usr/lib/u-boot/<device>/.
        let libd = pkg_stage.join("usr/lib/u-boot/turing-rk1");
        assert_eq!(std::fs::read(libd.join("idbloader.img")).unwrap(), b"IDBLOADER");
        assert_eq!(std::fs::read(libd.join("u-boot.itb")).unwrap(), b"UBOOTITB");
        let conf = std::fs::read_to_string(libd.join("install.conf")).unwrap();
        assert!(conf.contains("idbloader_offset=32768")); // rk1: idbloader @ 32KiB
        // control + doc present; no maintainer scripts (never auto-flash).
        assert!(pkg_stage.join("DEBIAN/control").exists());
        assert!(pkg_stage
            .join("usr/share/doc/u-boot-turing-rk1/README.Debian")
            .exists());
        assert!(!pkg_stage.join("DEBIAN/postinst").exists());
    }

    #[test]
    fn dpkg_deb_accepts_the_staged_package() {
        // End-to-end: stage the real tree, then build + inspect the .deb with the
        // host packaging tools, confirming dpkg-deb accepts our control stanza.
        // Engine is Linux-only; skip where the tools are absent.
        let have = |t: &str| {
            Command::new(t)
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        if !have("dpkg-deb") || !have("fakeroot") {
            eprintln!("skipping dpkg_deb_accepts_the_staged_package: dpkg-deb/fakeroot unavailable");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let build = rk1_build();
        let idb = tmp.path().join("idbloader.img");
        let itb = tmp.path().join("u-boot.itb");
        std::fs::write(&idb, b"IDB").unwrap();
        std::fs::write(&itb, b"ITB").unwrap();
        let pkg_stage = tmp.path().join("pkg-stage");
        stage_tree(&pkg_stage, &build, "u-boot-turing-rk1", "2026.04", "arm64", &idb, &itb).unwrap();

        let deb = tmp.path().join("u-boot-turing-rk1_2026.04_arm64.deb");
        let built = Command::new("fakeroot")
            .args(["dpkg-deb", "--build"])
            .arg(&pkg_stage)
            .arg(&deb)
            .output()
            .unwrap();
        assert!(
            built.status.success(),
            "dpkg-deb --build failed: {}",
            String::from_utf8_lossy(&built.stderr)
        );

        // dpkg-deb parses our control and reports the fields back.
        let info = Command::new("dpkg-deb").arg("-I").arg(&deb).output().unwrap();
        let info = String::from_utf8_lossy(&info.stdout);
        assert!(info.contains("Package: u-boot-turing-rk1"), "info was: {info}");
        assert!(info.contains("Version: 2026.04"));
        assert!(info.contains("Architecture: arm64"));

        // The payloads + install.conf ship at the documented path.
        let contents = Command::new("dpkg-deb").arg("-c").arg(&deb).output().unwrap();
        let contents = String::from_utf8_lossy(&contents.stdout);
        assert!(contents.contains("/usr/lib/u-boot/turing-rk1/idbloader.img"));
        assert!(contents.contains("/usr/lib/u-boot/turing-rk1/u-boot.itb"));
        assert!(contents.contains("/usr/lib/u-boot/turing-rk1/install.conf"));
    }

    #[test]
    fn uboot_deb_is_byte_reproducible_with_a_fixed_epoch() {
        // Two independent packagings with the same SOURCE_DATE_EPOCH must yield a
        // byte-identical .deb — dpkg-deb clamps the staged files' (build-clock)
        // mtimes to the epoch, so nothing wall-clock leaks into the archive.
        let have = |t: &str| {
            Command::new(t)
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        };
        if !have("dpkg-deb") || !have("fakeroot") {
            eprintln!("skipping uboot_deb_is_byte_reproducible: dpkg-deb/fakeroot unavailable");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let build = rk1_build();
        let idb = tmp.path().join("idbloader.img");
        let itb = tmp.path().join("u-boot.itb");
        std::fs::write(&idb, b"IDB").unwrap();
        std::fs::write(&itb, b"ITB").unwrap();
        let sink = |_: crate::event::Event| {};
        let step = Step::start(&sink, "uboot");
        let dummy = tmp.path().join("dummy");

        let package = |tag: &str| -> Vec<u8> {
            let work = tmp.path().join(tag);
            std::fs::create_dir_all(&work).unwrap();
            let opts = UbootOptions {
                source: "unused",
                patches_root: &dummy,
                blobs_dir: &dummy,
                work_dir: &work,
                out_dir: &work,
                patches_dev: false,
                store: None,
            };
            let deb = package_deb(&build, "2026.04", &opts, Some(1_600_000_000), &idb, &itb, &step)
                .unwrap();
            std::fs::read(&deb).unwrap()
        };

        // Sleep-free: the two runs stage files at (possibly different) build-clock
        // times, both far newer than the 2020 epoch, so both clamp to it identically.
        assert_eq!(package("run-a"), package("run-b"));
    }
}
