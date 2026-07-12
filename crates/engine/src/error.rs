//! Engine errors — the typed failures of the side-effecting build stages.
//!
//! The engine shells out to `git` and touches the filesystem, so its
//! failures are distinct from the pure config errors in
//! [`boot2deb_core::ConfigError`], which are re-wrapped via [`EngineError::Config`].

use std::path::Path;

/// A failure from an engine stage (git invocation, patch verify, pin resolution).
#[derive(Debug, thiserror::Error)]
pub enum EngineError {
    /// A pure config/resolution error surfaced through an engine stage.
    #[error(transparent)]
    Config(#[from] boot2deb_core::ConfigError),

    /// `git` could not be spawned at all (not installed, not on `PATH`).
    #[error("failed to run git ({context}): {source}")]
    GitSpawn {
        /// What the engine was trying to do.
        context: String,
        /// Underlying spawn error.
        #[source]
        source: std::io::Error,
    },

    /// `git` ran but exited non-zero for something other than a patch conflict.
    #[error("git {context} failed{}: {stderr}", exit_suffix(*.status))]
    GitFailed {
        /// What the engine was trying to do (e.g. `ls-remote linux-stable`).
        context: String,
        /// Process exit code, if the process was not signalled.
        status: Option<i32>,
        /// Captured stderr (trimmed).
        stderr: String,
    },

    /// A clone source or git ref looks like a command-line option (starts with
    /// `-`), so passing it to `git` as a positional could be interpreted as a flag
    /// — e.g. a `source = "--upload-pack=<cmd>"` reaching `git fetch` is command
    /// execution. Refused before any git runs (positionals are additionally guarded
    /// with `--end-of-options`).
    #[error("unsafe git {what} '{value}': must not start with '-'")]
    UnsafeGitArgument {
        /// Which argument (e.g. `"source"`, `"ref"`).
        what: &'static str,
        /// The offending value.
        value: String,
    },

    /// A make target derived from config (`base_defconfig` / `uboot_defconfig`)
    /// looks like a GNU make option (`-…`) or a variable assignment
    /// (`FOO=bar`) — either would change what `make` does instead of naming a
    /// target, and an assignment like `CC=<cmd>` is arbitrary-tool injection. A
    /// legitimate defconfig is a bare identifier, so both shapes are refused before
    /// `make` runs; the target positional is additionally guarded with `--`.
    #[error("unsafe make target {what} '{value}': must name a target, not start with '-' or contain '='")]
    UnsafeMakeTarget {
        /// Which argument (e.g. `"base_defconfig"`, `"uboot_defconfig"`).
        what: &'static str,
        /// The offending value.
        value: String,
    },

    /// A ref (tag/branch) did not resolve to a commit on the remote.
    #[error("ref '{reference}' not found at {url}")]
    RefNotFound {
        /// Remote URL queried.
        url: String,
        /// The ref that was not found.
        reference: String,
    },

    /// A named kernel source has no known upstream URL mapping.
    #[error("unknown kernel source tree '{name}' (no URL mapping)")]
    UnknownSourceTree {
        /// The unmapped tree name.
        name: String,
    },

    /// The committed lock disagrees with a fresh resolution on one or more axes the
    /// lock records from the resolved build — the config drifted since `update`,
    /// so the pins no longer describe the requested point. Each listed axis names its
    /// mismatch; the fix is to re-run `update`. Refused up front so a build never mixes
    /// newly resolved axes with stale pins (CFG-2).
    #[error(
        "lock is stale: the recipe resolves differently than it was locked ({}) \
         — re-run `boot2deb update <recipe>` to re-pin",
        .axes.join("; ")
    )]
    LockConfigDrift {
        /// One `"axis: lock X vs resolved Y"` message per drifted axis.
        axes: Vec<String>,
    },

    /// A patch file referenced by a profile does not exist on disk.
    #[error("patch not found: {path}")]
    PatchNotFound {
        /// Path that was expected to hold the patch.
        path: String,
    },

    /// The checkout to verify against has uncommitted changes. Verify snapshots
    /// and hard-resets the worktree, so it refuses to run on a dirty tree rather
    /// than risk discarding work.
    #[error("checkout has uncommitted changes: {repo} (verify needs a clean tree)")]
    DirtyCheckout {
        /// The checkout that was not clean.
        repo: String,
    },

    /// The `patches` checkout does not match the lock's pin — its HEAD is not the
    /// locked `patches.commit`, or it has uncommitted changes. The build reads the
    /// series from this checkout, so a drifted tree would silently apply a
    /// *different* series than the lock names. An explicit `--patches-path`
    /// override downgrades this to a warning for patch co-development.
    ///
    /// The remedy depends on *which side* moved: a checkout ahead of the pin (or
    /// dirty) holds work the lock should include — the fix is to commit and re-run
    /// `boot2deb update`, not to discard the work by re-checking-out the pin. Only
    /// a stale checkout is fixed by checking out the locked commit. The `relation`
    /// field ([`PinRelation`]) carries that distinction into the message.
    #[error(
        "patches checkout {root} is at {actual}{}, but the lock pins {expected}\n  {}",
        if *.dirty { " (with uncommitted changes)" } else { "" },
        pin_mismatch_remedy(*.relation, *.dirty, .root, .expected)
    )]
    PatchesPinMismatch {
        /// The patches checkout that drifted.
        root: String,
        /// Commit the lock pins the series at.
        expected: String,
        /// Commit the checkout is actually at.
        actual: String,
        /// Whether the checkout also had uncommitted changes.
        dirty: bool,
        /// How the checkout's HEAD relates to the pin — selects the remedy text.
        relation: PinRelation,
    },

    /// The `patches` checkout `update` would pin has uncommitted changes. The pin
    /// is `HEAD`, so those changes — typically a just-imported patch — would be
    /// silently absent from the lock and resurface later as a build-time
    /// [`PatchesPinMismatch`](EngineError::PatchesPinMismatch). Refused before any
    /// upstream ref is consulted: commit first, then re-run.
    #[error(
        "uncommitted changes in patches checkout {root} — commit them so the pin \
         includes your new patch, then re-run `boot2deb update <recipe>`"
    )]
    PatchesDirty {
        /// The dirty patches checkout.
        root: String,
    },

    /// `update` found no `patches` checkout at the given path. The pin is the
    /// checkout's `HEAD`, so `update` needs a local clone — unlike `build`,
    /// which reads the already-pinned commit and auto-fetches it with no
    /// checkout present.
    #[error(
        "no patches checkout at {path}: `update` pins the checkout's HEAD, so it \
         needs a local clone there (clone the patches repo, or point --patches-path \
         at one); `build` needs no checkout — it auto-fetches the pinned commit"
    )]
    PatchesCheckoutMissing {
        /// The path expected to hold the checkout.
        path: String,
    },

    /// A patch in the series did not apply to the target tree — the verify gate's
    /// hard error, naming the failing patch and the kernel it was checked against.
    /// Patches are never silently skipped or fuzzed in.
    #[error(
        "patch does not apply to {tree} at {target}:\n  {patch}\n{detail}"
    )]
    PatchDoesNotApply {
        /// Which source tree the series targets (e.g. `kernel`).
        tree: String,
        /// The target the tree was checked at (e.g. `rk3588-mainline-7.1 @ v7.1.1`).
        target: String,
        /// The patch that failed (patches-repo-relative path).
        patch: String,
        /// Trimmed `git am` output explaining the conflict.
        detail: String,
    },

    /// A streamed build subprocess — `make`, `merge_config.sh`, or a `git`
    /// clone/fetch run through [`build::run`](crate::build::run) — could not be
    /// spawned (not installed / not on `PATH`).
    #[error("failed to run {command} ({context}): {source}")]
    CommandSpawn {
        /// The program that failed to start (e.g. `make`).
        command: String,
        /// What the engine was trying to do.
        context: String,
        /// Underlying spawn error.
        #[source]
        source: std::io::Error,
    },

    /// A build subprocess ran but exited non-zero.
    #[error("{command} failed{} ({context}): {stderr}", exit_suffix(*.status))]
    CommandFailed {
        /// The program that failed (e.g. `make`).
        command: String,
        /// What the engine was trying to do (e.g. `make defconfig`).
        context: String,
        /// Process exit code, if the process was not signalled.
        status: Option<i32>,
        /// Captured stderr (trimmed).
        stderr: String,
    },

    /// A blob named by the resolved build (`rkbin.atf` / `rkbin.tpl`) does not
    /// exist in the blob directory, so there is nothing to hash into a lock pin.
    /// Blobs are vendored files, never fetched, so the remedy is to vendor the
    /// file — named here rather than surfacing as a bare I/O error.
    #[error(
        "blob {filename} not found under {dir} — vendor it there (see blobs/README.md)"
    )]
    BlobMissing {
        /// The blob filename the resolved build names.
        filename: String,
        /// The blob directory that was searched (`blobs/<soc>/` by default).
        dir: String,
    },

    /// A vendored blob's sha256 did not match the lock pin — the u-boot build
    /// refuses to consume it.
    #[error("blob {filename} hash mismatch: lock has {expected}, found {actual}")]
    BlobMismatch {
        /// Blob filename from the pin.
        filename: String,
        /// Hash recorded in the lock.
        expected: String,
        /// Hash of the vendored file.
        actual: String,
    },

    /// A lock blob pin was not in `"<filename>@sha256:<hex>"` form.
    #[error("malformed blob pin: {pin}")]
    BlobPinMalformed {
        /// The pin that could not be parsed.
        pin: String,
    },

    /// A checkout resolved to a different commit than the lock pins — the build
    /// reads only the lock, so a source that does not match it is a hard error
    /// rather than a silently different artifact.
    #[error("{what} checkout is at {actual}, but the lock pins {expected}")]
    CommitMismatch {
        /// What was being checked out (e.g. `kernel`, `u-boot`).
        what: String,
        /// Commit the lock pins.
        expected: String,
        /// Commit the checkout is actually at.
        actual: String,
    },

    /// A pinned commit could not be obtained from the source: it is neither
    /// shallow-fetchable by SHA nor reachable from any branch or tag after a
    /// full-history fetch. This happens when the upstream branch it was on has been
    /// rebased, force-pushed, or deleted, so only a local checkout (or a durable
    /// mirror) still holds it — the fetch mechanism cannot conjure a commit the
    /// remote no longer advertises.
    #[error(
        "{what} commit {commit} is not reachable from {url} \
         (the upstream branch may have been rebased, force-pushed, or deleted); \
         supply a local checkout with --{what}-src or mirror the commit to a durable remote"
    )]
    CommitUnreachable {
        /// What was being fetched (e.g. `mpp`, `librga`, `ffmpeg base`).
        what: String,
        /// The source URL the commit was sought from.
        url: String,
        /// The commit the lock pins but the remote does not hold.
        commit: String,
    },

    /// A media-accel build stage (userspace or ffmpeg) was invoked for a build
    /// whose lock carries no media-accel source pins. These stages run only when
    /// the resolved build selects a `requires_media_accel` feature (which pins the
    /// sources), so reaching one without pins is an internal scheduling bug, not a
    /// user misconfiguration — the CLI gates the stages on the pins' presence.
    #[error("internal: {stage} stage scheduled but the lock has no media-accel source pins")]
    MissingMediaAccelPins {
        /// The stage that was reached without pins (`userspace` or `ffmpeg`).
        stage: &'static str,
    },

    /// A `device_dts` source would overwrite a device-tree file the kernel already
    /// ships. `device_dts` owns the *new* board file; an edit to an *existing*
    /// upstream `.dts`/`.dtsi` is a patch in the kernel's patch profile, which `git
    /// am` applies with conflict detection. Silently clobbering the upstream file
    /// would hide that the fork has drifted, so the copy refuses. §4.
    #[error(
        "device_dts source '{src}' already exists in the kernel tree at {dest} — \
         device_dts adds a new board device tree; edit an existing one with a patch instead"
    )]
    DeviceDtsShadowsUpstream {
        /// The config-root-relative source that collided.
        src: String,
        /// The in-tree path it would have overwritten.
        dest: String,
    },

    /// The in-tree device-tree directory's `Makefile` has no `dtb-$(CONFIG_…) += …`
    /// rule to model the board's DTB entry on, so the engine cannot teach kbuild to
    /// build the copied `.dts` — a `.dts` compiled by nothing yields no DTB.
    #[error("no 'dtb-$(CONFIG_…) +=' rule found in {makefile} — cannot register '{dtb}' for build")]
    DeviceDtsNoMakefileRule {
        /// The device-tree directory Makefile that was inspected.
        makefile: String,
        /// The DTB that needed registering.
        dtb: String,
    },

    /// A build stage completed but an expected output artifact was not produced.
    #[error("{what} not found after build (looked in {location})")]
    ArtifactMissing {
        /// The artifact that was expected (e.g. `linux-image .deb`).
        what: String,
        /// Where it was looked for.
        location: String,
    },

    /// The solved rootfs manifest could not be fully content-pinned: some
    /// installed packages had no captured `.deb` to hash, so their sha256 is
    /// unknown. Surfaced rather than shipping a partially pinned manifest,
    /// naming a bounded sample of the offenders.
    #[error(
        "solved manifest incomplete: {count} installed package(s) had no captured .deb sha256 ({sample})"
    )]
    ManifestIncomplete {
        /// How many installed packages lacked a captured `.deb` hash.
        count: usize,
        /// A bounded, comma-joined sample of the missing `name version arch`.
        sample: String,
    },

    /// A freshly-solved rootfs manifest did not reproduce the committed pin
    /// (`RootfsPin.manifest_sha256`) — the live mirror moved off the pinned package
    /// set, so the build is not reproducing the locked rootfs. A hard error
    /// by default; `--save-manifest` accepts the new solve as the pin, or
    /// `--snapshot pin` builds against the captured snapshot that reproduces it.
    #[error(
        "solved rootfs manifest drifted from the committed pin:\n  \
         committed sha256 {expected}\n  solved    sha256 {actual}\n  \
         the live mirror moved off the pinned package set — re-run with --save-manifest \
         to accept the new solve, or --snapshot pin to build against the captured snapshot"
    )]
    ManifestDrift {
        /// The sha256 the lock pins (`RootfsPin.manifest_sha256`).
        expected: String,
        /// The sha256 of the freshly-solved manifest.
        actual: String,
    },

    /// A snapshot mode (`fallback`/`pin`) was requested — via `--snapshot` or the
    /// lock's captured mode — but the lock has no captured snapshot timestamp to
    /// use. There is nothing to fetch from, so the request cannot be honored
    /// silently; capture one first with `--save-snapshot`.
    #[error(
        "snapshot mode '{mode}' requested but the lock has no captured snapshot \
         timestamp — run a build with --save-snapshot first"
    )]
    SnapshotUnavailable {
        /// The requested mode's name (`fallback` / `pin`).
        mode: &'static str,
    },

    /// The resolved raw-gap offsets or image size are inconsistent — a bad
    /// ordering (idbloader < u-boot.itb < rootfs), a non-sector-aligned offset,
    /// an image too small to hold the GPT plus a rootfs partition, or a
    /// bootloader payload that would overrun the next region. Checked
    /// before any bytes are written, so a misconfigured layout fails cleanly.
    #[error("image geometry is invalid: {detail}")]
    ImageGeometry {
        /// What is wrong with the geometry.
        detail: String,
    },

    /// GPT partition-table assembly (`gpt` crate) failed.
    #[error("GPT assembly failed ({context}): {detail}")]
    Gpt {
        /// What the engine was doing (e.g. `add rootfs partition`).
        context: String,
        /// The crate's error rendered to text.
        detail: String,
    },

    /// A pre-built `extra_debs` deb could not be obtained from its
    /// locator — an HTTP fetch failed or an on-disk `path` was unreadable/missing.
    /// The build reads only the lock's pins, so an unfetchable pinned deb is a hard
    /// error, never a silently dropped package.
    #[error("failed to obtain extra_deb from {locator}: {detail}")]
    ExtraDebFetch {
        /// The locator that could not be obtained (URL or path).
        locator: String,
        /// What went wrong (HTTP status / transport / I/O detail).
        detail: String,
    },

    /// A fetched/read `extra_debs` deb's bytes did not hash to the pinned sha256
    /// — the URL served different bytes than were pinned, or the local
    /// file changed. The sha256 is the content identity, so a mismatch is a
    /// verification failure, never a silent swap.
    #[error("extra_deb {locator} hash mismatch: lock pins {expected}, got {actual}")]
    ExtraDebHashMismatch {
        /// The locator whose bytes mismatched.
        locator: String,
        /// The sha256 the lock pins.
        expected: String,
        /// The sha256 of the obtained bytes.
        actual: String,
    },

    /// The `patches` repo could not be auto-fetched at the lock-pinned commit
    /// — a clone/checkout via `gix` failed (offline, a bad URL, or the pinned
    /// commit not reachable from the fetched history). Patches are never silently
    /// skipped, so an unfetchable series is a hard error; the message names the
    /// fetch URL and pinned commit so the user can retry or point `--patches-path`
    /// at a local checkout.
    #[error("failed to fetch patches from {url} at {commit}: {detail}")]
    PatchesFetch {
        /// The clone URL that was attempted.
        url: String,
        /// The lock-pinned `patches.commit` being materialized.
        commit: String,
        /// What went wrong (gix transport / object / checkout detail).
        detail: String,
    },

    /// No `patches` source could be resolved: no local checkout at
    /// `--patches-path` / `../patches`, and no `patches_url` to auto-fetch from
    /// (the kernel omits it and `--patches-url` was not given). The message carries
    /// the exact commit so the user can fetch the series manually.
    #[error(
        "no patches source: no local checkout and no patches_url for commit {commit}.\n  \
         Provide one of:\n    \
         --patches-path <dir>   (a local checkout of the patches repo)\n    \
         --patches-url <url>    (auto-fetch the series at {commit})\n  \
         or set `patches_url` on the kernel definition."
    )]
    PatchesNoSource {
        /// The lock-pinned `patches.commit` the series would be fetched at.
        commit: String,
    },

    /// A patch handed to `patch import` could not be obtained from its source
    /// — an HTTP fetch failed or a local file was unreadable/missing.
    #[error("failed to read patch from {source_ref}: {detail}")]
    PatchImportFetch {
        /// The source that could not be read (URL or path).
        source_ref: String,
        /// What went wrong (HTTP status / transport / I/O detail).
        detail: String,
    },

    /// A destination label handed to `patch import` (via `--as`) escapes the
    /// patches repo — it is absolute or contains a `..` component. The repo-relative
    /// label must stay inside the repo.
    #[error("unsafe patch label '{label}': must be a repo-relative path with no '..' or leading '/'")]
    PatchImportUnsafeLabel {
        /// The offending label.
        label: String,
    },

    /// The destination file for `patch import` already exists — refusing to clobber
    /// an existing patch. Pick a different position/name or remove it first.
    #[error("patch destination {path} already exists (refusing to overwrite)")]
    PatchImportExists {
        /// The destination path that already exists.
        path: String,
    },

    /// Editing the profile manifest during `patch import` failed — the file could
    /// not be parsed as TOML, or the scope key held a non-array value.
    #[error("failed to update profile {path}: {detail}")]
    PatchImportProfile {
        /// The profile.toml being edited.
        path: String,
        /// What went wrong.
        detail: String,
    },

    /// A filesystem operation failed.
    #[error("failed to access {path}: {source}")]
    Io {
        /// Path that failed.
        path: String,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },
}

/// How a drifted `patches` checkout's HEAD relates to the locked pin. Selects
/// the [`PatchesPinMismatch`](EngineError::PatchesPinMismatch) remedy: "your
/// checkout has newer work" and "your checkout is stale" have opposite fixes,
/// and pointing an ahead-of-pin user at a re-checkout would tell them to discard
/// their work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinRelation {
    /// The pin is an ancestor of HEAD: the checkout holds commits the lock has
    /// not pinned yet (e.g. a committed `patch import`). Remedy: re-pin with
    /// `boot2deb update`.
    Ahead,
    /// HEAD is an ancestor of the pin: the checkout is stale — behind the series
    /// the lock names. Remedy: check out the locked commit.
    Behind,
    /// The histories diverged, or the relationship could not be determined (one
    /// of the commits is absent from the local checkout). Remedy: both options,
    /// spelled out.
    Unknown,
}

impl EngineError {
    /// Build an [`Io`](EngineError::Io) error for `path`.
    pub(crate) fn io(path: &Path, source: std::io::Error) -> Self {
        EngineError::Io {
            path: path.display().to_string(),
            source,
        }
    }
}

/// Render `" (exit N)"` for the `GitFailed` message, or `""` when signalled.
fn exit_suffix(status: Option<i32>) -> String {
    match status {
        Some(code) => format!(" (exit {code})"),
        None => String::new(),
    }
}

/// The remedy line of the `PatchesPinMismatch` message, chosen by how the
/// checkout relates to the pin. A dirty tree always leads with "commit" — its
/// changes are unpinnable until committed, whatever HEAD's relation is.
fn pin_mismatch_remedy(relation: PinRelation, dirty: bool, root: &str, expected: &str) -> String {
    if dirty {
        return format!(
            "the checkout has uncommitted changes — commit them in {root}, then re-pin \
             with `boot2deb update <recipe>` so the lock includes them (or pass \
             --patches-path <dir> to build from the working checkout)"
        );
    }
    match relation {
        PinRelation::Ahead => "the checkout is ahead of the pin — re-pin with \
             `boot2deb update <recipe>` to lock the new commits (or pass \
             --patches-path <dir> to build from the working checkout)"
            .to_string(),
        PinRelation::Behind => format!(
            "the checkout is behind the pin — check out the locked commit \
             (`git -C {root} checkout {expected}`) to build the locked series"
        ),
        PinRelation::Unknown => format!(
            "if the checkout's series is the one you want, re-pin with \
             `boot2deb update <recipe>`; to build the locked series instead, \
             re-checkout the patches repo at {expected}"
        ),
    }
}
