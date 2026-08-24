//! `build`: drive the compile / rootfs / image pipeline from the recipe's lock.
//!
//! Reads only the lock for pinned sources (re-pinning is `update`'s job); the
//! resolved build supplies the axes, and the lock-independent image knobs (layout,
//! size) are the only build-time overrides. Every stage streams the structured event
//! stream — rendered for a human, or as NDJSON under `--json` — and every produced
//! artifact travels on it as an [`Event::Artifact`], so both modes share one stdout
//! contract.

use crate::args::{BuildArgs, StageArg};
use crate::artifacts::{
    kernel_packages, kmod_packages, ledger_debs, record_artifacts, scope_repo_to_current_artifacts,
};
use crate::config::{
    apt_source_keyrings, device_dts_paths, extra_debs_store, fragment_paths, kmod_local_patches,
    overlay_dirs, preflight_config, resolve_patches_source, OverlayStage,
};
use crate::fsutil::absolutize;
use crate::render::{emit_artifact, note, print_event_at, print_event_json, short, Verbosity};
use crate::timing::Timeline;
use crate::workdir::mark_work_dir;
use boot2deb_core::lock::{SnapshotMode, SnapshotPin};
use boot2deb_core::model::{Overrides, ResolvedBoot, ResolvedBuild};
use boot2deb_core::series::Scope;
use boot2deb_core::{resolve_recipe, ConfigRoot};
use boot2deb_engine::build::{ffmpeg, kernel, kmod, uboot, userspace, BuildEnv};
use boot2deb_engine::debstore::DebStore;
use boot2deb_engine::event::{Event, Step};
use boot2deb_engine::image::{self, ImageOutput};
use boot2deb_engine::rootfs;
use boot2deb_engine::sandbox::{BuildSandbox, PackagingSandbox, RootlessSandbox, SandboxRole};
use boot2deb_engine::{extradebs, pins};
use std::path::PathBuf;

/// Run `build <recipe>`.
///
/// `pinned_plan` is [`reproduce`](super::reproduce)'s one addition: a published plan
/// document the rootfs replays instead of resolving. Every other input is the same, so
/// the two commands share one pipeline rather than one of them being a second
/// implementation of it. `None` is an ordinary build.
pub(crate) fn run(
    root: &ConfigRoot,
    recipe: &str,
    args: BuildArgs,
    pinned_plan: Option<&std::path::Path>,
    json: bool,
    verbosity: Verbosity,
) -> Result<(), Box<dyn std::error::Error>> {
    // `build` reads only the lock for pinned sources; the resolved build
    // supplies the axes. Only the lock-independent image knobs (layout, size) are
    // overridable here — the lock pins no image geometry. The source-pinning axes
    // (kernel/suite/features/boot-method) would mismatch the lock, so re-pinning
    // those is `update`'s job, not a build-time override.
    // A `--feature` selection names a *variant* of the recipe, which `update` must
    // already have pinned: `build` reads a lock, it never resolves one. Every derived
    // path — lock, work dir, image identity, provenance — keys off the reference, so a
    // variant build cannot land on the recipe's artifacts.
    let point = crate::config::build_point(recipe, args.features.clone())?;
    // Resolved before any work: a contradictory `--compress` is an argument error,
    // and discovering it after a multi-hour compile would waste the whole build.
    let compress = crate::args::image_compression(&args.compress)?;
    let reference = point.reference();
    let recipe = reference.as_str();
    // Every artifact this build publishes is named for the point rather than the
    // board, so several recipes can share one `--out-dir` without folding each
    // other's rootfs or bootloader into an image.
    let stem = point.artifact_stem();
    let lock = root
        .lock(recipe)
        .map_err(|err| -> Box<dyn std::error::Error> {
            // A variant whose lock was never written is the one likely mistake here, and
            // the generic "lock not found" would leave the operator guessing that `update`
            // takes the same flags. Name the line to run.
            if point.is_variant() && root.lock_path(recipe).is_ok_and(|p| !p.exists()) {
                format!(
                    "no lock for '{recipe}' — this feature selection has not been pinned yet. \
                 Run:\n    boot2deb update {} {}",
                    point.recipe(),
                    point
                        .features()
                        .iter()
                        .map(|f| format!("--feature {f}"))
                        .collect::<Vec<_>>()
                        .join(" ")
                )
                .into()
            } else {
                err.into()
            }
        })?;
    let overrides = Overrides {
        layout: args.layout,
        image_size: args.image_size.clone(),
        ..Overrides::default()
    };
    let resolved = resolve_recipe(root, recipe, &overrides)?;
    // Fail fast if the config drifted since `update`: the lock's resolved-derived axes
    // (kernel id, patch series, suite, extra_debs) must still match a fresh resolve,
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
    if let (false, Some(rootfs)) = (args.save_manifest, &lock.rootfs) {
        if let Some(pinned) = &rootfs.manifest_sha256 {
            let committed = root.recipe_sibling(recipe, &rootfs.manifest)?;
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

    let work_dir = crate::workdir::work_dir_for(root, recipe, args.work_dir);
    // Stamp the scratch tree as boot2deb-owned before anything writes into it:
    // `clean` removes only stamped work dirs.
    mark_work_dir(&work_dir)?;
    let out_dir = absolutize(args.out_dir.unwrap_or_else(|| work_dir.join("artifacts")));
    // The content-addressed caches — provisioner downloads and finished rootfs trees —
    // live under the work dir, so they persist across `--stage` invocations and are
    // shared by every build using this work dir.
    let cache_dir = work_dir.join("cache");
    // Downloaded `.deb`s, shared by the build sandbox's base and the image's rootfs.
    // Both provision the same suite and architecture, so their package sets overlap
    // heavily and each cached entry is content-addressed and digest-verified before
    // reuse — one cache is a smaller download, not a collision.
    let deb_cache = cache_dir.join("provisioner-debs");
    // Sweep stale `.partial` staging temps a hard-killed prior run may have left in the
    // artifact dir before the compile stages publish into it. No-op when the
    // dir does not exist yet.
    boot2deb_engine::gc::sweep_stale_temps(&out_dir);
    let blobs_dir = args.blobs_dir.clone().unwrap_or_else(|| {
        let rel = format!("blobs/{}", resolved.soc.as_str());
        root.find_asset(&rel)
            .unwrap_or_else(|| root.path().join(rel))
    });
    // Which compile nodes this build even has. Both are properties of the resolved
    // build, not of the stage flags: a distro-package kernel is installed from the
    // mirror rather than compiled, and a board whose firmware is its own (depthcharge)
    // builds no bootloader at all. Every stage below is gated on these, so an
    // inapplicable stage is skipped in a full build and named as an error when asked
    // for explicitly.
    let compiles_kernel = resolved.compiles_kernel();
    let builds_uboot = resolved.rkbin_boot().is_some();
    // A `--kmod-src` naming no declared module would be silently ignored — the kmod
    // node looks its overrides up by name and falls back to the locked source, so a
    // mistyped name would fetch from upstream and report nothing. Checked here, before
    // any stage runs, so the answer does not arrive after a kernel compile.
    if let Some((name, _)) = args
        .kmod_srcs
        .iter()
        .find(|(n, _)| !resolved.device_kmods.iter().any(|k| &k.name == n))
    {
        let declared: Vec<&str> = resolved
            .device_kmods
            .iter()
            .map(|k| k.name.as_str())
            .collect();
        return Err(format!(
            "--kmod-src names '{name}', which recipe '{recipe}' does not build. \
             Declared modules: {}",
            if declared.is_empty() {
                "none".to_string()
            } else {
                declared.join(", ")
            }
        )
        .into());
    }

    let kernel_src = match (
        &args.kernel_src,
        resolved.kernel.as_ref().and_then(|k| k.compiled()),
    ) {
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

    let pf = boot2deb_engine::preflight(resolved.arch);
    // Every stage past here assumes Linux, and the answer is already in hand.
    pf.ensure_can_build()?;
    // Every root a build provisions that is not the target's is provisioned for the
    // *host's* Debian architecture, so its commands run natively: the cross root
    // compiles there, and the packaging root archives payloads that may be a hundred
    // megabytes. Refused up front rather than at the first stage that needs one: a host
    // this cannot name has no architecture to provision a root at, and every deliverable
    // both compiles and packages.
    let host_deb_arch = pf
        .host
        .debian_arch()
        .ok_or_else(|| -> Box<dyn std::error::Error> {
            format!(
                "cannot name a Debian architecture for this host ({}) — boot2deb \
                 provisions host-arch roots to compile and archive in, and has no name \
                 to provision one under",
                pf.host.arch
            )
            .into()
        })?;
    // The cross root emits the target's objects → pass CROSS_COMPILE; it *is* the
    // target's architecture → none. The question is about the root the compile happens
    // in rather than about the host's own `cc`, which no stage invokes any more.
    let cross_compile =
        (host_deb_arch != resolved.arch.debian_arch()).then(|| resolved.cross_compile.clone());
    // The Tier-2 artifact store, unless disabled: a durable content-addressed
    // cache of the compile nodes' output `.deb`s under <root>/cache/artifacts, keyed
    // by each node's output signature.
    let artifact_store: Option<PathBuf> =
        (!args.no_artifact_cache).then(|| absolutize(root.path().join("cache").join("artifacts")));
    // The one host binary that still shapes a compiled byte: the `qemu-user` interpreter
    // that executes the *target-arch* sandbox's compiler. It runs on the host's binfmt
    // handler, not inside any root, so it is probed rather than resolved. The compilers
    // themselves are packages of their roots and are identified by the roots' names.
    let toolchain = boot2deb_engine::toolchain::HostToolchain::probe(
        // The interpreter, not the toolchain: an arm64 host building armhf compiles
        // through a cross gcc and then runs the result natively, so folding a
        // qemu-arm-static identity into the sandbox key would name a binary nothing
        // executes.
        pf.interpreter.then(|| resolved.arch.debian_arch()),
    );
    let build_env = BuildEnv {
        // What compiles the kernel, u-boot and the out-of-tree modules: the cross root's
        // own identity, derived from the same function that names its tree. Not a probe
        // of a host binary — there is none to probe, and the root's manifest
        // states its compiler sha256-pinned.
        toolchain_id: boot2deb_engine::build::cross_identity(
            host_deb_arch,
            resolved.arch.debian_arch(),
            &resolved.packaging_suite,
            &mirrors,
        ),
        // What the sandbox's own compiler is: the base it is provisioned as, named by
        // the same function that names its tree, plus the interpreter that runs it.
        // Empty where the build resolves no suite and so stands up no sandbox — the
        // nodes that read this never run on such a build.
        sandbox_id: resolved.suite.as_deref().map_or_else(String::new, |suite| {
            boot2deb_engine::build::sandbox_identity(
                resolved.arch.debian_arch(),
                suite,
                &mirrors,
                &toolchain,
            )
        }),
        // What archives the u-boot and kmod `.deb`s: the packaging root's own identity,
        // derived from the same function that names its tree so the key and the tree
        // cannot disagree.
        packaging_id: boot2deb_engine::build::packaging_identity(
            host_deb_arch,
            &resolved.packaging_suite,
            &mirrors,
        ),
        cross_compile,
        jobs: args.jobs,
    };
    // Started before the first step, so its total covers the work outside every step.
    let timeline = Timeline::new();
    // The one stdout contract for a build: human rendering, or NDJSON under
    // --json — artifact locations travel as Event::Artifact either way.
    // A closure rather than a `fn` pointer: the human renderer has to carry the
    // verbosity, and `--json` deliberately ignores it (the NDJSON stream is the
    // record of the build, and a filtered record is a wrong one).
    //
    // The timeline sees every event on both paths, since what it accumulates is a
    // property of the build rather than of how it is being rendered.
    let sink = |e: Event| {
        timeline.record(&e);
        if json {
            print_event_json(&e)
        } else {
            print_event_at(verbosity, &e)
        }
    };
    note(
        json,
        verbosity,
        &sink,
        "build",
        format!(
            "building {recipe} (arch {}, {} build, work {})",
            resolved.arch,
            if pf.cross_toolchain {
                "cross"
            } else {
                "native"
            },
            work_dir.display()
        ),
    );

    // Debian archive keyring for both bootstraps — the cross sandbox and the rootfs:
    // the explicit flag, else the vendored keyring resolved as a
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
    // Only an image build has a suite to bootstrap a sandbox for; the userspace/ffmpeg
    // stages that use it run only on a media-accel image build. A u-boot-only build
    // resolves no suite and stands up no sandbox.
    let sandbox: Option<Box<dyn BuildSandbox>> = resolved.suite.as_ref().map(|suite| {
        // The mirror list is in the path as well as in the sandbox, because
        // `ensure_ready` reuses an existing tree without re-checking its origin — see
        // `build_sandbox_dir`.
        let rootfs = boot2deb_engine::sandbox::build_sandbox_dir(
            &work_dir,
            SandboxRole::Target,
            resolved.arch.debian_arch(),
            suite,
            &mirrors,
        );
        Box::new(RootlessSandbox::new(
            SandboxRole::Target,
            rootfs,
            // The same directory `doctor`'s overlay check probes, so a build root is
            // established where the host was cleared to establish one.
            boot2deb_engine::sandbox::build_root_uppers(&work_dir),
            suite.clone(),
            resolved.arch.debian_arch().to_string(),
            // The build's own mirror list, not the default: under `--snapshot pin` the
            // compiler that produces the target `.deb`s must come from the same
            // point-in-time archive their runtime does.
            mirrors.clone(),
            keyring.clone(),
            Some(deb_cache.clone()),
        )) as Box<dyn BuildSandbox>
    });

    // The root the kernel, u-boot and kmod stages *compile* in: host-arch, carrying a
    // cross toolchain that emits the target's objects, so the compile runs natively and
    // a multi-minute kernel build never passes through `qemu-user`.
    //
    // Unconditional and lazily bootstrapped, like the packaging root and for the same
    // reasons — a `deliverable = uboot` build compiles without resolving an image suite,
    // so this reads `packaging_suite` too, and a build whose artifacts all restore from
    // the cache never provisions it. It shares that board's tree with its image builds
    // rather than standing up a second one.
    let cross_role = SandboxRole::Cross {
        target: resolved.arch.debian_arch(),
    };
    let cross = RootlessSandbox::new(
        cross_role,
        boot2deb_engine::sandbox::build_sandbox_dir(
            &work_dir,
            cross_role,
            host_deb_arch,
            &resolved.packaging_suite,
            &mirrors,
        ),
        boot2deb_engine::sandbox::build_root_uppers(&work_dir),
        resolved.packaging_suite.clone(),
        host_deb_arch,
        mirrors.clone(),
        keyring.clone(),
        Some(deb_cache.clone()),
    );

    // The root the u-boot and kmod `.deb`s are archived in. Unconditional, unlike the
    // build sandbox above: every deliverable packages something, including a
    // `deliverable = uboot` build that resolves no image suite at all — which is why
    // it reads `packaging_suite` (its own suite where it has one, the device's default
    // otherwise) rather than `suite`. Constructing one costs nothing; the stages that
    // archive call `ensure_ready` and pay for the bootstrap only if they reach it.
    let packaging = PackagingSandbox::new(
        boot2deb_engine::sandbox::packaging_root_dir(
            &work_dir,
            host_deb_arch,
            &resolved.packaging_suite,
            &mirrors,
        ),
        resolved.packaging_suite.clone(),
        host_deb_arch,
        // The same mirror list, for the same reason the build sandbox takes it: under
        // `--snapshot pin` the tool that archives the `.deb`s comes from the same
        // point-in-time archive their contents do.
        mirrors.clone(),
        keyring.clone(),
        Some(deb_cache.clone()),
    );

    // Resolve the patches source only when there is a series to apply: the lock pins
    // one (its kernel names a patch series) *and* this run includes a stage that
    // applies it (kernel/u-boot/userspace/ffmpeg — the userspace stage carries the MPP
    // CMA fix). A rootfs/image-only build, or any build of a no-patch kernel, never
    // reads or fetches the `patches` repo.
    //
    // The source is an explicit --patches-path co-dev checkout, else the default
    // ../patches if present, else an auto-fetch at the pinned commit.
    // The kmod stage carries its own quilt for the *driver* tree, but it builds that
    // module against the *kernel* tree — which `ensure_module_tree` may have to build
    // from source (a stale or absent tree), and that needs the board's kernel patch
    // series like any kernel build. So kmod resolves the kernel patches too.
    let stage_applies_patches = matches!(
        args.stage,
        StageArg::All
            | StageArg::Kernel
            | StageArg::Dtb
            | StageArg::Kmod
            | StageArg::Uboot
            | StageArg::Userspace
            | StageArg::Ffmpeg
    );
    // The kernel and u-boot patch axes read the same `patches` repo checkout (both pins
    // sit at the same commit), so resolve it once from whichever pin this build has.
    let checkout_pin = lock.patches.as_ref().or(lock.uboot_patches.as_ref());
    let checkout = match (checkout_pin, stage_applies_patches) {
        (Some(pin), true) => Some(resolve_patches_source(
            args.patches_path.as_deref(),
            args.patches_url.as_deref(),
            pin,
            root,
            &sink,
        )?),
        _ => None,
    };
    // Only a compiled kernel can name a patch series, so a lock that pins kernel
    // patches pins a kernel too. Check it rather than assume it: the kernel's version is
    // what the series' per-entry ranges are filtered against, and defaulting it would
    // silently select the wrong series.
    if lock.patches.is_some() && lock.kernel.is_none() {
        return Err(format!(
            "the lock for '{recipe}' pins a patch series but no kernel to apply it to \
             — re-run `boot2deb update`"
        )
        .into());
    }
    // Declared-intent prerequisite, before any stage runs. The compile nodes' gates ask
    // the same question, but only once the tree is cloned — a minute of network for an
    // answer already sitting on disk in the series manifests. `update` flags this at
    // pin time; this catches a lock that arrived some other way, or one whose series
    // envelope moved under it since.
    if let (Some((patches_root, _)), Some(pin), Some(kernel)) =
        (&checkout, &lock.patches, &lock.kernel)
    {
        let outside = crate::config::series_outside_envelope(
            patches_root,
            &pin.series,
            Scope::Kernel,
            &kernel.reference,
        )?;
        if let Some((name, declared)) = outside.first() {
            return Err(format!(
                "kernel {} is outside series '{name}' (declared {declared}) — this build would \
                 apply a series that makes no claim about this kernel. Measure it without \
                 re-pinning:\n  boot2deb verify-patches {recipe} --kernel {} \
                 --kernel-path <checkout> --keep-going\nthen widen applies_to_kernel in the \
                 series if it comes back clean, or retire the patches it names.",
                kernel.reference, kernel.reference
            )
            .into());
        }
    }
    // The same prerequisite on the u-boot axis, asked about the u-boot tag: the two
    // axes move independently, so a series' `applies_to_uboot` is a claim about the
    // u-boot version and nothing else. There is no `--kernel`-style candidate path
    // here, so the remedy is to widen the claim or retire the patches.
    if let (Some((patches_root, _)), Some(pin), Some(uboot)) =
        (&checkout, &lock.uboot_patches, &lock.uboot)
    {
        let outside = crate::config::series_outside_envelope(
            patches_root,
            &pin.series,
            Scope::Uboot,
            &uboot.reference,
        )?;
        if let Some((name, declared)) = outside.first() {
            return Err(format!(
                "u-boot {} is outside series '{name}' (declared {declared}) — this build would \
                 apply a series that makes no claim about this u-boot. Widen applies_to_uboot \
                 in the series, or retire the patches it names.",
                uboot.reference
            )
            .into());
        }
    }
    // Bind the resolved checkout to each axis's pin, so no stage can be handed a
    // series without a checkout to read it from (or vice versa). The kernel-scope
    // series is narrowed by the kernel version; the u-boot-scope series by the u-boot
    // version (u-boot is its own axis).
    let kernel_patches = checkout
        .as_ref()
        .zip(lock.patches.as_ref())
        .zip(lock.kernel.as_ref())
        .map(
            |(((root, dev), pin), kernel)| boot2deb_engine::build::PatchSource {
                root,
                pin,
                dev: *dev,
                version: &kernel.reference,
            },
        );
    let uboot_patches = checkout
        .as_ref()
        .zip(lock.uboot_patches.as_ref())
        .zip(lock.uboot.as_ref())
        .map(
            |(((root, dev), pin), uboot)| boot2deb_engine::build::PatchSource {
                root,
                pin,
                dev: *dev,
                version: &uboot.reference,
            },
        );

    // The rootfs tarball the image stage consumes: produced by the rootfs stage,
    // or supplied directly via --rootfs-tar for an image-only build.
    let mut rootfs_tar = args.rootfs_tar.clone();
    // The solved manifest, captured when this run builds the rootfs; joins the
    // image stage's per-image password to emit the provenance manifest at the end.
    let mut rootfs_manifest: Option<PathBuf> = None;
    // The plan document that stage published beside it — the install set plus the
    // archive state it resolved against, which the provenance manifest records and a
    // later `reproduce` replays.
    let mut rootfs_plan: Option<PathBuf> = None;
    // The per-image first-boot password, captured when this run assembles the image
    // (the image stage owns it, splicing it into the staged rootfs).
    let mut first_boot_password: Option<String> = None;
    // Which checks the image stage ran over the finished rootfs filesystem — reported
    // by that stage rather than re-probed here, since one of them depends on a host
    // tool being present. Recorded in the provenance manifest.
    let mut rootfs_verified_with: Vec<String> = Vec::new();
    // The on-disk contract that stage formatted the rootfs to, likewise reported rather
    // than re-derived: its geometry answers to the image's size, so nothing outside the
    // format itself knows it. `None` until the image stage runs — which is also when the
    // provenance manifest becomes writable at all.
    let mut rootfs_filesystem: Option<boot2deb_core::provenance::FilesystemProvenance> = None;
    // The whole-disk size that stage laid out. Reported by it rather than re-parsed from
    // the recipe, because a fitted `image_size` names a rule and not a number — the
    // format decides how large the rootfs is and the disk is sized around it.
    let mut image_bytes: Option<u64> = None;
    // What this run left to write, for the closing hint: the image files themselves,
    // paired with the medium each goes to. Empty for every run that stops short of the
    // image node, which is what makes the hint absent rather than wrong there.
    let mut flashables: Vec<crate::nextstep::Flashable> = Vec::new();
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
            resolved
                .kernel
                .as_ref()
                .map(|k| k.id())
                .unwrap_or("(none — u-boot-only recipe)")
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
    // A u-boot-only recipe (deliverable = uboot) resolves no rootfs or image, so an
    // explicit rootfs/image stage has nothing to build — name it rather than skip.
    if matches!(args.stage, StageArg::Rootfs | StageArg::Image) && !resolved.produces_image() {
        return Err(format!(
            "recipe '{recipe}' builds only a bootloader (deliverable = uboot), so it \
             resolves no rootfs or image — build it with `--stage uboot` (or omit --stage)"
        )
        .into());
    }

    // Kernel-tree inputs, shared by the kernel/dtb stages and the kmod stage (which
    // builds its modules against the same `<work>/linux` tree). Resolved once when the
    // build compiles a kernel; a distro kernel resolves none of this and runs none of
    // these stages. The fragment/dts Vecs are bound first so the borrowed
    // `KernelOptions` outlives both stage blocks.
    let kernel_inputs = if compiles_kernel {
        Some((
            fragment_paths(root, &resolved)?,
            device_dts_paths(root, &resolved)?,
        ))
    } else {
        None
    };
    let kernel_opts = kernel_inputs
        .as_ref()
        .map(|(fragments, device_dts)| kernel::KernelOptions {
            source: &kernel_src,
            patches: kernel_patches,
            fragments,
            device_dts,
            work_dir: &work_dir,
            cross: &cross,
            out_dir: &out_dir,
            store: artifact_store.as_deref(),
        });

    // The kernel stage and the DTB fast path share every filesystem input; both
    // prepare the same `<work>/linux` tree.
    if matches!(args.stage, StageArg::All | StageArg::Kernel | StageArg::Dtb) && compiles_kernel {
        let opts = kernel_opts
            .as_ref()
            .expect("a kernel-compiling build resolved kernel options");
        if matches!(args.stage, StageArg::Dtb) {
            let dtb = kernel::build_dtb(&resolved, &lock, opts, &build_env, &sink)?;
            emit_artifact(&sink, "dtb", "dtb", &dtb);
        } else {
            let artifacts = kernel::build_kernel(&resolved, &lock, opts, &build_env, &sink)?;
            emit_artifact(&sink, "kernel", "image_deb", &artifacts.image_deb);
            emit_artifact(&sink, "kernel", "headers_deb", &artifacts.headers_deb);
            record_artifacts(
                &out_dir,
                &[artifacts.image_deb.clone(), artifacts.headers_deb.clone()],
            )?;
            kernel_image_deb = Some(artifacts.image_deb.clone());
        }
    }

    // The kmod stage builds each board `device_kmods` module out-of-tree against the
    // kernel tree and stages a `<name>-modules-<kver>` `.deb`. It runs only when the
    // build compiles a kernel and the board declares kmods; a distro-kernel board is
    // already rejected for any `device_kmods` at resolve. Its `.deb`s join the ledger,
    // so the rootfs stage installs them from the local repo like the kernel deb.
    let has_kmods = !resolved.device_kmods.is_empty();
    if matches!(args.stage, StageArg::Kmod) && !(compiles_kernel && has_kmods) {
        return Err(format!(
            "recipe '{recipe}' declares no out-of-tree kernel modules — the requested kmod \
             stage has nothing to build"
        )
        .into());
    }
    let mut kmod_debs: Vec<PathBuf> = Vec::new();
    if matches!(args.stage, StageArg::All | StageArg::Kmod) && compiles_kernel && has_kmods {
        let kernel = kernel_opts
            .as_ref()
            .expect("a kernel-compiling build resolved kernel options");
        // The device's boot2deb-side compat patches (e.g. the SDIO-7.1 shim), resolved
        // from config-root-relative to absolute along the config search path.
        let local_patches = kmod_local_patches(root, &resolved)?;
        let opts = kmod::KmodOptions {
            kernel,
            sources: &args.kmod_srcs,
            local_patches: &local_patches,
            work_dir: &work_dir,
            out_dir: &out_dir,
            packaging: &packaging,
            store: artifact_store.as_deref(),
        };
        let artifacts = kmod::build_kmods(&resolved, &lock, &opts, &build_env, &sink)?;
        for deb in &artifacts.debs {
            emit_artifact(&sink, "kmod", "deb", deb);
        }
        record_artifacts(&out_dir, &artifacts.debs)?;
        kmod_debs = artifacts.debs;
    }

    if matches!(args.stage, StageArg::All | StageArg::Uboot) && builds_uboot {
        let opts = uboot::UbootOptions {
            source: &uboot_src,
            patches: uboot_patches,
            blobs_dir: &blobs_dir,
            work_dir: &work_dir,
            cross: &cross,
            out_dir: &out_dir,
            packaging: &packaging,
            stem: &stem,
            store: artifact_store.as_deref(),
        };
        let artifacts = uboot::build_uboot(&resolved, &lock, &opts, &build_env, &sink)?;
        emit_artifact(&sink, "uboot", "idbloader", &artifacts.idbloader);
        emit_artifact(&sink, "uboot", "uboot_itb", &artifacts.uboot_itb);
        emit_artifact(&sink, "uboot", "deb", &artifacts.deb);
        // The maskrom USB boot images, when this board's u-boot builds them: the
        // CODE471/CODE472 payloads for running this u-boot from RAM over USB.
        // pyrographer streams the raw usb471/usb472 pair; `maskrom_loader` is the two
        // packed into the single RKBOOT file `rkdeveloptool db` consumes directly.
        if let Some(m) = &artifacts.maskrom {
            emit_artifact(&sink, "uboot", "usb471", &m.usb471);
            emit_artifact(&sink, "uboot", "usb472", &m.usb472);
            emit_artifact(&sink, "uboot", "maskrom_loader", &m.loader);
        }
        record_artifacts(&out_dir, std::slice::from_ref(&artifacts.deb))?;
        // A uboot-only build also emits a standalone, directly-flashable bootloader
        // image (`<stem>-boot.img`) — the eMMC/SPI medium for a split install
        // whose OS lives on another disk. Emitted for an explicit `--stage uboot`, and
        // for a u-boot-only recipe's full build (which has no image stage to fold u-boot
        // into). An image build's `--stage all` skips it: the image stage folds u-boot
        // into the combined image, or emits `-boot.img` for `split`.
        if matches!(args.stage, StageArg::Uboot) || !resolved.produces_image() {
            let boot_img = image::build_bootloader_image(
                &resolved,
                &stem,
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
        let us = resolved
            .userspace
            .as_ref()
            .expect("media-accel build has userspace sources");
        // A tree the SoC does not declare has no clone source; the userspace stage
        // skips it, so the empty string is never read.
        let src = |flag: &Option<String>, decl: &Option<boot2deb_core::model::GitSource>| {
            flag.clone()
                .or_else(|| decl.as_ref().map(|s| s.git.clone()))
                .unwrap_or_default()
        };
        let mpp_src = src(&args.mpp_src, &us.mpp);
        let librga_src = src(&args.librga_src, &us.librga);
        let libmali_src = src(&args.libmali_src, &us.libmali);
        let opts = userspace::UserspaceOptions {
            mpp_src: &mpp_src,
            librga_src: &librga_src,
            libmali_src: &libmali_src,
            build_libmali: args.build_libmali,
            work_dir: &work_dir,
            out_dir: &out_dir,
            patches: kernel_patches,
            store: artifact_store.as_deref(),
        };
        let artifacts = userspace::build_userspace(
            &lock,
            &opts,
            resolved.arch.debian_arch(),
            &build_env,
            sandbox
                .as_deref()
                .expect("a media-accel build resolves a suite and a sandbox"),
            &sink,
        )?;
        for deb in &artifacts.debs {
            emit_artifact(&sink, "userspace", "deb", deb);
        }
        record_artifacts(&out_dir, &artifacts.debs)?;
    }

    if matches!(args.stage, StageArg::All | StageArg::Ffmpeg) && media_accel {
        let ff = resolved
            .ffmpeg
            .as_ref()
            .expect("media-accel build has ffmpeg sources");
        let ffmpeg_base_src = args
            .ffmpeg_base_src
            .clone()
            .unwrap_or_else(|| ff.base.git.clone());
        // ffmpeg build-depends on the userspace .debs; they are staged in
        // out_dir by the userspace stage (run it first, or with --stage all).
        let opts = ffmpeg::FfmpegOptions {
            base_src: &ffmpeg_base_src,
            patches: kernel_patches,
            userspace_debs: &out_dir,
            // The same flag the userspace stage above ran under, so this stage
            // recomputes the userspace packages' keys for the layer they were actually
            // built in rather than for a default.
            build_libmali: args.build_libmali,
            work_dir: &work_dir,
            out_dir: &out_dir,
            store: artifact_store.as_deref(),
        };
        let artifacts = ffmpeg::build_ffmpeg(
            &lock,
            &opts,
            resolved.arch.debian_arch(),
            &build_env,
            sandbox
                .as_deref()
                .expect("a media-accel build resolves a suite and a sandbox"),
            &sink,
        )?;
        emit_artifact(&sink, "ffmpeg", "deb", &artifacts.deb);
        record_artifacts(&out_dir, std::slice::from_ref(&artifacts.deb))?;
    }

    if matches!(args.stage, StageArg::All | StageArg::Rootfs) && resolved.produces_image() {
        // The rootfs stage runs only for an image build, which pins a rootfs.
        let rootfs_pin = lock.rootfs.as_ref().expect("an image build pins a rootfs");
        // Bootstrap the device rootfs: stand up a local apt repo from the
        // built .debs in out_dir, install the merged package set, apply the layered
        // overlay, and emit the tarball the image stage formats into ext4.
        let preinstall_overlay_dirs = overlay_dirs(root, &resolved, OverlayStage::PreInstall);
        let overlay_dirs = overlay_dirs(root, &resolved, OverlayStage::Customize);
        // The boot-method config the rootfs generates for itself. Only depthcharge has
        // any: its boot payload is a signed kernel built *inside* the rootfs, so the
        // rootfs has to know which board profile to sign for and what cmdline to bake in.
        let boot_config = resolved
            .depthcharge_boot()
            .map(|b| rootfs::BootConfig::Depthcharge {
                board: &b.board,
                cmdline: &b.cmdline,
                initramfs_compress: b.initramfs_compress,
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
        // Scope the local repo to the kernel and modules this build produced. The repo is
        // `--multiversion` and both rootfs backends resolve a bare package name
        // highest-version-wins, so a stale higher-versioned deb an earlier build left in
        // out_dir would outrank the fresh one — and a kernel's `git describe` version
        // *regresses* when patches are dropped, so a newer build can sort below older
        // residue. Dropping the stale versions makes the by-name installs below land on
        // this build's artifacts (and keeps the repo index honest for both backends).
        scope_repo_to_current_artifacts(&mut repo_debs, &kernel_image_deb, &kmod_debs);
        // The kernel image is a build artifact with a version-specific package
        // name, so install it by the name discovered from the built .deb, on top of
        // the resolved set (the static config can't name a version it hasn't built).
        // The out-of-tree modules debs join it — same rationale (their name embeds the
        // kernel release), so they too are installed by discovered name from the ledger.
        let mut extra_packages = kernel_packages(&kernel_image_deb, &repo_debs)?;
        extra_packages.extend(kmod_packages(&kmod_debs, &repo_debs)?);
        // Published under the point's stem, not the lock's `manifest` name: that name
        // is a bare leaf, correct beside the lock in its device folder and ambiguous in
        // a flat output directory two boards' `forky` recipes can share. The committed
        // copy `--save-manifest` writes keeps the lock's name.
        let manifest_out = out_dir.join(format!("{stem}.pkgs.lock"));
        // Resolve each feature apt source's signing keyring to the vendored host
        // path the bootstrap verifies the repo against. Existence was already gated at
        // preflight; this stage-time resolution is the backstop for a keyring
        // removed since.
        let apt_repos = apt_source_keyrings(root, &resolved.apt_sources)?;
        // The image's account of itself, staged into the rootfs at
        // `/etc/boot2deb/image.toml`. Assembled here rather than beside the provenance
        // manifest below because it has to exist *before* the rootfs is bootstrapped —
        // it ships inside the tree the bootstrap produces.
        let system_identity = boot2deb_core::provenance::system_identity(&resolved, &lock);
        // The interpreter that will run the tree's maintainer scripts, from the
        // toolchain probed above — `None` on a native build, where nothing is
        // interpreted. Bound here so the borrow outlives `opts`.
        let interpreter_id = toolchain.qemu_identity();
        let opts = rootfs::RootfsOptions {
            repo_debs: &repo_debs,
            overlay_dirs: &overlay_dirs,
            preinstall_overlay_dirs: &preinstall_overlay_dirs,
            boot_config,
            image_identity: &system_identity,
            rootfs_partuuid: identity.rootfs_partuuid,
            out_dir: &out_dir,
            stem: &stem,
            // The build's own scratch tree: the provisioned userland is multi-GB and
            // carries xattrs, so it must not land on whatever `TMPDIR` names.
            scratch_dir: &work_dir,
            keyring: keyring.as_deref(),
            interpreter_id: interpreter_id.as_deref(),
            manifest_out: &manifest_out,
            pinned_plan,
            mirrors: &mirrors,
            extra_packages: &extra_packages,
            cache_dir: Some(&cache_dir),
            refresh: args.refresh_rootfs,
            apt_sources: &apt_repos,
            // Clamp tarball mtimes to the locked kernel commit's date (the same
            // lock-derived seed the image identifiers use), so only the deliberate
            // per-image password varies between builds of one lock. None
            // on a rootfs-only build with no kernel tree in this work dir.
            source_date_epoch: kernel::source_date_epoch(&work_dir, &lock),
        };
        let artifacts = rootfs::build_rootfs(&resolved, &opts, &sink)?;
        emit_artifact(&sink, "rootfs", "tar", &artifacts.tar);
        emit_artifact(&sink, "rootfs", "manifest", &artifacts.manifest);
        emit_artifact(&sink, "rootfs", "plan", &artifacts.plan);
        // Manifest-as-input verification: unless `--save-manifest` re-pins,
        // a fresh solve must reproduce the committed pin — a drift means the live
        // mirror moved off the pinned package set. Hard error unless the drift is
        // explicitly allowed.
        let solved_digest = boot2deb_engine::manifest::digest(&artifacts.manifest)?;
        if !args.save_manifest {
            if let Some(pinned) = &rootfs_pin.manifest_sha256 {
                match boot2deb_engine::manifest::verify_reproduced(pinned, &solved_digest) {
                    Ok(()) => note(
                        json,
                        verbosity,
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
        rootfs_plan = Some(artifacts.plan);
    }

    if matches!(args.stage, StageArg::All | StageArg::Image) && resolved.produces_image() {
        // The image node consumes the rootfs tarball plus the u-boot raw-gap
        // payloads staged in out_dir by the earlier stages. The rootfs tar comes
        // from the rootfs stage in this run, else --rootfs-tar, else the
        // conventionally-named artifact the rootfs stage leaves in out_dir — the
        // same auto-discovery the u-boot payloads get below.
        let rootfs_tar = rootfs_tar
            .clone()
            .unwrap_or_else(|| out_dir.join(format!("{stem}-rootfs.tar")));
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
        let idbloader = out_dir.join(format!("{stem}-idbloader.img"));
        let uboot_itb = out_dir.join(format!("{stem}-u-boot.itb"));
        // Matched on the resolved boot method, not on a boolean, so adding a third
        // method is a compile error here rather than a silent route into the wrong arm.
        let boot = match &resolved.boot {
            ResolvedBoot::RockchipRkbin(_) => {
                for p in [&idbloader, &uboot_itb] {
                    if !p.exists() {
                        return Err(format!(
                            "{} not found — run `build {recipe} --stage uboot` first",
                            p.display()
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
            stem: &stem,
            work_dir: &work_dir,
            rootfs_label: &args.rootfs_label,
            identity: image_identity(recipe, &resolved),
            compress: &compress,
            keep_raw: args.keep_raw,
            jobs: build_env.jobs,
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
        for c in &artifacts.compressed {
            emit_artifact(&sink, "image", "compressed", &c.path);
        }
        flashables = crate::nextstep::flashables(&artifacts.output, &artifacts.compressed);
        // The per-image first-boot password: unique per build, expired so it
        // must be changed at first login. Surfaced here since it exists nowhere else
        // the operator can read it except the provenance manifest.
        note(
            json,
            verbosity,
            &sink,
            "image",
            format!(
                "first-boot pw: {}  (user {}, expired — change at first login)",
                artifacts.password,
                rootfs::DEFAULT_USER
            ),
        );
        first_boot_password = Some(artifacts.password);
        rootfs_verified_with = artifacts.rootfs_verified_with;
        rootfs_filesystem = Some(artifacts.rootfs_filesystem);
        image_bytes = Some(artifacts.image_bytes);
    }

    // The solved manifest describing the rootfs inside the image this run assembled:
    // from this run's own rootfs stage, else the one the rootfs stage left in `out_dir`
    // beside the tar — the same auto-discovery the tar itself gets, and correct for the
    // same reason, since one rootfs run writes both. An explicit `--rootfs-tar` names a
    // tree from outside this directory, whose manifest is not knowable here.
    let image_manifest: Option<PathBuf> = rootfs_manifest.clone().or_else(|| {
        if args.rootfs_tar.is_some() {
            return None;
        }
        lock.rootfs
            .as_ref()
            .map(|_| out_dir.join(format!("{stem}.pkgs.lock")))
            .filter(|p| p.exists())
    });
    // The plan document beside that manifest, found the same way and for the same
    // reason: one rootfs run writes the tar, the manifest and the plan together, so an
    // image-only re-run over that tar records the plan that produced it rather than
    // dropping the section.
    let image_plan: Option<PathBuf> = rootfs_plan
        .clone()
        .or_else(|| {
            if args.rootfs_tar.is_some() {
                return None;
            }
            Some(out_dir.join(format!("{stem}.plan")))
        })
        .filter(|p| p.exists());

    // The provenance manifest describes the image beside it, so it is emitted by every
    // run that assembles one — including an image-only run, whose freshly generated
    // first-boot password would otherwise leave the previous run's document standing
    // over a different image, misstating the one credential it exists to record.
    // It joins the lock's pins, the resolved build point, the solved-manifest digest,
    // the blob hashes, the toolchain identity, and the first-boot credential into
    // one "exactly what went into this image" document for support/security.
    let prov_path = out_dir.join(format!("{stem}.provenance.toml"));
    if first_boot_password.is_some() && image_manifest.is_none() {
        // An image was built and no manifest describes it. Any document here belongs to
        // an earlier image; removing it beats leaving one that reads as authoritative.
        if prov_path.exists() {
            std::fs::remove_file(&prov_path)
                .map_err(|e| format!("remove stale provenance {}: {e}", prov_path.display()))?;
            note(
                json,
                verbosity,
                &sink,
                "image",
                format!(
                    "removed {} — it described an earlier image, and this run has no \
                     solved manifest to write a new one from",
                    prov_path.display()
                ),
            );
        }
    }
    // All four come from the image stage, so the manifest is writable exactly when that
    // stage ran. Naming them together makes that structural rather than a comment: a
    // build stopping before the image writes no provenance, because the record it would
    // describe does not exist.
    if let (Some(manifest_path), Some(password), Some(filesystem), Some(image_bytes)) = (
        &image_manifest,
        &first_boot_password,
        &rootfs_filesystem,
        image_bytes,
    ) {
        let manifest_bytes = std::fs::read(manifest_path)
            .map_err(|e| format!("read solved manifest {}: {e}", manifest_path.display()))?;
        let manifest_sha256 = boot2deb_engine::blobs::sha256_hex(&manifest_bytes);
        let package_count = String::from_utf8_lossy(&manifest_bytes)
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .count();
        // The plan document beside it, read back off the published file so the digest
        // recorded is of what a reader will open. One rootfs run writes both, so a
        // manifest without a plan means the directory was edited between the two runs —
        // an error rather than a silently thinner record, since the archive state is
        // exactly what this section exists to carry.
        let plan_path = image_plan.as_deref().ok_or_else(|| {
            format!(
                "no plan document beside the solved manifest {} — the rootfs stage \
                 publishes {stem}.plan with it; re-run `--stage rootfs`",
                manifest_path.display()
            )
        })?;
        let plan_record = boot2deb_engine::rootfs::read_plan_record(plan_path)?;
        let plan_name = format!("{stem}.plan");
        // The three provisioned roots that produced this build's `.deb`s — the
        // target-arch base that compiled the media-accel ones, the host-arch cross root
        // that compiled the kernel, u-boot and modules, and the host-arch root whose
        // `dpkg` archived the staged trees. Each keeps its manifest in the work dir
        // beside the tree it describes; publish a copy beside the image so the record
        // travels with what it describes rather than staying behind in a scratch tree
        // `clean` removes.
        //
        // Any can be absent, and each for its own reason: no target-arch sandbox is
        // stood up by a build that compiles no media-accel `.deb`s, no cross root by one
        // that compiles no kernel, u-boot or module, and no packaging root by one whose
        // archived artifacts all came back from the artifact cache. A `None` here is
        // therefore "nothing of this kind was produced", never "not recorded".
        let build_sandbox = match (
            resolved.suite.as_ref(),
            sandbox.as_ref().and_then(|s| s.base_manifest()),
        ) {
            // Paired, not defaulted: the sandbox is bootstrapped *for* the resolved
            // suite, so a base without one is not a state this can reach.
            (Some(suite), Some(base_manifest)) => Some(publish_root_manifest(
                &out_dir,
                &format!("{stem}.sandbox.pkgs"),
                "sandbox-manifest",
                suite,
                resolved.arch.debian_arch(),
                &base_manifest,
                &sink,
            )?),
            _ => None,
        };
        let cross_sandbox = match cross.base_manifest() {
            // Recorded at the *host's* architecture, beside a `[build_sandbox]` recording
            // the target's — which is the whole distinction between the two compile roots
            // and the one a reader most needs the record to make.
            Some(base_manifest) => Some(publish_root_manifest(
                &out_dir,
                &format!("{stem}.cross.pkgs"),
                "cross-manifest",
                &resolved.packaging_suite,
                host_deb_arch,
                &base_manifest,
                &sink,
            )?),
            None => None,
        };
        let packaging_root = match packaging.base_manifest() {
            Some(base_manifest) => Some(publish_root_manifest(
                &out_dir,
                &format!("{stem}.packaging.pkgs"),
                "packaging-manifest",
                &resolved.packaging_suite,
                host_deb_arch,
                &base_manifest,
                &sink,
            )?),
            None => None,
        };
        let facts = boot2deb_core::provenance::BuildFacts {
            cross_sandbox,
            packaging_root,
            host_arch: pf.host.arch,
            cross: pf.cross_toolchain,
            manifest_sha256: &manifest_sha256,
            package_count,
            image_bytes,
            // Named by the leaf the rootfs stage publishes it under, so the manifest
            // refers to a sibling rather than to a path on this machine.
            plan: &plan_name,
            plan_sha256: &plan_record.sha256,
            archives: &plan_record.archives,
            user: rootfs::DEFAULT_USER,
            password,
            // Stamped by build.rs from the boot2deb checkout; the commit is empty when
            // built outside a git tree (e.g. a source tarball), leaving only the version.
            builder_version: env!("CARGO_PKG_VERSION"),
            builder_commit: option_env!("BOOT2DEB_GIT_COMMIT").filter(|s| !s.is_empty()),
            builder_dirty: matches!(option_env!("BOOT2DEB_GIT_DIRTY"), Some("true")),
            // Reported by the image stage that formatted it, so the manifest states the
            // contract the rootfs actually carries and the geometry that actually came
            // out — neither of them a value re-derived here from a declaration.
            filesystem: filesystem.clone(),
            // Reported by the image stage that ran them: the external cross-check is
            // present only where the host carries e2fsprogs, so verification depth is
            // host-determined and belongs in the record.
            rootfs_verified_with: &rootfs_verified_with,
            // The one host binary behind the arch selection above. The compilers are
            // not here: each is a package of a provisioned root, and the root records
            // it sha256-pinned in its own manifest below.
            qemu: toolchain.qemu(),
            jobs: build_env.jobs(),
            // Resolved from the sandbox's own profile, likewise: the environment and
            // mounts every build command ran under, which no source pin covers and
            // which the sandbox library is free to change between releases.
            sandbox: boot2deb_engine::sandbox::resolved_inputs()?,
            // The package set behind that profile: the environment above says what a
            // compile ran in, this says what it compiled against.
            build_sandbox,
        };
        let prov = boot2deb_core::provenance::assemble(&resolved, &lock, &facts);
        // The provenance lands in this recipe's own out_dir, named for the leaf
        // (slash-free), matching the committed manifest's leaf-based filename.
        std::fs::write(&prov_path, prov.to_toml_string()?)
            .map_err(|e| format!("write provenance {}: {e}", prov_path.display()))?;
        emit_artifact(&sink, "image", "provenance", &prov_path);

        // `--sbom`: the bill of materials, rendered from the two documents just
        // written rather than from the values still in memory, so it describes exactly
        // what shipped. Off unless asked for — an image build never silently gains a
        // file — and the same documents are producible later from the manifest above
        // by `boot2deb sbom`, which is what someone handed an image will use.
        for path in
            crate::commands::sbom::write_beside(&prov, &stem, &out_dir, manifest_path, &args.sbom)?
        {
            emit_artifact(&sink, "image", "sbom", &path);
        }
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
                verbosity,
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
            let rootfs_pin = new_lock
                .rootfs
                .as_mut()
                .expect("--save-manifest requires the rootfs stage, which pins a rootfs");
            let committed = root.recipe_sibling(recipe, &rootfs_pin.manifest)?;
            std::fs::copy(manifest_path, &committed)
                .map_err(|e| format!("commit manifest to {}: {e}", committed.display()))?;
            rootfs_pin.manifest_sha256 = Some(digest.clone());
            note(
                json,
                verbosity,
                &sink,
                "build",
                format!(
                    "saved manifest: {} (sha256 {})",
                    committed.display(),
                    short(digest)
                ),
            );
        }
        let path = root.lock_path(recipe)?;
        pins::write_lock(&path, &new_lock)?;
        note(
            json,
            verbosity,
            &sink,
            "build",
            format!("updated lock  : {}", path.display()),
        );
    }

    // Where the time went, what this build point does not do, then what to do with
    // what was built. All three summarize the stream above them, so they come after it
    // rather than in it, and the hint comes last because it is the actionable one.
    // None prints under `--json` (a table is not JSON, and the durations are already
    // on that stream as structured events) or under `--quiet`, which asks for the
    // artifacts alone.
    if !json && verbosity != Verbosity::Quiet {
        timeline.print();
        // The caveats matter most here: this is the only place an operator meets them
        // without going looking, and a freshly built image is exactly when knowing
        // what it will not do is worth something.
        if !resolved.caveats.is_empty() {
            println!("\ncaveats for {recipe}:");
            for c in &resolved.caveats {
                println!("  - {c}");
            }
        }
        let hint = crate::nextstep::hint(&flashables);
        if !hint.is_empty() {
            println!();
            for line in hint {
                println!("{line}");
            }
        }
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

/// Publish a provisioned root's package manifest beside the image and describe it for
/// the provenance document.
///
/// Copies `base_manifest` — which lives in the work dir, beside the tree it describes —
/// into `out_dir` as `name`, emits it as an artifact of kind `kind`, and returns the
/// record naming it, its sha256 and its package count.
///
/// One implementation for both roots, because a divergence would show up as two
/// sections of one document disagreeing about what a `[…_sandbox]` block means, which
/// is the kind of drift a reader has no way to detect.
fn publish_root_manifest(
    out_dir: &std::path::Path,
    name: &str,
    kind: &str,
    suite: &str,
    architecture: &str,
    base_manifest: &std::path::Path,
    sink: &dyn boot2deb_engine::event::EventSink,
) -> Result<boot2deb_core::provenance::ProvisionedRootProvenance, Box<dyn std::error::Error>> {
    let published = out_dir.join(name);
    std::fs::copy(base_manifest, &published).map_err(|e| {
        format!(
            "publish the base manifest {} to {}: {e}",
            base_manifest.display(),
            published.display()
        )
    })?;
    emit_artifact(sink, "image", kind, &published);
    // Re-read rather than reuse the source bytes: the digest must describe the file a
    // reader of the provenance will open, not the one it was copied from.
    let bytes =
        std::fs::read(&published).map_err(|e| format!("read {}: {e}", published.display()))?;
    Ok(boot2deb_core::provenance::ProvisionedRootProvenance {
        suite: suite.to_string(),
        architecture: architecture.to_string(),
        manifest: name.to_string(),
        manifest_sha256: boot2deb_engine::blobs::sha256_hex(&bytes),
        // Comments and blank lines are the manifest's own framing, not packages.
        package_count: String::from_utf8_lossy(&bytes)
            .lines()
            .filter(|l| !l.starts_with('#') && !l.trim().is_empty())
            .count(),
    })
}
