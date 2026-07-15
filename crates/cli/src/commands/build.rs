//! `build`: drive the compile / rootfs / image pipeline from the recipe's lock.
//!
//! Reads only the lock for pinned sources (re-pinning is `update`'s job); the
//! resolved build supplies the axes, and the lock-independent image knobs (layout,
//! size) are the only build-time overrides. Every stage streams the structured event
//! stream — rendered for a human, or as NDJSON under `--json` — and every produced
//! artifact travels on it as an [`Event::Artifact`], so both modes share one stdout
//! contract.

use crate::args::{BuildArgs, StageArg};
use crate::artifacts::{kernel_packages, ledger_debs, record_artifacts};
use crate::config::{
    apt_source_keyrings, device_dts_paths, extra_debs_store, fragment_paths, overlay_dirs,
    preflight_config, resolve_patches_source, OverlayStage,
};
use crate::fsutil::absolutize;
use crate::render::{emit_artifact, note, print_event, print_event_json, short};
use crate::workdir::mark_work_dir;
use boot2deb_core::lock::{SnapshotMode, SnapshotPin};
use boot2deb_core::model::{Overrides, ResolvedBoot, ResolvedBuild};
use boot2deb_core::{resolve_recipe, ConfigRoot};
use boot2deb_engine::build::{ffmpeg, kernel, uboot, userspace, BuildEnv};
use boot2deb_engine::debstore::DebStore;
use boot2deb_engine::event::{Event, Step};
use boot2deb_engine::image::{self, ImageOutput};
use boot2deb_engine::rootfs::{self, MmdebstrapRootfs, Rootfs};
use boot2deb_engine::sandbox::{BuildSandbox, RootlessSandbox};
use boot2deb_engine::{extradebs, pins};
use std::path::PathBuf;

/// Run `build <recipe>`.
pub(crate) fn run(
    root: &ConfigRoot,
    recipe: &str,
    args: BuildArgs,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    // `build` reads only the lock for pinned sources; the resolved build
    // supplies the axes. Only the lock-independent image knobs (layout, size) are
    // overridable here — the lock pins no image geometry. The source-pinning axes
    // (kernel/suite/features/boot-method) would mismatch the lock, so re-pinning
    // those is `update`'s job, not a build-time override.
    let lock = root.lock(recipe)?;
    let overrides = Overrides {
        layout: args.layout,
        image_size: args.image_size.clone(),
        ..Overrides::default()
    };
    let resolved = resolve_recipe(root, recipe, &overrides)?;
    // Fail fast if the config drifted since `update`: the lock's resolved-derived axes
    // (kernel id, patch profile, suite, extra_debs) must still match a fresh resolve,
    // or the build would mix new resolved axes with stale pins.
    boot2deb_engine::pins::check_lock_consistency(&lock, &resolved)?;
    // Validate the cheap local config invariants (image geometry, kernel-fragment
    // and apt-keyring existence) up front, so a bad layout or a missing file fails
    // before any stage runs rather than deep in the pipeline.
    preflight_config(root, &resolved)?;

    // Snapshot activation: the effective mode is `--snapshot`, else the
    // lock's captured mode, else off. Resolve the mirror list here so a
    // fallback/pin request with no captured snapshot fails before any stage runs.
    let snapshot_mode = args
        .snapshot
        .or(lock.snapshot.as_ref().map(|s| s.mode))
        .unwrap_or(SnapshotMode::Off);
    let mirrors = boot2deb_engine::snapshot::resolve_mirrors(
        boot2deb_engine::DEFAULT_MIRROR,
        lock.snapshot.as_ref(),
        snapshot_mode,
    )?;

    // Manifest-as-input: if the lock pins a solved-manifest sha256, the
    // committed manifest beside the lock must exist and hash to it, so the pin and
    // the committed artifact never disagree. Skipped when `--save-manifest` re-pins.
    if !args.save_manifest {
        if let Some(pinned) = &lock.rootfs.manifest_sha256 {
            let committed = root.recipe_sibling(recipe, &lock.rootfs.manifest)?;
            if !committed.exists() {
                return Err(format!(
                    "lock pins a manifest sha256 but the committed manifest {} is missing \
                     — commit it (build --save-manifest) or drop the pin",
                    committed.display()
                )
                .into());
            }
            let have = boot2deb_engine::manifest::digest(&committed)?;
            if &have != pinned {
                return Err(format!(
                    "committed manifest {} (sha256 {}) does not match the lock pin {} \
                     — re-run with --save-manifest to re-pin",
                    committed.display(),
                    short(&have),
                    short(pinned)
                )
                .into());
            }
        }
    }

    // Absolute paths: the sandbox enters an arm64 rootfs via `bwrap`, whose
    // `--bind`/`--chdir` require absolute host paths (a relative work dir would
    // resolve against the wrong root inside the namespace).
    let work_dir = absolutize(
        args.work_dir
            .unwrap_or_else(|| PathBuf::from("build").join(recipe)),
    );
    // Stamp the scratch tree as boot2deb-owned before anything writes into it:
    // `clean` removes only stamped work dirs.
    mark_work_dir(&work_dir)?;
    let out_dir = absolutize(args.out_dir.unwrap_or_else(|| work_dir.join("artifacts")));
    // Sweep stale `.partial` staging temps a hard-killed prior run may have left in the
    // artifact dir before the compile stages publish into it. No-op when the
    // dir does not exist yet.
    boot2deb_engine::gc::sweep_stale_temps(&out_dir);
    let blobs_dir = args.blobs_dir.clone().unwrap_or_else(|| {
        let rel = format!("blobs/{}", resolved.soc.as_str());
        root.find_asset(&rel).unwrap_or_else(|| root.path().join(rel))
    });
    // Which compile nodes this build even has. Both are properties of the resolved
    // build, not of the stage flags: a distro-package kernel is installed from the
    // mirror rather than compiled, and a board whose firmware is its own (depthcharge)
    // builds no bootloader at all. Every stage below is gated on these, so an
    // inapplicable stage is skipped in a full build and named as an error when asked
    // for explicitly.
    let compiles_kernel = resolved.compiles_kernel();
    let builds_uboot = resolved.rkbin_boot().is_some();

    let kernel_src = match (&args.kernel_src, resolved.kernel.compiled()) {
        (Some(s), _) => s.clone(),
        (None, Some(k)) => pins::kernel_source_url(&k.source)?,
        // Not fetched: a distro kernel has no source tree.
        (None, None) => String::new(),
    };
    let uboot_src = args.uboot_src.clone().unwrap_or_else(|| {
        resolved
            .rkbin_boot()
            .map(|b| b.uboot_source.clone())
            .unwrap_or_default()
    });
    // The userspace/ffmpeg clone sources default to the resolved SoC-layer URLs, but
    // only exist for a media-accel build; a base build has no such sources and skips
    // those stages, so these are computed inside the stage blocks below.

    // Cross-arch → pass CROSS_COMPILE; native → none.
    let pf = boot2deb_engine::preflight(resolved.arch);
    let cross_compile = pf.cross.then(|| resolved.cross_compile.clone());
    // The Tier-2 artifact store, unless disabled: a durable content-addressed
    // cache of the compile nodes' output `.deb`s under <root>/cache/artifacts, keyed
    // by each node's output signature. The host toolchain identity is folded into the
    // kernel/u-boot output signatures, so probe it once here (skipped when the cache
    // is off — its value then keys nothing).
    let artifact_store: Option<PathBuf> = (!args.no_artifact_cache)
        .then(|| absolutize(root.path().join("cache").join("artifacts")));
    let build_env = BuildEnv {
        toolchain_id: if artifact_store.is_some() {
            boot2deb_engine::toolchain::host_cc_identity(cross_compile.as_deref())
        } else {
            String::new()
        },
        cross_compile,
        jobs: args.jobs,
    };
    // The one stdout contract for a build: human rendering, or NDJSON under
    // --json — artifact locations travel as Event::Artifact either way.
    let sink: fn(Event) = if json {
        |e| print_event_json(&e)
    } else {
        |e| print_event(&e)
    };
    note(
        json,
        &sink,
        "build",
        format!(
            "building {recipe} (arch {}, {} build, work {})",
            resolved.arch,
            if pf.cross { "cross" } else { "native" },
            work_dir.display()
        ),
    );

    // Debian archive keyring for mmdebstrap — the cross sandbox and the rootfs
    // bootstrap: the explicit flag, else the vendored keyring resolved as a
    // non-overlayable trust anchor (an overlay copy is a fail-closed swap),
    // else None (the host apt trust store, only viable on a Debian host).
    //
    // A vendored keyring is additionally held to its fingerprint manifest: it decides
    // whose Release signatures the bootstrap accepts, and as a binary blob it is the
    // one vendored file a reviewer cannot read. An explicit --keyring is the
    // operator's own anchor, chosen deliberately, and is used as given.
    let keyring = match args.keyring.clone() {
        Some(explicit) => Some(explicit),
        None => {
            let vendored = root.find_trust_anchor(
                "blobs/keyrings/debian-archive-keyring.gpg",
                args.unsafe_overlay_keyring,
            )?;
            if let Some(path) = &vendored {
                boot2deb_engine::keyring::verify(path)?;
            }
            vendored
        }
    };

    // The userspace/ffmpeg stages compile the target's .debs inside a rootless
    // userland for the build's suite + arch — never on the host, even when the host
    // arch matches. Those .debs are packaged for the target suite, and their runtime
    // Depends come from `dpkg-shlibdeps` reading the libraries present at build time;
    // building on the host would link against the host's libraries and stamp the
    // host's package names and versions into Depends. Bootstrapped lazily on first
    // use under WORK_DIR/sandbox, keyed by arch + suite so one host can serve several.
    let sandbox: Box<dyn BuildSandbox> = {
        let rootfs = work_dir
            .join("sandbox")
            .join(format!("{}-{}", resolved.arch.debian_arch(), resolved.suite));
        Box::new(RootlessSandbox::new(
            rootfs,
            resolved.suite.clone(),
            resolved.arch.debian_arch().to_string(),
            keyring.clone(),
        ))
    };

    // Resolve the patches source only when there is a series to apply: the lock pins
    // one (its kernel names a patch profile) *and* this run includes a stage that
    // applies it (kernel/u-boot/userspace/ffmpeg — the userspace stage carries the MPP
    // CMA fix). A rootfs/image-only build, or any build of a no-patch kernel, never
    // reads or fetches the `patches` repo.
    //
    // The source is an explicit --patches-path co-dev checkout, else the default
    // ../patches if present, else an auto-fetch at the pinned commit.
    let stage_applies_patches = matches!(
        args.stage,
        StageArg::All
            | StageArg::Kernel
            | StageArg::Dtb
            | StageArg::Uboot
            | StageArg::Userspace
            | StageArg::Ffmpeg
    );
    let checkout = match (&lock.patches, stage_applies_patches) {
        (Some(pin), true) => Some(resolve_patches_source(
            args.patches_path.as_deref(),
            args.patches_url.as_deref(),
            &resolved,
            pin,
            root,
            &sink,
        )?),
        _ => None,
    };
    // Bind the resolved checkout to the lock's pin, so no stage can be handed a
    // profile without a checkout to read it from (or vice versa).
    let patches = checkout.as_ref().zip(lock.patches.as_ref()).map(
        |((root, dev), pin)| boot2deb_engine::build::PatchSource {
            root,
            pin,
            dev: *dev,
        },
    );

    // The rootfs tarball the image stage consumes: produced by the rootfs stage,
    // or supplied directly via --rootfs-tar for an image-only build.
    let mut rootfs_tar = args.rootfs_tar.clone();
    // The solved manifest, captured when this run builds the rootfs; joins the
    // image stage's per-image password to emit the provenance manifest at the end.
    let mut rootfs_manifest: Option<PathBuf> = None;
    // The per-image first-boot password, captured when this run assembles the image
    // (the image stage owns it, splicing it into the staged rootfs).
    let mut first_boot_password: Option<String> = None;
    // The freshly-solved manifest's sha256, set by the rootfs stage — verified
    // against the committed pin and recorded into the lock by `--save-manifest`.
    let mut solved_manifest_digest: Option<String> = None;
    // The `linux-image-*` .deb this run built, if the kernel stage ran here. The
    // rootfs stage installs the kernel by this exact artifact rather than by
    // scanning out_dir, so its package set never depends on stale debs left by
    // earlier builds of other kernel versions.
    let mut kernel_image_deb: Option<PathBuf> = None;

    // Asking for a stage this build does not have is a user error worth naming, not a
    // silent skip — otherwise `--stage kernel` on a board that installs Debian's
    // kernel would exit 0 having done nothing.
    if matches!(args.stage, StageArg::Kernel | StageArg::Dtb) && !compiles_kernel {
        return Err(format!(
            "recipe '{recipe}' uses kernel '{}', which is a distro package installed from \
             the Debian mirror — there is no kernel tree to compile, so the requested \
             stage has nothing to build",
            resolved.kernel.id()
        )
        .into());
    }
    if matches!(args.stage, StageArg::Uboot) && !builds_uboot {
        return Err(format!(
            "recipe '{recipe}' boots via '{}', whose firmware is the board's own — no \
             bootloader is built, so the requested stage has nothing to build",
            resolved.boot_method
        )
        .into());
    }

    // The kernel stage and the DTB fast path share every filesystem input; both
    // prepare the same `<work>/linux` tree, so they resolve their options identically.
    if matches!(args.stage, StageArg::All | StageArg::Kernel | StageArg::Dtb) && compiles_kernel {
        let fragments = fragment_paths(root, &resolved)?;
        let device_dts = device_dts_paths(root, &resolved)?;
        let opts = kernel::KernelOptions {
            source: &kernel_src,
            patches,
            fragments: &fragments,
            device_dts: &device_dts,
            work_dir: &work_dir,
            out_dir: &out_dir,
            store: artifact_store.as_deref(),
        };
        if matches!(args.stage, StageArg::Dtb) {
            let dtb = kernel::build_dtb(&resolved, &lock, &opts, &build_env, &sink)?;
            emit_artifact(&sink, "dtb", "dtb", &dtb);
        } else {
            let artifacts = kernel::build_kernel(&resolved, &lock, &opts, &build_env, &sink)?;
            emit_artifact(&sink, "kernel", "image_deb", &artifacts.image_deb);
            emit_artifact(&sink, "kernel", "headers_deb", &artifacts.headers_deb);
            record_artifacts(
                &out_dir,
                &[artifacts.image_deb.clone(), artifacts.headers_deb.clone()],
            )?;
            kernel_image_deb = Some(artifacts.image_deb.clone());
        }
    }

    if matches!(args.stage, StageArg::All | StageArg::Uboot) && builds_uboot {
        let opts = uboot::UbootOptions {
            source: &uboot_src,
            patches,
            blobs_dir: &blobs_dir,
            work_dir: &work_dir,
            out_dir: &out_dir,
            store: artifact_store.as_deref(),
        };
        let artifacts = uboot::build_uboot(&resolved, &lock, &opts, &build_env, &sink)?;
        emit_artifact(&sink, "uboot", "idbloader", &artifacts.idbloader);
        emit_artifact(&sink, "uboot", "uboot_itb", &artifacts.uboot_itb);
        emit_artifact(&sink, "uboot", "deb", &artifacts.deb);
        record_artifacts(&out_dir, std::slice::from_ref(&artifacts.deb))?;
        // A uboot-only build also emits a standalone, directly-flashable bootloader
        // image (`<device>-boot.img`) — the eMMC/SPI medium for a split install
        // whose OS lives on another disk. A full build skips it: the image stage
        // folds u-boot into the combined image, or emits `-boot.img` for `split`.
        if matches!(args.stage, StageArg::Uboot) {
            let boot_img = image::build_bootloader_image(
                &resolved,
                &artifacts.idbloader,
                &artifacts.uboot_itb,
                &out_dir,
                &sink,
            )?;
            emit_artifact(&sink, "bootloader-image", "boot_img", &boot_img);
        }
    }

    // The userspace/ffmpeg stages run only for a media-accel build (the resolved
    // build carries the sources). An explicit `--stage userspace|ffmpeg` on a base
    // recipe is a user error worth naming rather than silently skipping.
    let media_accel = resolved.userspace.is_some();
    if matches!(args.stage, StageArg::Userspace | StageArg::Ffmpeg) && !media_accel {
        return Err(format!(
            "recipe '{recipe}' builds no media-accel stack (no selected feature requires it), \
             so the requested userspace/ffmpeg stage has nothing to build — add a \
             media-accel feature to the recipe or omit --stage"
        )
        .into());
    }

    if matches!(args.stage, StageArg::All | StageArg::Userspace) && media_accel {
        let us = resolved.userspace.as_ref().expect("media-accel build has userspace sources");
        let mpp_src = args.mpp_src.clone().unwrap_or_else(|| us.mpp.git.clone());
        let librga_src = args.librga_src.clone().unwrap_or_else(|| us.librga.git.clone());
        let libmali_src = args.libmali_src.clone().unwrap_or_else(|| us.libmali.git.clone());
        let opts = userspace::UserspaceOptions {
            mpp_src: &mpp_src,
            librga_src: &librga_src,
            libmali_src: &libmali_src,
            build_libmali: args.build_libmali,
            work_dir: &work_dir,
            out_dir: &out_dir,
            patches,
            store: artifact_store.as_deref(),
        };
        let artifacts = userspace::build_userspace(
            &lock,
            &opts,
            resolved.arch.debian_arch(),
            &build_env,
            sandbox.as_ref(),
            &sink,
        )?;
        for deb in &artifacts.debs {
            emit_artifact(&sink, "userspace", "deb", deb);
        }
        record_artifacts(&out_dir, &artifacts.debs)?;
    }

    if matches!(args.stage, StageArg::All | StageArg::Ffmpeg) && media_accel {
        let ff = resolved.ffmpeg.as_ref().expect("media-accel build has ffmpeg sources");
        let ffmpeg_base_src = args.ffmpeg_base_src.clone().unwrap_or_else(|| ff.base.git.clone());
        // ffmpeg build-depends on the userspace .debs; they are staged in
        // out_dir by the userspace stage (run it first, or with --stage all).
        let opts = ffmpeg::FfmpegOptions {
            base_src: &ffmpeg_base_src,
            patches,
            userspace_debs: &out_dir,
            work_dir: &work_dir,
            out_dir: &out_dir,
            store: artifact_store.as_deref(),
        };
        let artifacts = ffmpeg::build_ffmpeg(
            &lock,
            &opts,
            resolved.arch.debian_arch(),
            &build_env,
            sandbox.as_ref(),
            &sink,
        )?;
        emit_artifact(&sink, "ffmpeg", "deb", &artifacts.deb);
        record_artifacts(&out_dir, std::slice::from_ref(&artifacts.deb))?;
    }

    if matches!(args.stage, StageArg::All | StageArg::Rootfs) {
        // Bootstrap the device rootfs: stand up a local apt repo from the
        // built .debs in out_dir, install the merged package set, apply the layered
        // overlay, and emit the tarball the image stage formats into ext4.
        let preinstall_overlay_dirs = overlay_dirs(root, &resolved, OverlayStage::PreInstall);
        let overlay_dirs = overlay_dirs(root, &resolved, OverlayStage::Customize);
        // The boot-method config the rootfs generates for itself. Only depthcharge has
        // any: its boot payload is a signed kernel built *inside* the rootfs, so the
        // rootfs has to know which board profile to sign for and what cmdline to bake in.
        let boot_config = resolved.depthcharge_boot().map(|b| rootfs::BootConfig::Depthcharge {
            board: &b.board,
            cmdline: &b.cmdline,
        });
        // The rootfs PARTUUID is an *input* here, not an output of the image node: under
        // depthcharge the signed kernel's root= is derived from this rootfs's own
        // /etc/fstab, so the partition has to be named before the filesystem exists.
        let identity = image_identity(recipe, &resolved);
        // The local apt repo is seeded from the artifact ledger — the exact debs the
        // compile stages recorded — not an extension-only scan of out_dir, so an
        // unsigned stray never becomes trusted apt input.
        //
        // A build that compiles nothing stages no `.deb`s of its own, and then an empty
        // ledger is the *correct* state, not a forgotten compile stage — so the ledger
        // is only consulted where artifacts are actually produced. Its local repo is
        // empty (or holds only `extra_debs`), and every package, kernel included, comes
        // from the mirror.
        let produces_debs = compiles_kernel || builds_uboot || media_accel;
        let mut repo_debs = if produces_debs {
            ledger_debs(&out_dir)?
        } else {
            Vec::new()
        };
        // Materialize the pre-built extra_debs into the content store and
        // add them to the local apt repo's deb set — the way a feature's packages
        // reach the solve, but for bytes pulled from outside the mirror. They then
        // fold into the rootfs cache key by content (via `file_fingerprints`), so a
        // changed extra_deb re-bootstraps. The local repo is the trust boundary for
        // these unsigned debs; a package set entry (or another package's
        // dependency) is what actually installs them.
        if !lock.extra_debs.is_empty() {
            let extra = {
                let step = Step::start(&sink, "extra-debs");
                let store = DebStore::open(&extra_debs_store(root))?;
                let paths = extradebs::materialize(root, &lock.extra_debs, &store, &step)?;
                step.finish();
                paths
            };
            repo_debs.extend(extra);
        }
        // The kernel image is a build artifact with a version-specific package
        // name, so install it by the name discovered from the built .deb, on top of
        // the resolved set (the static config can't name a version it hasn't built).
        let extra_packages = kernel_packages(&kernel_image_deb, &repo_debs)?;
        let manifest_out = out_dir.join(&lock.rootfs.manifest);
        // The content-addressed rootfs cache lives under the work dir, so it persists
        // across `--stage` invocations and is shared by every build using this
        // work dir.
        let cache_dir = work_dir.join("cache");
        // Resolve each feature apt source's signing keyring to the vendored host
        // path mmdebstrap verifies the repo against. Existence was already gated at
        // preflight; this stage-time resolution is the backstop for a keyring
        // removed since.
        let apt_repos = apt_source_keyrings(root, &resolved.apt_sources)?;
        // The image's account of itself, staged into the rootfs at
        // `/etc/boot2deb/image.toml`. Assembled here rather than beside the provenance
        // manifest below because it has to exist *before* the rootfs is bootstrapped —
        // it ships inside the tree the bootstrap produces.
        let system_identity = boot2deb_core::provenance::system_identity(&resolved, &lock);
        let opts = rootfs::RootfsOptions {
            repo_debs: &repo_debs,
            overlay_dirs: &overlay_dirs,
            preinstall_overlay_dirs: &preinstall_overlay_dirs,
            boot_config,
            image_identity: &system_identity,
            rootfs_partuuid: identity.rootfs_partuuid,
            out_dir: &out_dir,
            keyring: keyring.as_deref(),
            manifest_out: &manifest_out,
            mirrors: &mirrors,
            extra_packages: &extra_packages,
            rootfs_label: &args.rootfs_label,
            cache_dir: Some(&cache_dir),
            refresh: args.refresh_rootfs,
            apt_sources: &apt_repos,
            // Clamp tarball mtimes to the locked kernel commit's date (the same
            // lock-derived seed the image identifiers use), so only the deliberate
            // per-image password varies between builds of one lock. None
            // on a rootfs-only build with no kernel tree in this work dir.
            source_date_epoch: kernel::source_date_epoch(&work_dir, &lock),
        };
        let artifacts = MmdebstrapRootfs.build(&resolved, &opts, &sink)?;
        emit_artifact(&sink, "rootfs", "tar", &artifacts.tar);
        emit_artifact(&sink, "rootfs", "manifest", &artifacts.manifest);
        // Manifest-as-input verification: unless `--save-manifest` re-pins,
        // a fresh solve must reproduce the committed pin — a drift means the live
        // mirror moved off the pinned package set. Hard error unless the drift is
        // explicitly allowed.
        let solved_digest = boot2deb_engine::manifest::digest(&artifacts.manifest)?;
        if !args.save_manifest {
            if let Some(pinned) = &lock.rootfs.manifest_sha256 {
                match boot2deb_engine::manifest::verify_reproduced(pinned, &solved_digest) {
                    Ok(()) => note(
                        json,
                        &sink,
                        "rootfs",
                        "manifest OK  : reproduces the committed pin".into(),
                    ),
                    Err(e) if args.allow_manifest_drift => eprintln!("warning: {e}"),
                    Err(e) => return Err(e.into()),
                }
            }
        }
        solved_manifest_digest = Some(solved_digest);
        // The account is locked in the tarball; the unique per-image first-boot
        // password is assigned at image assembly (surfaced there), not here.
        rootfs_tar = Some(artifacts.tar);
        rootfs_manifest = Some(artifacts.manifest);
    }

    if matches!(args.stage, StageArg::All | StageArg::Image) {
        // The image node consumes the rootfs tarball plus the u-boot raw-gap
        // payloads staged in out_dir by the earlier stages. The rootfs tar comes
        // from the rootfs stage in this run, else --rootfs-tar, else the
        // conventionally-named artifact the rootfs stage leaves in out_dir — the
        // same auto-discovery the u-boot payloads get below.
        let rootfs_tar = rootfs_tar
            .clone()
            .unwrap_or_else(|| out_dir.join(format!("{}-rootfs.tar", resolved.device)));
        if !rootfs_tar.exists() {
            return Err(format!(
                "rootfs tar not found at {} — run `build {recipe} --stage rootfs` first (or pass --rootfs-tar)",
                rootfs_tar.display()
            )
            .into());
        }
        // Structural gate, not mere existence: confirm the tar is complete
        // and readable through its appended `./etc/shadow` member. An `--stage image`
        // retry after an interrupted rootfs stage then fails cleanly here instead of
        // formatting a truncated tar into a broken ext4 image.
        rootfs::validate_tar(&rootfs_tar)?;
        // The boot payload, per method. A raw-gap bootloader was staged into out_dir
        // by the u-boot stage; a depthcharge board's signed kernel needs nothing here,
        // because it is already inside the rootfs tarball (`depthchargectl` built it
        // there, so the same tool re-signs it on the running board).
        let idbloader = out_dir.join("idbloader.img");
        let uboot_itb = out_dir.join("u-boot.itb");
        // Matched on the resolved boot method, not on a boolean, so adding a third
        // method is a compile error here rather than a silent route into the wrong arm.
        let boot = match &resolved.boot {
            ResolvedBoot::RockchipRkbin(_) => {
                for (what, p) in [("idbloader.img", &idbloader), ("u-boot.itb", &uboot_itb)] {
                    if !p.exists() {
                        return Err(format!(
                            "{what} not found in {} — run `build {recipe} --stage uboot` first",
                            out_dir.display()
                        )
                        .into());
                    }
                }
                image::BootPayload::RockchipRkbin {
                    idbloader: &idbloader,
                    uboot_itb: &uboot_itb,
                }
            }
            ResolvedBoot::Depthcharge(_) => image::BootPayload::Depthcharge,
        };
        let opts = image::ImageOptions {
            rootfs_tar: &rootfs_tar,
            boot,
            out_dir: &out_dir,
            work_dir: &work_dir,
            rootfs_label: &args.rootfs_label,
            identity: image_identity(recipe, &resolved),
            compress: !args.no_compress,
            keep_raw: args.keep_raw,
        };
        let artifacts = image::build_image(&resolved, &opts, &sink)?;
        // The raw paths are deleted after compression unless --keep-raw, so only
        // print them when they still exist on disk.
        if !artifacts.raw_removed {
            match &artifacts.output {
                ImageOutput::Combined { image } => emit_artifact(&sink, "image", "image", image),
                ImageOutput::Split { bootloader, rootfs } => {
                    emit_artifact(&sink, "image", "boot_img", bootloader);
                    emit_artifact(&sink, "image", "rootfs_img", rootfs);
                }
            }
        }
        for xz in &artifacts.compressed {
            emit_artifact(&sink, "image", "compressed", xz);
        }
        // The per-image first-boot password: unique per build, expired so it
        // must be changed at first login. Surfaced here since it exists nowhere else
        // the operator can read it except the provenance manifest.
        note(
            json,
            &sink,
            "image",
            format!(
                "first-boot pw: {}  (user {}, expired — change at first login)",
                artifacts.password,
                rootfs::DEFAULT_USER
            ),
        );
        first_boot_password = Some(artifacts.password);
    }

    // Emit the provenance manifest when this run built both the rootfs and the image
    // — the point at which the solved manifest and the per-image password both exist.
    // It joins the lock's pins, the resolved build point, the solved-manifest digest,
    // the blob hashes, the toolchain identity, and the first-boot credential into
    // one "exactly what went into this image" document for support/security.
    if let (Some(manifest_path), Some(password)) = (&rootfs_manifest, &first_boot_password) {
        let manifest_bytes = std::fs::read(manifest_path)
            .map_err(|e| format!("read solved manifest {}: {e}", manifest_path.display()))?;
        let manifest_sha256 = boot2deb_engine::blobs::sha256_hex(&manifest_bytes);
        let package_count = String::from_utf8_lossy(&manifest_bytes)
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .count();
        let facts = boot2deb_core::provenance::BuildFacts {
            host_arch: pf.host.arch,
            cross: pf.cross,
            manifest_sha256: &manifest_sha256,
            package_count,
            user: rootfs::DEFAULT_USER,
            password,
            // Stamped by build.rs from the boot2deb checkout; the commit is empty when
            // built outside a git tree (e.g. a source tarball), leaving only the version.
            builder_version: env!("CARGO_PKG_VERSION"),
            builder_commit: option_env!("BOOT2DEB_GIT_COMMIT").filter(|s| !s.is_empty()),
            builder_dirty: matches!(option_env!("BOOT2DEB_GIT_DIRTY"), Some("true")),
        };
        let prov = boot2deb_core::provenance::assemble(&resolved, &lock, &facts);
        let prov_path = out_dir.join(format!("{recipe}.provenance.toml"));
        std::fs::write(&prov_path, prov.to_toml_string()?)
            .map_err(|e| format!("write provenance {}: {e}", prov_path.display()))?;
        emit_artifact(&sink, "image", "provenance", &prov_path);
    }

    // `--save-snapshot` / `--save-manifest`: persist the captured snapshot timestamp
    // and/or the freshly-solved manifest into the committed lock. Both mutate
    // the same lock, so apply them together and write it once.
    if args.save_snapshot || args.save_manifest {
        let mut new_lock = lock.clone();
        if args.save_snapshot {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|e| format!("system clock is before the Unix epoch: {e}"))?
                .as_secs();
            let ts = boot2deb_engine::snapshot::format_timestamp(now);
            // Captured dormant (mode=off): provenance until a later `--snapshot`
            // activates it, so it never silently changes the hot path.
            new_lock.snapshot = Some(SnapshotPin {
                timestamp: ts.clone(),
                mode: SnapshotMode::Off,
            });
            note(
                json,
                &sink,
                "build",
                format!("saved snapshot: {ts} (mode off — activate with --snapshot fallback|pin)"),
            );
        }
        if args.save_manifest {
            let manifest_path = rootfs_manifest.as_ref().ok_or(
                "--save-manifest needs the rootfs stage — run --stage all or --stage rootfs",
            )?;
            let digest = solved_manifest_digest.as_ref().ok_or(
                "--save-manifest needs the freshly-solved manifest digest — run --stage all or --stage rootfs",
            )?;
            let committed = root.recipe_sibling(recipe, &new_lock.rootfs.manifest)?;
            std::fs::copy(manifest_path, &committed)
                .map_err(|e| format!("commit manifest to {}: {e}", committed.display()))?;
            new_lock.rootfs.manifest_sha256 = Some(digest.clone());
            note(
                json,
                &sink,
                "build",
                format!("saved manifest: {} (sha256 {})", committed.display(), short(digest)),
            );
        }
        let path = root.lock_path(recipe)?;
        pins::write_lock(&path, &new_lock)?;
        note(json, &sink, "build", format!("updated lock  : {}", path.display()));
    }
    Ok(())
}

/// The image's deterministic on-disk identifiers, seeded by the **recipe name**.
///
/// The seed has to be stable across rebuilds (so the image reproduces) and distinct
/// per build point (so two images never claim the same PARTUUID). The recipe name is
/// exactly that, and — unlike the kernel commit — every build has one: a
/// distro-package kernel pins no commit at all, and even where one exists, a kernel
/// bump is no reason for a board's disk identifiers to change.
///
/// Distinctness is not cosmetic here. Under depthcharge the rootfs PARTUUID is baked
/// into the kernel's signed command line, so two recipes that shared one would
/// produce two cards a kernel cannot tell apart — the failure the phase-1 pipeline
/// lived with by hand ("never insert both cards at once") and this removes.
fn image_identity(recipe: &str, build: &ResolvedBuild) -> image::ImageIdentity {
    image::ImageIdentity::derive(recipe, &build.device)
}
