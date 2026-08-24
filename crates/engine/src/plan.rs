//! `why-rebuild` — explain, per compile node, whether its cached source
//! tree will be reused or rebuilt on the next `build`, and *why*, in terms of the
//! pinned inputs that changed since it was last stamped.
//!
//! This is "the payoff of structure": each compile stage stamps its
//! cloned+patched tree with a diffable [`SignatureManifest`], so `why-rebuild` can
//! recompute the current manifest from the lock and diff it against the stamp — a
//! rebuild is explained as "kernel.commit changed", not "the hash differs". Pure
//! except reading the on-disk stamps: no network, no build, no hardware.
//!
//! Both build caches are predicted, because a user planning around this output is
//! asking about the *expensive* case and the two tiers answer different halves of it:
//!
//! - **Tier 1** ([`NodeStatus`]) — the cloned+patched source tree, stamped in the work
//!   dir. A miss costs a clone and a patch run.
//! - **Tier 2** ([`ArtifactStatus`]) — the node's produced `.deb`s, in the durable
//!   store outside any work dir ([`crate::artstore`]). A *hit here skips the compile
//!   entirely*, so it dominates: a node can rebuild its tree and still not compile.
//!
//! **Nothing here is a second copy of the build's own answer.** Each verdict is computed
//! by calling the very function the stage keys its store lookup on; each node's tree
//! comes from the stage's own path helper
//! ([`kernel::tree_dir`](crate::build::kernel::tree_dir) and its siblings); and each
//! node's *name* comes from the stage's own [`NODE`](crate::build::kernel::NODE)
//! constant, because the artifact store is keyed by `(node, signature)` and a prediction
//! under a different name would answer a question about an entry no build ever wrote.
//!
//! What this module still decides for itself is the *set* of nodes and their order. That
//! is deliberate and not drift: `build` runs the rootfs and image nodes too, and `shell`
//! offers the packaging root, but neither is a node this predicts — the rootfs keys on a
//! live package solve (below), and the image node caches nothing.
//!
//! Out of scope, because it is not a static prediction: the rootfs node's cache keys
//! on the live package solve, which needs the mirror.

use crate::artstore::ArtifactStore;
use crate::signature::{self, RecordChange, SignatureManifest};
use boot2deb_core::lock::Lock;
use boot2deb_core::model::ResolvedBuild;
use std::path::{Path, PathBuf};

/// The Tier-2 artifact-cache verdict for one node: whether the next `build` restores
/// its stored `.deb`s instead of compiling them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactStatus {
    /// The store holds this node's current output signature: the compile is skipped.
    Hit,
    /// No entry for the current signature — the node compiles, then stores.
    Miss,
    /// The store is off for the predicted build (`--no-artifact-cache`), so the
    /// question does not arise.
    Disabled,
    /// The output signature could not be computed offline, so no claim is made.
    /// Distinct from [`Miss`](Self::Miss) on purpose: a spurious "will compile" is a
    /// wrong prediction, and this command exists to be right about that.
    Unknown,
}

/// The reuse decision `build` will make for one compile node's source tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NodeStatus {
    /// No previous build: the tree is absent, so it will be freshly built.
    Absent,
    /// The tree exists but carries no readable current-format signature stamp — an
    /// interrupted build or a foreign/older stamp. It will be rebuilt (fail-safe:
    /// an unverifiable tree is never trusted).
    Unstamped,
    /// The stamp matches the recomputed signature: the tree will be reused as-is
    /// (the compile step still re-runs).
    Reuse,
    /// The stamp differs: the tree will be rebuilt. The changes explain which pinned
    /// inputs moved (empty ⇒ only the stage-recipe version bumped, a build-logic
    /// change).
    Rebuild(Vec<RecordChange>),
}

/// One compile node's tree and the two cache decisions `build` will make for it.
#[derive(Debug, Clone)]
pub struct NodePlan {
    /// The build node (e.g. `kernel`, `userspace:mpp`, `ffmpeg`).
    pub node: String,
    /// The stamped source tree the Tier-1 decision applies to.
    pub tree: PathBuf,
    /// What `build` will do with `tree`.
    pub status: NodeStatus,
    /// Whether the compile is skipped by a Tier-2 artifact-cache hit. Independent of
    /// [`status`](Self::status): the store lives outside the work dir and is keyed by
    /// the node's *inputs*, so a freshly cloned tree with no stamp at all can still
    /// restore its `.deb`s and compile nothing.
    pub artifact: ArtifactStatus,
}

impl NodePlan {
    /// Evaluate a node: compare the `current` recomputed manifest against the stamp
    /// beside `tree`, and ask the artifact store whether it already holds `output`.
    ///
    /// `output` is the node's Tier-2 signature manifest, or `None` when it could not
    /// be computed — which reports [`ArtifactStatus::Unknown`] rather than guessing.
    fn evaluate(
        node: &str,
        tree: PathBuf,
        current: &SignatureManifest,
        output: Option<&SignatureManifest>,
        store: Option<&ArtifactStore>,
    ) -> NodePlan {
        let status = if !tree.exists() {
            NodeStatus::Absent
        } else {
            match signature::read_manifest(&tree) {
                None => NodeStatus::Unstamped,
                Some(prev) if prev.matches(current) => NodeStatus::Reuse,
                Some(prev) => NodeStatus::Rebuild(SignatureManifest::diff(&prev, current)),
            }
        };
        let artifact = match (store, output) {
            (None, _) => ArtifactStatus::Disabled,
            (Some(_), None) => ArtifactStatus::Unknown,
            (Some(store), Some(out)) => {
                if store.has(node, out.signature().as_str()) {
                    ArtifactStatus::Hit
                } else {
                    ArtifactStatus::Miss
                }
            }
        };
        NodePlan {
            node: node.to_string(),
            tree,
            status,
            artifact,
        }
    }
}

/// Inputs for [`plan_nodes`] — the lock plus the same dir / co-dev / libmali choices
/// `build` resolves, so the predicted trees and signatures match what a build uses.
pub struct PlanInputs<'a> {
    /// The recipe's resolved lock (the source pins).
    pub lock: &'a Lock,
    /// The build scratch dir (`build/RECIPE` by default) holding the source trees.
    pub work_dir: &'a Path,
    /// An explicit `--patches-path` co-dev checkout is in use: folded into the
    /// kernel/u-boot/ffmpeg signatures so a co-dev tree never matches a pinned stamp.
    pub patches_dev: bool,
    /// The co-dev `--patches-path` checkout, when `patches_dev`. Needed so the
    /// prediction folds the same live-series fingerprint the build stamps;
    /// `None` (or pinned mode) folds the series by commit only.
    pub patches_root: Option<&'a Path>,
    /// The userspace trees this build compiles, as the SoC declares them and resolution
    /// narrowed them: an optional tree is here only when the build asked for it.
    ///
    /// The prediction needs the declarations and not only the lock's pins, because the
    /// output key folds the whole set — one tree's `build_deps` are layered for the
    /// whole stage, so enabling an optional tree moves every tree's key.
    pub userspace: &'a [boot2deb_core::model::UserspaceTree],
    /// The build's resolved `device_dts` sources. Their content is folded into the
    /// kernel tree signature (the stage copies them into the tree), so the prediction
    /// must fold it too or an edited board `.dts` would be reported as "reuse". Empty
    /// for a board whose DTB is upstream.
    pub device_dts: &'a [PathBuf],
    /// The build's `device_kmods` descriptors — each predicts a `kmod:<name>` tree
    /// node. Empty for a board with no out-of-tree modules.
    pub device_kmods: &'a [boot2deb_core::model::ResolvedKmod],
    /// The resolved local compat-patch paths per kmod name (as [`kmod`](crate::build::kmod)
    /// consumes them). Their content folds into each kmod tree signature, so an edited
    /// shim is reported as a rebuild; a kmod absent here folds no local patch.
    pub kmod_local_patches: &'a [(String, Vec<PathBuf>)],
    /// The resolved build. The Tier-2 output signatures fold axes the lock does not
    /// carry — the kernel arch, the `KBUILD_IMAGE` path, the base defconfig, the
    /// u-boot defconfig — so predicting the artifact cache needs the resolution, not
    /// just the pins.
    pub build: &'a ResolvedBuild,
    /// The host-side identities the output signatures fold (the compiler that
    /// produces the kernel/u-boot bytes, the sandbox userland that produces the
    /// media-accel `.deb`s), assembled exactly as `build` assembles them.
    pub env: &'a crate::build::BuildEnv,
    /// The resolved kconfig fragment files, in merge order. Their *contents* fold
    /// into the kernel's output signature, so an edited fragment is an artifact-cache
    /// miss even though the source tree is untouched.
    pub fragments: &'a [PathBuf],
    /// Root of the Tier-2 artifact store, or `None` when the predicted build passes
    /// `--no-artifact-cache`. A root that cannot be opened predicts
    /// [`ArtifactStatus::Disabled`] for every node, which is what an unusable store
    /// means for the build too.
    pub artifact_store: Option<&'a Path>,
}

/// Predict both cache decisions for every compile node, in build order. Reads only
/// the lock, the on-disk stamps, and the artifact store's directory listing.
pub fn plan_nodes(inputs: &PlanInputs) -> Vec<NodePlan> {
    let lock = inputs.lock;
    let w = inputs.work_dir;
    // An unopenable store is reported as disabled rather than as a miss: it is what a
    // build would do with it, and claiming "will compile" from a failed `open` would be
    // a prediction about the store rather than about the build.
    let store = inputs
        .artifact_store
        .and_then(|root| ArtifactStore::open(root).ok());
    let store = store.as_ref();

    // Co-dev tree signatures fold the live-series fingerprint; recompute it
    // per scope exactly as each stage does so a co-dev prediction matches the stamp.
    // Pinned mode (or no patches root) folds by commit only. The `*_fp` Vecs are held
    // here so the borrowed [`SeriesIdentity::Dev`] outlives every use below.
    // A lock with no `[patches]` table (a kernel with no patch series) has no series
    // to fingerprint at all, in either mode.
    // Each scope folds the pin of the axis that patches its tree: the u-boot tree is
    // patched by `[uboot_patches]`, every other scope by the kernel-side `[patches]`.
    // Folding the wrong pin for u-boot would predict a shared stamp for distinct u-boot
    // series — exactly the artifact-cache collision `uboot::clone_manifest` guards.
    let fingerprint =
        |pin: Option<&boot2deb_core::lock::PatchesPin>, scope| match (inputs.patches_root, pin) {
            (Some(root), Some(pin)) if inputs.patches_dev => {
                crate::build::patch_series_fingerprint(root, &pin.series, scope)
            }
            _ => Vec::new(),
        };
    let dev = inputs.patches_dev;
    let kernel_fp = fingerprint(lock.patches.as_ref(), crate::build::PatchScope::Kernel);
    let uboot_fp = fingerprint(lock.uboot_patches.as_ref(), crate::build::PatchScope::Uboot);
    let ffmpeg_fp = fingerprint(lock.patches.as_ref(), crate::build::PatchScope::Ffmpeg);
    let userspace_fp = fingerprint(lock.patches.as_ref(), crate::build::PatchScope::Userspace);

    let dts_fp = crate::build::device_dts_fingerprint(inputs.device_dts);

    // A node exists only where the build has one, and the lock is the record of
    // that: it pins a kernel commit iff the kernel is compiled, and a u-boot commit
    // iff the boot method builds a bootloader. A board that installs Debian's kernel
    // and boots its own firmware has neither compile node, so it has nothing to
    // rebuild — and `why-rebuild` says so by listing no such node, rather than
    // reporting a phantom one as perpetually stale. Both manifests are therefore
    // computed from the pin, and a `None` pin simply contributes no node.
    //
    // Each node's Tier-2 manifest comes from the stage's own `output_manifest`, so the
    // key asked of the store is byte-for-byte the key the build will ask under. A
    // manifest that cannot be computed (a fragment that no longer reads, say) yields
    // `None` and reports the artifact status as unknown rather than as a miss.
    let mut nodes = Vec::new();
    // Held for the kmod nodes below: a module's output key folds the kernel *tree*
    // signature, since a kernel commit or patch bump changes module vermagic.
    let mut kernel_tree_sig = None;
    if let Ok(man) =
        crate::build::kernel::clone_manifest(lock, patch_series(dev, &kernel_fp), &dts_fp)
    {
        kernel_tree_sig = Some(man.signature());
        let kernel_out = inputs
            .build
            .image
            .as_ref()
            .and_then(|i| i.kernel.compiled())
            .and_then(|k| {
                crate::build::kernel::output_manifest(
                    inputs.build,
                    k,
                    lock,
                    inputs.fragments,
                    inputs.env,
                    patch_series(dev, &kernel_fp),
                    &dts_fp,
                )
                .ok()
            });
        nodes.push(NodePlan::evaluate(
            crate::build::kernel::NODE,
            crate::build::kernel::tree_dir(w),
            &man,
            kernel_out.as_ref(),
            store,
        ));
    }
    if let Ok(man) = crate::build::uboot::clone_manifest(lock, patch_series(dev, &uboot_fp)) {
        let out = inputs.build.rkbin_boot().and_then(|boot| {
            crate::build::uboot::output_manifest(
                inputs.build,
                boot,
                lock,
                inputs.env,
                patch_series(dev, &uboot_fp),
            )
            .ok()
        });
        nodes.push(NodePlan::evaluate(
            crate::build::uboot::NODE,
            crate::build::uboot::tree_dir(w),
            &man,
            out.as_ref(),
            store,
        ));
    }
    // The media-accel compile nodes (userspace packages + ffmpeg) exist only when
    // the recipe builds the transcode stack — i.e. the lock pins those sources. A
    // base build stops at kernel + u-boot.
    if !lock.userspace.is_empty() {
        let us = crate::build::userspace::stage_dir(w);
        // The userspace `git am` scope (the MPP CMA fix) folds into the patched tree's
        // signature, so recompute it the same way the stage stamps it —
        // `receives_userspace_patches` is the shared source of truth for which tree.
        let patch_inputs = crate::build::userspace::PatchInputs {
            pin: lock.patches.as_ref(),
            patches: patch_series(dev, &userspace_fp),
        };
        // The sandbox-built packages key their output on the suite they are built for,
        // which a media-accel build always has; a lock without one contributes no
        // output manifest rather than a wrong key.
        let suite = lock.rootfs.as_ref().map(|r| r.suite.as_str());
        let deb_arch = inputs.build.arch.debian_arch();
        // One node per tree the lock pins — the same set the userspace stage builds, so
        // the plan and the build agree on what exists for this SoC and this build.
        for pin in &lock.userspace {
            let Some(tree) = inputs.userspace.iter().find(|t| t.name == pin.name) else {
                // The lock pins a tree this resolution does not carry. The drift gate
                // reports that as drift; a prediction here would name a node no build
                // will run.
                continue;
            };
            let patches =
                crate::build::userspace::receives_userspace_patches(tree).then_some(&patch_inputs);
            let out = suite.map(|suite| {
                crate::build::userspace::output_manifest_for(
                    &pin.name,
                    &pin.commit,
                    suite,
                    deb_arch,
                    &inputs.env.sandbox_id,
                    inputs.userspace,
                    patches,
                )
            });
            nodes.push(NodePlan::evaluate(
                &crate::build::userspace::node_name_for(&pin.name),
                us.join(&pin.name),
                &crate::build::userspace::signature_manifest(&pin.name, &pin.commit, patches),
                out.as_ref(),
                store,
            ));
        }
    }
    if let Some(ff_pins) = &lock.ffmpeg {
        // ffmpeg links against the userspace `.deb`s, so its output key folds their
        // pins too — a media-accel lock always has both, and a lock with only one
        // yields no key rather than one that ignores the missing half.
        let out = (!lock.userspace.is_empty()).then(|| {
            crate::build::ffmpeg::output_manifest(
                lock,
                ff_pins,
                &lock.userspace,
                &crate::build::ffmpeg::OutputKeyInputs {
                    arch: inputs.build.arch.debian_arch(),
                    sandbox_id: &inputs.env.sandbox_id,
                    trees: inputs.userspace,
                    nonfree: inputs
                        .build
                        .image
                        .as_ref()
                        .is_some_and(|i| i.ffmpeg_nonfree),
                    patches: patch_series(dev, &ffmpeg_fp),
                    us_patches: patch_series(dev, &userspace_fp),
                },
            )
        });
        nodes.push(NodePlan::evaluate(
            crate::build::ffmpeg::NODE,
            crate::build::ffmpeg::tree_dir(w),
            &crate::build::ffmpeg::clone_manifest(
                ff_pins,
                lock.patches.as_ref(),
                patch_series(dev, &ffmpeg_fp),
            ),
            out.as_ref(),
            store,
        ));
    }
    // Out-of-tree kernel modules: each predicts a `kmod:<name>` tree at
    // `<work>/kmod/<name>` whose Tier-1 signature folds the driver commit, its in-repo
    // quilt, and the local compat-patch content — so an edited shim is reported as a
    // rebuild, not a reuse. A kmod whose lock pin is missing or whose local patch cannot
    // be read contributes no node (best-effort offline prediction, like the compile
    // nodes above whose manifest is a `Result`).
    for k in inputs.device_kmods {
        let Some(pin) = lock.kmods.iter().find(|p| p.name == k.name) else {
            continue;
        };
        let locals = inputs
            .kmod_local_patches
            .iter()
            .find(|(n, _)| n == &k.name)
            .map(|(_, v)| v.as_slice())
            .unwrap_or(&[]);
        let Ok(local_fps) = locals
            .iter()
            .map(|p| crate::build::file_fingerprint(p))
            .collect::<Result<Vec<_>, _>>()
        else {
            continue;
        };
        let man = crate::build::kmod::tree_signature_manifest(k, pin, &local_fps);
        // A kmod's compile is skipped only when *every* deb it produces is cached: the
        // modules deb, keyed on the kernel tree signature (which embeds the kver), and
        // — for a driver that ships one — the firmware deb. So the node reports a hit
        // only if both do, which is exactly the condition the stage returns early on.
        let mod_man = kernel_tree_sig.as_ref().map(|ksig| {
            crate::build::kmod::output_manifest(
                k,
                pin,
                &local_fps,
                ksig,
                inputs.build.arch.debian_arch(),
                &inputs.env.toolchain_id,
                &inputs.env.packaging_id,
            )
        });
        let fw_man = k.firmware.as_ref().map(|f| {
            crate::build::kmod::firmware_output_manifest(
                k,
                f,
                pin,
                &local_fps,
                &inputs.env.packaging_id,
            )
        });
        let node = crate::build::kmod::node_name(&k.name);
        let mut plan = NodePlan::evaluate(
            &node,
            crate::build::kmod::stage_dir(w).join(&k.name),
            &man,
            mod_man.as_ref(),
            store,
        );
        // Fold the companion firmware deb in: a hit on the modules alone still leaves
        // the driver tree to fetch and the firmware to package.
        if plan.artifact == ArtifactStatus::Hit {
            plan.artifact = match (&fw_man, store) {
                (None, _) => ArtifactStatus::Hit,
                (Some(m), Some(store)) => {
                    let fw_node = crate::build::kmod::firmware_node_name(&k.name);
                    if store.has(&fw_node, m.signature().as_str()) {
                        ArtifactStatus::Hit
                    } else {
                        ArtifactStatus::Miss
                    }
                }
                (Some(_), None) => ArtifactStatus::Disabled,
            };
        }
        nodes.push(plan);
    }
    nodes
}

/// The [`SeriesIdentity`](crate::build::SeriesIdentity) for a predicted node: co-dev folds
/// the live-series fingerprint `fp`, pinned folds by commit. A free `fn`
/// (not a closure) so the borrow of `fp` is elided cleanly into the return type.
fn patch_series(dev: bool, fp: &[String]) -> crate::build::SeriesIdentity<'_> {
    if dev {
        crate::build::SeriesIdentity::Dev(fp)
    } else {
        crate::build::SeriesIdentity::Pinned
    }
}

#[cfg(test)]
mod tests {

    /// The trees the RK1 fixture's SoC declares, less the optional ones — the set a
    /// plain `build` compiles, and what the prediction has to be given to match it.
    fn rk1_trees(build: &ResolvedBuild) -> Vec<boot2deb_core::model::UserspaceTree> {
        build
            .image
            .iter()
            .flat_map(|i| &i.userspace)
            .filter(|t| !t.optional)
            .cloned()
            .collect()
    }

    /// Every tree, including the optional ones — what `--userspace libmali` asks for.
    fn all_rk1_trees(build: &ResolvedBuild) -> Vec<boot2deb_core::model::UserspaceTree> {
        build
            .image
            .iter()
            .flat_map(|i| &i.userspace)
            .cloned()
            .collect()
    }

    /// A [`UserspacePin`] from a name and a [`GitPin`]-shaped fixture.
    fn named_pin(name: &str, p: GitPin) -> boot2deb_core::lock::UserspacePin {
        boot2deb_core::lock::UserspacePin {
            name: name.into(),
            source: p.source,
            reference: p.reference,
            commit: p.commit,
        }
    }
    use super::*;
    use crate::signature::write_manifest;
    use boot2deb_core::lock::{
        BlobsPin, FfmpegPins, GitPin, KernelPin, Lock, PatchesPin, RootfsPin, UbootPin,
    };

    /// A `BuildEnv` with fixed host identities. The Tier-1 assertions below are
    /// independent of it, and fixing it keeps them so — a real probe would make the
    /// tests depend on the machine's compiler.
    fn env_fixture() -> crate::build::BuildEnv {
        crate::build::BuildEnv {
            cross_compile: Some("aarch64-linux-gnu-".into()),
            jobs: None,
            toolchain_id: "gcc-13".into(),
            sandbox_id: "sandbox-fixture".into(),
            packaging_id: "packaging-fixture".into(),
        }
    }

    fn lock_fixture(kernel_commit: &str, mpp_commit: &str) -> Lock {
        let git = |c: &str| GitPin {
            source: "s".into(),
            reference: "r".into(),
            commit: c.into(),
        };
        Lock {
            kernel: Some(KernelPin {
                id: "k".into(),
                source: "ks".into(),
                reference: "v7.1.1".into(),
                commit: kernel_commit.into(),
            }),
            patches: Some(PatchesPin {
                series: vec!["rk3588-accel".into()],
                source: "ps".into(),
                reference: "main".into(),
                commit: "p1".into(),
            }),
            uboot: Some(UbootPin {
                source: "us".into(),
                reference: "v".into(),
                commit: "u1".into(),
            }),
            uboot_patches: None,
            userspace: vec![
                named_pin("mpp", git(mpp_commit)),
                named_pin("librga", git("rga1")),
                named_pin("libmali", git("mali1")),
            ],
            ffmpeg: Some(FfmpegPins {
                base: git("b1"),
                rockchip: Some(git("rk1")),
            }),
            rootfs: Some(RootfsPin {
                suite: "forky".into(),
                manifest: "m".into(),
                manifest_sha256: None,
            }),
            blobs: Some(BlobsPin {
                atf: "a".into(),
                tpl: "t".into(),
                bl32: None,
            }),
            kmods: vec![],
            extra_debs: vec![],
            snapshot: None,
        }
    }

    fn status_of<'a>(plan: &'a [NodePlan], node: &str) -> &'a NodeStatus {
        &plan
            .iter()
            .find(|n| n.node == node)
            .expect("node present")
            .status
    }

    #[test]
    fn absent_trees_are_reported_as_first_build() {
        let lock = lock_fixture("kc1", "mc1");
        let tmp = tempfile::tempdir().unwrap();
        let build = crate::test_support::rk1_build();
        let trees = rk1_trees(&build);
        let env = env_fixture();
        let plan = plan_nodes(&PlanInputs {
            lock: &lock,
            work_dir: tmp.path(),
            patches_dev: false,
            patches_root: None,
            userspace: &trees,
            device_dts: &[],
            device_kmods: &[],
            kmod_local_patches: &[],
            build: &build,
            env: &env,
            fragments: &[],
            artifact_store: None,
        });
        // No trees on disk yet → every node is a fresh build.
        assert!(plan.iter().all(|n| n.status == NodeStatus::Absent));
        // Build order: kernel, uboot, the two userspace packages, ffmpeg (no libmali).
        let names: Vec<&str> = plan.iter().map(|n| n.node.as_str()).collect();
        assert_eq!(
            names,
            [
                "kernel",
                "uboot",
                "userspace:mpp",
                "userspace:librga",
                "ffmpeg"
            ]
        );
    }

    /// An optional tree gets a node exactly when the build named it. The prediction is
    /// given the same narrowed set the build compiles, so a `--userspace libmali` run
    /// and a plain one predict different node sets — which is the truth, since only one
    /// of them builds that tree.
    #[test]
    fn an_optional_trees_node_appears_only_when_the_build_asks_for_it() {
        let lock = lock_fixture("kc1", "mc1");
        let build = crate::test_support::rk1_build();
        let env = env_fixture();
        let tmp = tempfile::tempdir().unwrap();
        let all = all_rk1_trees(&build);
        let with = plan_nodes(&PlanInputs {
            lock: &lock,
            work_dir: tmp.path(),
            patches_dev: false,
            patches_root: None,
            userspace: &all,
            device_dts: &[],
            device_kmods: &[],
            kmod_local_patches: &[],
            build: &build,
            env: &env,
            fragments: &[],
            artifact_store: None,
        });
        assert!(with.iter().any(|n| n.node == "userspace:libmali"));

        // And not when it does not: the same lock, the narrowed set.
        let narrow = rk1_trees(&build);
        let without = plan_nodes(&PlanInputs {
            lock: &lock,
            work_dir: tmp.path(),
            patches_dev: false,
            patches_root: None,
            userspace: &narrow,
            device_dts: &[],
            device_kmods: &[],
            kmod_local_patches: &[],
            build: &build,
            env: &env,
            fragments: &[],
            artifact_store: None,
        });
        assert!(!without.iter().any(|n| n.node == "userspace:libmali"));
    }

    #[test]
    fn base_build_plans_only_kernel_and_uboot() {
        // A lock with no media-accel pins (a base build) schedules neither the
        // userspace packages nor ffmpeg — only kernel + u-boot.
        let mut lock = lock_fixture("kc1", "mc1");
        lock.userspace = Vec::new();
        lock.ffmpeg = None;
        let tmp = tempfile::tempdir().unwrap();
        let build = crate::test_support::rk1_build();
        let env = env_fixture();
        let plan = plan_nodes(&PlanInputs {
            lock: &lock,
            work_dir: tmp.path(),
            patches_dev: false,
            patches_root: None,
            userspace: &[],
            device_dts: &[],
            device_kmods: &[],
            kmod_local_patches: &[],
            build: &build,
            env: &env,
            fragments: &[],
            artifact_store: None,
        });
        let names: Vec<&str> = plan.iter().map(|n| n.node.as_str()).collect();
        assert_eq!(names, ["kernel", "uboot"]);
    }

    #[test]
    fn matching_stamp_reuses_drift_rebuilds_with_the_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path();

        // Stamp the kernel + mpp trees as if a build at ("kc1","mc1") had run. The
        // paths are literals here on purpose: `plan_nodes` asks each stage where its
        // tree is, so a fixture that asked the same way would agree with a moved layout
        // instead of catching it.
        let old = lock_fixture("kc1", "mc1");
        let linux = work.join("linux");
        std::fs::create_dir_all(&linux).unwrap();
        write_manifest(
            &linux,
            &crate::build::kernel::clone_manifest(&old, crate::build::SeriesIdentity::Pinned, &[])
                .unwrap(),
        )
        .unwrap();
        let mpp = work.join("userspace").join("mpp");
        std::fs::create_dir_all(&mpp).unwrap();
        // Stamp mpp exactly as plan_nodes recomputes it: the MPP tree folds the patch
        // series, so include the same PatchInputs.
        let old_patches = crate::build::userspace::PatchInputs {
            pin: old.patches.as_ref(),
            patches: crate::build::SeriesIdentity::Pinned,
        };
        write_manifest(
            &mpp,
            &crate::build::userspace::signature_manifest(
                "mpp",
                &old.userspace
                    .iter()
                    .find(|p| p.name == "mpp")
                    .expect("the fixture pins mpp")
                    .commit,
                Some(&old_patches),
            ),
        )
        .unwrap();

        // Re-plan against a lock whose kernel commit moved but whose mpp commit did not.
        let new = lock_fixture("kc2", "mc1");
        let build = crate::test_support::rk1_build();
        let trees = rk1_trees(&build);
        let env = env_fixture();
        let plan = plan_nodes(&PlanInputs {
            lock: &new,
            work_dir: work,
            patches_dev: false,
            patches_root: None,
            userspace: &trees,
            device_dts: &[],
            device_kmods: &[],
            kmod_local_patches: &[],
            build: &build,
            env: &env,
            fragments: &[],
            artifact_store: None,
        });

        // mpp is unchanged → reuse.
        assert_eq!(status_of(&plan, "userspace:mpp"), &NodeStatus::Reuse);
        // kernel's commit moved → rebuild, naming the changed input.
        match status_of(&plan, "kernel") {
            NodeStatus::Rebuild(changes) => {
                let summary: Vec<String> = changes.iter().map(|c| c.summary()).collect();
                assert_eq!(summary, vec!["kernel.commit: kc1 → kc2"]);
            }
            other => panic!("expected kernel rebuild, got {other:?}"),
        }
        // uboot was never built → absent.
        assert_eq!(status_of(&plan, "uboot"), &NodeStatus::Absent);
    }

    #[test]
    fn an_unstamped_tree_is_rebuilt() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path();
        // A tree present with no stamp (an interrupted build) is not trusted.
        std::fs::create_dir_all(work.join("linux")).unwrap();
        let lock = lock_fixture("kc1", "mc1");
        let build = crate::test_support::rk1_build();
        let env = env_fixture();
        let plan = plan_nodes(&PlanInputs {
            lock: &lock,
            work_dir: work,
            patches_dev: false,
            patches_root: None,
            userspace: &[],
            device_dts: &[],
            device_kmods: &[],
            kmod_local_patches: &[],
            build: &build,
            env: &env,
            fragments: &[],
            artifact_store: None,
        });
        assert_eq!(status_of(&plan, "kernel"), &NodeStatus::Unstamped);
    }

    #[test]
    fn the_artifact_verdict_is_independent_of_the_tree_verdict() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        let store_root = tmp.path().join("store");
        let lock = lock_fixture("kc1", "mc1");
        let build = crate::test_support::rk1_build();
        let env = env_fixture();
        // A free `fn` rather than a closure so the one borrow that varies (the store)
        // shares the fixtures' lifetime instead of being inferred per call.
        fn inputs<'a>(
            lock: &'a Lock,
            work: &'a Path,
            build: &'a ResolvedBuild,
            env: &'a crate::build::BuildEnv,
            store: Option<&'a Path>,
        ) -> PlanInputs<'a> {
            PlanInputs {
                lock,
                work_dir: work,
                patches_dev: false,
                patches_root: None,
                userspace: &[],
                device_dts: &[],
                device_kmods: &[],
                kmod_local_patches: &[],
                build,
                env,
                fragments: &[],
                artifact_store: store,
            }
        }
        let inputs = |store| inputs(&lock, &work, &build, &env, store);

        // No store configured (`--no-artifact-cache`): every node reports disabled,
        // never "will compile" — the difference matters, because a disabled cache is
        // the user's own choice and a miss is a fact about the store.
        let off: Vec<NodePlan> = plan_nodes(&inputs(None));
        assert!(off.iter().all(|n| n.artifact == ArtifactStatus::Disabled));

        // An empty store: same nodes, now genuinely missing.
        let on: Vec<NodePlan> = plan_nodes(&inputs(Some(&store_root)));
        assert!(on.iter().all(|n| n.artifact == ArtifactStatus::Miss));
        // The trees are all absent in this fresh work dir, which is exactly the case
        // the two tiers come apart in: nothing is stamped, yet a stored artifact would
        // still let the compile be skipped.
        assert!(on.iter().all(|n| n.status == NodeStatus::Absent));

        // Store the kernel's predicted output under the key the plan asks by, and the
        // kernel — and only the kernel — flips to a hit while its tree stays absent.
        let store = ArtifactStore::open(&store_root).unwrap();
        let sig = crate::build::kernel::output_manifest(
            &build,
            build.image.as_ref().unwrap().kernel.compiled().unwrap(),
            &lock,
            &[],
            &env,
            crate::build::SeriesIdentity::Pinned,
            &[],
        )
        .unwrap()
        .signature();
        let deb = tmp.path().join("linux-image.deb");
        std::fs::write(&deb, b"deb").unwrap();
        store
            .put("kernel", sig.as_str(), &[("image_deb", &deb)])
            .unwrap();

        let hit: Vec<NodePlan> = plan_nodes(&inputs(Some(&store_root)));
        let kernel = hit.iter().find(|n| n.node == "kernel").unwrap();
        assert_eq!(kernel.artifact, ArtifactStatus::Hit);
        assert_eq!(kernel.status, NodeStatus::Absent);
        assert!(hit
            .iter()
            .filter(|n| n.node != "kernel")
            .all(|n| n.artifact == ArtifactStatus::Miss));
    }
}
