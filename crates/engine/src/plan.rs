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
//! Scope: the compile nodes whose reuse is a Tier-1 tree signature computable
//! offline from the lock — kernel, u-boot, the userspace packages, and ffmpeg. Two
//! things are deliberately out of scope because neither is a static, lock-only
//! prediction: the *compile* step of each node always re-runs (only the clone+patch
//! tree is cached), and the rootfs node's cache is keyed on the live package
//! solve (`mmdebstrap --simulate`), which needs the mirror.

use crate::signature::{self, RecordChange, SignatureManifest};
use boot2deb_core::lock::Lock;
use std::path::{Path, PathBuf};

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

/// One compile node's tree and the reuse decision `build` will make for it.
#[derive(Debug, Clone)]
pub struct NodePlan {
    /// The build node (e.g. `kernel`, `userspace:mpp`, `ffmpeg`).
    pub node: String,
    /// The stamped source tree the decision applies to.
    pub tree: PathBuf,
    /// What `build` will do with `tree`.
    pub status: NodeStatus,
}

impl NodePlan {
    /// Evaluate a node: compare the `current` recomputed manifest against the stamp
    /// beside `tree`.
    fn evaluate(node: &str, tree: PathBuf, current: &SignatureManifest) -> NodePlan {
        let status = if !tree.exists() {
            NodeStatus::Absent
        } else {
            match signature::read_manifest(&tree) {
                None => NodeStatus::Unstamped,
                Some(prev) if prev.matches(current) => NodeStatus::Reuse,
                Some(prev) => NodeStatus::Rebuild(SignatureManifest::diff(&prev, current)),
            }
        };
        NodePlan { node: node.to_string(), tree, status }
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
    /// prediction folds the same live-series fingerprint the build stamps (CACHE-1);
    /// `None` (or pinned mode) folds the series by commit only.
    pub patches_root: Option<&'a Path>,
    /// Include the optional `libmali` userspace node (built only with
    /// `--build-libmali`).
    pub include_libmali: bool,
    /// The build's resolved `device_dts` sources. Their content is folded into the
    /// kernel tree signature (the stage copies them into the tree), so the prediction
    /// must fold it too or an edited board `.dts` would be reported as "reuse". Empty
    /// for a board whose DTB is upstream. §4.
    pub device_dts: &'a [PathBuf],
}

/// Predict the reuse decision for every compile node, in build order. Reads
/// only the lock and the on-disk stamps.
pub fn plan_nodes(inputs: &PlanInputs) -> Vec<NodePlan> {
    let lock = inputs.lock;
    let w = inputs.work_dir;

    // Co-dev tree signatures fold the live-series fingerprint (CACHE-1); recompute it
    // per scope exactly as each stage does so a co-dev prediction matches the stamp.
    // Pinned mode (or no patches root) folds by commit only. The `*_fp` Vecs are held
    // here so the borrowed [`PatchSeries::Dev`] outlives every use below.
    // A lock with no `[patches]` table (a kernel with no patch profile) has no series
    // to fingerprint at all, in either mode.
    let fingerprint = |scope| match (inputs.patches_root, &lock.patches) {
        (Some(root), Some(pin)) if inputs.patches_dev => {
            crate::build::patch_series_fingerprint(root, &pin.profile, scope)
        }
        _ => Vec::new(),
    };
    let dev = inputs.patches_dev;
    let kernel_fp = fingerprint(crate::build::PatchScope::Kernel);
    let uboot_fp = fingerprint(crate::build::PatchScope::Uboot);
    let ffmpeg_fp = fingerprint(crate::build::PatchScope::Ffmpeg);
    let userspace_fp = fingerprint(crate::build::PatchScope::Userspace);

    let dts_fp = crate::build::device_dts_fingerprint(inputs.device_dts);

    let mut nodes = vec![
        NodePlan::evaluate(
            "kernel",
            w.join("linux"),
            &crate::build::kernel::clone_manifest(lock, patch_series(dev, &kernel_fp), &dts_fp),
        ),
        NodePlan::evaluate(
            "uboot",
            w.join("u-boot"),
            &crate::build::uboot::clone_manifest(lock, patch_series(dev, &uboot_fp)),
        ),
    ];
    // The media-accel compile nodes (userspace packages + ffmpeg) exist only when
    // the recipe builds the transcode stack — i.e. the lock pins those sources. A
    // base build stops at kernel + u-boot.
    if let Some(us_pins) = &lock.userspace {
        let us = w.join("userspace");
        // The userspace `git am` scope (the MPP CMA fix) folds into the patched
        // package's tree signature, so recompute it the same way the stage stamps it —
        // `receives_userspace_patches` is the shared source of truth for which package.
        let patch_inputs = crate::build::userspace::PatchInputs {
            pin: lock.patches.as_ref(),
            patches: patch_series(dev, &userspace_fp),
        };
        let us_patches = |name: &str| {
            crate::build::userspace::receives_userspace_patches(name).then_some(&patch_inputs)
        };
        nodes.push(NodePlan::evaluate(
            "userspace:mpp",
            us.join("mpp"),
            &crate::build::userspace::signature_manifest("mpp", &us_pins.mpp.commit, us_patches("mpp")),
        ));
        nodes.push(NodePlan::evaluate(
            "userspace:librga",
            us.join("librga"),
            &crate::build::userspace::signature_manifest(
                "librga",
                &us_pins.librga.commit,
                us_patches("librga"),
            ),
        ));
        if inputs.include_libmali {
            nodes.push(NodePlan::evaluate(
                "userspace:libmali",
                us.join("libmali"),
                &crate::build::userspace::signature_manifest(
                    "libmali",
                    &us_pins.libmali.commit,
                    us_patches("libmali"),
                ),
            ));
        }
    }
    if let Some(ff_pins) = &lock.ffmpeg {
        nodes.push(NodePlan::evaluate(
            "ffmpeg",
            w.join("ffmpeg").join("build"),
            &crate::build::ffmpeg::clone_manifest(
                ff_pins,
                lock.patches.as_ref(),
                patch_series(dev, &ffmpeg_fp),
            ),
        ));
    }
    nodes
}

/// The [`PatchSeries`](crate::build::PatchSeries) for a predicted node: co-dev folds
/// the live-series fingerprint `fp` (CACHE-1), pinned folds by commit. A free `fn`
/// (not a closure) so the borrow of `fp` is elided cleanly into the return type.
fn patch_series(dev: bool, fp: &[String]) -> crate::build::PatchSeries<'_> {
    if dev {
        crate::build::PatchSeries::Dev(fp)
    } else {
        crate::build::PatchSeries::Pinned
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::signature::write_manifest;
    use boot2deb_core::lock::{
        BlobsPin, FfmpegPins, GitPin, KernelPin, Lock, PatchesPin, RootfsPin, UbootPin,
        UserspacePins,
    };

    fn lock_fixture(kernel_commit: &str, mpp_commit: &str) -> Lock {
        let git = |c: &str| GitPin { source: "s".into(), reference: "r".into(), commit: c.into() };
        Lock {
            kernel: KernelPin { id: "k".into(), source: "ks".into(), reference: "v7.1.1".into(), commit: kernel_commit.into() },
            patches: Some(PatchesPin { profile: "rk3588-accel".into(), commit: "p1".into() }),
            uboot: UbootPin { source: "us".into(), reference: "v".into(), commit: "u1".into() },
            userspace: Some(UserspacePins {
                mpp: git(mpp_commit),
                librga: git("rga1"),
                libmali: git("mali1"),
            }),
            ffmpeg: Some(FfmpegPins { base: git("b1"), rockchip: git("rk1") }),
            rootfs: RootfsPin { suite: "forky".into(), manifest: "m".into(), manifest_sha256: None },
            blobs: BlobsPin { atf: "a".into(), tpl: "t".into(), bl32: None },
            extra_debs: vec![],
            snapshot: None,
        }
    }

    fn status_of<'a>(plan: &'a [NodePlan], node: &str) -> &'a NodeStatus {
        &plan.iter().find(|n| n.node == node).expect("node present").status
    }

    #[test]
    fn absent_trees_are_reported_as_first_build() {
        let lock = lock_fixture("kc1", "mc1");
        let tmp = tempfile::tempdir().unwrap();
        let plan = plan_nodes(&PlanInputs {
            lock: &lock,
            work_dir: tmp.path(),
            patches_dev: false,
            patches_root: None,
            include_libmali: false,
            device_dts: &[],
        });
        // No trees on disk yet → every node is a fresh build.
        assert!(plan.iter().all(|n| n.status == NodeStatus::Absent));
        // Build order: kernel, uboot, the two userspace packages, ffmpeg (no libmali).
        let names: Vec<&str> = plan.iter().map(|n| n.node.as_str()).collect();
        assert_eq!(names, ["kernel", "uboot", "userspace:mpp", "userspace:librga", "ffmpeg"]);
    }

    #[test]
    fn libmali_node_is_gated_on_the_flag() {
        let lock = lock_fixture("kc1", "mc1");
        let tmp = tempfile::tempdir().unwrap();
        let with = plan_nodes(&PlanInputs {
            lock: &lock,
            work_dir: tmp.path(),
            patches_dev: false,
            patches_root: None,
            include_libmali: true,
            device_dts: &[],
        });
        assert!(with.iter().any(|n| n.node == "userspace:libmali"));
    }

    #[test]
    fn base_build_plans_only_kernel_and_uboot() {
        // A lock with no media-accel pins (a base build) schedules neither the
        // userspace packages nor ffmpeg — only kernel + u-boot (UX-21).
        let mut lock = lock_fixture("kc1", "mc1");
        lock.userspace = None;
        lock.ffmpeg = None;
        let tmp = tempfile::tempdir().unwrap();
        let plan = plan_nodes(&PlanInputs {
            lock: &lock,
            work_dir: tmp.path(),
            patches_dev: false,
            patches_root: None,
            include_libmali: true,
            device_dts: &[],
        });
        let names: Vec<&str> = plan.iter().map(|n| n.node.as_str()).collect();
        assert_eq!(names, ["kernel", "uboot"]);
    }

    #[test]
    fn matching_stamp_reuses_drift_rebuilds_with_the_reason() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path();

        // Stamp the kernel + mpp trees as if a build at ("kc1","mc1") had run.
        let old = lock_fixture("kc1", "mc1");
        let linux = work.join("linux");
        std::fs::create_dir_all(&linux).unwrap();
        write_manifest(
            &linux,
            &crate::build::kernel::clone_manifest(&old, crate::build::PatchSeries::Pinned, &[]),
        )
        .unwrap();
        let mpp = work.join("userspace").join("mpp");
        std::fs::create_dir_all(&mpp).unwrap();
        // Stamp mpp exactly as plan_nodes recomputes it: the MPP tree folds the patch
        // series, so include the same PatchInputs.
        let old_patches = crate::build::userspace::PatchInputs {
            pin: old.patches.as_ref(),
            patches: crate::build::PatchSeries::Pinned,
        };
        write_manifest(
            &mpp,
            &crate::build::userspace::signature_manifest(
                "mpp",
                &old.userspace.as_ref().unwrap().mpp.commit,
                Some(&old_patches),
            ),
        )
        .unwrap();

        // Re-plan against a lock whose kernel commit moved but whose mpp commit did not.
        let new = lock_fixture("kc2", "mc1");
        let plan = plan_nodes(&PlanInputs {
            lock: &new,
            work_dir: work,
            patches_dev: false,
            patches_root: None,
            include_libmali: false,
            device_dts: &[],
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
        let plan = plan_nodes(&PlanInputs {
            lock: &lock,
            work_dir: work,
            patches_dev: false,
            patches_root: None,
            include_libmali: false,
            device_dts: &[],
        });
        assert_eq!(status_of(&plan, "kernel"), &NodeStatus::Unstamped);
    }
}
