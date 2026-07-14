//! Typed configuration errors.
//!
//! Every failure of loading or resolving config is one of these variants, so the
//! whole "is this build well-formed?" question is answered — with an actionable
//! message — *before* any build work starts.

/// An error from loading a config layer or resolving a build.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// A referenced config file does not exist. `kind` is the layer kind
    /// (`"device"`, `"kernel"`, …) for a readable message.
    #[error("{kind} '{name}' not found (looked at {path})")]
    NotFound {
        /// Layer kind, e.g. `"device"`.
        kind: &'static str,
        /// The name that was looked up.
        name: String,
        /// The path that was tried.
        path: String,
    },

    /// A layer/recipe name (from a CLI argument or a config cross-reference) is
    /// not a bare identifier, so it cannot be trusted to join into a filesystem
    /// path. Names must match `[A-Za-z0-9._-]`, be non-empty, not start with a dot,
    /// and contain no path separators or `..` — this stops a `../` traversal or an
    /// absolute path from escaping the config root (both a read *and*, via
    /// `lock_path`, a write target).
    #[error("invalid {kind} name '{name}': must be a bare identifier ([A-Za-z0-9._-], no separators or '..')")]
    InvalidName {
        /// Layer kind, e.g. `"device"`.
        kind: &'static str,
        /// The offending name.
        name: String,
    },

    /// An overlay ships a copy of a *trust anchor* asset (the Debian archive
    /// keyring) that the shipped root also provides. Overlays are operator-supplied
    /// but not necessarily audited line-by-line, and honoring an overlay's archive
    /// keyring silently changes which `Release` signatures apt accepts — a
    /// trust-anchor swap (TRUST-1). Resolution fails closed rather than pick the
    /// overlay's copy; `--unsafe-overlay-keyring` opts into the overlay explicitly.
    #[error(
        "overlay trust-anchor conflict: an overlay ships '{asset}', which shadows the \
         shipped archive keyring — refusing to trust an unaudited keyring. Pass \
         --unsafe-overlay-keyring to use the overlay's copy, or remove it from the overlay."
    )]
    OverlayTrustAnchor {
        /// The repo-relative asset path an overlay tried to shadow.
        asset: String,
    },

    /// A config file exists but could not be read (permissions, etc.).
    #[error("failed to read {path}: {source}")]
    Io {
        /// The file that failed to read.
        path: String,
        /// Underlying I/O error.
        #[source]
        source: std::io::Error,
    },

    /// A config file was read but is not valid TOML for its type (bad syntax,
    /// unknown field, wrong value type).
    #[error("failed to parse {path}: {source}")]
    Parse {
        /// The file that failed to parse.
        path: String,
        /// Underlying deserialization error.
        #[source]
        source: toml::de::Error,
    },

    /// A kernel definition has no `flavor`. The flavor selects which *shape* the
    /// definition has — a compiled kernel's source ref and fragments, or a distro
    /// kernel's package name — so without it there is no struct to validate the file
    /// against.
    #[error("kernel '{kernel}' has no `flavor` (expected mainline, vendor, or distro-package) in {path}")]
    MissingKernelFlavor {
        /// The kernel definition id.
        kernel: String,
        /// The file that lacks the key.
        path: String,
    },

    /// A generated artifact (e.g. a lockfile) could not be serialized to TOML.
    #[error("failed to serialize {what}: {source}")]
    Serialize {
        /// What was being serialized.
        what: &'static str,
        /// Underlying serialization error.
        #[source]
        source: toml::ser::Error,
    },

    /// The chosen kernel is not in the device's `supported_kernels`.
    #[error("device '{device}' does not support kernel '{kernel}' (supported: {supported})")]
    UnknownKernelForDevice {
        /// The device being resolved.
        device: String,
        /// The requested kernel id.
        kernel: String,
        /// Comma-separated list of what the device does support.
        supported: String,
    },

    /// The chosen kernel does not list the device's SoC in `supported_socs`.
    #[error("kernel '{kernel}' does not support soc '{soc}' (supported: {supported})")]
    SocMismatch {
        /// The kernel id.
        kernel: String,
        /// The device's SoC.
        soc: String,
        /// Comma-separated SoCs the kernel supports.
        supported: String,
    },

    /// The chosen boot method is not in the device's `supported_boot_methods`.
    #[error("device '{device}' does not support boot method '{boot_method}' (supported: {supported})")]
    UnsupportedBootMethod {
        /// The device being resolved.
        device: String,
        /// The requested boot method.
        boot_method: String,
        /// Comma-separated boot methods the device supports.
        supported: String,
    },

    /// A required blob field (e.g. `rkbin.atf`) is empty.
    #[error("device '{device}' is missing a required blob: {what}")]
    MissingBlob {
        /// The device being resolved.
        device: String,
        /// Which blob field is missing.
        what: String,
    },

    /// The device omits a field the *resolved boot method* requires. The
    /// requirement is method-scoped, not universal — a board that boots depthcharge
    /// has no `uboot_defconfig` because it compiles no u-boot, and one that boots
    /// rkbin has no `[depthcharge]` block — so the error names the method that wants
    /// it rather than implying every device must carry it.
    #[error("device '{device}' boots via '{boot_method}', which requires `{what}` — add it to devices/{device}.toml")]
    MissingBootField {
        /// The device being resolved.
        device: String,
        /// The boot method that requires the field.
        boot_method: &'static str,
        /// The missing field, as authored in the device layer.
        what: &'static str,
    },

    /// The requested depthcharge board profile is not in the device's
    /// `supported_boards`. A profile describes the *firmware* the unit runs (a stock
    /// C201 and a libreboot'd one differ), so picking the wrong one produces an image
    /// that firmware will not boot — caught here rather than on the hardware.
    #[error("device '{device}' does not support board profile '{board}' (supported: {supported})")]
    UnknownBoardProfile {
        /// The device being resolved.
        device: String,
        /// The requested profile.
        board: String,
        /// Comma-separated profiles the device does support.
        supported: String,
    },

    /// The requested image layout has no meaning under the resolved boot method.
    #[error("boot method '{boot_method}' does not support the '{layout}' layout: {why}")]
    UnsupportedLayout {
        /// The resolved boot method.
        boot_method: &'static str,
        /// The requested layout.
        layout: String,
        /// Why the combination cannot be built.
        why: &'static str,
    },

    /// A ChromeOS kernel-partition attribute does not fit its field. `priority` and
    /// `tries` are 4 bits each, so a value above 15 cannot be written — see
    /// [`kpart_flags`](crate::chromeos::kpart_flags).
    #[error("{field} = {value} does not fit its 4-bit GPT attribute field (0-15)")]
    InvalidKpartAttr {
        /// The offending field (`kpart_priority` or `kpart_tries`).
        field: &'static str,
        /// The authored value.
        value: u8,
    },

    /// A boot method's kernel command line carries something the signing tool cannot
    /// or will not honour, so the value would not survive into the booted kernel.
    #[error("invalid kernel cmdline {value:?}: {why}")]
    InvalidCmdline {
        /// The offending cmdline.
        value: String,
        /// Why it cannot be used.
        why: &'static str,
    },

    /// The device declares an input that only a *compiled* kernel consumes — a board
    /// device tree, or board kconfig fragments — while the resolved kernel is a
    /// distro package that compiles nothing. Nothing would ever build the DTB or merge
    /// the fragments, so the board would read as configured and boot as broken.
    #[error(
        "device '{device}' declares `{what}`, but kernel '{kernel}' is a distro-package \
         kernel that compiles nothing — the value would never be used"
    )]
    DistroKernelCompilesNothing {
        /// The device being resolved.
        device: String,
        /// The distro-package kernel it was paired with.
        kernel: String,
        /// The compile-only device field that would be ignored.
        what: &'static str,
    },

    /// An `--overlay` argument does not name an existing directory. An empty path
    /// would resolve assets against the current directory and a mistyped one would
    /// shadow nothing, so both fail before any layer is read.
    #[error("invalid overlay '{path}': {why}")]
    InvalidOverlay {
        /// The offending overlay path.
        path: String,
        /// What is wrong with it.
        why: &'static str,
    },

    /// A `device_dts` entry is not a contained, relative device-tree source path.
    /// The entries are joined onto every config-root search path, so an absolute
    /// path or a `..` component would read — and later copy into the kernel tree —
    /// a file from outside the config tree.
    #[error(
        "device '{device}' has an invalid device_dts entry '{path}': {why} \
         (expected a config-root-relative path to a .dts or .dtsi)"
    )]
    InvalidDeviceDts {
        /// The device being resolved.
        device: String,
        /// The offending entry.
        path: String,
        /// What is wrong with it.
        why: &'static str,
    },

    /// A board lists `device_dts` sources but none of them compiles the DTB named
    /// by `kernel_dtb` — the boot would look for a DTB the kernel never builds. The
    /// basenames must correspond (`rockchip/board.dtb` ← `.../board.dts`).
    #[error(
        "device '{device}': kernel_dtb '{kernel_dtb}' is not built by any device_dts \
         source ({sources}) — expected a '{expected}' among them"
    )]
    KernelDtbNotInDeviceDts {
        /// The device being resolved.
        device: String,
        /// The DTB the board is configured to boot.
        kernel_dtb: String,
        /// Comma-separated `device_dts` entries.
        sources: String,
        /// The `.dts` basename that would satisfy the check.
        expected: String,
    },

    /// A patch profile's `applies_to_kernel` is not a valid semver requirement.
    #[error("profile '{profile}' has invalid applies_to_kernel '{value}': {source}")]
    InvalidVersionReq {
        /// The profile whose range failed to parse.
        profile: String,
        /// The offending `applies_to_kernel` string.
        value: String,
        /// Underlying semver parse error.
        #[source]
        source: semver::Error,
    },

    /// A size / offset string could not be parsed to bytes (bad number, missing
    /// or unknown unit, or overflow) — see [`parse_size`](crate::size::parse_size).
    #[error("invalid size '{value}' (expected e.g. '512', '32KiB', '8MiB', '2G')")]
    InvalidSize {
        /// The offending size string.
        value: String,
    },

    /// A kernel version string could not be parsed as a semver version.
    #[error("kernel version '{value}' is not a valid version: {source}")]
    InvalidKernelVersion {
        /// The offending version string.
        value: String,
        /// Underlying semver parse error.
        #[source]
        source: semver::Error,
    },

    /// The resolved kernel version falls outside the profile's declared range —
    /// the "declared intent" mismatch caught before the verify gate runs.
    #[error(
        "profile '{profile}' does not target kernel {kernel_version} \
         (applies_to_kernel = '{applies_to}')"
    )]
    KernelOutsideProfileRange {
        /// The patch profile.
        profile: String,
        /// The resolved kernel version that is out of range.
        kernel_version: String,
        /// The profile's declared range.
        applies_to: String,
    },

    /// A selected feature does not support the resolved SoC.
    #[error("feature '{feature}' does not support soc '{soc}' (supported socs: {supported})")]
    IncompatibleFeatureSoc {
        /// The feature being validated.
        feature: String,
        /// The resolved SoC.
        soc: String,
        /// Comma-separated SoCs the feature's `requires_soc` lists.
        supported: String,
    },

    /// A selected feature declares `requires_media_accel` but the resolved SoC
    /// provides no `[userspace]`/`[ffmpeg]` source stanzas to build the stack
    /// from. The remedy is to add those stanzas at the SoC layer (as RK3588 does)
    /// or drop the feature for this target.
    #[error(
        "feature '{feature}' builds the media-accel stack but soc '{soc}' declares no \
         [userspace]/[ffmpeg] sources — add them at socs/{soc}.toml or drop the feature"
    )]
    FeatureRequiresMediaAccel {
        /// The feature that requires the media-accel source trees.
        feature: String,
        /// The resolved SoC that lacks them.
        soc: String,
    },

    /// A selected feature does not support the resolved arch. The arch gate
    /// for a discrete-GPU capability feature, orthogonal to the SoC gate.
    #[error("feature '{feature}' does not support arch '{arch}' (supported arches: {supported})")]
    IncompatibleFeatureArch {
        /// The feature being validated.
        feature: String,
        /// The resolved arch.
        arch: String,
        /// Comma-separated arches the feature's `requires_arch` lists.
        supported: String,
    },

    /// Two selected features contribute an apt source with the same `name` but
    /// differing definitions, so the rootfs solve cannot tell which repo to
    /// activate. Identical duplicates are fine (de-duplicated); a genuine
    /// clash is rejected.
    #[error(
        "features '{feature}' and '{other}' both define apt source '{name}' with \
         different settings"
    )]
    ConflictingAptSource {
        /// One feature defining the source.
        feature: String,
        /// The other feature defining a clashing source of the same name.
        other: String,
        /// The apt-source name that clashes.
        name: String,
    },

    /// An `apt_sources` field cannot be rendered into the apt one-line source
    /// (`deb [signed-by=…] <uri> <suite> <components…>`): the line is positional
    /// and space-separated, so an empty value or one carrying whitespace or
    /// `[`/`]` would be parsed as line structure rather than content — and a
    /// non-http(s) URI would point the bootstrap solve at an arbitrary
    /// transport.
    #[error("feature '{feature}': apt source '{name}' has an unusable {field}: {value:?}")]
    AptSourceBadField {
        /// The feature contributing the source.
        feature: String,
        /// The apt source's `name`.
        name: String,
        /// Which field is unusable (`name`, `uri`, `suite`, or `components`).
        field: &'static str,
        /// The offending value.
        value: String,
    },

    /// The same feature was selected more than once. Features apply their overlay
    /// and packages, so a duplicate would apply an overlay twice — rejected rather
    /// than silently deduplicated.
    #[error("feature '{feature}' selected more than once")]
    DuplicateFeature {
        /// The repeated feature name.
        feature: String,
    },

    /// Two selected features declare a mutual conflict, so they cannot be
    /// combined in one build.
    #[error("features '{feature}' and '{conflicts_with}' cannot be combined")]
    ConflictingFeatures {
        /// One feature in the conflicting pair.
        feature: String,
        /// The other feature it conflicts with.
        conflicts_with: String,
    },

    /// The resolved suite (device `default_suite` or a `--suite` override) is not a
    /// well-formed Debian codename: it must be a bare token starting with an
    /// alphanumeric and drawn from `[A-Za-z0-9._-]`. Rejected at resolve so an
    /// invalid suite fails immediately instead of deep in `mmdebstrap`, and so a
    /// leading `-` can never reach the bootstrap as a positional (CFG-3, pairs with
    /// SUB-2's `--` hardening).
    #[error("invalid suite '{value}': must be a Debian codename (a bare token in [A-Za-z0-9._-] starting with an alphanumeric)")]
    InvalidSuite {
        /// The offending suite string.
        value: String,
    },

    /// An `extra_debs` entry does not set exactly one locator: it must carry either
    /// a `url` or a `path`, not both and not neither. The sha256
    /// identifies the offending entry.
    #[error("extra_deb (sha256 {sha256}) must set exactly one of `url` or `path`")]
    ExtraDebLocator {
        /// The content hash of the malformed entry.
        sha256: String,
    },

    /// An `extra_debs` entry's sha256 is not a 64-character lowercase-hex string,
    /// so it cannot be the content pin the build verifies the fetched bytes against.
    #[error("extra_deb sha256 '{value}' is not 64 lowercase hex characters")]
    ExtraDebBadHash {
        /// The offending sha256 string.
        value: String,
    },

    /// An `extra_debs` `path` locator escapes the config root: it is absolute or
    /// contains a `..` component. A `path` deb is resolved relative to a config root
    /// (an overlay may ship it), so it must stay within one — an out-of-root
    /// read is a config-containment breach, not a valid source.
    #[error("extra_deb path '{value}' must be a relative path within the config root (no leading `/`, no `..`)")]
    ExtraDebUnsafePath {
        /// The offending path string.
        value: String,
    },

    /// A patch handed to `patch import` had no content to normalize.
    #[error("patch is empty")]
    PatchEmpty,

    /// A patch handed to `patch import` carried no diff payload — a
    /// metadata-only mail or prose, which would be written as an empty patch.
    #[error("patch has no diff (no `diff --git`/`--- a/…` payload found)")]
    PatchNoDiff,

    /// A patch handed to `patch import` has no subject and none could be
    /// derived — a bare diff whose changed file could not be named, or an mbox
    /// missing its `Subject:` header. Pass `--subject`.
    #[error("patch has no subject and none could be derived (pass --subject)")]
    PatchMissingSubject,

    /// `patch import` could not choose a filename prefix for the requested position.
    /// Consecutive integer neighbors auto-degrade to a lettered sub-prefix
    /// ([`derive_prefix`](crate::profile::derive_prefix)), so this remains only for
    /// the one case with no room below it: prepending before a `000`-prefixed first
    /// entry. Pass an explicit destination label with `--as`.
    #[error(
        "cannot place a patch before prefix {after:03} (nothing sorts below it); \
         pass an explicit label with --as (e.g. --as media-accel/kernel/000a-<slug>.patch)"
    )]
    PatchPrefixNoGap {
        /// The prefix of the first entry, which the new patch would precede (`0`,
        /// since a higher first entry leaves integer room and does not reach here).
        after: u32,
    },

    /// A locale name is not one `locale-gen` could act on. The name reaches
    /// `/etc/locale.gen` as a `<name> <charset>` line and `/etc/locale.conf` as a
    /// shell-sourced `LANG=` value, so it must both carry a codeset and contain
    /// nothing a shell would interpret.
    #[error("invalid locale '{value}': {why}")]
    InvalidLocale {
        /// The offending locale.
        value: String,
        /// Why it cannot be used.
        why: &'static str,
    },

    /// A timezone is not a name `tzdata` could resolve. It becomes the target of the
    /// `/etc/localtime` symlink under `/usr/share/zoneinfo/`, so a name that escaped
    /// that directory would point the system clock at an arbitrary file.
    #[error("invalid timezone '{value}': {why}")]
    InvalidTimezone {
        /// The offending timezone.
        value: String,
        /// Why it cannot be used.
        why: &'static str,
    },

    /// A keymap field carries something `/etc/default/keyboard` cannot hold. That
    /// file is *sourced by shell* (`console-setup`, `keyboard-setup`), so a value with
    /// a quote or a substitution in it would execute rather than configure.
    #[error("invalid keymap {field} '{value}': {why}")]
    InvalidKeymap {
        /// Which XKB field (`layout`, `model`, `variant`, `options`).
        field: &'static str,
        /// The offending value.
        value: String,
        /// Why it cannot be used.
        why: &'static str,
    },
}
