//! Kernel compile stage: clone the pinned tree, apply the locked
//! patch series (`git am`), lay down the fragment-derived `.config`, and
//! run `make bindeb-pkg` — producing the `linux-image` / `linux-headers` `.deb`s.
//!
//! The `.config` is generated exactly as the parity check does ([`crate::kconfig`]):
//! base defconfig + fragments merged out-of-tree, then copied into the tree, so the
//! shipped kernel is configured from the same fragments `verify-config` checks.

use crate::build::{
    self, deb_names, pick_deb, stage_artifact, BuildEnv, ClonePinned, CloneMode, PatchScope,
    PatchSeries,
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
    /// Checkout of the `patches` repo at the locked commit.
    pub patches_root: &'a Path,
    /// Resolved kconfig fragment files in merge order (base → soc → accel →
    /// device), as produced from the resolved build's fragment names.
    pub fragments: &'a [PathBuf],
    /// Scratch directory holding the kernel clone (`<work>/linux`) and the `.deb`s
    /// `bindeb-pkg` drops beside it.
    pub work_dir: &'a Path,
    /// Directory the produced `.deb`s are staged into.
    pub out_dir: &'a Path,
    /// The `patches_root` is an explicit `--patches-path` co-dev checkout: a
    /// patches-pin mismatch is a loud warning rather than a hard error.
    pub patches_dev: bool,
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
    let series_fp = if opts.patches_dev {
        build::patch_series_fingerprint(opts.patches_root, &lock.patches.profile, PatchScope::Kernel)
    } else {
        Vec::new()
    };
    let patches = if opts.patches_dev {
        PatchSeries::Dev(&series_fp)
    } else {
        PatchSeries::Pinned
    };

    // Tier-2 output cache: if the full output signature (tree inputs +
    // config + toolchain) is already stored, restore the `.deb`s and skip the
    // clone/patch/configure/compile entirely — the whole payoff of the store.
    let out_man = output_manifest(build, lock, opts, env, patches)?;
    if let Some(root) = opts.store {
        let store = crate::artstore::ArtifactStore::open(root)?;
        if let Some(files) = store.restore("kernel", out_man.signature().as_str(), opts.out_dir)? {
            if let (Some(image_deb), Some(headers_deb)) = (
                crate::build::role_path(&files, "image_deb"),
                crate::build::role_path(&files, "headers_deb"),
            ) {
                step.log(format!(
                    "restored kernel .debs from the artifact cache (signature {})",
                    out_man.signature().short()
                ));
                step.progress(100);
                step.finish();
                return Ok(KernelArtifacts { image_deb, headers_deb });
            }
        }
    }

    // Tier-1 reuse: reuse the cloned+patched tree only when it is stamped
    // with the current input signature. A lock bump (kernel commit or patch pin)
    // changes the signature, so the stale tree is removed and rebuilt rather than
    // silently reused (COR-1). configure()/compile() re-run regardless, so the
    // signature covers only the tree-shaping clone/patch inputs.
    let man = clone_manifest(lock, patches);
    if crate::signature::is_fresh(&tree, &man) {
        step.log(format!(
            "reusing kernel tree at {} (signature {})",
            tree.display(),
            man.signature().short()
        ));
    } else {
        if tree.exists() {
            step.log(format!(
                "kernel tree at {} is stale (inputs changed) — rebuilding",
                tree.display()
            ));
            std::fs::remove_dir_all(&tree).map_err(|s| EngineError::io(&tree, s))?;
        }
        clone_and_patch(build, lock, opts, &tree, &step)?;
        crate::signature::write_manifest(&tree, &man)?;
    }
    step.progress(30);

    configure(build, opts, env, &tree, &step)?;
    step.progress(40);

    // Deterministic build timestamp from the locked base commit, not the tree's
    // README mtime (= clone time) or HEAD (a patch commit stamped now) (COR-9).
    let epoch = crate::git::commit_epoch(&tree, &lock.kernel.commit).ok();
    compile(build, env, &tree, epoch, &step)?;

    let artifacts = collect(opts, &step)?;

    // Store the produced `.deb`s under the output signature so a later build (or a
    // rebuild after `clean`) restores instead of recompiling.
    if let Some(root) = opts.store {
        let store = crate::artstore::ArtifactStore::open(root)?;
        store.put(
            "kernel",
            out_man.signature().as_str(),
            &[
                ("image_deb", artifacts.image_deb.as_path()),
                ("headers_deb", artifacts.headers_deb.as_path()),
            ],
        )?;
        step.log("stored kernel .debs to the artifact cache");
    }
    step.progress(100);
    step.finish();
    Ok(artifacts)
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
) -> Result<crate::signature::SignatureManifest, EngineError> {
    let tree_sig = clone_manifest(lock, patches).signature();
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
/// reference, and the patch series (`build::fold_patch_series`). The source URL is
/// excluded (a commit content-addresses the tree, so the same commit from any mirror
/// is the same tree). The reference is folded because the patch-applicability gate
/// keys on it, so a reference change without a commit change must restamp the tree to
/// force the gate to re-evaluate (CACHE-4). The [`PatchSeries`] fold covers the
/// pinned commit and — in co-dev mode — the live-series fingerprint (CACHE-1), so a
/// co-dev build never shares a stamp with a pinned one and an edited patch restamps.
/// Public so `why-rebuild` ([`crate::plan`]) recomputes the same signature it stamps
/// here.
pub fn clone_manifest(lock: &Lock, patches: PatchSeries) -> crate::signature::SignatureManifest {
    let mut b = crate::signature::SignatureBuilder::new("kernel", CLONE_STAGE_VERSION);
    b.fold_scalar("kernel.commit", &lock.kernel.commit);
    b.fold_scalar("kernel.reference", &lock.kernel.reference);
    build::fold_patch_series(&mut b, &lock.patches.profile, &lock.patches.commit, patches);
    b.manifest()
}

/// Shallow-clone the pinned kernel, verify it sits at the locked commit, enforce
/// the patches pin, and apply the locked kernel patch series in place. A
/// failure removes the partial tree so a resume never reuses a half-patched kernel
/// (via [`build::clone_pinned`], which homes that guard and the pin check).
fn clone_and_patch(
    _build: &ResolvedBuild,
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
        patches_root: opts.patches_root,
        patches_commit: &lock.patches.commit,
        patches_dev: opts.patches_dev,
        profile: &lock.patches.profile,
        scope: PatchScope::Kernel,
        target: &target,
        gate_reference: Some(&lock.kernel.reference),
    };
    let n = build::clone_pinned(&spec, step)?;
    step.log(format!("applied {n} kernel patches ({})", lock.patches.profile));
    Ok(())
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
        let git = |c: &str| GitPin { reference: "r".into(), commit: c.into() };
        Lock {
            kernel: KernelPin { id: "k".into(), reference: "v".into(), commit: kernel_commit.into() },
            patches: PatchesPin { profile: "rk3588-accel".into(), commit: patches_commit.into() },
            uboot: UbootPin { reference: "v".into(), commit: "u".into() },
            userspace: UserspacePins { mpp: git("m"), librga: git("r"), libmali: git("l") },
            ffmpeg: FfmpegPins { base: git("b"), rockchip: git("rk") },
            rootfs: RootfsPin { suite: "forky".into(), manifest: "m".into(), manifest_sha256: None },
            blobs: BlobsPin { atf: "a".into(), tpl: "t".into() },
            extra_debs: vec![],
            snapshot: None,
        }
    }

    #[test]
    fn clone_manifest_tracks_pin_and_dev_inputs() {
        let sig = |kc, pc, patches| clone_manifest(&lock_with(kc, pc), patches).signature;
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
            clone_manifest(&a, PatchSeries::Pinned).signature,
            clone_manifest(&b, PatchSeries::Pinned).signature
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
            patches_root: tmp.path(),
            fragments: &frags,
            work_dir: tmp.path(),
            out_dir: tmp.path(),
            patches_dev: false,
            store: None,
        };
        let env = |tc: &str| BuildEnv {
            cross_compile: None,
            jobs: None,
            toolchain_id: tc.to_string(),
        };
        let sig = |lock: &Lock, env: &BuildEnv| {
            output_manifest(&build, lock, &opts, env, PatchSeries::Pinned).unwrap().signature
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
