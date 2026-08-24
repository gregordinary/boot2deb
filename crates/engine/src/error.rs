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

    /// The build host is not Linux. Every stage past resolution needs user
    /// namespaces, binfmt, and Linux filesystem semantics, so the answer is known at
    /// preflight rather than minutes into the pipeline.
    #[error(
        "builds require a Linux host; this is {os}. The read-only commands \
         (resolve, list-*, doctor, support-matrix) work here — building does not."
    )]
    HostNotLinux {
        /// `std::env::consts::OS` for the host that cannot build.
        os: String,
    },

    /// A staged build root does not satisfy its own declared dependencies.
    ///
    /// The base is provisioned once and cached; the layer over it is resolved against
    /// the archive as it stands when the build runs. When those two describe different
    /// archive states, a layer package can install whose declared dependency the base
    /// does not meet. Nothing fails at that moment — the failure arrives later as a
    /// link or compile error naming a library that is present and correct, which sends
    /// a reader after the wrong thing entirely.
    ///
    /// Raised as soon as the layer is staged, naming the package, the constraint it
    /// declared, and what is actually installed, so the skew is stated rather than
    /// inferred. Dropping the cached build roots is what clears it: they are provisioned
    /// again against the archive as it stands, and
    /// [`build_root_trees`](crate::sandbox::build_root_trees) is the set that has to go.
    ///
    /// `RECIPE` in the message is a placeholder — the engine knows the stage that staged
    /// the layer, not the build point the caller named it under.
    #[error(
        "the {stage} build root does not satisfy its own dependencies — the cached base \
         and the freshly resolved layer describe different archive states:\n{}\n\
         Drop the cached build roots so the next build provisions them against the \
         current archive: `boot2deb clean RECIPE --build-roots`.",
        .unmet.join("\n")
    )]
    LayerIncoherent {
        /// The stage whose root was staged (e.g. `ffmpeg`).
        stage: String,
        /// One rendered line per unmet dependency, already ordered.
        unmet: Vec<String>,
    },

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
    /// newly resolved axes with stale pins.
    #[error(
        "lock is stale: the recipe resolves differently than it was locked ({}) \
         — re-run `boot2deb update <recipe>` to re-pin",
        .axes.join("; ")
    )]
    LockConfigDrift {
        /// One `"axis: lock X vs resolved Y"` message per drifted axis.
        axes: Vec<String>,
    },

    /// A patch file referenced by a series does not exist on disk.
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

    /// The `patches` checkout's HEAD is not the locked `patches.commit`. The build
    /// reads the series from this checkout, so a drifted tree would silently apply a
    /// *different* series than the lock names. An explicit `--patches-path`
    /// override downgrades this to a warning for patch co-development.
    ///
    /// Only raised when the commits genuinely differ; a checkout sitting *on* the pin
    /// with uncommitted work is [`PatchesWorktreeDirty`](EngineError::PatchesWorktreeDirty),
    /// which is a different problem and must not be reported as a commit mismatch.
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
        /// Commit the checkout is actually at, never equal to `expected`.
        actual: String,
        /// Whether the checkout also had uncommitted changes.
        dirty: bool,
        /// How the checkout's HEAD relates to the pin — selects the remedy text.
        relation: PinRelation,
    },

    /// The `patches` checkout is on the locked commit but its worktree is not clean.
    /// The lock names a *commit*, so uncommitted work is not part of the series the
    /// lock describes — building would apply something unreproducible.
    ///
    /// Separate from [`PatchesPinMismatch`](EngineError::PatchesPinMismatch) because
    /// nothing is mismatched: naming one commit as both "is at" and "pins" reads as a
    /// contradiction and sends the reader looking for drift that is not there.
    #[error(
        "patches checkout {root} has uncommitted changes at the pinned commit \
         {commit}\n  {}",
        dirty_pin_remedy(.root)
    )]
    PatchesWorktreeDirty {
        /// The patches checkout holding uncommitted work.
        root: String,
        /// The commit both the lock and the checkout's HEAD name.
        commit: String,
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
    #[error("patch does not apply to {tree} at {target}:\n  {patch}\n{detail}")]
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

    /// A build sandbox ([`crate::sandbox`]) could not configure or launch a
    /// command. Distinct from [`CommandFailed`](Self::CommandFailed): the
    /// command never ran — a rejected spec, a namespace/mount setup failure, or
    /// an exec error — rather than running to a non-zero exit.
    #[error("sandbox failed ({context}): {source}")]
    Sandbox {
        /// What the engine was trying to run inside the sandbox.
        context: String,
        /// The underlying ferroday-cage error.
        #[source]
        source: ferroday_cage::Error,
    },

    /// A `boot2deb try` run failed: the guest never reached a login prompt,
    /// authentication with the generated password was refused, a unit failed,
    /// the selftest failed, or first-boot re-ran on the second boot. Carries
    /// the phase and what the console showed — the message is the whole
    /// diagnosis, since the guest is gone by the time the error is read.
    #[error("try: {context}: {message}")]
    TryBoot {
        /// What the harness was doing (`first boot`, `log in to the guest`, …).
        context: String,
        /// What went wrong, with the console or QEMU stderr tail where useful.
        message: String,
    },

    /// The rootless Debian bootstrap (ferroday-cage's in-process provisioner)
    /// could not produce the target-arch rootfs — a configuration error, a
    /// download/verification failure, or a failed in-cage dpkg wave. Carries the
    /// provisioner's own message: the provision and configuration error types are
    /// distinct, so it is flattened to text here rather than wrapped.
    #[error("rootfs bootstrap failed ({context}): {message}")]
    Bootstrap {
        /// What the bootstrap was doing (configuring, or fetching/assembling).
        context: String,
        /// The provisioner's error, rendered.
        message: String,
    },

    /// A published plan document could not be read as one — malformed, or written in a
    /// format version this boot2deb does not read. Distinct from
    /// [`Bootstrap`](Self::Bootstrap) because no bootstrap is involved: the readers
    /// that hit this are answering questions *about* a published image, and calling
    /// that a failed bootstrap would name a stage the command never ran.
    ///
    /// The plan format is versioned and a mismatch is refused rather than guessed at, so
    /// the provisioner's own message — which names both versions — is carried verbatim.
    #[error("cannot read the plan document {path}: {message}")]
    PlanDocument {
        /// The document that could not be read.
        path: String,
        /// The provisioner's error, rendered.
        message: String,
    },

    /// A [`SandboxRun`](crate::sandbox::SandboxRun) carried an empty `argv`, so there
    /// is no program to run. The field's contract states it is non-empty and every
    /// in-tree call site honours it; this makes the invariant structural rather than
    /// conventional, since the alternative at the indexing site is a panic — and
    /// [`BuildRoot::run`](crate::sandbox::BuildRoot::run) is public API an out-of-tree
    /// caller can hand a spec to.
    #[error("no command to run in the sandbox ({context}): the argv is empty")]
    EmptyArgv {
        /// What the engine was trying to run inside the sandbox.
        context: String,
    },

    /// [`shell::open`](crate::shell::open) was called with a standard input that is
    /// not a terminal.
    ///
    /// The session is a relay between the caller's terminal and the sandbox's own, so
    /// it has two ends: without a terminal on this side there is nothing to put in raw
    /// mode, no window size to follow, and no way to type into the sandbox. Refused at
    /// the start rather than discovered as a session that echoes nothing.
    #[error(
        "`shell` needs a terminal on standard input, and this one is not a terminal. \
         It relays the caller's terminal to a pseudoterminal inside the sandbox, so \
         there has to be one to relay. To run a command in the same root without a \
         terminal, use the build stages."
    )]
    ShellNeedsTerminal,

    /// An operation on the caller's own terminal failed — reading its window size,
    /// putting it in raw mode, restoring it, or the `signalfd` the session follows
    /// `SIGWINCH` through.
    ///
    /// Distinct from [`Sandbox`](Self::Sandbox), which carries what the sandbox
    /// library refused: this is the *caller's* end of the relay, which the library
    /// never touches.
    #[error("failed to {context}: {source}")]
    Terminal {
        /// What was being done to the caller's terminal, as a verb phrase.
        context: &'static str,
        /// The underlying `errno`.
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
    #[error("blob {filename} not found under {dir} — vendor it there (see blobs/README.md)")]
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

    /// A vendored keyring has no sibling fingerprint manifest. The manifest is what
    /// makes the keyring reviewable, so its absence is a hard error rather than an
    /// unchecked pass — otherwise deleting one file would silently disable the
    /// trust-anchor audit.
    #[error(
        "keyring {keyring} has no fingerprint manifest at {manifest} — every vendored \
         keyring ships one (see blobs/keyrings/README.md)"
    )]
    KeyringManifestMissing {
        /// The keyring that was to be verified.
        keyring: String,
        /// The manifest path that was expected beside it.
        manifest: String,
    },

    /// A fingerprint manifest could not be parsed (a bad fingerprint, a duplicate, or
    /// no entries at all).
    #[error("keyring manifest {manifest} is malformed: {reason}")]
    KeyringManifestMalformed {
        /// The manifest that could not be parsed.
        manifest: String,
        /// What was wrong, with the offending line number.
        reason: String,
    },

    /// A keyring could not be parsed as an OpenPGP packet stream. Fails closed: a
    /// keyring whose contents cannot be fully accounted for is never declared
    /// verified.
    #[error("keyring {keyring} is not a parseable OpenPGP keyring: {reason}")]
    KeyringMalformed {
        /// The keyring that could not be parsed.
        keyring: String,
        /// What the parser choked on.
        reason: String,
    },

    /// A keyring's primary keys are not the ones its manifest vets. An `unexpected`
    /// key is a trust anchor nobody reviewed; a `missing` one means the manifest is
    /// stale (a key rotation, which is a deliberate re-validation event). Either way
    /// the build stops rather than bootstrapping against unvetted keys.
    #[error(
        "keyring {keyring} does not match its vetted fingerprints in {manifest}\
         {}{}\n  \
         refresh both together, or restore the keyring — see blobs/keyrings/README.md",
        crate::error::fmt_keys("\n  unexpected key (not vetted): ", unexpected),
        crate::error::fmt_keys("\n  vetted key not in the keyring: ", missing)
    )]
    KeyringFingerprintMismatch {
        /// The keyring whose contents were rejected.
        keyring: String,
        /// The manifest it was held to.
        manifest: String,
        /// Fingerprints present in the keyring but absent from the manifest.
        unexpected: Vec<String>,
        /// Manifest entries (fingerprint and label) absent from the keyring.
        missing: Vec<String>,
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
    /// upstream `.dts`/`.dtsi` is a patch in the kernel's patch series, which `git
    /// am` applies with conflict detection. Silently clobbering the upstream file
    /// would hide that the fork has drifted, so the copy refuses.
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
    #[error(
        "no 'dtb-$(CONFIG_…) +=' rule found in {makefile} — cannot register '{dtb}' for build"
    )]
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

    /// A compile stage was reached for a build that has no such stage — the kernel
    /// node on a distro-package kernel, the u-boot node on a board whose firmware is
    /// its own. The CLI schedules stages from the resolved build, so a normal run
    /// cannot reach this; it is the engine's own contract check against being handed
    /// a build it should never have been given.
    #[error("the {stage} stage does not apply to this build: {why}")]
    StageNotApplicable {
        /// The stage that was invoked.
        stage: &'static str,
        /// Why this build has no such stage.
        why: &'static str,
    },

    /// The lock omits a pin the stage needs. A lock omits a pin exactly when the
    /// build has no such dependency, so this means the lock and the config disagree —
    /// a lock written before the kernel's flavor or the board's boot method changed.
    #[error(
        "the lock has no [{what}] pin, which the {stage} stage requires — the lock \
         predates a change to this recipe; re-run `boot2deb update <recipe>`"
    )]
    MissingPin {
        /// The absent lock table.
        what: &'static str,
        /// The stage that needs it.
        stage: &'static str,
    },

    /// The signed kernel partition image is not one this image could boot. The
    /// cmdline is baked into its vboot signature, so a wrong value cannot be repaired
    /// after the fact — and on a board with no serial console every variant of "wrong"
    /// looks the same from the outside: it powers up, finds no root, and reboots. So
    /// each is caught here, at build time, with the reason named.
    #[error("the signed kernel partition is not bootable for this image: {detail}")]
    KpartInvalid {
        /// What is wrong with it, and why that would fail to boot.
        detail: String,
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

    /// The named path is not an image artifact this engine reads or writes: an
    /// extension outside the set a build produces, or a compressed stream whose
    /// container does not parse.
    #[error("cannot read {target} as an image: {detail}")]
    ImageFileInvalid {
        /// The path as the caller named it.
        target: String,
        /// Why it is not a readable image.
        detail: String,
    },

    /// The written file handed back fewer bytes than were written — a truncated
    /// copy, which a filesystem that ran out of space mid-write (or a medium that
    /// silently drops writes) produces.
    #[error(
        "verification failed on {target}: wrote {expected_bytes} bytes but could read \
         back only {read_bytes} — the destination did not keep what it acknowledged"
    )]
    ImageVerifyShortRead {
        /// The written file.
        target: String,
        /// Bytes the write put down.
        expected_bytes: u64,
        /// Bytes the re-read produced before EOF.
        read_bytes: u64,
    },

    /// The written bytes read back differently than they were written. The write
    /// itself succeeded, so something between the stream and the re-read changed
    /// the data.
    #[error(
        "verification failed on {target}: the re-read bytes hash to {actual}, the \
         written stream hashed to {expected} — the destination corrupted the image"
    )]
    ImageVerifyDigest {
        /// The written file.
        target: String,
        /// SHA-256 of the written stream.
        expected: String,
        /// SHA-256 of what came back.
        actual: String,
    },

    /// The partition table in the written file does not match the one the source
    /// artifact carries (or could not be read back at all).
    #[error("partition-table verification failed on {target}: {detail}")]
    ImageVerifyGpt {
        /// The written file (or the artifact, for the planned-table half).
        target: String,
        /// What differed or failed to parse.
        detail: String,
    },

    /// A `press` tree addition cannot be placed: an invalid destination path, a
    /// destination the image holds a directory at, an unreadable source file, or
    /// two additions claiming one path. Named per destination so a multi-flag
    /// press fails pointing at the flag that is wrong.
    #[error("cannot add {dest} to the image: {detail}")]
    PressAddition {
        /// The in-image destination path the addition named.
        dest: String,
        /// Why it cannot be placed.
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
    /// `--patches-path` / `../patches`, no `--patches-url`, and the lock's pin
    /// records no repo either. The message carries the exact commit so the user can
    /// fetch the series manually.
    ///
    /// The pin's `source` is written by `update` from the resolved config, so the
    /// durable fix names the axis that lost its URL: `patches_url` lives on the
    /// kernel definition for a kernel series and on `boot-methods/rockchip-rkbin.toml`
    /// for a u-boot one. A `deliverable = "uboot"` recipe has no kernel definition at
    /// all, which is why the message must not name only that one.
    #[error(
        "no patches source: no local checkout, and the lock records no repo for \
         commit {commit}.\n  \
         Provide one of:\n    \
         --patches-path <dir>   (a local checkout of the patches repo)\n    \
         --patches-url <url>    (auto-fetch the series at {commit})\n  \
         or re-record the source with `boot2deb update <recipe>`, which takes it from\n  \
         `patches_url` on the kernel definition (kernel series) or on\n  \
         boot-methods/rockchip-rkbin.toml (u-boot series)."
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
    #[error(
        "unsafe patch label '{label}': must be a repo-relative path with no '..' or leading '/'"
    )]
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

    /// Editing the series manifest during `patch import` failed — the file could
    /// not be parsed as TOML, or the scope key held a non-array value.
    #[error("failed to update series {path}: {detail}")]
    PatchImportSeries {
        /// The series.toml being edited.
        path: String,
        /// What went wrong.
        detail: String,
    },

    /// Building the rootfs ext4 filesystem failed inside the pure-Rust formatter: the
    /// rootfs archive could not be parsed, or the image could not be realized (a
    /// geometry, allocation, or serialization failure). Unlike a host tool's nonzero
    /// exit, this is a typed failure raised from within the build.
    #[error("formatting the rootfs ext4 image failed: {detail}")]
    Ext4Format {
        /// The formatter's own error, rendered.
        detail: String,
    },

    /// Producing the image's first-boot credential failed inside the in-process
    /// hasher. Like [`Ext4Format`](Self::Ext4Format) this is a typed failure raised
    /// from within the build rather than a host tool's nonzero exit — the credential
    /// path shells out to nothing.
    #[error("{context} failed: {message}")]
    Secret {
        /// What was being produced.
        context: String,
        /// The hasher's own error, rendered.
        message: String,
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

/// The remedy for uncommitted work in a patches checkout: the changes are
/// unpinnable until committed, so this leads with "commit" whatever HEAD is doing.
/// Shared by [`EngineError::PatchesWorktreeDirty`] and the dirty case of
/// [`pin_mismatch_remedy`], which must not drift apart.
fn dirty_pin_remedy(root: &str) -> String {
    format!(
        "the checkout has uncommitted changes — commit them in {root}, then re-pin \
         with `boot2deb update <recipe>` so the lock includes them (or pass \
         --patches-path <dir> to build from the working checkout)"
    )
}

/// The remedy line of the `PatchesPinMismatch` message, chosen by how the
/// checkout relates to the pin. A dirty tree always leads with "commit" — its
/// changes are unpinnable until committed, whatever HEAD's relation is.
fn pin_mismatch_remedy(relation: PinRelation, dirty: bool, root: &str, expected: &str) -> String {
    if dirty {
        return dirty_pin_remedy(root);
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

/// Render a list of keys under `heading`, one per line, or nothing when empty — so a
/// [`EngineError::KeyringFingerprintMismatch`] naming only unexpected keys does not
/// also print an empty "missing" heading.
fn fmt_keys(heading: &str, keys: &[String]) -> String {
    if keys.is_empty() {
        return String::new();
    }
    keys.iter()
        .map(|k| format!("{heading}{k}"))
        .collect::<Vec<_>>()
        .join("")
}
