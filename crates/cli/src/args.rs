//! The clap surface: the command tree, each subcommand's argument group, and the
//! value parsers that turn flag strings into the typed model. Pure — parsing and
//! validation only; the handlers in [`crate::commands`] own every side effect.

use boot2deb_core::lock::SnapshotMode;
use boot2deb_core::model::{BootMethod, Keymap, Layout, Overrides};
use boot2deb_core::profile::Scope;
use clap::{Args, Parser, Subcommand, ValueEnum};
use std::path::PathBuf;

/// The `boot2deb` binary's argument tree: the global config-root/overlay/output
/// flags plus the subcommand.
#[derive(Parser)]
#[command(name = "boot2deb", version, about = "Device → Debian builder")]
pub(crate) struct Cli {
    /// Config root (the boot2deb repo dir holding devices/, socs/, ...).
    #[arg(long, global = true, default_value = ".")]
    pub(crate) root: PathBuf,

    /// Out-of-tree overlay directory holding your own devices/, socs/, kernels/,
    /// features/, or recipes/ files. Repeatable; later overlays win, and any
    /// overlay wins over the shipped root — a same-named layer is deep-merged
    /// last-wins, a new-named one adds a target. Fragments/blobs/overlay trees an
    /// overlay ships are resolved along the same path.
    #[arg(long = "overlay", global = true)]
    pub(crate) overlay: Vec<PathBuf>,

    /// Machine-readable output: `list-*` and `resolve` print a JSON document;
    /// `build` streams NDJSON events (one JSON object per line, tagged by its
    /// `event` field, artifacts included) instead of the human rendering.
    /// Other commands are unaffected. Errors still go to stderr as text.
    #[arg(long, global = true)]
    pub(crate) json: bool,

    #[command(subcommand)]
    pub(crate) command: Command,
}

/// The subcommands, each dispatched to its handler in [`crate::commands`].
#[derive(Subcommand)]
pub(crate) enum Command {
    /// List available devices.
    ListDevices,
    /// List available recipes.
    ListRecipes,
    /// List available kernel definitions (the `--kernel` override's valid values).
    ListKernels,
    /// List available rootfs features (the `--feature` override's valid values).
    ListFeatures,
    /// Scaffold a new `devices/<name>.toml` (and, by default, a matching recipe)
    /// from the typed model: it offers the valid SoC/boot-method/kernel/feature
    /// choices, fills every derivable value, and marks the researched values
    /// (`kernel_dtb`, `uboot_defconfig`, the rkbin blobs) with `# TODO:` comments.
    /// Interactive on a terminal; drive it with flags for scripting. Writes into the
    /// highest-precedence `--overlay` when one is given, else the primary root.
    NewDevice {
        /// Device name — the `devices/<name>.toml` (and recipe) file stem.
        name: String,
        #[command(flatten)]
        args: NewDeviceArgs,
    },
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
pub(crate) enum StageArg {
    /// The full pipeline: kernel, u-boot, userspace, ffmpeg, rootfs, then the
    /// disk image — a complete device image from the lock.
    All,
    /// Only the kernel `.deb`s.
    Kernel,
    /// Only the board DTB, rebuilt in the already-patched kernel tree — the
    /// board-bring-up loop (edit the `device_dts` source, rebuild, reflash) without a
    /// full kernel build.
    Dtb,
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

/// `build`'s flags: the stage selector, per-tree clone-source overrides, the
/// scratch/output locations, and the lock-independent image + cache knobs.
#[derive(Args)]
pub(crate) struct BuildArgs {
    /// Which stage(s) to run.
    #[arg(long, value_enum, default_value_t = StageArg::All)]
    pub(crate) stage: StageArg,
    /// Kernel clone source (git URL or local path); default: the kernel
    /// definition's source URL. A local clone (e.g. ../linux) is far faster.
    #[arg(long)]
    pub(crate) kernel_src: Option<String>,
    /// u-boot clone source (git URL or local path); default: the boot method's
    /// `uboot_source`.
    #[arg(long)]
    pub(crate) uboot_src: Option<String>,
    /// MPP clone source (git URL or local path); default: the SoC layer's
    /// `userspace.mpp` URL. A local checkout (e.g. ../mpp-rockchip) is far faster.
    #[arg(long)]
    pub(crate) mpp_src: Option<String>,
    /// librga clone source; default: the SoC layer's `userspace.librga` URL.
    #[arg(long)]
    pub(crate) librga_src: Option<String>,
    /// libmali clone source; default: the SoC layer's `userspace.libmali` URL.
    #[arg(long)]
    pub(crate) libmali_src: Option<String>,
    /// ffmpeg base (Kwiboo) clone source; default: the SoC layer's `ffmpeg.base`
    /// URL. A local checkout makes the fetch near-instant.
    #[arg(long)]
    pub(crate) ffmpeg_base_src: Option<String>,
    /// Also build the Mali userspace (off by default — unused on a headless box).
    #[arg(long)]
    pub(crate) build_libmali: bool,
    /// `patches` repo checkout the series is read from. Omit to use `../patches`
    /// (if present, with the lock's `patches.commit` enforced), else auto-fetch the
    /// series at the pinned commit from `--patches-url`/the kernel's `patches_url`.
    /// Pass an explicit path to co-develop the series from a working checkout,
    /// which downgrades a pin mismatch to a loud warning.
    #[arg(long)]
    pub(crate) patches_path: Option<PathBuf>,
    /// Clone URL for auto-fetching the `patches` series when no local checkout is
    /// present; default: the kernel definition's `patches_url`. The series is
    /// fetched at the lock's `patches.commit` into a durable cache and its pin
    /// enforced. Ignored when `--patches-path` or `../patches` supplies a checkout.
    #[arg(long)]
    pub(crate) patches_url: Option<String>,
    /// Vendored rkbin blob directory (default: blobs/SOC under the config root).
    #[arg(long)]
    pub(crate) blobs_dir: Option<PathBuf>,
    /// Debian archive keyring for the cross sandbox bootstrap (default: the
    /// vendored blobs/keyrings/debian-archive-keyring.gpg; omit on a Debian host
    /// to use its apt trust store).
    #[arg(long)]
    pub(crate) keyring: Option<PathBuf>,
    /// Trust an overlay-shipped copy of the archive keyring. By default an overlay
    /// that ships blobs/keyrings/debian-archive-keyring.gpg is refused as a
    /// trust-anchor swap (TRUST-1); this opts into the overlay's copy explicitly.
    #[arg(long)]
    pub(crate) unsafe_overlay_keyring: bool,
    /// Scratch dir for clones + builds (default: build/RECIPE).
    #[arg(long)]
    pub(crate) work_dir: Option<PathBuf>,
    /// Where produced artifacts are staged (default: WORK_DIR/artifacts).
    #[arg(long)]
    pub(crate) out_dir: Option<PathBuf>,
    /// `make -j` parallelism (default: host available parallelism). Must be at
    /// least 1 — 0 would reach `make -j0` ("unlimited"), never what a typo means.
    #[arg(long, value_parser = parse_jobs)]
    pub(crate) jobs: Option<usize>,
    /// Rootfs `tar` archive for the image stage. Optional: `--stage image`
    /// otherwise uses the tar the rootfs stage produced (auto-discovered in the
    /// output dir), so this is only needed to point at a tar built elsewhere.
    #[arg(long)]
    pub(crate) rootfs_tar: Option<PathBuf>,
    /// ext4 volume label / GPT partition name for the image rootfs.
    #[arg(long, default_value = "rootfs")]
    pub(crate) rootfs_label: String,
    /// Skip `.xz` compression of the finished image(s).
    #[arg(long)]
    pub(crate) no_compress: bool,
    /// Keep the raw `.img` after compressing it (default: delete it once the `.xz`
    /// is written, since it is derivable and the largest artifact). Conflicts with
    /// `--no-compress`, under which the raw image is the only output anyway.
    #[arg(long, conflicts_with = "no_compress")]
    pub(crate) keep_raw: bool,
    /// Image layout override (`combined` | `split`); default: the recipe/device
    /// layout. Lock-independent — it changes only image packaging, not any pinned
    /// source, so it is safe to set against an existing lock.
    #[arg(long, value_parser = parse_layout)]
    pub(crate) layout: Option<Layout>,
    /// Image-size override (e.g. `4G`); default: the recipe/device `image_size`.
    /// Lock-independent — it changes only image geometry, not any pinned source.
    #[arg(long = "image-size")]
    pub(crate) image_size: Option<String>,
    /// Snapshot activation for the rootfs bootstrap: `off` (live mirror),
    /// `fallback` (live first, `snapshot.debian.org` fills 404s), `pin` (snapshot
    /// only, fully deterministic). Default: the lock's captured mode (off if none).
    /// `fallback`/`pin` need a captured snapshot (`--save-snapshot`).
    #[arg(long, value_parser = parse_snapshot_mode)]
    pub(crate) snapshot: Option<SnapshotMode>,
    /// After a successful build, capture the current UTC time as a
    /// `snapshot.debian.org` timestamp into the lock (dormant, `mode = off`), so the
    /// solved versions stay fetchable after they rotate off the live mirror; a later
    /// build activates it with `--snapshot fallback|pin`.
    #[arg(long)]
    pub(crate) save_snapshot: bool,
    /// After the rootfs stage, commit the solved package manifest beside the lock
    /// and record its sha256 in the lock (`[rootfs].manifest_sha256`) — the
    /// reproducibility pin later builds verify a fresh solve against.
    #[arg(long)]
    pub(crate) save_manifest: bool,
    /// Downgrade a solved-manifest drift from the committed pin to a warning instead
    /// of a hard error — for co-development or a knowingly-moved mirror. Re-pin
    /// deliberately with `--save-manifest` (which skips the drift check entirely,
    /// so combining the two is rejected as contradictory).
    #[arg(long, conflicts_with = "save_manifest")]
    pub(crate) allow_manifest_drift: bool,
    /// Ignore a rootfs cache hit and re-bootstrap, refreshing the stored tree.
    /// The cheap `--simulate` solve still runs — the rootfs cache keys on
    /// the *solved* set, so a moved mirror already rebuilds automatically; this is
    /// the manual escape when you want a clean bootstrap regardless.
    #[arg(long)]
    pub(crate) refresh_rootfs: bool,
    /// Disable the Tier-2 artifact cache: always recompile the kernel /
    /// u-boot / userspace / ffmpeg `.deb`s instead of restoring a stored output on a
    /// signature hit, and do not store this build's outputs. The durable store at
    /// `<root>/cache/artifacts` is left untouched.
    #[arg(long)]
    pub(crate) no_artifact_cache: bool,
}

/// `why-rebuild`'s flags: the work dir whose stamps are read, plus the two build
/// knobs that change what the prediction should assume.
#[derive(Args)]
pub(crate) struct WhyRebuildArgs {
    /// Build scratch dir to inspect (default: build/RECIPE) — must match the dir the
    /// build used, since the stamps live there.
    #[arg(long)]
    pub(crate) work_dir: Option<PathBuf>,
    /// The build being reasoned about used an explicit `--patches-path` co-dev
    /// checkout (folded into the kernel/u-boot/ffmpeg signatures). Pass the same
    /// value so the prediction matches what that build would reuse.
    #[arg(long)]
    pub(crate) patches_path: Option<PathBuf>,
    /// Include the optional libmali userspace node (built only with
    /// `--build-libmali`).
    #[arg(long)]
    pub(crate) build_libmali: bool,
}

/// `clean`'s flags: which subtree to remove, and the two safety knobs
/// (`--dry-run` preview, `--force` past the ownership stamp).
#[derive(Args)]
pub(crate) struct CleanArgs {
    /// Build scratch dir to clean (default: build/RECIPE).
    #[arg(long)]
    pub(crate) work_dir: Option<PathBuf>,
    /// Remove only the rootfs early-cutoff cache (WORK_DIR/cache), keeping the
    /// compiled source trees and artifacts.
    #[arg(long)]
    pub(crate) cache: bool,
    /// Remove only the bootstrapped cross-build sandbox (WORK_DIR/sandbox) — the
    /// largest single reclaimable tree.
    #[arg(long)]
    pub(crate) sandbox: bool,
    /// Remove the durable Tier-2 artifact store (`<root>/cache/artifacts`).
    /// Unlike the other selectors this store is shared across recipes, so this
    /// clears cached outputs for *every* recipe, not just this one.
    #[arg(long)]
    pub(crate) artifacts: bool,
    /// Show what would be removed (with sizes) without removing anything.
    #[arg(long)]
    pub(crate) dry_run: bool,
    /// Remove the work dir even when it is not stamped as boot2deb-created (no
    /// `.boot2deb-work` marker). Without this, `clean` refuses such a target, so
    /// a mistyped `--work-dir` cannot recursively delete an arbitrary tree.
    #[arg(long)]
    pub(crate) force: bool,
}

/// `new-device`'s flags: one per scaffolded axis (each prompted when omitted on a
/// terminal), plus the write-behavior knobs.
#[derive(Args)]
pub(crate) struct NewDeviceArgs {
    /// Board description. Prompted if omitted on a terminal.
    #[arg(long)]
    pub(crate) description: Option<String>,
    /// SoC (e.g. rk3588). Must already have a `socs/<soc>.toml`. Prompted if
    /// omitted on a terminal; required otherwise.
    #[arg(long)]
    pub(crate) soc: Option<String>,
    /// Boot method (e.g. rockchip-rkbin). Prompted/defaulted if omitted.
    #[arg(long)]
    pub(crate) boot_method: Option<String>,
    /// Kernel definition id (e.g. rk3588-mainline-7.1). Must support the chosen
    /// SoC. Prompted/defaulted if omitted.
    #[arg(long)]
    pub(crate) kernel: Option<String>,
    /// Default Debian suite. Prompted/defaulted (forky) if omitted.
    #[arg(long)]
    pub(crate) suite: Option<String>,
    /// Default image layout (combined | split). Prompted/defaulted if omitted.
    #[arg(long)]
    pub(crate) layout: Option<String>,
    /// Default image hostname. Defaults to the device name.
    #[arg(long)]
    pub(crate) hostname: Option<String>,
    /// Default image size (e.g. 2G). Prompted/defaulted if omitted.
    #[arg(long)]
    pub(crate) image_size: Option<String>,
    /// A feature the scaffolded recipe selects (repeatable). Must be compatible with
    /// the chosen SoC/arch. Prompted from the compatible set on a terminal.
    #[arg(long = "feature")]
    pub(crate) features: Vec<String>,
    /// Do not scaffold a recipe — write only the device file.
    #[arg(long)]
    pub(crate) no_recipe: bool,
    /// Overwrite existing files instead of refusing.
    #[arg(long)]
    pub(crate) force: bool,
    /// Never prompt; take every value from flags/defaults. Implied when stdin is
    /// not a terminal.
    #[arg(long)]
    pub(crate) non_interactive: bool,
}

/// `update`'s flags: the per-tree refs to pin (each inheriting the previous lock's
/// pin when omitted) plus the blob/patches/manifest inputs.
#[derive(Args)]
pub(crate) struct UpdateArgs {
    /// Kernel ref to pin, resolved to a commit (e.g. v7.1.1). Optional once a lock
    /// exists: omitting it re-pins the *previous lock's* kernel ref, so a routine
    /// re-pin (e.g. after importing a patch) needs no kernel tag the user did not
    /// touch. Required only for the first update, which has no prior ref to inherit.
    /// Auto-resolving a kernel `track` to its latest tag is a later refinement.
    #[arg(long)]
    pub(crate) kernel_ref: Option<String>,
    /// u-boot ref to pin (default: the boot-method's `uboot_ref`).
    #[arg(long)]
    pub(crate) uboot_ref: Option<String>,
    /// MPP source ref to pin (default: the SoC layer's `userspace.mpp` ref).
    #[arg(long)]
    pub(crate) mpp_ref: Option<String>,
    /// librga source ref to pin (default: the SoC layer's `userspace.librga`).
    #[arg(long)]
    pub(crate) librga_ref: Option<String>,
    /// libmali source ref to pin (default: the SoC layer's `userspace.libmali`).
    #[arg(long)]
    pub(crate) libmali_ref: Option<String>,
    /// ffmpeg base (V4L2) ref to pin (default: the SoC layer's `ffmpeg.base`).
    #[arg(long)]
    pub(crate) ffmpeg_base_ref: Option<String>,
    /// ffmpeg Rockchip provenance-tree ref to pin (default: the SoC layer's
    /// `ffmpeg.rockchip`). Recorded as the graft's provenance; not fetched.
    #[arg(long)]
    pub(crate) ffmpeg_rockchip_ref: Option<String>,
    /// `patches` repo checkout whose HEAD pins the series. `update` requires this
    /// local clone when the kernel names a patch profile — the pin *is* its HEAD —
    /// unlike `build`, which auto-fetches the already-pinned commit and needs no
    /// checkout.
    #[arg(long, default_value = "../patches")]
    pub(crate) patches_path: PathBuf,
    /// Vendored rkbin blob directory (default: blobs/SOC under the config root).
    #[arg(long)]
    pub(crate) blobs_dir: Option<PathBuf>,
    /// Name recorded for the solved package manifest the rootfs stage writes
    /// (default: RECIPE.pkgs.lock).
    #[arg(long)]
    pub(crate) rootfs_manifest: Option<String>,
}

/// `verify-patches`' flags: an explicit checkout per source tree, or the clone
/// source to auto-fetch it from at the locked pin.
#[derive(Args)]
pub(crate) struct VerifyArgs {
    /// Kernel checkout to verify the kernel series against. Optional: omit it and
    /// the locked kernel is auto-fetched at its pinned ref into a durable cache, so
    /// verification works on a fresh clone with no hand-cloned tree.
    #[arg(long)]
    pub(crate) kernel_path: Option<PathBuf>,
    /// Kernel clone source (git URL or local path) for the auto-fetch, in place of
    /// the kernel definition's upstream URL. A local checkout (e.g. ../linux) that
    /// holds the locked commit makes the fetch near-instant. Ignored with
    /// `--kernel-path`, and only used on the first materialization (the cache keys on
    /// the commit, so later runs are hits regardless).
    #[arg(long)]
    pub(crate) kernel_src: Option<String>,
    /// ffmpeg checkout to verify the ffmpeg series against. Optional: omit it and,
    /// when the profile carries ffmpeg patches, the locked ffmpeg base is
    /// auto-fetched at its pin.
    #[arg(long)]
    pub(crate) ffmpeg_path: Option<PathBuf>,
    /// ffmpeg base clone source (git URL or local path) for the auto-fetch, in place
    /// of the SoC layer's `ffmpeg.base` URL. A local checkout makes the fetch
    /// near-instant. Ignored with `--ffmpeg-path`.
    #[arg(long)]
    pub(crate) ffmpeg_base_src: Option<String>,
    /// Userspace (MPP/RGA) checkout to verify the userspace series against. Optional:
    /// omit it and, when the profile carries userspace patches, the locked MPP tree
    /// is auto-fetched at its pin.
    #[arg(long)]
    pub(crate) userspace_path: Option<PathBuf>,
    /// MPP clone source (git URL or local path) for the auto-fetch, in place of the
    /// SoC layer's `userspace.mpp` URL. A local checkout (e.g. ../mpp-rockchip) makes
    /// the fetch near-instant. Ignored with `--userspace-path`.
    #[arg(long)]
    pub(crate) mpp_src: Option<String>,
    /// `patches` repo checkout the profile + patches are read from. Omit to use
    /// `../patches` if present, else auto-fetch the series at the lock's
    /// `patches.commit`.
    #[arg(long)]
    pub(crate) patches_path: Option<PathBuf>,
    /// Clone URL for auto-fetching the `patches` series when no local checkout is
    /// present; default: the kernel definition's `patches_url`.
    #[arg(long)]
    pub(crate) patches_url: Option<String>,
}

/// `verify-config`'s flags: the kernel tree to configure (explicit or auto-fetched)
/// and the optional reference `.config` to check parity against.
#[derive(Args)]
pub(crate) struct ConfigArgs {
    /// Kernel checkout (at the locked ref, patch series applied) to configure.
    /// Optional: omit it and the locked kernel is auto-fetched at its pinned ref and
    /// the kernel patch series applied for you, so the gate works on a fresh clone.
    #[arg(long)]
    pub(crate) kernel_path: Option<PathBuf>,
    /// Reference `.config` to check byte-identical `CONFIG_*` parity against. Omit
    /// for a clean-merge check only.
    #[arg(long)]
    pub(crate) reference_config: Option<PathBuf>,
    /// Directory for the two out-of-tree config builds (default: a temp dir).
    #[arg(long)]
    pub(crate) work_dir: Option<PathBuf>,
    /// Kernel clone source (git URL or local path) for the auto-fetch, in place of
    /// the kernel definition's upstream URL. A local checkout (e.g. ../linux) that
    /// holds the locked commit makes the fetch near-instant. Ignored with
    /// `--kernel-path`.
    #[arg(long)]
    pub(crate) kernel_src: Option<String>,
    /// `patches` repo checkout the kernel series is read from when auto-fetching the
    /// tree (ignored with `--kernel-path`, which is assumed already patched). Omit to
    /// use `../patches` if present, else auto-fetch at the lock's `patches.commit`.
    #[arg(long)]
    pub(crate) patches_path: Option<PathBuf>,
    /// Clone URL for auto-fetching the `patches` series; default: the kernel
    /// definition's `patches_url`. Used only when auto-fetching the kernel tree.
    #[arg(long)]
    pub(crate) patches_url: Option<String>,
}

/// `patch`'s subcommands.
#[derive(Subcommand)]
pub(crate) enum PatchAction {
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

/// `patch import`'s flags: where the patch lands (profile, scope, position, name)
/// and how it is verified before the profile edit is kept.
#[derive(Args)]
pub(crate) struct PatchImportArgs {
    /// Profile to slot the patch into (e.g. rk3588-accel) — names
    /// `profiles/<name>/profile.toml` in the patches repo.
    #[arg(long)]
    pub(crate) profile: String,
    /// Which source tree's ordered list to insert into.
    #[arg(long, value_parser = parse_scope)]
    pub(crate) scope: Scope,
    /// 1-based position in the scope list to insert at (default: append to the
    /// end). 0 or a value past one-beyond-the-end is an error, not a clamp.
    #[arg(long)]
    pub(crate) position: Option<usize>,
    /// Repo subdirectory to write the patch into (default: `media-accel/<scope>`).
    /// Use e.g. `rocket` to target the NPU scope of the kernel list.
    #[arg(long)]
    pub(crate) dest_dir: Option<String>,
    /// Filename slug override (default: a kebab-case slug of the subject). The
    /// written file is `<dest-dir>/<prefix>-<slug>.patch`.
    #[arg(long)]
    pub(crate) name: Option<String>,
    /// Explicit repo-relative destination label, overriding the derived
    /// dir/prefix/slug entirely (e.g. `media-accel/kernel/045-fix.patch`).
    #[arg(long = "as")]
    pub(crate) label: Option<String>,
    /// `From:` author for a synthesized header (bare diff / `git show` fallback).
    #[arg(long, default_value = "boot2deb import <import@boot2deb>")]
    pub(crate) author: String,
    /// Subject override — the title for a bare diff carrying none, or an override
    /// for `git show`. Ignored for an already-formatted mbox.
    #[arg(long)]
    pub(crate) subject: Option<String>,
    /// DEP-3 `Origin:` provenance trailer to add to the commit message.
    #[arg(long)]
    pub(crate) origin: Option<String>,
    /// `patches` repo checkout to write into (default: `../patches`). `patch
    /// import` requires this local clone — it writes the patch file and edits the
    /// profile there — unlike `build`, which auto-fetches pinned commits.
    #[arg(long, default_value = "../patches")]
    pub(crate) patches_path: PathBuf,
    /// Source checkout to dry-run `git am`-verify the spliced series against.
    /// Omit to import without verifying (a warning is printed).
    #[arg(long)]
    pub(crate) verify_tree: Option<PathBuf>,
    /// Overwrite the destination file if it already exists (default: refuse).
    #[arg(long)]
    pub(crate) force: bool,
}

/// The axis overrides `resolve` and `doctor` accept, mapped onto [`Overrides`].
#[derive(Args, Default)]
pub(crate) struct OverrideArgs {
    #[arg(long)]
    pub(crate) kernel: Option<String>,
    #[arg(long)]
    pub(crate) suite: Option<String>,
    #[arg(long, value_parser = parse_layout)]
    pub(crate) layout: Option<Layout>,
    #[arg(long = "boot-method", value_parser = parse_boot_method)]
    pub(crate) boot_method: Option<BootMethod>,
    /// Depthcharge board profile (e.g. `speedy-libreboot`). A profile describes the
    /// *firmware* a unit runs, not the board model — so a unit with replacement
    /// firmware may take a different one. Must be in the device's
    /// `supported_boards`; ignored by boot methods with no board profile.
    #[arg(long)]
    pub(crate) board: Option<String>,
    /// Rootfs feature add-in, repeatable (`--feature media-accel-rockchip`). When
    /// any is given, replaces the recipe's feature list.
    #[arg(long = "feature")]
    pub(crate) features: Vec<String>,
    #[arg(long = "image-size")]
    pub(crate) image_size: Option<String>,
    /// System locale — the image's `LANG` (e.g. `de_DE.UTF-8`); default: the
    /// recipe/base `locale`. Always generated into the image, so it is safe to name a
    /// locale nothing else lists.
    #[arg(long)]
    pub(crate) locale: Option<String>,
    /// Extra locale to generate into the image, repeatable (`--locale-gen
    /// fr_FR.UTF-8`). When any is given, replaces the base `locales_generate` list;
    /// the system locale is generated regardless.
    #[arg(long = "locale-gen")]
    pub(crate) locales_generate: Vec<String>,
    /// System timezone (e.g. `America/New_York`); default: the recipe/base `timezone`.
    #[arg(long)]
    pub(crate) timezone: Option<String>,
    /// Console keyboard layout (e.g. `gb`); default: the recipe/device `keymap`, and
    /// none at all on a headless board. Sets `XKBLAYOUT`; the model, variant, and
    /// options keep their defaults — set those in the device's `[keymap]` table.
    #[arg(long)]
    pub(crate) keymap: Option<String>,
}

impl From<OverrideArgs> for Overrides {
    fn from(a: OverrideArgs) -> Self {
        Overrides {
            kernel: a.kernel,
            suite: a.suite,
            layout: a.layout,
            boot_method: a.boot_method,
            board: a.board,
            features: (!a.features.is_empty()).then_some(a.features),
            image_size: a.image_size,
            locale: a.locale,
            locales_generate: (!a.locales_generate.is_empty()).then_some(a.locales_generate),
            timezone: a.timezone,
            keymap: a.keymap.as_deref().map(Keymap::from_layout),
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

/// Parse `--jobs`: a positive `make -j` count. 0 is rejected — `make -j0` means
/// "unlimited", which is never what a typo intends.
fn parse_jobs(s: &str) -> Result<usize, String> {
    match s.parse::<usize>() {
        Ok(0) => Err("must be at least 1 (omit --jobs to use all cores)".into()),
        Ok(n) => Ok(n),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_jobs_rejects_zero() {
        assert_eq!(parse_jobs("4"), Ok(4));
        assert!(parse_jobs("0").unwrap_err().contains("at least 1"));
        assert!(parse_jobs("x").is_err());
    }

    #[test]
    fn the_command_tree_is_well_formed() {
        // clap's own consistency checks (duplicate flags, bad conflicts_with targets,
        // ill-formed defaults) run here rather than surfacing as a runtime panic.
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
