//! Userspace media-accel compile stage: build the Rockchip MPP and RGA
//! (and, on request, Mali) `.deb`s from the locked source pins, inside a
//! target-arch [`BuildSandbox`].
//!
//! Each package is fetched at its exact locked commit (`build::fetch_commit`),
//! then `dpkg-buildpackage` runs in the sandbox with the tsukumijima forks'
//! gcc-14 warning relaxations. The produced `.deb`s (`librockchip-mpp1`,
//! `librga2`, their `-dev`s, …) are staged out; `ffmpeg-rk` later build-depends on
//! the `-dev`s and runtime-depends on `librockchip-mpp1` + `librga2`.
//!
//! Libmali is off by default: the transcode pipeline rides the VPU + RGA, not the
//! GPU, so a headless box never needs it. When requested, its
//! `debian/targets` is filtered to the board's Mali variant to avoid compiling the
//! full variant matrix.

use crate::build::{self, deb_names, stage_artifact, BuildEnv, PatchScope, PatchSeries};
use crate::error::EngineError;
use crate::event::{EventSink, Step};
use crate::sandbox::{BuildSandbox, SandboxRun};
use boot2deb_core::lock::{GitPin, Lock};
use std::path::{Path, PathBuf};

/// Stage-recipe version for a userspace tree signature: bump when the
/// fetch/build logic that shapes a reused tree changes.
const FETCH_STAGE_VERSION: u32 = 1;

/// Stage-recipe version for a userspace **output** signature (Tier-2 artifact cache):
/// bump when the build/package logic changes a package's `.deb`s in a way the
/// folded inputs do not already capture.
const OUTPUT_STAGE_VERSION: u32 = 1;

/// Debian build-deps installed in the sandbox for MPP + RGA.
const USERSPACE_DEPS: &[&str] = &[
    "cmake",
    "meson",
    "ninja-build",
    "pkg-config",
    "dh-exec",
    "libdrm-dev",
];

/// Additional build-deps only Mali's variants probe (X11/Wayland `.pc` files).
const LIBMALI_DEPS: &[&str] = &[
    "libgbm-dev",
    "libwayland-dev",
    "libx11-dev",
    "libx11-xcb-dev",
    "libxcb-dri2-0-dev",
    "libxdamage-dev",
    "libxext-dev",
];

/// `DEB_CFLAGS_APPEND` for the MPP/RGA builds. The tsukumijima forks pre-date
/// gcc-14's stricter defaults and trip `-Werror` on K&R empty-paren prototypes;
/// demoting these three back to warnings lets the build proceed without altering
/// the produced binaries' behavior.
const RELAX_CFLAGS: &str =
    "-Wno-error=incompatible-pointer-types -Wno-error=int-conversion -Wno-error=implicit-function-declaration";

/// Default Mali variant kept when building libmali — the RK3588 Valhall G610.
/// Filtering to it skips the ~140 other-GPU variants.
const LIBMALI_VARIANT: &str = "aarch64-linux-gnu/libmali-valhall-g610";

/// One buildable userspace package: where to fetch it and which `.deb`s it emits.
struct Package<'a> {
    /// Directory name under `<work>/userspace/` and the label in logs.
    name: &'a str,
    /// Clone source (git URL or local checkout path).
    source: &'a str,
    /// Locked commit pin.
    pin: &'a GitPin,
    /// `.deb` name prefixes this package produces, for collection. Resume skips the
    /// package only when **every** prefix is already staged — a crash between a
    /// multi-binary package's outputs must not look "done" (COR-8).
    deb_prefixes: &'a [&'a str],
}

/// Filesystem inputs for the userspace stage.
pub struct UserspaceOptions<'a> {
    /// MPP clone source (git URL or local path; a local checkout is far faster).
    pub mpp_src: &'a str,
    /// librga clone source.
    pub librga_src: &'a str,
    /// libmali clone source (used only when `build_libmali`).
    pub libmali_src: &'a str,
    /// Build the Mali userspace too (off by default — unused on a headless box).
    pub build_libmali: bool,
    /// Checkout of the `patches` repo at the locked commit, for the userspace patch
    /// scope — the MPP CMA fix. The MPP tree receives it; librga/libmali
    /// build unpatched.
    pub patches_root: &'a Path,
    /// The `patches_root` is an explicit `--patches-path` co-dev checkout: a
    /// patches-pin mismatch is a loud warning rather than a hard error.
    pub patches_dev: bool,
    /// Scratch dir; sources are cloned under `<work>/userspace/<name>` and the
    /// `.deb`s `dpkg-buildpackage` drops land in `<work>/userspace/`.
    pub work_dir: &'a Path,
    /// Directory the produced `.deb`s are staged into.
    pub out_dir: &'a Path,
    /// Root of the Tier-2 artifact store ([`crate::artstore`]), or `None` to
    /// disable output caching. Cached per package: a hit restores that package's
    /// `.deb`s and, when *every* package is cached, the sandbox bootstrap is skipped
    /// too; a miss builds the package and stores its `.deb`s.
    pub store: Option<&'a Path>,
}

/// The userspace `.deb`s produced by [`build_userspace`], in collection order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserspaceArtifacts {
    /// Every staged `.deb` (mpp + rga runtime/`-dev`, optional mali).
    pub debs: Vec<PathBuf>,
}

/// Run the userspace stage, emitting its [`Event`](crate::event::Event)s to `sink`.
///
/// Reads only the [`Lock`] for the source pins. The `sandbox` supplies the
/// target-arch build environment ([`NativeSandbox`](crate::sandbox::NativeSandbox)
/// natively, [`RootlessSandbox`](crate::sandbox::RootlessSandbox) cross); this
/// stage is agnostic to which. A package whose `.deb`s are already staged in the
/// work dir is skipped (resume).
pub fn build_userspace(
    lock: &Lock,
    opts: &UserspaceOptions,
    arch: &str,
    env: &BuildEnv,
    sandbox: &dyn BuildSandbox,
    sink: &dyn EventSink,
) -> Result<UserspaceArtifacts, EngineError> {
    let step = Step::start(sink, "userspace");
    let stage_root = opts.work_dir.join("userspace");

    // The CLI schedules this stage only for a media-accel build, whose lock pins
    // the userspace sources; reaching it without pins is an internal bug.
    let userspace = lock
        .userspace
        .as_ref()
        .ok_or(EngineError::MissingMediaAccelPins { stage: "userspace" })?;

    let mut packages = vec![
        Package {
            name: "mpp",
            source: opts.mpp_src,
            pin: &userspace.mpp,
            deb_prefixes: &[
                "librockchip-mpp1_",
                "librockchip-mpp-dev_",
                "librockchip-vpu0_",
                "rockchip-mpp-demos_",
            ],
        },
        Package {
            name: "librga",
            source: opts.librga_src,
            pin: &userspace.librga,
            deb_prefixes: &["librga2_", "librga-dev_"],
        },
    ];
    if opts.build_libmali {
        packages.push(Package {
            name: "libmali",
            source: opts.libmali_src,
            pin: &userspace.libmali,
            deb_prefixes: &["libmali-"],
        });
    }

    // Patch context: the profile's `userspace` scope — the MPP CMA fix — is
    // applied to the MPP tree; librga/libmali build unpatched upstream. The profile +
    // its pin are the same for the whole build; [`receives_userspace_patches`] decides
    // which package's tree gets the series (and folds it into that tree's signature).
    // In co-dev mode the userspace series fingerprint is folded into the MPP tree
    // signature so an edited userspace patch restamps it (CACHE-1); `series_fp` lives
    // in the ctx so the borrowed [`PatchSeries::Dev`] outlives the package loop.
    let series_fp = if opts.patches_dev {
        build::patch_series_fingerprint(opts.patches_root, &lock.patches.profile, PatchScope::Userspace)
    } else {
        Vec::new()
    };
    let patch_ctx = UserspacePatchCtx {
        root: opts.patches_root,
        commit: &lock.patches.commit,
        profile: &lock.patches.profile,
        dev: opts.patches_dev,
        series_fp,
    };

    // Tier-2 output cache: decide each package's hit/miss up front, so a
    // fully-cached userspace stage skips the sandbox bootstrap entirely (the real
    // payoff after a `clean --sandbox`). The per-package output signature folds the
    // fetch pin + patch series (MPP) + build recipe + suite/arch (
    // `package_output_manifest`).
    let store = opts.store.map(crate::artstore::ArtifactStore::open).transpose()?;
    let out_sigs: Vec<String> = packages
        .iter()
        .map(|p| {
            let pi = patch_ctx.inputs_for(p.name);
            package_output_manifest(p, &lock.rootfs.suite, arch, pi.as_ref())
                .signature()
                .as_str()
                .to_string()
        })
        .collect();
    let cached: Vec<bool> = packages
        .iter()
        .zip(&out_sigs)
        .map(|(p, sig)| store.as_ref().is_some_and(|s| s.has(&node_name(p), sig)))
        .collect();
    let all_cached = store.is_some() && cached.iter().all(|&c| c);

    step.log(format!("sandbox: {}", sandbox.describe()));
    if all_cached {
        step.log("all userspace packages cached — skipping sandbox setup");
    } else {
        sandbox.ensure_ready(&step)?;
        step.progress(15);
        let mut deps: Vec<&str> = USERSPACE_DEPS.to_vec();
        if opts.build_libmali {
            deps.extend_from_slice(LIBMALI_DEPS);
        }
        sandbox.install(&deps, &step)?;
    }
    step.progress(25);

    // Build (or restore) each package, spreading coarse progress across 25..90.
    let span = 65u8;
    std::fs::create_dir_all(&stage_root).map_err(|s| EngineError::io(&stage_root, s))?;
    for (i, pkg) in packages.iter().enumerate() {
        let restored = if cached[i] {
            let store = store.as_ref().expect("cached implies a store");
            store
                .restore(&node_name(pkg), &out_sigs[i], &stage_root)?
                .inspect(|_| step.log(format!("{}: restored from artifact cache", pkg.name)))
                .is_some()
        } else {
            false
        };
        if !restored {
            build_one(pkg, &stage_root, env, sandbox, &patch_ctx, &step)?;
            if let Some(s) = store.as_ref() {
                store_package(s, pkg, &node_name(pkg), &out_sigs[i], &stage_root, &step)?;
            }
        }
        step.progress(25 + span * (i as u8 + 1) / packages.len() as u8);
    }

    let artifacts = collect(&packages, &stage_root, opts.out_dir, &step)?;
    step.progress(100);
    step.finish();
    Ok(artifacts)
}

/// Fetch (if needed), apply the package's patch scope, and build one package,
/// leaving its `.deb`s in `stage_root`.
fn build_one(
    pkg: &Package,
    stage_root: &Path,
    env: &BuildEnv,
    sandbox: &dyn BuildSandbox,
    patches: &UserspacePatchCtx,
    step: &Step,
) -> Result<(), EngineError> {
    let tree = stage_root.join(pkg.name);
    let man = package_signature(pkg, patches.inputs_for(pkg.name).as_ref());
    let fresh = crate::signature::is_fresh(&tree, &man);
    // Reuse the built `.deb`s only when the fetched+patched tree still matches the
    // locked commit *and* patch series (Tier-1): a pin or patch bump
    // makes the tree stale, so its tree + any stale-version `.deb`s are purged and
    // it is refetched/repatched/rebuilt rather than silently reused (COR-1/COR-8).
    if fresh && package_staged(stage_root, pkg)? {
        step.log(format!("{}: already built, skipping", pkg.name));
        return Ok(());
    }
    if fresh {
        step.log(format!("{}: reusing tree at {}", pkg.name, tree.display()));
    } else {
        if tree.exists() {
            step.log(format!("{}: source pin changed — refetching", pkg.name));
            std::fs::remove_dir_all(&tree).map_err(|s| EngineError::io(&tree, s))?;
        }
        // Purge stale-version `.deb`s so `collect` cannot ship an old one and
        // `package_staged` cannot be fooled by it.
        remove_staged_debs(stage_root, pkg)?;
        build::fetch_commit(pkg.source, &pkg.pin.reference, &pkg.pin.commit, pkg.name, &tree, step)?;
        // Apply the profile's `userspace` scope onto the fetched base — the MPP CMA
        // fix, mirroring the kernel/ffmpeg stages' clone→apply flow. Only
        // MPP is patched; librga/libmali are unpatched upstream. The series is
        // materialized in the patches repo (durable base + patch), so the pin
        // is a re-fetchable tag rather than a locally-authored commit.
        if receives_userspace_patches(pkg.name) {
            apply_patches(pkg, &tree, patches, step).inspect_err(|_| {
                // Never leave a half-patched, unstamped tree a resume would trust.
                let _ = std::fs::remove_dir_all(&tree);
            })?;
        }
        crate::signature::write_manifest(&tree, &man)?;
    }

    if pkg.name == "libmali" {
        filter_libmali_targets(&tree.join("debian/targets"), LIBMALI_VARIANT, step)?;
    }

    // Deterministic build timestamp from the locked *base* commit (COR-9). For
    // MPP the tree now carries the patch series, so — like the kernel/ffmpeg stages —
    // read the base pin explicitly (still reachable after `git am`), not HEAD.
    let epoch = crate::git::commit_epoch(&tree, &pkg.pin.commit).ok();
    let dpkg_env = dpkg_env(env.jobs(), epoch);
    let argv: Vec<String> = ["dpkg-buildpackage", "-us", "-uc", "-b"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    let binds = [stage_root.to_path_buf()];
    let context = format!("dpkg-buildpackage {}", pkg.name);
    let spec = SandboxRun {
        work: &tree,
        binds: &binds,
        ro_binds: &[],
        net: false,
        env: &dpkg_env,
        argv: &argv,
        context: &context,
    };
    sandbox.run(&spec, step)?;
    step.log(format!("{}: built", pkg.name));
    Ok(())
}

/// Copy every produced `.deb` from `stage_root` into `out_dir`, in package order.
fn collect(
    packages: &[Package],
    stage_root: &Path,
    out_dir: &Path,
    step: &Step,
) -> Result<UserspaceArtifacts, EngineError> {
    let names = deb_names(stage_root)?;
    let mut debs = Vec::new();
    let mut seen = Vec::new();
    for pkg in packages {
        for name in select_debs(&names, pkg.deb_prefixes) {
            if seen.contains(name) {
                continue;
            }
            seen.push(name.clone());
            debs.push(stage_artifact(out_dir, &stage_root.join(name))?);
        }
    }
    if debs.is_empty() {
        return Err(EngineError::ArtifactMissing {
            what: "userspace .debs".into(),
            location: stage_root.display().to_string(),
        });
    }
    step.log(format!("staged {} userspace .deb(s)", debs.len()));
    Ok(UserspaceArtifacts { debs })
}

/// Whether a userspace package receives the profile's `userspace` patch scope.
/// Only MPP carries a local patch — the CMA fix (`allocator_dma_heap`);
/// librga/libmali build unpatched upstream. The single source of truth so the stage
/// and `why-rebuild` ([`crate::plan`]) agree on which package's tree gets the series
/// and folds it into its signature.
pub fn receives_userspace_patches(name: &str) -> bool {
    name == "mpp"
}

/// The patch inputs folded into a userspace package's tree signature when it
/// receives the `userspace` scope — the profile name, its pinned commit, and
/// the applied-series identity, mirroring how the kernel/ffmpeg tree signatures fold
/// their series (`build::fold_patch_series`). A package that carries no patch folds
/// none of this.
pub struct PatchInputs<'a> {
    /// Patch profile name (`lock.patches.profile`).
    pub profile: &'a str,
    /// Pinned `patches` repo commit (`lock.patches.commit`).
    pub commit: &'a str,
    /// The applied series' identity: pinned by commit, or (co-dev) the live-series
    /// fingerprint so an edited userspace patch restamps the MPP tree (CACHE-1).
    pub patches: PatchSeries<'a>,
}

/// The userspace stage's patch context: the `patches` checkout to read the series
/// from, the pin to enforce, and the profile — shared across the packages, applied
/// only to the ones [`receives_userspace_patches`] selects.
struct UserspacePatchCtx<'a> {
    /// The `patches` checkout root (`--patches-path` co-dev, `../patches`, or the
    /// auto-fetched cache).
    root: &'a Path,
    /// The lock's `patches.commit` the checkout is pinned to.
    commit: &'a str,
    /// The lock's `patches.profile`.
    profile: &'a str,
    /// A co-dev `--patches-path` checkout (a pin mismatch warns).
    dev: bool,
    /// The co-dev live-series fingerprint of the `userspace` scope (empty in pinned
    /// mode), folded into the MPP tree signature so an edited userspace patch restamps
    /// it (CACHE-1).
    series_fp: Vec<String>,
}

impl UserspacePatchCtx<'_> {
    /// The [`PatchInputs`] a package folds into its signature — `Some` iff it
    /// receives the userspace scope, `None` otherwise.
    fn inputs_for(&self, name: &str) -> Option<PatchInputs<'_>> {
        receives_userspace_patches(name).then_some(PatchInputs {
            profile: self.profile,
            commit: self.commit,
            patches: if self.dev {
                PatchSeries::Dev(&self.series_fp)
            } else {
                PatchSeries::Pinned
            },
        })
    }
}

/// Apply the profile's `userspace` scope onto `pkg`'s fetched tree in place,
/// via the shared [`apply_profile_scope`](crate::build::apply_profile_scope) — the
/// same pin-enforcement + verify-applies gate the kernel/ffmpeg stages use. No
/// kernel-range gate here (that guards the kernel node; the profile is validated
/// there). Logs the applied count.
fn apply_patches(
    pkg: &Package,
    tree: &Path,
    ctx: &UserspacePatchCtx,
    step: &Step,
) -> Result<(), EngineError> {
    let target = format!("{} @ {}", pkg.name, pkg.pin.reference);
    let n = build::apply_profile_scope(
        &build::ApplyScope {
            tree,
            patches_root: ctx.root,
            patches_commit: ctx.commit,
            patches_dev: ctx.dev,
            profile: ctx.profile,
            scope: build::PatchScope::Userspace,
            target: &target,
            gate_reference: None,
        },
        step,
    )?;
    step.log(format!("{}: applied {n} userspace patch(es) ({})", pkg.name, ctx.profile));
    Ok(())
}

/// Tier-1 signature manifest of a fetched userspace source tree, keyed by
/// package `name`, its locked `commit` (which content-addresses the fetched tree),
/// and — when the package receives the `userspace` scope — the patch profile
/// with its pinned commit, so a patch change restamps the tree just as a pin bump
/// does. Public and parameterized so `why-rebuild` ([`crate::plan`]) recomputes the
/// same per-package signature this stage stamps — the node is `userspace:<name>`.
pub fn signature_manifest(
    name: &str,
    commit: &str,
    patches: Option<&PatchInputs>,
) -> crate::signature::SignatureManifest {
    let mut b =
        crate::signature::SignatureBuilder::new(&format!("userspace:{name}"), FETCH_STAGE_VERSION);
    b.fold_scalar("commit", commit);
    if let Some(p) = patches {
        build::fold_patch_series(&mut b, p.profile, p.commit, p.patches);
    }
    b.manifest()
}

/// The signature manifest for a resolved [`Package`], via [`signature_manifest`]; the
/// caller passes the package's [`PatchInputs`] (`Some` for MPP, `None` otherwise).
fn package_signature(
    pkg: &Package,
    patches: Option<&PatchInputs>,
) -> crate::signature::SignatureManifest {
    signature_manifest(pkg.name, &pkg.pin.commit, patches)
}

/// The artifact-store node name for a package's `.deb`s, e.g. `userspace:mpp`
/// (matching the Tier-1 per-package node name).
fn node_name(pkg: &Package) -> String {
    format!("userspace:{}", pkg.name)
}

/// The Tier-2 output signature manifest of a userspace package's `.deb`s from
/// primitives. It folds the Tier-1 fetch signature ([`signature_manifest`], the
/// commit + MPP patch series) as a dependency, then the build recipe: the
/// gcc-14 warning relaxation, the target arch, and the **suite** — the package
/// compiles inside the target-arch sandbox, whose toolchain is the suite's `gcc`, so
/// the suite stands in for that toolchain identity (its bounded within-suite drift is
/// acceptable; these accel debs are not byte-gated). Libmali also folds its
/// variant filter. On a signature hit the store restores this package's `.deb`s
/// rather than rebuilding; a patch change reaches this output signature through the
/// folded tree dependency.
///
/// Public and keyed by primitives (not a `Package`) so the ffmpeg stage recomputes
/// the mpp/librga dependency signatures from the lock and folds them into its own
/// output key (CACHE-2) — an ffmpeg build links against those `.deb`s, so a change to
/// them must invalidate the cached ffmpeg deb.
pub fn output_manifest_for(
    name: &str,
    commit: &str,
    suite: &str,
    arch: &str,
    patches: Option<&PatchInputs>,
) -> crate::signature::SignatureManifest {
    let tree_sig = signature_manifest(name, commit, patches).signature();
    let mut b =
        crate::signature::SignatureBuilder::new(&format!("userspace:{name}:out"), OUTPUT_STAGE_VERSION);
    b.fold_dep(&tree_sig)
        .fold_scalar("relax_cflags", RELAX_CFLAGS)
        .fold_scalar("suite", suite)
        .fold_scalar("arch", arch);
    if name == "libmali" {
        b.fold_scalar("libmali_variant", LIBMALI_VARIANT);
    }
    b.manifest()
}

/// The Tier-2 output signature manifest of a resolved [`Package`]'s `.deb`s, via
/// [`output_manifest_for`] with the package's name + pinned commit.
fn package_output_manifest(
    pkg: &Package,
    suite: &str,
    arch: &str,
    patches: Option<&PatchInputs>,
) -> crate::signature::SignatureManifest {
    output_manifest_for(pkg.name, &pkg.pin.commit, suite, arch, patches)
}

/// Store the package's freshly-built `.deb`s (selected from `stage_root` by its name
/// prefixes) under `(node, sig)` in the artifact store, so a later build restores
/// them instead of rebuilding. All share the role `deb` — [`collect`]
/// re-selects them on restore, so their order/role beyond presence does not matter.
fn store_package(
    store: &crate::artstore::ArtifactStore,
    pkg: &Package,
    node: &str,
    sig: &str,
    stage_root: &Path,
    step: &Step,
) -> Result<(), EngineError> {
    let names = deb_names(stage_root)?;
    let paths: Vec<PathBuf> = select_debs(&names, pkg.deb_prefixes)
        .iter()
        .map(|n| stage_root.join(n))
        .collect();
    let refs: Vec<(&str, &Path)> = paths.iter().map(|p| ("deb", p.as_path())).collect();
    store.put(node, sig, &refs)?;
    step.log(format!(
        "{}: stored {} .deb(s) to the artifact cache",
        pkg.name,
        refs.len()
    ));
    Ok(())
}

/// Remove `pkg`'s already-staged `.deb`s from `stage_root` (by name prefix), so a
/// refetch after a pin bump leaves no stale-version `.deb` for `collect` to ship
/// or `package_staged` to miscount. An absent dir/file is a no-op.
fn remove_staged_debs(stage_root: &Path, pkg: &Package) -> Result<(), EngineError> {
    if !stage_root.exists() {
        return Ok(());
    }
    for name in deb_names(stage_root)? {
        if pkg.deb_prefixes.iter().any(|p| name.starts_with(p)) {
            let path = stage_root.join(&name);
            std::fs::remove_file(&path).map_err(|s| EngineError::io(&path, s))?;
        }
    }
    Ok(())
}

/// True only if **all** of `pkg`'s `.deb`s already sit in `stage_root` (resume
/// check). Requiring every prefix — not just one — means a crash partway through a
/// multi-binary package's outputs re-runs it rather than skipping to a later stage
/// that then fails on the missing `.deb` (COR-8).
fn package_staged(stage_root: &Path, pkg: &Package) -> Result<bool, EngineError> {
    if !stage_root.exists() {
        return Ok(false);
    }
    let names = deb_names(stage_root)?;
    Ok(pkg
        .deb_prefixes
        .iter()
        .all(|prefix| names.iter().any(|n| n.starts_with(prefix))))
}

/// The env for a `dpkg-buildpackage` run: the gcc-14 warning relaxation, a
/// `parallel=` matching the resolved job count, and — when known — the locked
/// commit's `SOURCE_DATE_EPOCH` for a reproducible build timestamp (COR-9). Pure,
/// so it is testable.
fn dpkg_env(jobs: usize, source_date_epoch: Option<u64>) -> Vec<(String, String)> {
    let mut env = vec![
        ("DEB_CFLAGS_APPEND".to_string(), RELAX_CFLAGS.to_string()),
        ("DEB_BUILD_OPTIONS".to_string(), format!("parallel={jobs}")),
    ];
    if let Some(epoch) = source_date_epoch {
        env.push(("SOURCE_DATE_EPOCH".to_string(), epoch.to_string()));
    }
    env
}

/// `.deb` file names in `names` matching any of `prefixes`. Pure selection so
/// collection is testable without a build.
fn select_debs<'a>(names: &'a [String], prefixes: &[&str]) -> Vec<&'a String> {
    names
        .iter()
        .filter(|n| prefixes.iter().any(|p| n.starts_with(p)))
        .collect()
}

/// Rewrite libmali's `debian/targets` to only the lines naming the board's Mali
/// variant, skipping the full variant matrix. A no-op if the file is absent; if
/// the variant matches nothing, leaves the file untouched (build all) with a
/// warning, rather than producing an empty target set.
fn filter_libmali_targets(targets: &Path, variant: &str, step: &Step) -> Result<(), EngineError> {
    if !targets.exists() {
        return Ok(());
    }
    let content = std::fs::read_to_string(targets).map_err(|s| EngineError::io(targets, s))?;
    let filtered = filter_targets(&content, variant);
    if filtered.trim().is_empty() {
        step.emit(
            crate::event::Stream::Stderr,
            format!("warning: libmali variant '{variant}' matched no targets; building all"),
        );
        return Ok(());
    }
    std::fs::write(targets, &filtered).map_err(|s| EngineError::io(targets, s))?;
    let kept = filtered.lines().count();
    step.log(format!("libmali: filtered to {kept} target(s) matching {variant}"));
    Ok(())
}

/// Keep the lines of `content` containing `variant` (each newline-terminated).
/// Pure, so the filter is testable.
fn filter_targets(content: &str, variant: &str) -> String {
    content
        .lines()
        .filter(|l| l.contains(variant))
        .map(|l| format!("{l}\n"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn select_debs_matches_prefixes() {
        let names = vec![
            "librockchip-mpp1_1.5.0_arm64.deb".to_string(),
            "librockchip-mpp-dev_1.5.0_arm64.deb".to_string(),
            "librga2_2.2.0_arm64.deb".to_string(),
            "unrelated_1_arm64.deb".to_string(),
        ];
        let mpp = select_debs(&names, &["librockchip-mpp1_", "librockchip-mpp-dev_"]);
        assert_eq!(mpp.len(), 2);
        assert!(mpp.iter().all(|n| n.starts_with("librockchip-mpp")));
        let rga = select_debs(&names, &["librga2_"]);
        assert_eq!(rga.len(), 1);
        assert!(select_debs(&names, &["nonexistent_"]).is_empty());
    }

    #[test]
    fn dpkg_env_sets_relax_cflags_and_parallel() {
        let env = dpkg_env(8, Some(1_700_000_000));
        assert!(env
            .iter()
            .any(|(k, v)| k == "DEB_CFLAGS_APPEND" && v.contains("incompatible-pointer-types")));
        assert!(env
            .iter()
            .any(|(k, v)| k == "DEB_BUILD_OPTIONS" && v == "parallel=8"));
        // The locked commit's epoch rides along for a reproducible timestamp.
        assert!(env
            .iter()
            .any(|(k, v)| k == "SOURCE_DATE_EPOCH" && v == "1700000000"));
        // Absent when unknown.
        assert!(!dpkg_env(8, None)
            .iter()
            .any(|(k, _)| k == "SOURCE_DATE_EPOCH"));
    }

    #[test]
    fn filter_targets_keeps_only_matching_variant() {
        let content = "\
aarch64-linux-gnu/libmali-valhall-g610 gbm
aarch64-linux-gnu/libmali-bifrost-g52 gbm
aarch64-linux-gnu/libmali-valhall-g610 wayland
arm-linux-gnueabihf/libmali-utgard-450 x11
";
        let kept = filter_targets(content, "aarch64-linux-gnu/libmali-valhall-g610");
        assert_eq!(kept.lines().count(), 2);
        assert!(kept.lines().all(|l| l.contains("valhall-g610")));
        // An unmatched variant yields an empty set (caller warns + skips).
        assert!(filter_targets(content, "libmali-nonexistent").is_empty());
    }

    #[test]
    fn package_signature_tracks_commit_and_name() {
        let pin_a = GitPin { reference: "master".into(), commit: "c1".into() };
        let pin_b = GitPin { reference: "master".into(), commit: "c2".into() };
        let mpp_a = Package { name: "mpp", source: "", pin: &pin_a, deb_prefixes: &[] };
        let mpp_a2 = Package { name: "mpp", source: "x", pin: &pin_a, deb_prefixes: &["y_"] };
        let mpp_b = Package { name: "mpp", source: "", pin: &pin_b, deb_prefixes: &[] };
        let rga_a = Package { name: "librga", source: "", pin: &pin_a, deb_prefixes: &[] };
        // Same commit → same signature (source/prefixes are not tree-shaping).
        assert_eq!(package_signature(&mpp_a, None), package_signature(&mpp_a2, None));
        // A commit bump invalidates the reused tree + debs.
        assert_ne!(package_signature(&mpp_a, None), package_signature(&mpp_b, None));
        // Different packages at the same commit never collide.
        assert_ne!(package_signature(&mpp_a, None), package_signature(&rga_a, None));
    }

    #[test]
    fn patch_series_folds_into_the_patched_package_signature() {
        // Only MPP receives the userspace scope.
        assert!(receives_userspace_patches("mpp"));
        assert!(!receives_userspace_patches("librga"));
        assert!(!receives_userspace_patches("libmali"));

        let pin = GitPin {
            reference: "v1.5.0-1-20260121-750e76e".into(),
            commit: "750e76ec2d9287babfaf08c8bf395ebc5e8778ea".into(),
        };
        let mpp = Package { name: "mpp", source: "", pin: &pin, deb_prefixes: &[] };
        let p1 = PatchInputs { profile: "rk3588-accel", commit: "p1", patches: PatchSeries::Pinned };
        let p2 = PatchInputs { profile: "rk3588-accel", commit: "p2", patches: PatchSeries::Pinned };
        let empty: Vec<String> = vec![];
        let p1_dev = PatchInputs {
            profile: "rk3588-accel",
            commit: "p1",
            patches: PatchSeries::Dev(&empty),
        };
        // Folding a patch series changes the tree signature vs an unpatched fetch...
        assert_ne!(package_signature(&mpp, None), package_signature(&mpp, Some(&p1)));
        // ...a patch-pin bump changes it again (a patch change restamps the tree)...
        assert_ne!(package_signature(&mpp, Some(&p1)), package_signature(&mpp, Some(&p2)));
        // ...and a co-dev build never shares a stamp with a pinned one.
        assert_ne!(package_signature(&mpp, Some(&p1)), package_signature(&mpp, Some(&p1_dev)));
        // ...and a co-dev userspace-patch content change restamps the MPP tree (CACHE-1).
        let fp1 = vec!["media-accel/userspace/001.patch=aaa".to_string()];
        let fp2 = vec!["media-accel/userspace/001.patch=bbb".to_string()];
        let dev1 = PatchInputs { profile: "rk3588-accel", commit: "p1", patches: PatchSeries::Dev(&fp1) };
        let dev2 = PatchInputs { profile: "rk3588-accel", commit: "p1", patches: PatchSeries::Dev(&fp2) };
        assert_ne!(package_signature(&mpp, Some(&dev1)), package_signature(&mpp, Some(&dev2)));
    }

    #[test]
    fn remove_staged_debs_purges_only_matching_prefixes() {
        let tmp = tempfile::tempdir().unwrap();
        let stage_root = tmp.path().join("userspace");
        std::fs::create_dir_all(&stage_root).unwrap();
        for n in [
            "librockchip-mpp1_1.5.0_arm64.deb",
            "librockchip-mpp-dev_1.5.0_arm64.deb",
            "librga2_2.2.0_arm64.deb",
        ] {
            std::fs::write(stage_root.join(n), b"x").unwrap();
        }
        let pin = GitPin { reference: "c".into(), commit: "c".into() };
        let mpp = Package {
            name: "mpp",
            source: "",
            pin: &pin,
            deb_prefixes: &["librockchip-mpp1_", "librockchip-mpp-dev_"],
        };
        remove_staged_debs(&stage_root, &mpp).unwrap();
        // mpp's debs are gone; the unrelated rga deb stays.
        assert!(!stage_root.join("librockchip-mpp1_1.5.0_arm64.deb").exists());
        assert!(!stage_root.join("librockchip-mpp-dev_1.5.0_arm64.deb").exists());
        assert!(stage_root.join("librga2_2.2.0_arm64.deb").exists());
    }

    #[test]
    fn package_staged_requires_every_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let stage_root = tmp.path().join("userspace");
        std::fs::create_dir_all(&stage_root).unwrap();
        let pin = GitPin { reference: "c".into(), commit: "c".into() };
        let mpp = Package {
            name: "mpp",
            source: "",
            pin: &pin,
            deb_prefixes: &["librockchip-mpp1_", "librockchip-mpp-dev_"],
        };
        // Only the runtime lib present: a crash before the -dev deb → NOT staged.
        std::fs::write(stage_root.join("librockchip-mpp1_1.5.0_arm64.deb"), b"x").unwrap();
        assert!(!package_staged(&stage_root, &mpp).unwrap());
        // Both binaries present → staged (resume may skip).
        std::fs::write(stage_root.join("librockchip-mpp-dev_1.5.0_arm64.deb"), b"x").unwrap();
        assert!(package_staged(&stage_root, &mpp).unwrap());
    }

    #[test]
    fn collect_stages_debs_in_package_order() {
        let tmp = tempfile::tempdir().unwrap();
        let stage_root = tmp.path().join("userspace");
        let out = tmp.path().join("out");
        std::fs::create_dir_all(&stage_root).unwrap();
        for n in [
            "librockchip-mpp1_1_arm64.deb",
            "librga2_2_arm64.deb",
            "librga-dev_2_arm64.deb",
        ] {
            std::fs::write(stage_root.join(n), b"x").unwrap();
        }
        let mpp_pin = GitPin {
            reference: "c".into(),
            commit: "c".into(),
        };
        let rga_pin = mpp_pin.clone();
        let packages = vec![
            Package {
                name: "mpp",
                source: "",
                pin: &mpp_pin,
                deb_prefixes: &["librockchip-mpp1_"],
            },
            Package {
                name: "librga",
                source: "",
                pin: &rga_pin,
                deb_prefixes: &["librga2_", "librga-dev_"],
            },
        ];
        let sink = |_: crate::event::Event| {};
        let step = Step::start(&sink, "userspace");
        let arts = collect(&packages, &stage_root, &out, &step).unwrap();
        assert_eq!(arts.debs.len(), 3);
        // mpp deb staged before the rga debs (package order preserved).
        assert!(arts.debs[0].file_name().unwrap().to_str().unwrap().starts_with("librockchip-mpp1_"));
        assert!(arts.debs.iter().all(|p| p.exists()));
    }

    #[test]
    fn package_output_manifest_covers_commit_suite_and_arch() {
        fn mpp(p: &GitPin) -> Package<'_> {
            Package {
                name: "mpp",
                source: "",
                pin: p,
                deb_prefixes: &["librockchip-mpp1_"],
            }
        }
        let pin = |c: &str| GitPin { reference: "r".into(), commit: c.into() };
        let p1 = pin("c1");
        let sig = |pkg: &Package, suite: &str, arch: &str| {
            package_output_manifest(pkg, suite, arch, None).signature
        };
        let base = sig(&mpp(&p1), "forky", "arm64");
        // Stable under identical inputs.
        assert_eq!(base, sig(&mpp(&p1), "forky", "arm64"));
        // A source-pin bump reaches the output signature through the fetch dependency.
        let p2 = pin("c2");
        assert_ne!(base, sig(&mpp(&p2), "forky", "arm64"));
        // Suite (the sandbox toolchain proxy) and arch each split the key.
        assert_ne!(base, sig(&mpp(&p1), "sid", "arm64"));
        assert_ne!(base, sig(&mpp(&p1), "forky", "armhf"));
        // A patch series reaches the output signature through the tree dependency.
        let patches = PatchInputs {
            profile: "rk3588-accel",
            commit: "pc1",
            patches: PatchSeries::Pinned,
        };
        assert_ne!(
            base,
            package_output_manifest(&mpp(&p1), "forky", "arm64", Some(&patches)).signature
        );
        // Distinct packages never share an output entry (their node names differ).
        let rga = Package { name: "librga", source: "", pin: &p1, deb_prefixes: &["librga2_"] };
        assert_ne!(base, sig(&rga, "forky", "arm64"));
    }
}
