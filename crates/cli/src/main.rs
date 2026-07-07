//! boot2deb CLI — a thin client over the config core and the engine.
//!
//! Subcommands: `list-devices`, `list-recipes`, `resolve`, and `doctor` (config
//! inspection + host preflight); `update` (resolve upstream refs into the lock);
//! `verify-patches` and `verify-config` (the patch and kernel-config gates);
//! `patch import` (fetch + normalize + slot a patch into a profile); `build`
//! (drive the compile / rootfs / image pipeline from the lock); `why-rebuild`
//! (explain, offline, which compile nodes the next build reuses vs. rebuilds);
//! and `clean` (remove a recipe's build scratch).

use boot2deb_core::lock::{SnapshotMode, SnapshotPin};
use boot2deb_core::mbox::{self, ImportMeta};
use boot2deb_core::model::{BootMethod, Layout, Overrides, ResolvedBuild};
use boot2deb_core::profile::{derive_prefix, Scope};
use boot2deb_core::{load_profile, resolve_device, resolve_recipe, ConfigRoot};
use boot2deb_engine::build::{ffmpeg, kernel, uboot, userspace, BuildEnv};
use boot2deb_engine::checks::CheckStatus;
use boot2deb_engine::event::{Event, Step, Stream};
use boot2deb_engine::image::{self, ImageOutput};
use boot2deb_engine::rootfs::{self, MmdebstrapRootfs, Rootfs};
use boot2deb_engine::sandbox::{BuildSandbox, NativeSandbox, RootlessSandbox};
use boot2deb_engine::debstore::DebStore;
use boot2deb_engine::{
    extradebs, kconfig, patchfetch, patches, patchimport, pins, plan, sources, EngineError,
    EventSink,
};
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

#[derive(Parser)]
#[command(name = "boot2deb", version, about = "Device → Debian builder")]
struct Cli {
    /// Config root (the boot2deb repo dir holding devices/, socs/, ...).
    #[arg(long, global = true, default_value = ".")]
    root: PathBuf,

    /// Out-of-tree overlay directory holding your own devices/, socs/, kernels/,
    /// features/, or recipes/ files. Repeatable; later overlays win, and any
    /// overlay wins over the shipped root — a same-named layer is deep-merged
    /// last-wins, a new-named one adds a target. Fragments/blobs/overlay trees an
    /// overlay ships are resolved along the same path.
    #[arg(long = "overlay", global = true)]
    overlay: Vec<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// List available devices.
    ListDevices,
    /// List available recipes.
    ListRecipes,
    /// Resolve a device or recipe to a complete build (no build work).
    Resolve {
        /// Device name (e.g. turing-rk1) or recipe name (e.g. turing-rk1-forky).
        target: String,
        #[command(flatten)]
        overrides: OverrideArgs,
    },
    /// Report host arch/OS and whether a target build is cross-arch.
    Doctor {
        /// Optional device/recipe to report cross-arch status against.
        target: Option<String>,
        #[command(flatten)]
        overrides: OverrideArgs,
    },
    /// Resolve upstream refs + hash blobs and write the recipe's `.lock`.
    /// The sole path that consults upstream; `build` reads only the lock.
    Update {
        /// Recipe to resolve (e.g. turing-rk1-forky).
        recipe: String,
        #[command(flatten)]
        args: UpdateArgs,
    },
    /// Dry-run the locked patch series against source checkouts with
    /// `git am --3way`, hard-erroring on the first patch that does not apply.
    VerifyPatches {
        /// Recipe whose lock names the kernel ref + patch profile.
        recipe: String,
        #[command(flatten)]
        args: VerifyArgs,
    },
    /// Generate the kernel `.config` (base defconfig + fragments via
    /// `merge_config.sh`) on a patched kernel tree; with a reference config,
    /// additionally check byte-identical `CONFIG_*` parity against it.
    VerifyConfig {
        /// Recipe whose resolved kernel names the base defconfig + fragments.
        recipe: String,
        #[command(flatten)]
        args: ConfigArgs,
    },
    /// Probe each locked source pin against its *configured* upstream URL and
    /// report whether it is a durable tag, an ephemeral branch, or ORPHANED (not
    /// re-fetchable) — the source-pin durability survey as a command.
    /// Read-only: `git ls-remote` plus a timeout-bounded ancestry check, no build,
    /// no checkout, no hardware.
    VerifySources {
        /// Recipe whose lock names the source pins (e.g. turing-rk1-forky).
        recipe: String,
    },
    /// Curate the patch series. Subcommand: `import`.
    Patch {
        #[command(subcommand)]
        action: PatchAction,
    },
    /// Drive the build stages (kernel, u-boot, userspace, ffmpeg, and the disk
    /// image) from the recipe's lock, streaming the structured build event stream.
    /// Reads only the lock for pinned sources; the lock-independent
    /// image axes (`--layout`, `--image-size`) are overridable, while re-pinning a
    /// source axis (kernel/suite/features/boot-method) is `update`'s job.
    Build {
        /// Recipe to build (e.g. turing-rk1-forky); its `.lock` must exist.
        recipe: String,
        #[command(flatten)]
        args: BuildArgs,
    },
    /// Explain, per compile node, whether the next `build` will reuse or rebuild its
    /// cached source tree — and which pinned inputs changed if it will rebuild.
    /// Offline: reads the lock and the on-disk build stamps, runs no build.
    WhyRebuild {
        /// Recipe to inspect (e.g. turing-rk1-forky); its `.lock` must exist.
        recipe: String,
        #[command(flatten)]
        args: WhyRebuildArgs,
    },
    /// Remove a recipe's build scratch (clones, sandbox, rootfs cache, artifacts)
    /// under its work dir, to reclaim disk or force a clean rebuild.
    Clean {
        /// Recipe whose build scratch to remove (e.g. turing-rk1-forky).
        recipe: String,
        #[command(flatten)]
        args: CleanArgs,
    },
}

/// Which stage(s) `build` runs.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
enum StageArg {
    /// The full pipeline: kernel, u-boot, userspace, ffmpeg, rootfs, then the
    /// disk image — a complete device image from the lock.
    All,
    /// Only the kernel `.deb`s.
    Kernel,
    /// Only the u-boot boot payloads.
    Uboot,
    /// Only the userspace media-accel `.deb`s (MPP/RGA).
    Userspace,
    /// Only the ffmpeg-rk `.deb` (build the userspace stage first).
    Ffmpeg,
    /// Only the rootfs tarball + solved manifest. Installs the built
    /// `.deb`s from the output dir, so run the compile stages first.
    Rootfs,
    /// Only the disk image. Uses the rootfs tar from `--stage rootfs` (or
    /// `--rootfs-tar`) plus the u-boot payloads (run `--stage uboot` first).
    Image,
}

#[derive(Args)]
struct BuildArgs {
    /// Which stage(s) to run.
    #[arg(long, value_enum, default_value_t = StageArg::All)]
    stage: StageArg,
    /// Kernel clone source (git URL or local path); default: the kernel
    /// definition's source URL. A local clone (e.g. ../linux) is far faster.
    #[arg(long)]
    kernel_src: Option<String>,
    /// u-boot clone source (git URL or local path); default: the boot method's
    /// `uboot_source`.
    #[arg(long)]
    uboot_src: Option<String>,
    /// MPP clone source (git URL or local path); default: the SoC layer's
    /// `userspace.mpp` URL. A local checkout (e.g. ../mpp-rockchip) is far faster.
    #[arg(long)]
    mpp_src: Option<String>,
    /// librga clone source; default: the SoC layer's `userspace.librga` URL.
    #[arg(long)]
    librga_src: Option<String>,
    /// libmali clone source; default: the SoC layer's `userspace.libmali` URL.
    #[arg(long)]
    libmali_src: Option<String>,
    /// ffmpeg base (Kwiboo) clone source; default: the SoC layer's `ffmpeg.base`
    /// URL. A local checkout makes the fetch near-instant.
    #[arg(long)]
    ffmpeg_base_src: Option<String>,
    /// Also build the Mali userspace (off by default — unused on a headless box).
    #[arg(long)]
    build_libmali: bool,
    /// `patches` repo checkout the series is read from. Omit to use `../patches`
    /// (if present, with the lock's `patches.commit` enforced), else auto-fetch the
    /// series at the pinned commit from `--patches-url`/the kernel's `patches_url`.
    /// Pass an explicit path to co-develop the series from a working checkout,
    /// which downgrades a pin mismatch to a loud warning.
    #[arg(long)]
    patches_path: Option<PathBuf>,
    /// Clone URL for auto-fetching the `patches` series when no local checkout is
    /// present; default: the kernel definition's `patches_url`. The series is
    /// fetched at the lock's `patches.commit` into a durable cache and its pin
    /// enforced. Ignored when `--patches-path` or `../patches` supplies a checkout.
    #[arg(long)]
    patches_url: Option<String>,
    /// Vendored rkbin blob directory (default: blobs/SOC under the config root).
    #[arg(long)]
    blobs_dir: Option<PathBuf>,
    /// Debian archive keyring for the cross sandbox bootstrap (default: the
    /// vendored blobs/keyrings/debian-archive-keyring.gpg; omit on a Debian host
    /// to use its apt trust store).
    #[arg(long)]
    keyring: Option<PathBuf>,
    /// Trust an overlay-shipped copy of the archive keyring. By default an overlay
    /// that ships blobs/keyrings/debian-archive-keyring.gpg is refused as a
    /// trust-anchor swap (TRUST-1); this opts into the overlay's copy explicitly.
    #[arg(long)]
    unsafe_overlay_keyring: bool,
    /// Scratch dir for clones + builds (default: build/RECIPE).
    #[arg(long)]
    work_dir: Option<PathBuf>,
    /// Where produced artifacts are staged (default: WORK_DIR/artifacts).
    #[arg(long)]
    out_dir: Option<PathBuf>,
    /// `make -j` parallelism (default: host available parallelism).
    #[arg(long)]
    jobs: Option<usize>,
    /// Rootfs `tar` archive for the image stage. Optional: `--stage image`
    /// otherwise uses the tar the rootfs stage produced (auto-discovered in the
    /// output dir), so this is only needed to point at a tar built elsewhere.
    #[arg(long)]
    rootfs_tar: Option<PathBuf>,
    /// ext4 volume label / GPT partition name for the image rootfs.
    #[arg(long, default_value = "rootfs")]
    rootfs_label: String,
    /// Skip `.xz` compression of the finished image(s).
    #[arg(long)]
    no_compress: bool,
    /// Keep the raw `.img` after compressing it (default: delete it once the `.xz`
    /// is written, since it is derivable and the largest artifact).
    #[arg(long)]
    keep_raw: bool,
    /// Image layout override (`combined` | `split`); default: the recipe/device
    /// layout. Lock-independent — it changes only image packaging, not any pinned
    /// source, so it is safe to set against an existing lock.
    #[arg(long, value_parser = parse_layout)]
    layout: Option<Layout>,
    /// Image-size override (e.g. `4G`); default: the recipe/device `image_size`.
    /// Lock-independent — it changes only image geometry, not any pinned source.
    #[arg(long = "image-size")]
    image_size: Option<String>,
    /// Snapshot activation for the rootfs bootstrap: `off` (live mirror),
    /// `fallback` (live first, `snapshot.debian.org` fills 404s), `pin` (snapshot
    /// only, fully deterministic). Default: the lock's captured mode (off if none).
    /// `fallback`/`pin` need a captured snapshot (`--save-snapshot`).
    #[arg(long, value_parser = parse_snapshot_mode)]
    snapshot: Option<SnapshotMode>,
    /// After a successful build, capture the current UTC time as a
    /// `snapshot.debian.org` timestamp into the lock (dormant, `mode = off`), so the
    /// solved versions stay fetchable after they rotate off the live mirror; a later
    /// build activates it with `--snapshot fallback|pin`.
    #[arg(long)]
    save_snapshot: bool,
    /// After the rootfs stage, commit the solved package manifest beside the lock
    /// and record its sha256 in the lock (`[rootfs].manifest_sha256`) — the
    /// reproducibility pin later builds verify a fresh solve against.
    #[arg(long)]
    save_manifest: bool,
    /// Downgrade a solved-manifest drift from the committed pin to a warning instead
    /// of a hard error — for co-development or a knowingly-moved mirror. Re-pin
    /// deliberately with `--save-manifest`.
    #[arg(long)]
    allow_manifest_drift: bool,
    /// Ignore a rootfs cache hit and re-bootstrap, refreshing the stored tree.
    /// The cheap `--simulate` solve still runs — the rootfs cache keys on
    /// the *solved* set, so a moved mirror already rebuilds automatically; this is
    /// the manual escape when you want a clean bootstrap regardless.
    #[arg(long)]
    refresh_rootfs: bool,
    /// Disable the Tier-2 artifact cache: always recompile the kernel /
    /// u-boot / userspace / ffmpeg `.deb`s instead of restoring a stored output on a
    /// signature hit, and do not store this build's outputs. The durable store at
    /// `<root>/cache/artifacts` is left untouched.
    #[arg(long)]
    no_artifact_cache: bool,
}

#[derive(Args)]
struct WhyRebuildArgs {
    /// Build scratch dir to inspect (default: build/RECIPE) — must match the dir the
    /// build used, since the stamps live there.
    #[arg(long)]
    work_dir: Option<PathBuf>,
    /// The build being reasoned about used an explicit `--patches-path` co-dev
    /// checkout (folded into the kernel/u-boot/ffmpeg signatures). Pass the same
    /// value so the prediction matches what that build would reuse.
    #[arg(long)]
    patches_path: Option<PathBuf>,
    /// Include the optional libmali userspace node (built only with
    /// `--build-libmali`).
    #[arg(long)]
    build_libmali: bool,
}

#[derive(Args)]
struct CleanArgs {
    /// Build scratch dir to clean (default: build/RECIPE).
    #[arg(long)]
    work_dir: Option<PathBuf>,
    /// Remove only the rootfs early-cutoff cache (WORK_DIR/cache), keeping the
    /// compiled source trees and artifacts.
    #[arg(long)]
    cache: bool,
    /// Remove only the bootstrapped cross-build sandbox (WORK_DIR/sandbox) — the
    /// largest single reclaimable tree.
    #[arg(long)]
    sandbox: bool,
    /// Remove the durable Tier-2 artifact store (`<root>/cache/artifacts`).
    /// Unlike the other selectors this store is shared across recipes, so this
    /// clears cached outputs for *every* recipe, not just this one.
    #[arg(long)]
    artifacts: bool,
    /// Show what would be removed (with sizes) without removing anything.
    #[arg(long)]
    dry_run: bool,
}

#[derive(Args)]
struct UpdateArgs {
    /// Kernel ref to pin, resolved to a commit (e.g. v7.1.1). Auto-resolving a
    /// kernel `track` to its latest tag is a later refinement.
    #[arg(long)]
    kernel_ref: String,
    /// u-boot ref to pin (default: the boot-method's `uboot_ref`).
    #[arg(long)]
    uboot_ref: Option<String>,
    /// MPP source ref to pin (default: the SoC layer's `userspace.mpp` ref).
    #[arg(long)]
    mpp_ref: Option<String>,
    /// librga source ref to pin (default: the SoC layer's `userspace.librga`).
    #[arg(long)]
    librga_ref: Option<String>,
    /// libmali source ref to pin (default: the SoC layer's `userspace.libmali`).
    #[arg(long)]
    libmali_ref: Option<String>,
    /// ffmpeg base (V4L2) ref to pin (default: the SoC layer's `ffmpeg.base`).
    #[arg(long)]
    ffmpeg_base_ref: Option<String>,
    /// ffmpeg Rockchip provenance-tree ref to pin (default: the SoC layer's
    /// `ffmpeg.rockchip`). Recorded as the graft's provenance; not fetched.
    #[arg(long)]
    ffmpeg_rockchip_ref: Option<String>,
    /// `patches` repo checkout whose HEAD pins the series.
    #[arg(long, default_value = "../patches")]
    patches_path: PathBuf,
    /// Vendored rkbin blob directory (default: blobs/SOC under the config root).
    #[arg(long)]
    blobs_dir: Option<PathBuf>,
    /// Name recorded for the solved package manifest the rootfs stage writes
    /// (default: RECIPE.pkgs.lock).
    #[arg(long)]
    rootfs_manifest: Option<String>,
}

#[derive(Args)]
struct VerifyArgs {
    /// Kernel checkout to verify the kernel series against.
    #[arg(long)]
    kernel_path: PathBuf,
    /// ffmpeg checkout to also verify the ffmpeg series against.
    #[arg(long)]
    ffmpeg_path: Option<PathBuf>,
    /// Userspace (MPP/RGA) checkout to also verify the userspace series against.
    #[arg(long)]
    userspace_path: Option<PathBuf>,
    /// `patches` repo checkout the profile + patches are read from.
    #[arg(long, default_value = "../patches")]
    patches_path: PathBuf,
}

#[derive(Args)]
struct ConfigArgs {
    /// Kernel checkout (at the locked ref, patch series applied) to configure.
    #[arg(long)]
    kernel_path: PathBuf,
    /// Reference `.config` to check byte-identical `CONFIG_*` parity against. Omit
    /// for a clean-merge check only.
    #[arg(long)]
    reference_config: Option<PathBuf>,
    /// Directory for the two out-of-tree config builds (default: a temp dir).
    #[arg(long)]
    work_dir: Option<PathBuf>,
}

#[derive(Subcommand)]
enum PatchAction {
    /// Fetch a patch (patchwork/mbox URL, a file, or `-` for stdin), normalize it to
    /// canonical `git am`-ready mbox, slot it into a profile's scope at a position,
    /// and — with `--verify-tree` — dry-run `git am`-verify the resulting series.
    Import {
        /// Patch source: an `http(s)://` URL (a patchwork mbox), a local file path,
        /// or `-` to read from stdin.
        source: String,
        #[command(flatten)]
        args: PatchImportArgs,
    },
}

#[derive(Args)]
struct PatchImportArgs {
    /// Profile to slot the patch into (e.g. rk3588-accel) — names
    /// `profiles/<name>/profile.toml` in the patches repo.
    #[arg(long)]
    profile: String,
    /// Which source tree's ordered list to insert into.
    #[arg(long, value_parser = parse_scope)]
    scope: Scope,
    /// 1-based position in the scope list to insert at (default: append to the end).
    #[arg(long)]
    position: Option<usize>,
    /// Repo subdirectory to write the patch into (default: `media-accel/<scope>`).
    /// Use e.g. `rocket` to target the NPU scope of the kernel list.
    #[arg(long)]
    dest_dir: Option<String>,
    /// Filename slug override (default: a kebab-case slug of the subject). The
    /// written file is `<dest-dir>/<prefix>-<slug>.patch`.
    #[arg(long)]
    name: Option<String>,
    /// Explicit repo-relative destination label, overriding the derived
    /// dir/prefix/slug entirely (e.g. `media-accel/kernel/045-fix.patch`).
    #[arg(long = "as")]
    label: Option<String>,
    /// `From:` author for a synthesized header (bare diff / `git show` fallback).
    #[arg(long, default_value = "boot2deb import <import@boot2deb>")]
    author: String,
    /// Subject override — the title for a bare diff carrying none, or an override
    /// for `git show`. Ignored for an already-formatted mbox.
    #[arg(long)]
    subject: Option<String>,
    /// DEP-3 `Origin:` provenance trailer to add to the commit message.
    #[arg(long)]
    origin: Option<String>,
    /// `patches` repo checkout to write into (default: `../patches`).
    #[arg(long, default_value = "../patches")]
    patches_path: PathBuf,
    /// Source checkout to dry-run `git am`-verify the spliced series against.
    /// Omit to import without verifying (a warning is printed).
    #[arg(long)]
    verify_tree: Option<PathBuf>,
    /// Overwrite the destination file if it already exists (default: refuse).
    #[arg(long)]
    force: bool,
}

#[derive(Args, Default)]
struct OverrideArgs {
    #[arg(long)]
    kernel: Option<String>,
    #[arg(long)]
    suite: Option<String>,
    #[arg(long, value_parser = parse_layout)]
    layout: Option<Layout>,
    #[arg(long = "boot-method", value_parser = parse_boot_method)]
    boot_method: Option<BootMethod>,
    /// Rootfs feature add-in, repeatable (`--feature media-accel-rockchip`). When
    /// any is given, replaces the recipe's feature list.
    #[arg(long = "feature")]
    features: Vec<String>,
    #[arg(long = "image-size")]
    image_size: Option<String>,
}

impl From<OverrideArgs> for Overrides {
    fn from(a: OverrideArgs) -> Self {
        Overrides {
            kernel: a.kernel,
            suite: a.suite,
            layout: a.layout,
            boot_method: a.boot_method,
            features: (!a.features.is_empty()).then_some(a.features),
            image_size: a.image_size,
        }
    }
}

// clap value parsing reuses the model's FromStr (kebab-case).
fn parse_layout(s: &str) -> Result<Layout, String> {
    s.parse()
}
fn parse_boot_method(s: &str) -> Result<BootMethod, String> {
    s.parse()
}
/// Parse the `--snapshot` activation mode; matches the lock's serialized form.
fn parse_snapshot_mode(s: &str) -> Result<SnapshotMode, String> {
    match s {
        "off" => Ok(SnapshotMode::Off),
        "fallback" => Ok(SnapshotMode::Fallback),
        "pin" => Ok(SnapshotMode::Pin),
        other => Err(format!("unknown snapshot mode '{other}' (expected off|fallback|pin)")),
    }
}
/// Parse the `patch import --scope` value; reuses the model's `FromStr`.
fn parse_scope(s: &str) -> Result<Scope, String> {
    s.parse()
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    let root = ConfigRoot::with_overlays(cli.root, cli.overlay);
    match run(&root, cli.command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(root: &ConfigRoot, command: Command) -> Result<(), Box<dyn std::error::Error>> {
    match command {
        Command::ListDevices => {
            for name in root.list("devices")? {
                match root.device(&name) {
                    Ok(d) => println!("{name:<20} {}", d.description),
                    Err(_) => println!("{name:<20} (unreadable)"),
                }
            }
        }
        Command::ListRecipes => {
            for name in root.list("recipes")? {
                match root.recipe(&name) {
                    Ok(r) => println!("{name:<24} device={}", r.device),
                    Err(_) => println!("{name:<24} (unreadable)"),
                }
            }
        }
        Command::Resolve { target, overrides } => {
            let build = resolve(root, &target, overrides.into())?;
            print_build(&build);
        }
        Command::Doctor { target, overrides } => {
            doctor(root, target, overrides.into())?;
        }
        Command::Update { recipe, args } => {
            update(root, &recipe, args)?;
        }
        Command::VerifyPatches { recipe, args } => {
            verify_patches(root, &recipe, args)?;
        }
        Command::VerifyConfig { recipe, args } => {
            verify_config(root, &recipe, args)?;
        }
        Command::VerifySources { recipe } => {
            verify_sources(root, &recipe)?;
        }
        Command::Patch { action } => match action {
            PatchAction::Import { source, args } => {
                patch_import(&source, args)?;
            }
        },
        Command::Build { recipe, args } => {
            build(root, &recipe, args)?;
        }
        Command::WhyRebuild { recipe, args } => {
            why_rebuild(root, &recipe, args)?;
        }
        Command::Clean { recipe, args } => {
            clean(root, &recipe, args)?;
        }
    }
    Ok(())
}

/// Drive the compile stages from the recipe's lock, printing the event stream.
fn build(root: &ConfigRoot, recipe: &str, args: BuildArgs) -> Result<(), Box<dyn std::error::Error>> {
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
    // or the build would mix new resolved axes with stale pins (CFG-2).
    boot2deb_engine::pins::check_lock_consistency(&lock, &resolved)?;
    // Validate the cheap pure config invariants (image geometry + kernel-fragment
    // existence) up front, so a bad layout or a missing fragment fails before any stage
    // runs rather than at the image/kernel node after the pipeline (CFG-4).
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
    let out_dir = absolutize(args.out_dir.unwrap_or_else(|| work_dir.join("artifacts")));
    // Sweep stale `.partial` staging temps a hard-killed prior run may have left in the
    // artifact dir before the compile stages publish into it (ATOM-3). No-op when the
    // dir does not exist yet.
    boot2deb_engine::gc::sweep_stale_temps(&out_dir);
    let blobs_dir = args.blobs_dir.clone().unwrap_or_else(|| {
        let rel = format!("blobs/{}", resolved.soc.as_str());
        root.find_asset(&rel)
            .unwrap_or_else(|| root.path().join(rel))
    });
    let kernel_src = match args.kernel_src {
        Some(s) => s,
        None => pins::kernel_source_url(&resolved.kernel.source)?,
    };
    let uboot_src = args.uboot_src.unwrap_or_else(|| resolved.uboot_source.clone());
    let mpp_src = args.mpp_src.unwrap_or_else(|| resolved.userspace.mpp.git.clone());
    let librga_src = args
        .librga_src
        .unwrap_or_else(|| resolved.userspace.librga.git.clone());
    let libmali_src = args
        .libmali_src
        .unwrap_or_else(|| resolved.userspace.libmali.git.clone());
    let ffmpeg_base_src = args
        .ffmpeg_base_src
        .unwrap_or_else(|| resolved.ffmpeg.base.git.clone());

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
    println!(
        "building {recipe} (arch {}, {} build, work {})",
        resolved.arch,
        if pf.cross { "cross" } else { "native" },
        work_dir.display()
    );

    // Debian archive keyring for mmdebstrap — the cross sandbox and the rootfs
    // bootstrap: the explicit flag, else the vendored keyring resolved as a
    // non-overlayable trust anchor (an overlay copy is a fail-closed swap, TRUST-1),
    // else None (the host apt trust store, only viable on a Debian host).
    let keyring = match args.keyring.clone() {
        Some(explicit) => Some(explicit),
        None => root.find_trust_anchor(
            "blobs/keyrings/debian-archive-keyring.gpg",
            args.unsafe_overlay_keyring,
        )?,
    };

    // The userspace/ffmpeg stages compile arm64 .debs in a sandbox: the host
    // directly when native, else a rootless arm64 userland. Bootstrapped
    // lazily on first use under WORK_DIR/sandbox.
    let sandbox: Box<dyn BuildSandbox> = if pf.cross {
        let rootfs = work_dir
            .join("sandbox")
            .join(format!("{}-{}", resolved.arch, resolved.suite));
        Box::new(RootlessSandbox::new(
            rootfs,
            resolved.suite.clone(),
            resolved.arch.as_str().to_string(),
            keyring.clone(),
        ))
    } else {
        Box::new(NativeSandbox)
    };

    let sink = |e: Event| print_event(&e);

    // Resolve the patches source only for the stages that apply the series
    // (kernel/u-boot/userspace/ffmpeg): an explicit --patches-path co-dev checkout,
    // else the default ../patches if present, else auto-fetch at the pinned commit.
    // The userspace stage applies the MPP CMA fix to the MPP tree. A
    // rootfs/image-only build needs no patches, so it never fetches.
    let needs_patches = matches!(
        args.stage,
        StageArg::All | StageArg::Kernel | StageArg::Uboot | StageArg::Userspace | StageArg::Ffmpeg
    );
    let (patches_path, patches_dev) = if needs_patches {
        resolve_patches_source(
            args.patches_path.as_deref(),
            args.patches_url.as_deref(),
            &resolved,
            &lock,
            root,
            &sink,
        )?
    } else {
        (PathBuf::from("../patches"), false)
    };

    // The rootfs tarball the image stage consumes: produced by the rootfs stage,
    // or supplied directly via --rootfs-tar for an image-only build.
    let mut rootfs_tar = args.rootfs_tar.clone();
    // Captured when this run builds the rootfs (the point the per-image password +
    // solved manifest exist), to emit the provenance manifest at the end.
    let mut rootfs_out: Option<(PathBuf, String)> = None;
    // The freshly-solved manifest's sha256, set by the rootfs stage — verified
    // against the committed pin and recorded into the lock by `--save-manifest`.
    let mut solved_manifest_digest: Option<String> = None;
    // The `linux-image-*` .deb this run built, if the kernel stage ran here. The
    // rootfs stage installs the kernel by this exact artifact rather than by
    // scanning out_dir, so its package set never depends on stale debs left by
    // earlier builds of other kernel versions.
    let mut kernel_image_deb: Option<PathBuf> = None;

    if matches!(args.stage, StageArg::All | StageArg::Kernel) {
        let fragments = fragment_paths(root, &resolved)?;
        let opts = kernel::KernelOptions {
            source: &kernel_src,
            patches_root: &patches_path,
            fragments: &fragments,
            work_dir: &work_dir,
            out_dir: &out_dir,
            patches_dev,
            store: artifact_store.as_deref(),
        };
        let artifacts = run_stage("kernel", &sink, || {
            kernel::build_kernel(&resolved, &lock, &opts, &build_env, &sink)
        })?;
        println!("kernel image  : {}", artifacts.image_deb.display());
        println!("kernel headers: {}", artifacts.headers_deb.display());
        record_artifacts(&out_dir, &[artifacts.image_deb.clone(), artifacts.headers_deb.clone()])?;
        kernel_image_deb = Some(artifacts.image_deb.clone());
    }

    if matches!(args.stage, StageArg::All | StageArg::Uboot) {
        let opts = uboot::UbootOptions {
            source: &uboot_src,
            patches_root: &patches_path,
            blobs_dir: &blobs_dir,
            work_dir: &work_dir,
            out_dir: &out_dir,
            patches_dev,
            store: artifact_store.as_deref(),
        };
        let artifacts = run_stage("uboot", &sink, || {
            uboot::build_uboot(&resolved, &lock, &opts, &build_env, &sink)
        })?;
        println!("idbloader     : {}", artifacts.idbloader.display());
        println!("u-boot.itb    : {}", artifacts.uboot_itb.display());
        println!("u-boot deb    : {}", artifacts.deb.display());
        record_artifacts(&out_dir, std::slice::from_ref(&artifacts.deb))?;
        // A uboot-only build also emits a standalone, directly-flashable bootloader
        // image (`<device>-boot.img`) — the eMMC/SPI medium for a split install
        // whose OS lives on another disk. A full build skips it: the image stage
        // folds u-boot into the combined image, or emits `-boot.img` for `split`.
        if matches!(args.stage, StageArg::Uboot) {
            let boot_img = run_stage("bootloader-image", &sink, || {
                image::build_bootloader_image(
                    &resolved,
                    &artifacts.idbloader,
                    &artifacts.uboot_itb,
                    &out_dir,
                    &sink,
                )
            })?;
            println!("boot image    : {}", boot_img.display());
        }
    }

    if matches!(args.stage, StageArg::All | StageArg::Userspace) {
        let opts = userspace::UserspaceOptions {
            mpp_src: &mpp_src,
            librga_src: &librga_src,
            libmali_src: &libmali_src,
            build_libmali: args.build_libmali,
            work_dir: &work_dir,
            out_dir: &out_dir,
            patches_root: &patches_path,
            patches_dev,
            store: artifact_store.as_deref(),
        };
        let artifacts = run_stage("userspace", &sink, || {
            userspace::build_userspace(
                &lock,
                &opts,
                resolved.arch.as_str(),
                &build_env,
                sandbox.as_ref(),
                &sink,
            )
        })?;
        for deb in &artifacts.debs {
            println!("userspace deb : {}", deb.display());
        }
        record_artifacts(&out_dir, &artifacts.debs)?;
    }

    if matches!(args.stage, StageArg::All | StageArg::Ffmpeg) {
        // ffmpeg build-depends on the userspace .debs; they are staged in
        // out_dir by the userspace stage (run it first, or with --stage all).
        let opts = ffmpeg::FfmpegOptions {
            base_src: &ffmpeg_base_src,
            patches_root: &patches_path,
            userspace_debs: &out_dir,
            work_dir: &work_dir,
            out_dir: &out_dir,
            patches_dev,
            store: artifact_store.as_deref(),
        };
        let artifacts = run_stage("ffmpeg", &sink, || {
            ffmpeg::build_ffmpeg(
                &lock,
                &opts,
                resolved.arch.as_str(),
                &build_env,
                sandbox.as_ref(),
                &sink,
            )
        })?;
        println!("ffmpeg deb    : {}", artifacts.deb.display());
        record_artifacts(&out_dir, std::slice::from_ref(&artifacts.deb))?;
    }

    if matches!(args.stage, StageArg::All | StageArg::Rootfs) {
        // Bootstrap the device rootfs: stand up a local apt repo from the
        // built .debs in out_dir, install the merged package set, apply the layered
        // overlay, and emit the tarball the image stage formats into ext4.
        let overlay_dirs = overlay_dirs(root, &resolved);
        // The local apt repo is seeded from the artifact ledger — the exact debs the
        // compile stages recorded — not an extension-only scan of out_dir, so an
        // unsigned stray never becomes trusted apt input (TRUST-3).
        let mut repo_debs = ledger_debs(&out_dir)?;
        // Materialize the pre-built extra_debs into the content store and
        // add them to the local apt repo's deb set — the way a feature's packages
        // reach the solve, but for bytes pulled from outside the mirror. They then
        // fold into the rootfs cache key by content (via `file_fingerprints`), so a
        // changed extra_deb re-bootstraps. The local repo is the trust boundary for
        // these unsigned debs; a package set entry (or another package's
        // dependency) is what actually installs them.
        if !lock.extra_debs.is_empty() {
            let extra = run_stage("extra-debs", &sink, || {
                let step = Step::start(&sink, "extra-debs");
                let store = DebStore::open(&extra_debs_store(root))?;
                let paths = extradebs::materialize(root, &lock.extra_debs, &store, &step)?;
                step.finish();
                Ok(paths)
            })?;
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
        // Resolve each feature apt source's signing keyring to a vendored host path,
        // failing fast if a declared source's keyring is missing (CFG-1): the
        // repo is verified during the solve, not trusted blindly, so its key is a
        // build-host prerequisite like the Debian archive keyring.
        let mut apt_repos = Vec::with_capacity(resolved.apt_sources.len());
        for source in &resolved.apt_sources {
            let rel = format!("blobs/keyrings/{}", source.signed_by);
            let keyring = root.find_asset(&rel).ok_or_else(|| {
                format!(
                    "apt source '{}' requires signing keyring '{}', but it is not vendored \
                     — add it under blobs/keyrings/ (see blobs/keyrings/README.md)",
                    source.name, rel
                )
            })?;
            apt_repos.push(rootfs::AptRepo { source, keyring });
        }
        let opts = rootfs::RootfsOptions {
            repo_debs: &repo_debs,
            overlay_dirs: &overlay_dirs,
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
            // per-image password varies between builds of one lock (DET-2/DET-4). None
            // on a rootfs-only build with no kernel tree in this work dir.
            source_date_epoch: kernel::source_date_epoch(&work_dir, &lock),
        };
        let artifacts = run_stage("rootfs", &sink, || {
            MmdebstrapRootfs.build(&resolved, &opts, &sink)
        })?;
        println!("rootfs tar   : {}", artifacts.tar.display());
        println!("manifest     : {}", artifacts.manifest.display());
        // Manifest-as-input verification: unless `--save-manifest` re-pins,
        // a fresh solve must reproduce the committed pin — a drift means the live
        // mirror moved off the pinned package set. Hard error unless the drift is
        // explicitly allowed.
        let solved_digest = boot2deb_engine::manifest::digest(&artifacts.manifest)?;
        if !args.save_manifest {
            if let Some(pinned) = &lock.rootfs.manifest_sha256 {
                match boot2deb_engine::manifest::verify_reproduced(pinned, &solved_digest) {
                    Ok(()) => println!("manifest OK  : reproduces the committed pin"),
                    Err(e) if args.allow_manifest_drift => eprintln!("warning: {e}"),
                    Err(e) => return Err(e.into()),
                }
            }
        }
        solved_manifest_digest = Some(solved_digest);
        // The per-image first-boot password (SEC-6): unique per build, must
        // be changed at first login. Surfaced here since it exists nowhere else the
        // operator can read it except the provenance manifest.
        println!(
            "first-boot pw: {}  (user {}, expired — change at first login)",
            artifacts.password,
            rootfs::DEFAULT_USER
        );
        rootfs_tar = Some(artifacts.tar);
        rootfs_out = Some((artifacts.manifest, artifacts.password));
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
        // Structural gate, not mere existence (ATOM-1): confirm the tar is complete
        // and readable through its appended `./etc/shadow` member. An `--stage image`
        // retry after an interrupted rootfs stage then fails cleanly here instead of
        // formatting a truncated tar into a broken ext4 image.
        rootfs::validate_tar(&rootfs_tar)?;
        let idbloader = out_dir.join("idbloader.img");
        let uboot_itb = out_dir.join("u-boot.itb");
        for (what, p) in [("idbloader.img", &idbloader), ("u-boot.itb", &uboot_itb)] {
            if !p.exists() {
                return Err(format!(
                    "{what} not found in {} — run `build {recipe} --stage uboot` first",
                    out_dir.display()
                )
                .into());
            }
        }
        let opts = image::ImageOptions {
            rootfs_tar: &rootfs_tar,
            idbloader: &idbloader,
            uboot_itb: &uboot_itb,
            out_dir: &out_dir,
            work_dir: &work_dir,
            rootfs_label: &args.rootfs_label,
            // Seed the deterministic ext4 UUID + GPT GUIDs from the locked kernel
            // commit, so the image's identifiers are a function of the lock.
            image_seed: &lock.kernel.commit,
            compress: !args.no_compress,
            keep_raw: args.keep_raw,
        };
        let artifacts = run_stage("image", &sink, || image::build_image(&resolved, &opts, &sink))?;
        // The raw paths are deleted after compression unless --keep-raw, so only
        // print them when they still exist on disk.
        if !artifacts.raw_removed {
            match &artifacts.output {
                ImageOutput::Combined { image } => println!("image         : {}", image.display()),
                ImageOutput::Split { bootloader, rootfs } => {
                    println!("boot image    : {}", bootloader.display());
                    println!("rootfs image  : {}", rootfs.display());
                }
            }
        }
        for xz in &artifacts.compressed {
            println!("compressed    : {}", xz.display());
        }
    }

    // Emit the provenance manifest when this run built the rootfs — the
    // point at which the per-image password and solved manifest both exist. It
    // joins the lock's pins, the resolved build point, the solved-manifest digest,
    // the blob hashes, the toolchain identity, and the first-boot credential into
    // one "exactly what went into this image" document for support/security.
    if let Some((manifest_path, password)) = &rootfs_out {
        let manifest_bytes = std::fs::read(manifest_path).map_err(|e| {
            format!("read solved manifest {}: {e}", manifest_path.display())
        })?;
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
        };
        let prov = boot2deb_core::provenance::assemble(&resolved, &lock, &facts);
        let prov_path = out_dir.join(format!("{recipe}.provenance.toml"));
        std::fs::write(&prov_path, prov.to_toml_string()?)
            .map_err(|e| format!("write provenance {}: {e}", prov_path.display()))?;
        println!("provenance   : {}", prov_path.display());
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
            println!("saved snapshot: {ts} (mode off — activate with --snapshot fallback|pin)");
        }
        if args.save_manifest {
            let (manifest_path, _) = rootfs_out.as_ref().ok_or(
                "--save-manifest needs the rootfs stage — run --stage all or --stage rootfs",
            )?;
            let digest = solved_manifest_digest.as_ref().ok_or(
                "--save-manifest needs the freshly-solved manifest digest — run --stage all or --stage rootfs",
            )?;
            let committed = root.recipe_sibling(recipe, &new_lock.rootfs.manifest)?;
            std::fs::copy(manifest_path, &committed)
                .map_err(|e| format!("commit manifest to {}: {e}", committed.display()))?;
            new_lock.rootfs.manifest_sha256 = Some(digest.clone());
            println!("saved manifest: {} (sha256 {})", committed.display(), short(digest));
        }
        let path = root.lock_path(recipe)?;
        pins::write_lock(&path, &new_lock)?;
        println!("updated lock  : {}", path.display());
    }
    Ok(())
}

/// Resolve the patches source for a build, returning the checkout path and
/// whether it is a co-development checkout (a pin mismatch is downgraded to a
/// warning rather than a hard error). Precedence:
///
/// 1. An explicit `--patches-path <dir>` — co-development from a working checkout.
/// 2. The default `../patches` if it is a git checkout — the pin is enforced.
/// 3. Auto-fetch the series at the lock's `patches.commit` from `--patches-url` or
///    the kernel definition's `patches_url`, into a durable commit-addressed cache
///    (`<root>/cache/patches/<commit>`), so a build with no local checkout resolves
///    automatically (the North-Star "selecting a device auto-fetches the right
///    patches"). With no URL available this is a hard [`EngineError::PatchesNoSource`]
///    naming the pinned commit — patches are never silently skipped.
fn resolve_patches_source(
    patches_path: Option<&Path>,
    patches_url: Option<&str>,
    resolved: &ResolvedBuild,
    lock: &boot2deb_core::lock::Lock,
    root: &ConfigRoot,
    sink: &dyn EventSink,
) -> Result<(PathBuf, bool), Box<dyn std::error::Error>> {
    if let Some(path) = patches_path {
        return Ok((path.to_path_buf(), true));
    }
    let default_local = PathBuf::from("../patches");
    if default_local.join(".git").exists() {
        return Ok((default_local, false));
    }
    let url = patches_url
        .map(str::to_string)
        .or_else(|| resolved.kernel.patches_url.clone())
        .ok_or_else(|| EngineError::PatchesNoSource {
            commit: lock.patches.commit.clone(),
        })?;
    let cache_root = root.path().join("cache").join("patches");
    let step = Step::start(sink, "patches");
    let dir = patchfetch::fetch_profile(&url, &lock.patches.commit, &cache_root, &step)?;
    step.finish();
    Ok((dir, false))
}

/// Run one stage closure, emitting [`Event::Error`] on failure so the stream
/// carries the failure before the typed error propagates.
fn run_stage<T>(
    step: &str,
    sink: &dyn EventSink,
    f: impl FnOnce() -> Result<T, boot2deb_engine::EngineError>,
) -> Result<T, boot2deb_engine::EngineError> {
    f().inspect_err(|e| {
        sink.emit(Event::Error {
            step: step.to_string(),
            context: e.to_string(),
        });
    })
}

/// Resolve a build's kernel fragment names to `fragments/<name>.config` paths
/// along the config search path, erroring if any is missing. An overlay may
/// ship the fragments for a device/kernel it adds; the highest-precedence copy
/// wins.
fn fragment_paths(
    root: &ConfigRoot,
    build: &ResolvedBuild,
) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let mut paths = Vec::new();
    for name in &build.kernel.config_fragments {
        let rel = format!("fragments/{name}.config");
        let path = root
            .find_asset(&rel)
            .ok_or_else(|| format!("fragment not found: {rel} (searched the config path)"))?;
        paths.push(path);
    }
    Ok(paths)
}

/// Validate the resolved build's cheap, pure config invariants before a lock is written
/// or any stage runs (CFG-4): the whole image geometry (offset ordering, alignment,
/// GPT/rootfs fit — via the engine), and that every referenced kernel
/// `config_fragments` file exists under the config path. Run by both `update` (so a
/// malformed axis fails before the lock is committed) and `build` (so it fails before
/// any stage compiles), so a bad `rootfs_offset` or a typo'd fragment name surfaces at
/// resolution rather than deep in the build — the same fail-early discipline as the
/// device/kernel/suite checks (CFG-2/CFG-3).
fn preflight_config(
    root: &ConfigRoot,
    build: &ResolvedBuild,
) -> Result<(), Box<dyn std::error::Error>> {
    image::validate_geometry(build)?;
    // Resolve each fragment purely to assert it exists; the paths are re-resolved where
    // the kernel stage actually consumes them.
    fragment_paths(root, build)?;
    Ok(())
}

/// Render one build [`Event`] to the terminal: step boundaries as `==>` headers,
/// subprocess lines indented (stderr to stderr), progress and errors called out.
fn print_event(event: &Event) {
    match event {
        Event::StepStarted { step } => println!("==> [{step}] started"),
        Event::Progress { step, pct } => println!("--> [{step}] {pct}%"),
        Event::Log { stream, line, .. } => match stream {
            Stream::Stdout => println!("    {line}"),
            Stream::Stderr => eprintln!("    {line}"),
        },
        Event::StepFinished { step } => println!("==> [{step}] done"),
        Event::Error { step, context } => eprintln!("==> [{step}] error: {context}"),
    }
}

/// Resolve the recipe, consult upstream + hash blobs, and write its lock.
fn update(
    root: &ConfigRoot,
    recipe: &str,
    args: UpdateArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let build = resolve_recipe(root, recipe, &Overrides::default())?;
    // Validate the pure config invariants (image geometry + kernel-fragment existence)
    // before resolving/committing the lock, so a bad `rootfs_offset` or a typo'd
    // fragment fails here rather than being pinned into the lock and failing at the next
    // build (CFG-4).
    preflight_config(root, &build)?;
    // An omitted per-tree ref flag preserves the *previous lock's* ref, not the
    // config's symbolic ref (COR-12). Otherwise a routine `update` that only bumps
    // the kernel would silently re-pin every other tree from its committed exact
    // commit back to the current branch head. Flags still override; a first update
    // (no prior lock) falls back to the config default.
    let prev = root.lock(recipe).ok();
    let ref_for = |flag: Option<String>, from_lock: fn(&boot2deb_core::lock::Lock) -> String, default: &str| {
        flag.or_else(|| prev.as_ref().map(from_lock))
            .unwrap_or_else(|| default.to_string())
    };
    let uboot_ref = ref_for(args.uboot_ref, |l| l.uboot.reference.clone(), &build.uboot_ref);
    let mpp_ref = ref_for(
        args.mpp_ref,
        |l| l.userspace.mpp.reference.clone(),
        &build.userspace.mpp.git_ref,
    );
    let librga_ref = ref_for(
        args.librga_ref,
        |l| l.userspace.librga.reference.clone(),
        &build.userspace.librga.git_ref,
    );
    let libmali_ref = ref_for(
        args.libmali_ref,
        |l| l.userspace.libmali.reference.clone(),
        &build.userspace.libmali.git_ref,
    );
    let ffmpeg_base_ref = ref_for(
        args.ffmpeg_base_ref,
        |l| l.ffmpeg.base.reference.clone(),
        &build.ffmpeg.base.git_ref,
    );
    let ffmpeg_rockchip_ref = ref_for(
        args.ffmpeg_rockchip_ref,
        |l| l.ffmpeg.rockchip.reference.clone(),
        &build.ffmpeg.rockchip.git_ref,
    );
    let blobs_dir = args.blobs_dir.clone().unwrap_or_else(|| {
        let rel = format!("blobs/{}", build.soc.as_str());
        root.find_asset(&rel).unwrap_or_else(|| root.path().join(rel))
    });
    let manifest = args
        .rootfs_manifest
        .unwrap_or_else(|| format!("{recipe}.pkgs.lock"));
    let opts = pins::UpdateOptions {
        kernel_ref: &args.kernel_ref,
        uboot_ref: &uboot_ref,
        mpp_ref: &mpp_ref,
        librga_ref: &librga_ref,
        libmali_ref: &libmali_ref,
        ffmpeg_base_ref: &ffmpeg_base_ref,
        ffmpeg_rockchip_ref: &ffmpeg_rockchip_ref,
        blobs_dir: &blobs_dir,
        patches_path: &args.patches_path,
        rootfs_manifest: &manifest,
    };
    let lock = pins::resolve_lock(&build, &opts)?;
    // Fetch + verify + store each pre-built extra_deb before committing the lock, so
    // a dead URL, a missing file, or a wrong hash fails now rather than at the next
    // build. Fills the durable content store `build` later reads.
    if !lock.extra_debs.is_empty() {
        let sink = |e: Event| print_event(&e);
        let step = Step::start(&sink, "extra-debs");
        let store = DebStore::open(&extra_debs_store(root))?;
        extradebs::materialize(root, &lock.extra_debs, &store, &step)?;
        step.finish();
    }
    let path = root.lock_path(recipe)?;
    pins::write_lock(&path, &lock)?;

    println!("wrote {}", path.display());
    println!(
        "  kernel   {} {} {}",
        lock.kernel.id,
        lock.kernel.reference,
        short(&lock.kernel.commit)
    );
    println!(
        "  u-boot   {} {}",
        lock.uboot.reference,
        short(&lock.uboot.commit)
    );
    println!(
        "  patches  {} {}",
        lock.patches.profile,
        short(&lock.patches.commit)
    );
    println!(
        "  mpp      {} {}",
        lock.userspace.mpp.reference,
        short(&lock.userspace.mpp.commit)
    );
    println!(
        "  librga   {} {}",
        lock.userspace.librga.reference,
        short(&lock.userspace.librga.commit)
    );
    println!(
        "  libmali  {} {}",
        lock.userspace.libmali.reference,
        short(&lock.userspace.libmali.commit)
    );
    println!(
        "  ffmpeg   {} {}",
        lock.ffmpeg.base.reference,
        short(&lock.ffmpeg.base.commit)
    );
    println!(
        "  ff-rk    {} {} (graft provenance)",
        lock.ffmpeg.rockchip.reference,
        short(&lock.ffmpeg.rockchip.commit)
    );
    println!(
        "  rootfs   {} (manifest {})",
        lock.rootfs.suite, lock.rootfs.manifest
    );
    println!("  blob atf {}", lock.blobs.atf);
    println!("  blob tpl {}", lock.blobs.tpl);
    for d in &lock.extra_debs {
        println!("  extradeb {} {}", d.locator_label(), short(&d.sha256));
    }

    // Source-pin durability: flag, at pin time, any source that did not
    // resolve to a durable release tag — an ephemeral branch tip, or a commit
    // advertised by no ref (which may exist only in a local checkout and is then not
    // reproducible from upstream, the mpp anti-pattern of). Cheap: one
    // `git ls-remote` per source against its *configured* URL, no ancestry fetch;
    // `verify-sources` does the deep reachability probe. Advisory — never blocks the
    // lock write (the onus is on whoever pins a non-durable source).
    let axes = source_axes(&build, &lock)?;
    let mut flagged = false;
    for axis in &axes {
        match sources::pin_warning(&axis.url, axis.reference, axis.commit) {
            sources::PinWarning::Durable => {}
            sources::PinWarning::Ephemeral(branch) => {
                flagged = true;
                eprintln!(
                    "  warning: {} pins the tip of branch '{branch}' — a force-push/rebase/delete \
                     can orphan it; pin a release tag for durability",
                    axis.name
                );
            }
            sources::PinWarning::Unadvertised => {
                flagged = true;
                eprintln!(
                    "  note: {} commit {} is advertised by no tag or branch on {} — if it exists \
                     only in a local checkout this pin is NOT reproducible from upstream; run \
                     `boot2deb verify-sources {recipe}` to confirm reachability",
                    axis.name,
                    short(axis.commit),
                    axis.url
                );
            }
            sources::PinWarning::Skipped(reason) => {
                eprintln!("  note: could not check {} pin durability: {reason}", axis.name);
            }
        }
    }
    if flagged {
        eprintln!(
            "  (durable = a release tag, re-fetchable forever; see \
             `boot2deb verify-sources {recipe}` for the full reachability report)"
        );
    }
    Ok(())
}

/// The fetched source axes as `(name, configured upstream URL, locked ref,
/// locked commit)` — the set `verify-sources` probes and `update` warns on, always
/// against the *configured* URL (never a `--<pkg>-src` override). The ffmpeg
/// `rockchip` pin is provenance-only (never fetched at build), so it is omitted.
struct SourceAxis<'a> {
    /// Human name for the report (`kernel`, `u-boot`, `mpp`, …).
    name: &'static str,
    /// The configured upstream clone URL.
    url: String,
    /// The pinned ref (tag/branch name, or the bare commit).
    reference: &'a str,
    /// The exact pinned commit.
    commit: &'a str,
}

/// Build the [`SourceAxis`] list from a resolved build (for the configured URLs) and
/// its lock (for the pins). The kernel URL resolution is the only fallible step.
fn source_axes<'a>(
    build: &ResolvedBuild,
    lock: &'a boot2deb_core::lock::Lock,
) -> Result<Vec<SourceAxis<'a>>, Box<dyn std::error::Error>> {
    Ok(vec![
        SourceAxis {
            name: "kernel",
            url: pins::kernel_source_url(&build.kernel.source)?,
            reference: &lock.kernel.reference,
            commit: &lock.kernel.commit,
        },
        SourceAxis {
            name: "u-boot",
            url: build.uboot_source.clone(),
            reference: &lock.uboot.reference,
            commit: &lock.uboot.commit,
        },
        SourceAxis {
            name: "mpp",
            url: build.userspace.mpp.git.clone(),
            reference: &lock.userspace.mpp.reference,
            commit: &lock.userspace.mpp.commit,
        },
        SourceAxis {
            name: "librga",
            url: build.userspace.librga.git.clone(),
            reference: &lock.userspace.librga.reference,
            commit: &lock.userspace.librga.commit,
        },
        SourceAxis {
            name: "libmali",
            url: build.userspace.libmali.git.clone(),
            reference: &lock.userspace.libmali.reference,
            commit: &lock.userspace.libmali.commit,
        },
        SourceAxis {
            name: "ffmpeg-base",
            url: build.ffmpeg.base.git.clone(),
            reference: &lock.ffmpeg.base.reference,
            commit: &lock.ffmpeg.base.commit,
        },
    ])
}

/// Probe each locked source pin against its configured upstream URL and report its
/// re-fetch durability. Read-only: `git ls-remote` plus a timeout-bounded
/// ancestry check per pin; no build, no checkout, no hardware. Exits non-zero if any
/// pin is ORPHANED (not re-fetchable), so CI can gate on it.
fn verify_sources(root: &ConfigRoot, recipe: &str) -> Result<(), Box<dyn std::error::Error>> {
    let build = resolve_recipe(root, recipe, &Overrides::default())?;
    let lock = root.lock(recipe)?;
    let axes = source_axes(&build, &lock)?;
    println!(
        "probing {} source pins for {recipe} against their configured upstreams (read-only)\n",
        axes.len()
    );
    let mut orphaned = 0usize;
    let mut undurable = 0usize;
    for axis in &axes {
        let d = sources::probe(&axis.url, axis.reference, axis.commit);
        // Show the ref only when it is a name; a bare-commit pin's ref is the commit.
        let ref_display = if axis.reference == axis.commit {
            "(bare commit)".to_string()
        } else {
            axis.reference.to_string()
        };
        println!(
            "  {:<12} {:<9} {} @ {}",
            axis.name,
            d.label(),
            ref_display,
            short(axis.commit)
        );
        println!("               {}", d.detail());
        match d {
            sources::Durability::Orphaned(_) => orphaned += 1,
            sources::Durability::Durable(_) => {}
            _ => undurable += 1,
        }
    }
    println!();
    if orphaned > 0 {
        return Err(format!(
            "{orphaned} source pin(s) are ORPHANED — not re-fetchable from their configured URL. \
             Re-pin via `boot2deb update` to a durable tag, or point the source at a mirror that \
             holds the commit (the config `git` field / a `--<pkg>-src` build override). A build \
             from these pins needs a local checkout of the source."
        )
        .into());
    }
    if undurable > 0 {
        println!(
            "{undurable} pin(s) are not durable tags (ephemeral or unconfirmed). They build today \
             but may rot upstream; pin release tags for long-term reproducibility."
        );
    } else {
        println!("all source pins are durable tags.");
    }
    Ok(())
}

/// Verify the locked patch series applies to the provided source checkouts.
fn verify_patches(
    root: &ConfigRoot,
    recipe: &str,
    args: VerifyArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let lock = root.lock(recipe)?;
    let profile = load_profile(&args.patches_path, &lock.patches.profile)?;
    // Declared-intent gate: is the locked kernel in the profile's range?
    profile.ensure_applies(&lock.patches.profile, &lock.kernel.reference)?;
    let target = format!("{} @ {}", lock.kernel.id, lock.kernel.reference);

    // Verify the kernel series, plus any tree whose checkout was supplied.
    let mut trees: Vec<(&str, &[String], &Path)> =
        vec![("kernel", profile.kernel.as_slice(), args.kernel_path.as_path())];
    if let Some(p) = &args.ffmpeg_path {
        trees.push(("ffmpeg", profile.ffmpeg.as_slice(), p.as_path()));
    }
    if let Some(p) = &args.userspace_path {
        trees.push(("userspace", profile.userspace.as_slice(), p.as_path()));
    }

    let report = patches::verify_profile(&args.patches_path, &target, &trees)?;
    for (tree, n) in &report {
        println!("verify-patches {recipe}: {tree} series applies ({n} patches) against {target}");
    }
    Ok(())
}

/// Generate the kernel `.config` from the resolved fragments and, when a reference
/// config is given, check it against that reference.
fn verify_config(
    root: &ConfigRoot,
    recipe: &str,
    args: ConfigArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let build = resolve_recipe(root, recipe, &Overrides::default())?;
    // Fragment names resolve to fragments/<name>.config along the config search
    // path (overlay-aware), erroring if any is missing.
    let fragments = fragment_paths(root, &build)?;
    // Resolve the config in the same toolchain context the kernel build uses, so the
    // gate validates the config the build actually ships (cross-toolchain-probed
    // symbols included), not a host-probed variant.
    let pf = boot2deb_engine::preflight(build.arch);
    let cross = pf.cross.then(|| build.cross_compile.clone());
    let inputs = kconfig::ConfigInputs {
        tree: &args.kernel_path,
        arch: &build.kernel_arch,
        cross_compile: cross.as_deref(),
        base_defconfig: &build.kernel.base_defconfig,
        fragments: &fragments,
    };
    let work_dir = args
        .work_dir
        .unwrap_or_else(|| std::env::temp_dir().join(format!("boot2deb-{recipe}-kconfig")));

    // Stream the config `make` runs (defconfig / merge_config / olddefconfig) like
    // any build stage, so a long or wedged run is visible rather than silent.
    let sink = |e: Event| print_event(&e);
    let step = Step::start(&sink, "verify-config");

    match &args.reference_config {
        Some(reference) => {
            let report = kconfig::check_parity(&inputs, reference, &work_dir, &step)?;
            for sym in &report.unmet {
                println!("warning: fragment symbol not in final .config: {sym}");
            }
            if report.is_match() {
                println!(
                    "verify-config {recipe}: CONFIG_* parity OK ({} symbols) vs {}",
                    report.reference_symbols,
                    reference.display()
                );
            } else {
                eprintln!(
                    "verify-config {recipe}: {} CONFIG_* difference(s) vs {} (generated {} / reference {}):",
                    report.differences.len(),
                    reference.display(),
                    report.generated_symbols,
                    report.reference_symbols
                );
                for d in &report.differences {
                    eprintln!("  {}: generated={} reference={}", d.symbol, d.left, d.right);
                }
                return Err("kernel config parity check failed".into());
            }
        }
        None => {
            let generated = kconfig::generate(&inputs, &work_dir.join("gen"), &step)?;
            if generated.unmet.is_empty() {
                println!(
                    "verify-config {recipe}: clean merge ({} symbols); no reference config given",
                    generated.config.len()
                );
            } else {
                eprintln!(
                    "verify-config {recipe}: {} fragment symbol(s) not in final .config:",
                    generated.unmet.len()
                );
                for sym in &generated.unmet {
                    eprintln!("  {sym}");
                }
                return Err("kernel config merge left symbols unmet".into());
            }
        }
    }
    step.finish();
    Ok(())
}

/// Import a patch into a profile: fetch from a URL/file/stdin, normalize to
/// canonical `git am`-ready mbox, write it into the patches repo, insert its label
/// into the profile's scope at the requested position, and — with `--verify-tree` —
/// dry-run `git am`-verify the resulting series. The file write and the profile edit
/// are rolled back if the verify fails, so a rejected patch leaves the repo untouched.
fn patch_import(source: &str, args: PatchImportArgs) -> Result<(), Box<dyn std::error::Error>> {
    // Fetch: `-` reads stdin; otherwise a URL is fetched or a file is read.
    let bytes = if source == "-" {
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut std::io::stdin(), &mut buf)?;
        buf
    } else {
        patchimport::fetch(source)?
    };
    let text = String::from_utf8_lossy(&bytes);

    // Normalize (pure): classify the shape and produce canonical mbox + subject.
    let meta = ImportMeta {
        author: Some(args.author.clone()),
        subject: args.subject.clone(),
        origin: args.origin.clone(),
    };
    let normalized = mbox::normalize(&text, &meta)?;

    // The current scope list fixes the insertion index and the derived prefix.
    let profile = load_profile(&args.patches_path, &args.profile)?;
    let scope_list = profile.scope(args.scope);
    // `--position` is 1-based; default appends to the end. Clamp to the list length.
    let index = args
        .position
        .map(|p| p.saturating_sub(1))
        .unwrap_or(scope_list.len())
        .min(scope_list.len());

    // The destination label: `--as` verbatim, else <dest-dir>/<prefix>-<slug>.patch.
    let label = match &args.label {
        Some(explicit) => explicit.clone(),
        None => {
            let dest_dir = args
                .dest_dir
                .clone()
                .unwrap_or_else(|| format!("media-accel/{}", args.scope.as_str()));
            let slug = args
                .name
                .clone()
                .unwrap_or_else(|| mbox::slugify(&normalized.subject));
            let prefix = derive_prefix(scope_list, index)?;
            format!("{dest_dir}/{prefix}-{slug}.patch")
        }
    };
    patchimport::safe_label(&label)?;

    let dest_path = args.patches_path.join(&label);
    if dest_path.exists() && !args.force {
        return Err(EngineError::PatchImportExists {
            path: dest_path.display().to_string(),
        }
        .into());
    }

    println!(
        "patch import: detected {}, subject \"{}\"",
        normalized.kind.label(),
        normalized.subject
    );

    // Write the normalized patch into the repo.
    if let Some(parent) = dest_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&dest_path, &normalized.mbox)?;
    println!("patch import: wrote {} ({} bytes)", label, normalized.mbox.len());

    // Verify the resulting series (with the new patch spliced in at `index`) against
    // a source checkout, if one was supplied. A failure rolls back the written file.
    match &args.verify_tree {
        Some(tree) => {
            let mut spliced = scope_list.to_vec();
            spliced.insert(index, label.clone());
            let target = format!("{} ({})", args.profile, tree.display());
            match patches::verify_tree(
                &args.patches_path,
                &spliced,
                tree,
                args.scope.as_str(),
                &target,
            ) {
                Ok(n) => println!(
                    "patch import: git am-verified the {} series ({n} patches) against {}",
                    args.scope.as_str(),
                    tree.display()
                ),
                Err(e) => {
                    let _ = std::fs::remove_file(&dest_path);
                    return Err(e.into());
                }
            }
        }
        None => {
            eprintln!(
                "warning: patch written but not verified — pass --verify-tree <checkout> to \
                 dry-run `git am` the series, or run `verify-patches`."
            );
        }
    }

    // Slot the label into the profile manifest, preserving its comments/layout. If
    // this fails, roll back the written patch so no partial import survives.
    let profile_path = args
        .patches_path
        .join("profiles")
        .join(&args.profile)
        .join("profile.toml");
    if let Err(e) = patchimport::insert_into_profile(&profile_path, args.scope, index, &label) {
        let _ = std::fs::remove_file(&dest_path);
        return Err(e.into());
    }
    println!(
        "patch import: {}/{} now lists the patch at position {} of {}",
        args.profile,
        args.scope.as_str(),
        index + 1,
        scope_list.len() + 1
    );
    Ok(())
}

/// First 12 chars of a commit id for display.
fn short(commit: &str) -> &str {
    &commit[..commit.len().min(12)]
}

/// Make `path` absolute (against the current dir) if it is relative, so it is
/// safe to hand to `bwrap --bind`/`--chdir` inside the sandbox namespace. Falls
/// back to the input if the current dir is unreadable.
/// Explain, per compile node, whether the next `build` reuses or rebuilds its
/// source tree, and why. Offline: reads only the lock and the on-disk
/// build stamps — runs no build, touches no network or hardware.
fn why_rebuild(
    root: &ConfigRoot,
    recipe: &str,
    args: WhyRebuildArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let lock = root.lock(recipe)?;
    let work_dir = absolutize(
        args.work_dir
            .unwrap_or_else(|| PathBuf::from("build").join(recipe)),
    );
    let nodes = plan::plan_nodes(&plan::PlanInputs {
        lock: &lock,
        work_dir: &work_dir,
        patches_dev: args.patches_path.is_some(),
        // Co-dev predictions fold the live-series fingerprint, so pass the checkout
        // the build reads its patches from (CACHE-1); `None` in pinned mode.
        patches_root: args.patches_path.as_deref(),
        include_libmali: args.build_libmali,
    });

    println!("why-rebuild {recipe} (work {})", work_dir.display());
    for node in &nodes {
        let (verb, reason) = match &node.status {
            plan::NodeStatus::Absent => ("build", "no previous build".to_string()),
            plan::NodeStatus::Unstamped => {
                ("rebuild", "tree present but not stamped".to_string())
            }
            plan::NodeStatus::Reuse => ("reuse", String::new()),
            plan::NodeStatus::Rebuild(changes) if changes.is_empty() => {
                ("rebuild", "build logic changed".to_string())
            }
            plan::NodeStatus::Rebuild(changes) => (
                "rebuild",
                changes.iter().map(|c| c.summary()).collect::<Vec<_>>().join(", "),
            ),
        };
        if reason.is_empty() {
            println!("  {:<18} {verb}", node.node);
        } else {
            println!("  {:<18} {verb}  ({reason})", node.node);
        }
    }
    // Scope note: the stamp gates only the cloned+patched *tree*; the compile
    // step always re-runs, and the rootfs cache keys on the live package solve.
    println!(
        "note: only each node's source tree is cached; the compile step always re-runs, \
         and the rootfs cache keys on the live package solve."
    );
    Ok(())
}

/// Remove a recipe's build scratch (or a selected subtree), to reclaim disk or
/// force a clean rebuild. `--dry-run` previews without removing.
fn clean(root: &ConfigRoot, recipe: &str, args: CleanArgs) -> Result<(), Box<dyn std::error::Error>> {
    // Validate the recipe-name shape (reject `..`/absolute/separators) before it is
    // joined into a filesystem path, consistent with the config write paths (SEC-2).
    root.lock_path(recipe)?;
    let work_dir = absolutize(
        args.work_dir
            .unwrap_or_else(|| PathBuf::from("build").join(recipe)),
    );
    // Selectors carve out a subtree; with none, the whole work dir goes. The
    // artifact store is a selector too, but lives under the config root (shared
    // across recipes), not the work dir.
    let targets: Vec<PathBuf> = match (args.cache, args.sandbox, args.artifacts) {
        (false, false, false) => vec![work_dir.clone()],
        (cache, sandbox, artifacts) => {
            let mut t = Vec::new();
            if cache {
                t.push(work_dir.join("cache"));
            }
            if sandbox {
                t.push(work_dir.join("sandbox"));
            }
            if artifacts {
                t.push(absolutize(root.path().join("cache").join("artifacts")));
            }
            t
        }
    };

    let mut removed_any = false;
    for target in &targets {
        if !target.exists() {
            println!("  {} (absent)", target.display());
            continue;
        }
        let size = human_size(dir_size(target));
        if args.dry_run {
            println!("  would remove {} ({size})", target.display());
        } else {
            std::fs::remove_dir_all(target)
                .map_err(|e| format!("failed to remove {}: {e}", target.display()))?;
            println!("  removed {} ({size})", target.display());
            removed_any = true;
        }
    }
    if args.dry_run {
        println!("(dry run — nothing removed)");
    } else if !removed_any {
        println!("nothing to remove");
    }
    Ok(())
}

/// Total size in bytes of a directory tree, following no symlinks (counts the link,
/// not its target). Best-effort: an unreadable entry contributes nothing rather
/// than failing the whole size estimate.
fn dir_size(path: &Path) -> u64 {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    if meta.is_dir() {
        match std::fs::read_dir(path) {
            Ok(entries) => entries.flatten().map(|e| dir_size(&e.path())).sum(),
            Err(_) => 0,
        }
    } else {
        meta.len()
    }
}

/// Render a byte count as a short human string (`1.5 GiB`, `812 MiB`, `4.0 KiB`).
fn human_size(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KiB", "MiB", "GiB", "TiB"];
    let mut val = bytes as f64;
    let mut unit = 0;
    while val >= 1024.0 && unit < UNITS.len() - 1 {
        val /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} {}", UNITS[unit])
    } else {
        format!("{val:.1} {}", UNITS[unit])
    }
}

fn absolutize(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&path))
            .unwrap_or(path)
    }
}

/// Overlay directories for a build's rootfs, in merge order:
/// base → soc → boot-method → device → each feature. Each logical layer is
/// expanded along the config search path (shipped copy first, then any overlay's
/// copy of the same tree), so an overlay's overlay-tree stacks right after — and
/// thus wins over — the shipped one, matching the layer merge semantics. Absent
/// dirs contribute nothing.
fn overlay_dirs(root: &ConfigRoot, b: &ResolvedBuild) -> Vec<PathBuf> {
    let mut rels = vec![
        "base/overlay".to_string(),
        format!("socs/{}/overlay", b.soc.as_str()),
        format!("boot-methods/{}/overlay", b.boot_method.as_str()),
        format!("devices/{}/overlay", b.device),
    ];
    for feature in &b.features {
        rels.push(format!("features/{feature}/overlay"));
    }
    rels.iter().flat_map(|rel| root.find_asset_all(rel)).collect()
}

/// Name of the artifact ledger written into `out_dir` — the explicit allowlist of
/// `.deb`s this build produced. The rootfs stage's local apt repo ingests exactly
/// the invocation's own recorded outputs, never every `*.deb` that happens to sit in
/// `out_dir` (TRUST-3): the repo emits `[trusted=yes]`, so an unsigned stray or a
/// leftover from another build must not become trusted apt input. Persisted in
/// `out_dir` so a later `--stage rootfs` run still sees the compile stages' outputs
/// recorded by an earlier invocation.
const ARTIFACT_LEDGER: &str = ".boot2deb-artifacts";

/// Record each produced `.deb` into the `out_dir` artifact ledger (TRUST-3),
/// idempotently: the ledger is the set of file names the build staged into
/// `out_dir`, rewritten sorted so the file is deterministic. Paths not directly
/// under `out_dir` are ignored — the ledger names local-repo inputs, which every
/// stage stages into `out_dir`.
fn record_artifacts(out_dir: &Path, debs: &[PathBuf]) -> Result<(), Box<dyn std::error::Error>> {
    let ledger = out_dir.join(ARTIFACT_LEDGER);
    let mut names: std::collections::BTreeSet<String> = read_ledger_names(&ledger)?;
    for deb in debs {
        // Only debs staged directly under out_dir belong in the ledger.
        let in_out_dir = deb.parent() == Some(out_dir);
        if let (true, Some(name)) = (in_out_dir, deb.file_name().and_then(|n| n.to_str())) {
            names.insert(name.to_string());
        }
    }
    let body = names.into_iter().collect::<Vec<_>>().join("\n");
    std::fs::write(&ledger, body).map_err(|source| {
        format!("cannot write artifact ledger {} ({source})", ledger.display())
    })?;
    Ok(())
}

/// The ledger's recorded file names, or an empty set if the ledger does not exist.
fn read_ledger_names(ledger: &Path) -> Result<std::collections::BTreeSet<String>, Box<dyn std::error::Error>> {
    match std::fs::read_to_string(ledger) {
        Ok(text) => Ok(text.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
        Err(source) => Err(format!("cannot read artifact ledger {} ({source})", ledger.display()).into()),
    }
}

/// The `.deb`s the build recorded in the `out_dir` artifact ledger that still exist,
/// sorted — the local apt repo's trusted input set (TRUST-3). Unlike an
/// extension-only scan, a stray or partially-written `.deb` the build did not record
/// is never ingested. A missing ledger (no compile stage staged into this `out_dir`)
/// is a hard error with the same "run the compile stages first" hint the scan gave.
fn ledger_debs(out_dir: &Path) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let ledger = out_dir.join(ARTIFACT_LEDGER);
    let names = read_ledger_names(&ledger)?;
    let mut debs: Vec<PathBuf> = names
        .into_iter()
        .map(|n| out_dir.join(n))
        .filter(|p| p.exists())
        .collect();
    // Empty means either no ledger, or the recorded debs are all gone — either way
    // there is nothing to seed the local repo, so fail with the compile-stage hint
    // rather than bootstrap against an empty repo.
    if debs.is_empty() {
        return Err(format!(
            "no recorded build artifacts in {} — run the compile stages first \
             (e.g. `build --stage all`, or `--stage kernel/uboot/userspace/ffmpeg`)",
            out_dir.display()
        )
        .into());
    }
    debs.sort();
    Ok(debs)
}

/// The content-addressed store for pre-built `extra_debs`: a durable
/// build-host cache under the config root, shared by `update` (which fills it) and
/// `build` (which reads it). It sits outside any recipe work dir, so `clean` leaves
/// it intact — the build "no longer depends on [the source] staying put".
fn extra_debs_store(root: &ConfigRoot) -> PathBuf {
    root.path().join("cache").join("extra-debs")
}

/// Package name of each `.deb` — its file name up to the first `_` (dpkg forbids
/// `_` in package names, so `<package>_<version>_<arch>.deb` splits unambiguously).
fn deb_package_names(debs: &[PathBuf]) -> Vec<String> {
    debs.iter()
        .filter_map(|d| d.file_name()?.to_str()?.split('_').next().map(String::from))
        .collect()
}

/// The `linux-image-*` package name(s) the rootfs stage installs on top of the
/// resolved package set. The kernel is a build artifact whose package name
/// embeds a version the static config cannot name, so it is installed by the name
/// discovered from the built `.deb`.
///
/// To keep the install reproducible — a function of the current lock, not of
/// residue in `out_dir` — the kernel built in *this* run (`kernel_image_deb`) is
/// authoritative when the kernel stage ran here. For a standalone `--stage rootfs`
/// (kernel built by a prior invocation) the name is taken from `out_dir`, but only
/// when unambiguous: exactly one distinct `linux-image-*` package. Several distinct
/// kernel packages — stale debs from builds of different kernel versions sharing an
/// `out_dir` — are a hard error rather than a silent, non-reproducible guess.
fn kernel_packages(
    kernel_image_deb: &Option<PathBuf>,
    repo_debs: &[PathBuf],
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    if let Some(deb) = kernel_image_deb {
        return Ok(deb_package_names(std::slice::from_ref(deb)));
    }
    let mut names: Vec<String> = deb_package_names(repo_debs)
        .into_iter()
        .filter(|p| p.starts_with("linux-image-"))
        .collect();
    names.sort();
    names.dedup();
    if names.len() > 1 {
        return Err(format!(
            "multiple kernel packages in the output dir ({}) — cannot pick one for the rootfs. \
             Rebuild the kernel this run (build --stage all) or `clean` the stale debs first.",
            names.join(", ")
        )
        .into());
    }
    Ok(names)
}

/// Resolve `target` as a recipe if one exists, else as a device.
fn resolve(
    root: &ConfigRoot,
    target: &str,
    overrides: Overrides,
) -> Result<ResolvedBuild, boot2deb_core::ConfigError> {
    if root.list("recipes")?.iter().any(|n| n == target) {
        // A name that is both a recipe and a device resolves as the recipe; surface
        // the ambiguity rather than silently preferring one (COR-20).
        if root.list("devices")?.iter().any(|n| n == target) {
            eprintln!("note: '{target}' is both a recipe and a device — resolving as the recipe");
        }
        resolve_recipe(root, target, &overrides)
    } else {
        resolve_device(root, target, &overrides)
    }
}

fn doctor(
    root: &ConfigRoot,
    target: Option<String>,
    overrides: Overrides,
) -> Result<(), Box<dyn std::error::Error>> {
    let host = boot2deb_core::HostInfo::detect();
    println!("host arch : {}", host.arch);
    println!("host os   : {}", host.os);
    if !host.is_linux() {
        println!("note      : builds require a Linux host; this is a client-only platform");
    }
    let Some(target) = target else {
        return Ok(());
    };
    let build = resolve(root, &target, overrides)?;
    let pf = boot2deb_engine::preflight(build.arch);
    println!("target    : {target} (arch {})", build.arch);
    if pf.cross {
        println!(
            "cross     : yes — needs qemu-user binfmt for {} maintainer scripts/compiles",
            build.arch
        );
    } else {
        println!("cross     : no — native {} build, no qemu-user needed", build.arch);
    }

    // Tool-presence preflight: report each requirement with its path or a
    // host-specific install hint, then fail if any required tool is missing.
    println!();
    let checks = boot2deb_engine::checks::tool_checks(build.arch, &build.cross_compile);
    let mut blocking = 0usize;
    for c in &checks {
        match &c.status {
            CheckStatus::Present(detail) => {
                println!("  ok      {:<28} {}", c.name, detail);
            }
            CheckStatus::Missing(remedy) => {
                let tag = if c.required { "MISSING " } else { "absent  " };
                println!("  {tag}{:<28} {} — {}", c.name, c.purpose, remedy);
                if c.is_blocking() {
                    blocking += 1;
                }
            }
        }
    }
    println!();
    if blocking == 0 {
        println!("result    : all required host tools present");
        Ok(())
    } else {
        Err(format!("{blocking} required host tool(s) missing — install them before building").into())
    }
}

fn print_build(b: &ResolvedBuild) {
    println!("device       : {} — {}", b.device, b.description);
    println!("arch / soc   : {} / {}", b.arch, b.soc);
    println!("boot method  : {}", b.boot_method);
    println!("kernel       : {} ({}, base {})", b.kernel.id, b.kernel.flavor, b.kernel.base_defconfig);
    println!("  track      : {}", b.kernel.track.as_deref().unwrap_or("-"));
    println!("  profile    : {}", b.kernel.patch_profile);
    println!("  fragments  : {}", b.kernel.config_fragments.join(", "));
    println!("suite        : {}", b.suite);
    println!(
        "features     : {}",
        if b.features.is_empty() { "-".to_string() } else { b.features.join(", ") }
    );
    println!("rootfs pkgs  : {}", b.rootfs_packages.join(", "));
    if !b.apt_sources.is_empty() {
        println!(
            "apt sources  : {}",
            b.apt_sources
                .iter()
                .map(|s| format!("{} ({})", s.name, s.uri))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    if !b.extra_debs.is_empty() {
        println!(
            "extra debs   : {}",
            b.extra_debs
                .iter()
                .map(|d| format!("{} ({})", d.locator_label(), short(&d.sha256)))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
    println!("layout       : {}", b.layout);
    println!("image size   : {}", b.image_size);
    println!("hostname     : {}", b.hostname);
    println!("dtb          : {}", b.kernel_dtb);
    println!("u-boot       : {} ({})", b.uboot_ref, b.uboot_defconfig);
    println!("rkbin atf    : {}", b.rkbin.atf);
    println!("rkbin tpl    : {}", b.rkbin.tpl);
    println!(
        "offsets      : idbloader {}, u-boot.itb {}, rootfs {}",
        b.offsets.idbloader, b.offsets.uboot_itb, b.offsets.rootfs
    );
    println!("modules      : {}", b.modules.join(", "));
    println!("cross-compile: {}", b.cross_compile);
    println!(
        "mpp / librga : {} ({}) / {} ({})",
        b.userspace.mpp.git, b.userspace.mpp.git_ref, b.userspace.librga.git, b.userspace.librga.git_ref
    );
    println!(
        "ffmpeg base  : {} ({})",
        b.ffmpeg.base.git, b.ffmpeg.base.git_ref
    );
    println!(
        "ffmpeg rk    : {} ({})",
        b.ffmpeg.rockchip.git, b.ffmpeg.rockchip.git_ref
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_packages_prefers_this_runs_artifact() {
        // When the kernel stage ran this run, its exact .deb is authoritative and
        // stale linux-image debs in out_dir are ignored — no ambiguity, no scan.
        let built = PathBuf::from("/out/linux-image-6.12.0-1-arm64_1_arm64.deb");
        let repo = vec![
            built.clone(),
            PathBuf::from("/out/linux-image-6.9.0-1-arm64_1_arm64.deb"),
            PathBuf::from("/out/u-boot-turing-rk1_1_arm64.deb"),
        ];
        let pkgs = kernel_packages(&Some(built), &repo).unwrap();
        assert_eq!(pkgs, vec!["linux-image-6.12.0-1-arm64".to_string()]);
    }

    #[test]
    fn kernel_packages_standalone_uses_sole_kernel_deb() {
        // Standalone --stage rootfs: exactly one kernel deb in out_dir is unambiguous.
        let repo = vec![
            PathBuf::from("/out/linux-image-6.12.0-1-arm64_1_arm64.deb"),
            PathBuf::from("/out/u-boot-turing-rk1_1_arm64.deb"),
        ];
        let pkgs = kernel_packages(&None, &repo).unwrap();
        assert_eq!(pkgs, vec!["linux-image-6.12.0-1-arm64".to_string()]);
    }

    #[test]
    fn kernel_packages_standalone_errors_on_stale_ambiguity() {
        // Two distinct kernel versions from earlier builds sharing an out_dir must
        // not be silently guessed — the rootfs stage refuses rather than pick one.
        let repo = vec![
            PathBuf::from("/out/linux-image-6.12.0-1-arm64_1_arm64.deb"),
            PathBuf::from("/out/linux-image-6.9.0-1-arm64_1_arm64.deb"),
        ];
        let err = kernel_packages(&None, &repo).unwrap_err().to_string();
        assert!(err.contains("multiple kernel packages"), "{err}");
    }

    #[test]
    fn kernel_packages_none_when_no_kernel_deb() {
        let repo = vec![PathBuf::from("/out/u-boot-turing-rk1_1_arm64.deb")];
        assert!(kernel_packages(&None, &repo).unwrap().is_empty());
    }

    #[test]
    fn ledger_ingests_only_recorded_debs_not_strays() {
        // TRUST-3: the local repo seed is the recorded artifact set, never an
        // extension-only scan — a stray .deb dropped into out_dir is not ingested.
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path();
        let recorded = out.join("librockchip-mpp1_1.5.0-1_arm64.deb");
        std::fs::write(&recorded, b"deb").unwrap();
        record_artifacts(out, std::slice::from_ref(&recorded)).unwrap();
        // Recording is idempotent (re-recording the same deb keeps one entry).
        record_artifacts(out, std::slice::from_ref(&recorded)).unwrap();
        // A stray unsigned deb the build never recorded.
        std::fs::write(out.join("evil_1.0_arm64.deb"), b"deb").unwrap();

        let debs = ledger_debs(out).unwrap();
        assert_eq!(debs, vec![recorded.clone()], "only the recorded deb is ingested");

        // A recorded deb whose file was removed is silently skipped.
        std::fs::remove_file(&recorded).unwrap();
        assert!(ledger_debs(out).is_err(), "empty existing set is an error");
    }

    #[test]
    fn ledger_missing_is_a_clear_error() {
        // No compile stage staged into this out_dir → a hard error pointing at the
        // compile stages, not a silent empty repo (TRUST-3).
        let dir = tempfile::tempdir().unwrap();
        let err = ledger_debs(dir.path()).unwrap_err().to_string();
        assert!(err.contains("run the compile stages first"), "{err}");
    }

    /// The boot2deb repo root (two levels up from this crate's manifest), for tests
    /// that resolve the shipped config.
    fn repo_root() -> ConfigRoot {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap()
            .to_path_buf();
        ConfigRoot::new(dir)
    }

    #[test]
    fn preflight_accepts_shipped_config_and_rejects_bad_geometry_or_fragment() {
        // CFG-4: geometry + fragment existence are validated up front (by both update
        // and build), so a bad axis fails at resolution, not deep in the build.
        let root = repo_root();
        let resolved = resolve_recipe(&root, "turing-rk1-forky", &Overrides::default()).unwrap();
        // The shipped RK1 config passes.
        preflight_config(&root, &resolved).unwrap();

        // A nonsensical rootfs offset (the review's own probe value) is rejected.
        let mut bad_geom = resolved.clone();
        bad_geom.offsets.rootfs = "1".to_string();
        assert!(preflight_config(&root, &bad_geom).is_err(), "bad geometry must fail preflight");

        // A referenced-but-missing kernel fragment is rejected.
        let mut bad_frag = resolved.clone();
        bad_frag.kernel.config_fragments.push("definitely-no-such-fragment".to_string());
        let err = preflight_config(&root, &bad_frag).unwrap_err().to_string();
        assert!(err.contains("fragment not found"), "expected a fragment error, got: {err}");
    }
}
