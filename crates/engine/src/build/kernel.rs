//! Kernel compile stage: clone the pinned tree, apply the locked
//! patch series (`git am`), install the board's loose device-tree sources, lay down
//! the fragment-derived `.config`, and run `make bindeb-pkg` — producing the
//! `linux-image` / `linux-headers` `.deb`s.
//!
//! The `.config` is generated exactly as the parity check does ([`crate::kconfig`]):
//! base defconfig + fragments merged out-of-tree, then copied into the tree, so the
//! shipped kernel is configured from the same fragments `verify-config` checks.
//!
//! A board whose `.dts` is not yet upstream carries it in `device_dts` (§4): the
//! clone step copies those sources into the in-tree DT dir and registers the board
//! DTB in that dir's `Makefile`, so `bindeb-pkg` ships it in the `linux-image` deb
//! like any in-tree board. [`build_dtb`] rebuilds just that DTB for the bring-up
//! edit → reflash loop.

use crate::build::{
    self, deb_names, pick_deb, stage_artifact, BuildEnv, ClonePinned, CloneMode, PatchScope,
    PatchSeries, PatchSource,
};
use crate::error::EngineError;
use crate::event::{EventSink, Step};
use crate::kconfig;
use boot2deb_core::lock::Lock;
use boot2deb_core::ResolvedBuild;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Debian package build revision baked into `LOCALVERSION` and the changelog.
/// Fixed at 1: a boot2deb build is a fresh reproduction, not an incrementing
/// series of packagings.
const BUILD_VERSION: u32 = 1;

/// Stage-recipe version for the kernel tree signature: bump when the
/// clone/patch logic that shapes the reused tree changes, so an old stamp is
/// treated as stale and the tree is rebuilt.
const CLONE_STAGE_VERSION: u32 = 1;

/// `.deb` name prefixes `make bindeb-pkg` drops into the work dir (beside the
/// tree). [`build_kernel`] sweeps these before each compile so [`collect`]'s
/// highest-version pick can only see the `.deb`s the current compile produced —
/// a leftover from a build at different pins must not outrank them (COR-22).
/// Covers everything `bindeb-pkg` emits, not just the two `collect` stages.
const KERNEL_DEB_PREFIXES: &[&str] = &["linux-image-", "linux-headers-", "linux-libc-dev"];

/// Stage-recipe version for the kernel **output** signature (Tier-2 artifact cache):
/// bump when the compile/package logic changes the produced `.deb`s in a way
/// the folded inputs do not already capture (e.g. a changed `make` invocation).
/// v2: the config is now generated with `CROSS_COMPILE`, so the same fragments
/// resolve a different `.config` (the cross-toolchain-probed symbols) than under v1.
const OUTPUT_STAGE_VERSION: u32 = 2;

/// Filesystem inputs for the kernel stage (the lock and resolved build carry the
/// pins and axes; these are the on-disk locations).
pub struct KernelOptions<'a> {
    /// Git URL or local path to clone the kernel from, at the locked ref. A local
    /// clone (e.g. `../linux`) makes the shallow clone near-instant.
    pub source: &'a str,
    /// The patch series to apply, or `None` when the resolved kernel names no patch
    /// profile — the tree is then compiled exactly as cloned and the `patches` repo is
    /// never read.
    pub patches: Option<PatchSource<'a>>,
    /// Resolved kconfig fragment files in merge order (base → soc → accel →
    /// device), as produced from the resolved build's fragment names.
    pub fragments: &'a [PathBuf],
    /// Resolved `device_dts` sources (the board `.dts` plus any board `.dtsi`), in
    /// authored order, as produced from the resolved build's `device_dts` names.
    /// Empty for a board whose DTB is already upstream. §4.
    pub device_dts: &'a [PathBuf],
    /// Scratch directory holding the kernel clone (`<work>/linux`) and the `.deb`s
    /// `bindeb-pkg` drops beside it.
    pub work_dir: &'a Path,
    /// Directory the produced `.deb`s are staged into.
    pub out_dir: &'a Path,
    /// Root of the Tier-2 artifact store ([`crate::artstore`]), or `None` to
    /// disable output caching. On a hit the built `.deb`s are restored instead of
    /// recompiled; on a miss they are stored after the build.
    pub store: Option<&'a Path>,
}

/// The kernel `.deb`s produced by [`build_kernel`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KernelArtifacts {
    /// The `linux-image-*` package (kernel + modules + DTBs).
    pub image_deb: PathBuf,
    /// The `linux-headers-*` package.
    pub headers_deb: PathBuf,
}

/// The kernel source tree the [`build_kernel`] stage clones and reuses under
/// `work_dir` (`<work_dir>/linux`). Exposed so other nodes can read the tree — above
/// all [`source_date_epoch`] — without duplicating the layout literal.
pub fn tree_dir(work_dir: &Path) -> PathBuf {
    work_dir.join("linux")
}

/// The locked kernel commit's committer date as a `SOURCE_DATE_EPOCH`, read from the
/// cloned tree under `work_dir` — the lock-derived deterministic build timestamp.
/// The rootfs node reuses it (the same seed the image identifiers derive from,
/// the kernel commit) so its tarball mtimes are stable across builds of one lock
/// (DET-2/DET-4). `None` when the kernel tree is absent (a build that has not run the
/// kernel stage in this `work_dir`) or the commit object is unreadable; the caller then
/// proceeds without mtime clamping.
pub fn source_date_epoch(work_dir: &Path, lock: &Lock) -> Option<u64> {
    crate::git::commit_epoch(&tree_dir(work_dir), &lock.kernel.commit).ok()
}

/// Run the kernel stage, emitting its [`Event`](crate::event::Event)s to `sink`.
///
/// Reads only the [`Lock`] for pins: the kernel ref/commit, the patch
/// profile + its commit. A freshly-cloned tree is verified to sit at the locked
/// commit before patches are applied; a reused `<work>/linux` is left as-is
/// (already patched) and only reconfigured + rebuilt.
pub fn build_kernel(
    build: &ResolvedBuild,
    lock: &Lock,
    opts: &KernelOptions,
    env: &BuildEnv,
    sink: &dyn EventSink,
) -> Result<KernelArtifacts, EngineError> {
    let step = Step::start(sink, "kernel");
    let tree = tree_dir(opts.work_dir);

    // The applied patch series' identity for the Tier-1/Tier-2 signatures:
    // pinned by `patches.commit`, or — in co-dev (`--patches-path`) mode — the
    // fingerprint of the live series, so an edited patch restamps the tree instead of
    // restoring a stale one (CACHE-1). Computed once; `series_fp` outlives `patches`.
    let series_fp = build::dev_series_fingerprint(opts.patches, PatchScope::Kernel);
    let patches = build::series_identity(opts.patches, &series_fp);

    // The board's own device-tree sources are copied into the tree, so their content
    // — like a co-dev patch's — shapes it: fold it into the Tier-1 signature so an
    // edited `.dts` restamps rather than reuses.
    let dts_fp = build::device_dts_fingerprint(opts.device_dts);

    // Tier-2 output cache: if the full output signature (tree inputs +
    // config + toolchain) is already stored, restore the `.deb`s and skip the
    // clone/patch/configure/compile entirely — the whole payoff of the store.
    let out_man = output_manifest(build, lock, opts, env, patches, &dts_fp)?;
    if let Some([image_deb, headers_deb]) = build::restore_stage_outputs(
        opts.store,
        "kernel",
        &out_man.signature(),
        opts.out_dir,
        &["image_deb", "headers_deb"],
        &step,
    )?
    .as_deref()
    {
        step.progress(100);
        step.finish();
        return Ok(KernelArtifacts {
            image_deb: image_deb.clone(),
            headers_deb: headers_deb.clone(),
        });
    }

    // Tier-1 reuse of the cloned+patched tree (COR-1): a lock bump (kernel commit
    // or patch pin) rebuilds it; configure()/compile() re-run regardless.
    let man = clone_manifest(lock, patches, &dts_fp);
    build::reuse_or_refresh_tree(&tree, &man, "kernel", &step, || {
        clone_and_patch(build, lock, opts, &tree, &step)
    })?;
    step.progress(30);

    configure(build, opts, env, &tree, &step)?;
    step.progress(40);

    // Sweep kernel `.deb`s a previous build left in the work dir: `remove_dir_all`
    // above clears only the tree, and `collect` picks by highest version among
    // whatever sits beside it — a stale, higher-versioned leftover (a repin down
    // with the artifact cache off or cleared) would silently ship (COR-22).
    build::purge_stage_debs(opts.work_dir, KERNEL_DEB_PREFIXES)?;

    // Deterministic build timestamp from the locked base commit, not the tree's
    // README mtime (= clone time) or HEAD (a patch commit stamped now) (COR-9).
    let epoch = crate::git::commit_epoch(&tree, &lock.kernel.commit).ok();
    compile(build, env, &tree, epoch, &step)?;

    let artifacts = collect(opts, &step)?;

    // Store the produced `.deb`s under the output signature so a later build (or a
    // rebuild after `clean`) restores instead of recompiling.
    build::store_stage_outputs(
        opts.store,
        "kernel",
        &out_man.signature(),
        &[
            ("image_deb", artifacts.image_deb.as_path()),
            ("headers_deb", artifacts.headers_deb.as_path()),
        ],
        &step,
    )?;
    step.progress(100);
    step.finish();
    Ok(artifacts)
}

/// Rebuild only the board DTB (`make <dt_dir>/<board>.dtb`) in the already-patched
/// tree, staging it into `out_dir` — the bring-up fast path (§4).
///
/// Prepares the tree exactly as [`build_kernel`] does (clone + `git am` +
/// `device_dts` install on a stale or absent tree, reuse on a fresh one) and
/// regenerates the `.config`, which kbuild needs before it will build any DTB.
/// It then compiles the one DTB instead of the whole kernel, so an edit to the board
/// `.dts` reaches a flashable DTB in seconds rather than a full kernel build. Neither
/// artifact cache tier applies: the output is a single small file whose only input is
/// a source the developer is actively editing.
pub fn build_dtb(
    build: &ResolvedBuild,
    lock: &Lock,
    opts: &KernelOptions,
    env: &BuildEnv,
    sink: &dyn EventSink,
) -> Result<PathBuf, EngineError> {
    let step = Step::start(sink, "dtb");
    let tree = tree_dir(opts.work_dir);

    let series_fp = build::dev_series_fingerprint(opts.patches, PatchScope::Kernel);
    let patches = build::series_identity(opts.patches, &series_fp);
    let dts_fp = build::device_dts_fingerprint(opts.device_dts);
    let man = clone_manifest(lock, patches, &dts_fp);
    if crate::signature::is_fresh(&tree, &man) {
        step.log(format!("reusing kernel tree at {}", tree.display()));
    } else {
        if tree.exists() {
            std::fs::remove_dir_all(&tree).map_err(|s| EngineError::io(&tree, s))?;
        }
        clone_and_patch(build, lock, opts, &tree, &step)?;
        crate::signature::write_manifest(&tree, &man)?;
    }
    step.progress(40);

    configure(build, opts, env, &tree, &step)?;

    // `kernel_dtb` is already `<dt_dir>/<board>.dtb`, which is exactly how kbuild's
    // `%.dtb` rule names a DTB (relative to `arch/<arch>/boot/dts`).
    build::reject_unsafe_make_target("kernel_dtb", &build.kernel_dtb)?;
    let mut make = Command::new("make");
    make.arg("-C")
        .arg(&tree)
        .arg(format!("-j{}", env.jobs()))
        .arg("--")
        .arg(&build.kernel_dtb);
    for (key, value) in kbuild_env(build, crate::git::commit_epoch(&tree, &lock.kernel.commit).ok()) {
        make.env(key, value);
    }
    if let Some(prefix) = &env.cross_compile {
        make.env("CROSS_COMPILE", prefix);
    }
    build::run(make, "make", "make <board>.dtb", &step)?;

    let built = dt_source_dir(build, &tree).join(
        Path::new(&build.kernel_dtb)
            .file_name()
            .unwrap_or_default(),
    );
    if !built.exists() {
        return Err(EngineError::ArtifactMissing {
            what: build.kernel_dtb.clone(),
            location: built.display().to_string(),
        });
    }
    let staged = stage_artifact(opts.out_dir, &built)?;
    step.log(format!("staged {}", staged.display()));
    step.progress(100);
    step.finish();
    Ok(staged)
}

/// The Tier-2 output signature manifest of the kernel `.deb`s: every input
/// that determines the produced packages, not just the source tree. It folds the
/// Tier-1 tree signature ([`clone_manifest`]) as a dependency (covering the kernel
/// commit + patch series), then the inputs the compile/package step adds — the
/// kconfig fragments' *contents* (order-sensitive, last-wins merge), the base
/// defconfig, the kernel arch, the `KBUILD_IMAGE` path, the `LOCALVERSION` suffix,
/// whether the build is cross, and the host toolchain identity. On a signature hit
/// the artifact store restores the `.deb`s instead of rebuilding, so the key must
/// cover everything that can change them. `SOURCE_DATE_EPOCH` derives from the
/// kernel commit, already folded via the tree dependency.
fn output_manifest(
    build: &ResolvedBuild,
    lock: &Lock,
    opts: &KernelOptions,
    env: &BuildEnv,
    patches: PatchSeries,
    device_dts: &[String],
) -> Result<crate::signature::SignatureManifest, EngineError> {
    let tree_sig = clone_manifest(lock, patches, device_dts).signature();
    let mut fragments = Vec::with_capacity(opts.fragments.len());
    for frag in opts.fragments {
        let name = frag.file_name().and_then(|n| n.to_str()).unwrap_or("");
        fragments.push(format!("{name}={}", crate::build::file_fingerprint(frag)?));
    }
    let mut b = crate::signature::SignatureBuilder::new("kernel:out", OUTPUT_STAGE_VERSION);
    b.fold_dep(&tree_sig)
        .fold_ordered("fragments", &fragments)
        .fold_scalar("base_defconfig", &build.kernel.base_defconfig)
        .fold_scalar("kernel_arch", &build.kernel_arch)
        .fold_scalar("kbuild_image", &build.kbuild_image)
        .fold_scalar("localversion", &localversion(build))
        .fold_scalar("cross", env.cross_compile.as_deref().unwrap_or(""))
        .fold_scalar("toolchain", &env.toolchain_id);
    Ok(b.manifest())
}

/// The Tier-1 signature manifest of the cloned+patched kernel tree: the
/// pinned inputs that determine its content — the kernel commit, the kernel
/// reference, the patch series (`build::fold_patch_series`), and the board's loose
/// device-tree sources. The source URL is
/// excluded (a commit content-addresses the tree, so the same commit from any mirror
/// is the same tree). The reference is folded because the patch-applicability gate
/// keys on it, so a reference change without a commit change must restamp the tree to
/// force the gate to re-evaluate (CACHE-4). The [`PatchSeries`] fold covers the
/// pinned commit and — in co-dev mode — the live-series fingerprint (CACHE-1), so a
/// co-dev build never shares a stamp with a pinned one and an edited patch restamps.
/// `device_dts` is the ordered content fingerprint from
/// [`device_dts_fingerprint`](crate::build::device_dts_fingerprint); it is folded only
/// when non-empty, so a board with an upstream DTB signs exactly as it did before the
/// mechanism existed and keeps its cached tree.
/// Public so `why-rebuild` ([`crate::plan`]) recomputes the same signature it stamps
/// here.
pub fn clone_manifest(
    lock: &Lock,
    patches: PatchSeries,
    device_dts: &[String],
) -> crate::signature::SignatureManifest {
    let mut b = crate::signature::SignatureBuilder::new("kernel", CLONE_STAGE_VERSION);
    b.fold_scalar("kernel.commit", &lock.kernel.commit);
    b.fold_scalar("kernel.reference", &lock.kernel.reference);
    build::fold_patch_series(&mut b, lock.patches.as_ref(), patches);
    if !device_dts.is_empty() {
        b.fold_ordered("device_dts", device_dts);
    }
    b.manifest()
}

/// Shallow-clone the pinned kernel, verify it sits at the locked commit, enforce
/// the patches pin, apply the locked kernel patch series in place, and install the
/// board's `device_dts` sources. A
/// failure removes the partial tree so a resume never reuses a half-patched kernel
/// (via [`build::clone_pinned`], which homes that guard and the pin check).
///
/// The device-tree install runs **after** `git am` — a patch may touch the DT dir's
/// `Makefile`, and the board's DTB rule must survive that — and **before** any
/// `make`, so the first compile sees the board `.dts`.
fn clone_and_patch(
    build: &ResolvedBuild,
    lock: &Lock,
    opts: &KernelOptions,
    tree: &Path,
    step: &Step,
) -> Result<(), EngineError> {
    let target = format!("{} @ {}", lock.kernel.id, lock.kernel.reference);
    let spec = ClonePinned {
        source: opts.source,
        reference: &lock.kernel.reference,
        commit: &lock.kernel.commit,
        mode: CloneMode::Shallow,
        tree,
        what: "kernel",
        patches: opts.patches,
        scope: PatchScope::Kernel,
        target: &target,
        gate_reference: Some(&lock.kernel.reference),
    };
    let n = build::clone_pinned(&spec, step)?;
    match opts.patches {
        Some(p) => step.log(format!("applied {n} kernel patches ({})", p.pin.profile)),
        None => step.log("no patch profile: compiling the kernel tree as cloned"),
    }
    install_device_dts(build, opts.device_dts, tree, step)?;
    Ok(())
}

/// The in-tree device-tree source directory for a build: `arch/<arch>/boot/dts/<dt_dir>`.
/// This is where `device_dts` sources land and where `kernel_dtb` is compiled, so both
/// the install and the [`build_dtb`] fast path derive their paths from it.
fn dt_source_dir(build: &ResolvedBuild, tree: &Path) -> PathBuf {
    tree.join("arch")
        .join(&build.kernel_arch)
        .join("boot")
        .join("dts")
        .join(&build.dt_dir)
}

/// Copy the board's loose device-tree sources into the kernel's DT dir and register
/// the board DTB with kbuild, so `bindeb-pkg` compiles and ships it like any in-tree
/// board (§4).
///
/// Copy-into-tree rather than a standalone `dtc` run: a forked board `.dts`'s
/// `#include "<soc>.dtsi"` then resolves for free, and the DTB rides in the
/// `linux-image` deb with the kernel it was built against.
///
/// A source that would overwrite an existing in-tree file is refused
/// ([`EngineError::DeviceDtsShadowsUpstream`]) — that is a patch's job, not this
/// mechanism's. Only `.dts` sources are registered in the `Makefile`; a `.dtsi` is an
/// include, compiled through whatever `.dts` pulls it in.
fn install_device_dts(
    build: &ResolvedBuild,
    sources: &[PathBuf],
    tree: &Path,
    step: &Step,
) -> Result<(), EngineError> {
    if sources.is_empty() {
        return Ok(());
    }
    let dt_dir = dt_source_dir(build, tree);
    std::fs::create_dir_all(&dt_dir).map_err(|s| EngineError::io(&dt_dir, s))?;

    for src in sources {
        let name = src
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or_default()
            .to_string();
        let dest = dt_dir.join(&name);
        if dest.exists() {
            return Err(EngineError::DeviceDtsShadowsUpstream {
                src: src.display().to_string(),
                dest: dest.display().to_string(),
            });
        }
        std::fs::copy(src, &dest).map_err(|s| EngineError::io(src, s))?;
        if name.ends_with(".dts") {
            let dtb = name.replace(".dts", ".dtb");
            register_dtb(&dt_dir, &dtb)?;
        }
    }
    step.log(format!(
        "installed {} device-tree source(s) into {}",
        sources.len(),
        dt_dir.display()
    ));
    Ok(())
}

/// Append `dtb-$(CONFIG_…) += <dtb>` to a DT directory's `Makefile`, idempotently.
///
/// The `CONFIG_` symbol is read from the Makefile's own first `dtb-$(CONFIG_…) +=`
/// rule rather than hardcoded per SoC vendor: whatever gates the neighbouring boards'
/// DTBs gates this board's too, which is the only correct answer and keeps the engine
/// vendor-agnostic. A tree with no such rule cannot build the DTB, so that is an error
/// rather than a silently-ignored append.
fn register_dtb(dt_dir: &Path, dtb: &str) -> Result<(), EngineError> {
    let makefile = dt_dir.join("Makefile");
    let content = std::fs::read_to_string(&makefile).map_err(|s| EngineError::io(&makefile, s))?;

    // Idempotent: a reused or already-registered tree is left untouched. Scanning
    // every line (not just `dtb-` ones) also catches a rule split over `\`
    // continuations.
    if content
        .lines()
        .any(|line| line.split_whitespace().any(|tok| tok == dtb))
    {
        return Ok(());
    }
    let symbol = dtb_config_symbol(&content)
        .ok_or_else(|| EngineError::DeviceDtsNoMakefileRule {
            makefile: makefile.display().to_string(),
            dtb: dtb.to_string(),
        })?
        .to_string();
    let mut out = content;
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!("dtb-$({symbol}) += {dtb}\n"));
    std::fs::write(&makefile, out).map_err(|s| EngineError::io(&makefile, s))?;
    Ok(())
}

/// The `CONFIG_` symbol guarding DTB builds in a DT directory's `Makefile`, taken from
/// its first `dtb-$(CONFIG_…) +=` rule (e.g. `CONFIG_ARCH_ROCKCHIP`). Pure, so the
/// parse is unit-testable against real Makefile shapes.
fn dtb_config_symbol(makefile: &str) -> Option<&str> {
    makefile.lines().find_map(|line| {
        let rest = line.trim_start().strip_prefix("dtb-$(")?;
        let (symbol, _) = rest.split_once(')')?;
        symbol.starts_with("CONFIG_").then_some(symbol)
    })
}

/// Generate the fragment-merged `.config` (identical to the parity check's)
/// and copy it into the tree as `.config`.
fn configure(
    build: &ResolvedBuild,
    opts: &KernelOptions,
    env: &BuildEnv,
    tree: &Path,
    step: &Step,
) -> Result<(), EngineError> {
    mrproper_if_dirty(build, env, tree, step)?;
    let inputs = kconfig::ConfigInputs {
        tree,
        arch: &build.kernel_arch,
        // Resolve the config in the same toolchain context the kernel compiles in,
        // so cross-toolchain-probed symbols (ARM64_BTI/E0PD/…) are settled here and
        // `make bindeb-pkg` never prompts via an interactive oldconfig.
        cross_compile: env.cross_compile.as_deref(),
        base_defconfig: &build.kernel.base_defconfig,
        fragments: opts.fragments,
    };
    let config_out = opts.work_dir.join("config-gen");
    let generated = kconfig::generate(&inputs, &config_out, step)?;
    for sym in &generated.unmet {
        step.emit(
            crate::event::Stream::Stderr,
            format!("warning: fragment symbol not in final .config: {sym}"),
        );
    }
    let src = config_out.join(".config");
    let dst = tree.join(".config");
    std::fs::copy(&src, &dst).map_err(|source| EngineError::io(&src, source))?;
    step.log(format!(
        "configured kernel .config ({} symbols) from {} fragments",
        generated.config.len(),
        opts.fragments.len()
    ));
    Ok(())
}

/// The paths whose presence makes kbuild call a source tree "not clean" — exactly the
/// three `outputmakefile` tests. All are outputs of an *in-tree* build.
fn in_tree_build_state(build: &ResolvedBuild, tree: &Path) -> [PathBuf; 3] {
    [
        tree.join(".config"),
        tree.join("include").join("config"),
        tree.join("arch").join(&build.kernel_arch).join("include").join("generated"),
    ]
}

/// `make mrproper` the tree if a previous in-tree build left state behind.
///
/// The `.config` is generated by an out-of-tree (`O=`) build so the same code can
/// configure a *shared* verify-tree without dirtying it — but kbuild refuses an `O=`
/// build whose source tree carries in-tree build output, and `configure` itself ends
/// by copying the generated `.config` into the tree for `make bindeb-pkg`. So the
/// second configure of any reused tree — a rebuild after a fragment edit, a repeated
/// `--stage dtb`, a resumed interrupted build — would fail with kbuild's "The source
/// tree is not clean" unless the tree is reset first. The Tier-1 clone/patch work is
/// still reused; only the build output is discarded, which a changed `.config` would
/// have invalidated anyway. Mirrors the u-boot stage's `distclean` of a reused tree.
fn mrproper_if_dirty(
    build: &ResolvedBuild,
    env: &BuildEnv,
    tree: &Path,
    step: &Step,
) -> Result<(), EngineError> {
    if !in_tree_build_state(build, tree).iter().any(|p| p.exists()) {
        return Ok(());
    }
    step.log("kernel tree carries a previous in-tree build — mrproper before configuring");
    let mut make = Command::new("make");
    make.arg("-C")
        .arg(tree)
        .arg(format!("ARCH={}", build.kernel_arch))
        .arg("mrproper");
    if let Some(prefix) = &env.cross_compile {
        make.env("CROSS_COMPILE", prefix);
    }
    build::run(make, "make", "make mrproper", step)
}

/// Run `make bindeb-pkg` with the resolved kbuild env and cross settings.
/// `source_date_epoch` is the locked commit's committer date (COR-9).
fn compile(
    build: &ResolvedBuild,
    env: &BuildEnv,
    tree: &Path,
    source_date_epoch: Option<u64>,
    step: &Step,
) -> Result<(), EngineError> {
    let mut make = Command::new("make");
    make.arg("-C")
        .arg(tree)
        .arg(format!("-j{}", env.jobs()))
        .arg("bindeb-pkg")
        .arg(format!("KBUILD_IMAGE={}", build.kbuild_image))
        .arg(format!("LOCALVERSION={}", localversion(build)));
    // Cross builds skip dpkg's build-dep check for target-arch -dev packages we
    // don't need at compile time (no module signing in this config).
    if env.cross_compile.is_some() {
        make.arg("DPKG_FLAGS=-d");
    }
    for (key, value) in kbuild_env(build, source_date_epoch) {
        make.env(key, value);
    }
    if let Some(prefix) = &env.cross_compile {
        make.env("CROSS_COMPILE", prefix);
    }
    build::run(make, "make", "make bindeb-pkg", step)
}

/// Locate and stage the produced kernel `.deb`s from beside the tree.
fn collect(opts: &KernelOptions, step: &Step) -> Result<KernelArtifacts, EngineError> {
    // `bindeb-pkg` drops the .debs in the tree's parent — our work dir.
    let names = deb_names(opts.work_dir)?;
    let image = pick_deb(&names, "linux-image-").ok_or_else(|| EngineError::ArtifactMissing {
        what: "linux-image .deb".into(),
        location: opts.work_dir.display().to_string(),
    })?;
    let headers = pick_deb(&names, "linux-headers-").ok_or_else(|| EngineError::ArtifactMissing {
        what: "linux-headers .deb".into(),
        location: opts.work_dir.display().to_string(),
    })?;
    let image_deb = stage_artifact(opts.out_dir, &opts.work_dir.join(&image))?;
    let headers_deb = stage_artifact(opts.out_dir, &opts.work_dir.join(&headers))?;
    step.log(format!("staged {image} and {headers}"));
    Ok(KernelArtifacts {
        image_deb,
        headers_deb,
    })
}

/// The Debian `LOCALVERSION` suffix, e.g. `-1-arm64`.
fn localversion(build: &ResolvedBuild) -> String {
    format!("-{BUILD_VERSION}-{}", build.arch)
}

/// The reproducibility + packaging env passed to `make bindeb-pkg`. Pure so the
/// mapping is testable; `CROSS_COMPILE` is added separately (it is a host/target
/// fact, not a kbuild constant).
fn kbuild_env(build: &ResolvedBuild, source_date_epoch: Option<u64>) -> Vec<(String, String)> {
    let mut env = vec![
        ("ARCH".to_string(), build.kernel_arch.clone()),
        ("KDEB_CHANGELOG_DIST".to_string(), "stable".to_string()),
        ("KBUILD_BUILD_USER".to_string(), "boot2deb".to_string()),
        ("KBUILD_BUILD_HOST".to_string(), "boot2deb".to_string()),
        (
            "KBUILD_BUILD_VERSION".to_string(),
            BUILD_VERSION.to_string(),
        ),
    ];
    if let Some(epoch) = source_date_epoch {
        env.push(("SOURCE_DATE_EPOCH".to_string(), epoch.to_string()));
    }
    env
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::rk1_build;
    use boot2deb_core::lock::{
        BlobsPin, FfmpegPins, GitPin, KernelPin, Lock, PatchesPin, RootfsPin, UbootPin,
        UserspacePins,
    };

    fn lock_with(kernel_commit: &str, patches_commit: &str) -> Lock {
        let git = |c: &str| GitPin { source: "s".into(), reference: "r".into(), commit: c.into() };
        Lock {
            kernel: KernelPin { id: "k".into(), source: "ks".into(), reference: "v".into(), commit: kernel_commit.into() },
            patches: Some(PatchesPin { profile: "rk3588-accel".into(), commit: patches_commit.into() }),
            uboot: UbootPin { source: "us".into(), reference: "v".into(), commit: "u".into() },
            userspace: Some(UserspacePins { mpp: git("m"), librga: git("r"), libmali: git("l") }),
            ffmpeg: Some(FfmpegPins { base: git("b"), rockchip: git("rk") }),
            rootfs: RootfsPin { suite: "forky".into(), manifest: "m".into(), manifest_sha256: None },
            blobs: BlobsPin { atf: "a".into(), tpl: "t".into(), bl32: None },
            extra_debs: vec![],
            snapshot: None,
        }
    }

    #[test]
    fn clone_manifest_tracks_pin_and_dev_inputs() {
        let sig = |kc, pc, patches| clone_manifest(&lock_with(kc, pc), patches, &[]).signature;
        let base = sig("kc1", "pc1", PatchSeries::Pinned);
        // Same pins → same signature (a plain rebuild reuses the tree).
        assert_eq!(base, sig("kc1", "pc1", PatchSeries::Pinned));
        // A kernel bump changes it (stale tree rebuilds, not silently reused).
        assert_ne!(base, sig("kc2", "pc1", PatchSeries::Pinned));
        // A patches-pin bump changes it.
        assert_ne!(base, sig("kc1", "pc2", PatchSeries::Pinned));
        // Co-dev mode never shares a stamp with a pinned build, even with an empty
        // fingerprint (the `patches_dev` marker alone differs).
        let empty: Vec<String> = vec![];
        assert_ne!(base, sig("kc1", "pc1", PatchSeries::Dev(&empty)));
        // A co-dev series *content* change restamps the tree (CACHE-1): editing a
        // patch file (its digest) changes the fingerprint, hence the signature.
        let fp1 = vec!["media-accel/kernel/040.patch=aaa".to_string()];
        let fp2 = vec!["media-accel/kernel/040.patch=bbb".to_string()];
        assert_ne!(
            sig("kc1", "pc1", PatchSeries::Dev(&fp1)),
            sig("kc1", "pc1", PatchSeries::Dev(&fp2))
        );
    }

    #[test]
    fn kernel_reference_folds_into_clone_signature() {
        // The patch-applicability gate keys on kernel.reference, so a reference
        // change with no commit change must restamp the tree (CACHE-4).
        let mut a = lock_with("kc1", "pc1");
        let mut b = lock_with("kc1", "pc1");
        a.kernel.reference = "v7.1.1".into();
        b.kernel.reference = "v7.1.2".into();
        assert_ne!(
            clone_manifest(&a, PatchSeries::Pinned, &[]).signature,
            clone_manifest(&b, PatchSeries::Pinned, &[]).signature
        );
    }

    #[test]
    fn device_dts_content_folds_into_the_clone_signature() {
        let lock = lock_with("kc1", "pc1");
        let sig = |dts: &[String]| clone_manifest(&lock, PatchSeries::Pinned, dts).signature;

        // A board with an upstream DTB folds nothing, so its tree signature is exactly
        // what it was before `device_dts` existed — its cached tree stays valid.
        let upstream = sig(&[]);
        assert_eq!(upstream, clone_manifest(&lock, PatchSeries::Pinned, &[]).signature);

        // Carrying a board `.dts` at all distinguishes the tree (it now holds an extra
        // source and a Makefile rule).
        let v1 = vec!["board.dts=aaa".to_string()];
        assert_ne!(upstream, sig(&v1));

        // Editing the board `.dts` restamps the tree, so the next build re-copies and
        // recompiles rather than reusing a tree built from the old source.
        let v2 = vec!["board.dts=bbb".to_string()];
        assert_ne!(sig(&v1), sig(&v2));

        // Order is significant (a `.dtsi` include order can change the DTB).
        let a = vec!["a.dtsi=1".to_string(), "b.dts=2".to_string()];
        let b = vec!["b.dts=2".to_string(), "a.dtsi=1".to_string()];
        assert_ne!(sig(&a), sig(&b));
    }

    #[test]
    fn stale_higher_versioned_debs_cannot_survive_the_precompile_sweep() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        std::fs::create_dir_all(&work).unwrap();
        // Leftovers from a build at higher pins, exactly where bindeb-pkg drops them.
        for stale in [
            "linux-image-7.1.2-1-arm64_7.1.2-1_arm64.deb",
            "linux-headers-7.1.2-1-arm64_7.1.2-1_arm64.deb",
            "linux-libc-dev_7.1.2-1_arm64.deb",
        ] {
            std::fs::write(work.join(stale), b"stale").unwrap();
        }
        // The sweep build_kernel runs before compile...
        build::purge_stage_debs(&work, KERNEL_DEB_PREFIXES).unwrap();
        // ...then the compile writes the current build's outputs...
        for fresh in [
            "linux-image-7.1.1-1-arm64_7.1.1-1_arm64.deb",
            "linux-headers-7.1.1-1-arm64_7.1.1-1_arm64.deb",
        ] {
            std::fs::write(work.join(fresh), b"fresh").unwrap();
        }
        // ...and collect can only stage those, not a higher-versioned leftover.
        let out = tmp.path().join("out");
        let opts = KernelOptions {
            source: "",
            patches: None,
            fragments: &[],
            device_dts: &[],
            work_dir: &work,
            out_dir: &out,
            store: None,
        };
        let sink = |_e: crate::event::Event| {};
        let step = Step::start(&sink, "kernel");
        let staged = collect(&opts, &step).unwrap();
        assert!(staged.image_deb.ends_with("linux-image-7.1.1-1-arm64_7.1.1-1_arm64.deb"));
        assert!(staged.headers_deb.ends_with("linux-headers-7.1.1-1-arm64_7.1.1-1_arm64.deb"));
    }

    #[test]
    fn in_tree_build_state_matches_kbuilds_cleanliness_test() {
        let tmp = tempfile::tempdir().unwrap();
        let tree = tmp.path();
        let build = rk1_build();
        let sink = |_e: crate::event::Event| {};
        let step = Step::start(&sink, "configure");

        // A freshly-cloned tree carries none of the three, so nothing is cleaned and
        // no `make` runs (this test never shells out).
        assert!(!in_tree_build_state(&build, tree).iter().any(|p| p.exists()));
        mrproper_if_dirty(&build, &BuildEnv { cross_compile: None, jobs: None, toolchain_id: String::new() }, tree, &step).unwrap();

        // Each of kbuild's three markers, alone, makes the tree dirty. `configure`
        // itself creates the first by copying the generated `.config` in, which is why
        // a reused tree must be reset before the next out-of-tree config build.
        let markers = in_tree_build_state(&build, tree);
        assert_eq!(markers[0], tree.join(".config"));
        assert_eq!(markers[1], tree.join("include/config"));
        assert_eq!(markers[2], tree.join("arch/arm64/include/generated"));
        for marker in &markers {
            if let Some(parent) = marker.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(marker, "x").unwrap();
            assert!(
                in_tree_build_state(&build, tree).iter().any(|p| p.exists()),
                "{} must mark the tree dirty",
                marker.display()
            );
            std::fs::remove_file(marker).unwrap();
        }
    }

    #[test]
    fn dtb_config_symbol_reads_the_dirs_own_guard() {
        let rockchip = "# SPDX\ndtb-$(CONFIG_ARCH_ROCKCHIP) += rk3576-evb1-v10.dtb\n";
        assert_eq!(dtb_config_symbol(rockchip), Some("CONFIG_ARCH_ROCKCHIP"));
        // Another vendor's dir yields that vendor's symbol — nothing is hardcoded.
        assert_eq!(
            dtb_config_symbol("dtb-$(CONFIG_ARCH_SUNXI) += sun50i.dtb\n"),
            Some("CONFIG_ARCH_SUNXI")
        );
        // Leading whitespace is tolerated; a non-CONFIG variable is not a guard.
        assert_eq!(dtb_config_symbol("  dtb-$(CONFIG_X) += a.dtb"), Some("CONFIG_X"));
        assert_eq!(dtb_config_symbol("dtb-$(FOO) += a.dtb\n"), None);
        assert_eq!(dtb_config_symbol("subdir-y += rockchip\n"), None);
    }

    #[test]
    fn register_dtb_appends_once_and_is_idempotent() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let makefile = dir.join("Makefile");
        std::fs::write(&makefile, "dtb-$(CONFIG_ARCH_ROCKCHIP) += rk3576-evb1-v10.dtb\n").unwrap();

        register_dtb(dir, "rk3576-h96-max-m9.dtb").unwrap();
        let after = std::fs::read_to_string(&makefile).unwrap();
        assert!(after.contains("dtb-$(CONFIG_ARCH_ROCKCHIP) += rk3576-h96-max-m9.dtb"));
        assert!(after.contains("rk3576-evb1-v10.dtb"), "upstream rules are preserved");

        // A reused tree must not accumulate duplicate rules.
        register_dtb(dir, "rk3576-h96-max-m9.dtb").unwrap();
        let again = std::fs::read_to_string(&makefile).unwrap();
        assert_eq!(after, again);
        assert_eq!(again.matches("rk3576-h96-max-m9.dtb").count(), 1);
    }

    #[test]
    fn register_dtb_needs_a_rule_to_model_and_a_final_newline() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // A Makefile with no `dtb-$(CONFIG_…)` rule gives nothing to model the entry on.
        std::fs::write(dir.join("Makefile"), "subdir-y += foo\n").unwrap();
        assert!(matches!(
            register_dtb(dir, "board.dtb"),
            Err(EngineError::DeviceDtsNoMakefileRule { .. })
        ));
        // A Makefile whose last line lacks a newline still gets a well-formed rule.
        std::fs::write(dir.join("Makefile"), "dtb-$(CONFIG_A) += a.dtb").unwrap();
        register_dtb(dir, "board.dtb").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.join("Makefile")).unwrap(),
            "dtb-$(CONFIG_A) += a.dtb\ndtb-$(CONFIG_A) += board.dtb\n"
        );
    }

    #[test]
    fn install_device_dts_copies_sources_and_refuses_to_shadow_upstream() {
        let tmp = tempfile::tempdir().unwrap();
        let tree = tmp.path().join("linux");
        let build = rk1_build();
        let dt_dir = dt_source_dir(&build, &tree);
        std::fs::create_dir_all(&dt_dir).unwrap();
        std::fs::write(dt_dir.join("Makefile"), "dtb-$(CONFIG_ARCH_ROCKCHIP) += up.dtb\n").unwrap();
        std::fs::write(dt_dir.join("upstream.dts"), "/* in-tree */\n").unwrap();

        let src_dir = tmp.path().join("cfg");
        std::fs::create_dir_all(&src_dir).unwrap();
        let dtsi = src_dir.join("board-common.dtsi");
        let dts = src_dir.join("board.dts");
        std::fs::write(&dtsi, "/* common */\n").unwrap();
        std::fs::write(&dts, "/* board */\n").unwrap();

        let sink = |_e: crate::event::Event| {};
        let step = Step::start(&sink, "device-dts");
        install_device_dts(&build, &[dtsi, dts], &tree, &step).unwrap();

        // Both sources land in the DT dir; only the `.dts` gets a build rule (a `.dtsi`
        // is compiled through whatever includes it).
        assert!(dt_dir.join("board-common.dtsi").exists());
        assert!(dt_dir.join("board.dts").exists());
        let makefile = std::fs::read_to_string(dt_dir.join("Makefile")).unwrap();
        assert!(makefile.contains("dtb-$(CONFIG_ARCH_ROCKCHIP) += board.dtb"));
        assert!(!makefile.contains("board-common.dtb"));

        // An empty source list is the upstream-DTB case: nothing is touched.
        let untouched = std::fs::read_to_string(dt_dir.join("Makefile")).unwrap();
        install_device_dts(&build, &[], &tree, &step).unwrap();
        assert_eq!(untouched, std::fs::read_to_string(dt_dir.join("Makefile")).unwrap());

        // A source colliding with an in-tree file is refused: editing upstream DT is a
        // patch's job, and clobbering it would hide the drift.
        let shadow = src_dir.join("upstream.dts");
        std::fs::write(&shadow, "/* mine */\n").unwrap();
        assert!(matches!(
            install_device_dts(&build, &[shadow], &tree, &step),
            Err(EngineError::DeviceDtsShadowsUpstream { .. })
        ));
        assert_eq!(
            std::fs::read_to_string(dt_dir.join("upstream.dts")).unwrap(),
            "/* in-tree */\n",
            "the upstream source is left intact"
        );
    }

    #[test]
    fn output_manifest_covers_config_toolchain_and_tree_inputs() {
        let tmp = tempfile::tempdir().unwrap();
        let f1 = tmp.path().join("frag-a");
        let f2 = tmp.path().join("frag-b");
        std::fs::write(&f1, "CONFIG_A=y\n").unwrap();
        std::fs::write(&f2, "CONFIG_B=y\n").unwrap();
        let build = rk1_build();
        let lock = lock_with("kc1", "pc1");
        let frags = vec![f1.clone(), f2.clone()];
        let opts = KernelOptions {
            source: "",
            patches: None,
            fragments: &frags,
            device_dts: &[],
            work_dir: tmp.path(),
            out_dir: tmp.path(),
            store: None,
        };
        let env = |tc: &str| BuildEnv {
            cross_compile: None,
            jobs: None,
            toolchain_id: tc.to_string(),
        };
        let sig = |lock: &Lock, env: &BuildEnv| {
            output_manifest(&build, lock, &opts, env, PatchSeries::Pinned, &[]).unwrap().signature
        };
        let base = sig(&lock, &env("gcc-1"));
        // Identical inputs → identical signature (a plain rebuild restores).
        assert_eq!(base, sig(&lock, &env("gcc-1")));
        // A different toolchain must not restore another compiler's .debs.
        assert_ne!(base, sig(&lock, &env("gcc-2")));
        // A pin bump reaches the output signature through the tree dependency.
        assert_ne!(base, sig(&lock_with("kc2", "pc1"), &env("gcc-1")));
        // A fragment's *content* changing (same path) changes the .config → rebuild.
        std::fs::write(&f1, "CONFIG_A=n\n").unwrap();
        assert_ne!(base, sig(&lock, &env("gcc-1")));
    }

    #[test]
    fn localversion_uses_build_version_and_arch() {
        assert_eq!(localversion(&rk1_build()), "-1-arm64");
    }

    #[test]
    fn kbuild_env_sets_arch_and_optional_epoch() {
        let build = rk1_build();
        let env = kbuild_env(&build, Some(1_700_000_000));
        assert!(env.contains(&("ARCH".to_string(), "arm64".to_string())));
        assert!(env
            .iter()
            .any(|(k, v)| k == "SOURCE_DATE_EPOCH" && v == "1700000000"));
        // No epoch → the var is simply absent.
        let env = kbuild_env(&build, None);
        assert!(!env.iter().any(|(k, _)| k == "SOURCE_DATE_EPOCH"));
        // CROSS_COMPILE is never in the kbuild env (added from BuildEnv).
        assert!(!env.iter().any(|(k, _)| k == "CROSS_COMPILE"));
    }
}
