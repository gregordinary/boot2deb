//! The clap surface: the command tree, each subcommand's argument group, and the
//! value parsers that turn flag strings into the typed model. Pure — parsing and
//! validation only; the handlers in [`crate::commands`] own every side effect.

use crate::commands;
use boot2deb_core::lock::SnapshotMode;
use boot2deb_core::model::{BootMethod, Keymap, Layout, Overrides, SudoPolicy};
use boot2deb_core::series::Scope;
use boot2deb_engine::image::ImageCompression;
use clap::{Args, Parser, Subcommand, ValueEnum};
use clap_complete::Shell;
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

    /// Machine-readable output: `list-*`, `resolve`, `doctor`, and the `verify-*`
    /// commands print a JSON document; `build` streams NDJSON events (one JSON object
    /// per line, tagged by its `event` field, artifacts included) instead of the human
    /// rendering. A command with no machine form rejects the flag rather than ignoring
    /// it. Errors still go to stderr as text.
    #[arg(long, global = true)]
    pub(crate) json: bool,

    /// Print only what a command produced — artifact paths and errors — and none of
    /// its progress. Conflicts with `--verbose`; ignored under `--json`, where the
    /// stream is the record.
    #[arg(long, short, global = true, conflicts_with = "verbose")]
    pub(crate) quiet: bool,

    /// Print every line the build's subprocesses emit (`make`, `git`,
    /// `dpkg-buildpackage`) as well as the step boundaries and each stage's own
    /// decisions. The default shows the latter only, which keeps a tens-of-minutes
    /// compile readable; reach for this when a stage fails or hangs.
    #[arg(long, short, global = true)]
    pub(crate) verbose: bool,

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
    /// List available out-of-tree kernel-module sets (a device's `device_kmods`
    /// entries).
    ListKmods,
    /// Print the support matrix: each shipped recipe's support claim joined to the
    /// exact pins its lock records.
    SupportMatrix {
        /// Emit the `docs/src/reference/support-matrix.md` page verbatim, for
        /// regenerating it after a claim changes or a lock is re-pinned.
        #[arg(long)]
        markdown: bool,
    },
    /// Print the complete flag reference: every command's positional arguments and
    /// flags, generated from this command tree so it cannot drift from the binary.
    /// `--help` answers this per command; this answers it for all of them at once.
    CliReference {
        /// Emit the `docs/src/reference/cli-flags.md` page verbatim, for regenerating
        /// it after a flag is added, removed, or re-described.
        #[arg(long)]
        markdown: bool,
    },
    /// Print a shell completion script on stdout, for the shell named. Install it
    /// where your shell looks (e.g. `boot2deb completions bash > \
    /// ~/.local/share/bash-completion/completions/boot2deb`); boot2deb writes no files
    /// itself, since where they belong is the packager's call.
    Completions {
        /// Shell to generate for.
        #[arg(value_enum)]
        shell: Shell,
    },
    /// Print the `boot2deb(1)` man page (roff) on stdout, e.g.
    /// `boot2deb man > /usr/share/man/man1/boot2deb.1`.
    Man,
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
        /// Device name (e.g. turing-rk1) or recipe name (e.g. turing-rk1/forky).
        target: String,
        #[command(flatten)]
        overrides: OverrideArgs,
    },
    /// Preflight the host: arch/OS facts, and whether every tool a build needs is
    /// present — with the exact per-distro install command for anything missing. With
    /// a target it asks only for what *that* recipe will invoke; bare, it runs the
    /// requirements every board shares. A missing required tool is a non-zero exit.
    Doctor {
        /// Device/recipe to preflight. Omit to check only the requirements no board
        /// can opt out of (user namespaces, the `.deb` packaging tools, the vendored
        /// apt trust anchors) — the answerable half before a board is chosen.
        target: Option<String>,
        /// Scratch dir the target would build in; default: `<root>/build/<target>`. Only the
        /// overlay check reads it — it probes the filesystem that dir lands on, so
        /// checking a build you will run with `--work-dir` needs the same path here.
        #[arg(long)]
        work_dir: Option<std::path::PathBuf>,
        #[command(flatten)]
        overrides: OverrideArgs,
    },
    /// Resolve upstream refs + hash blobs and write the recipe's `.lock`.
    /// The sole path that consults upstream; `build` reads only the lock.
    Update {
        /// Recipe to resolve (e.g. turing-rk1/forky).
        recipe: String,
        #[command(flatten)]
        args: UpdateArgs,
    },
    /// Dry-run the locked patch series against source checkouts with
    /// `git am --3way`, hard-erroring on the first patch that does not apply.
    VerifyPatches {
        /// Recipe whose lock names the kernel ref + patch series.
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
    /// Ask the archives a build would resolve against whether they carry every
    /// package the recipe names, and report the ones they do not. Runs the read half
    /// of a resolve — release and indexes, nothing downloaded, no closure computed —
    /// so one pass answers every name at once, before any build work starts.
    VerifyPackages {
        /// Recipe whose resolved package set to check (e.g. turing-rk1/forky).
        recipe: String,
    },
    /// Hold a finished image artifact to the invariants that are checkable without a
    /// board: the artifact set is present, the plan document parses and its digest
    /// matches what the provenance records, `[[archives]]` is well formed, the ext4
    /// filesystem is exactly its GPT partition, and a fitted `--image-size` left the
    /// slack it asked for. Read-only, no root: only the head of the artifact is
    /// decompressed. The off-board half of the hardware gate.
    VerifyImage {
        /// Recipe whose built image to verify (e.g. turing-rk1/forky).
        recipe: String,
        /// Directory holding the built artifacts (default: the recipe's own
        /// `<work>/artifacts`).
        #[arg(long)]
        out_dir: Option<PathBuf>,
    },
    /// Probe each locked source pin against its *configured* upstream URL and
    /// report whether it is a durable tag, an ephemeral branch, or ORPHANED (not
    /// re-fetchable) — the source-pin durability survey as a command.
    /// Read-only: `git ls-remote` plus a timeout-bounded ancestry check, no build,
    /// no checkout, no hardware.
    VerifySources {
        /// Recipe whose lock names the source pins (e.g. turing-rk1/forky).
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
        /// Recipe to build (e.g. turing-rk1/forky); its `.lock` must exist.
        recipe: String,
        #[command(flatten)]
        args: BuildArgs,
    },
    /// Rebuild an image from the plan document a previous build published, instead of
    /// resolving the archive afresh. The lock pins the sources; the plan pins the
    /// package versions the archive served, which the lock cannot. Takes every `build`
    /// flag, and differs from it in one way: the rootfs installs the plan's exact set by
    /// the digests it records, reading neither a release nor a package index — so the
    /// plan, not an archive signature, is what those digests chain to.
    Reproduce {
        /// Recipe to reproduce (e.g. turing-rk1/forky); its `.lock` must exist.
        recipe: String,
        /// Directory holding the published `<stem>.plan` (and, for the builder
        /// advisory, `<stem>.provenance.toml`) — the directory the image shipped from.
        /// Default: this build point's own output dir, which is where a build on this
        /// machine already published them.
        #[arg(long)]
        from: Option<PathBuf>,
        #[command(flatten)]
        args: BuildArgs,
    },
    /// Compare two build points: the packages, the kernel pin and its requested
    /// config, the patch series and the patch files behind them, every other source
    /// pin, the rkbin blobs, and what built each side. Each side is a recipe name, a
    /// `.lock`, or a `.provenance.toml`; mixing is allowed, and a section only one
    /// side can answer is reported unavailable rather than as a change. Offline —
    /// reads documents the build already wrote.
    Diff {
        /// The left side: a recipe (e.g. turing-rk1/forky), or a path to a `.lock`
        /// or `.provenance.toml`.
        left: String,
        /// The right side, in any of the same forms.
        right: String,
        /// Report only these sections (repeatable). Default: all of them.
        #[arg(long = "section", value_enum)]
        sections: Vec<commands::diff::SectionArg>,
        /// `patches` checkout to resolve a moved patches commit into named files.
        /// Default: the config root's sibling `../patches`.
        #[arg(long)]
        patches_path: Option<PathBuf>,
    },
    /// Export an image's bill of materials as SPDX 2.3 or CycloneDX 1.6 JSON, from the
    /// provenance manifest and solved package manifest a build published. Lists every
    /// installed package with its version and sha256, every pinned source tree the image
    /// was compiled from, every rkbin blob, and every externally-fetched `.deb`.
    /// Licenses are declared NOASSERTION — boot2deb records none, and inventing them
    /// would produce a field that looks authoritative and is not. Offline; builds
    /// nothing.
    Sbom {
        /// Recipe whose published image to describe (e.g. turing-rk1/forky), or a path
        /// to a `.provenance.toml` shipped with an image.
        target: String,
        /// Document format to write.
        #[arg(long, value_enum, default_value = "spdx")]
        format: commands::sbom::FormatArg,
        /// Write to this file instead of stdout.
        #[arg(long)]
        out: Option<PathBuf>,
        /// Rootfs feature the published image was built with, repeatable — the same
        /// selection `build --feature` used. It names which image's documents to read;
        /// passing the reference directly (`sbom turing-rk1/forky+jellyfin`) is
        /// equivalent. Ignored when a `.provenance.toml` path is given, which already
        /// names one image.
        #[arg(long = "feature")]
        features: Vec<String>,
    },
    /// Break down what an image's package set weighs, from the plan document a build
    /// published — per binary package, per source package, or per repository. The
    /// figures are the archives' own `Installed-Size` estimates in kibibytes, so they
    /// answer "what did the packages contribute" and not "how large is the image":
    /// they exclude filesystem overhead and everything the image gains after `dpkg`.
    /// Offline; builds nothing.
    Size {
        /// Recipe whose published image to weigh (e.g. turing-rk1/forky), or a path to
        /// a `.plan` shipped with an image.
        target: String,
        /// Axis to roll up on: one row per binary package, per source package (which
        /// attributes a source's several outputs to the thing that was built), or per
        /// repository (which separates what Debian shipped from what this build
        /// compiled).
        #[arg(long, value_enum, default_value = "package")]
        by: commands::size::ByArg,
        /// Show only the heaviest N rows; `0` shows every row. The totals always
        /// describe the whole set, so a truncated view still says what it is a view of.
        #[arg(long, default_value = "25")]
        top: usize,
        /// Rootfs feature the published image was built with, repeatable — the same
        /// selection `build --feature` used. It names which image's plan to read;
        /// passing the reference directly (`size turing-rk1/forky+jellyfin`) is
        /// equivalent. Ignored when a `.plan` path is given, which already names one
        /// image.
        #[arg(long = "feature")]
        features: Vec<String>,
    },
    /// Survey what has moved upstream since the locks were pinned: for each recipe's
    /// git source pins, whether a newer release tag exists (and how far behind the
    /// pin is), or whether a pinned branch's tip has moved. Read-only — one
    /// `git ls-remote` per distinct remote, no fetch and no re-pin. Being behind is
    /// not a failure, so this always exits zero; `verify-sources` is the gate, and it
    /// answers the different question of whether a pin is still fetchable at all.
    Outdated {
        /// Recipes to survey (e.g. turing-rk1/forky). Default: every recipe in the
        /// config tree.
        recipes: Vec<String>,
    },
    /// Explain, per compile node, what the next `build` will actually redo: whether it
    /// reuses or rebuilds the cached source tree (naming the pinned input that moved),
    /// and whether the durable artifact cache lets it skip the compile entirely.
    /// Offline: reads the lock, the build stamps, and the artifact store; runs no
    /// build.
    WhyRebuild {
        /// Recipe to inspect (e.g. turing-rk1/forky); its `.lock` must exist.
        recipe: String,
        #[command(flatten)]
        args: WhyRebuildArgs,
    },
    /// Open an interactive shell in the root a build stage compiles in — the same base
    /// tree, the same layered build-dependencies, the same mounts and the same
    /// environment the compile has. The way to diagnose a failed compile by looking at
    /// it rather than by reading what it printed. Provisions the root if this work dir
    /// has none; needs a terminal.
    Shell {
        /// Recipe whose root to enter (e.g. turing-rk1/forky); its `.lock` must exist.
        recipe: String,
        #[command(flatten)]
        args: ShellArgs,
    },
    /// Remove a recipe's build scratch (clones, sandbox, rootfs cache) under its work
    /// dir, or sweep the durable caches every recipe shares, to reclaim disk or force
    /// a clean rebuild.
    Clean {
        /// Recipe whose build scratch to remove (e.g. turing-rk1/forky). Optional
        /// when every selector given is root-scoped (`--artifacts`,
        /// `--verify-trees`, `--kconfig`, `--all-caches`), since those name a shared
        /// store rather than one recipe's work dir.
        recipe: Option<String>,
        #[command(flatten)]
        args: CleanArgs,
    },
    /// Produce a ready-to-flash image file from a build's artifacts, verified and
    /// optionally personalized per unit (`--hostname`/`--ssh-key`/`--wifi-ssid` seed
    /// keys) or extended with per-site files (`--copy`/`--deb`/`--embed-image`, which
    /// re-assemble the image from the kept rootfs tar). boot2deb does not write
    /// devices — hand the pressed file to any flasher, `dd` included.
    Press {
        /// Recipe whose artifacts to press (e.g. turing-rk1/forky).
        recipe: String,
        /// The image file to write, for a build with one artifact (a combined image
        /// or a u-boot deliverable). A split build is two files for two media and
        /// takes `--boot-out` + `--rootfs-out` instead.
        output: Option<PathBuf>,
        #[command(flatten)]
        args: PressArgs,
    },
    /// Rewrite the per-unit seed partition of an already-pressed image file — the
    /// same personalization `press` applies, without re-pressing. With no keys the
    /// seed resets to the empty template. Takes a file: to re-personalize a card
    /// that is already written, edit `seed.txt` on its `B2D-SEED` volume directly.
    Seed {
        /// The pressed image file whose seed partition to rewrite.
        image: PathBuf,
        #[command(flatten)]
        args: SeedArgs,
    },
    /// Boot the built image under QEMU before it is flashed, and assert the
    /// userland works: systemd reaches multi-user with no failed unit, the
    /// generated password logs in, first-boot completes, the on-image selftest
    /// passes in userland mode — and a second boot of the same disk still does,
    /// the check no single-boot smoke test covers. Boots the suite's generic
    /// kernel as a fixture; the shipped kernel and the board are not under test.
    Try {
        /// Recipe whose built image to boot (e.g. turing-rk1/forky); run
        /// `boot2deb build` first.
        recipe: String,
        #[command(flatten)]
        args: TryArgs,
    },
}

/// A `--compress` value: one of the image containers, or `none`.
///
/// `none` is a value of this option rather than a separate `--no-compress` flag so
/// that "how is the image packaged" has exactly one spelling. It is only meaningful
/// on its own, which [`image_compression`] enforces.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ImageCompressionArg {
    /// `.xz` — the smallest artifact, and what an operator pipes into `dd`.
    Xz,
    /// `.gz` — larger, but the only container u-boot's `gzwrite` can read, so this
    /// is the one for an image a board writes to its own disk from the bootloader.
    Gz,
    /// Emit the raw `.img` only.
    None,
}

/// Resolve the `--compress` values into the engine's ordered container list.
///
/// Order is the operator's preference and is preserved; a format named twice is
/// kept once, at its first position, since emitting the same container twice would
/// just overwrite it. `none` mixed with a real format is rejected rather than
/// silently resolved either way — the two readings ("no compression" and "compress,
/// plus nothing") contradict, and guessing would delete a raw image the operator
/// asked to keep.
pub(crate) fn image_compression(
    args: &[ImageCompressionArg],
) -> Result<Vec<ImageCompression>, String> {
    if args.contains(&ImageCompressionArg::None) {
        return if args.len() == 1 {
            Ok(Vec::new())
        } else {
            Err("--compress none cannot be combined with a compression format".into())
        };
    }
    let mut out = Vec::with_capacity(args.len());
    for arg in args {
        let format = match arg {
            ImageCompressionArg::Xz => ImageCompression::Xz,
            ImageCompressionArg::Gz => ImageCompression::Gz,
            ImageCompressionArg::None => unreachable!("handled above"),
        };
        if !out.contains(&format) {
            out.push(format);
        }
    }
    Ok(out)
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
    /// Only the out-of-tree kernel-module `.deb`s, built against the kernel tree from
    /// the board's `device_kmods` (e.g. the AIC8800 Wi-Fi driver). Reuses an existing
    /// kernel tree when one is present, so it need not rebuild the kernel.
    Kmod,
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
    /// Rootfs feature to select, repeatable — the same selection `update --feature`
    /// pinned. It names which lock to build from (`<recipe>+<feature>...`), it does
    /// not re-resolve one: `update` must have written that variant's lock first, and
    /// a selection with no lock is an error naming the `update` line to run. Passing
    /// the reference directly (`build turing-rk1/forky+jellyfin`) is equivalent.
    #[arg(long = "feature")]
    pub(crate) features: Vec<String>,
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
    /// Media-accel userspace clone source, as `NAME=SRC`, repeatable; default: that
    /// tree's own `[[userspace]]` URL. The SoC declares which trees it has, so each
    /// override names one (`--userspace-src mpp=../mpp-rockchip`). A local checkout is
    /// far faster than a fresh clone. The clone is still made at the locked commit, so
    /// the named tree must contain it.
    #[arg(long = "userspace-src", value_name = "NAME=SRC", value_parser = parse_named_source)]
    pub(crate) userspace_srcs: Vec<(String, String)>,
    /// ffmpeg base (Kwiboo) clone source; default: the SoC layer's `ffmpeg.base`
    /// URL. A local checkout makes the fetch near-instant.
    #[arg(long)]
    pub(crate) ffmpeg_base_src: Option<String>,
    /// Out-of-tree module clone source, as `NAME=SRC`, repeatable; default: that
    /// kmod's locked `source`. Unlike the single-tree axes there are several modules,
    /// so each override names the `device_kmods` entry it applies to
    /// (`--kmod-src aic8800=../aic8800`). The clone is still made at the locked
    /// commit, so the named tree must contain it.
    #[arg(long = "kmod-src", value_name = "NAME=SRC", value_parser = parse_named_source)]
    pub(crate) kmod_srcs: Vec<(String, String)>,
    /// Also build an *optional* media-accel userspace tree, by name, repeatable.
    ///
    /// A tree the SoC marks `optional` is skipped unless named here: libmali is the
    /// live case — the transcode pipeline rides the VPU and the RGA, not the GPU, so a
    /// headless box never needs the blob and compiling its variant matrix is minutes
    /// for nothing. Naming an optional tree also changes what the *whole* userspace
    /// stage layers, so every tree's cache key moves with it.
    #[arg(long = "userspace", value_name = "NAME")]
    pub(crate) userspace: Vec<String>,
    /// `patches` repo checkout the series is read from. Omit to use the config
    /// root's sibling `../patches` (if present, with the lock's `patches.commit`
    /// enforced), else auto-fetch the series at the pinned commit from
    /// `--patches-url`/the repo the pin names. Pass an explicit path to
    /// co-develop the series from a working checkout, which downgrades a pin
    /// mismatch to a loud warning.
    #[arg(long)]
    pub(crate) patches_path: Option<PathBuf>,
    /// Clone URL for auto-fetching the `patches` series when no local checkout is
    /// present; default: the repo the lock's patch pin names. The series is
    /// fetched at the lock's `patches.commit` into a durable cache and its pin
    /// enforced. Ignored when `--patches-path` or the sibling `../patches` supplies a
    /// checkout.
    #[arg(long)]
    pub(crate) patches_url: Option<String>,
    /// Vendored rkbin blob directory (default: blobs/SOC under the config root).
    #[arg(long)]
    pub(crate) blobs_dir: Option<PathBuf>,
    /// Debian archive keyring every root this build provisions is verified
    /// against (default: the vendored
    /// blobs/keyrings/debian-archive-keyring.gpg; omit on a Debian host to use
    /// its apt trust store).
    #[arg(long)]
    pub(crate) keyring: Option<PathBuf>,
    /// Trust an overlay-shipped copy of the archive keyring. By default an overlay
    /// that ships blobs/keyrings/debian-archive-keyring.gpg is refused as a
    /// trust-anchor swap; this opts into the overlay's copy explicitly.
    #[arg(long)]
    pub(crate) unsafe_overlay_keyring: bool,
    /// Scratch dir for clones + builds (default: `<root>/build/RECIPE`).
    #[arg(long)]
    pub(crate) work_dir: Option<PathBuf>,
    /// Where produced artifacts are staged (default: WORK_DIR/artifacts). Every
    /// artifact is named for the recipe, so several builds may share one directory.
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
    /// Containers to compress the finished image(s) into, comma-separated and in
    /// preference order — `xz` (default), `gz`, or `none`. Use `gz` for an image
    /// u-boot will write to a disk itself: `gzwrite` reads gzip only, never xz.
    /// `--compress xz,gz` emits both; the first named is what the `next:` hint
    /// points at.
    #[arg(
        long,
        default_value = "xz",
        value_delimiter = ',',
        value_name = "FMT[,FMT]"
    )]
    pub(crate) compress: Vec<ImageCompressionArg>,
    /// Keep the raw `.img` after compressing it (default: delete it once every
    /// requested container is written, since it is derivable and the largest
    /// artifact). Has no effect under `--compress none`, where the raw image is the
    /// only output anyway.
    #[arg(long)]
    pub(crate) keep_raw: bool,
    /// Image layout override (`combined` | `split`); default: the recipe/device
    /// layout. Lock-independent — it changes only image packaging, not any pinned
    /// source, so it is safe to set against an existing lock.
    #[arg(long, value_parser = parse_layout)]
    pub(crate) layout: Option<Layout>,
    /// Image-size override (e.g. `4G`, or `fit+20%` to size the image to its contents
    /// with a fifth of the rootfs left free); default: the recipe/device `image_size`.
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
    /// Also write a software bill of materials beside the image, in this format
    /// (repeatable — `--sbom spdx --sbom cyclonedx` writes both). Off by default, so
    /// a build never silently gains a file; the same documents can be produced later
    /// from the published provenance manifest with `boot2deb sbom`. Set
    /// `SOURCE_DATE_EPOCH` for a byte-reproducible document — everything else in it is
    /// derived from the image's own content.
    #[arg(long = "sbom", value_enum)]
    pub(crate) sbom: Vec<commands::sbom::FormatArg>,
    /// Ignore a rootfs cache hit and re-bootstrap, refreshing the stored tree.
    /// The plan is still resolved — the rootfs cache keys on the *solved* set, so a
    /// moved mirror already rebuilds automatically; this is the manual escape when
    /// you want a clean bootstrap regardless.
    #[arg(long)]
    pub(crate) refresh_rootfs: bool,
    /// Disable the Tier-2 artifact cache: always recompile the kernel /
    /// u-boot / userspace / ffmpeg `.deb`s instead of restoring a stored output on a
    /// signature hit, and do not store this build's outputs. The durable store at
    /// `<root>/cache/artifacts` is left untouched.
    #[arg(long)]
    pub(crate) no_artifact_cache: bool,
    /// Build even though this `boot2deb` binary does not match the source checkout it
    /// is being run from — it was compiled before the checkout's current commit, or
    /// before edits under `crates/`. The image is built by the *running* binary either
    /// way; what the mismatch costs is the truth of the `[built_with]` stamp, which
    /// would name a commit that is not what ran. The fix is normally `cargo build`,
    /// which takes seconds; this is for the case where you mean it.
    #[arg(long)]
    pub(crate) allow_stale_builder: bool,
}

/// `why-rebuild`'s flags: the work dir whose stamps are read, plus the build knobs
/// that change what the prediction should assume.
#[derive(Args)]
pub(crate) struct WhyRebuildArgs {
    /// Build scratch dir to inspect (default: `<root>/build/RECIPE`) — must match the dir the
    /// build used, since the stamps live there.
    #[arg(long)]
    pub(crate) work_dir: Option<PathBuf>,
    /// The build being reasoned about used an explicit `--patches-path` co-dev
    /// checkout (folded into the kernel/u-boot/ffmpeg signatures). Pass the same
    /// value so the prediction matches what that build would reuse.
    #[arg(long)]
    pub(crate) patches_path: Option<PathBuf>,
    /// The build being reasoned about names these optional userspace trees
    /// (`--userspace <name>`). Pass the same set: an optional tree changes what the
    /// whole userspace stage layers, so it moves every userspace node's key.
    #[arg(long = "userspace", value_name = "NAME")]
    pub(crate) userspace: Vec<String>,
    /// The build being reasoned about passes `--no-artifact-cache`. The Tier-2
    /// artifact cache is then off, so no node restores a stored `.deb` and every one
    /// recompiles — pass it here to see that prediction rather than the cached one.
    #[arg(long)]
    pub(crate) no_artifact_cache: bool,
}

/// Which root `shell` enters. One per root a build command can fail in.
///
/// The names are `build --stage`'s, for the stages that have both, because they name
/// the same work: a `--stage kernel` that failed is diagnosed with `shell --stage
/// kernel`. `packaging` is here and not there — it is a root rather than a build node,
/// shared by every stage that archives a `.deb`. The build stages with no root of their
/// own (`dtb` compiles in the kernel's, `rootfs` and `image` assemble trees) have no
/// entry.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ShellStageArg {
    /// The cross root the kernel compiles in.
    Kernel,
    /// The cross root u-boot compiles in.
    Uboot,
    /// The cross root an out-of-tree module compiles in.
    Kmod,
    /// The target-arch root the MPP/RGA/Mali packages compile in.
    Userspace,
    /// The target-arch root ffmpeg compiles in, carrying this build's own userspace
    /// `.deb`s — so the userspace stage has to have produced them first.
    Ffmpeg,
    /// The host-arch packaging root a staged tree becomes a `.deb` in.
    Packaging,
}

impl From<ShellStageArg> for boot2deb_engine::shell::ShellStage {
    fn from(stage: ShellStageArg) -> Self {
        match stage {
            ShellStageArg::Kernel => Self::Kernel,
            ShellStageArg::Uboot => Self::Uboot,
            ShellStageArg::Kmod => Self::Kmod,
            ShellStageArg::Userspace => Self::Userspace,
            ShellStageArg::Ffmpeg => Self::Ffmpeg,
            ShellStageArg::Packaging => Self::Packaging,
        }
    }
}

/// `shell`'s flags: which root to enter, which build point's roots those are, and what
/// to run in it.
///
/// A subset of [`BuildArgs`], and deliberately only the flags that decide *which tree*
/// is entered: the work dir it lives under, the feature selection that picks the lock,
/// the snapshot activation and mirror list its key covers, and the one layer input
/// (`--build-libmali`) that changes what is staged over it. A flag that only changes
/// what a build *produces* has nothing to say to a session.
#[derive(Args)]
pub(crate) struct ShellArgs {
    /// Which root to enter. Required: the whole point is entering a *particular*
    /// stage's root, and no default is more likely right than another.
    #[arg(long, value_enum)]
    pub(crate) stage: ShellStageArg,
    /// Rootfs feature to select, repeatable — the same selection `build --feature`
    /// used, since a variant builds in a work dir of its own. Passing the reference
    /// directly (`shell turing-rk1/forky+jellyfin`) is equivalent.
    #[arg(long = "feature")]
    pub(crate) features: Vec<String>,
    /// Build scratch dir whose roots to enter (default: `<root>/build/RECIPE`) — the
    /// same default `build` uses, so a session lands in the tree a build made.
    #[arg(long)]
    pub(crate) work_dir: Option<PathBuf>,
    /// Directory holding the `.deb`s the compile stages staged (default:
    /// `WORK_DIR/artifacts`). Read only by `--stage ffmpeg`, whose root layers this
    /// build's own userspace packages out of it.
    #[arg(long)]
    pub(crate) out_dir: Option<PathBuf>,
    /// Enter the userspace root as a build naming these optional trees would see it,
    /// carrying the development packages their own probes need — the same set the
    /// userspace stage ran under.
    #[arg(long = "userspace", value_name = "NAME")]
    pub(crate) userspace: Vec<String>,
    /// Snapshot activation, as `build` takes it. Default: the lock's captured mode.
    /// It is in every provisioned root's cache key, so a session opened under a
    /// different mode than the build ran under would enter a different tree.
    #[arg(long, value_parser = parse_snapshot_mode)]
    pub(crate) snapshot: Option<SnapshotMode>,
    /// Debian archive keyring for the bootstrap, if the root has to be provisioned.
    /// Default: the vendored `blobs/keyrings/debian-archive-keyring.gpg`.
    #[arg(long)]
    pub(crate) keyring: Option<PathBuf>,
    /// The command to run in the root, and its arguments. Default: an interactive
    /// `bash`. Everything after `--` is taken verbatim, so a command's own flags reach
    /// it rather than boot2deb.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true)]
    pub(crate) command: Vec<String>,
}

/// `clean`'s flags: which subtree to remove, and the two safety knobs
/// (`--dry-run` preview, `--force` past the ownership stamp).
///
/// The selectors fall in two scopes, and which scope is asked for decides whether the
/// `RECIPE` positional is required. `--cache`/`--sandbox`/`--build-roots` (and the
/// no-selector whole-tree default) name a subtree of *one recipe's* work dir;
/// `--artifacts`/`--verify-trees`/`--kconfig`/`--all-caches` name a store under the
/// config root that every recipe shares, so they sweep without naming one.
#[derive(Args)]
pub(crate) struct CleanArgs {
    /// Build scratch dir to clean (default: `<root>/build/RECIPE`).
    #[arg(long)]
    pub(crate) work_dir: Option<PathBuf>,
    /// Remove only the rootfs early-cutoff cache (WORK_DIR/cache), keeping the
    /// compiled source trees and artifacts.
    #[arg(long)]
    pub(crate) cache: bool,
    /// Remove only the provisioned roots (WORK_DIR/sandbox: the target-arch build
    /// sandbox and the host-arch packaging root) — the largest reclaimable tree.
    #[arg(long)]
    pub(crate) sandbox: bool,
    /// Remove the provisioned *build* roots and the layers staged over them, sparing the
    /// packaging root, so the next build provisions them against the archive as it
    /// stands now — the answer to `the <stage> build root does not satisfy its own
    /// dependencies`, where a cached base has aged past the archive its layer resolved
    /// from. `--sandbox` clears the same skew but takes the packaging root with it,
    /// which is a second bootstrap for a root that is never layered and cannot skew.
    #[arg(long, conflicts_with = "sandbox")]
    pub(crate) build_roots: bool,
    /// Remove the durable Tier-2 artifact store (`<root>/cache/artifacts`).
    /// Root-scoped: this store is shared across recipes, so it clears cached outputs
    /// for *every* recipe, not just one.
    #[arg(long, conflicts_with = "all_caches")]
    pub(crate) artifacts: bool,
    /// Prune the auto-fetched source checkouts (`<root>/cache/verify-trees`, and the
    /// `patches` checkouts beside them) down to what is still pinned: a checkout is
    /// commit-addressed, so one whose commit no `recipes/*/*.lock` names can only be
    /// re-fetched, never reconstructed from, and is dead. Root-scoped. Pinned
    /// checkouts stay — `--all-caches` is what takes those too.
    #[arg(long, conflicts_with = "all_caches")]
    pub(crate) verify_trees: bool,
    /// Remove `verify-config`'s scratch tree (`<root>/cache/kconfig`), one work dir
    /// per recipe holding a provisioned cross root and a kbuild output dir. Pure
    /// scratch: the next `verify-config` re-provisions. Root-scoped.
    #[arg(long, conflicts_with = "all_caches")]
    pub(crate) kconfig: bool,
    /// Remove the whole durable cache tree (`<root>/cache`) — artifacts, every
    /// auto-fetched checkout *including the pinned ones*, the kconfig scratch, and the
    /// pre-built extra-deb store. Root-scoped, and the nuclear option: everything here
    /// is reclaimable by construction, but re-earning it costs a full re-fetch and a
    /// cache-cold rebuild.
    #[arg(long)]
    pub(crate) all_caches: bool,
    /// Show what would be removed (with sizes) without removing anything.
    #[arg(long)]
    pub(crate) dry_run: bool,
    /// Remove the work dir even when it is not stamped as boot2deb-created (no
    /// `.boot2deb-work` marker). Without this, `clean` refuses such a target, so
    /// a mistyped `--work-dir` cannot recursively delete an arbitrary tree.
    #[arg(long)]
    pub(crate) force: bool,
}

/// The per-unit seed keys, shared by `press` (stamping a fresh file) and `seed`
/// (re-stamping an existing one) so the two cannot drift.
#[derive(Args)]
pub(crate) struct SeedKeyArgs {
    /// Per-unit hostname, written into the image's seed partition and applied by
    /// the device at first boot.
    #[arg(long)]
    pub(crate) hostname: Option<String>,
    /// SSH public key (the full `ssh-ed25519 AAAA... comment` line), repeatable —
    /// appended to the default account's `authorized_keys` at first boot.
    #[arg(long = "ssh-key")]
    pub(crate) ssh_keys: Vec<String>,
    /// Wi-Fi network the device joins at first boot (images with NetworkManager
    /// only — every Wi-Fi-capable board's has it). The per-site value that never
    /// belongs in a committed recipe.
    #[arg(long = "wifi-ssid")]
    pub(crate) wifi_ssid: Option<String>,
    /// WPA passphrase for `--wifi-ssid` (8-63 characters, or 64 hex digits).
    /// Omit for an open network. Stored as plain text in the seed partition,
    /// like every seed key.
    #[arg(long = "wifi-psk", requires = "wifi_ssid")]
    pub(crate) wifi_psk: Option<String>,
    /// Static IPv4 (`ADDRESS/PREFIX[,GATEWAY[,DNS...]]`) for the connection the
    /// seed sets up: the Wi-Fi profile when `--wifi-ssid` is present, the wired
    /// interface otherwise — NetworkManager or dhcpcd, whichever the image
    /// carries. Omit for DHCP.
    #[arg(long = "static-ip", value_name = "ADDR/PREFIX[,GW[,DNS...]]")]
    pub(crate) static_ip: Option<String>,
}

impl SeedKeyArgs {
    /// Whether any key was named at all.
    pub(crate) fn is_empty(&self) -> bool {
        self.hostname.is_none()
            && self.ssh_keys.is_empty()
            && self.wifi_ssid.is_none()
            && self.wifi_psk.is_none()
            && self.static_ip.is_none()
    }
}

/// `press`'s flags: the split-build output names, the per-unit seed keys, the
/// tree additions, and the verification knob.
#[derive(Args)]
pub(crate) struct PressArgs {
    /// The boot image's output file, for a `split` build — what goes onto the
    /// eMMC/SPI medium the board boots from.
    #[arg(long = "boot-out")]
    pub(crate) boot_out: Option<PathBuf>,
    /// The rootfs image's output file, for a `split` build — what goes onto the
    /// disk the OS lives on.
    #[arg(long = "rootfs-out")]
    pub(crate) rootfs_out: Option<PathBuf>,
    #[command(flatten)]
    pub(crate) keys: SeedKeyArgs,
    /// Copy a host file into the image at an absolute path (`SRC:DEST`),
    /// repeatable — a site config, a one-off script. Mode 0644 (0755 when the
    /// source is executable), owner root. Re-assembles the image from the kept
    /// rootfs tar, so the build must have run. A source named `*.tmpl` is a
    /// template: its `{{image.<name>}}` references (hostname, PARTUUIDs, suite,
    /// …) are expanded at press time and it lands at DEST.
    #[arg(long = "copy", value_name = "SRC:DEST")]
    pub(crate) copy: Vec<String>,
    /// Copy a whole directory that mirrors the target rootfs, repeatable —
    /// `DIR/etc/site.conf` lands at `/etc/site.conf`. Every regular file and
    /// symlink under it is placed; directories are not, since the parents each
    /// file needs are created root-owned 0755. Same modes as `--copy`, and a
    /// `*.tmpl` file is expanded and lands without the suffix.
    #[arg(long = "copy-tree", value_name = "DIR")]
    pub(crate) copy_tree: Vec<PathBuf>,
    /// Stage a local .deb (repeatable) for installation at first boot via
    /// `dpkg -i`. Dependencies already in the image resolve immediately;
    /// missing ones are fetched only if the board has network by then.
    #[arg(long = "deb", value_name = "PATH")]
    pub(crate) debs: Vec<PathBuf>,
    /// Carry the recipe's own compressed image artifact inside the pressed image
    /// (at /var/lib/boot2deb/install/), so the booted board can install itself to
    /// internal storage with `boot2deb-install-to` — the boot-from-card,
    /// install-to-eMMC workflow.
    #[arg(long)]
    pub(crate) embed_image: bool,
    /// Skip the post-write verification of the pressed file. The press is not
    /// faster; only the re-read is saved.
    #[arg(long)]
    pub(crate) no_verify: bool,
    /// Print what would be pressed — artifacts, outputs, additions, seed keys —
    /// without writing anything.
    #[arg(long)]
    pub(crate) dry_run: bool,
    /// Image layout override (`combined` | `split`), matching the `build` that
    /// produced the artifacts.
    #[arg(long, value_parser = parse_layout)]
    pub(crate) layout: Option<Layout>,
    /// ext4 volume label / GPT partition name for a re-assembled rootfs — match
    /// the `build --rootfs-label` the artifacts were made with.
    #[arg(long, default_value = "rootfs")]
    pub(crate) rootfs_label: String,
    /// Build scratch dir holding the artifacts (default: `<root>/build/RECIPE`).
    #[arg(long)]
    pub(crate) work_dir: Option<PathBuf>,
    /// Directory the build wrote its artifacts to (default: `WORK_DIR/artifacts`).
    #[arg(long)]
    pub(crate) out_dir: Option<PathBuf>,
}

/// `seed`'s flags: the keys to write, or none to reset the seed to the empty
/// template.
#[derive(Args)]
pub(crate) struct SeedArgs {
    #[command(flatten)]
    pub(crate) keys: SeedKeyArgs,
    /// Print what the seed would say without writing anything.
    #[arg(long)]
    pub(crate) dry_run: bool,
}

/// `try`'s flags: how patient one boot may be, and what survives the run.
#[derive(Args)]
pub(crate) struct TryArgs {
    /// Seconds one boot may take to reach a login prompt (and to settle after
    /// it). The default is sized for TCG emulation on a loaded host; with KVM
    /// a boot takes a fraction of it, and the timeout is a ceiling, not a wait.
    #[arg(long, default_value_t = 900)]
    pub(crate) timeout: u64,
    /// Keep the booted disk copy under the work dir after the run, for a
    /// post-mortem or to boot it by hand. Its account password was changed at
    /// first login; the run's report prints the one now set.
    #[arg(long)]
    pub(crate) keep_disk: bool,
    /// Discard the cached fixture kernel and harvest the suite's current one —
    /// how a new point release of the generic kernel is picked up.
    #[arg(long)]
    pub(crate) refresh_fixture: bool,
    /// Build scratch directory (default `build/<recipe>` under the config root)
    /// — where the disk copy and the fixture kernel live.
    #[arg(long)]
    pub(crate) work_dir: Option<PathBuf>,
    /// Where the build's artifacts were written, when not the default
    /// `<work-dir>/artifacts`.
    #[arg(long)]
    pub(crate) out_dir: Option<PathBuf>,
    /// Debian archive keyring for the fixture-kernel root's bootstrap (default:
    /// the vendored `debian-archive-keyring.gpg`).
    #[arg(long)]
    pub(crate) keyring: Option<PathBuf>,
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
    /// Kernel definition id (e.g. rk3588-mainline-7.2). Must support the chosen
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

/// `update`'s flags: the per-tree refs to pin (each following its config layer's
/// declared ref when omitted) plus the blob/patches/manifest inputs.
#[derive(Args)]
pub(crate) struct UpdateArgs {
    /// Rootfs feature to select, repeatable (`--feature jellyfin --feature
    /// media-accel-rockchip`). Replaces the recipe's own feature list and pins the
    /// result as a *variant* of the recipe: the lock, its solved package manifest,
    /// and the build directory are all named `<recipe>+<feature>...`, so the recipe's
    /// own lock is left alone and two selections never collide. Order is significant
    /// — kernel fragments and patch series compose in selection order. A variant
    /// carries no `[support]` claim; the claim belongs to the recipe.
    #[arg(long = "feature")]
    pub(crate) features: Vec<String>,
    /// Kernel ref to pin, resolved to a commit (e.g. v7.2). Optional once a lock
    /// exists: omitting it re-pins the *previous lock's* kernel ref, so a routine
    /// re-pin (e.g. after importing a patch) needs no kernel tag the user did not
    /// touch. Required only for the first update, which has no prior ref to inherit.
    /// Auto-resolving a kernel `track` to its latest tag is a later refinement.
    #[arg(long)]
    pub(crate) kernel_ref: Option<String>,
    /// u-boot ref to pin. Defaults to the boot-method's `uboot_ref`, re-read on every
    /// update, so bumping that one constraint moves every board on the method — except
    /// a lock already pinned to a bare commit sha, which is kept as the deliberate
    /// hand-pin only this flag can have created.
    #[arg(long)]
    pub(crate) uboot_ref: Option<String>,
    /// Media-accel userspace ref to pin, as `NAME=REF`, repeatable. Defaults to that
    /// tree's own `[[userspace]]` ref, re-read on every update; a lock pinned to a bare
    /// commit sha is kept instead. The SoC declares which trees it has, so each override
    /// names one (`--userspace-ref mpp=v1.5.0`).
    #[arg(long = "userspace-ref", value_name = "NAME=REF", value_parser = parse_named_source)]
    pub(crate) userspace_refs: Vec<(String, String)>,
    /// ffmpeg base (V4L2) ref to pin. Defaults to the SoC layer's `ffmpeg.base`,
    /// re-read on every update; a lock pinned to a bare commit sha is kept instead.
    #[arg(long)]
    pub(crate) ffmpeg_base_ref: Option<String>,
    /// ffmpeg Rockchip provenance-tree ref to pin. Defaults to the SoC layer's
    /// `ffmpeg.rockchip`, re-read on every update; a lock pinned to a bare commit sha
    /// is kept instead. Recorded as the graft's provenance; not fetched.
    #[arg(long)]
    pub(crate) ffmpeg_rockchip_ref: Option<String>,
    /// `patches` repo checkout whose HEAD pins the series (default: the config
    /// root's sibling `../patches`). `update` requires this local clone when the
    /// kernel names a patch series — the pin *is* its HEAD — unlike `build`, which
    /// auto-fetches the already-pinned commit and needs no checkout.
    #[arg(long)]
    pub(crate) patches_path: Option<PathBuf>,
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
    /// when the series carries ffmpeg patches, the locked ffmpeg base is
    /// auto-fetched at its pin.
    #[arg(long)]
    pub(crate) ffmpeg_path: Option<PathBuf>,
    /// ffmpeg base clone source (git URL or local path) for the auto-fetch, in place
    /// of the SoC layer's `ffmpeg.base` URL. A local checkout makes the fetch
    /// near-instant. Ignored with `--ffmpeg-path`.
    #[arg(long)]
    pub(crate) ffmpeg_base_src: Option<String>,
    /// u-boot checkout to verify the u-boot series against. Optional: omit it and,
    /// when the recipe pins a u-boot series, the locked u-boot is auto-fetched at
    /// its pin.
    #[arg(long)]
    pub(crate) uboot_path: Option<PathBuf>,
    /// u-boot clone source (git URL or local path) for the auto-fetch, in place of
    /// the boot method's `uboot_source`. Ignored with `--uboot-path`.
    #[arg(long)]
    pub(crate) uboot_src: Option<String>,
    /// Userspace (MPP/RGA) checkout to verify the userspace series against. Optional:
    /// omit it and, when the series carries userspace patches, the locked MPP tree
    /// is auto-fetched at its pin.
    #[arg(long)]
    pub(crate) userspace_path: Option<PathBuf>,
    /// Clone source (git URL or local path) for the auto-fetch of the *patched*
    /// userspace tree, in place of that tree's own `[[userspace]]` URL. A local checkout
    /// makes the fetch near-instant. Ignored with `--userspace-path`.
    #[arg(long)]
    pub(crate) userspace_src: Option<String>,
    /// `patches` repo checkout the series + patches are read from. Omit to use the
    /// config root's sibling `../patches` if present, else auto-fetch the series at
    /// the lock's `patches.commit`.
    #[arg(long)]
    pub(crate) patches_path: Option<PathBuf>,
    /// Clone URL for auto-fetching the `patches` series when no local checkout is
    /// present; default: the repo the lock's patch pin names.
    #[arg(long)]
    pub(crate) patches_url: Option<String>,
    /// Verify against this kernel version instead of the one the lock pins, leaving
    /// the lock untouched — "would this series survive 7.2?" answered before
    /// adopting 7.2. Takes a kernel tag (`v7.2`, `v7.2-rc3`); pair it with
    /// `--kernel-path` or `--kernel-src` pointing at a tree that holds it.
    ///
    /// A version outside the series' declared `applies_to_kernel` is measured, not
    /// refused: that is the case worth asking about, and gating on the envelope would
    /// answer the question by assuming it. The run says so and reports what `git am`
    /// actually does, so a clean result is the evidence for widening the envelope.
    ///
    /// A release candidate is matched against its base release here, so an `-rc`
    /// tree is answerable; the build path stays release-strict.
    ///
    /// Kernel axis only: a recipe that pins no kernel (a `deliverable = "uboot"`
    /// one) rejects it rather than quietly verifying its u-boot series and
    /// reporting a green that answers nothing.
    #[arg(long, value_name = "VERSION")]
    pub(crate) kernel: Option<String>,
    /// Report every patch that fails to apply rather than stopping at the first.
    ///
    /// One boundary usually spawns adjacent ones, so the first failure is rarely the
    /// whole story. Note that each failing patch is skipped, so later results are
    /// measured against a tree missing it — a map of the damage, not a final verdict.
    #[arg(long)]
    pub(crate) keep_going: bool,
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
    /// use the config root's sibling `../patches` if present, else auto-fetch at the
    /// lock's `patches.commit`.
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
    /// canonical `git am`-ready mbox, slot it into a series' scope at a position,
    /// and — with `--verify-tree` — dry-run `git am`-verify the resulting series.
    Import {
        /// Patch source: an `http(s)://` URL (a patchwork mbox), a local file path,
        /// or `-` to read from stdin.
        source: String,
        #[command(flatten)]
        args: PatchImportArgs,
    },
}

/// `patch import`'s flags: where the patch lands (series, scope, position, name)
/// and how it is verified before the series edit is kept.
#[derive(Args)]
pub(crate) struct PatchImportArgs {
    /// Series to slot the patch into (e.g. rk3588-accel) — names
    /// `series/<name>/series.toml` in the patches repo.
    #[arg(long)]
    pub(crate) series: String,
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
    /// `patches` repo checkout to write into (default: the config root's sibling
    /// `../patches`). `patch import` requires this local clone — it writes the patch
    /// file and edits the series there — unlike `build`, which auto-fetches pinned
    /// commits.
    #[arg(long)]
    pub(crate) patches_path: Option<PathBuf>,
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
    /// Kernel definition id (`list-kernels` shows the valid values); default: the
    /// recipe/device `default_kernel`. Must be one of the device's
    /// `supported_kernels`.
    #[arg(long)]
    pub(crate) kernel: Option<String>,
    /// u-boot patch series (e.g. `rk3576-display`); default: the recipe/device
    /// `default_uboot_series`. Must be one of the device's `supported_uboot_series`.
    #[arg(long = "uboot-series")]
    pub(crate) uboot_series: Option<String>,
    /// Debian suite the image is built for (e.g. `forky`, `trixie`); default: the
    /// recipe/device `default_suite`. Re-pinning it for a build is `update`'s job —
    /// here it resolves a different build point.
    #[arg(long)]
    pub(crate) suite: Option<String>,
    /// Image packaging: `combined` (one whole-disk image) or `split` (a
    /// bootloader-only image plus a separate rootfs image, for a two-medium install);
    /// default: the recipe/device `default_layout`.
    #[arg(long, value_parser = parse_layout)]
    pub(crate) layout: Option<Layout>,
    /// How the board boots: `rockchip-rkbin` (u-boot compiled into a raw gap) or
    /// `depthcharge` (a signed ChromeOS kernel partition); default: the device's own.
    /// Must be one of the device's `supported_boot_methods`.
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
    /// Total image size (e.g. `4G`); default: the recipe/device `image_size`. The
    /// rootfs grows to fill its medium on first boot, so this bounds the *artifact*,
    /// not the installed system.
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
    /// NTP server the image prefers, repeatable (`--ntp-server ntp.lan`); default: the
    /// recipe/base `ntp_servers`. When any is given, replaces that list. Debian's
    /// fallback pool is kept either way, so this sets a preference rather than the only
    /// source — worth setting for a board that boots on a network the public pool
    /// cannot be reached from.
    #[arg(long = "ntp-server")]
    pub(crate) ntp_servers: Vec<String>,
    /// Console keyboard layout (e.g. `gb`); default: the recipe/device `keymap`, and
    /// none at all on a headless board. Sets `XKBLAYOUT`; the model, variant, and
    /// options keep their defaults — set those in the device's `[keymap]` table.
    #[arg(long)]
    pub(crate) keymap: Option<String>,
    /// What `sudo` asks of the default account: `nopasswd` (root with no prompt) or
    /// `password` (prompts for the account's own); default: the recipe/base `sudo`.
    #[arg(long, value_parser = parse_sudo_policy)]
    pub(crate) sudo: Option<SudoPolicy>,
    /// Length of the generated per-image first-boot password; default: the recipe/base
    /// `first_boot_password_length`. Shorter is friendlier to transcribe at a console
    /// and weaker in exactly one way — an attack on the password hash inside a shared
    /// image — so authorize an SSH key (`ssh_authorized_keys`) rather than shortening
    /// this if the goal is to stop typing it.
    #[arg(long = "password-length")]
    pub(crate) password_length: Option<u8>,
}

impl From<OverrideArgs> for Overrides {
    fn from(a: OverrideArgs) -> Self {
        Overrides {
            // The deliverable is a recipe property, not a CLI override; a direct device
            // build is always an image.
            deliverable: Default::default(),
            kernel: a.kernel,
            uboot_series: a.uboot_series,
            suite: a.suite,
            layout: a.layout,
            boot_method: a.boot_method,
            board: a.board,
            features: (!a.features.is_empty()).then_some(a.features),
            image_size: a.image_size,
            locale: a.locale,
            locales_generate: (!a.locales_generate.is_empty()).then_some(a.locales_generate),
            timezone: a.timezone,
            ntp_servers: (!a.ntp_servers.is_empty()).then_some(a.ntp_servers),
            keymap: a.keymap.as_deref().map(Keymap::from_layout),
            sudo: a.sudo,
            first_boot_password_length: a.password_length,
            // Authorized keys are config-only. A key is written down so that *every*
            // build of a point carries it, which a per-invocation flag cannot express —
            // and a flag on `resolve` would name a point `build` could not reach.
            ssh_authorized_keys: None,
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
fn parse_sudo_policy(s: &str) -> Result<SudoPolicy, String> {
    s.parse()
}
/// Parse the `--snapshot` activation mode; matches the lock's serialized form.
fn parse_snapshot_mode(s: &str) -> Result<SnapshotMode, String> {
    match s {
        "off" => Ok(SnapshotMode::Off),
        "fallback" => Ok(SnapshotMode::Fallback),
        "pin" => Ok(SnapshotMode::Pin),
        other => Err(format!(
            "unknown snapshot mode '{other}' (expected off|fallback|pin)"
        )),
    }
}
/// Parse the `patch import --scope` value; reuses the model's `FromStr`.
fn parse_scope(s: &str) -> Result<Scope, String> {
    s.parse()
}

/// Parse a `NAME=SRC` clone-source override into its two halves.
///
/// Split at the *first* `=`, because a source may contain one (a URL query, a path)
/// while a `device_kmods` entry name may not — names are bare identifiers. Both halves
/// must be non-empty: `--kmod-src aic8800=` is a truncated command line, not a request
/// to clone from nowhere.
fn parse_named_source(s: &str) -> Result<(String, String), String> {
    match s.split_once('=') {
        Some((name, src)) if !name.is_empty() && !src.is_empty() => {
            Ok((name.to_string(), src.to_string()))
        }
        _ => Err(format!(
            "expected NAME=SRC (e.g. aic8800=../aic8800), got '{s}'"
        )),
    }
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
    fn compress_none_stands_alone_and_anything_else_keeps_its_order() {
        use ImageCompressionArg as A;
        assert_eq!(image_compression(&[A::None]), Ok(Vec::new()));
        // Order is the operator's preference and decides which container the
        // finished-build hint points at, so it is preserved, not normalized.
        assert_eq!(
            image_compression(&[A::Gz, A::Xz]),
            Ok(vec![ImageCompression::Gz, ImageCompression::Xz])
        );
        // A format named twice would just overwrite its own artifact.
        assert_eq!(
            image_compression(&[A::Xz, A::Gz, A::Xz]),
            Ok(vec![ImageCompression::Xz, ImageCompression::Gz])
        );
        // `none` alongside a format is two contradictory readings, so it is an
        // error rather than a guess that could delete a raw image.
        assert!(image_compression(&[A::None, A::Xz]).is_err());
        assert!(image_compression(&[A::Xz, A::None]).is_err());
    }

    #[test]
    fn a_named_source_splits_at_the_first_equals() {
        assert_eq!(
            parse_named_source("aic8800=../aic8800"),
            Ok(("aic8800".into(), "../aic8800".into()))
        );
        // A source may hold `=` (a URL query); a kmod name may not, so the first
        // separator is the right one and the rest belongs to the source.
        assert_eq!(
            parse_named_source("aic8800=https://host/r.git?a=b"),
            Ok(("aic8800".into(), "https://host/r.git?a=b".into()))
        );
        for bad in ["aic8800", "=../src", "aic8800=", ""] {
            assert!(
                parse_named_source(bad).unwrap_err().contains("NAME=SRC"),
                "'{bad}' should be rejected"
            );
        }
    }

    #[test]
    fn the_command_tree_is_well_formed() {
        // clap's own consistency checks (duplicate flags, bad conflicts_with targets,
        // ill-formed defaults) run here rather than surfacing as a runtime panic.
        use clap::CommandFactory;
        Cli::command().debug_assert();
    }
}
