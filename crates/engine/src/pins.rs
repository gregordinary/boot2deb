//! Lock resolution: turn a [`ResolvedBuild`] plus the recipe's ref
//! constraints into an exact [`Lock`] — the sole path that consults upstream.
//!
//! The assembly (`assemble_lock`) is pure and unit-tested. The resolution
//! ([`resolve_lock`]) is the side-effecting shell. It peels refs to commits with
//! `git ls-remote`, pins and hashes the rkbin blobs at a commit of their upstream
//! ([`rkbin::resolve`]), and reads the patches-repo HEAD. `boot2deb build` never runs
//! this — it reads the committed lock.

use crate::blobs;
use crate::error::EngineError;
use crate::git;
use crate::rkbin;
use boot2deb_core::lock::{
    AppPin, BlobsPin, DotnetPins, FfmpegPins, GitPin, KernelPin, KmodPin, Lock, NugetPin,
    PatchesPin, RootfsPin, TfaPin, UbootPin, UserspacePin,
};
use boot2deb_core::model::{App, AppBuild, DotnetSdk, KernelSource};
use boot2deb_core::ResolvedBuild;
use std::path::Path;

/// Inputs for `boot2deb update` beyond the resolved build itself.
///
/// The refs are the exact tags to pin. Auto-resolving a kernel `track` to its
/// latest tag is a later refinement. Today the lock is seeded by pinning `v7.2.3`
/// explicitly, which is also how any specific historical build is pinned.
pub struct UpdateOptions<'a> {
    /// Kernel ref to pin and resolve to a commit (e.g. `v7.2.3`).
    pub kernel_ref: &'a str,
    /// u-boot ref to pin (defaults to the boot-method's `uboot_ref`).
    pub uboot_ref: &'a str,
    /// Trusted Firmware-A ref to pin (defaults to the SoC's `[tfa].ref`). Read only
    /// when the resolved BL31 provider is `tfa`.
    pub tfa_ref: &'a str,
    /// Per-tree refs to pin the media-accel userspace sources at (`(name, ref)`), in
    /// any order. A tree absent here falls back to its own `[[userspace]]` declared
    /// `ref`. The caller seeds it from the previous lock (inheritance) or a
    /// `--userspace-ref` flag, the same rule the out-of-tree modules follow.
    pub userspace_refs: &'a [(String, String)],
    /// ffmpeg base (V4L2) ref to pin (defaults to the SoC layer's `ffmpeg.base`).
    pub ffmpeg_base_ref: &'a str,
    /// ffmpeg Rockchip provenance-tree ref to pin (defaults to the SoC layer's
    /// `ffmpeg.rockchip`). Recorded as provenance for the graft series. Not
    /// fetched at build time.
    pub ffmpeg_rockchip_ref: &'a str,
    /// Per-name refs to pin the device's out-of-tree modules at (`(name, ref)`), in
    /// any order. A name absent here falls back to that `device_kmods` entry's declared
    /// `ref`. The caller seeds it from the previous lock (inheritance) or a flag.
    pub kmod_refs: &'a [(String, String)],
    /// The previous lock's rkbin pin, when there is one. Its commit is kept while it
    /// is from the same repository and ref and contains every file the build names
    /// ([`rkbin::resolve`]). An update that moves another axis then leaves the rkbin
    /// pin where it was.
    pub previous_blobs: Option<&'a BlobsPin>,
    /// Checkout of the `patches` repo whose HEAD pins the series. Consulted only when
    /// the resolved kernel names a patch series. A build that applies no patches
    /// leaves this unread and locks no `[patches]` table, so it needs no checkout.
    pub patches_path: &'a Path,
    /// Path recorded for the solved package manifest the rootfs stage writes
    /// (the content pin itself is produced then).
    pub rootfs_manifest: &'a str,
    /// The digest of the compiled kernel's configuration inputs
    /// ([`kernel_config_digest`]), which the lock records as
    /// [`KernelPin::config_sha256`]. The caller computes it, because the inputs are
    /// files found along the config search path. `None` for a build that compiles no
    /// kernel, and required for one that does.
    pub kernel_config_sha256: Option<&'a str>,
    /// The rootfs `SOURCE_DATE_EPOCH` this lock records, in Unix seconds: the time of
    /// this update. The caller keeps the previous lock's value instead when no pin moved
    /// ([`Lock::keep_source_date_epoch`]).
    pub source_date_epoch: u64,
    /// Produces each `dotnet-deb` app's NuGet pin from its pinned trees. Consulted once
    /// per such app, after its trees are pinned.
    pub nuget: &'a dyn NugetResolver,
}

/// The pinned trees and toolchain of one `dotnet-deb` app, as `update` has just
/// resolved them: everything a restore needs to reproduce the build's package set.
pub struct AppTreePins<'a> {
    /// The app's own tree.
    pub app: &'a GitPin,
    /// The tree whose `debian/` builds the deb.
    pub packaging: &'a GitPin,
    /// The app's patch-series pin, when it applies one.
    pub patches: Option<&'a PatchesPin>,
    /// The SDK the restore runs.
    pub sdk: &'a DotnetSdk,
}

/// Produces a `dotnet-deb` app's [`NugetPin`] — the one step of `update` that runs a
/// restore with the network.
///
/// A trait because the step needs what only the caller has. That is the sandbox roots
/// to run the restore in, and the directory beside the lock to write the sidecar into.
/// [`resolve_lock`] stays the one place that decides *which* pins exist, and asks this
/// for the one it cannot compute itself.
pub trait NugetResolver {
    /// Restore `app`'s package set from `pins`, write it as the sidecar the returned
    /// pin names, and return that pin.
    fn resolve(&self, app: &App, pins: &AppTreePins) -> Result<NugetPin, EngineError>;
}

/// The digest a lock records for a compiled kernel's configuration inputs
/// ([`KernelPin::config_sha256`]), as 64 lowercase hex.
///
/// `fragments` is each config fragment in merge order, as `(name, contents)`, and
/// `device_dts` each board device-tree source in installed order, likewise. The names
/// are the config's own: a fragment's name from the kernel definition, a source's path
/// from the device layer. A renamed or reordered input is therefore a different
/// configuration, as it is to the merge. Pure, so `update` and `build` reach the same
/// answer from the same files.
pub fn kernel_config_digest(
    base_defconfig: &str,
    fragments: &[(&str, &[u8])],
    device_dts: &[(&str, &[u8])],
) -> String {
    let mut text = format!("base_defconfig {base_defconfig}\n");
    for (name, bytes) in fragments {
        text.push_str(&format!(
            "fragment {name} {}\n",
            crate::blobs::sha256_hex(bytes)
        ));
    }
    for (name, bytes) in device_dts {
        text.push_str(&format!(
            "device_dts {name} {}\n",
            crate::blobs::sha256_hex(bytes)
        ));
    }
    crate::blobs::sha256_hex(text.as_bytes())
}

/// Refuse a build whose kernel configuration moved since the lock was written.
///
/// `digest` is [`kernel_config_digest`] over the config tree as it is now, or `None`
/// for a build that compiles no kernel. A lock whose
/// [`config_sha256`](KernelPin::config_sha256) differs describes a kernel with another
/// configuration under the same revision, which apt could not tell from this one. The
/// fix is `update`, which records the new digest and moves the revision on. The
/// lock's presence of a kernel is [`check_lock_consistency`]'s to compare.
pub fn check_kernel_config(lock: &Lock, digest: Option<&str>) -> Result<(), EngineError> {
    match (&lock.kernel, digest) {
        (Some(pin), Some(digest)) if pin.config_sha256 != digest => {
            Err(EngineError::LockConfigDrift {
                axes: vec![format!(
                    "kernel configuration (base defconfig, config fragments, board \
                     device-tree sources): lock '{}' vs config tree '{}'",
                    &pin.config_sha256[..12],
                    &digest[..digest.len().min(12)]
                )],
            })
        }
        _ => Ok(()),
    }
}

/// The digest a lock records for the u-boot package's configuration inputs
/// ([`UbootPin::config_sha256`]), as 64 lowercase hex. It is `None` for a build that
/// compiles no u-boot.
///
/// The inputs are the ones that field's documentation lists: the values the u-boot
/// node folds into its output signature that no pin records. Each is recorded under its
/// name with its length, so no two sets of values digest alike. The submodules are a
/// set, as the TF-A node folds them. Pure, so `update` and `build` reach the same answer
/// from the same resolved build.
pub fn uboot_config_digest(build: &ResolvedBuild) -> Option<String> {
    let boot = build.rkbin_boot()?;
    let mut inputs = vec![
        ("device", build.device.as_str()),
        ("description", build.description.as_str()),
        ("soc", build.soc.as_str()),
        ("boot_method", build.boot_method.as_str()),
        ("arch", build.arch.as_str()),
        ("uboot_defconfig", boot.uboot_defconfig.as_str()),
        ("offset.idbloader", boot.offsets.idbloader.as_str()),
        ("offset.uboot_itb", boot.offsets.uboot_itb.as_str()),
    ];
    if let Some(tfa) = boot.bl31.tfa() {
        inputs.push(("tfa.plat", tfa.plat.as_str()));
        let mut submodules: Vec<&str> = tfa.submodules.iter().map(String::as_str).collect();
        submodules.sort_unstable();
        submodules.dedup();
        inputs.extend(submodules.into_iter().map(|s| ("tfa.submodule", s)));
    }
    let text: String = inputs
        .iter()
        .map(|(name, value)| format!("{name} {} {value}\n", value.len()))
        .collect();
    Some(crate::blobs::sha256_hex(text.as_bytes()))
}

/// Refuse a build whose u-boot configuration moved since the lock was written.
///
/// `digest` is [`uboot_config_digest`] over the build as it resolves now, or `None` for
/// a build that compiles no u-boot. A lock whose
/// [`config_sha256`](UbootPin::config_sha256) differs would archive another package
/// under the version it names. Running `update` records the new digest and moves the
/// [revision](UbootPin::revision) on. The lock's presence of a u-boot is
/// [`check_lock_consistency`]'s to compare.
pub fn check_uboot_config(lock: &Lock, digest: Option<&str>) -> Result<(), EngineError> {
    match (&lock.uboot, digest) {
        (Some(pin), Some(digest)) if pin.config_sha256 != digest => {
            Err(EngineError::LockConfigDrift {
                axes: vec![format!(
                    "u-boot configuration (defconfig, offsets, board identity, TF-A \
                     platform): lock '{}' vs config tree '{}'",
                    &pin.config_sha256[..12],
                    &digest[..digest.len().min(12)]
                )],
            })
        }
        _ => Ok(()),
    }
}

/// Resolve a build to an exact [`Lock`] by consulting upstream and the vendored
/// blobs. This is the only function that reaches the network.
///
/// The patches checkout is pinned first. A missing one
/// ([`EngineError::PatchesCheckoutMissing`]) or a dirty one
/// ([`EngineError::PatchesDirty`]) is refused before any upstream ref is
/// consulted. The pin is `HEAD`, so `update` needs a local clone. Uncommitted
/// changes, typically a just-imported patch, would be silently absent from the
/// lock and resurface at the next build as a pin mismatch.
/// Failing on the local problem first also keeps the refusal instant.
///
/// A kernel with no patch series skips that step entirely. There is no series to pin,
/// so the `patches` checkout is never read and the resulting lock omits `[patches]`.
/// Pinning a commit nothing consumes would both record a phantom dependency and make
/// `update` fail on a machine with no `patches` clone.
pub fn resolve_lock(build: &ResolvedBuild, opts: &UpdateOptions) -> Result<Lock, EngineError> {
    // Every patch axis — the kernel's, u-boot's and each app's — is pinned at the same
    // local `patches` checkout's HEAD. Establish it once when any names a series (a
    // missing checkout gets the tailored setup error, not a raw git failure: this is
    // the one command that *requires* a local clone, where `build` would auto-fetch),
    // then pin each axis against it. A build with neither series never reads it.
    let image = build.image.as_ref();
    let kernel_series = image.map(|i| i.kernel.patch_series()).unwrap_or(&[]);
    let uboot_series = build.rkbin_boot().and_then(|b| b.uboot_series.as_deref());
    let app_series = image.is_some_and(|i| i.apps.iter().any(|a| !a.patch_series.is_empty()));
    let patches_commit = if !kernel_series.is_empty() || uboot_series.is_some() || app_series {
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
        Some(git::rev_parse_head(opts.patches_path)?)
    } else {
        None
    };
    // Resolution guarantees named series contain their source + ref, so a series is
    // never pinned without naming the repo and ref it came from.
    let patches = (!kernel_series.is_empty()).then(|| {
        let compiled = image
            .and_then(|i| i.kernel.compiled())
            .expect("kernel patch series imply a compiled kernel");
        PatchesPin {
            series: kernel_series.to_vec(),
            source: compiled
                .patches_url
                .clone()
                .expect("resolution rejects series without a patches_url"),
            reference: compiled
                .patches_ref
                .clone()
                .expect("resolution pairs patches_ref with the series"),
            commit: patches_commit
                .clone()
                .expect("kernel series mean the checkout was read"),
        }
    });
    let uboot_patches = uboot_series.map(|series| {
        let boot = build
            .rkbin_boot()
            .expect("a u-boot patch series implies a rockchip-rkbin boot");
        PatchesPin {
            series: vec![series.to_string()],
            source: boot
                .uboot_patches_url
                .clone()
                .expect("resolution rejects a u-boot series without a patches_url"),
            reference: boot
                .uboot_patches_ref
                .clone()
                .expect("resolution pairs the u-boot patches_ref with the series"),
            commit: patches_commit
                .clone()
                .expect("a u-boot series means the checkout was read"),
        }
    });
    // Pin the kernel only when it is compiled from source. A distro-package kernel is
    // installed from the mirror, so its version and hash are pinned in the solved
    // package manifest like any other package's — there is no ref to peel and no
    // commit to record, and the lock omits `[kernel]` entirely.
    let kernel = image
        .and_then(|i| i.kernel.compiled())
        .map(|k| -> Result<KernelPin, EngineError> {
            let source = kernel_source_url(&k.source)?;
            let commit = git::resolve_ref(&source, opts.kernel_ref)?;
            Ok(KernelPin {
                id: k.id.clone(),
                source,
                reference: boot2deb_core::sources::normalize_ref(opts.kernel_ref),
                commit,
                // The first revision of this ref. The caller moves it on from the previous
                // lock's with `Lock::carry_kernel_revision`.
                revision: 1,
                config_sha256: opts
                    .kernel_config_sha256
                    .expect("the caller digests a compiled kernel's configuration")
                    .to_string(),
            })
        })
        .transpose()?;
    // Likewise u-boot and the rkbin blobs: only the boot method that compiles a
    // bootloader has them. A depthcharge board's firmware is its own. The BL31 is
    // pinned as exactly one of the rkbin blob or the TF-A tree, whichever provider
    // resolution selected.
    let (uboot, blobs, tfa) = match build.rkbin_boot() {
        Some(boot) => {
            let uboot = UbootPin {
                source: boot.uboot_source.clone(),
                reference: boot2deb_core::sources::normalize_ref(opts.uboot_ref),
                commit: git::resolve_ref(&boot.uboot_source, opts.uboot_ref)?,
                // The first revision of this upstream version. The caller moves it on
                // from the previous lock's with `Lock::carry_uboot_revision`.
                revision: 1,
                config_sha256: uboot_config_digest(build)
                    .expect("a rockchip-rkbin boot compiles u-boot"),
            };
            let tfa = boot
                .bl31
                .tfa()
                .map(|src| -> Result<TfaPin, EngineError> {
                    Ok(TfaPin {
                        source: src.git.clone(),
                        reference: boot2deb_core::sources::normalize_ref(opts.tfa_ref),
                        commit: git::resolve_ref(&src.git, opts.tfa_ref)?,
                    })
                })
                .transpose()?;
            let blobs = pin_blobs(boot, opts.previous_blobs)?;
            (Some(uboot), Some(blobs), tfa)
        }
        None => (None, None, None),
    };
    // Pin the media-accel sources only when the build includes them (a
    // `requires_media_accel` feature is selected); a base build peels no such refs
    // and its lock omits both tables entirely.
    // One pin per tree the build compiles, in the SoC's declared order — so the lock's
    // `[[userspace]]` array mirrors that SoC's own statement about what hardware it has.
    // The ref to pin comes from `opts.userspace_refs` (previous-lock inheritance or a
    // `--userspace-ref <name>=<ref>` flag) and falls back to the tree's declared `ref`.
    let userspace = image
        .map(|i| i.userspace.as_slice())
        .unwrap_or(&[])
        .iter()
        .map(|t| -> Result<UserspacePin, EngineError> {
            let reference = opts
                .userspace_refs
                .iter()
                .find(|(name, _)| name == &t.name)
                .map(|(_, r)| r.as_str())
                .unwrap_or(&t.git_ref);
            let pin = git_pin(&t.git, reference)?;
            Ok(UserspacePin {
                name: t.name.clone(),
                source: pin.source,
                reference: pin.reference,
                commit: pin.commit,
            })
        })
        .collect::<Result<Vec<_>, EngineError>>()?;
    let ffmpeg = image
        .and_then(|i| i.ffmpeg.as_ref())
        .map(|f| -> Result<FfmpegPins, EngineError> {
            Ok(FfmpegPins {
                base: git_pin(&f.base.git, opts.ffmpeg_base_ref)?,
                rockchip: f
                    .rockchip
                    .as_ref()
                    .map(|s| git_pin(&s.git, opts.ffmpeg_rockchip_ref))
                    .transpose()?,
            })
        })
        .transpose()?;
    // Pin each out-of-tree module the board declares, in declared order. The ref to
    // pin comes from `opts.kmod_refs` (previous-lock inheritance / flag) and falls back
    // to the device's own declared ref; the commit is peeled like any other git source.
    let kmods = image
        .map(|i| i.device_kmods.as_slice())
        .unwrap_or(&[])
        .iter()
        .map(|k| -> Result<KmodPin, EngineError> {
            let reference = opts
                .kmod_refs
                .iter()
                .find(|(name, _)| name == &k.name)
                .map(|(_, r)| r.as_str())
                .unwrap_or(&k.git_ref);
            let pin = git_pin(&k.git, reference)?;
            Ok(KmodPin {
                name: k.name.clone(),
                source: pin.source,
                reference: pin.reference,
                commit: pin.commit,
            })
        })
        .collect::<Result<Vec<_>, EngineError>>()?;
    // Pin each application a selected feature compiles from source, in resolution
    // order. There is no `--app-ref` flag: unlike a userspace tree or an out-of-tree
    // module, an app is upstream software pinned at a release tag, and moving it is
    // an edit to the feature rather than a per-build choice. Its trees are pinned
    // first, and the NuGet set a `dotnet-deb` build restores is resolved from them.
    let apps = image
        .map(|i| i.apps.as_slice())
        .unwrap_or(&[])
        .iter()
        .map(|a| -> Result<AppPin, EngineError> {
            let pin = git_pin(&a.git, &a.git_ref)?;
            let patches = (!a.patch_series.is_empty()).then(|| PatchesPin {
                series: a.patch_series.clone(),
                source: a
                    .patches_url
                    .clone()
                    .expect("resolution rejects an app series without a patches_url"),
                reference: a
                    .patches_ref
                    .clone()
                    .expect("resolution pairs an app's patches_ref with its series"),
                commit: patches_commit
                    .clone()
                    .expect("an app series means the checkout was read"),
            });
            let dotnet = match &a.build {
                AppBuild::DotnetDeb(d) => {
                    let packaging = git_pin(&d.packaging.git, &d.packaging.git_ref)?;
                    let nuget = opts.nuget.resolve(
                        a,
                        &AppTreePins {
                            app: &pin,
                            packaging: &packaging,
                            patches: patches.as_ref(),
                            sdk: &d.sdk,
                        },
                    )?;
                    Some(DotnetPins {
                        packaging,
                        sdk: d.sdk.clone(),
                        nuget,
                    })
                }
            };
            Ok(AppPin {
                name: a.name.clone(),
                source: pin.source,
                reference: pin.reference,
                commit: pin.commit,
                patches,
                dotnet,
            })
        })
        .collect::<Result<Vec<_>, EngineError>>()?;
    Ok(assemble_lock(
        build,
        opts,
        kernel,
        uboot,
        patches,
        uboot_patches,
        tfa,
        userspace,
        ffmpeg,
        blobs,
        kmods,
        apps,
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
/// The write is atomic: a uniquely-named temp beside the destination, renamed into
/// place. The lock is the build's source of truth. An interruption or storage fault
/// mid-write must never leave a truncated `.lock` a later `build` would parse or
/// partially trust. The temp shares the destination's directory so
/// the rename stays on one filesystem (where rename is atomic).
pub fn write_lock(path: &Path, lock: &Lock) -> Result<(), EngineError> {
    let text = lock.to_toml_string()?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));
    let file_name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("recipe.lock");
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
// Each pin is its own type, so a transposed pair does not compile — the argument a
// parameter object would win here has already been won by the types. What the list is
// long *for* is that a lock has this many tables.
#[allow(clippy::too_many_arguments)]
fn assemble_lock(
    build: &ResolvedBuild,
    opts: &UpdateOptions,
    kernel: Option<KernelPin>,
    uboot: Option<UbootPin>,
    patches: Option<PatchesPin>,
    uboot_patches: Option<PatchesPin>,
    tfa: Option<TfaPin>,
    userspace: Vec<UserspacePin>,
    ffmpeg: Option<FfmpegPins>,
    blobs: Option<BlobsPin>,
    kmods: Vec<KmodPin>,
    apps: Vec<AppPin>,
) -> Lock {
    Lock {
        kernel,
        patches,
        uboot,
        uboot_patches,
        tfa,
        userspace,
        ffmpeg,
        // One pin per device `device_kmods` entry; empty (and omitted from the committed
        // lock) for a board that has no out-of-tree module.
        kmods,
        // One pin per app a selected feature compiles; empty, and omitted, otherwise.
        apps,
        // The rootfs pin exists only for an image build; a u-boot-only build resolves
        // no suite, so the lock omits `[rootfs]`.
        rootfs: build.image.as_ref().map(|image| RootfsPin {
            distro: build.distro.name.clone(),
            suite: image.suite.clone(),
            manifest: opts.rootfs_manifest.to_string(),
            // Set once the solved manifest is committed beside the lock; a bare
            // `update` names the manifest but has not produced it yet.
            manifest_sha256: None,
            source_date_epoch: opts.source_date_epoch,
        }),
        blobs,
        // The resolved extra-deb pins recorded verbatim — the sha256 is already the
        // exact content pin, so there is nothing to resolve. `update`
        // fetches/verifies/stores them; `build` materializes from the store. Empty
        // when no layer or feature adds one.
        extra_debs: build
            .image
            .as_ref()
            .map(|i| i.extra_debs.clone())
            .unwrap_or_default(),
        // Captured opt-in on a successful build (`--save-snapshot`), not here.
        snapshot: None,
    }
}

/// Assert the committed lock still agrees with a fresh resolution on every axis the
/// lock records *from the resolved build*:
///
/// - The kernel definition id
/// - Every commit pin's source repo (kernel / u-boot / TF-A / rkbin / userspace /
///   ffmpeg / out-of-tree modules)
/// - The BL31 provider, which decides whether `[blobs].atf` or `[tfa]` exists
/// - The rkbin blob file names
/// - The patch series
/// - The distro and the suite
/// - The resolved extra-deb set
/// - The out-of-tree module set
/// - Media-accel presence
///
/// Those are the exact fields `assemble_lock` copies out of the [`ResolvedBuild`].
///
/// A mismatch means the config drifted since `update` (a device/recipe/suite/feature
/// change), so the lock's pins no longer describe the point the recipe now resolves
/// to. `build` calls this up front and hard-errors with the drifted axes named, rather
/// than building a hybrid of newly resolved axes and stale pins. A hybrid would also
/// leave the cache keyed inconsistently (some stages fold lock suite, runtime setup
/// uses resolved suite). The source-repo comparisons are what keep a commit pin
/// meaningful. A boot-method or SoC-layer flip to a different repo would otherwise
/// fetch that repo at the old commit. That commit need not exist there, or worse,
/// names an unrelated object.
///
/// Deliberately *not* checked: the refs, commits, and hashes (they come from
/// `update`'s refs plus upstream resolution, so they have no fresh-resolve
/// counterpart), the manifest name (update-derived), and layout/image-size (not
/// recorded in the lock, because `build` accepts them as per-invocation overrides).
///
/// Kept beside `assemble_lock` so the two stay in lockstep — every resolved-derived
/// field written there is checked here.
pub fn check_lock_consistency(lock: &Lock, build: &ResolvedBuild) -> Result<(), EngineError> {
    // Every image-axis check reads through this: a u-boot deliverable has none of them,
    // and the pins that describe them must then be absent rather than merely unequal —
    // which is what the `presence` checks below assert.
    let image = build.image.as_ref();
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
    if let (Some(pin), Some(kernel)) = (
        &lock.kernel,
        build.image.as_ref().and_then(|i| i.kernel.compiled()),
    ) {
        diff(&mut axes, "kernel id", &pin.id, &kernel.id);
        // The kernel URL is derived from the definition's source; an unknown named
        // tree cannot resolve to a comparable URL here, and the build fails on it
        // moments later with the precise error, so it is skipped rather than doubled.
        if let Ok(url) = kernel_source_url(&kernel.source) {
            diff(&mut axes, "kernel source", &pin.source, &url);
        }
    }
    // The kernel *package* is what a distro-kernel build depends on instead, and it
    // is part of the resolved package set, so a change to it is caught by the manifest —
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
    // The BL31 provider: the lock pins TF-A iff the resolved build selects it. A
    // recipe that moved between providers must re-pin, not pack the other BL31. The
    // source is compared where both sides have TF-A, as u-boot's is.
    let resolved_tfa = build.rkbin_boot().and_then(|b| b.bl31.tfa());
    presence(
        &mut axes,
        "tf-a",
        lock.tfa.is_some(),
        resolved_tfa.is_some(),
    );
    if let (Some(pin), Some(src)) = (&lock.tfa, resolved_tfa) {
        diff(&mut axes, "tf-a source", &pin.source, &src.git);
    }
    // A tree appearing or disappearing is drift in its own right — the SoC's statement
    // about its hardware changed, or this build stopped asking for an optional tree — so
    // presence is checked per name before the source URL, and the URL only where both
    // sides have the tree.
    let resolved_us = image.map(|i| i.userspace.as_slice()).unwrap_or(&[]);
    for name in lock
        .userspace
        .iter()
        .map(|p| p.name.as_str())
        .chain(resolved_us.iter().map(|t| t.name.as_str()))
        .collect::<std::collections::BTreeSet<_>>()
    {
        let lock_pin = lock.userspace.iter().find(|p| p.name == name);
        let tree = resolved_us.iter().find(|t| t.name == name);
        presence(&mut axes, name, lock_pin.is_some(), tree.is_some());
        if let (Some(p), Some(t)) = (lock_pin, tree) {
            diff(&mut axes, &format!("{name} source"), &p.source, &t.git);
        }
    }
    // Presence before sources, as the userspace trees get: the ffmpeg pin exists iff the
    // build compiles the media-accel stack, so an appearance or disappearance is the
    // feature selection having moved since `update` — the same drift the userspace check
    // below reports, and it must be caught on this axis too rather than only where both
    // sides happen to agree.
    presence(
        &mut axes,
        "ffmpeg",
        lock.ffmpeg.is_some(),
        image.is_some_and(|i| i.ffmpeg.is_some()),
    );
    if let (Some(lock_ff), Some(ff)) = (&lock.ffmpeg, image.and_then(|i| i.ffmpeg.as_ref())) {
        diff(
            &mut axes,
            "ffmpeg base source",
            &lock_ff.base.source,
            &ff.base.git,
        );
        presence(
            &mut axes,
            "ffmpeg rockchip",
            lock_ff.rockchip.is_some(),
            ff.rockchip.is_some(),
        );
        if let (Some(p), Some(s)) = (&lock_ff.rockchip, &ff.rockchip) {
            diff(&mut axes, "ffmpeg rockchip source", &p.source, &s.git);
        }
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
        // The commit is meaningful only in the repository it was pinned from.
        diff(
            &mut axes,
            "rkbin source",
            &pins.source,
            &boot.rkbin.upstream.git,
        );
        match (&pins.atf, boot.bl31.rkbin_file()) {
            (Some(locked), Some(resolved)) => {
                diff(&mut axes, "atf blob", blob_pin_file(locked), resolved)
            }
            (locked, resolved) => {
                presence(&mut axes, "atf blob", locked.is_some(), resolved.is_some())
            }
        }
        diff(
            &mut axes,
            "tpl blob",
            blob_pin_file(&pins.tpl),
            &boot.rkbin.tpl,
        );
        match (&pins.bl32, &boot.rkbin.bl32) {
            (Some(locked), Some(resolved)) => {
                diff(&mut axes, "bl32 blob", blob_pin_file(locked), resolved)
            }
            (locked, resolved) => {
                presence(&mut axes, "bl32 blob", locked.is_some(), resolved.is_some())
            }
        }
    }
    let show = |p: Option<&str>| p.unwrap_or("(none)").to_string();
    let show_list = |p: &[String]| {
        if p.is_empty() {
            "(none)".to_string()
        } else {
            p.join(", ")
        }
    };
    // The kernel composes an ordered series list; a changed set or order must re-pin.
    let lock_series: &[String] = lock
        .patches
        .as_ref()
        .map(|p| p.series.as_slice())
        .unwrap_or(&[]);
    let resolved_series = image.map(|i| i.kernel.patch_series()).unwrap_or(&[]);
    if lock_series != resolved_series {
        axes.push(format!(
            "patch series: lock '{}' vs resolved '{}'",
            show_list(lock_series),
            show_list(resolved_series)
        ));
    }
    // The u-boot patch axis: the lock pins it iff the resolved boot method selects a
    // real u-boot series. A series change (or a board switching off u-boot) since
    // `update` must re-pin, not build the old series. u-boot names exactly one series,
    // so compare the lone name.
    let lock_uboot_series = lock
        .uboot_patches
        .as_ref()
        .and_then(|p| p.series.first())
        .map(String::as_str);
    let resolved_uboot_series = build.rkbin_boot().and_then(|b| b.uboot_series.as_deref());
    if lock_uboot_series != resolved_uboot_series {
        axes.push(format!(
            "u-boot patch series: lock '{}' vs resolved '{}'",
            show(lock_uboot_series),
            show(resolved_uboot_series)
        ));
    }
    // The rootfs pin exists iff the build produces an image; a suite drift is only
    // meaningful when both sides have one.
    presence(
        &mut axes,
        "rootfs",
        lock.rootfs.is_some(),
        build.produces_image(),
    );
    if let (Some(pin), Some(image)) = (&lock.rootfs, image) {
        diff(&mut axes, "distro", &pin.distro, &build.distro.name);
        diff(&mut axes, "suite", &pin.suite, &image.suite);
    }
    let resolved_debs = image.map(|i| i.extra_debs.as_slice()).unwrap_or(&[]);
    if lock.extra_debs != resolved_debs {
        axes.push(format!(
            "extra_debs: lock records {} vs resolved {}",
            lock.extra_debs.len(),
            resolved_debs.len()
        ));
    }
    // Media-accel presence: the lock pins userspace/ffmpeg iff the resolved build
    // builds the stack. A drift here (a feature added or dropped since `update`)
    // would otherwise silently skip or demand the transcode nodes — re-pin instead.
    // The ffmpeg half is checked with its sources above.
    presence(
        &mut axes,
        "media-accel sources",
        !lock.userspace.is_empty(),
        image.is_some_and(|i| !i.userspace.is_empty()),
    );
    // Out-of-tree modules: the lock pins one per `device_kmods` entry. A board that
    // added, removed, or renamed a kmod — or repointed one at a different repo — must
    // re-pin, since building the old commit from a URL that need not contain it is the
    // very source-drift class the git pins guard against. Keyed by name, order-free.
    let resolved_kmods = image.map(|i| i.device_kmods.as_slice()).unwrap_or(&[]);
    for res in resolved_kmods {
        match lock.kmods.iter().find(|p| p.name == res.name) {
            Some(pin) => diff(
                &mut axes,
                &format!("kmod '{}' source", res.name),
                &pin.source,
                &res.git,
            ),
            None => axes.push(format!("kmod '{}': resolved but not in lock", res.name)),
        }
    }
    for pin in &lock.kmods {
        if !resolved_kmods.iter().any(|k| k.name == pin.name) {
            axes.push(format!(
                "kmod '{}': in lock but no longer resolved",
                pin.name
            ));
        }
    }
    app_drift(
        &mut axes,
        &lock.apps,
        image.map(|i| i.apps.as_slice()).unwrap_or(&[]),
    );
    if axes.is_empty() {
        Ok(())
    } else {
        Err(EngineError::LockConfigDrift { axes })
    }
}

/// Record each way the lock's `[[apps]]` disagree with the resolved apps.
///
/// Unlike most sources, an app's *refs* come from config rather than from `update`
/// flags: a feature moving `ref = "v12.1"` to `"v12.2"` is exactly the edit that must
/// re-pin, so the refs are compared here along with the sources. So is the SDK, which
/// the lock records verbatim and which decides the NuGet set `update` resolved. Keyed
/// by name, order-free, like the kmods.
fn app_drift(axes: &mut Vec<String>, locked: &[AppPin], resolved: &[App]) {
    for app in resolved {
        let Some(pin) = locked.iter().find(|p| p.name == app.name) else {
            axes.push(format!("app '{}': resolved but not in lock", app.name));
            continue;
        };
        let name = &app.name;
        let mut diff = |axis: &str, locked: &str, resolved: &str| {
            if locked != resolved {
                axes.push(format!(
                    "app '{name}' {axis}: lock '{locked}' vs resolved '{resolved}'"
                ));
            }
        };
        diff("source", &pin.source, &app.git);
        diff(
            "ref",
            &pin.reference,
            &boot2deb_core::sources::normalize_ref(&app.git_ref),
        );
        let locked_series = pin.patches.as_ref().map(|p| p.series.join(", "));
        diff(
            "patch series",
            locked_series.as_deref().unwrap_or("(none)"),
            &if app.patch_series.is_empty() {
                "(none)".to_string()
            } else {
                app.patch_series.join(", ")
            },
        );
        if let (Some(p), Some(url)) = (&pin.patches, &app.patches_url) {
            diff("patches source", &p.source, url);
        }
        match (&app.build, &pin.dotnet) {
            (AppBuild::DotnetDeb(d), Some(locked)) => {
                diff(
                    "packaging source",
                    &locked.packaging.source,
                    &d.packaging.git,
                );
                diff(
                    "packaging ref",
                    &locked.packaging.reference,
                    &boot2deb_core::sources::normalize_ref(&d.packaging.git_ref),
                );
                diff("sdk version", &locked.sdk.version, &d.sdk.version);
                if locked.sdk.sha512 != d.sdk.sha512 {
                    axes.push(format!("app '{name}' sdk sha512 table changed"));
                }
            }
            (AppBuild::DotnetDeb(_), None) => {
                axes.push(format!("app '{name}': lock has no dotnet pins"));
            }
        }
    }
    for pin in locked {
        if !resolved.iter().any(|a| a.name == pin.name) {
            axes.push(format!(
                "app '{}': in lock but no longer resolved",
                pin.name
            ));
        }
    }
}

/// The `<file>` component of a `"<file>@sha256:<hex>"` blob pin — the
/// resolve-derived half the drift gate compares (the hash half is update-derived).
fn blob_pin_file(pin: &str) -> &str {
    pin.split('@').next().unwrap_or(pin)
}

/// How a `patches` checkout departs from a lock's pin, or `None` when it sits on the
/// pin with a clean worktree. That is the one state in which a series read from that
/// checkout is exactly the series the lock names.
///
/// Read-only and non-enforcing, for the survey commands. They verify the *working
/// tree* on purpose, because that is what patch co-development needs. They therefore
/// owe the reader a note that their green says nothing about the pinned series. The
/// build path is where the pin is enforced instead — see `build::verify_patches_pin`.
pub fn patches_drift(patches_root: &Path, expected: &str) -> Result<Option<String>, EngineError> {
    let head = git::rev_parse_head(patches_root)?;
    let clean = git::is_clean(patches_root)?;
    if head == expected && clean {
        return Ok(None);
    }
    Ok(Some(describe_patches_drift(
        patches_root,
        &head,
        expected,
        clean,
    )))
}

/// Render one departure as a single clause. Distinguishes the two independent ways a
/// checkout can depart — HEAD naming another commit, and uncommitted work — so a tree
/// sitting *on* the pin is never described as being at a different commit than the one
/// it is at.
pub(crate) fn describe_patches_drift(
    root: &Path,
    head: &str,
    expected: &str,
    clean: bool,
) -> String {
    let root = root.display();
    if head == expected {
        return format!(
            "patches checkout {root} has uncommitted changes at the pinned commit {expected}"
        );
    }
    let dirt = if clean {
        ""
    } else {
        " with uncommitted changes"
    };
    format!("patches checkout {root} is at {head}{dirt}, but the lock pins {expected}")
}

/// Upstream URL for a kernel source. A known named tree resolves to a git.kernel.org
/// URL, and an explicit `{ git, ref }` uses its URL directly. Also the default
/// clone source for the kernel build stage when `--kernel-src` is not given.
pub fn kernel_source_url(source: &KernelSource) -> Result<String, EngineError> {
    match source {
        KernelSource::Named(name) => named_tree_url(name)
            .ok_or_else(|| EngineError::UnknownSourceTree { name: name.clone() }),
        KernelSource::Git { git, .. } => Ok(git.clone()),
    }
}

/// Map a well-known kernel tree name to its clone URL.
fn named_tree_url(name: &str) -> Option<String> {
    let url = match name {
        "linux-stable" => "https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux-stable.git",
        "torvalds" | "linux" => {
            "https://git.kernel.org/pub/scm/linux/kernel/git/torvalds/linux.git"
        }
        _ => return None,
    };
    Some(url.to_string())
}

/// Pin the rkbin blobs a u-boot build consumes: the commit [`rkbin::resolve`] picks
/// and each named file's `"<filename>@sha256:<hex>"`, hashed from the bytes at that
/// commit. The u-boot stage fetches and verifies exactly these
/// ([`rkbin::ensure_files`]).
fn pin_blobs(
    boot: &boot2deb_core::model::ResolvedRkbinBoot,
    previous: Option<&BlobsPin>,
) -> Result<BlobsPin, EngineError> {
    let up = &boot.rkbin.upstream;
    let atf = boot.bl31.rkbin_file();
    let bl32 = boot.rkbin.bl32.as_deref();
    let names: Vec<&str> = [atf, Some(boot.rkbin.tpl.as_str()), bl32]
        .into_iter()
        .flatten()
        .collect();
    let keep = previous
        .filter(|p| p.source == up.git && p.reference == up.git_ref)
        .map(|p| p.commit.as_str());
    let fetched = rkbin::resolve(up, keep, &names)?;
    let pin_of = |name: &str| {
        fetched
            .files
            .iter()
            .find(|(f, _)| f == name)
            .map(|(f, bytes)| blobs::pin(f, bytes))
            .expect("rkbin::resolve returns every file it was asked for")
    };
    Ok(BlobsPin {
        source: up.git.clone(),
        reference: up.git_ref.clone(),
        commit: fetched.commit.clone(),
        atf: atf.map(pin_of),
        tpl: pin_of(&boot.rkbin.tpl),
        bl32: bl32.map(pin_of),
    })
}

#[cfg(test)]
mod tests {
    /// The configuration digest the fixtures' `update` records for a compiled kernel.
    const CONFIG_SHA256: &str = "dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd";

    /// The digest moves with every input it covers, including where an input sits in
    /// the merge order and what it is called, and holds still otherwise.
    #[test]
    fn the_kernel_config_digest_covers_contents_order_and_names() {
        let a: (&str, &[u8]) = ("soc/rk3588", b"CONFIG_A=y\n");
        let b: (&str, &[u8]) = ("device/turing-rk1", b"CONFIG_B=m\n");
        let dts: (&str, &[u8]) = ("devices/x/board.dts", b"/dts-v1/;\n");
        let base = kernel_config_digest("defconfig", &[a, b], &[dts]);
        assert_eq!(base.len(), 64);
        assert_eq!(base, kernel_config_digest("defconfig", &[a, b], &[dts]));
        for other in [
            kernel_config_digest("rockchip_linux_defconfig", &[a, b], &[dts]),
            kernel_config_digest("defconfig", &[b, a], &[dts]),
            kernel_config_digest("defconfig", &[a, ("soc/rk3588", b"CONFIG_B=m\n")], &[dts]),
            kernel_config_digest("defconfig", &[a, ("device/other", b"CONFIG_B=m\n")], &[dts]),
            kernel_config_digest("defconfig", &[a, b], &[]),
            kernel_config_digest("defconfig", &[a, b], &[("devices/x/board.dts", b"changed")]),
        ] {
            assert_ne!(base, other);
        }
    }

    /// `build` refuses a lock whose recorded configuration is not the tree's, names the
    /// axis, and says to re-run `update`. A matching tree, and a build with no compiled
    /// kernel, pass.
    #[test]
    fn a_kernel_configuration_that_moved_since_the_lock_is_refused() {
        let lock = matching_lock(&rk1_build());
        check_kernel_config(&lock, Some(CONFIG_SHA256)).unwrap();
        check_kernel_config(&lock, None).unwrap();
        let moved = "e".repeat(64);
        let err = check_kernel_config(&lock, Some(&moved)).unwrap_err();
        let EngineError::LockConfigDrift { axes } = &err else {
            panic!("{err}");
        };
        assert_eq!(axes.len(), 1);
        assert!(axes[0].starts_with("kernel configuration"), "{}", axes[0]);
        assert!(err.to_string().contains("boot2deb update"), "{err}");
    }

    /// The u-boot digest moves with every unpinned input the package is built from,
    /// holds still for a resolved value the package does not read, and does not exist
    /// for a board whose boot method compiles no u-boot.
    #[test]
    fn the_uboot_config_digest_covers_what_the_package_reads_and_no_more() {
        let rkbin = rk1_build();
        let base = uboot_config_digest(&rkbin).unwrap();
        assert_eq!(base.len(), 64);
        assert_eq!(
            uboot_config_digest(&rk1_build()).as_deref(),
            Some(base.as_str())
        );
        let digest =
            |build: &ResolvedBuild, edit: &dyn Fn(&mut ResolvedBuild, &mut ResolvedRkbinBoot)| {
                let mut build = build.clone();
                let mut boot = build.rkbin_boot().unwrap().clone();
                edit(&mut build, &mut boot);
                build.boot = ResolvedBoot::RockchipRkbin(Box::new(boot));
                uboot_config_digest(&build).unwrap()
            };
        for moved in [
            digest(&rkbin, &|_, b| {
                b.uboot_defconfig = "rock5b-rk3588_defconfig".into()
            }),
            digest(&rkbin, &|_, b| b.offsets.idbloader = "64KiB".into()),
            digest(&rkbin, &|_, b| b.offsets.uboot_itb = "16MiB".into()),
            digest(&rkbin, &|r, _| r.description = "Another board".into()),
            digest(&rkbin, &|r, _| r.device = "another-board".into()),
        ] {
            assert_ne!(moved, base);
        }
        // Where the partitions begin is the image's, not the package's.
        assert_eq!(
            digest(&rkbin, &|_, b| b.offsets.partitions = "32MiB".into()),
            base
        );
        // A value's length is recorded with it, so two inputs cannot trade characters
        // across their boundary and digest alike.
        assert_ne!(
            digest(&rkbin, &|_, b| {
                b.offsets.idbloader = "32KiB 1".into();
                b.offsets.uboot_itb = "MiB".into();
            }),
            digest(&rkbin, &|_, b| {
                b.offsets.idbloader = "32KiB".into();
                b.offsets.uboot_itb = "1 MiB".into();
            }),
        );

        // Under TF-A the platform and the submodule set are inputs too, and the set's
        // order is not.
        let tfa = boot2deb_core::resolve::resolve_recipe(
            &crate::test_support::repo_root(),
            crate::test_support::RK1_MEDIA_ACCEL,
            &boot2deb_core::model::Overrides {
                bl31: Some(boot2deb_core::model::Bl31Provider::Tfa),
                ..Default::default()
            },
        )
        .unwrap();
        let tfa_base = uboot_config_digest(&tfa).unwrap();
        let tfa_source = |edit: &dyn Fn(&mut boot2deb_core::model::TfaSource)| {
            digest(&tfa, &|_, b| {
                let mut src = b.bl31.tfa().unwrap().clone();
                edit(&mut src);
                b.bl31 = boot2deb_core::model::Bl31::Tfa(Box::new(src));
            })
        };
        assert_eq!(tfa_source(&|_| {}), tfa_base);
        assert_ne!(tfa_source(&|s| s.plat = "rk3576".into()), tfa_base);
        assert_ne!(
            tfa_source(&|s| s.submodules = vec!["contrib/dtc".into(), "x".into()]),
            tfa_base
        );
        assert_eq!(
            tfa_source(&|s| s.submodules = vec!["b".into(), "a".into()]),
            tfa_source(&|s| s.submodules = vec!["a".into(), "b".into()])
        );

        let depthcharge = boot2deb_core::resolve::resolve_recipe(
            &crate::test_support::repo_root(),
            "asus-c201/forky",
            &boot2deb_core::model::Overrides::default(),
        )
        .unwrap();
        assert_eq!(uboot_config_digest(&depthcharge), None);
    }

    /// `build` refuses a lock whose u-boot digest is not the config tree's, naming the
    /// axis and `update` as the fix, and has nothing to compare for a build that
    /// compiles no u-boot.
    #[test]
    fn a_uboot_configuration_that_moved_since_the_lock_is_refused() {
        let build = rk1_build();
        let lock = matching_lock(&build);
        let digest = uboot_config_digest(&build).unwrap();
        check_uboot_config(&lock, Some(&digest)).unwrap();
        check_uboot_config(&lock, None).unwrap();
        let err = check_uboot_config(&lock, Some(&"e".repeat(64))).unwrap_err();
        let EngineError::LockConfigDrift { axes } = &err else {
            panic!("{err}");
        };
        assert_eq!(axes.len(), 1);
        assert!(axes[0].starts_with("u-boot configuration"), "{}", axes[0]);
        assert!(err.to_string().contains("boot2deb update"), "{err}");
    }

    /// A resolver for builds that declare no `dotnet-deb` app: reaching it is a bug in
    /// the test, not a resolution to stub.
    struct NoApps;
    impl NugetResolver for NoApps {
        fn resolve(&self, app: &App, _: &AppTreePins) -> Result<NugetPin, EngineError> {
            panic!(
                "no app is declared, yet NuGet was asked to resolve '{}'",
                app.name
            )
        }
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

    /// The image half of a fixture build. Every fixture here resolves a shipped image
    /// recipe, so the axis is there; the unwrap states that rather than threading an
    /// `Option` through every assertion.
    fn image_of(build: &boot2deb_core::ResolvedBuild) -> &boot2deb_core::ResolvedImage {
        pair_of(build).image
    }

    /// The same fixture build as an [`ImageBuild`] pair, for the stages that take one.
    fn pair_of(build: &boot2deb_core::ResolvedBuild) -> boot2deb_core::ImageBuild<'_> {
        build.as_image().expect("the fixture recipes build images")
    }
    use super::*;
    use crate::test_support::rk1_build;
    use boot2deb_core::model::{ResolvedBoot, ResolvedRkbinBoot};

    #[test]
    fn named_tree_maps_to_kernel_org() {
        assert!(named_tree_url("linux-stable")
            .unwrap()
            .contains("linux-stable.git"));
        assert!(named_tree_url("bogus-tree").is_none());
    }

    /// The clause distinguishes the three departures, and in particular never renders
    /// an on-pin dirty tree as a commit mismatch — naming one commit as both "is at"
    /// and "pins" sends the reader looking for drift that is not there.
    #[test]
    fn describe_drift_separates_a_moved_head_from_uncommitted_work() {
        let root = Path::new("/p");
        let (a, b) = ("aaaa", "bbbb");

        let on_pin_dirty = describe_patches_drift(root, a, a, false);
        assert!(
            on_pin_dirty.contains("uncommitted changes at the pinned commit"),
            "{on_pin_dirty}"
        );
        assert!(
            !on_pin_dirty.contains("but the lock pins"),
            "{on_pin_dirty}"
        );

        assert_eq!(
            describe_patches_drift(root, b, a, true),
            format!("patches checkout /p is at {b}, but the lock pins {a}")
        );
        assert_eq!(
            describe_patches_drift(root, b, a, false),
            format!(
                "patches checkout /p is at {b} with uncommitted changes, but the lock pins {a}"
            )
        );
    }

    /// `patches_drift` reads a real checkout: silent when it is on the pin and clean,
    /// and speaking up for each way it can depart. The `None` case is the essential
    /// one — a survey that warned on every run would train the reader to ignore it.
    #[test]
    fn patches_drift_is_silent_only_on_a_clean_checkout_at_the_pin() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let head = commit_one(repo);

        assert_eq!(patches_drift(repo, &head).unwrap(), None);

        std::fs::write(repo.join("extra"), "uncommitted").unwrap();
        let dirty = patches_drift(repo, &head).unwrap().unwrap();
        assert!(
            dirty.contains("uncommitted changes at the pinned commit"),
            "{dirty}"
        );

        std::fs::remove_file(repo.join("extra")).unwrap();
        let other = "0".repeat(40);
        let moved = patches_drift(repo, &other).unwrap().unwrap();
        assert!(moved.contains("but the lock pins"), "{moved}");
        assert!(
            moved.contains(&head),
            "the clause names the actual head: {moved}"
        );
    }

    /// `git init` plus one commit, returning its HEAD.
    fn commit_one(dir: &Path) -> String {
        let git = |args: &[&str]| {
            assert!(std::process::Command::new("git")
                .args(args)
                .current_dir(dir)
                .env("GIT_CONFIG_GLOBAL", "/dev/null")
                .env("GIT_CONFIG_SYSTEM", "/dev/null")
                .output()
                .unwrap()
                .status
                .success());
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(dir.join("series"), "x").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-qm", "one"]);
        crate::git::rev_parse_head(dir).unwrap()
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
            tfa_ref: "",
            userspace_refs: &[],
            ffmpeg_base_ref: "unused",
            ffmpeg_rockchip_ref: "unused",
            kmod_refs: &[],
            previous_blobs: None,
            patches_path: repo,
            rootfs_manifest: "unused.pkgs.lock",
            source_date_epoch: 1_790_347_034,
            kernel_config_sha256: Some(CONFIG_SHA256),
            nuget: &NoApps,
        };
        let err = resolve_lock(&build, &opts).unwrap_err();
        match &err {
            EngineError::PatchesDirty { root } => {
                assert_eq!(*root, repo.display().to_string());
            }
            e => panic!("expected PatchesDirty, got {e:?}"),
        }
        assert!(
            err.to_string().contains("commit them"),
            "remedy in message: {err}"
        );
    }

    #[test]
    fn a_no_patch_kernel_never_reads_the_patches_checkout() {
        // The dirty-checkout refusal above is the observable proof that `resolve_lock`
        // consults `patches_path`. Point a no-patch build at something that is not a
        // git repo at all: whatever else fails, it must not fail on the patches step.
        // A fully-upstream board: no patch series on either axis, and no media-accel
        // sources either (the transcode stack is what a patch series exists for).
        let mut build = rk1_build();
        let image = build
            .image
            .as_mut()
            .expect("the fixture recipe builds an image");
        if let boot2deb_core::model::ResolvedKernel::Compiled(k) = &mut image.kernel {
            k.patch_series = Vec::new();
        }
        image.userspace = Vec::new();
        image.ffmpeg = None;
        if let boot2deb_core::model::ResolvedBoot::RockchipRkbin(b) = &mut build.boot {
            b.uboot_series = None;
        }
        let opts = UpdateOptions {
            kernel_ref: "v7.1.1",
            uboot_ref: "unused",
            tfa_ref: "",
            userspace_refs: &[],
            ffmpeg_base_ref: "unused",
            ffmpeg_rockchip_ref: "unused",
            kmod_refs: &[],
            previous_blobs: None,
            patches_path: Path::new("/definitely/not/a/git/repo"),
            rootfs_manifest: "unused.pkgs.lock",
            source_date_epoch: 1_790_347_034,
            kernel_config_sha256: Some(CONFIG_SHA256),
            nuget: &NoApps,
        };
        if let Err(e) = resolve_lock(&build, &opts) {
            assert!(
                !matches!(e, EngineError::PatchesDirty { .. }),
                "a no-patch build must not consult the patches checkout, got {e:?}"
            );
        }
        // The pure assembly enforces the real contract: no series -> no `[patches]`.
        // Sources and blob files mirror the resolved build so the drift gate below
        // sees a lock that genuinely describes it.
        let boot = build.rkbin_boot().unwrap();
        let kernel = image_of(&build).kernel.compiled().unwrap();
        let lock = assemble_lock(
            &build,
            &opts,
            Some(KernelPin {
                id: kernel.id.clone(),
                source: kernel_source_url(&kernel.source).unwrap(),
                reference: "v7.1.1".into(),
                commit: "a".repeat(40),
                revision: 1,
                config_sha256: "d".repeat(64),
            }),
            Some(UbootPin {
                source: boot.uboot_source.clone(),
                reference: "v2026.04".into(),
                commit: "b".repeat(40),
                revision: 1,
                config_sha256: "9".repeat(64),
            }),
            None,       // patches
            None,       // uboot_patches
            None,       // tfa
            Vec::new(), // userspace
            None,       // ffmpeg
            Some(BlobsPin {
                source: "https://github.com/rockchip-linux/rkbin.git".into(),
                reference: "master".into(),
                commit: "c".repeat(40),
                atf: boot
                    .bl31
                    .rkbin_file()
                    .map(|f| format!("{f}@sha256:{}", "0".repeat(64))),
                tpl: format!("{}@sha256:{}", boot.rkbin.tpl, "1".repeat(64)),
                bl32: None,
            }),
            Vec::new(), // kmods
            Vec::new(), // apps
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
        // Commits are full 40-hex shas so the round-trip deserialize accepts them;
        // the char picks them apart.
        let git = |c: char| GitPin {
            source: "s".into(),
            reference: "r".into(),
            commit: std::iter::repeat_n(c, 40).collect(),
        };
        let lock = Lock {
            kernel: Some(KernelPin {
                id: "k".into(),
                source: "ks".into(),
                reference: "v7.1.1".into(),
                commit: "a".repeat(40),
                revision: 1,
                config_sha256: "d".repeat(64),
            }),
            patches: Some(PatchesPin {
                series: vec!["rk3588-accel".into()],
                source: "ps".into(),
                reference: "main".into(),
                commit: "b".repeat(40),
            }),
            uboot: Some(UbootPin {
                source: "us".into(),
                reference: "v2026.04".into(),
                commit: "c".repeat(40),
                revision: 1,
                config_sha256: "9".repeat(64),
            }),
            uboot_patches: None,
            tfa: None,
            userspace: vec![
                named_pin("mpp", git('1')),
                named_pin("librga", git('2')),
                named_pin("libmali", git('3')),
            ],
            ffmpeg: Some(FfmpegPins {
                base: git('4'),
                rockchip: Some(git('5')),
            }),
            rootfs: Some(RootfsPin {
                distro: "debian".into(),
                suite: "forky".into(),
                manifest: "m.lock".into(),
                manifest_sha256: None,
                source_date_epoch: 1_790_347_034,
            }),
            blobs: Some(BlobsPin {
                source: "https://github.com/rockchip-linux/rkbin.git".into(),
                reference: "master".into(),
                commit: "c".repeat(40),
                atf: Some(
                    "a.elf@sha256:0000000000000000000000000000000000000000000000000000000000000000"
                        .into(),
                ),
                tpl:
                    "t.bin@sha256:1111111111111111111111111111111111111111111111111111111111111111"
                        .into(),
                bl32: None,
            }),
            kmods: vec![],
            apps: vec![],
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

    /// A resolved `dotnet-deb` app and the lock pin `update` would write for it.
    fn app_and_pin() -> (App, AppPin) {
        let sdk = DotnetSdk {
            version: "10.0.401".into(),
            sha512: [("amd64".to_string(), "a".repeat(128))]
                .into_iter()
                .collect(),
        };
        let app = App {
            name: "jellyfin".into(),
            git: "https://x/jellyfin.git".into(),
            git_ref: "v12.1".into(),
            deb: "jellyfin-server".into(),
            patch_series: vec!["jellyfin".into()],
            patches_url: Some("https://x/patches.git".into()),
            patches_ref: Some("main".into()),
            build: AppBuild::DotnetDeb(boot2deb_core::model::DotnetDebBuild {
                packaging: boot2deb_core::model::GitSource {
                    git: "https://x/jellyfin-packaging.git".into(),
                    git_ref: "v12.1-1".into(),
                },
                source_dir: "jellyfin-server".into(),
                project: "Jellyfin.Server".into(),
                sdk: sdk.clone(),
            }),
        };
        let pin = AppPin {
            name: "jellyfin".into(),
            source: "https://x/jellyfin.git".into(),
            reference: "v12.1".into(),
            commit: "1".repeat(40),
            patches: Some(PatchesPin {
                series: vec!["jellyfin".into()],
                source: "https://x/patches.git".into(),
                reference: "main".into(),
                commit: "2".repeat(40),
            }),
            dotnet: Some(DotnetPins {
                packaging: GitPin {
                    source: "https://x/jellyfin-packaging.git".into(),
                    reference: "v12.1-1".into(),
                    commit: "3".repeat(40),
                },
                sdk,
                nuget: NugetPin {
                    manifest: "x.jellyfin.nuget.lock".into(),
                    manifest_sha256: "4".repeat(64),
                },
            }),
        };
        (app, pin)
    }

    /// An app's ref comes from the feature, not an `update` flag, so a feature that
    /// moves it has drifted from its lock. So has one that changes the SDK, which
    /// decided the NuGet set the lock pins.
    #[test]
    fn an_app_whose_feature_moved_has_drifted_from_its_lock() {
        let (app, pin) = app_and_pin();
        let mut axes = Vec::new();
        app_drift(
            &mut axes,
            std::slice::from_ref(&pin),
            std::slice::from_ref(&app),
        );
        assert!(axes.is_empty(), "a matching lock is clean: {axes:?}");

        let mut moved = app.clone();
        moved.git_ref = "v12.2".into();
        let AppBuild::DotnetDeb(d) = &mut moved.build;
        d.sdk.version = "10.0.402".into();
        let mut axes = Vec::new();
        app_drift(&mut axes, std::slice::from_ref(&pin), &[moved]);
        assert_eq!(axes.len(), 2, "{axes:?}");
        assert!(axes[0].contains("ref: lock 'v12.1' vs resolved 'v12.2'"));
        assert!(axes[1].contains("sdk version"));

        let mut axes = Vec::new();
        app_drift(&mut axes, &[], std::slice::from_ref(&app));
        assert_eq!(axes, ["app 'jellyfin': resolved but not in lock"]);
        let mut axes = Vec::new();
        app_drift(&mut axes, &[pin], &[]);
        assert_eq!(axes, ["app 'jellyfin': in lock but no longer resolved"]);
    }

    #[test]
    fn assembles_lock_from_resolved_build() {
        let build = rk1_build();
        let opts = UpdateOptions {
            kernel_ref: "v7.1.1",
            uboot_ref: "v2026.04",
            tfa_ref: "",
            userspace_refs: &[],
            ffmpeg_base_ref: "v4l2-request-n8.1",
            ffmpeg_rockchip_ref: "8.1",
            kmod_refs: &[],
            previous_blobs: None,
            patches_path: Path::new("/unused"),
            rootfs_manifest: "turing-rk1-forky.pkgs.lock",
            source_date_epoch: 1_790_347_034,
            kernel_config_sha256: Some(CONFIG_SHA256),
            nuget: &NoApps,
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
                revision: 1,
                config_sha256: "d".repeat(64),
            }),
            Some(UbootPin {
                source: build.rkbin_boot().unwrap().uboot_source.clone(),
                reference: "v2026.04".into(),
                commit: "88dc2788777babfd6322fa655df549a019aa1e69".into(),
                revision: 1,
                config_sha256: "9".repeat(64),
            }),
            Some(PatchesPin {
                series: vec!["rk3588-accel".into()],
                source: "https://example.invalid/patches.git".into(),
                reference: "main".into(),
                commit: "67750099d1f73e36ca3551de380744a72e4d5ef7".into(),
            }),
            None, // uboot_patches
            None, // tfa
            vec![
                    named_pin("mpp", git_pin("mainline-cma-fix", "95a6c48816d39b190be4b7333ad6fc249c08590c")),
                    named_pin("librga", git_pin("master", "2cffdf6f332c3ddb93eb087841d78e8b487db2a3")),
                    named_pin("libmali", git_pin("master", "bd33ee262f47fd936b831afccaa0759b3ecc2482")),
                ],
            Some(FfmpegPins {
                base: git_pin("v4l2-request-n8.1", "b57fbbe50c9b2656fad86a1a7eeabfd2b2a50935"),
                rockchip: Some(git_pin("8.1", "f66f2f804627e4464c2d1b10181772b5437bb991")),
            }),
            Some(BlobsPin {
                source: "https://github.com/rockchip-linux/rkbin.git".into(),
                reference: "master".into(),
                commit: "c".repeat(40),
                atf: Some("rk3588_bl31_v1.51.elf@sha256:2222222222222222222222222222222222222222222222222222222222222222".into()),
                tpl: "rk3588_ddr_v1.19.bin@sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee".into(),
                bl32: None,
            }),
            Vec::new(), // kmods
            Vec::new(), // apps
        );
        let kernel_pin = lock.kernel.as_ref().unwrap();
        assert_eq!(kernel_pin.id, "rk3588-mainline-7.1");
        assert_eq!(
            kernel_pin.source,
            "https://git.kernel.org/pub/scm/linux/kernel/git/stable/linux-stable.git"
        );
        assert_eq!(kernel_pin.reference, "v7.1.1");
        assert_eq!(
            lock.patches.as_ref().unwrap().series,
            vec!["rk3588-accel".to_string()]
        );
        // The u-boot source is recorded from the resolved boot method.
        let uboot_pin = lock.uboot.as_ref().unwrap();
        assert_eq!(uboot_pin.source, build.rkbin_boot().unwrap().uboot_source);
        assert_eq!(uboot_pin.reference, "v2026.04");
        let ff = lock.ffmpeg.as_ref().unwrap();
        assert_eq!(
            lock.userspace
                .iter()
                .find(|p| p.name == "mpp")
                .expect("the RK1 pins mpp")
                .commit,
            "95a6c48816d39b190be4b7333ad6fc249c08590c"
        );
        assert_eq!(ff.base.commit, "b57fbbe50c9b2656fad86a1a7eeabfd2b2a50935");
        assert_eq!(ff.rockchip.as_ref().unwrap().reference, "8.1");
        assert_eq!(lock.rootfs.as_ref().unwrap().suite, "forky");
        // The distro the suite resolved to comes with it, from the resolved build.
        assert_eq!(lock.rootfs.as_ref().unwrap().distro, "debian");
        assert_eq!(
            lock.rootfs.as_ref().unwrap().manifest,
            "turing-rk1-forky.pkgs.lock"
        );
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
        drifted.rootfs.as_mut().unwrap().suite = "sid".into();
        match check_lock_consistency(&drifted, &build).unwrap_err() {
            EngineError::LockConfigDrift { axes } => {
                assert_eq!(axes.len(), 1);
                assert!(
                    axes[0].contains("suite"),
                    "message names the axis: {}",
                    axes[0]
                );
            }
            other => panic!("expected LockConfigDrift, got {other:?}"),
        }
        // So is a suite that now resolves to another distribution: the solved manifest
        // beside the lock was resolved against the other archive.
        let mut drifted = base_lock();
        drifted.rootfs.as_mut().unwrap().distro = "ubuntu".into();
        match check_lock_consistency(&drifted, &build).unwrap_err() {
            EngineError::LockConfigDrift { axes } => {
                assert_eq!(axes.len(), 1);
                assert!(axes[0].contains("distro"), "{}", axes[0]);
            }
            other => panic!("expected LockConfigDrift, got {other:?}"),
        }
        // Multiple drifted axes are all reported.
        let mut drifted = base_lock();
        drifted.kernel.as_mut().unwrap().id = "other-kernel".into();
        drifted.patches.as_mut().unwrap().series = vec!["other-series".into()];
        match check_lock_consistency(&drifted, &build).unwrap_err() {
            EngineError::LockConfigDrift { axes } => assert_eq!(axes.len(), 2),
            other => panic!("expected LockConfigDrift, got {other:?}"),
        }
    }

    /// Every committed lock, variants included, describes the build point its recipe
    /// resolves to today. A config edit that would make `build` refuse a shipped lock
    /// fails here instead, naming the lock and its drifted axes.
    #[test]
    fn every_shipped_lock_agrees_with_its_fresh_resolution() {
        let root = crate::test_support::repo_root();
        let recipes = root.path().join("recipes");
        let mut locks = Vec::new();
        let mut dirs = vec![recipes.clone()];
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_some_and(|e| e == "lock")
                    // The solved package manifest and an app's NuGet sidecar sit
                    // beside the lock under the same extension; neither is a lock.
                    && !path
                        .file_stem()
                        .is_some_and(|s| s.to_string_lossy().contains('.'))
                {
                    let rel = path.strip_prefix(&recipes).unwrap().with_extension("");
                    locks.push(rel.to_string_lossy().into_owned());
                }
            }
        }
        locks.sort();
        assert!(!locks.is_empty());
        let drifted: Vec<String> = locks
            .iter()
            .filter_map(|reference| {
                let build = boot2deb_core::resolve::resolve_recipe(
                    &root,
                    reference,
                    &boot2deb_core::model::Overrides::default(),
                )
                .unwrap_or_else(|e| panic!("{reference} does not resolve: {e}"));
                let lock = root.lock(reference).unwrap();
                check_lock_consistency(&lock, &build)
                    .err()
                    .map(|e| format!("{reference}: {e}"))
            })
            .collect();
        assert!(drifted.is_empty(), "{drifted:#?}");
    }

    /// Selecting the other BL31 provider is drift, in both directions: the lock pins
    /// exactly the BL31 its point resolved, and a point that moved between providers
    /// re-pins rather than packing the other one.
    #[test]
    fn moving_between_bl31_providers_is_drift() {
        let rkbin = rk1_build();
        let tfa = boot2deb_core::resolve::resolve_recipe(
            &crate::test_support::repo_root(),
            crate::test_support::RK1_MEDIA_ACCEL,
            &boot2deb_core::model::Overrides {
                bl31: Some(boot2deb_core::model::Bl31Provider::Tfa),
                ..Default::default()
            },
        )
        .unwrap();
        let rkbin_lock = matching_lock(&rkbin);
        check_lock_consistency(&rkbin_lock, &rkbin).unwrap();
        let axes = |lock: &Lock, build: &ResolvedBuild| match check_lock_consistency(lock, build) {
            Err(EngineError::LockConfigDrift { axes }) => axes,
            other => panic!("expected LockConfigDrift, got {other:?}"),
        };
        let drift = axes(&rkbin_lock, &tfa);
        assert!(drift.iter().any(|a| a.starts_with("tf-a:")), "{drift:?}");
        assert!(
            drift.iter().any(|a| a.starts_with("atf blob:")),
            "{drift:?}"
        );
        // A TF-A lock for the TF-A point passes, and its source is held to the SoC's.
        let src = tfa.rkbin_boot().unwrap().bl31.tfa().unwrap();
        let mut tfa_lock = matching_lock(&tfa);
        tfa_lock.blobs.as_mut().unwrap().atf = None;
        tfa_lock.tfa = Some(TfaPin {
            source: src.git.clone(),
            reference: src.git_ref.clone(),
            commit: "e".repeat(40),
        });
        check_lock_consistency(&tfa_lock, &tfa).unwrap();
        assert!(!axes(&tfa_lock, &rkbin).is_empty());
        tfa_lock.tfa.as_mut().unwrap().source = "https://example.invalid/fork.git".into();
        let drift = axes(&tfa_lock, &tfa);
        assert_eq!(drift.len(), 1, "{drift:?}");
        assert!(drift[0].starts_with("tf-a source"), "{drift:?}");
    }

    #[test]
    fn a_media_accel_tree_appearing_or_vanishing_is_drift_on_both_axes() {
        // The userspace and ffmpeg pins exist together — resolution gates both on the
        // same media-accel feature — so both must be presence-checked, not only
        // compared where the lock and the resolve happen to agree. A lock that lost one
        // would otherwise reach a build that demands it.
        let build = rk1_build();
        for drop_axis in ["userspace", "ffmpeg"] {
            let mut lock = matching_lock(&build);
            match drop_axis {
                "userspace" => lock.userspace = Vec::new(),
                _ => lock.ffmpeg = None,
            }
            match check_lock_consistency(&lock, &build).unwrap_err() {
                EngineError::LockConfigDrift { axes } => assert!(
                    axes.iter().any(|a| a.contains("absent")),
                    "dropping {drop_axis} must report a presence drift: {axes:?}"
                ),
                other => panic!("expected LockConfigDrift, got {other:?}"),
            }
        }
    }

    #[test]
    fn update_names_a_missing_patches_checkout_with_a_remedy() {
        // A kernel with a patch series needs a local checkout (the pin is its
        // HEAD); a missing one is the tailored setup error, not a raw git failure
        // — and it fails before any upstream ref is consulted.
        let build = rk1_build();
        let opts = UpdateOptions {
            kernel_ref: "v7.1.1",
            uboot_ref: "unused",
            tfa_ref: "",
            userspace_refs: &[],
            ffmpeg_base_ref: "unused",
            ffmpeg_rockchip_ref: "unused",
            kmod_refs: &[],
            previous_blobs: None,
            patches_path: Path::new("/definitely/not/a/checkout"),
            rootfs_manifest: "unused.pkgs.lock",
            source_date_epoch: 1_790_347_034,
            kernel_config_sha256: Some(CONFIG_SHA256),
            nuget: &NoApps,
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
        // The axes that matter here: a config flip that changes where a commit pin points
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
                base_lock(&|l| {
                    l.uboot.as_mut().unwrap().source = "https://other.example/u-boot.git".into()
                }),
            ),
            (
                "kernel source",
                base_lock(&|l| {
                    l.kernel.as_mut().unwrap().source = "https://other.example/linux.git".into()
                }),
            ),
            (
                "mpp source",
                base_lock(&|l| {
                    l.userspace
                        .iter_mut()
                        .find(|p| p.name == "mpp")
                        .expect("the fixture pins mpp")
                        .source = "https://other.example/mpp.git".into()
                }),
            ),
            (
                "ffmpeg base source",
                base_lock(&|l| {
                    l.ffmpeg.as_mut().unwrap().base.source =
                        "https://other.example/ffmpeg.git".into()
                }),
            ),
            (
                "rkbin source",
                base_lock(&|l| {
                    l.blobs.as_mut().unwrap().source = "https://other.example/rkbin.git".into()
                }),
            ),
            (
                "atf blob",
                base_lock(&|l| {
                    l.blobs.as_mut().unwrap().atf = Some("rk3588_bl31_v0.99.elf@sha256:aa".into())
                }),
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
        let ff = image_of(build).ffmpeg.as_ref().unwrap();
        let git = |source: &str, c: &str| GitPin {
            source: source.into(),
            reference: "r".into(),
            commit: c.into(),
        };
        let kernel = image_of(build).kernel.compiled().unwrap();
        let boot = build.rkbin_boot().unwrap();
        Lock {
            kernel: Some(KernelPin {
                id: kernel.id.clone(),
                source: kernel_source_url(&kernel.source).unwrap(),
                reference: "v7.1.1".into(),
                commit: "kc".into(),
                revision: 1,
                config_sha256: "d".repeat(64),
            }),
            patches: {
                let series = image_of(build).kernel.patch_series();
                (!series.is_empty()).then(|| PatchesPin {
                    series: series.to_vec(),
                    source: "https://example.invalid/patches.git".into(),
                    reference: "main".into(),
                    commit: "p".into(),
                })
            },
            uboot: Some(UbootPin {
                source: boot.uboot_source.clone(),
                reference: "v".into(),
                commit: "u".into(),
                revision: 1,
                config_sha256: uboot_config_digest(build).unwrap(),
            }),
            uboot_patches: boot.uboot_series.as_ref().map(|series| PatchesPin {
                series: vec![series.clone()],
                source: "https://example.invalid/patches.git".into(),
                reference: "main".into(),
                commit: "up".into(),
            }),
            // Mirror whichever trees the fixture's SoC declares, so the lock built
            // here matches the config it is later drift-checked against.
            tfa: None,
            userspace: image_of(build)
                .userspace
                .iter()
                .map(|t| named_pin(&t.name, git(&t.git, "c")))
                .collect(),
            ffmpeg: Some(FfmpegPins {
                base: git(&ff.base.git, "b"),
                rockchip: ff.rockchip.as_ref().map(|s| git(&s.git, "rk")),
            }),
            rootfs: Some(RootfsPin {
                distro: "debian".into(),
                suite: image_of(build).suite.clone(),
                manifest: "m".into(),
                manifest_sha256: None,
                source_date_epoch: 1_790_347_034,
            }),
            blobs: Some(BlobsPin {
                source: "https://github.com/rockchip-linux/rkbin.git".into(),
                reference: "master".into(),
                commit: "c".repeat(40),
                atf: boot.bl31.rkbin_file().map(|f| format!("{f}@sha256:aa")),
                tpl: format!("{}@sha256:bb", boot.rkbin.tpl),
                bl32: boot.rkbin.bl32.as_ref().map(|f| format!("{f}@sha256:cc")),
            }),
            kmods: vec![],
            apps: vec![],
            extra_debs: image_of(build).extra_debs.clone(),
            snapshot: None,
        }
    }
}
