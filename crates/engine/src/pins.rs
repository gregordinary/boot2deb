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
    /// Checkout of the `patches` repo whose HEAD pins the series. Consulted only when
    /// the resolved kernel names a patch profile; a build that applies no patches
    /// leaves this unread and locks no `[patches]` table, so it needs no checkout.
    pub patches_path: &'a Path,
    /// Path recorded for the solved package manifest the rootfs stage writes
    /// (the content pin itself is produced then).
    pub rootfs_manifest: &'a str,
}

/// Resolve a build to an exact [`Lock`] by consulting upstream and the vendored
/// blobs. This is the only function that reaches the network.
///
/// The patches checkout is pinned first, and a missing one
/// ([`EngineError::PatchesCheckoutMissing`]) or a dirty one
/// ([`EngineError::PatchesDirty`]) is refused before any upstream ref is
/// consulted: the pin is `HEAD`, so `update` needs a local clone, and
/// uncommitted changes — typically a just-imported patch — would be silently
/// absent from the lock and resurface at the next build as a pin mismatch.
/// Failing on the local problem first also keeps the refusal instant.
///
/// A kernel with no patch profile skips that step entirely: there is no series to pin,
/// so the `patches` checkout is never read and the resulting lock omits `[patches]`.
/// Pinning a commit nothing consumes would both record a phantom dependency and make
/// `update` fail on a machine with no `patches` clone.
pub fn resolve_lock(build: &ResolvedBuild, opts: &UpdateOptions) -> Result<Lock, EngineError> {
    let patches = build
        .kernel
        .patch_profile()
        .map(|profile| -> Result<PatchesPin, EngineError> {
            // A missing checkout gets the tailored setup error, not a raw git
            // failure: this is the one command that *requires* a local clone
            // (the pin is its HEAD), where `build` would auto-fetch instead.
            if !opts.patches_path.join(".git").exists() {
                return Err(EngineError::PatchesCheckoutMissing {
                    path: opts.patches_path.display().to_string(),
                });
            }
            if !git::is_clean(opts.patches_path)? {
                return Err(EngineError::PatchesDirty {
                    root: opts.patches_path.display().to_string(),
                });
            }
            Ok(PatchesPin {
                profile: profile.to_string(),
                commit: git::rev_parse_head(opts.patches_path)?,
            })
        })
        .transpose()?;
    // Pin the kernel only when it is compiled from source. A distro-package kernel is
    // installed from the mirror, so its version and hash are pinned in the solved
    // package manifest like any other package's — there is no ref to peel and no
    // commit to record, and the lock omits `[kernel]` entirely.
    let kernel = build
        .kernel
        .compiled()
        .map(|k| -> Result<KernelPin, EngineError> {
            let source = kernel_source_url(&k.source)?;
            let commit = git::resolve_ref(&source, opts.kernel_ref)?;
            Ok(KernelPin {
                id: k.id.clone(),
                source,
                reference: boot2deb_core::sources::normalize_ref(opts.kernel_ref),
                commit,
            })
        })
        .transpose()?;
    // Likewise u-boot and the rkbin blobs: only the boot method that compiles a
    // bootloader has them. A depthcharge board's firmware is its own.
    let (uboot, blobs) = match build.rkbin_boot() {
        Some(boot) => {
            let uboot = UbootPin {
                source: boot.uboot_source.clone(),
                reference: boot2deb_core::sources::normalize_ref(opts.uboot_ref),
                commit: git::resolve_ref(&boot.uboot_source, opts.uboot_ref)?,
            };
            let blobs = BlobsPin {
                atf: blob_pin(opts.blobs_dir, &boot.rkbin.atf)?,
                tpl: blob_pin(opts.blobs_dir, &boot.rkbin.tpl)?,
                bl32: boot
                    .rkbin
                    .bl32
                    .as_deref()
                    .map(|f| blob_pin(opts.blobs_dir, f))
                    .transpose()?,
            };
            (Some(uboot), Some(blobs))
        }
        None => (None, None),
    };
    // Pin the media-accel sources only when the build carries them (a
    // `requires_media_accel` feature is selected); a base build peels no such refs
    // and its lock omits both tables entirely.
    let userspace = build
        .userspace
        .as_ref()
        .map(|u| -> Result<UserspacePins, EngineError> {
            Ok(UserspacePins {
                mpp: git_pin(&u.mpp.git, opts.mpp_ref)?,
                librga: git_pin(&u.librga.git, opts.librga_ref)?,
                libmali: git_pin(&u.libmali.git, opts.libmali_ref)?,
            })
        })
        .transpose()?;
    let ffmpeg = build
        .ffmpeg
        .as_ref()
        .map(|f| -> Result<FfmpegPins, EngineError> {
            Ok(FfmpegPins {
                base: git_pin(&f.base.git, opts.ffmpeg_base_ref)?,
                rockchip: git_pin(&f.rockchip.git, opts.ffmpeg_rockchip_ref)?,
            })
        })
        .transpose()?;
    Ok(assemble_lock(
        build, opts, kernel, uboot, patches, userspace, ffmpeg, blobs,
    ))
}

/// Resolve one git source's ref to an exact commit and record all three into a
/// [`GitPin`] (source URL, ref, commit). A full-SHA ref pins that exact commit
/// — canonicalized to lowercase so `reference` and `commit` agree and classify
/// as a bare commit; a branch/tag name is kept verbatim and peeled to its
/// commit via `ls-remote`.
fn git_pin(url: &str, reference: &str) -> Result<GitPin, EngineError> {
    Ok(GitPin {
        source: url.to_string(),
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
///
/// Every pin is an `Option` for the same reason: a lock records what the build
/// *depends on*, and a build depends on a kernel commit only if it compiles a kernel,
/// on a u-boot commit and rkbin blobs only if it builds a bootloader, on a patch
/// commit only if it applies a series. What a build does not have, its lock does not
/// claim.
#[allow(clippy::too_many_arguments)]
fn assemble_lock(
    build: &ResolvedBuild,
    opts: &UpdateOptions,
    kernel: Option<KernelPin>,
    uboot: Option<UbootPin>,
    patches: Option<PatchesPin>,
    userspace: Option<UserspacePins>,
    ffmpeg: Option<FfmpegPins>,
    blobs: Option<BlobsPin>,
) -> Lock {
    Lock {
        kernel,
        patches,
        uboot,
        userspace,
        ffmpeg,
        rootfs: RootfsPin {
            suite: build.suite.clone(),
            manifest: opts.rootfs_manifest.to_string(),
            // Set once the solved manifest is committed beside the lock; a bare
            // `update` names the manifest but has not produced it yet.
            manifest_sha256: None,
        },
        blobs,
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
/// lock records *from the resolved build*: the kernel definition id, every commit
/// pin's source repo (kernel / u-boot / userspace / ffmpeg), the rkbin blob file
/// names, the patch profile, the suite, the resolved extra-deb set, and media-accel
/// presence (the exact fields `assemble_lock` copies out of the [`ResolvedBuild`]).
///
/// A mismatch means the config drifted since `update` (a device/recipe/suite/feature
/// change), so the lock's pins no longer describe the point the recipe now resolves
/// to. `build` calls this up front and hard-errors with the drifted axes named, rather
/// than building a hybrid of newly resolved axes and stale pins — which would also
/// leave the cache keyed inconsistently (some stages fold lock suite, runtime setup
/// uses resolved suite) (CFG-2). The source-repo comparisons are what keep a commit
/// pin meaningful: a boot-method or SoC-layer flip to a different repo would
/// otherwise fetch that repo at the old commit — a commit that need not exist there,
/// or worse, names an unrelated object (COR-23).
///
/// Deliberately *not* checked: the refs, commits, and hashes (they come from
/// `update`'s refs plus upstream resolution, so they have no fresh-resolve
/// counterpart), the manifest name (update-derived), and layout/image-size (not
/// recorded in the lock; `build` accepts them as per-invocation overrides).
///
/// Kept beside `assemble_lock` so the two stay in lockstep — every resolved-derived
/// field written there is checked here.
pub fn check_lock_consistency(lock: &Lock, build: &ResolvedBuild) -> Result<(), EngineError> {
    /// Record one drifted axis as `axis: lock '<locked>' vs resolved '<resolved>'`.
    fn diff(axes: &mut Vec<String>, axis: &str, locked: &str, resolved: &str) {
        if locked != resolved {
            axes.push(format!("{axis}: lock '{locked}' vs resolved '{resolved}'"));
        }
    }
    /// Record a *shape* drift: the lock pins a source the resolved build no longer
    /// has (or the reverse). A recipe that switched to a distro kernel, or a board to
    /// a boot method with no u-boot, changes which pins exist at all — and building
    /// with the old ones would compile a kernel the image will not install.
    fn presence(axes: &mut Vec<String>, axis: &str, locked: bool, resolved: bool) {
        if locked != resolved {
            let show = |p: bool| if p { "present" } else { "absent" };
            axes.push(format!(
                "{axis}: lock {} vs resolved {}",
                show(locked),
                show(resolved)
            ));
        }
    }
    let mut axes = Vec::new();
    presence(
        &mut axes,
        "compiled kernel",
        lock.kernel.is_some(),
        build.compiles_kernel(),
    );
    if let (Some(pin), Some(kernel)) = (&lock.kernel, build.kernel.compiled()) {
        diff(&mut axes, "kernel id", &pin.id, &kernel.id);
        // The kernel URL is derived from the definition's source; an unknown named
        // tree cannot resolve to a comparable URL here, and the build fails on it
        // moments later with the precise error, so it is skipped rather than doubled.
        if let Ok(url) = kernel_source_url(&kernel.source) {
            diff(&mut axes, "kernel source", &pin.source, &url);
        }
    }
    // The kernel *package* is what a distro-kernel build depends on instead, and it
    // rides the resolved package set, so a change to it is caught by the manifest —
    // but the id must still agree between lock and config.
    presence(
        &mut axes,
        "u-boot",
        lock.uboot.is_some(),
        build.rkbin_boot().is_some(),
    );
    if let (Some(pin), Some(boot)) = (&lock.uboot, build.rkbin_boot()) {
        diff(&mut axes, "u-boot source", &pin.source, &boot.uboot_source);
    }
    if let (Some(lock_us), Some(us)) = (&lock.userspace, &build.userspace) {
        diff(&mut axes, "mpp source", &lock_us.mpp.source, &us.mpp.git);
        diff(&mut axes, "librga source", &lock_us.librga.source, &us.librga.git);
        diff(&mut axes, "libmali source", &lock_us.libmali.source, &us.libmali.git);
    }
    if let (Some(lock_ff), Some(ff)) = (&lock.ffmpeg, &build.ffmpeg) {
        diff(&mut axes, "ffmpeg base source", &lock_ff.base.source, &ff.base.git);
        diff(&mut axes, "ffmpeg rockchip source", &lock_ff.rockchip.source, &ff.rockchip.git);
    }
    // Blob pins are `<file>@sha256:<hex>`; the file component is resolve-derived
    // (the SoC/device layers name the blob set), so a layer flip to a different
    // ATF/TPL/BL32 file must re-pin rather than verify-and-ship the old bytes.
    presence(
        &mut axes,
        "rkbin blobs",
        lock.blobs.is_some(),
        build.rkbin_boot().is_some(),
    );
    if let (Some(pins), Some(boot)) = (&lock.blobs, build.rkbin_boot()) {
        diff(&mut axes, "atf blob", blob_pin_file(&pins.atf), &boot.rkbin.atf);
        diff(&mut axes, "tpl blob", blob_pin_file(&pins.tpl), &boot.rkbin.tpl);
        match (&pins.bl32, &boot.rkbin.bl32) {
            (Some(locked), Some(resolved)) => {
                diff(&mut axes, "bl32 blob", blob_pin_file(locked), resolved)
            }
            (locked, resolved) => {
                presence(&mut axes, "bl32 blob", locked.is_some(), resolved.is_some())
            }
        }
    }
    let lock_profile = lock.patches.as_ref().map(|p| p.profile.as_str());
    if lock_profile != build.kernel.patch_profile() {
        let show = |p: Option<&str>| p.unwrap_or("(none)").to_string();
        axes.push(format!(
            "patch profile: lock '{}' vs resolved '{}'",
            show(lock_profile),
            show(build.kernel.patch_profile())
        ));
    }
    diff(&mut axes, "suite", &lock.rootfs.suite, &build.suite);
    if lock.extra_debs != build.extra_debs {
        axes.push(format!(
            "extra_debs: lock records {} vs resolved {}",
            lock.extra_debs.len(),
            build.extra_debs.len()
        ));
    }
    // Media-accel presence: the lock pins userspace/ffmpeg iff the resolved build
    // builds the stack. A drift here (a feature added or dropped since `update`)
    // would otherwise silently skip or demand the transcode nodes — re-pin instead.
    if lock.userspace.is_some() != build.userspace.is_some() {
        axes.push(format!(
            "media-accel sources: lock {} vs resolved {}",
            if lock.userspace.is_some() { "present" } else { "absent" },
            if build.userspace.is_some() { "present" } else { "absent" },
        ));
    }
    if axes.is_empty() {
        Ok(())
    } else {
        Err(EngineError::LockConfigDrift { axes })
    }
}

/// The `<file>` component of a `"<file>@sha256:<hex>"` blob pin — the
/// resolve-derived half the drift gate compares (the hash half is update-derived).
fn blob_pin_file(pin: &str) -> &str {
    pin.split('@').next().unwrap_or(pin)
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
/// The u-boot build verifies the same pin with [`blobs::verify`]. A blob that
/// does not exist is [`EngineError::BlobMissing`] — the remedy is to vendor the
/// file, which a bare I/O error would not say.
fn blob_pin(dir: &Path, filename: &str) -> Result<String, EngineError> {
    let path = dir.join(filename);
    let bytes = match std::fs::read(&path) {
        Ok(bytes) => bytes,
        Err(source) if source.kind() == std::io::ErrorKind::NotFound => {
            return Err(EngineError::BlobMissing {
                filename: filename.to_string(),
                dir: dir.display().to_string(),
            })
        }
        Err(source) => return Err(EngineError::io(&path, source)),
    };
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
    fn blob_pin_names_the_vendoring_remedy_when_missing() {
        // A blob the resolved build names but the blob dir does not hold: the
        // error names the file, the searched dir, and the vendoring remedy —
        // not a bare I/O "No such file or directory".
        let dir = tempfile::tempdir().unwrap();
        let err = blob_pin(dir.path(), "rk3588_bl31_v1.51.elf").unwrap_err();
        match &err {
            EngineError::BlobMissing { filename, dir: d } => {
                assert_eq!(filename, "rk3588_bl31_v1.51.elf");
                assert_eq!(*d, dir.path().display().to_string());
            }
            e => panic!("expected BlobMissing, got {e:?}"),
        }
        assert!(err.to_string().contains("vendor it there"), "remedy in message: {err}");
        // A present blob still pins.
        std::fs::write(dir.path().join("blob.bin"), b"bytes").unwrap();
        assert!(blob_pin(dir.path(), "blob.bin").unwrap().starts_with("blob.bin@sha256:"));
    }

    #[test]
    fn resolve_lock_refuses_a_dirty_patches_checkout_before_any_network() {
        // An uncommitted file in the patches checkout: `update` would pin a HEAD
        // that silently excludes it, so resolve_lock refuses. The clean check runs
        // before any upstream ref resolution, which keeps this test offline.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let git = |args: &[&str]| {
            assert!(
                std::process::Command::new("git")
                    .arg("-C")
                    .arg(repo)
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success(),
                "git {args:?} failed"
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@boot2deb"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(repo.join("committed"), "x").unwrap();
        git(&["add", "committed"]);
        git(&["commit", "-qm", "c"]);
        std::fs::write(repo.join("imported.patch"), "not committed").unwrap();

        let build = rk1_build();
        let opts = UpdateOptions {
            kernel_ref: "v7.1.1",
            uboot_ref: "unused",
            mpp_ref: "unused",
            librga_ref: "unused",
            libmali_ref: "unused",
            ffmpeg_base_ref: "unused",
            ffmpeg_rockchip_ref: "unused",
            blobs_dir: Path::new("/unused"),
            patches_path: repo,
            rootfs_manifest: "unused.pkgs.lock",
        };
        let err = resolve_lock(&build, &opts).unwrap_err();
        match &err {
            EngineError::PatchesDirty { root } => {
                assert_eq!(*root, repo.display().to_string());
            }
            e => panic!("expected PatchesDirty, got {e:?}"),
        }
        assert!(err.to_string().contains("commit them"), "remedy in message: {err}");
    }

    #[test]
    fn a_no_patch_kernel_never_reads_the_patches_checkout() {
        // The dirty-checkout refusal above is the observable proof that `resolve_lock`
        // consults `patches_path`. Point a no-patch build at something that is not a
        // git repo at all: whatever else fails, it must not fail on the patches step.
        // A fully-upstream board: no patch profile, and no media-accel sources either
        // (the transcode stack is what a patch profile exists for).
        let mut build = rk1_build();
        if let boot2deb_core::model::ResolvedKernel::Compiled(k) = &mut build.kernel {
            k.patch_profile = None;
        }
        build.userspace = None;
        build.ffmpeg = None;
        let opts = UpdateOptions {
            kernel_ref: "v7.1.1",
            uboot_ref: "unused",
            mpp_ref: "unused",
            librga_ref: "unused",
            libmali_ref: "unused",
            ffmpeg_base_ref: "unused",
            ffmpeg_rockchip_ref: "unused",
            blobs_dir: Path::new("/unused"),
            patches_path: Path::new("/definitely/not/a/git/repo"),
            rootfs_manifest: "unused.pkgs.lock",
        };
        if let Err(e) = resolve_lock(&build, &opts) {
            assert!(
                !matches!(e, EngineError::PatchesDirty { .. }),
                "a no-patch build must not consult the patches checkout, got {e:?}"
            );
        }
        // The pure assembly carries the real contract: no profile -> no `[patches]`.
        // Sources and blob files mirror the resolved build so the drift gate below
        // sees a lock that genuinely describes it.
        let boot = build.rkbin_boot().unwrap();
        let kernel = build.kernel.compiled().unwrap();
        let lock = assemble_lock(
            &build,
            &opts,
            Some(KernelPin {
                id: kernel.id.clone(),
                source: kernel_source_url(&kernel.source).unwrap(),
                reference: "v7.1.1".into(),
                commit: "a".repeat(40),
            }),
            Some(UbootPin {
                source: boot.uboot_source.clone(),
                reference: "v2026.04".into(),
                commit: "b".repeat(40),
            }),
            None,
            None,
            None,
            Some(BlobsPin {
                atf: format!("{}@sha256:{}", boot.rkbin.atf, "0".repeat(64)),
                tpl: format!("{}@sha256:{}", boot.rkbin.tpl, "1".repeat(64)),
                bl32: None,
            }),
        );
        assert!(lock.patches.is_none());
        assert!(!lock.to_toml_string().unwrap().contains("[patches]"));
        // ...and the drift gate agrees the lock still describes the build.
        assert!(check_lock_consistency(&lock, &build).is_ok());
    }

    #[test]
    fn write_lock_is_atomic_and_leaves_no_temp() {
        use boot2deb_core::lock::{
            BlobsPin, FfmpegPins, GitPin, KernelPin, PatchesPin, RootfsPin, UbootPin,
        };
        // Commits are full 40-hex shas so the round-trip deserialize accepts them
        // (SUB-3); the char picks them apart.
        let git = |c: char| GitPin { source: "s".into(), reference: "r".into(), commit: std::iter::repeat_n(c, 40).collect() };
        let lock = Lock {
            kernel: Some(KernelPin { id: "k".into(), source: "ks".into(), reference: "v7.1.1".into(), commit: "a".repeat(40) }),
            patches: Some(PatchesPin { profile: "rk3588-accel".into(), commit: "b".repeat(40) }),
            uboot: Some(UbootPin { source: "us".into(), reference: "v2026.04".into(), commit: "c".repeat(40) }),
            userspace: Some(UserspacePins { mpp: git('1'), librga: git('2'), libmali: git('3') }),
            ffmpeg: Some(FfmpegPins { base: git('4'), rockchip: git('5') }),
            rootfs: RootfsPin { suite: "forky".into(), manifest: "m.lock".into(), manifest_sha256: None },
            blobs: Some(BlobsPin { atf: "a.elf@sha256:0000000000000000000000000000000000000000000000000000000000000000".into(), tpl: "t.bin@sha256:1111111111111111111111111111111111111111111111111111111111111111".into(), bl32: None }),
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
            source: "https://src.example/repo.git".into(),
            reference: r.into(),
            commit: c.into(),
        };
        let lock = assemble_lock(
            &build,
            &opts,
            Some(KernelPin {
                id: "rk3588-mainline-7.1".into(),
                source: "https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux-stable.git".into(),
                reference: "v7.1.1".into(),
                commit: "c9acdc466e9aa96352f658b9276aa8a45b8e817d".into(),
            }),
            Some(UbootPin {
                source: build.rkbin_boot().unwrap().uboot_source.clone(),
                reference: "v2026.04".into(),
                commit: "88dc2788777babfd6322fa655df549a019aa1e69".into(),
            }),
            Some(PatchesPin {
                profile: "rk3588-accel".into(),
                commit: "67750099d1f73e36ca3551de380744a72e4d5ef7".into(),
            }),
            Some(UserspacePins {
                mpp: git_pin("mainline-cma-fix", "95a6c48816d39b190be4b7333ad6fc249c08590c"),
                librga: git_pin("master", "2cffdf6f332c3ddb93eb087841d78e8b487db2a3"),
                libmali: git_pin("master", "bd33ee262f47fd936b831afccaa0759b3ecc2482"),
            }),
            Some(FfmpegPins {
                base: git_pin("v4l2-request-n8.1", "b57fbbe50c9b2656fad86a1a7eeabfd2b2a50935"),
                rockchip: git_pin("8.1", "f66f2f804627e4464c2d1b10181772b5437bb991"),
            }),
            Some(BlobsPin {
                atf: "rk3588_bl31_v1.51.elf@sha256:2222222222222222222222222222222222222222222222222222222222222222".into(),
                tpl: "rk3588_ddr_v1.19.bin@sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".into(),
                bl32: None,
            }),
        );
        let kernel_pin = lock.kernel.as_ref().unwrap();
        assert_eq!(kernel_pin.id, "rk3588-mainline-7.1");
        assert_eq!(
            kernel_pin.source,
            "https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux-stable.git"
        );
        assert_eq!(kernel_pin.reference, "v7.1.1");
        assert_eq!(lock.patches.as_ref().unwrap().profile, "rk3588-accel");
        // The u-boot source is recorded from the resolved boot method (COR-23).
        let uboot_pin = lock.uboot.as_ref().unwrap();
        assert_eq!(uboot_pin.source, build.rkbin_boot().unwrap().uboot_source);
        assert_eq!(uboot_pin.reference, "v2026.04");
        let us = lock.userspace.as_ref().unwrap();
        let ff = lock.ffmpeg.as_ref().unwrap();
        assert_eq!(us.mpp.commit, "95a6c48816d39b190be4b7333ad6fc249c08590c");
        assert_eq!(ff.base.commit, "b57fbbe50c9b2656fad86a1a7eeabfd2b2a50935");
        assert_eq!(ff.rockchip.reference, "8.1");
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
        let base_lock = || matching_lock(&build);
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
        drifted.kernel.as_mut().unwrap().id = "other-kernel".into();
        drifted.patches.as_mut().unwrap().profile = "other-profile".into();
        match check_lock_consistency(&drifted, &build).unwrap_err() {
            EngineError::LockConfigDrift { axes } => assert_eq!(axes.len(), 2),
            other => panic!("expected LockConfigDrift, got {other:?}"),
        }
    }

    #[test]
    fn update_names_a_missing_patches_checkout_with_a_remedy() {
        // A kernel with a patch profile needs a local checkout (the pin is its
        // HEAD); a missing one is the tailored setup error, not a raw git failure
        // — and it fails before any upstream ref is consulted.
        let build = rk1_build();
        let opts = UpdateOptions {
            kernel_ref: "v7.1.1",
            uboot_ref: "unused",
            mpp_ref: "unused",
            librga_ref: "unused",
            libmali_ref: "unused",
            ffmpeg_base_ref: "unused",
            ffmpeg_rockchip_ref: "unused",
            blobs_dir: Path::new("/unused"),
            patches_path: Path::new("/definitely/not/a/checkout"),
            rootfs_manifest: "unused.pkgs.lock",
        };
        match resolve_lock(&build, &opts).unwrap_err() {
            EngineError::PatchesCheckoutMissing { path } => {
                assert!(path.contains("definitely"), "{path}");
            }
            other => panic!("expected PatchesCheckoutMissing, got {other:?}"),
        }
    }

    #[test]
    fn lock_consistency_catches_source_and_blob_drift() {
        // The COR-23 axes: a config flip that changes where a commit pin points
        // (boot method / SoC layer source) or which blob files the build consumes
        // must fail the drift gate, not fetch the old pin from the new place.
        let build = rk1_build();
        let base_lock = |mutate: &dyn Fn(&mut Lock)| {
            let mut lock = matching_lock(&build);
            mutate(&mut lock);
            lock
        };
        for (label, lock) in [
            (
                "u-boot source",
                base_lock(&|l| l.uboot.as_mut().unwrap().source = "https://other.example/u-boot.git".into()),
            ),
            (
                "kernel source",
                base_lock(&|l| l.kernel.as_mut().unwrap().source = "https://other.example/linux.git".into()),
            ),
            (
                "mpp source",
                base_lock(&|l| {
                    l.userspace.as_mut().unwrap().mpp.source = "https://other.example/mpp.git".into()
                }),
            ),
            (
                "ffmpeg base source",
                base_lock(&|l| {
                    l.ffmpeg.as_mut().unwrap().base.source = "https://other.example/ffmpeg.git".into()
                }),
            ),
            (
                "atf blob",
                base_lock(&|l| l.blobs.as_mut().unwrap().atf = "rk3588_bl31_v0.99.elf@sha256:aa".into()),
            ),
            (
                "bl32 blob",
                base_lock(&|l| l.blobs.as_mut().unwrap().bl32 = Some("optee.bin@sha256:dd".into())),
            ),
        ] {
            match check_lock_consistency(&lock, &build).unwrap_err() {
                EngineError::LockConfigDrift { axes } => {
                    assert_eq!(axes.len(), 1, "{label}: exactly one axis drifts: {axes:?}");
                    assert!(axes[0].contains(label), "{label} named in: {}", axes[0]);
                }
                other => panic!("{label}: expected LockConfigDrift, got {other:?}"),
            }
        }
    }

    /// A lock whose resolve-derived axes all match `build` — the drift-test
    /// baseline (commits/hashes are placeholders; the gate never reads them).
    fn matching_lock(build: &ResolvedBuild) -> Lock {
        let us = build.userspace.as_ref().unwrap();
        let ff = build.ffmpeg.as_ref().unwrap();
        let git = |source: &str, c: &str| GitPin {
            source: source.into(),
            reference: "r".into(),
            commit: c.into(),
        };
        let kernel = build.kernel.compiled().unwrap();
        let boot = build.rkbin_boot().unwrap();
        Lock {
            kernel: Some(KernelPin {
                id: kernel.id.clone(),
                source: kernel_source_url(&kernel.source).unwrap(),
                reference: "v7.1.1".into(),
                commit: "kc".into(),
            }),
            patches: build.kernel.patch_profile().map(|profile| PatchesPin {
                profile: profile.to_string(),
                commit: "p".into(),
            }),
            uboot: Some(UbootPin {
                source: boot.uboot_source.clone(),
                reference: "v".into(),
                commit: "u".into(),
            }),
            userspace: Some(UserspacePins {
                mpp: git(&us.mpp.git, "m"),
                librga: git(&us.librga.git, "r"),
                libmali: git(&us.libmali.git, "l"),
            }),
            ffmpeg: Some(FfmpegPins {
                base: git(&ff.base.git, "b"),
                rockchip: git(&ff.rockchip.git, "rk"),
            }),
            rootfs: RootfsPin {
                suite: build.suite.clone(),
                manifest: "m".into(),
                manifest_sha256: None,
            },
            blobs: Some(BlobsPin {
                atf: format!("{}@sha256:aa", boot.rkbin.atf),
                tpl: format!("{}@sha256:bb", boot.rkbin.tpl),
                bl32: boot.rkbin.bl32.as_ref().map(|f| format!("{f}@sha256:cc")),
            }),
            extra_debs: build.extra_debs.clone(),
            snapshot: None,
        }
    }
}
