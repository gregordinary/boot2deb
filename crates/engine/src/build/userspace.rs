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

use crate::build::{
    self, deb_names, stage_artifact, BuildEnv, PatchScope, PatchSource, SeriesIdentity,
};
use crate::error::EngineError;
use crate::event::{EventSink, Step};
use crate::sandbox::{BuildRoot, BuildRootSpec, BuildSandbox, SandboxRun};
use boot2deb_core::lock::{Lock, UserspacePin};
use boot2deb_core::model::UserspaceTree;
use std::path::{Path, PathBuf};

/// Stage-recipe version for a userspace tree signature: bump when the
/// fetch/build logic that shapes a reused tree changes.
const FETCH_STAGE_VERSION: u32 = 1;

/// Where this stage's package trees and its produced `.deb`s live under `work_dir`
/// (`<work_dir>/userspace`) — one directory per package inside it. Exposed for the same
/// reason [`kernel::tree_dir`](crate::build::kernel::tree_dir) is: a reader of the tree
/// — [`crate::shell`], which starts an interactive session in it — should not restate
/// the layout literal.
pub fn stage_dir(work_dir: &Path) -> PathBuf {
    work_dir.join("userspace")
}

/// Stage-recipe version for a userspace **output** signature (Tier-2 artifact cache):
/// bump when the build/package logic changes a package's `.deb`s in a way the
/// folded inputs do not already capture.
const OUTPUT_STAGE_VERSION: u32 = 2;

/// Where the lookup probe appends, relative to the stage root — a bound host path, so
/// the record outlives the cage that wrote it and is not itself in the overlay under
/// investigation.
const LOOKUP_PROBE_REPORT: &str = "lookup-probe.log";

/// Debian build-deps every userspace tree's packaging needs. What one tree needs *on
/// top* of these is its own [`build_deps`](boot2deb_core::model::UserspaceTree::build_deps).
const USERSPACE_DEPS: &[&str] = &[
    "cmake",
    "meson",
    "ninja-build",
    "pkg-config",
    "dh-exec",
    "libdrm-dev",
];

/// The build-dependency set this stage layers over the sandbox base.
///
/// One function, read by the [`BuildRootSpec`] that stages the layer *and* by
/// [`output_manifest_for`], which keys every package of the stage on it — so a package
/// cannot reach a `./configure` without reaching the key. [`crate::shell`] reads it too,
/// so an interactive session lands in the root this stage compiles in rather than one
/// that resembles it.
///
/// A tree's own `build_deps` are an increment to the layer for the *whole stage*, not a
/// per-package one, which is why they are an input to every tree's signature and not
/// only their own: those `.pc` files are present in the root every tree's `cmake` and
/// `meson` runs probe, whether or not the tree that asked for them is being built.
///
/// De-duplicated and sorted, so two trees naming one dependency layer it once and the
/// declaration order cannot move a cache key.
pub fn layer_packages(trees: &[UserspaceTree]) -> Vec<String> {
    let mut deps: Vec<String> = USERSPACE_DEPS.iter().map(|d| (*d).to_string()).collect();
    for t in trees {
        deps.extend(t.build_deps.iter().cloned());
    }
    deps.sort();
    deps.dedup();
    deps
}

/// `DEB_CFLAGS_APPEND` for the MPP/RGA builds. The tsukumijima forks pre-date
/// gcc-14's stricter defaults and trip `-Werror` on K&R empty-paren prototypes;
/// demoting these three back to warnings lets the build proceed without altering
/// the produced binaries' behavior.
const RELAX_CFLAGS: &str =
    "-Wno-error=incompatible-pointer-types -Wno-error=int-conversion -Wno-error=implicit-function-declaration";

/// One buildable userspace package: the tree as the SoC declares it, where to fetch it,
/// and the exact commit the lock pins.
///
/// The declaration is carried whole rather than copied field by field, so a tree that
/// gains a knob reaches the stage without this type changing.
struct Package<'a> {
    /// The SoC's `[[userspace]]` entry: name, `.deb`s, patch scope, variant filter.
    tree: &'a UserspaceTree,
    /// Clone source (git URL or local checkout path; a local checkout is far faster).
    source: &'a str,
    /// The lock's pin for it.
    pin: &'a UserspacePin,
}

impl Package<'_> {
    /// The tree's name — its directory under `<work>/userspace/`, its cache node, and
    /// its label in logs.
    fn name(&self) -> &str {
        &self.tree.name
    }

    /// The `.deb` file-name prefixes this package produces. A produced file is
    /// `<name>_<version>_<arch>.deb`, so the trailing underscore is what keeps
    /// `librga2_` from also matching `librga2-foo_`.
    ///
    /// Resume skips the package only when **every** prefix is already staged: a crash
    /// between a multi-binary package's outputs must not look finished.
    fn deb_prefixes(&self) -> Vec<String> {
        self.tree.debs.iter().map(|d| format!("{d}_")).collect()
    }
}

/// Filesystem inputs for the userspace stage.
pub struct UserspaceOptions<'a> {
    /// The userspace trees this build compiles, as the SoC declares them and resolution
    /// narrowed them: an optional tree is here only when the build asked for it.
    pub trees: &'a [UserspaceTree],
    /// Per-tree clone-source overrides (`(name, source)`), from `--userspace-src`. A
    /// tree absent here is cloned from its declared `git`; a local checkout is far
    /// faster than a fresh clone.
    pub sources: &'a [(String, String)],
    /// The `userspace` patch scope's checkout + pin — the MPP CMA fix. The tree that
    /// declares [`patched`](UserspaceTree::patched) receives it; the rest build
    /// unpatched upstream. `None` when the resolved kernel names no patch series.
    pub patches: Option<PatchSource<'a>>,
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
/// Reads only the [`Lock`] for the source pins. The `sandbox` supplies the userland
/// for the build's suite + arch ([`RootlessSandbox`](crate::sandbox::RootlessSandbox));
/// this stage is agnostic to the backend. A package whose `.deb`s are already staged
/// in the work dir is skipped (resume).
pub fn build_userspace(
    lock: &Lock,
    opts: &UserspaceOptions,
    arch: &str,
    env: &BuildEnv,
    sandbox: &dyn BuildSandbox,
    sink: &dyn EventSink,
) -> Result<UserspaceArtifacts, EngineError> {
    let step = Step::start(sink, "userspace");
    let stage_root = stage_dir(opts.work_dir);

    // The CLI schedules this stage only for a media-accel build, whose lock pins
    // the userspace sources; reaching it without pins is an internal bug.
    if lock.userspace.is_empty() {
        return Err(EngineError::MissingMediaAccelPins { stage: "userspace" });
    }
    let userspace = &lock.userspace;

    // One entry per tree the build resolved *and* the lock pinned. A tree the SoC does
    // not declare is not a missing input — it is a part that has no such tree (no vendor
    // `mpp_service` to talk to, or a GPU whose userspace is Mesa from the mirror) — and
    // an optional tree this build did not ask for is likewise simply absent. Either way
    // nothing downstream expects its `.deb`.
    let packages: Vec<Package> = opts
        .trees
        .iter()
        .filter_map(|tree| {
            let pin = userspace.iter().find(|p| p.name == tree.name)?;
            Some(Package {
                tree,
                source: opts
                    .sources
                    .iter()
                    .find(|(name, _)| name == &tree.name)
                    .map(|(_, src)| src.as_str())
                    .unwrap_or(&tree.git),
                pin,
            })
        })
        .collect();

    // Patch context: the series' `userspace` scope — the MPP CMA fix — is
    // applied to the MPP tree; librga/libmali build unpatched upstream. The series +
    // its pin are the same for the whole build; [`receives_userspace_patches`] decides
    // which package's tree gets the series (and folds it into that tree's signature).
    // In co-dev mode the userspace series fingerprint is folded into the MPP tree
    // signature so an edited userspace patch restamps it; `series_fp` lives
    // in the ctx so the borrowed [`SeriesIdentity::Dev`] outlives the package loop.
    let series_fp = build::dev_series_fingerprint(opts.patches, PatchScope::Userspace);
    let patch_ctx = UserspacePatchCtx {
        patches: opts.patches,
        series_fp,
    };

    // Tier-2 output cache: decide each package's hit/miss up front, so a
    // fully-cached userspace stage skips the sandbox bootstrap entirely (the real
    // payoff after a `clean --sandbox`). The per-package output signature folds the
    // fetch pin + patch series (MPP) + build recipe + suite/arch (
    // `package_output_manifest`).
    let store = opts
        .store
        .map(crate::artstore::ArtifactStore::open)
        .transpose()?;
    // The userspace node runs only for a media-accel image build, which pins a rootfs.
    let suite = lock
        .rootfs
        .as_ref()
        .expect("the userspace node runs only for an image build, which pins a rootfs")
        .suite
        .as_str();
    let out_sigs: Vec<String> = packages
        .iter()
        .map(|p| {
            let pi = patch_ctx.inputs_for(p.tree);
            package_output_manifest(p, suite, arch, &env.sandbox_id, opts.trees, pi.as_ref())
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
    // The build root is acquired only where a compile happens. A fully-cached stage
    // restores every `.deb` from the artifact store and never enters the sandbox, so it
    // neither bootstraps the base nor stages an increment — which is also why the
    // layered shape costs a warm rebuild nothing.
    let root = if all_cached {
        step.log("all userspace packages cached — skipping sandbox setup");
        None
    } else {
        sandbox.ensure_ready(&step)?;
        step.progress(15);
        let deps = layer_packages(opts.trees);
        let deps: Vec<&str> = deps.iter().map(String::as_str).collect();
        Some(sandbox.build_root(
            &BuildRootSpec {
                packages: &deps,
                // The userspace packages build against the suite alone: they are the
                // first stage to compile, so there is nothing of this build's own for
                // them to depend on.
                pool: None,
                stage: "userspace",
            },
            &step,
        )?)
    };
    step.progress(25);

    // Build (or restore) each package, spreading coarse progress across 25..90.
    let span = 65u8;
    std::fs::create_dir_all(&stage_root).map_err(|s| EngineError::io(&stage_root, s))?;
    for (i, pkg) in packages.iter().enumerate() {
        let restored = if cached[i] {
            let store = store.as_ref().expect("cached implies a store");
            // The restore lands beside whatever the shared stage dir already
            // holds, and `collect` copies *every* matching name — sweep the
            // package's stale-version `.deb`s first so a leftover from a build
            // at different pins cannot ride along with the restored set.
            build::purge_stage_debs(&stage_root, &prefix_refs(&pkg.deb_prefixes()))?;
            store
                .restore(&node_name(pkg), &out_sigs[i], &stage_root)?
                .inspect(|_| {
                    // Per package, not per step: this stage is the one that can restore
                    // some of its `.deb`s and compile the rest, so it reports both.
                    step.restored();
                    step.log(format!("{}: restored from artifact cache", pkg.name()))
                })
                .is_some()
        } else {
            false
        };
        if !restored {
            // `build_one` is reached only on a miss, a miss implies `!all_cached`, and
            // that is exactly the branch that acquired the root — so a compile always
            // has one and a fully-cached stage never stages one.
            let root = root
                .as_ref()
                .expect("a cache miss implies the branch that acquires the build root");
            build_one(pkg, &stage_root, env, root, &patch_ctx, &step)?;
            step.compiled();
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

/// Fetch (if needed), apply the package's patch scope, and build one package in `root`,
/// leaving its `.deb`s in `stage_root`.
///
/// `root` is the stage's build root — the sandbox base plus the userspace build-deps —
/// so `dpkg-buildpackage` sees exactly the packages this stage declared and nothing an
/// earlier build left behind.
fn build_one(
    pkg: &Package,
    stage_root: &Path,
    env: &BuildEnv,
    root: &BuildRoot,
    patches: &UserspacePatchCtx,
    step: &Step,
) -> Result<(), EngineError> {
    let tree = stage_root.join(pkg.name());
    let man = package_signature(pkg, patches.inputs_for(pkg.tree).as_ref());
    // Skip the whole build only when the fetched+patched tree still matches the
    // locked commit *and* patch series and **all** of the package's `.deb`s are
    // staged: a crash between compile and staging re-runs the
    // package rather than skipping to a later stage that misses a `.deb`.
    if crate::signature::is_fresh(&tree, &man) && package_staged(stage_root, pkg)? {
        step.log(format!("{}: already built, skipping", pkg.name()));
        return Ok(());
    }
    build::reuse_or_refresh_tree(&tree, &man, pkg.name(), step, || {
        // Purge stale-version `.deb`s so `collect` cannot ship an old one and
        // `package_staged` cannot be fooled by it.
        build::purge_stage_debs(stage_root, &prefix_refs(&pkg.deb_prefixes()))?;
        build::fetch_commit(
            pkg.source,
            &pkg.pin.reference,
            &pkg.pin.commit,
            pkg.name(),
            &tree,
            step,
        )?;
        // Apply the series' `userspace` scope onto the fetched base — the MPP CMA
        // fix, mirroring the kernel/ffmpeg stages' clone→apply flow. Only
        // MPP is patched; librga/libmali are unpatched upstream. The series is
        // materialized in the patches repo (durable base + patch), so the pin
        // is a re-fetchable tag rather than a locally-authored commit.
        if receives_userspace_patches(pkg.tree) {
            apply_patches(pkg, &tree, patches, step).inspect_err(|_| {
                // Never leave a half-patched, unstamped tree a resume would trust.
                let _ = std::fs::remove_dir_all(&tree);
            })?;
        }
        Ok(())
    })?;

    if let Some(variant) = &pkg.tree.targets_filter {
        filter_targets(&tree.join("debian/targets"), variant, step)?;
    }

    // Deterministic build timestamp from the locked *base* commit. For
    // MPP the tree now carries the patch series, so — like the kernel/ffmpeg stages —
    // read the base pin explicitly (still reachable after `git am`), not HEAD.
    let epoch = crate::git::commit_epoch(&tree, &pkg.pin.commit).ok();
    let dpkg_env = dpkg_env(env.jobs(), epoch);
    let build: Vec<String> = ["dpkg-buildpackage", "-us", "-uc", "-b"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    // The stage that has never lost a header, instrumented on the same terms as the one
    // that has: this root is layered and emulated exactly as ffmpeg's is, and it stays
    // a control only for as long as something is watching it.
    let report = stage_root.join(LOOKUP_PROBE_REPORT);
    let binds = [stage_root.to_path_buf()];
    let context = format!("dpkg-buildpackage {}", pkg.name());
    let spec = SandboxRun {
        work: &tree,
        binds: &binds,
        env: &dpkg_env,
        argv: &build,
        context: &context,
        probe: Some(&report),
    };
    root.run(&spec, step)?;
    step.log(format!("{}: built", pkg.name()));
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
    // `out_dir` accumulates staged `.deb`s across builds, and the ffmpeg stage
    // re-scans it for these packages by prefix + highest version
    // (`required_userspace_debs`) — sweep each package's stale versions before
    // staging the fresh set, so a leftover from earlier pins can never outrank
    // it there. All sweeps run before any staging: prefixes may overlap
    // across packages, and a later sweep must not remove an earlier stage copy.
    for pkg in packages {
        build::purge_stage_debs(out_dir, &prefix_refs(&pkg.deb_prefixes()))?;
    }
    let mut debs = Vec::new();
    let mut seen = Vec::new();
    for pkg in packages {
        for name in select_debs(&names, &pkg.deb_prefixes()) {
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

/// Whether a userspace tree receives the series' `userspace` patch scope.
///
/// The tree says so itself ([`UserspaceTree::patched`]) rather than the stage comparing
/// a name: on the RK35xx family that is MPP alone — the CMA fix
/// (`allocator_dma_heap`) — and librga builds unpatched upstream, but a second family's
/// patched tree needs a config edit and no code. Read by the stage *and* by `why-rebuild`
/// ([`crate::plan`]), so the two agree on which tree's signature folds the series.
pub fn receives_userspace_patches(tree: &UserspaceTree) -> bool {
    tree.patched
}

/// The patch inputs folded into a userspace package's tree signature when it
/// receives the `userspace` scope — the series name, its pinned commit, and
/// the applied-series identity, mirroring how the kernel/ffmpeg tree signatures fold
/// their series (`build::fold_patch_series`). A package that carries no patch folds
/// none of this.
pub struct PatchInputs<'a> {
    /// The lock's patch pin (series + `patches` repo commit), or `None` when the
    /// build's kernel names no patch series and nothing is applied.
    pub pin: Option<&'a boot2deb_core::lock::PatchesPin>,
    /// The applied series' identity: pinned by commit, or (co-dev) the live-series
    /// fingerprint so an edited userspace patch restamps the MPP tree.
    pub patches: SeriesIdentity<'a>,
}

/// The userspace stage's patch context: the checkout + pin to apply from — shared
/// across the packages, applied only to the ones [`receives_userspace_patches`]
/// selects. `None` when the build's kernel names no patch series.
struct UserspacePatchCtx<'a> {
    /// The checkout and pin the `userspace` scope is read from, or `None` when the
    /// build applies no patches.
    patches: Option<build::PatchSource<'a>>,
    /// The co-dev live-series fingerprint of the `userspace` scope (empty in pinned
    /// mode), folded into the MPP tree signature so an edited userspace patch restamps
    /// it.
    series_fp: Vec<String>,
}

impl UserspacePatchCtx<'_> {
    /// The [`PatchInputs`] a package folds into its signature — `Some` iff it
    /// receives the userspace scope, `None` otherwise.
    fn inputs_for(&self, tree: &UserspaceTree) -> Option<PatchInputs<'_>> {
        receives_userspace_patches(tree).then_some(PatchInputs {
            pin: self.patches.map(|p| p.pin),
            patches: build::series_identity(self.patches, &self.series_fp),
        })
    }
}

/// Apply the series' `userspace` scope onto `pkg`'s fetched tree in place,
/// via the shared [`apply_series_scope`](crate::build::apply_series_scope) — the
/// same pin-enforcement + verify-applies gate the kernel/ffmpeg stages use. No
/// kernel-range gate here (that guards the kernel node; the series is validated
/// there). Logs the applied count.
fn apply_patches(
    pkg: &Package,
    tree: &Path,
    ctx: &UserspacePatchCtx,
    step: &Step,
) -> Result<(), EngineError> {
    let target = format!("{} @ {}", pkg.name(), pkg.pin.reference);
    let n = build::apply_series_scope(
        &build::ApplyScope {
            tree,
            patches: ctx.patches,
            scope: build::PatchScope::Userspace,
            target: &target,
            gate_reference: None,
        },
        step,
    )?;
    if let Some(p) = ctx.patches {
        step.log(format!(
            "{}: applied {n} userspace patch(es) ({})",
            pkg.name(),
            p.pin.series.join(", ")
        ));
    }
    Ok(())
}

/// Tier-1 signature manifest of a fetched userspace source tree, keyed by
/// package `name`, its locked `commit` (which content-addresses the fetched tree),
/// and — when the package receives the `userspace` scope — the patch series
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
        build::fold_patch_series(&mut b, p.pin, p.patches);
    }
    b.manifest()
}

/// The signature manifest for a resolved [`Package`], via [`signature_manifest`]; the
/// caller passes the package's [`PatchInputs`] (`Some` for MPP, `None` otherwise).
fn package_signature(
    pkg: &Package,
    patches: Option<&PatchInputs>,
) -> crate::signature::SignatureManifest {
    signature_manifest(pkg.name(), &pkg.pin.commit, patches)
}

/// The artifact-store node name for a package's `.deb`s, e.g. `userspace:mpp`
/// (matching the Tier-1 per-package node name).
fn node_name(pkg: &Package) -> String {
    node_name_for(pkg.name())
}

/// The same, from a bare tree name — for [`why-rebuild`](crate::plan), which predicts
/// against the store before any `Package` exists.
///
/// Public because the prediction and the store lookup must be the *same string*: the
/// store is keyed by `(node, signature)`, so a prediction computed under a different
/// name would answer a question about an entry no build ever wrote.
pub fn node_name_for(name: &str) -> String {
    format!("userspace:{name}")
}

/// The Tier-2 output signature manifest of a userspace package's `.deb`s from
/// primitives. It folds the Tier-1 fetch signature ([`signature_manifest`], the
/// commit + MPP patch series) as a dependency, then the build recipe: the
/// gcc-14 warning relaxation, the target arch, the **suite**, and `sandbox` — the
/// package compiles inside the target-arch sandbox, so what produced it is that
/// sandbox's toolchain. The suite names the userland; `sandbox`
/// ([`BuildEnv::sandbox_id`](crate::build::BuildEnv::sandbox_id)) identifies the
/// *instance* of it — which mirror it was bootstrapped from, and which `qemu-user`
/// executes its compiler — so a snapshot-pinned build and a live-mirror build never
/// restore each other's `.deb`s. Libmali also folds its variant filter. On a
/// signature hit the store restores this package's `.deb`s rather than rebuilding; a
/// patch change reaches this output signature through the folded tree dependency. A
/// tree with a `targets_filter` folds that too, since it decides what was compiled.
///
/// Public and keyed by primitives (not a `Package`) so the ffmpeg stage recomputes
/// the mpp/librga dependency signatures from the lock and folds them into its own
/// output key — an ffmpeg build links against those `.deb`s, so a change to
/// them must invalidate the cached ffmpeg deb.
pub fn output_manifest_for(
    name: &str,
    commit: &str,
    suite: &str,
    arch: &str,
    sandbox_id: &str,
    trees: &[UserspaceTree],
    patches: Option<&PatchInputs>,
) -> crate::signature::SignatureManifest {
    let tree_sig = signature_manifest(name, commit, patches).signature();
    let mut b = crate::signature::SignatureBuilder::new(
        &format!("userspace:{name}:out"),
        OUTPUT_STAGE_VERSION,
    );
    b.fold_dep(&tree_sig)
        .fold_scalar("relax_cflags", RELAX_CFLAGS)
        .fold_scalar("suite", suite)
        .fold_scalar("arch", arch)
        .fold_scalar("sandbox", sandbox_id)
        // The base root's identity above covers what the sandbox is; this covers what
        // was layered over it. Both reach the compile, so both reach the key — see
        // [`layer_packages`] for why one tree's `build_deps` are an input to every
        // package of the stage rather than to its own alone.
        .fold_set("build_deps", &layer_packages(trees));
    if let Some(variant) = trees
        .iter()
        .find(|t| t.name == name)
        .and_then(|t| t.targets_filter.as_deref())
    {
        b.fold_scalar("targets_filter", variant);
    }
    b.manifest()
}

/// The Tier-2 output signature manifest of a resolved [`Package`]'s `.deb`s, via
/// [`output_manifest_for`] with the package's name + pinned commit.
fn package_output_manifest(
    pkg: &Package,
    suite: &str,
    arch: &str,
    sandbox_id: &str,
    trees: &[UserspaceTree],
    patches: Option<&PatchInputs>,
) -> crate::signature::SignatureManifest {
    output_manifest_for(
        pkg.name(),
        &pkg.pin.commit,
        suite,
        arch,
        sandbox_id,
        trees,
        patches,
    )
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
    let paths: Vec<PathBuf> = select_debs(&names, &pkg.deb_prefixes())
        .iter()
        .map(|n| stage_root.join(n))
        .collect();
    let refs: Vec<(&str, &Path)> = paths.iter().map(|p| ("deb", p.as_path())).collect();
    store.put(node, sig, &refs)?;
    step.log(format!(
        "{}: stored {} .deb(s) to the artifact cache",
        pkg.name(),
        refs.len()
    ));
    Ok(())
}

/// True only if **all** of `pkg`'s `.deb`s already sit in `stage_root` (resume
/// check). Requiring every prefix — not just one — means a crash partway through a
/// multi-binary package's outputs re-runs it rather than skipping to a later stage
/// that then fails on the missing `.deb`.
fn package_staged(stage_root: &Path, pkg: &Package) -> Result<bool, EngineError> {
    if !stage_root.exists() {
        return Ok(false);
    }
    let names = deb_names(stage_root)?;
    Ok(pkg
        .deb_prefixes()
        .iter()
        .all(|prefix| names.iter().any(|n| n.starts_with(prefix))))
}

/// The env for a `dpkg-buildpackage` run: the gcc-14 warning relaxation, a
/// `parallel=` matching the resolved job count, and — when known — the locked
/// commit's `SOURCE_DATE_EPOCH` for a reproducible build timestamp. Pure,
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

/// Borrow a prefix list as `&str`s, for the helpers that take a slice of them.
fn prefix_refs(prefixes: &[String]) -> Vec<&str> {
    prefixes.iter().map(String::as_str).collect()
}

/// `.deb` file names in `names` matching any of `prefixes`. Pure selection so
/// collection is testable without a build.
fn select_debs<'a>(names: &'a [String], prefixes: &[String]) -> Vec<&'a String> {
    names
        .iter()
        .filter(|n| prefixes.iter().any(|p| n.starts_with(p)))
        .collect()
}

/// Rewrite a tree's `debian/targets` to only the lines naming
/// [`targets_filter`](UserspaceTree::targets_filter), skipping the rest of a vendor
/// variant matrix — libmali's is ~140 GPU variants, of which one board needs one.
///
/// A no-op if the file is absent; if the filter matches nothing, the file is left
/// untouched (build all) with a warning, rather than producing an empty target set.
fn filter_targets(targets: &Path, variant: &str, step: &Step) -> Result<(), EngineError> {
    if !targets.exists() {
        return Ok(());
    }
    let content = std::fs::read_to_string(targets).map_err(|s| EngineError::io(targets, s))?;
    let filtered = keep_variant_lines(&content, variant);
    if filtered.trim().is_empty() {
        step.emit(
            crate::event::Stream::Stderr,
            crate::event::LogOrigin::Stage,
            format!("warning: targets filter '{variant}' matched nothing; building all"),
        );
        return Ok(());
    }
    std::fs::write(targets, &filtered).map_err(|s| EngineError::io(targets, s))?;
    let kept = filtered.lines().count();
    step.log(format!("filtered to {kept} target(s) matching {variant}"));
    Ok(())
}

/// Keep the lines of `content` containing `variant` (each newline-terminated).
/// Pure, so the filter is testable.
fn keep_variant_lines(content: &str, variant: &str) -> String {
    content
        .lines()
        .filter(|l| l.contains(variant))
        .map(|l| format!("{l}\n"))
        .collect()
}

#[cfg(test)]
mod tests {

    /// A tree fixture: the declaration the SoC would author, minus the knobs a
    /// signature test does not exercise.
    fn tree(name: &str) -> UserspaceTree {
        UserspaceTree {
            name: name.into(),
            git: "s".into(),
            git_ref: "master".into(),
            debs: vec![format!("lib{name}")],
            links: Vec::new(),
            ffmpeg_flag: None,
            ffmpeg_requires: Vec::new(),
            patched: name == "mpp",
            optional: false,
            build_deps: Vec::new(),
            targets_filter: None,
        }
    }

    /// A pin fixture for a tree.
    fn pin_at(name: &str, commit: &str) -> UserspacePin {
        UserspacePin {
            name: name.into(),
            source: "s".into(),
            reference: "master".into(),
            commit: commit.into(),
        }
    }
    use super::*;

    #[test]
    fn select_debs_matches_prefixes() {
        let names = vec![
            "librockchip-mpp1_1.5.0_arm64.deb".to_string(),
            "librockchip-mpp-dev_1.5.0_arm64.deb".to_string(),
            "librga2_2.2.0_arm64.deb".to_string(),
            "unrelated_1_arm64.deb".to_string(),
        ];
        let mpp = select_debs(
            &names,
            &[
                "librockchip-mpp1_".to_string(),
                "librockchip-mpp-dev_".to_string(),
            ],
        );
        assert_eq!(mpp.len(), 2);
        assert!(mpp.iter().all(|n| n.starts_with("librockchip-mpp")));
        let rga = select_debs(&names, &["librga2_".to_string()]);
        assert_eq!(rga.len(), 1);
        assert!(select_debs(&names, &["nonexistent_".to_string()]).is_empty());
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
        let kept = keep_variant_lines(content, "aarch64-linux-gnu/libmali-valhall-g610");
        assert_eq!(kept.lines().count(), 2);
        assert!(kept.lines().all(|l| l.contains("valhall-g610")));
        // An unmatched variant yields an empty set (caller warns + skips).
        assert!(keep_variant_lines(content, "libmali-nonexistent").is_empty());
    }

    #[test]
    fn package_signature_tracks_commit_and_name() {
        let (mpp_tree, rga_tree) = (tree("mpp"), tree("librga"));
        let pin_a = pin_at("mpp", "c1");
        let pin_b = pin_at("mpp", "c2");
        let rga_pin = pin_at("librga", "c1");
        let mpp_a = Package {
            tree: &mpp_tree,
            source: "",
            pin: &pin_a,
        };
        let mpp_a2 = Package {
            tree: &mpp_tree,
            source: "x",
            pin: &pin_a,
        };
        let mpp_b = Package {
            tree: &mpp_tree,
            source: "",
            pin: &pin_b,
        };
        let rga_a = Package {
            tree: &rga_tree,
            source: "",
            pin: &rga_pin,
        };
        // Same commit → same signature (source/prefixes are not tree-shaping).
        assert_eq!(
            package_signature(&mpp_a, None),
            package_signature(&mpp_a2, None)
        );
        // A commit bump invalidates the reused tree + debs.
        assert_ne!(
            package_signature(&mpp_a, None),
            package_signature(&mpp_b, None)
        );
        // Different packages at the same commit never collide.
        assert_ne!(
            package_signature(&mpp_a, None),
            package_signature(&rga_a, None)
        );
    }

    #[test]
    fn patch_series_folds_into_the_patched_package_signature() {
        // The tree says so itself, rather than the stage comparing a name.
        assert!(receives_userspace_patches(&tree("mpp")));
        assert!(!receives_userspace_patches(&tree("librga")));
        assert!(!receives_userspace_patches(&tree("libmali")));

        let mpp_tree = tree("mpp");
        let pin = pin_at("mpp", "750e76ec2d9287babfaf08c8bf395ebc5e8778ea");
        let mpp = Package {
            tree: &mpp_tree,
            source: "",
            pin: &pin,
        };
        let pin_at = |commit: &str| boot2deb_core::lock::PatchesPin {
            series: vec!["rk3588-accel".into()],
            source: "https://example.invalid/patches.git".into(),
            reference: "main".into(),
            commit: commit.into(),
        };
        let (pin1, pin2) = (pin_at("p1"), pin_at("p2"));
        let p1 = PatchInputs {
            pin: Some(&pin1),
            patches: SeriesIdentity::Pinned,
        };
        let p2 = PatchInputs {
            pin: Some(&pin2),
            patches: SeriesIdentity::Pinned,
        };
        let empty: Vec<String> = vec![];
        let p1_dev = PatchInputs {
            pin: Some(&pin1),
            patches: SeriesIdentity::Dev(&empty),
        };
        // Folding a patch series changes the tree signature vs an unpatched fetch...
        assert_ne!(
            package_signature(&mpp, None),
            package_signature(&mpp, Some(&p1))
        );
        // ...a patch-pin bump changes it again (a patch change restamps the tree)...
        assert_ne!(
            package_signature(&mpp, Some(&p1)),
            package_signature(&mpp, Some(&p2))
        );
        // ...and a co-dev build never shares a stamp with a pinned one.
        assert_ne!(
            package_signature(&mpp, Some(&p1)),
            package_signature(&mpp, Some(&p1_dev))
        );
        // ...and a co-dev userspace-patch content change restamps the MPP tree.
        let fp1 = vec!["media-accel/userspace/001.patch=aaa".to_string()];
        let fp2 = vec!["media-accel/userspace/001.patch=bbb".to_string()];
        let dev1 = PatchInputs {
            pin: Some(&pin1),
            patches: SeriesIdentity::Dev(&fp1),
        };
        let dev2 = PatchInputs {
            pin: Some(&pin1),
            patches: SeriesIdentity::Dev(&fp2),
        };
        assert_ne!(
            package_signature(&mpp, Some(&dev1)),
            package_signature(&mpp, Some(&dev2))
        );
        // A build with no patch series signs distinctly from every patched variant and
        // from an unpatched fetch — its MPP tree really is unpatched, but the node still
        // records that the scope was considered.
        let none = PatchInputs {
            pin: None,
            patches: SeriesIdentity::Pinned,
        };
        assert_ne!(
            package_signature(&mpp, Some(&none)),
            package_signature(&mpp, Some(&p1))
        );
    }

    #[test]
    fn collect_sweeps_stale_out_dir_versions_before_staging() {
        let tmp = tempfile::tempdir().unwrap();
        let stage_root = tmp.path().join("userspace");
        let out_dir = tmp.path().join("artifacts");
        std::fs::create_dir_all(&stage_root).unwrap();
        std::fs::create_dir_all(&out_dir).unwrap();
        // A previous build (at different pins) staged 1.6.0 into out_dir. The
        // ffmpeg stage selects from out_dir by highest version, so the stale
        // deb must not survive a fresh stage that produced 1.5.0.
        std::fs::write(out_dir.join("librockchip-mpp1_1.6.0_arm64.deb"), b"stale").unwrap();
        // Another stage's artifact in the shared out_dir stays untouched.
        std::fs::write(
            out_dir.join("u-boot-turing-rk1_2026.04_arm64.deb"),
            b"other",
        )
        .unwrap();
        std::fs::write(
            stage_root.join("librockchip-mpp1_1.5.0_arm64.deb"),
            b"fresh",
        )
        .unwrap();

        let mut mpp_tree = tree("mpp");
        mpp_tree.debs = vec!["librockchip-mpp1".into()];
        let pin = pin_at("mpp", "c");
        let mpp = Package {
            tree: &mpp_tree,
            source: "",
            pin: &pin,
        };
        let sink = |_e: crate::event::Event| {};
        let step = Step::start(&sink, "userspace");
        let artifacts = collect(&[mpp], &stage_root, &out_dir, &step).unwrap();

        assert_eq!(
            artifacts.debs,
            vec![out_dir.join("librockchip-mpp1_1.5.0_arm64.deb")]
        );
        assert!(!out_dir.join("librockchip-mpp1_1.6.0_arm64.deb").exists());
        assert!(out_dir.join("u-boot-turing-rk1_2026.04_arm64.deb").exists());
    }

    #[test]
    fn package_staged_requires_every_prefix() {
        let tmp = tempfile::tempdir().unwrap();
        let stage_root = tmp.path().join("userspace");
        std::fs::create_dir_all(&stage_root).unwrap();
        let mut mpp_tree = tree("mpp");
        mpp_tree.debs = vec!["librockchip-mpp1".into(), "librockchip-mpp-dev".into()];
        let pin = pin_at("mpp", "c");
        let mpp = Package {
            tree: &mpp_tree,
            source: "",
            pin: &pin,
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
        let mut mpp_tree = tree("mpp");
        mpp_tree.debs = vec!["librockchip-mpp1".into()];
        let mut rga_tree = tree("librga");
        rga_tree.debs = vec!["librga2".into(), "librga-dev".into()];
        let mpp_pin = pin_at("mpp", "c");
        let rga_pin = pin_at("librga", "c");
        let packages = vec![
            Package {
                tree: &mpp_tree,
                source: "",
                pin: &mpp_pin,
            },
            Package {
                tree: &rga_tree,
                source: "",
                pin: &rga_pin,
            },
        ];
        let sink = |_: crate::event::Event| {};
        let step = Step::start(&sink, "userspace");
        let arts = collect(&packages, &stage_root, &out, &step).unwrap();
        assert_eq!(arts.debs.len(), 3);
        // mpp deb staged before the rga debs (package order preserved).
        assert!(arts.debs[0]
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with("librockchip-mpp1_"));
        assert!(arts.debs.iter().all(|p| p.exists()));
    }

    /// A stand-in for [`BuildEnv::sandbox_id`] in the signature tests: whatever it
    /// says, the same value must key the same and a different one must not.
    const SANDBOX: &str = "http://deb.debian.org/debian | qemu-aarch64 version 9.2.0";

    #[test]
    fn package_output_manifest_covers_commit_suite_and_arch() {
        let mpp_tree = tree("mpp");
        let mpp = |p| Package {
            tree: &mpp_tree,
            source: "",
            pin: p,
        };
        let p1 = pin_at("mpp", "c1");
        let trees = [mpp_tree.clone()];
        let sig = |pkg: &Package, suite: &str, arch: &str| {
            package_output_manifest(pkg, suite, arch, SANDBOX, &trees, None).signature
        };
        let base = sig(&mpp(&p1), "forky", "arm64");
        // Stable under identical inputs.
        assert_eq!(base, sig(&mpp(&p1), "forky", "arm64"));
        // A source-pin bump reaches the output signature through the fetch dependency.
        let p2 = pin_at("mpp", "c2");
        assert_ne!(base, sig(&mpp(&p2), "forky", "arm64"));
        // Suite (the sandbox toolchain proxy) and arch each split the key.
        assert_ne!(base, sig(&mpp(&p1), "sid", "arm64"));
        assert_ne!(base, sig(&mpp(&p1), "forky", "armhf"));
        // A patch series reaches the output signature through the tree dependency.
        let pc1 = boot2deb_core::lock::PatchesPin {
            series: vec!["rk3588-accel".into()],
            source: "https://example.invalid/patches.git".into(),
            reference: "main".into(),
            commit: "pc1".into(),
        };
        let patches = PatchInputs {
            pin: Some(&pc1),
            patches: SeriesIdentity::Pinned,
        };
        assert_ne!(
            base,
            package_output_manifest(&mpp(&p1), "forky", "arm64", SANDBOX, &trees, Some(&patches))
                .signature
        );
        // Distinct packages never share an output entry (their node names differ).
        let rga_tree = tree("librga");
        let rga = Package {
            tree: &rga_tree,
            source: "",
            pin: &p1,
        };
        assert_ne!(base, sig(&rga, "forky", "arm64"));
    }

    /// The sandbox that compiled a package is part of that package's identity.
    ///
    /// The suite names the userland but does not identify the instance of it: a
    /// snapshot-pinned sandbox and a live-mirror one carry the same suite and can
    /// carry different compilers, and on a cross host the `qemu-user` that executes
    /// that compiler is an input too. Both travel in the one `sandbox` scalar, so it
    /// is asserted as one: change it, and the store must not restore the other
    /// sandbox's `.deb`s.
    #[test]
    fn the_sandbox_that_compiled_a_package_splits_its_output_key() {
        let trees = [tree("mpp")];
        let sig = |sandbox: &str| {
            output_manifest_for("mpp", "c1", "forky", "arm64", sandbox, &trees, None).signature
        };
        let base = sig(SANDBOX);
        assert_eq!(base, sig(SANDBOX), "stable under an identical sandbox");
        // A snapshot-pinned userland is a different compiler than the live mirror's.
        assert_ne!(
            base,
            sig("https://snapshot.debian.org/archive/debian/20260628T083000Z/")
        );
        // So is the same userland under a different interpreter.
        assert_ne!(
            base,
            sig("http://deb.debian.org/debian | qemu-aarch64 version 10.0.0")
        );
    }

    /// What was *layered over* the sandbox splits the key too, for every package of the
    /// stage rather than for the one the layer was widened for.
    ///
    /// Enabling an optional tree adds *its* `build_deps` to the one build root the whole
    /// stage shares, so every other tree's `cmake`/`meson` probes see them whether or not
    /// that tree is what is being built. Without this fold, asking for it would change
    /// what the compile detects while leaving the artifact store answering the same
    /// question — which is how a `.deb` built in one environment gets restored into
    /// another.
    #[test]
    fn the_layer_over_that_sandbox_splits_it_for_every_package_in_the_stage() {
        let mut mali = tree("libmali");
        mali.optional = true;
        mali.build_deps = vec!["libgbm-dev".into(), "libwayland-dev".into()];
        let narrow = [tree("mpp"), tree("librga")];
        let wide = [tree("mpp"), tree("librga"), mali];
        let sig = |name: &str, trees: &[UserspaceTree]| {
            output_manifest_for(name, "c1", "forky", "arm64", SANDBOX, trees, None).signature
        };
        for name in ["mpp", "librga", "libmali"] {
            assert_ne!(
                sig(name, &narrow),
                sig(name, &wide),
                "{name} compiles in the widened root too"
            );
        }
        // And the fold is of the resolved set, not of a flag: the same declarations
        // yield the same key.
        assert_eq!(sig("mpp", &wide), sig("mpp", &wide));
        // Sorted and de-duplicated, which is what makes the key independent of the
        // SoC's declaration order.
        let mut base: Vec<String> = USERSPACE_DEPS.iter().map(|d| (*d).to_string()).collect();
        base.sort();
        assert_eq!(layer_packages(&narrow), base);
        // De-duplicated and sorted, so two trees naming one dependency layer it once and
        // the declaration order cannot move a key.
        let widened = layer_packages(&wide);
        assert!(widened.contains(&"libgbm-dev".to_string()));
        assert!(widened.windows(2).all(|w| w[0] < w[1]), "sorted and unique");
    }
}
