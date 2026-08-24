//! ffmpeg-rk compile stage: assemble the hybrid FFmpeg — mainline
//! V4L2-request stateless *decode* from the Kwiboo base, Rockchip rkmpp *encode*
//! and rkrga *scale* grafted on from nyanmisaka — and package it as the
//! `ffmpeg-rk` `.deb`.
//!
//! The graft is the series' ffmpeg `git am` series: the nyanmisaka
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
//! `librga-dev`) and runtime-depends on `librockchip-mpp1` + `librga2`. Those packages
//! are this build's own output, so the stage publishes the `.deb`s a prior
//! [`userspace`] run produced as a trusted `file://` pool and layers them into its
//! build root from there — resolved like any other package, with their transitive
//! dependencies, rather than pushed into the tree behind the resolver's back.
//!
//! [`userspace`]: crate::build::userspace

use crate::build::elf;
use crate::build::{
    self, deb_names, pick_deb, probe, stage_artifact, BuildEnv, CloneMode, ClonePinned, PatchScope,
    PatchSource, SeriesIdentity,
};
use crate::error::EngineError;
use crate::event::{EventSink, Step};
use crate::git;
use crate::repo::LocalDistsRepo;
use crate::sandbox::{BuildRoot, BuildRootSpec, BuildSandbox, SandboxRun};
use boot2deb_core::lock::{FfmpegPins, Lock, UserspacePins};
use std::path::{Path, PathBuf};

/// Install prefix baked into the build; keeps `ffmpeg-rk` out of the system FFmpeg's
/// paths so both can coexist. The coexistence is load-bearing rather than tidy: this
/// tree's `libav*`/`libsw*` carry the same sonames as the suite's own FFmpeg packages,
/// so the two sets must never meet on one search path — see [`rpath_ldflag`], which is
/// what makes the separation hold at runtime.
///
/// A caller reaches the programs either by absolute path under this prefix or through
/// the `-rk`-suffixed links [`stage_path_symlinks`] puts in `/usr/bin`.
const INSTALL_PREFIX: &str = "/opt/ffmpeg-rk";

/// Stage-recipe version for the ffmpeg tree signature: bump when the
/// fetch/patch logic that shapes the reused tree changes.
const CLONE_STAGE_VERSION: u32 = 1;

/// Stage-recipe version for the ffmpeg **output** signature (Tier-2 artifact cache):
/// bump when the configure/compile/package logic changes the produced `.deb`
/// in a way the folded inputs do not already capture.
const OUTPUT_STAGE_VERSION: u32 = 2;

/// Debian package name.
const PKG_NAME: &str = "ffmpeg-rk";

/// Where the lookup probe appends, relative to the stage root — a bound host path, so
/// the record outlives the cage that wrote it and is not itself in the overlay under
/// investigation.
const LOOKUP_PROBE_REPORT: &str = "lookup-probe.log";

/// ffmpeg build-deps layered into its build root. The base tooling
/// (`build-essential`, `pkg-config`) is already in the sandbox base set; these are
/// the codec/format libraries `./configure` probes, and they come from the suite.
/// `librockchip-mpp-dev` / `librga-dev` are *not* here — they are this build's own
/// output, added per SoC by [`userspace_dev_packages`] and resolved from the pool.
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
    // Build-root only: headers and loader stub for the three Vulkan flags in
    // [`BASE_CONFIGURE_FLAGS`]. `libvulkan-dev` is the largest entry in this list by
    // an order of magnitude (~37 MB, nearly all of it the registry XML and the
    // validation headers) and none of it reaches an image.
    "libvulkan-dev",
    "libplacebo-dev",
    "libshaderc-dev",
    // `libshaderc-dev` satisfies `spirv_library`, which is what gives us libplacebo and
    // `overlay_vulkan`. The shader-compiling filters -- `scale_vulkan`, `bwdif_vulkan`,
    // `avgblur_vulkan` -- gate on `spirv_compiler` instead, which `check_glslc` probes
    // for a `glslc` binary at build time. Debian ships that binary in its own package.
    "glslc",
];

/// The userspace `.deb` name prefixes ffmpeg build-depends on for this build, in
/// install order — each tree's runtime lib first, then its `-dev`.
///
/// Derived from the pins, not fixed: ffmpeg build-depends on a userspace package
/// exactly when it is configured against that library, so the list must track the
/// same SoC-declared trees [`configure_flags`] reads. Demanding
/// `librockchip-mpp-dev` on a SoC that builds no MPP would fail the stage looking
/// for a `.deb` nothing produces.
fn userspace_dep_prefixes(userspace: &UserspacePins) -> Vec<&'static str> {
    let mut prefixes = Vec::new();
    if userspace.mpp.is_some() {
        prefixes.extend(["librockchip-mpp1_", "librockchip-mpp-dev_"]);
    }
    if userspace.librga.is_some() {
        prefixes.extend(["librga2_", "librga-dev_"]);
    }
    prefixes
}

/// The userspace packages ffmpeg's build root layers in — **each tree's runtime library
/// and its `-dev`**, one pair per userspace tree the SoC declares. The layered
/// counterpart of [`userspace_dep_prefixes`], and deliberately the same set.
///
/// Names, not file paths: they are resolved from the build pool like any other package,
/// with their transitive dependencies, rather than pushed into the tree behind the
/// resolver's back.
///
/// **Both halves are named explicitly because the vendor packaging does not relate
/// them.** `librockchip-mpp-dev` declares `Depends: librockchip-mpp1`, but `librga-dev`
/// declares no dependencies at all — so asking only for the `-dev` packages yields a
/// build root with librga's headers and `librga.pc` but no `librga.so`, and ffmpeg's
/// `./configure` fails its link probe with "librga not found using pkg-config". The
/// runtime library is also what carries the `shlibs` `dpkg-shlibdeps` reads, so naming
/// it is what makes the produced deb's `Depends` resolvable at all.
///
/// Derived from the pins for the same reason the prefixes are: asking for
/// `librockchip-mpp-dev` on a SoC that builds no MPP would fail the resolution on a
/// package the pool cannot hold.
fn userspace_layer_packages(userspace: &UserspacePins) -> Vec<&'static str> {
    let mut packages = Vec::new();
    if userspace.mpp.is_some() {
        packages.extend(["librockchip-mpp1", "librockchip-mpp-dev"]);
    }
    if userspace.librga.is_some() {
        packages.extend(["librga2", "librga-dev"]);
    }
    packages
}

/// The whole build-dependency set this stage layers over the sandbox base: the suite's
/// codec libraries plus this build's own userspace `.deb`s.
///
/// One function, read by the [`BuildRootSpec`] that stages the layer *and* by the output
/// signature that keys on it, so a package cannot reach `./configure` without reaching
/// the key. Which matters here more than anywhere: ffmpeg's `./configure` is a probe
/// suite, and every entry decides whether a codec is compiled in.
fn layer_packages(userspace: &UserspacePins) -> Vec<&'static str> {
    let mut packages: Vec<&'static str> = FFMPEG_DEPS.to_vec();
    packages.extend(userspace_layer_packages(userspace));
    packages
}

/// The runtime packages the produced `.deb` must depend on — one per userspace tree the
/// SoC declares, the libraries ffmpeg links against from this build's own output.
fn required_runtime_depends(userspace: &UserspacePins) -> Vec<&'static str> {
    let mut packages = Vec::new();
    if userspace.mpp.is_some() {
        packages.push("librockchip-mpp1");
    }
    if userspace.librga.is_some() {
        packages.push("librga2");
    }
    packages
}

/// Refuse a `depends` that omits a userspace library this build linked against.
///
/// `dpkg-shlibdeps` maps each `NEEDED` soname to the package whose `shlibs` claims it.
/// When that package is absent from the build root it **does not fail** — with
/// `--ignore-missing-info` it warns, and for a soname no package owns it errors, but a
/// package present without usable `shlibs` metadata simply contributes nothing. The
/// result is an `ffmpeg-rk` deb with no `Depends: librga2` that installs cleanly and
/// breaks at runtime on the board, which no exit status reports.
///
/// So the resolved text is checked rather than the run's status. This is the exact string
/// [`control_text`] writes as `Depends:`, checked before the `.deb` is built, so a
/// dropped dependency stops the stage instead of shipping.
fn assert_userspace_depends(depends: &str, userspace: &UserspacePins) -> Result<(), EngineError> {
    // Field-split rather than substring-match: `librga2` must not be satisfied by
    // `librga2-dev`, and a version relation (`librga2 (>= 1.2)`) is the package plus a
    // constraint, so the name is the first token of a comma-separated field.
    let named: Vec<&str> = depends
        .split(',')
        .filter_map(|d| d.split_whitespace().next())
        .collect();
    let missing: Vec<&str> = required_runtime_depends(userspace)
        .into_iter()
        .filter(|want| !named.contains(want))
        .collect();
    if missing.is_empty() {
        return Ok(());
    }
    Err(EngineError::ArtifactMissing {
        what: format!(
            "runtime Depends on {} in the ffmpeg-rk control file — dpkg-shlibdeps \
             resolved `{depends}`, so the build root did not carry the shlibs of the \
             userspace package(s) this build links against",
            missing.join(", ")
        ),
        location: "debian/substvars (shlibs:Depends)".into(),
    })
}

/// `./configure` feature flags every build gets, whatever the SoC: the container
/// and codec set, plus `--enable-v4l2-request`, which needs no vendor userspace at
/// all — it decodes through the kernel's own stateless V4L2 API.
///
/// The Rockchip-specific flags are not here; they are decided per build by
/// [`configure_flags`] from the userspace trees the SoC actually declares.
///
/// The three Vulkan flags are a set, not three independent choices:
/// - `--enable-vulkan` compiles `libavutil/hwcontext_vulkan.c`, which is where a
///   V4L2-request DRM PRIME frame becomes importable at all.
/// - `--enable-libplacebo` adds `vf_libplacebo` — scale, tone-map, deinterlace and
///   alpha compositing over that context.
/// - `--enable-libshaderc` supplies `spirv_compiler`/`spirv_library`, without which
///   `./configure` builds *neither* `scale_vulkan`/`overlay_vulkan` nor swscale's
///   Vulkan compute backend, regardless of the other two.
///
/// Linking them costs the image the Vulkan runtime libraries through the deb's
/// `Depends`, and nothing else: the GPU driver that actually executes the work is an
/// ICD loaded at runtime, is not a dependency of anything here, and is opted into per
/// image by the `vulkan` rootfs feature.
const BASE_CONFIGURE_FLAGS: &[&str] = &[
    "--enable-gpl",
    "--enable-version3",
    "--enable-nonfree",
    "--enable-shared",
    "--disable-static",
    "--enable-libdrm",
    "--enable-libudev",
    "--enable-v4l2-request",
    "--enable-libx264",
    "--enable-libx265",
    "--enable-libfdk-aac",
    "--enable-libass",
    "--enable-libfreetype",
    "--enable-openssl",
    "--enable-vulkan",
    "--enable-libplacebo",
    "--enable-libshaderc",
];

/// The linker flag that bakes the install `libdir` into every object this stage
/// builds, so each finds its siblings without help from the environment.
///
/// The bundled `libav*`/`libsw*` carry the *same* sonames as Debian's own FFmpeg
/// packages — `libavcodec.so.62`, `libavutil.so.60`, `libavfilter.so.11` and the rest
/// — so the one thing this deb must never do is put its `libdir` on the system-wide
/// search path. An `/etc/ld.so.conf.d` drop-in would silently re-point every program
/// on the box that links `libavcodec.so.62` at this build, which is exactly the
/// shadowing [`INSTALL_PREFIX`] exists to prevent; a per-object `RUNPATH` keeps the
/// resolution local to these binaries and leaves the system FFmpeg alone.
///
/// It is spelled as an explicit linker flag rather than configure's `--enable-rpath`
/// so the emitted tag does not depend on how a given tree spells that option.
/// [`assert_runpath`] then proves it landed, rather than trusting that it did.
fn rpath_ldflag() -> String {
    format!("--extra-ldflags=-Wl,-rpath,{INSTALL_PREFIX}/lib")
}

/// The `./configure` flags for this build: [`BASE_CONFIGURE_FLAGS`] plus the
/// [`rpath_ldflag`] and one flag per Rockchip userspace tree the SoC declares.
///
/// The SoC's declared sources *are* the capability statement, so the configure
/// surface is derived from them rather than fixed: a part with no vendor
/// `mpp_service` pins no MPP and must not be asked for `--enable-rkmpp`, since the
/// library it would link does not exist for that build.
///
/// `--enable-rkrga` additionally requires `--enable-rkmpp` — the rkrga filters
/// allocate their frames as `AVRKMPPFramesContext`, and ffmpeg's own configure
/// rejects the pair — so librga alone yields no rkrga. On such a SoC librga is still
/// built and shipped for programs that speak its API directly; it just is not an
/// ffmpeg filter.
fn configure_flags(userspace: &UserspacePins) -> Vec<String> {
    let mut flags: Vec<String> = BASE_CONFIGURE_FLAGS
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    flags.push(rpath_ldflag());
    if userspace.mpp.is_some() {
        flags.push("--enable-rkmpp".to_string());
        if userspace.librga.is_some() {
            flags.push("--enable-rkrga".to_string());
        }
    }
    flags
}

/// Filesystem inputs for the ffmpeg stage.
pub struct FfmpegOptions<'a> {
    /// Kwiboo base clone source (git URL or local path). A local checkout of the
    /// FFmpeg tree makes the fetch near-instant.
    pub base_src: &'a str,
    /// The `ffmpeg` patch scope's checkout + pin — the materialized graft series plus
    /// the NV15 fix. `None` when the resolved kernel names no patch series.
    pub patches: Option<PatchSource<'a>>,
    /// Directory holding the userspace `.deb`s ffmpeg build-depends on — the output
    /// dir of a prior [`userspace`](crate::build::userspace) run.
    pub userspace_debs: &'a Path,
    /// Whether the userspace stage of this build also built Mali.
    ///
    /// Nothing here compiles Mali, and ffmpeg does not link it. It is carried because
    /// the flag decides what the *userspace* stage layered over the shared sandbox base
    /// ([`layer_packages`](crate::build::userspace::layer_packages)), and this stage's
    /// key folds the userspace packages' output signatures — so recomputing them here
    /// with a different answer would name `.deb`s that were never built.
    pub build_libmali: bool,
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
/// Reads only the [`Lock`] for pins: the base commit and the patch series.
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
    // outlive the borrowed `SeriesIdentity`.
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
    let out_man = output_manifest(
        lock,
        ffmpeg,
        userspace,
        arch,
        &env.sandbox_id,
        opts.build_libmali,
        ffmpeg_patches,
        us_patches,
    );
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
    let debs = required_userspace_debs(opts.userspace_debs, userspace)?;
    // The ffmpeg node runs only for a media-accel image build, which resolves a suite;
    // the pool and the layer are both stamped with it.
    let suite = lock
        .rootfs
        .as_ref()
        .expect("the ffmpeg node runs only for an image build, which pins a rootfs")
        .suite
        .as_str();

    // Tier-1 reuse of the fetched+patched tree: a lock bump (base commit
    // or patch pin) rebuilds it; configure/compile re-run regardless.
    let man = clone_manifest(ffmpeg, lock.patches.as_ref(), ffmpeg_patches);
    build::reuse_or_refresh_tree(&tree, &man, "ffmpeg", &step, || {
        fetch_and_patch(ffmpeg, opts, &tree, &step)
    })?;
    step.progress(30);

    step.log(format!("sandbox: {}", sandbox.describe()));
    sandbox.ensure_ready(&step)?;
    // The build's own userspace `.deb`s as a trusted `file://` pool, so ffmpeg's layer
    // resolves them the way it resolves anything else — the package *and* its transitive
    // dependencies, in one resolution. This is ffmpeg's own pool, assembled from the
    // packages it build-depends on: the rootfs node's pool is a per-build snapshot of the
    // whole artifact ledger that it clears and rewrites after every stage, so it cannot
    // carry anything forward.
    // Deterministic build timestamp from the locked base commit; the tree's HEAD is a
    // `git am` patch commit stamped now, so read the base explicitly. It dates both the
    // pool's `Release` — without which a publish takes the wall clock and is not
    // byte-reproducible — and the compile itself.
    let source_date_epoch = git::commit_epoch(&tree, &ffmpeg.base.commit).ok();
    let pool_dir = stage_root.join("build-pool");
    let pool = LocalDistsRepo::assemble(&pool_dir, &debs, suite, arch, source_date_epoch, &step)?;
    let packages = layer_packages(userspace);
    let root = sandbox.build_root(
        &BuildRootSpec {
            packages: &packages,
            pool: Some(pool.file_url()),
            stage: "ffmpeg",
        },
        &step,
    )?;
    step.progress(45);

    // Bind the ffmpeg stage root so the build tree, pkg-stage, and produced .deb
    // are all visible inside the sandbox at their host paths.
    let binds = [stage_root.clone()];
    let pkg_stage = stage_root.join("pkg-stage");
    // A stale pkg-stage from an interrupted run would poison `make install`.
    let _ = std::fs::remove_dir_all(&pkg_stage);

    let build_env: Vec<(String, String)> = source_date_epoch
        .map(|e| vec![("SOURCE_DATE_EPOCH".to_string(), e.to_string())])
        .unwrap_or_default();

    configure(
        &root,
        &tree,
        &binds,
        &build_env,
        &configure_flags(userspace),
        &step,
    )?;
    step.progress(55);
    compile(
        &root,
        env,
        &tree,
        &stage_root.join(LOOKUP_PROBE_REPORT),
        &binds,
        &build_env,
        &step,
    )?;
    step.progress(85);
    install_to_stage(&root, &tree, &pkg_stage, &binds, &step)?;
    // The staged tree must be able to run from where it will be installed before it is
    // worth archiving: a missing RUNPATH is invisible to `make install` and fatal on
    // the board. Checked here, so the stage fails rather than shipping.
    assert_runpath(&pkg_stage, &step)?;
    stage_path_symlinks(&pkg_stage, &step)?;
    step.progress(88);

    // Derive the runtime Depends from what the built binaries actually link
    // (`dpkg-shlibdeps`), rather than a hand-maintained soname list — so the deb
    // tracks whatever library versions the target suite currently ships.
    let depends = resolve_depends(&root, &stage_root, &pkg_stage, arch, &binds, &step)?;
    // A missing `shlibs` entry does not fail `dpkg-shlibdeps`; it silently drops the
    // dependency. So the run's exit status proves nothing about the one thing the pool
    // exists to deliver, and the resolved text is checked instead.
    assert_userspace_depends(&depends, userspace)?;
    step.progress(90);

    let version = deb_version(&ffmpeg.base.reference, &ffmpeg.base.commit);
    let control = control_text(arch, &version, &depends);
    write_control(&pkg_stage, &control)?;
    let deb_name = format!("{PKG_NAME}_{version}_{arch}.deb");
    let deb_in_stage = stage_root.join(&deb_name);
    package_deb(&root, &pkg_stage, &deb_in_stage, &build_env, &binds, &step)?;

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
/// Public so `why-rebuild` ([`crate::plan`]) asks the artifact store the same
/// question this stage does, rather than reimplementing the key it is asking under.
#[allow(clippy::too_many_arguments)]
pub fn output_manifest(
    lock: &Lock,
    ffmpeg: &FfmpegPins,
    userspace: &UserspacePins,
    arch: &str,
    sandbox_id: &str,
    build_libmali: bool,
    patches: SeriesIdentity,
    us_patches: SeriesIdentity,
) -> crate::signature::SignatureManifest {
    // The ffmpeg node runs only for a media-accel image build, which resolves a suite.
    let suite = lock
        .rootfs
        .as_ref()
        .expect("the ffmpeg node runs only for an image build, which pins a rootfs")
        .suite
        .as_str();
    let tree_sig = clone_manifest(ffmpeg, lock.patches.as_ref(), patches).signature();
    let mpp_inputs = crate::build::userspace::PatchInputs {
        pin: lock.patches.as_ref(),
        patches: us_patches,
    };
    let flags = configure_flags(userspace);
    let mut b = crate::signature::SignatureBuilder::new("ffmpeg:out", OUTPUT_STAGE_VERSION);
    b.fold_dep(&tree_sig)
        .fold_ordered("configure_flags", &flags)
        .fold_scalar("arch", arch)
        .fold_scalar("suite", suite)
        // The sandbox instance that compiles this deb — which mirror its userland came
        // from, and which `qemu-user` runs its compiler. See
        // [`BuildEnv::sandbox_id`](crate::build::BuildEnv::sandbox_id).
        .fold_scalar("sandbox", sandbox_id)
        // What was layered over that sandbox, from the same function that stages it.
        // `./configure` is a probe suite, so every entry decides whether a codec is
        // compiled in.
        .fold_set("build_deps", &layer_packages(userspace))
        .fold_scalar("base.reference", &ffmpeg.base.reference)
        .fold_scalar("pkg_name", PKG_NAME);
    // Fold a dependency only for a tree this build has. Folding the absent ones as
    // empty would make two SoCs with different hardware hash alike; omitting them
    // keeps the signature a statement about what was actually linked. `flags` already
    // records *which* trees those were, so the two cannot disagree.
    if let Some(mpp) = &userspace.mpp {
        let dep = crate::build::userspace::output_manifest_for(
            "mpp",
            &mpp.commit,
            suite,
            arch,
            sandbox_id,
            build_libmali,
            Some(&mpp_inputs),
        )
        .signature();
        b.fold_dep(&dep);
    }
    if let Some(rga) = &userspace.librga {
        let dep = crate::build::userspace::output_manifest_for(
            "librga",
            &rga.commit,
            suite,
            arch,
            sandbox_id,
            build_libmali,
            None,
        )
        .signature();
        b.fold_dep(&dep);
    }
    b.manifest()
}

/// The Tier-1 signature manifest of the fetched+patched ffmpeg tree: the
/// base commit and the patch series (`build::fold_patch_series`) that together
/// determine the tree. The source URL is excluded (the commit content-addresses the
/// base). The [`SeriesIdentity`] fold covers the pinned patch commit and — in co-dev
/// mode — the live-series fingerprint, so a co-dev build never shares a
/// stamp with a pinned one and an edited patch restamps. Public so `why-rebuild`
/// ([`crate::plan`]) recomputes the same signature it stamps here. Takes the
/// [`FfmpegPins`] and the patch series/commit directly rather than the whole
/// [`Lock`], since it is only meaningful for a media-accel build (one that has
/// ffmpeg pins).
pub fn clone_manifest(
    ffmpeg: &FfmpegPins,
    pin: Option<&boot2deb_core::lock::PatchesPin>,
    patches: SeriesIdentity,
) -> crate::signature::SignatureManifest {
    let mut b = crate::signature::SignatureBuilder::new("ffmpeg", CLONE_STAGE_VERSION);
    b.fold_scalar("ffmpeg.base.commit", &ffmpeg.base.commit);
    build::fold_patch_series(&mut b, pin, patches);
    b.manifest()
}

/// Fetch the base at its locked commit and `git am` the series' ffmpeg series —
/// the materialized nyanmisaka graft plus the NV15 fix — leaving the tree at
/// the fully-assembled source the build compiles.
///
/// On any failure the partial tree is removed, so a re-run after a failed `git am`
/// starts clean rather than silently reusing a half-applied series (the reuse
/// check in [`build_ffmpeg`] only ever sees a completed tree). The graft rides in
/// the series' ffmpeg scope; no kernel-range gate here — that guards the kernel
/// node, and the series is already validated there.
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
        step.log(format!(
            "applied {n} ffmpeg patch(es) ({})",
            p.pin.series.join(", ")
        ));
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
    root: &BuildRoot,
    tree: &Path,
    binds: &[PathBuf],
    env: &[(String, String)],
    flags: &[String],
    step: &Step,
) -> Result<(), EngineError> {
    let mut argv = vec![
        tree.join("configure").to_string_lossy().into_owned(),
        format!("--prefix={INSTALL_PREFIX}"),
    ];
    argv.extend(flags.iter().cloned());
    let spec = SandboxRun {
        work: tree,
        binds,
        env,
        argv: &argv,
        context: "ffmpeg ./configure",
    };
    root.run(&spec, step)
}

/// Run `make -j` inside the sandbox. The build is target-native there (the sandbox
/// is a target-arch userland, reached via qemu-user on a cross host), so no
/// `CROSS_COMPILE` — unlike the kernel and u-boot stages, which cross-compile in a
/// host-arch root.
///
/// This is the build's one parallel command in a freshly mounted overlay, and the only
/// place a header has ever gone missing that was not, so it runs under
/// [`probe::wrap`](crate::build::probe::wrap) — free on success, and the difference
/// between a transient and a durably wrong mount when it is not.
fn compile(
    root: &BuildRoot,
    env: &BuildEnv,
    tree: &Path,
    report: &Path,
    binds: &[PathBuf],
    run_env: &[(String, String)],
    step: &Step,
) -> Result<(), EngineError> {
    let make = vec!["make".to_string(), format!("-j{}", env.jobs())];
    let argv = probe::wrap(&make, report);
    let spec = SandboxRun {
        work: tree,
        binds,
        env: run_env,
        argv: &argv,
        context: "ffmpeg make",
    };
    root.run(&spec, step)
}

/// Run `make install DESTDIR=<pkg_stage>` inside the sandbox, staging the install
/// tree under the prefix for packaging.
fn install_to_stage(
    root: &BuildRoot,
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
        env: &[],
        argv: &argv,
        context: "ffmpeg make install",
    };
    root.run(&spec, step)
}

/// Build the `.deb` from the staged install tree with `dpkg-deb`, run in the build
/// root — the same root that compiled the tree, so the archiver is the suite's rather
/// than the host's.
///
/// No `fakeroot`: the root maps the caller to uid 0
/// ([`sandbox`](crate::sandbox) module docs), so a tree `make install` staged is
/// already `root:root` where it is archived, and `dpkg-deb` records the ownership a
/// `.deb` must carry with nothing faking anything.
///
/// `build_env` carries `SOURCE_DATE_EPOCH` (the locked base commit's committer
/// date), so `dpkg-deb` clamps every archive member's mtime to it — the `.deb`
/// is byte-reproducible rather than stamped with the build clock.
fn package_deb(
    root: &BuildRoot,
    pkg_stage: &Path,
    deb_out: &Path,
    build_env: &[(String, String)],
    binds: &[PathBuf],
    step: &Step,
) -> Result<(), EngineError> {
    let argv = vec![
        "dpkg-deb".to_string(),
        "--build".to_string(),
        pkg_stage.to_string_lossy().into_owned(),
        deb_out.to_string_lossy().into_owned(),
    ];
    let spec = SandboxRun {
        work: pkg_stage,
        binds,
        env: build_env,
        argv: &argv,
        context: "dpkg-deb --build ffmpeg-rk",
    };
    root.run(&spec, step)
}

/// Select the userspace `.deb`s ffmpeg build-depends on (highest version each)
/// from `dir`, in install order, erroring if the dir or any package is absent —
/// which means the userspace stage was not run first.
fn required_userspace_debs(
    dir: &Path,
    userspace: &UserspacePins,
) -> Result<Vec<PathBuf>, EngineError> {
    let prefixes = userspace_dep_prefixes(userspace);
    // No vendor userspace at all (a decode-only stack): nothing to install, and the
    // userspace stage may legitimately have produced no output dir.
    if prefixes.is_empty() {
        return Ok(Vec::new());
    }
    if !dir.exists() {
        return Err(EngineError::ArtifactMissing {
            what: "userspace .debs (run the userspace stage first)".into(),
            location: dir.display().to_string(),
        });
    }
    let names = deb_names(dir)?;
    let mut debs = Vec::new();
    for prefix in prefixes {
        match pick_deb(&names, prefix) {
            Some(name) => debs.push(dir.join(name)),
            None => {
                return Err(EngineError::ArtifactMissing {
                    what: format!(
                        "userspace dependency {prefix}*.deb (run the userspace stage first)"
                    ),
                    location: dir.display().to_string(),
                })
            }
        }
    }
    Ok(debs)
}

/// Compute the runtime `Depends:` from the built binaries with `dpkg-shlibdeps`,
/// run inside the build root so it reads the target-arch ELFs against the target's
/// dpkg/shlibs data — including the `shlibs` of the userspace packages the root
/// layered in from this build's own pool.
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
    root: &BuildRoot,
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
        env: &[],
        argv: &argv,
        context: "dpkg-shlibdeps ffmpeg-rk",
    };
    root.run(&spec, step)?;

    let vars = std::fs::read_to_string(&substvars).map_err(|s| EngineError::io(&substvars, s))?;
    let depends = parse_shlibs_depends(&vars).ok_or_else(|| EngineError::ArtifactMissing {
        what: "shlibs:Depends from dpkg-shlibdeps".into(),
        location: substvars.display().to_string(),
    })?;
    let _ = std::fs::remove_dir_all(&work);
    step.log(format!("resolved runtime Depends: {depends}"));
    Ok(depends)
}

/// Assert every staged executable and private library resolves its siblings through
/// the install prefix, before the tree is archived into a `.deb`.
///
/// A `make install` that produces a *runnable* tree is not the same as one that
/// produces a **self-sufficient** one. With no `RUNPATH`, `/opt/ffmpeg-rk/bin/ffmpeg`
/// starts only for a caller that has already set `LD_LIBRARY_PATH` — so the binary
/// the media-accel image exists to ship dies with `libavdevice.so.62: cannot open
/// shared object file`, and the failure surfaces on the board instead of in the build
/// that caused it.
///
/// Every object is checked, not just the executables: `DT_RUNPATH` is consulted only
/// for the object that carries it and is **not** inherited by that object's own
/// dependencies, so an `ffmpeg` binary that resolves perfectly proves nothing about
/// whether `libavcodec` can find `libavutil`.
fn assert_runpath(pkg_stage: &Path, step: &Step) -> Result<(), EngineError> {
    let libdir = format!("{INSTALL_PREFIX}/lib");
    let objects = scan_binaries(pkg_stage)?;
    let mut found: Vec<(String, Option<String>)> = Vec::new();
    for path in &objects {
        let bytes = std::fs::read(path).map_err(|source| EngineError::io(path, source))?;
        let name = path.file_name().unwrap_or(path.as_os_str());
        found.push((name.to_string_lossy().into_owned(), elf::runpath(&bytes)));
    }
    let missing = objects_missing_runpath(&found, &libdir);
    if missing.is_empty() {
        step.log(format!(
            "verified RUNPATH {libdir} on {} staged object(s)",
            objects.len()
        ));
        return Ok(());
    }
    Err(EngineError::ArtifactMissing {
        what: format!(
            "RUNPATH {libdir} on {} of {} staged object(s) — {} — so the built tree \
             would resolve its own libraries only under LD_LIBRARY_PATH; the \
             `--extra-ldflags=-Wl,-rpath` passed to ./configure did not reach the link",
            missing.len(),
            objects.len(),
            missing.join(", ")
        ),
        location: pkg_stage.display().to_string(),
    })
}

/// The staged objects whose search path does not name `libdir`, rendered as
/// `name (what it carries instead)` for the failure message. Pure, so the policy is
/// testable without staging a build tree.
///
/// The search path is field-split on `:` rather than substring-matched: a `RUNPATH` of
/// `/opt/ffmpeg-rk/lib-old` contains the libdir as a prefix but does not resolve to
/// it, and would otherwise pass.
fn objects_missing_runpath(found: &[(String, Option<String>)], libdir: &str) -> Vec<String> {
    found
        .iter()
        .filter(|(_, path)| {
            !path
                .iter()
                .any(|r| r.split(':').any(|entry| entry == libdir))
        })
        .map(|(name, path)| format!("{name} ({})", path.as_deref().unwrap_or("no RUNPATH")))
        .collect()
}

/// Stage a `/usr/bin/<name>-rk` symlink for each program under the install prefix, so
/// the build is reachable without typing an absolute path.
///
/// The `-rk` suffix is not decoration. Debian's own `ffmpeg` package owns
/// `/usr/bin/ffmpeg`, so claiming that path would be a dpkg file conflict *and* would
/// put this build in front of the distro's for every caller on the box — the same
/// shadowing [`rpath_ldflag`] keeps out of the loader. A distinct name coexists.
///
/// The links are staged into the `.deb` rather than shipped in a feature overlay so
/// that dpkg owns them: an overlay file belongs to no package and would outlive a
/// purge of `ffmpeg-rk` as a dangling link.
fn stage_path_symlinks(pkg_stage: &Path, step: &Step) -> Result<(), EngineError> {
    let bin_dir = pkg_stage.join(&INSTALL_PREFIX[1..]).join("bin");
    let mut names: Vec<String> = read_dir_entries(&bin_dir)?
        .iter()
        .filter(|e| e.path().is_file())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    if names.is_empty() {
        return Ok(());
    }
    let usr_bin = pkg_stage.join("usr/bin");
    std::fs::create_dir_all(&usr_bin).map_err(|source| EngineError::io(&usr_bin, source))?;
    // Mode-normalized like the control metadata: the host umask must not decide the
    // permissions of a directory that ships in the archive.
    build::set_mode(&usr_bin, 0o755)?;
    for name in &names {
        let link = usr_bin.join(format!("{name}-rk"));
        // A stale link from an interrupted run would make `symlink` fail with EEXIST.
        let _ = std::fs::remove_file(&link);
        std::os::unix::fs::symlink(format!("{INSTALL_PREFIX}/bin/{name}"), &link)
            .map_err(|source| EngineError::io(&link, source))?;
    }
    step.log(format!(
        "linked {} into /usr/bin",
        names
            .iter()
            .map(|n| format!("{n}-rk"))
            .collect::<Vec<_>>()
            .join(", ")
    ));
    Ok(())
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
        Ok(entries) => entries
            .map(|e| e.map_err(|s| EngineError::io(dir, s)))
            .collect(),
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

    /// A pin set declaring every userspace tree — the RK3588-shaped stack.
    fn all_trees() -> UserspacePins {
        let git = |c: &str| {
            Some(GitPin {
                source: "s".into(),
                reference: "r".into(),
                commit: c.into(),
            })
        };
        UserspacePins {
            mpp: git("m"),
            librga: git("r"),
            libmali: git("l"),
        }
    }

    /// A pin set with librga but no MPP and no libmali — the RK3576-shaped stack,
    /// where decode is the kernel's own V4L2 path and the GPU runs on Mesa.
    fn rga_only() -> UserspacePins {
        UserspacePins {
            mpp: None,
            librga: Some(GitPin {
                source: "s".into(),
                reference: "r".into(),
                commit: "r".into(),
            }),
            libmali: None,
        }
    }

    /// The base flags plus nothing: a SoC declaring no vendor userspace at all.
    fn no_trees() -> UserspacePins {
        UserspacePins {
            mpp: None,
            librga: None,
            libmali: None,
        }
    }

    #[test]
    fn configure_flags_follow_the_socs_declared_trees() {
        // Everything declared: the full vendor pipeline.
        let full = configure_flags(&all_trees());
        assert!(full.contains(&"--enable-rkmpp".to_string()));
        assert!(full.contains(&"--enable-rkrga".to_string()));

        // librga without MPP yields *neither* rkmpp nor rkrga: ffmpeg's own configure
        // rejects rkrga without rkmpp, so asking for it would fail the build. librga is
        // still built and shipped — it just is not an ffmpeg filter.
        let rga = configure_flags(&rga_only());
        assert!(!rga.contains(&"--enable-rkmpp".to_string()));
        assert!(!rga.contains(&"--enable-rkrga".to_string()));

        // v4l2-request is unconditional: it needs no vendor userspace, only the
        // kernel's stateless decoder, which is the whole point on a mainline SoC.
        for flags in [&full, &rga, &configure_flags(&no_trees())] {
            assert!(flags.contains(&"--enable-v4l2-request".to_string()));
        }
    }

    #[test]
    fn the_runpath_check_names_every_object_that_would_ship_unresolvable() {
        let libdir = "/opt/ffmpeg-rk/lib";
        let staged = [
            ("ffmpeg".to_string(), Some(libdir.to_string())),
            // The shape the deb shipped before the flag existed.
            ("ffprobe".to_string(), None),
            // Present but pointing elsewhere: a tag alone is not the guarantee.
            (
                "libavcodec.so.62".to_string(),
                Some("/usr/lib/aarch64-linux-gnu".to_string()),
            ),
            // A prefix of the libdir is not the libdir.
            (
                "libavutil.so.60".to_string(),
                Some("/opt/ffmpeg-rk/lib-old".to_string()),
            ),
            // One entry among several still satisfies it.
            (
                "libavfilter.so.11".to_string(),
                Some(format!("/some/other/dir:{libdir}")),
            ),
        ];
        assert_eq!(
            objects_missing_runpath(&staged, libdir),
            vec![
                "ffprobe (no RUNPATH)".to_string(),
                "libavcodec.so.62 (/usr/lib/aarch64-linux-gnu)".to_string(),
                "libavutil.so.60 (/opt/ffmpeg-rk/lib-old)".to_string(),
            ]
        );
        // A fully-linked tree is silent.
        assert!(objects_missing_runpath(
            &[("ffmpeg".to_string(), Some(libdir.to_string()))],
            libdir
        )
        .is_empty());
    }

    #[test]
    fn every_build_bakes_the_install_libdir_into_its_objects() {
        // Unconditional, and named exactly: the deb is unusable without it, and an
        // ld.so.conf.d drop-in is not an alternative — these sonames collide with the
        // suite's own FFmpeg, so a global search path would shadow it system-wide.
        let want = "--extra-ldflags=-Wl,-rpath,/opt/ffmpeg-rk/lib".to_string();
        for trees in [all_trees(), rga_only(), no_trees()] {
            assert!(configure_flags(&trees).contains(&want));
        }
        // The flag must name the same prefix the tree installs to, or the objects
        // would resolve against a directory the deb never ships.
        assert!(rpath_ldflag().ends_with(&format!("{INSTALL_PREFIX}/lib")));
    }

    #[test]
    fn the_build_dep_debs_track_the_same_trees_as_the_configure_flags() {
        // The two must not disagree: ffmpeg build-depends on a userspace package
        // exactly when it is configured against that library.
        assert_eq!(
            userspace_dep_prefixes(&all_trees()),
            vec![
                "librockchip-mpp1_",
                "librockchip-mpp-dev_",
                "librga2_",
                "librga-dev_"
            ]
        );
        assert_eq!(
            userspace_dep_prefixes(&rga_only()),
            vec!["librga2_", "librga-dev_"]
        );
        assert!(userspace_dep_prefixes(&no_trees()).is_empty());
    }

    /// The packages the build root layers in track the same trees the configure flags
    /// and the deb prefixes do — the three must not disagree about which userspace this
    /// build has — and each tree contributes **both** its runtime library and its `-dev`.
    ///
    /// Naming only the `-dev` half is the shape that fails: `librga-dev` declares no
    /// dependencies, so nothing would pull `librga2` in, and `./configure` would fail
    /// its link probe on a build root holding the headers without the library.
    #[test]
    fn the_layered_packages_track_the_socs_declared_trees() {
        assert_eq!(
            userspace_layer_packages(&all_trees()),
            vec![
                "librockchip-mpp1",
                "librockchip-mpp-dev",
                "librga2",
                "librga-dev"
            ]
        );
        assert_eq!(
            userspace_layer_packages(&rga_only()),
            vec!["librga2", "librga-dev"]
        );
        assert!(userspace_layer_packages(&no_trees()).is_empty());
        // The set the layer installs is the set the old path installed by path, so the
        // two cannot disagree about what a build root holds.
        assert_eq!(
            userspace_layer_packages(&all_trees()).len(),
            userspace_dep_prefixes(&all_trees()).len()
        );
        // The runtime half, which the produced deb must depend on.
        assert_eq!(
            required_runtime_depends(&all_trees()),
            vec!["librockchip-mpp1", "librga2"]
        );
        assert_eq!(required_runtime_depends(&rga_only()), vec!["librga2"]);
        assert!(required_runtime_depends(&no_trees()).is_empty());
    }

    /// A dropped userspace dependency fails the stage rather than shipping.
    ///
    /// This is the regression the layered build root could introduce and that no exit
    /// status would report: the packages arrive through the pool's resolution instead of
    /// `apt-get install /path/to.deb`, and if their `shlibs` are not where
    /// `dpkg-shlibdeps` looks it emits a `Depends` without them, producing a deb that
    /// installs cleanly and breaks on the board.
    #[test]
    fn a_depends_missing_a_userspace_lib_is_refused() {
        // The real resolved value from a good build.
        let good = "libass9 (>= 1:0.15.0), libc6 (>= 2.38), librga2, librockchip-mpp1, \
                    libx265-216 (>= 4.2)";
        assert!(assert_userspace_depends(good, &all_trees()).is_ok());
        // Order is irrelevant, and a version relation still names its package.
        assert!(assert_userspace_depends(
            "librockchip-mpp1 (>= 1.5.0), libc6, librga2 (>= 2.2.0)",
            &all_trees()
        )
        .is_ok());

        // rkrga dropped: the exact silent failure correction 3 describes.
        let dropped = "libass9 (>= 1:0.15.0), libc6 (>= 2.38), librockchip-mpp1";
        let err = assert_userspace_depends(dropped, &all_trees()).unwrap_err();
        match &err {
            EngineError::ArtifactMissing { what, .. } => {
                assert!(what.contains("librga2"), "{what}");
                // The error carries what was resolved, so the failure is diagnosable
                // without re-running the build.
                assert!(what.contains(dropped), "{what}");
            }
            other => panic!("expected ArtifactMissing, got {other:?}"),
        }

        // A prefix must not satisfy the requirement: `librga2-dev` is not `librga2`.
        assert!(
            assert_userspace_depends("libc6, librga2-dev, librockchip-mpp1", &all_trees()).is_err()
        );
        // An empty Depends fails rather than passing vacuously.
        assert!(assert_userspace_depends("", &all_trees()).is_err());

        // A SoC that builds no vendor userspace requires none of it, so any Depends
        // satisfies the check — including one naming no userspace package at all.
        assert!(assert_userspace_depends("libc6 (>= 2.38)", &no_trees()).is_ok());
        // And an RGA-only stack demands only librga2.
        assert!(assert_userspace_depends("libc6, librga2", &rga_only()).is_ok());
        assert!(assert_userspace_depends("libc6, librockchip-mpp1", &rga_only()).is_err());
    }

    #[test]
    fn a_stack_with_no_vendor_userspace_demands_no_debs() {
        // The userspace stage produces no output dir at all in this shape, so the
        // dependency check must not treat its absence as a forgotten stage.
        let tmp = tempfile::tempdir().unwrap();
        let missing = tmp.path().join("never-created");
        assert!(required_userspace_debs(&missing, &no_trees())
            .unwrap()
            .is_empty());
    }

    fn lock_with(base_commit: &str, patches_commit: &str) -> Lock {
        let git = |c: &str| GitPin {
            source: "s".into(),
            reference: "r".into(),
            commit: c.into(),
        };
        Lock {
            kernel: Some(KernelPin {
                id: "k".into(),
                source: "ks".into(),
                reference: "v".into(),
                commit: "kc".into(),
            }),
            patches: Some(PatchesPin {
                series: vec!["rk3588-accel".into()],
                source: "ps".into(),
                reference: "main".into(),
                commit: patches_commit.into(),
            }),
            uboot: Some(UbootPin {
                source: "us".into(),
                reference: "v".into(),
                commit: "uc".into(),
            }),
            uboot_patches: None,
            userspace: Some(UserspacePins {
                mpp: Some(git("m")),
                librga: Some(git("r")),
                libmali: Some(git("l")),
            }),
            ffmpeg: Some(FfmpegPins {
                base: git(base_commit),
                rockchip: Some(git("rk")),
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

    #[test]
    fn unreadable_scan_dir_is_an_error_not_an_empty_scan() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        // An absent directory is a legitimate empty scan...
        assert!(read_dir_entries(&tmp.path().join("missing"))
            .unwrap()
            .is_empty());
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
        let base = sig("bc1", "pc1", SeriesIdentity::Pinned);
        assert_eq!(base, sig("bc1", "pc1", SeriesIdentity::Pinned));
        // A base-tree bump or a patch-pin bump each invalidate the reused tree.
        assert_ne!(base, sig("bc2", "pc1", SeriesIdentity::Pinned));
        assert_ne!(base, sig("bc1", "pc2", SeriesIdentity::Pinned));
        // Co-dev mode splits the key; a co-dev content change restamps.
        let empty: Vec<String> = vec![];
        assert_ne!(base, sig("bc1", "pc1", SeriesIdentity::Dev(&empty)));
        let fp1 = vec!["media-accel/ffmpeg/0001.patch=aaa".to_string()];
        let fp2 = vec!["media-accel/ffmpeg/0001.patch=bbb".to_string()];
        assert_ne!(
            sig("bc1", "pc1", SeriesIdentity::Dev(&fp1)),
            sig("bc1", "pc1", SeriesIdentity::Dev(&fp2))
        );
    }

    /// A stand-in for [`BuildEnv::sandbox_id`](crate::build::BuildEnv::sandbox_id) in
    /// the signature tests.
    const SANDBOX: &str = "http://deb.debian.org/debian | qemu-aarch64 version 9.2.0";

    #[test]
    fn output_manifest_covers_tree_arch_and_suite() {
        let sig = |lock: &Lock, arch: &str| {
            let ff = lock.ffmpeg.as_ref().unwrap();
            let us = lock.userspace.as_ref().unwrap();
            output_manifest(
                lock,
                ff,
                us,
                arch,
                SANDBOX,
                false,
                SeriesIdentity::Pinned,
                SeriesIdentity::Pinned,
            )
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
        // The suite names the sandbox's userland, so it splits the key...
        let mut sid = lock_with("bc1", "pc1");
        sid.rootfs.as_mut().unwrap().suite = "sid".into();
        assert_ne!(base, sig(&sid, "arm64"));
        // ...and so does the sandbox instance within one suite: a snapshot-pinned
        // userland compiles this deb with a different gcc than the live mirror's.
        let lock = lock_with("bc1", "pc1");
        assert_ne!(
            base,
            output_manifest(
                &lock,
                lock.ffmpeg.as_ref().unwrap(),
                lock.userspace.as_ref().unwrap(),
                "arm64",
                "https://snapshot.debian.org/archive/debian/20260628T083000Z/",
                false,
                SeriesIdentity::Pinned,
                SeriesIdentity::Pinned,
            )
            .signature
        );
        // Co-dev mode never shares an output entry with a pinned build.
        let dev_lock = lock_with("bc1", "pc1");
        assert_ne!(
            base,
            output_manifest(
                &dev_lock,
                dev_lock.ffmpeg.as_ref().unwrap(),
                dev_lock.userspace.as_ref().unwrap(),
                "arm64",
                SANDBOX,
                false,
                SeriesIdentity::Dev(&[]),
                SeriesIdentity::Dev(&[]),
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
            output_manifest(
                lock,
                ff,
                us,
                "arm64",
                SANDBOX,
                false,
                SeriesIdentity::Pinned,
                SeriesIdentity::Pinned,
            )
            .signature
            .clone()
        };
        let base = sig(&lock_with("bc1", "pc1"));
        // An MPP pin bump (ffmpeg base/patch/suite/arch unchanged) splits the key.
        let mut mpp_bump = lock_with("bc1", "pc1");
        mpp_bump
            .userspace
            .as_mut()
            .unwrap()
            .mpp
            .as_mut()
            .unwrap()
            .commit = "m2".into();
        assert_ne!(base, sig(&mpp_bump));
        // An RGA pin bump likewise splits it.
        let mut rga_bump = lock_with("bc1", "pc1");
        rga_bump
            .userspace
            .as_mut()
            .unwrap()
            .librga
            .as_mut()
            .unwrap()
            .commit = "r2".into();
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
        let c = control_text(
            "arm64",
            "8.1-19-g942418aa06",
            "librockchip-mpp1, librga2, libc6",
        );
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
        let debs = required_userspace_debs(dir, &all_trees()).unwrap();
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
        let err = required_userspace_debs(dir, &all_trees()).unwrap_err();
        match err {
            EngineError::ArtifactMissing { what, .. } => assert!(what.contains("-dev")),
            other => panic!("expected ArtifactMissing, got {other:?}"),
        }
        // A missing dir is also a clear error, not an I/O panic.
        assert!(matches!(
            required_userspace_debs(&dir.join("nope"), &all_trees()).unwrap_err(),
            EngineError::ArtifactMissing { .. }
        ));
    }
}
