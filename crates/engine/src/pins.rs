//! Lock resolution: turn a [`ResolvedBuild`] plus the recipe's ref
//! constraints into an exact [`Lock`] — the sole path that consults upstream.
//!
//! The assembly (`assemble_lock`) is pure and unit-tested; the resolution
//! ([`resolve_lock`]) is the side-effecting shell: it peels refs to commits with
//! `git ls-remote`, hashes the vendored rkbin blobs, and reads the patches-repo
//! HEAD. `boot2deb build` never runs this — it reads the committed lock.

use crate::blobs;
use crate::error::EngineError;
use crate::git;
use boot2deb_core::lock::{
    BlobsPin, FfmpegPins, GitPin, KernelPin, Lock, PatchesPin, RootfsPin, UbootPin, UserspacePins,
};
use boot2deb_core::model::KernelSource;
use boot2deb_core::ResolvedBuild;
use std::path::Path;

/// Inputs for `boot2deb update` beyond the resolved build itself.
///
/// The refs are the exact tags to pin. Auto-resolving a kernel `track` to its
/// latest tag is a later refinement; today the lock is seeded by pinning
/// `v7.1.1` explicitly, which is also how any specific historical build is pinned.
pub struct UpdateOptions<'a> {
    /// Kernel ref to pin and resolve to a commit (e.g. `v7.1.1`).
    pub kernel_ref: &'a str,
    /// u-boot ref to pin (defaults to the boot-method's `uboot_ref`).
    pub uboot_ref: &'a str,
    /// MPP source ref to pin (defaults to the SoC layer's `userspace.mpp` ref).
    pub mpp_ref: &'a str,
    /// librga source ref to pin (defaults to the SoC layer's `userspace.librga`).
    pub librga_ref: &'a str,
    /// libmali source ref to pin (defaults to the SoC layer's `userspace.libmali`).
    pub libmali_ref: &'a str,
    /// ffmpeg base (V4L2) ref to pin (defaults to the SoC layer's `ffmpeg.base`).
    pub ffmpeg_base_ref: &'a str,
    /// ffmpeg Rockchip provenance-tree ref to pin (defaults to the SoC layer's
    /// `ffmpeg.rockchip`). Recorded as provenance for the graft series; not
    /// fetched at build time.
    pub ffmpeg_rockchip_ref: &'a str,
    /// Directory holding the vendored rkbin blobs to hash.
    pub blobs_dir: &'a Path,
    /// Checkout of the `patches` repo whose HEAD pins the series.
    pub patches_path: &'a Path,
    /// Path recorded for the solved package manifest the rootfs stage writes
    /// (the content pin itself is produced then).
    pub rootfs_manifest: &'a str,
}

/// Resolve a build to an exact [`Lock`] by consulting upstream and the vendored
/// blobs. This is the only function that reaches the network.
pub fn resolve_lock(build: &ResolvedBuild, opts: &UpdateOptions) -> Result<Lock, EngineError> {
    let kernel_url = kernel_source_url(&build.kernel.source)?;
    let kernel_commit = git::resolve_ref(&kernel_url, opts.kernel_ref)?;
    let uboot_commit = git::resolve_ref(&build.uboot_source, opts.uboot_ref)?;
    let patches_commit = git::rev_parse_head(opts.patches_path)?;
    let userspace = UserspacePins {
        mpp: git_pin(&build.userspace.mpp.git, opts.mpp_ref)?,
        librga: git_pin(&build.userspace.librga.git, opts.librga_ref)?,
        libmali: git_pin(&build.userspace.libmali.git, opts.libmali_ref)?,
    };
    let ffmpeg = FfmpegPins {
        base: git_pin(&build.ffmpeg.base.git, opts.ffmpeg_base_ref)?,
        rockchip: git_pin(&build.ffmpeg.rockchip.git, opts.ffmpeg_rockchip_ref)?,
    };
    let atf = blob_pin(opts.blobs_dir, &build.rkbin.atf)?;
    let tpl = blob_pin(opts.blobs_dir, &build.rkbin.tpl)?;
    Ok(assemble_lock(
        build,
        opts,
        kernel_commit,
        uboot_commit,
        patches_commit,
        userspace,
        ffmpeg,
        atf,
        tpl,
    ))
}

/// Resolve one git source's ref to an exact commit and pair them into a
/// [`GitPin`]. A full-SHA ref pins that exact commit — canonicalized to
/// lowercase so `reference` and `commit` agree and classify as a bare commit; a
/// branch/tag name is kept verbatim and peeled to its commit via `ls-remote`.
fn git_pin(url: &str, reference: &str) -> Result<GitPin, EngineError> {
    Ok(GitPin {
        reference: boot2deb_core::sources::normalize_ref(reference),
        commit: git::resolve_ref(url, reference)?,
    })
}

/// Write a lock to `recipes/<name>.lock` in its canonical committed form.
///
/// The write is atomic — a uniquely-named temp beside the destination, renamed into
/// place — because the lock is the build's source of truth: an interruption
/// or storage fault mid-write must never leave a truncated `.lock` a later `build`
/// would parse or partially trust. The temp shares the destination's directory so
/// the rename stays on one filesystem (where rename is atomic).
pub fn write_lock(path: &Path, lock: &Lock) -> Result<(), EngineError> {
    let text = lock.to_toml_string()?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("recipe.lock");
    let tmp = dir.join(format!(".{file_name}.{}.partial", std::process::id()));
    std::fs::write(&tmp, text).map_err(|source| EngineError::io(&tmp, source))?;
    std::fs::rename(&tmp, path).map_err(|source| {
        let _ = std::fs::remove_file(&tmp);
        EngineError::io(path, source)
    })
}

/// Assemble a [`Lock`] from already-resolved values. Pure: no I/O, so the mapping
/// of build fields onto lock pins is unit-testable.
#[allow(clippy::too_many_arguments)]
fn assemble_lock(
    build: &ResolvedBuild,
    opts: &UpdateOptions,
    kernel_commit: String,
    uboot_commit: String,
    patches_commit: String,
    userspace: UserspacePins,
    ffmpeg: FfmpegPins,
    atf: String,
    tpl: String,
) -> Lock {
    Lock {
        kernel: KernelPin {
            id: build.kernel.id.clone(),
            reference: boot2deb_core::sources::normalize_ref(opts.kernel_ref),
            commit: kernel_commit,
        },
        patches: PatchesPin {
            profile: build.kernel.patch_profile.clone(),
            commit: patches_commit,
        },
        uboot: UbootPin {
            reference: boot2deb_core::sources::normalize_ref(opts.uboot_ref),
            commit: uboot_commit,
        },
        userspace,
        ffmpeg,
        rootfs: RootfsPin {
            suite: build.suite.clone(),
            manifest: opts.rootfs_manifest.to_string(),
            // Set once the solved manifest is committed beside the lock; a bare
            // `update` names the manifest but has not produced it yet.
            manifest_sha256: None,
        },
        blobs: BlobsPin { atf, tpl },
        // The resolved extra-deb pins recorded verbatim — the sha256 is already the
        // exact content pin, so there is nothing to resolve. `update`
        // fetches/verifies/stores them; `build` materializes from the store. Empty
        // when no layer or feature adds one.
        extra_debs: build.extra_debs.clone(),
        // Captured opt-in on a successful build (`--save-snapshot`), not here.
        snapshot: None,
    }
}

/// Assert the committed lock still agrees with a fresh resolution on every axis the
/// lock records *from the resolved build* — the kernel definition id, the patch
/// profile, the suite, and the resolved extra-deb set (the exact fields
/// `assemble_lock` copies out of the [`ResolvedBuild`]).
///
/// A mismatch means the config drifted since `update` (a device/recipe/suite/feature
/// change), so the lock's pins no longer describe the point the recipe now resolves
/// to. `build` calls this up front and hard-errors with the drifted axes named, rather
/// than building a hybrid of newly resolved axes and stale pins — which would also
/// leave the cache keyed inconsistently (some stages fold lock suite, runtime setup
/// uses resolved suite) (CFG-2). The commit/ref/blob pins are deliberately *not*
/// checked here: they come from `update`'s refs plus upstream resolution, not the pure
/// resolve, so they have no fresh-resolve counterpart to compare against.
///
/// Kept beside `assemble_lock` so the two stay in lockstep — every resolved-derived
/// field written there is checked here.
pub fn check_lock_consistency(lock: &Lock, build: &ResolvedBuild) -> Result<(), EngineError> {
    let mut axes = Vec::new();
    if lock.kernel.id != build.kernel.id {
        axes.push(format!(
            "kernel id: lock '{}' vs resolved '{}'",
            lock.kernel.id, build.kernel.id
        ));
    }
    if lock.patches.profile != build.kernel.patch_profile {
        axes.push(format!(
            "patch profile: lock '{}' vs resolved '{}'",
            lock.patches.profile, build.kernel.patch_profile
        ));
    }
    if lock.rootfs.suite != build.suite {
        axes.push(format!(
            "suite: lock '{}' vs resolved '{}'",
            lock.rootfs.suite, build.suite
        ));
    }
    if lock.extra_debs != build.extra_debs {
        axes.push(format!(
            "extra_debs: lock records {} vs resolved {}",
            lock.extra_debs.len(),
            build.extra_debs.len()
        ));
    }
    if axes.is_empty() {
        Ok(())
    } else {
        Err(EngineError::LockConfigDrift { axes })
    }
}

/// Upstream URL for a kernel source: a known named tree resolves to a git.kernel.org
/// URL; an explicit `{ git, ref }` uses its URL directly. Also the default
/// clone source for the kernel build stage when `--kernel-src` is not given.
pub fn kernel_source_url(source: &KernelSource) -> Result<String, EngineError> {
    match source {
        KernelSource::Named(name) => {
            named_tree_url(name).ok_or_else(|| EngineError::UnknownSourceTree {
                name: name.clone(),
            })
        }
        KernelSource::Git { git, .. } => Ok(git.clone()),
    }
}

/// Map a well-known kernel tree name to its clone URL.
fn named_tree_url(name: &str) -> Option<String> {
    let url = match name {
        "linux-stable" => {
            "https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux-stable.git"
        }
        "torvalds" | "linux" => {
            "https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git"
        }
        _ => return None,
    };
    Some(url.to_string())
}

/// Hash a vendored blob and format its lock pin `"<filename>@sha256:<hex>"`.
/// The u-boot build verifies the same pin with [`blobs::verify`].
fn blob_pin(dir: &Path, filename: &str) -> Result<String, EngineError> {
    let path = dir.join(filename);
    let bytes = std::fs::read(&path).map_err(|source| EngineError::io(&path, source))?;
    Ok(blobs::pin(filename, &bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::rk1_build;

    #[test]
    fn named_tree_maps_to_kernel_org() {
        assert!(named_tree_url("linux-stable").unwrap().contains("linux-stable.git"));
        assert!(named_tree_url("bogus-tree").is_none());
    }

    #[test]
    fn write_lock_is_atomic_and_leaves_no_temp() {
        use boot2deb_core::lock::{
            BlobsPin, FfmpegPins, GitPin, KernelPin, PatchesPin, RootfsPin, UbootPin,
        };
        // Commits are full 40-hex shas so the round-trip deserialize accepts them
        // (SUB-3); the char picks them apart.
        let git = |c: char| GitPin { reference: "r".into(), commit: std::iter::repeat_n(c, 40).collect() };
        let lock = Lock {
            kernel: KernelPin { id: "k".into(), reference: "v7.1.1".into(), commit: "a".repeat(40) },
            patches: PatchesPin { profile: "rk3588-accel".into(), commit: "b".repeat(40) },
            uboot: UbootPin { reference: "v2026.04".into(), commit: "c".repeat(40) },
            userspace: UserspacePins { mpp: git('1'), librga: git('2'), libmali: git('3') },
            ffmpeg: FfmpegPins { base: git('4'), rockchip: git('5') },
            rootfs: RootfsPin { suite: "forky".into(), manifest: "m.lock".into(), manifest_sha256: None },
            blobs: BlobsPin { atf: "a".into(), tpl: "t".into() },
            extra_debs: vec![],
            snapshot: None,
        };
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("turing-rk1-forky.lock");
        write_lock(&path, &lock).unwrap();
        // The committed lock parses back to the same value...
        let back: Lock = toml::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(lock, back);
        // ...and no `.partial` temp is left behind in the directory.
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains("partial"))
            .collect();
        assert!(leftovers.is_empty(), "temp file left behind: {leftovers:?}");
    }

    #[test]
    fn assembles_lock_from_resolved_build() {
        let build = rk1_build();
        let opts = UpdateOptions {
            kernel_ref: "v7.1.1",
            uboot_ref: "v2026.04",
            mpp_ref: "mainline-cma-fix",
            librga_ref: "master",
            libmali_ref: "master",
            ffmpeg_base_ref: "v4l2-request-n8.1",
            ffmpeg_rockchip_ref: "8.1",
            blobs_dir: Path::new("/unused"),
            patches_path: Path::new("/unused"),
            rootfs_manifest: "turing-rk1-forky.pkgs.lock",
        };
        let git_pin = |r: &str, c: &str| boot2deb_core::lock::GitPin {
            reference: r.into(),
            commit: c.into(),
        };
        let lock = assemble_lock(
            &build,
            &opts,
            "c9acdc466e9aa96352f658b9276aa8a45b8e817d".into(),
            "88dc2788777babfd6322fa655df549a019aa1e69".into(),
            "67750099d1f73e36ca3551de380744a72e4d5ef7".into(),
            UserspacePins {
                mpp: git_pin("mainline-cma-fix", "95a6c48816d39b190be4b7333ad6fc249c08590c"),
                librga: git_pin("master", "2cffdf6f332c3ddb93eb087841d78e8b487db2a3"),
                libmali: git_pin("master", "bd33ee262f47fd936b831afccaa0759b3ecc2482"),
            },
            FfmpegPins {
                base: git_pin("v4l2-request-n8.1", "b57fbbe50c9b2656fad86a1a7eeabfd2b2a50935"),
                rockchip: git_pin("8.1", "f66f2f804627e4464c2d1b10181772b5437bb991"),
            },
            "rk3588_bl31_v1.51.elf@sha256:2847".into(),
            "rk3588_ddr..._v1.19.bin@sha256:e109".into(),
        );
        assert_eq!(lock.kernel.id, "rk3588-mainline-7.1");
        assert_eq!(lock.kernel.reference, "v7.1.1");
        assert_eq!(lock.patches.profile, "rk3588-accel");
        assert_eq!(lock.uboot.reference, "v2026.04");
        assert_eq!(lock.userspace.mpp.commit, "95a6c48816d39b190be4b7333ad6fc249c08590c");
        assert_eq!(lock.ffmpeg.base.commit, "b57fbbe50c9b2656fad86a1a7eeabfd2b2a50935");
        assert_eq!(lock.ffmpeg.rockchip.reference, "8.1");
        assert_eq!(lock.rootfs.suite, "forky");
        assert_eq!(lock.rootfs.manifest, "turing-rk1-forky.pkgs.lock");
        assert!(lock.snapshot.is_none());
        // The shipped RK1 config pulls no pre-built debs; the recorded set is empty
        // (and omitted from the committed lock).
        assert!(lock.extra_debs.is_empty());
        // Serializes to the committed form and parses back.
        let text = lock.to_toml_string().unwrap();
        let back: Lock = toml::from_str(&text).unwrap();
        assert_eq!(lock, back);
    }

    #[test]
    fn lock_consistency_passes_when_matching_and_names_drift() {
        let build = rk1_build();
        let git = |c: &str| GitPin { reference: "r".into(), commit: c.into() };
        // A lock whose resolved-derived axes match the fresh resolution.
        let base_lock = || Lock {
            kernel: KernelPin {
                id: build.kernel.id.clone(),
                reference: "v7.1.1".into(),
                commit: "kc".into(),
            },
            patches: PatchesPin {
                profile: build.kernel.patch_profile.clone(),
                commit: "p".into(),
            },
            uboot: UbootPin { reference: "v".into(), commit: "u".into() },
            userspace: UserspacePins { mpp: git("m"), librga: git("r"), libmali: git("l") },
            ffmpeg: FfmpegPins { base: git("b"), rockchip: git("rk") },
            rootfs: RootfsPin {
                suite: build.suite.clone(),
                manifest: "m".into(),
                manifest_sha256: None,
            },
            blobs: BlobsPin { atf: "a".into(), tpl: "t".into() },
            extra_debs: build.extra_debs.clone(),
            snapshot: None,
        };
        // In step with the resolve → passes.
        check_lock_consistency(&base_lock(), &build).unwrap();
        // A suite change (config drifted since update) is caught and named.
        let mut drifted = base_lock();
        drifted.rootfs.suite = "sid".into();
        match check_lock_consistency(&drifted, &build).unwrap_err() {
            EngineError::LockConfigDrift { axes } => {
                assert_eq!(axes.len(), 1);
                assert!(axes[0].contains("suite"), "message names the axis: {}", axes[0]);
            }
            other => panic!("expected LockConfigDrift, got {other:?}"),
        }
        // Multiple drifted axes are all reported.
        let mut drifted = base_lock();
        drifted.kernel.id = "other-kernel".into();
        drifted.patches.profile = "other-profile".into();
        match check_lock_consistency(&drifted, &build).unwrap_err() {
            EngineError::LockConfigDrift { axes } => assert_eq!(axes.len(), 2),
            other => panic!("expected LockConfigDrift, got {other:?}"),
        }
    }
}
