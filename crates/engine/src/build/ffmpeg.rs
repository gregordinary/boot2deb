//! ffmpeg-rk compile stage: assemble the hybrid FFmpeg — mainline
//! V4L2-request stateless *decode* from the Kwiboo base, Rockchip rkmpp *encode*
//! and rkrga *scale* grafted on from nyanmisaka — and package it as the
//! `ffmpeg-rk` `.deb`.
//!
//! The graft is the profile's ffmpeg `git am` series: the nyanmisaka
//! encode/scale commits, resolved and materialized as patches (one — the RKMPP
//! hwcontext — needs a 3-way conflict resolution a plain cherry-pick cannot
//! reproduce), followed by the NV15 scale_rkrga fix. A patch that will not apply
//! is a hard error naming it — the "never silently skip" contract of the verify
//! gate. The build fetches only the base tree; the series carries the graft.
//! Then `./configure` + `make` + `make install` run inside a target-arch
//! [`BuildSandbox`], and the staged install tree is wrapped into a self-contained
//! `.deb` installing to `/opt/ffmpeg-rk` so it coexists with any system FFmpeg.
//!
//! ffmpeg build-depends on the userspace `-dev` packages (`librockchip-mpp-dev` +
//! `librga-dev`) and runtime-depends on `librockchip-mpp1` + `librga2`, so
//! the stage installs the userspace `.deb`s (from a prior [`userspace`] run) into
//! the sandbox before building.
//!
//! [`userspace`]: crate::build::userspace

use crate::build::{
    self, deb_names, pick_deb, stage_artifact, BuildEnv, ClonePinned, CloneMode, PatchScope,
    PatchSeries, PatchSource,
};
use crate::error::EngineError;
use crate::event::{EventSink, Step};
use crate::sandbox::{BuildSandbox, SandboxRun};
use crate::git;
use boot2deb_core::lock::{FfmpegPins, Lock, UserspacePins};
use std::path::{Path, PathBuf};

/// Install prefix baked into the build; keeps `ffmpeg-rk` out of the system
/// FFmpeg's paths so both can coexist (Jellyfin points at `/opt/ffmpeg-rk/bin/ffmpeg`).
const INSTALL_PREFIX: &str = "/opt/ffmpeg-rk";

/// Stage-recipe version for the ffmpeg tree signature: bump when the
/// fetch/patch logic that shapes the reused tree changes.
const CLONE_STAGE_VERSION: u32 = 1;

/// Stage-recipe version for the ffmpeg **output** signature (Tier-2 artifact cache):
/// bump when the configure/compile/package logic changes the produced `.deb`
/// in a way the folded inputs do not already capture.
const OUTPUT_STAGE_VERSION: u32 = 1;

/// Debian package name.
const PKG_NAME: &str = "ffmpeg-rk";

/// ffmpeg build-deps installed into the sandbox. The base tooling
/// (`build-essential`, `pkg-config`) is already in the sandbox base set; these are
/// the codec/format libraries `./configure` probes. `librockchip-mpp-dev` /
/// `librga-dev` are *not* here — they come from the userspace `.deb`s
/// ([`install_local_debs`](crate::sandbox::BuildSandbox::install_local_debs)).
const FFMPEG_DEPS: &[&str] = &[
    "nasm",
    "yasm",
    "libdrm-dev",
    "libudev-dev",
    "libass-dev",
    "libx264-dev",
    "libx265-dev",
    "libfdk-aac-dev",
    "libssl-dev",
    "libfreetype-dev",
];

/// The userspace `.deb` name prefixes ffmpeg build-depends on, in install order —
/// the runtime libs first, then their `-dev`s. Selected (highest version each)
/// from the userspace stage's output dir and installed into the sandbox.
const USERSPACE_DEP_PREFIXES: &[&str] = &[
    "librockchip-mpp1_",
    "librockchip-mpp-dev_",
    "librga2_",
    "librga-dev_",
];

/// `./configure` feature flags (the `--prefix` is added separately). Single source
/// of truth for the hybrid pipeline: V4L2-request decode + rkmpp/rkrga + the codec
/// set the RK1 media stack needs.
const CONFIGURE_FLAGS: &[&str] = &[
    "--enable-gpl",
    "--enable-version3",
    "--enable-nonfree",
    "--enable-shared",
    "--disable-static",
    "--enable-libdrm",
    "--enable-libudev",
    "--enable-v4l2-request",
    "--enable-rkmpp",
    "--enable-rkrga",
    "--enable-libx264",
    "--enable-libx265",
    "--enable-libfdk-aac",
    "--enable-libass",
    "--enable-libfreetype",
    "--enable-openssl",
];


/// Filesystem inputs for the ffmpeg stage.
pub struct FfmpegOptions<'a> {
    /// Kwiboo base clone source (git URL or local path). A local checkout of the
    /// FFmpeg tree makes the fetch near-instant.
    pub base_src: &'a str,
    /// The `ffmpeg` patch scope's checkout + pin — the materialized graft series plus
    /// the NV15 fix. `None` when the resolved kernel names no patch profile.
    pub patches: Option<PatchSource<'a>>,
    /// Directory holding the userspace `.deb`s ffmpeg build-depends on — the output
    /// dir of a prior [`userspace`](crate::build::userspace) run.
    pub userspace_debs: &'a Path,
    /// Scratch directory; the ffmpeg tree, `pkg-stage`, and the built `.deb` live
    /// under `<work>/ffmpeg/`.
    pub work_dir: &'a Path,
    /// Directory the produced `.deb` is staged into.
    pub out_dir: &'a Path,
    /// Root of the Tier-2 artifact store ([`crate::artstore`]), or `None` to
    /// disable output caching. On a hit the `ffmpeg-rk` deb is restored; on a miss it
    /// is stored after the build.
    pub store: Option<&'a Path>,
}

/// The `ffmpeg-rk` `.deb` produced by [`build_ffmpeg`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FfmpegArtifacts {
    /// The staged `ffmpeg-rk_<version>_<arch>.deb`.
    pub deb: PathBuf,
}

/// Run the ffmpeg stage, emitting its [`Event`](crate::event::Event)s to `sink`.
///
/// Reads only the [`Lock`] for pins: the base commit and the patch profile.
/// `arch` is the Debian architecture for the control file and deb name (e.g.
/// `arm64`). The `sandbox` supplies the target-arch build environment. A tree
/// already present at `<work>/ffmpeg/build` is reused (already patched) and only
/// rebuilt.
pub fn build_ffmpeg(
    lock: &Lock,
    opts: &FfmpegOptions,
    arch: &str,
    env: &BuildEnv,
    sandbox: &dyn BuildSandbox,
    sink: &dyn EventSink,
) -> Result<FfmpegArtifacts, EngineError> {
    let step = Step::start(sink, "ffmpeg");
    let stage_root = opts.work_dir.join("ffmpeg");
    let tree = stage_root.join("build");

    // ffmpeg build-depends on the userspace debs, so a media-accel build always
    // carries both pin sets; the CLI schedules this stage only then. Reaching it
    // without pins is an internal scheduling bug.
    let ffmpeg = lock
        .ffmpeg
        .as_ref()
        .ok_or(EngineError::MissingMediaAccelPins { stage: "ffmpeg" })?;
    let userspace = lock
        .userspace
        .as_ref()
        .ok_or(EngineError::MissingMediaAccelPins { stage: "ffmpeg" })?;

    // The applied patch series identities for the signatures: ffmpeg's own tree
    // series, plus — for the userspace-dependency fold — the `userspace` scope
    // (the MPP CMA fix). Pinned by commit, or fingerprinted from the live checkout in
    // co-dev mode so an edited patch restamps the tree/deb. The `*_fp` locals
    // outlive the borrowed `PatchSeries`.
    let ffmpeg_fp = build::dev_series_fingerprint(opts.patches, PatchScope::Ffmpeg);
    let us_fp = build::dev_series_fingerprint(opts.patches, PatchScope::Userspace);
    let (ffmpeg_patches, us_patches) = (
        build::series_identity(opts.patches, &ffmpeg_fp),
        build::series_identity(opts.patches, &us_fp),
    );

    // Tier-2 output cache: restore the `ffmpeg-rk` deb and skip the whole
    // fetch/patch/configure/compile — the largest single-node payoff (the ~70 min
    // qemu build). Checked before the userspace-deb dependency: a cached ffmpeg
    // needs neither the sandbox nor the userspace `.deb`s (the userspace dep identity
    // in the key is recomputed from the lock, not read from the built debs).
    let out_man = output_manifest(lock, ffmpeg, userspace, arch, ffmpeg_patches, us_patches);
    if let Some([deb]) = build::restore_stage_outputs(
        opts.store,
        "ffmpeg",
        &out_man.signature(),
        opts.out_dir,
        &["deb"],
        &step,
    )?
    .as_deref()
    {
        step.progress(100);
        step.finish();
        return Ok(FfmpegArtifacts { deb: deb.clone() });
    }

    // Fail fast on the build-time dependency (the userspace `.deb`s) before the
    // expensive source fetch, so a forgotten userspace stage errors immediately.
    let debs = required_userspace_debs(opts.userspace_debs)?;

    // Tier-1 reuse of the fetched+patched tree: a lock bump (base commit
    // or patch pin) rebuilds it; configure/compile re-run regardless.
    let man = clone_manifest(ffmpeg, lock.patches.as_ref(), ffmpeg_patches);
    build::reuse_or_refresh_tree(&tree, &man, "ffmpeg", &step, || {
        fetch_and_patch(ffmpeg, opts, &tree, &step)
    })?;
    step.progress(30);

    step.log(format!("sandbox: {}", sandbox.describe()));
    sandbox.ensure_ready(&step)?;
    sandbox.install(FFMPEG_DEPS, &step)?;
    sandbox.install_local_debs(&debs, &step)?;
    step.progress(45);

    // Bind the ffmpeg stage root so the build tree, pkg-stage, and produced .deb
    // are all visible inside the sandbox at their host paths.
    let binds = [stage_root.clone()];
    let pkg_stage = stage_root.join("pkg-stage");
    // A stale pkg-stage from an interrupted run would poison `make install`.
    let _ = std::fs::remove_dir_all(&pkg_stage);

    // Deterministic build timestamp from the locked base commit; the
    // tree's HEAD is a `git am` patch commit stamped now, so read the base explicitly.
    let build_env: Vec<(String, String)> = git::commit_epoch(&tree, &ffmpeg.base.commit)
        .ok()
        .map(|e| vec![("SOURCE_DATE_EPOCH".to_string(), e.to_string())])
        .unwrap_or_default();

    configure(sandbox, &tree, &binds, &build_env, &step)?;
    step.progress(55);
    compile(sandbox, env, &tree, &binds, &build_env, &step)?;
    step.progress(85);
    install_to_stage(sandbox, &tree, &pkg_stage, &binds, &step)?;
    step.progress(88);

    // Derive the runtime Depends from what the built binaries actually link
    // (`dpkg-shlibdeps`), rather than a hand-maintained soname list — so the deb
    // tracks whatever library versions the target suite currently ships.
    let depends = resolve_depends(sandbox, &stage_root, &pkg_stage, arch, &binds, &step)?;
    step.progress(90);

    let version = deb_version(&ffmpeg.base.reference, &ffmpeg.base.commit);
    let control = control_text(arch, &version, &depends);
    write_control(&pkg_stage, &control)?;
    let deb_name = format!("{PKG_NAME}_{version}_{arch}.deb");
    let deb_in_stage = stage_root.join(&deb_name);
    package_deb(sandbox, &pkg_stage, &deb_in_stage, &build_env, &binds, &step)?;

    let deb = stage_artifact(opts.out_dir, &deb_in_stage)?;
    step.log(format!("staged {deb_name}"));

    // Store the deb under the output signature.
    build::store_stage_outputs(
        opts.store,
        "ffmpeg",
        &out_man.signature(),
        &[("deb", deb.as_path())],
        &step,
    )?;
    step.progress(100);
    step.finish();
    Ok(FfmpegArtifacts { deb })
}

/// The Tier-2 output signature manifest of the `ffmpeg-rk` deb. It folds the
/// Tier-1 tree signature ([`clone_manifest`]) as a dependency (base commit + patch
/// series), then the inputs the sandbox build adds: the `./configure` feature flags
/// (order-sensitive), the target arch, the base ref (which becomes the deb version),
/// and the **suite**. Unlike the host-cross kernel/u-boot nodes, ffmpeg compiles
/// inside the target-arch sandbox, whose toolchain is the suite's `gcc`; the suite
/// stands in for that toolchain identity, and the runtime `Depends` `dpkg-shlibdeps`
/// resolves against the suite's libraries. The residual within-suite `gcc`
/// point-release drift is bounded and these accel debs are not byte-gated, so a hit
/// restores a functionally-equivalent deb; `--no-artifact-cache` forces a rebuild.
///
/// It also folds the Tier-2 output signatures of the **MPP** and **RGA** userspace
/// packages ffmpeg build-depends on (`--enable-rkmpp`/`--enable-rkrga`), recomputed
/// from the lock: the built ffmpeg deb links against those `.deb`s, so a
/// change to a userspace pin, patch series, suite, or arch must invalidate the cached
/// ffmpeg deb rather than restore one built against stale userspace libraries. Only
/// MPP carries the `userspace` patch scope, so its dep folds `us_patches`
/// while RGA is unpatched. Folding the lock-derived dep *signatures* (not the built
/// deb bytes) keeps the key computable without the userspace `.deb`s present.
fn output_manifest(
    lock: &Lock,
    ffmpeg: &FfmpegPins,
    userspace: &UserspacePins,
    arch: &str,
    patches: PatchSeries,
    us_patches: PatchSeries,
) -> crate::signature::SignatureManifest {
    let tree_sig = clone_manifest(ffmpeg, lock.patches.as_ref(), patches).signature();
    let mpp_inputs = crate::build::userspace::PatchInputs {
        pin: lock.patches.as_ref(),
        patches: us_patches,
    };
    let mpp_dep = crate::build::userspace::output_manifest_for(
        "mpp",
        &userspace.mpp.commit,
        &lock.rootfs.suite,
        arch,
        Some(&mpp_inputs),
    )
    .signature();
    let rga_dep = crate::build::userspace::output_manifest_for(
        "librga",
        &userspace.librga.commit,
        &lock.rootfs.suite,
        arch,
        None,
    )
    .signature();
    let mut b = crate::signature::SignatureBuilder::new("ffmpeg:out", OUTPUT_STAGE_VERSION);
    b.fold_dep(&tree_sig)
        .fold_ordered("configure_flags", CONFIGURE_FLAGS)
        .fold_scalar("arch", arch)
        .fold_scalar("suite", &lock.rootfs.suite)
        .fold_scalar("base.reference", &ffmpeg.base.reference)
        .fold_scalar("pkg_name", PKG_NAME)
        .fold_dep(&mpp_dep)
        .fold_dep(&rga_dep);
    b.manifest()
}

/// The Tier-1 signature manifest of the fetched+patched ffmpeg tree: the
/// base commit and the patch series (`build::fold_patch_series`) that together
/// determine the tree. The source URL is excluded (the commit content-addresses the
/// base). The [`PatchSeries`] fold covers the pinned patch commit and — in co-dev
/// mode — the live-series fingerprint, so a co-dev build never shares a
/// stamp with a pinned one and an edited patch restamps. Public so `why-rebuild`
/// ([`crate::plan`]) recomputes the same signature it stamps here. Takes the
/// [`FfmpegPins`] and the patch profile/commit directly rather than the whole
/// [`Lock`], since it is only meaningful for a media-accel build (one that has
/// ffmpeg pins).
pub fn clone_manifest(
    ffmpeg: &FfmpegPins,
    pin: Option<&boot2deb_core::lock::PatchesPin>,
    patches: PatchSeries,
) -> crate::signature::SignatureManifest {
    let mut b = crate::signature::SignatureBuilder::new("ffmpeg", CLONE_STAGE_VERSION);
    b.fold_scalar("ffmpeg.base.commit", &ffmpeg.base.commit);
    build::fold_patch_series(&mut b, pin, patches);
    b.manifest()
}

/// Fetch the base at its locked commit and `git am` the profile's ffmpeg series —
/// the materialized nyanmisaka graft plus the NV15 fix — leaving the tree at
/// the fully-assembled source the build compiles.
///
/// On any failure the partial tree is removed, so a re-run after a failed `git am`
/// starts clean rather than silently reusing a half-applied series (the reuse
/// check in [`build_ffmpeg`] only ever sees a completed tree). The graft rides in
/// the profile's ffmpeg scope; no kernel-range gate here — that guards the kernel
/// node, and the profile is already validated there.
fn fetch_and_patch(
    ffmpeg: &FfmpegPins,
    opts: &FfmpegOptions,
    tree: &Path,
    step: &Step,
) -> Result<(), EngineError> {
    let target = format!("ffmpeg-rk @ {}", ffmpeg.base.reference);
    let spec = ClonePinned {
        source: opts.base_src,
        reference: &ffmpeg.base.reference,
        commit: &ffmpeg.base.commit,
        mode: CloneMode::Fetch,
        tree,
        what: "ffmpeg base",
        patches: opts.patches,
        scope: PatchScope::Ffmpeg,
        target: &target,
        gate_reference: None,
    };
    let n = build::clone_pinned(&spec, step)?;
    if let Some(p) = opts.patches {
        step.log(format!("applied {n} ffmpeg patch(es) ({})", p.pin.profile));
    }
    Ok(())
}

/// Run `configure` with the resolved flags inside the sandbox.
///
/// The program is the tree's absolute `configure` path (the tree is bound at the
/// same path in the cross sandbox), not a relative `./configure`: a relative
/// program path is resolved against the *parent* process's cwd, not the run's
/// working dir, so it would misfire on the native path.
fn configure(
    sandbox: &dyn BuildSandbox,
    tree: &Path,
    binds: &[PathBuf],
    env: &[(String, String)],
    step: &Step,
) -> Result<(), EngineError> {
    let mut argv = vec![
        tree.join("configure").to_string_lossy().into_owned(),
        format!("--prefix={INSTALL_PREFIX}"),
    ];
    argv.extend(CONFIGURE_FLAGS.iter().map(|s| s.to_string()));
    let spec = SandboxRun {
        work: tree,
        binds,
        ro_binds: &[],
        net: false,
        env,
        argv: &argv,
        context: "ffmpeg ./configure",
    };
    sandbox.run(&spec, step)
}

/// Run `make -j` inside the sandbox. The build is target-native there (arm64 in
/// the cross sandbox via qemu-user), so no `CROSS_COMPILE` — unlike the
/// host-cross-compiled kernel/u-boot nodes.
fn compile(
    sandbox: &dyn BuildSandbox,
    env: &BuildEnv,
    tree: &Path,
    binds: &[PathBuf],
    run_env: &[(String, String)],
    step: &Step,
) -> Result<(), EngineError> {
    let argv = vec!["make".to_string(), format!("-j{}", env.jobs())];
    let spec = SandboxRun {
        work: tree,
        binds,
        ro_binds: &[],
        net: false,
        env: run_env,
        argv: &argv,
        context: "ffmpeg make",
    };
    sandbox.run(&spec, step)
}

/// Run `make install DESTDIR=<pkg_stage>` inside the sandbox, staging the install
/// tree under the prefix for packaging.
fn install_to_stage(
    sandbox: &dyn BuildSandbox,
    tree: &Path,
    pkg_stage: &Path,
    binds: &[PathBuf],
    step: &Step,
) -> Result<(), EngineError> {
    let argv = vec![
        "make".to_string(),
        "install".to_string(),
        format!("DESTDIR={}", pkg_stage.display()),
    ];
    let spec = SandboxRun {
        work: tree,
        binds,
        ro_binds: &[],
        net: false,
        env: &[],
        argv: &argv,
        context: "ffmpeg make install",
    };
    sandbox.run(&spec, step)
}

/// Build the `.deb` from the staged install tree with `fakeroot dpkg-deb`, run in
/// the sandbox so the packaged file ownership is correct on either path.
///
/// `build_env` carries `SOURCE_DATE_EPOCH` (the locked base commit's committer
/// date), so `dpkg-deb` clamps every archive member's mtime to it — the `.deb`
/// is byte-reproducible rather than stamped with the build clock.
fn package_deb(
    sandbox: &dyn BuildSandbox,
    pkg_stage: &Path,
    deb_out: &Path,
    build_env: &[(String, String)],
    binds: &[PathBuf],
    step: &Step,
) -> Result<(), EngineError> {
    let argv = vec![
        "fakeroot".to_string(),
        "dpkg-deb".to_string(),
        "--build".to_string(),
        pkg_stage.to_string_lossy().into_owned(),
        deb_out.to_string_lossy().into_owned(),
    ];
    let spec = SandboxRun {
        work: pkg_stage,
        binds,
        ro_binds: &[],
        net: false,
        env: build_env,
        argv: &argv,
        context: "dpkg-deb --build ffmpeg-rk",
    };
    sandbox.run(&spec, step)
}

/// Select the userspace `.deb`s ffmpeg build-depends on (highest version each)
/// from `dir`, in install order, erroring if the dir or any package is absent —
/// which means the userspace stage was not run first.
fn required_userspace_debs(dir: &Path) -> Result<Vec<PathBuf>, EngineError> {
    if !dir.exists() {
        return Err(EngineError::ArtifactMissing {
            what: "userspace .debs (run the userspace stage first)".into(),
            location: dir.display().to_string(),
        });
    }
    let names = deb_names(dir)?;
    let mut debs = Vec::new();
    for prefix in USERSPACE_DEP_PREFIXES {
        match pick_deb(&names, prefix) {
            Some(name) => debs.push(dir.join(name)),
            None => {
                return Err(EngineError::ArtifactMissing {
                    what: format!("userspace dependency {prefix}*.deb (run the userspace stage first)"),
                    location: dir.display().to_string(),
                })
            }
        }
    }
    Ok(debs)
}

/// Compute the runtime `Depends:` from the built binaries with `dpkg-shlibdeps`,
/// run inside the sandbox so it reads the target-arch ELFs against the target's
/// dpkg/shlibs data.
///
/// `dpkg-shlibdeps` scans the installed executables and private shared libraries,
/// maps each `NEEDED` soname to the package + minimum version providing it (system
/// libs from the suite, plus our own `librockchip-mpp1`/`librga2` via their
/// `shlibs`). The bundled `libav*`/`libsw*` under the install prefix belong to no
/// package, so a generated `debian/shlibs.local` declares their sonames as
/// internally satisfied (empty dependency) — otherwise `dpkg-shlibdeps` errors on
/// them. It needs a minimal `debian/control` in its working dir and writes the
/// result to `debian/substvars`, read back from the host (the stage root is bound
/// into the sandbox at its host path). A *system* soname with no provider stays a
/// hard error (no `--ignore-missing-info`): a missing dep must fail loud, not ship
/// broken.
fn resolve_depends(
    sandbox: &dyn BuildSandbox,
    stage_root: &Path,
    pkg_stage: &Path,
    arch: &str,
    binds: &[PathBuf],
    step: &Step,
) -> Result<String, EngineError> {
    let work = stage_root.join("shlibdeps");
    let _ = std::fs::remove_dir_all(&work);
    let debian = work.join("debian");
    std::fs::create_dir_all(&debian).map_err(|s| EngineError::io(&debian, s))?;
    // Minimal source stanza — dpkg-shlibdeps reads the package name and arch from
    // it, and the empty substvars gives it a file to write the result into.
    let control = format!("Source: {PKG_NAME}\n\nPackage: {PKG_NAME}\nArchitecture: {arch}\n");
    std::fs::write(debian.join("control"), control)
        .map_err(|s| EngineError::io(&debian.join("control"), s))?;
    let substvars = debian.join("substvars");
    std::fs::write(&substvars, "").map_err(|s| EngineError::io(&substvars, s))?;

    let lib_dir = pkg_stage.join(&INSTALL_PREFIX[1..]).join("lib");
    // Declare the bundled private sonames as internally satisfied so dpkg-shlibdeps
    // emits no dependency on them (and does not error for want of a provider).
    let shlibs_local = private_shlibs_local(&lib_dir)?;
    std::fs::write(debian.join("shlibs.local"), &shlibs_local)
        .map_err(|s| EngineError::io(&debian.join("shlibs.local"), s))?;

    let bins = scan_binaries(pkg_stage)?;
    if bins.is_empty() {
        return Err(EngineError::ArtifactMissing {
            what: "ffmpeg binaries to scan for dependencies".into(),
            location: pkg_stage.display().to_string(),
        });
    }
    let mut argv = vec![
        "dpkg-shlibdeps".to_string(),
        format!("-l{}", lib_dir.display()),
    ];
    argv.extend(bins.iter().map(|p| p.to_string_lossy().into_owned()));
    let spec = SandboxRun {
        work: &work,
        binds,
        ro_binds: &[],
        net: false,
        env: &[],
        argv: &argv,
        context: "dpkg-shlibdeps ffmpeg-rk",
    };
    sandbox.run(&spec, step)?;

    let vars = std::fs::read_to_string(&substvars).map_err(|s| EngineError::io(&substvars, s))?;
    let depends = parse_shlibs_depends(&vars).ok_or_else(|| EngineError::ArtifactMissing {
        what: "shlibs:Depends from dpkg-shlibdeps".into(),
        location: substvars.display().to_string(),
    })?;
    let _ = std::fs::remove_dir_all(&work);
    step.log(format!("resolved runtime Depends: {depends}"));
    Ok(depends)
}

/// The executables and private shared libraries under the install prefix that
/// `dpkg-shlibdeps` scans: everything in `bin/` plus the versioned `.so.*` files
/// in `lib/` (the unversioned `.so` symlinks and `pkgconfig/` are skipped). Sorted
/// for a deterministic argv.
fn scan_binaries(pkg_stage: &Path) -> Result<Vec<PathBuf>, EngineError> {
    let prefix = pkg_stage.join(&INSTALL_PREFIX[1..]);
    let mut out = Vec::new();
    for e in read_dir_entries(&prefix.join("bin"))? {
        let p = e.path();
        if p.is_file() {
            out.push(p);
        }
    }
    for e in read_dir_entries(&prefix.join("lib"))? {
        let p = e.path();
        // Versioned shared objects — the `.so.` infix matches both the real
        // `libfoo.so.N.M.P` and the `libfoo.so.N` SONAME symlink (which
        // `is_file()` follows, so it is included too; harmless for shlibdeps).
        // The unversioned `.so` dev symlink has no `.so.` infix and is excluded.
        let name = e.file_name();
        let name = name.to_string_lossy();
        if p.is_file() && name.contains(".so.") {
            out.push(p);
        }
    }
    out.sort();
    Ok(out)
}

/// `read_dir` that treats an *absent* directory as empty but surfaces an
/// unreadable one: an I/O or permissions failure here would otherwise silently
/// shrink the `dpkg-shlibdeps` input set and ship an incomplete runtime
/// `Depends`.
fn read_dir_entries(dir: &Path) -> Result<Vec<std::fs::DirEntry>, EngineError> {
    match std::fs::read_dir(dir) {
        Ok(entries) => entries.map(|e| e.map_err(|s| EngineError::io(dir, s))).collect(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(e) => Err(EngineError::io(dir, e)),
    }
}

/// A `debian/shlibs.local` marking every private soname under `lib_dir` as
/// internally satisfied (empty dependency), so `dpkg-shlibdeps` neither errors on
/// nor emits a dependency for the bundled `libav*`/`libsw*`. Each line is
/// `<libname> <soversion> ` (trailing space = empty dependency) per the deb
/// `shlibs` format, derived from the SONAME symlinks (`libfoo.so.N`, not the real
/// `libfoo.so.N.M.P`). Sorted + deduped for determinism.
fn private_shlibs_local(lib_dir: &Path) -> Result<String, EngineError> {
    let mut lines: Vec<String> = Vec::new();
    for e in read_dir_entries(lib_dir)? {
        if let Some((lib, ver)) = soname_entry(&e.file_name().to_string_lossy()) {
            lines.push(format!("{lib} {ver} \n"));
        }
    }
    lines.sort();
    lines.dedup();
    Ok(lines.concat())
}

/// Parse a SONAME-symlink filename into its shlibs `(libname, soversion)` — e.g.
/// `libavutil.so.60` → `("libavutil", "60")`. Returns `None` for real versioned
/// files (`libfoo.so.N.M.P`), the `.so` dev symlink, and non-libraries. Pure, so
/// the mapping is testable.
fn soname_entry(name: &str) -> Option<(&str, &str)> {
    let (lib, ver) = name.split_once(".so.")?;
    (lib.starts_with("lib") && !ver.is_empty() && ver.bytes().all(|b| b.is_ascii_digit()))
        .then_some((lib, ver))
}

/// Extract the `shlibs:Depends=` value from a `dpkg-shlibdeps` substvars file.
/// Pure, so the parse is testable. Returns `None` if the variable is absent.
fn parse_shlibs_depends(substvars: &str) -> Option<String> {
    substvars.lines().find_map(|l| {
        l.strip_prefix("shlibs:Depends=")
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .map(str::to_string)
    })
}

/// Write the `DEBIAN/control` file into the staged package tree.
///
/// The dir and file are mode-normalized (0755/0644) so the host umask does not leak
/// into the packaged control metadata. Only the metadata this code writes is
/// normalized — the `make install` payload carries its own explicit install modes.
fn write_control(pkg_stage: &Path, control: &str) -> Result<(), EngineError> {
    let debian = pkg_stage.join("DEBIAN");
    std::fs::create_dir_all(&debian).map_err(|source| EngineError::io(&debian, source))?;
    build::set_mode(&debian, 0o755)?;
    let path = debian.join("control");
    std::fs::write(&path, control).map_err(|source| EngineError::io(&path, source))?;
    build::set_mode(&path, 0o644)
}

/// The `DEBIAN/control` contents for `arch` at `version`, with the
/// `dpkg-shlibdeps`-derived runtime `depends`. Pure, so the control stanza
/// is testable.
fn control_text(arch: &str, version: &str, depends: &str) -> String {
    format!(
        "Package: {PKG_NAME}\n\
         Version: {version}\n\
         Section: video\n\
         Priority: optional\n\
         Architecture: {arch}\n\
         Depends: {depends}\n\
         Maintainer: boot2deb <build@boot2deb>\n\
         Description: FFmpeg with V4L2 stateless decode + Rockchip RKMPP encode for RK3588\n\
        \x20Hybrid pipeline for the RK3588 media stack:\n\
        \x20* -hwaccel v4l2request decode (rkvdec / hantro)\n\
        \x20* h264_rkmpp / hevc_rkmpp encode via VEPU580 + MPP userspace\n\
        \x20* scale_rkrga / vpp_rkrga via librga\n\
        \x20Installs to {INSTALL_PREFIX} so it coexists with the system FFmpeg.\n"
    )
}

/// The Debian version for the `ffmpeg-rk` deb, derived from the lock the way
/// u-boot's is: the pinned base `reference` (a leading `v`/`n` tag marker
/// dropped) plus the short base commit for uniqueness, sanitized to the Debian
/// upstream-version set ([`build::sanitize_deb_version`], which guarantees a
/// digit-leading result).
///
/// `git describe` is unusable here: the base is fetched depth-1, so it has no
/// ancestor tags and would fall through to a bare short hash — no ordering, and
/// possibly letter-leading. Deriving from the lock is stable and reproducible.
fn deb_version(reference: &str, commit: &str) -> String {
    let base = reference
        .strip_prefix('v')
        .or_else(|| reference.strip_prefix('n'))
        .unwrap_or(reference);
    let short = &commit[..commit.len().min(12)];
    build::sanitize_deb_version(&format!("{base}+g{short}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use boot2deb_core::lock::{
        BlobsPin, FfmpegPins, GitPin, KernelPin, PatchesPin, RootfsPin, UbootPin, UserspacePins,
    };

    fn lock_with(base_commit: &str, patches_commit: &str) -> Lock {
        let git = |c: &str| GitPin { source: "s".into(), reference: "r".into(), commit: c.into() };
        Lock {
            kernel: Some(KernelPin { id: "k".into(), source: "ks".into(), reference: "v".into(), commit: "kc".into() }),
            patches: Some(PatchesPin { profile: "rk3588-accel".into(), commit: patches_commit.into() }),
            uboot: Some(UbootPin { source: "us".into(), reference: "v".into(), commit: "uc".into() }),
            userspace: Some(UserspacePins { mpp: git("m"), librga: git("r"), libmali: git("l") }),
            ffmpeg: Some(FfmpegPins { base: git(base_commit), rockchip: git("rk") }),
            rootfs: RootfsPin { suite: "forky".into(), manifest: "m".into(), manifest_sha256: None },
            blobs: Some(BlobsPin { atf: "a".into(), tpl: "t".into(), bl32: None }),
            extra_debs: vec![],
            snapshot: None,
        }
    }

    #[test]
    fn unreadable_scan_dir_is_an_error_not_an_empty_scan() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        // An absent directory is a legitimate empty scan...
        assert!(read_dir_entries(&tmp.path().join("missing")).unwrap().is_empty());
        // ...but an unreadable one must surface, or `dpkg-shlibdeps` would compute
        // an incomplete Depends from a silently-shrunk input set.
        let dir = tmp.path().join("noread");
        std::fs::create_dir(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();
        // With DAC override (root), the mode does not bite; skip rather than
        // assert something the host cannot produce.
        let mode_bites = std::fs::read_dir(&dir).is_err();
        let result = read_dir_entries(&dir);
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        if !mode_bites {
            eprintln!("skipping: running with permission override (root)");
            return;
        }
        result.unwrap_err();
    }

    #[test]
    fn clone_manifest_tracks_base_commit_and_patch_pin() {
        let sig = |bc: &str, pc: &str, patches| {
            let lock = lock_with(bc, pc);
            let ff = lock.ffmpeg.as_ref().unwrap();
            clone_manifest(ff, lock.patches.as_ref(), patches).signature
        };
        let base = sig("bc1", "pc1", PatchSeries::Pinned);
        assert_eq!(base, sig("bc1", "pc1", PatchSeries::Pinned));
        // A base-tree bump or a patch-pin bump each invalidate the reused tree.
        assert_ne!(base, sig("bc2", "pc1", PatchSeries::Pinned));
        assert_ne!(base, sig("bc1", "pc2", PatchSeries::Pinned));
        // Co-dev mode splits the key; a co-dev content change restamps.
        let empty: Vec<String> = vec![];
        assert_ne!(base, sig("bc1", "pc1", PatchSeries::Dev(&empty)));
        let fp1 = vec!["media-accel/ffmpeg/0001.patch=aaa".to_string()];
        let fp2 = vec!["media-accel/ffmpeg/0001.patch=bbb".to_string()];
        assert_ne!(
            sig("bc1", "pc1", PatchSeries::Dev(&fp1)),
            sig("bc1", "pc1", PatchSeries::Dev(&fp2))
        );
    }

    #[test]
    fn output_manifest_covers_tree_arch_and_suite() {
        let sig = |lock: &Lock, arch: &str| {
            let ff = lock.ffmpeg.as_ref().unwrap();
            let us = lock.userspace.as_ref().unwrap();
            output_manifest(lock, ff, us, arch, PatchSeries::Pinned, PatchSeries::Pinned)
                .signature
                .clone()
        };
        let base = sig(&lock_with("bc1", "pc1"), "arm64");
        // Stable under identical inputs.
        assert_eq!(base, sig(&lock_with("bc1", "pc1"), "arm64"));
        // A base/patch pin bump reaches the output signature through the tree dep.
        assert_ne!(base, sig(&lock_with("bc2", "pc1"), "arm64"));
        assert_ne!(base, sig(&lock_with("bc1", "pc2"), "arm64"));
        // Arch splits the key (a hit must not restore a foreign-arch deb).
        assert_ne!(base, sig(&lock_with("bc1", "pc1"), "armhf"));
        // The suite stands in for the sandbox toolchain identity, so it splits too.
        let mut sid = lock_with("bc1", "pc1");
        sid.rootfs.suite = "sid".into();
        assert_ne!(base, sig(&sid, "arm64"));
        // Co-dev mode never shares an output entry with a pinned build.
        let dev_lock = lock_with("bc1", "pc1");
        assert_ne!(
            base,
            output_manifest(
                &dev_lock,
                dev_lock.ffmpeg.as_ref().unwrap(),
                dev_lock.userspace.as_ref().unwrap(),
                "arm64",
                PatchSeries::Dev(&[]),
                PatchSeries::Dev(&[]),
            )
            .signature
        );
    }

    #[test]
    fn output_manifest_folds_userspace_dependency_identity() {
        // ffmpeg links against the MPP + RGA userspace debs, so a change to
        // either userspace pin must invalidate the cached ffmpeg deb.
        let sig = |lock: &Lock| {
            let ff = lock.ffmpeg.as_ref().unwrap();
            let us = lock.userspace.as_ref().unwrap();
            output_manifest(lock, ff, us, "arm64", PatchSeries::Pinned, PatchSeries::Pinned)
                .signature
                .clone()
        };
        let base = sig(&lock_with("bc1", "pc1"));
        // An MPP pin bump (ffmpeg base/patch/suite/arch unchanged) splits the key.
        let mut mpp_bump = lock_with("bc1", "pc1");
        mpp_bump.userspace.as_mut().unwrap().mpp.commit = "m2".into();
        assert_ne!(base, sig(&mpp_bump));
        // An RGA pin bump likewise splits it.
        let mut rga_bump = lock_with("bc1", "pc1");
        rga_bump.userspace.as_mut().unwrap().librga.commit = "r2".into();
        assert_ne!(base, sig(&rga_bump));
    }

    #[test]
    fn deb_version_derives_from_lock_reference_and_commit() {
        // The `v4l2-request-n8.1` tag with the short base commit; leading `v` dropped,
        // digit-leading, `+g<short>` appended for uniqueness.
        assert_eq!(
            deb_version("v4l2-request-n8.1", "b57fbbe5c0de1234567890"),
            "4l2-request-n8.1+gb57fbbe5c0de"
        );
        // A digit-leading reference stays digit-leading; short commit is truncated.
        assert_eq!(deb_version("8.1", "fadff234400011"), "8.1+gfadff2344000");
        // A leading `n` (FFmpeg tag marker) is dropped like `v`.
        assert_eq!(deb_version("n8.1", "abcdef012345"), "8.1+gabcdef012345");
        // A letter-leading branch name gets the `0` prefix (Debian needs a digit).
        assert_eq!(deb_version("main", "abc123def456"), "0main+gabc123def456");
    }

    #[test]
    fn control_text_has_arch_version_and_runtime_deps() {
        let c = control_text("arm64", "8.1-19-g942418aa06", "librockchip-mpp1, librga2, libc6");
        assert!(c.contains("Package: ffmpeg-rk"));
        assert!(c.contains("Version: 8.1-19-g942418aa06"));
        assert!(c.contains("Architecture: arm64"));
        // The resolved (dpkg-shlibdeps) Depends is inserted verbatim.
        assert!(c.contains("Depends: librockchip-mpp1, librga2, libc6\n"));
        // Continuation lines of the Description are space-prefixed per deb-control.
        assert!(c.lines().any(|l| l.starts_with(" * -hwaccel")));
        assert!(c.contains(INSTALL_PREFIX));
    }

    #[test]
    fn soname_entry_matches_only_soname_symlinks() {
        assert_eq!(soname_entry("libavutil.so.60"), Some(("libavutil", "60")));
        assert_eq!(soname_entry("libswscale.so.9"), Some(("libswscale", "9")));
        // The real versioned file, the `.so` dev symlink, and non-libs are skipped.
        assert_eq!(soname_entry("libavutil.so.60.26.100"), None);
        assert_eq!(soname_entry("libavutil.so"), None);
        assert_eq!(soname_entry("ffmpeg"), None);
    }

    #[test]
    fn private_shlibs_local_lists_sonames_with_empty_deps() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // A realistic install lib dir: SONAME symlinks + real files + a dev symlink.
        for n in [
            "libavutil.so.60",
            "libavutil.so.60.26.100",
            "libavutil.so",
            "libswscale.so.9",
            "pkgconfig",
        ] {
            std::fs::write(dir.join(n), b"x").unwrap();
        }
        let local = private_shlibs_local(dir).unwrap();
        // One line per soname symlink, trailing space (empty dependency), sorted.
        assert_eq!(local, "libavutil 60 \nlibswscale 9 \n");
    }

    #[test]
    fn parse_shlibs_depends_extracts_the_value() {
        let vars = "shlibs:Depends=libc6 (>= 2.38), libx265-216 (>= 4.2), librga2\n";
        assert_eq!(
            parse_shlibs_depends(vars).as_deref(),
            Some("libc6 (>= 2.38), libx265-216 (>= 4.2), librga2")
        );
        // A file with other substvars but no shlibs:Depends yields None.
        assert_eq!(parse_shlibs_depends("misc:Depends=foo\n"), None);
        // An empty value (no libraries resolved) is treated as absent.
        assert_eq!(parse_shlibs_depends("shlibs:Depends=\n"), None);
    }

    #[test]
    fn required_userspace_debs_selects_in_install_order() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        for n in [
            "librockchip-mpp1_1.5.0-1_arm64.deb",
            "librockchip-mpp-dev_1.5.0-1_arm64.deb",
            "librga2_2.2.0-1_arm64.deb",
            "librga-dev_2.2.0-1_arm64.deb",
            "rockchip-mpp-demos_1.5.0-1_arm64.deb", // present but not a build dep
        ] {
            std::fs::write(dir.join(n), b"x").unwrap();
        }
        let debs = required_userspace_debs(dir).unwrap();
        let names: Vec<String> = debs
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(
            names,
            vec![
                "librockchip-mpp1_1.5.0-1_arm64.deb",
                "librockchip-mpp-dev_1.5.0-1_arm64.deb",
                "librga2_2.2.0-1_arm64.deb",
                "librga-dev_2.2.0-1_arm64.deb",
            ]
        );
    }

    #[test]
    fn required_userspace_debs_errors_when_a_dep_is_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // Only the runtime libs, no -dev packages.
        std::fs::write(dir.join("librockchip-mpp1_1_arm64.deb"), b"x").unwrap();
        std::fs::write(dir.join("librga2_2_arm64.deb"), b"x").unwrap();
        let err = required_userspace_debs(dir).unwrap_err();
        match err {
            EngineError::ArtifactMissing { what, .. } => assert!(what.contains("-dev")),
            other => panic!("expected ArtifactMissing, got {other:?}"),
        }
        // A missing dir is also a clear error, not an I/O panic.
        assert!(matches!(
            required_userspace_debs(&dir.join("nope")).unwrap_err(),
            EngineError::ArtifactMissing { .. }
        ));
    }
}
