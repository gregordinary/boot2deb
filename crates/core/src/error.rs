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
    #[error(
        "{kind} '{name}' not found (looked at {path}){}",
        not_found_hint(kind, similar)
    )]
    NotFound {
        /// Layer kind, e.g. `"device"`.
        kind: &'static str,
        /// The name that was looked up.
        name: String,
        /// The path that was tried.
        path: String,
        /// Names of the same kind close enough to `name` to be the intended one,
        /// best match first, or empty when nothing is near.
        ///
        /// A name is an *open* set — unlike a closed axis such as `--kernel`, whose
        /// error can name every valid value — so the honest answer to a typo is the
        /// handful it might have been, not the whole inventory. Empty is not "no such
        /// names exist"; it is "none of them look like this one", and the message then
        /// falls back to naming the command that lists them.
        similar: Vec<String>,
    },

    /// A flat layer name or manifest filename (from a CLI argument or a config
    /// cross-reference) is not a bare identifier, so it cannot be trusted to join
    /// into a filesystem path. Names must match `[A-Za-z0-9._-]`, be non-empty, not
    /// start with a dot, and contain no path separators or `..` — this stops a `../`
    /// traversal or an absolute path from escaping the config root (both a read *and*,
    /// via `lock_path`, a write target). Recipe references, which nest one level and
    /// so admit a single `/`, are validated separately ([`InvalidRecipeRef`](Self::InvalidRecipeRef)).
    #[error("invalid {kind} name '{name}': must be a bare identifier ([A-Za-z0-9._-], no separators or '..')")]
    InvalidName {
        /// Layer kind, e.g. `"soc"`.
        kind: &'static str,
        /// The offending name.
        name: String,
    },

    /// A device slug is not a valid host name. Tighter than
    /// [`InvalidName`](Self::InvalidName), which the slug also has to satisfy and
    /// automatically does: a board's slug is the host name its image comes up under
    /// unless the board states another, so a slug outside the host-name shape would
    /// make the default `boot2deb new-device` writes one no image could carry.
    #[error(
        "invalid device name '{name}': {why} — the slug is also the image's default \
         hostname, so it must be a valid host name (e.g. 'my-board')"
    )]
    InvalidDeviceName {
        /// The offending slug.
        name: String,
        /// Which part of the host-name shape it fails.
        why: &'static str,
    },

    /// A device's `extends` chain closes on itself, so there is no base-most device
    /// to start the merge from. Reported with the whole walk, base-most last, because
    /// the offending edge is rarely in the device that was asked for.
    #[error("device '{device}' extends itself through {chain}")]
    DeviceExtendsCycle {
        /// The device whose resolution was requested.
        device: String,
        /// The walk that closed, joined with `-> `.
        chain: String,
    },

    /// A device's `extends` is present but not a string, so it names no parent
    /// device. Caught while walking the chain — before deserialization, which is
    /// where the key's own type would otherwise be checked — so the message names
    /// the device rather than a merged value's field.
    #[error("device '{device}': extends must be a device name (a string), found {found}")]
    InvalidDeviceExtends {
        /// The device whose file carries the bad value.
        device: String,
        /// The TOML type that was found instead.
        found: &'static str,
    },

    /// A device layer's value for one of the accumulating array keys — `caveats`,
    /// `expect`, `nonfree_firmware_packages`, `packages`, `exclude` — is present but is
    /// not an array.
    ///
    /// Caught while walking the `extends` chain rather than at deserialization, so the
    /// message names the file that holds it. That attribution is the whole point: the
    /// merge is last-wins, so a malformed value in an ancestor that the child overrides
    /// would deserialize cleanly and never be reported.
    #[error("{path}: {field} must be an array, found {found}")]
    InvalidDeviceField {
        /// The device file that holds the bad value.
        path: String,
        /// The key whose value is not an array.
        field: &'static str,
        /// The TOML type that was found instead.
        found: &'static str,
    },

    /// A recipe reference (a CLI argument or config cross-reference) is not a valid
    /// `<device>/<leaf>` — or bare `<leaf>` — path. Recipes are the one layer that
    /// nests one level under a device folder, so a reference may carry a *single*
    /// interior `/`; both halves must be bare identifiers (`[A-Za-z0-9._-]`, non-empty,
    /// no leading dot). That bars `..`, a leading/trailing/absolute/doubled slash, and
    /// more than one separator, so a reference can never escape `recipes/` when joined
    /// into `recipes/<ref>.toml`, its `.lock`, or its manifest sibling.
    #[error(
        "invalid recipe name '{name}': must be `<device>/<leaf>` or a bare identifier \
         (at most one '/', each part [A-Za-z0-9._-], no '..')"
    )]
    InvalidRecipeRef {
        /// The offending reference.
        name: String,
    },

    /// An overlay ships a copy of a *trust anchor* asset (the Debian archive
    /// keyring) that the shipped root also provides. Overlays are operator-supplied
    /// but not necessarily audited line-by-line, and honoring an overlay's archive
    /// keyring silently changes which `Release` signatures apt accepts — a
    /// trust-anchor swap. Resolution fails closed rather than pick the
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

    /// A solved package manifest has a line that is not
    /// `name version arch sha256` — see [`manifest::parse`](crate::manifest::parse).
    ///
    /// Attributed to a line rather than to the file as a whole because a manifest is a
    /// content pin: dropping the line would silently shrink the package set the file
    /// claims to pin, and a reader could not tell that had happened.
    #[error("{path}:{line}: not a `name version arch sha256` manifest line: {content}")]
    InvalidManifest {
        /// The manifest that failed to parse.
        path: String,
        /// 1-based line number of the offending line.
        line: usize,
        /// The line itself, trimmed.
        content: String,
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

    /// The chosen u-boot series is not in the device's `supported_uboot_series`.
    #[error(
        "device '{device}' does not support u-boot series '{series}' (supported: {supported})"
    )]
    UnknownUbootSeriesForDevice {
        /// The device being resolved.
        device: String,
        /// The requested u-boot series.
        series: String,
        /// Comma-separated list of what the device does support.
        supported: String,
    },

    /// A recipe declares `deliverable = "uboot"` on a board whose boot method builds
    /// no bootloader of ours (e.g. depthcharge, whose firmware is its own) — so there
    /// is no u-boot-only artifact to emit.
    #[error(
        "device '{device}' builds no bootloader under boot method '{boot_method}', \
         so it has no u-boot-only deliverable"
    )]
    UbootOnlyWithoutBootloader {
        /// The device being resolved.
        device: String,
        /// The boot method that builds no bootloader.
        boot_method: String,
    },

    /// An override was set on an axis the resolved deliverable does not have — a
    /// rootfs axis (`--suite`, `--feature`, `--locale`, …) on a `deliverable = "uboot"`
    /// recipe, which resolves no kernel, no suite, and no rootfs.
    ///
    /// Named rather than dropped: an inapplicable *stage* is already a user error worth
    /// naming rather than a silent skip, and an inapplicable *axis* is the same mistake
    /// one level up. Silently discarding it would accept a misspelled feature name, or
    /// an `--image-size` that is a hard error on every other path, and exit 0.
    #[error(
        "{flag} does not apply to a u-boot-only build of '{device}': the deliverable is \
         the bootloader alone, so it resolves no kernel, suite, or rootfs for that axis \
         to change — drop the flag (or the recipe key that sets it), or build an image \
         recipe"
    )]
    OverrideNotApplicable {
        /// The device being resolved.
        device: String,
        /// The offending axis as its flag spells it (e.g. `--suite`, `--image-size`),
        /// or as its recipe key spells it for an axis that has no flag
        /// (`ssh_authorized_keys`).
        flag: &'static str,
    },

    /// The device declares `supported_uboot_series` but no `default_uboot_series`,
    /// and none was selected — so a build has no u-boot series to resolve.
    #[error(
        "device '{device}' lists supported_uboot_series but no default_uboot_series — \
         set one (or select a series per recipe / with --uboot-series)"
    )]
    MissingDefaultUbootSeries {
        /// The device being resolved.
        device: String,
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
    #[error(
        "device '{device}' does not support boot method '{boot_method}' (supported: {supported})"
    )]
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

    /// The derived rootfs offset (`kpart_offset + slots × kpart_size`) overflows
    /// [`u64`]. Only reachable from author-supplied sizes near the type's ceiling, and
    /// reported rather than wrapped: a wrapped value would place the rootfs partition
    /// inside the kernel slots.
    #[error(
        "depthcharge geometry overflows: kpart offset {offset} + {slots} × {size} does \
         not fit a 64-bit byte offset"
    )]
    KpartGeometryOverflow {
        /// The authored first-slot offset.
        offset: String,
        /// The authored per-slot size (the device's if it states one).
        size: String,
        /// The slot count.
        slots: u8,
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

    /// `kpart_slots` is outside `1..=MAX_KPART_SLOTS`. Zero slots would leave the
    /// firmware nothing to boot; above the cap is a typo, not an intent — see
    /// [`MAX_KPART_SLOTS`](crate::chromeos::MAX_KPART_SLOTS).
    #[error(
        "kpart_slots = {value} is out of range (1-{max}); 2 is what gives a kernel \
         upgrade a slot to fall back to"
    )]
    InvalidKpartSlots {
        /// The authored value.
        value: u8,
        /// The cap ([`MAX_KPART_SLOTS`](crate::chromeos::MAX_KPART_SLOTS)).
        max: u8,
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

    /// A selected feature contributes a kernel input — kconfig fragments or a patch
    /// series — while the resolved kernel is a distro package that compiles
    /// nothing. The capability's driver would never be patched in or configured on,
    /// so the feature would install its userspace against hardware support that is
    /// not there.
    #[error(
        "feature '{feature}' declares `{what}`, but kernel '{kernel}' is a \
         distro-package kernel that compiles nothing — the feature's driver would \
         never be built"
    )]
    FeatureNeedsCompiledKernel {
        /// The feature that contributed the kernel input.
        feature: String,
        /// The distro-package kernel it was paired with.
        kernel: String,
        /// The feature field that would be ignored (`config_fragments` or
        /// `patch_series`).
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

    /// A `kmods/<name>.toml` layer is malformed: a non-package-safe name, or a
    /// `subdir`/`patch_dir`/patch/module path that is absolute or escapes the tree it is
    /// joined onto. The subdir feeds `make M=` and the local patches are read from the
    /// config root, so an escaping value would build or read a file from outside the
    /// intended tree.
    #[error("kmod '{kmod}' is invalid (kmods/{kmod}.toml): {why}")]
    InvalidKmod {
        /// The kmod file stem — the offending value itself when the name is what is
        /// wrong.
        kmod: String,
        /// What is wrong with it.
        why: &'static str,
    },

    /// A device's `device_kmods` names the same kmod twice. One kmod is one build node,
    /// one deb, and one lock pin, so a repeat is a config mistake with no meaning rather
    /// than a request to build it twice.
    #[error("device '{device}' names kmod '{kmod}' more than once")]
    DuplicateKmod {
        /// The device being resolved.
        device: String,
        /// The repeated kmod name.
        kmod: String,
    },

    /// A patch series' `applies_to_kernel` is not a valid semver requirement.
    #[error("series '{series}' has invalid applies_to_kernel '{value}': {source}")]
    InvalidVersionReq {
        /// The series whose range failed to parse.
        series: String,
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

    /// An `image_size` could not be parsed — see
    /// [`parse_image_size`](crate::size::parse_image_size).
    ///
    /// Distinct from [`InvalidSize`](Self::InvalidSize) because `image_size` accepts a
    /// form the offsets do not: a size measured from the rootfs. An author who wrote a
    /// bare `fit` needs to be shown the slack forms, and an author who mistyped an offset
    /// must not be.
    #[error(
        "invalid image size '{value}' (expected a size like '4G', or a measured size \
         like 'fit+20%' / 'fit+512M' — a bare 'fit' names no slack, and a fitted \
         filesystem with nothing free boots into a full disk)"
    )]
    InvalidImageSize {
        /// The offending `image_size` string.
        value: String,
    },

    /// A version tag could not be parsed as a semver version, so no series range
    /// can be matched against it. Raised for either axis — a kernel tag or a u-boot
    /// one — hence the axis-neutral wording.
    #[error("'{value}' is not a version a series range can be matched against: {source}")]
    InvalidVersion {
        /// The offending version string.
        value: String,
        /// Underlying semver parse error.
        #[source]
        source: semver::Error,
    },

    /// A kernel definition names one or more patch series but no `patches_url`. The
    /// lock records the source beside the commit, and a commit id is meaningless
    /// outside the repo it came from, so a series without a source cannot be pinned
    /// honestly.
    #[error(
        "kernel '{kernel}' names patch series [{series}] but no patches_url — \
         add `patches_url` to kernels/{kernel}.toml (the lock records it beside the \
         pinned commit, which means nothing without the repo it is in)"
    )]
    MissingPatchesUrl {
        /// Kernel definition id.
        kernel: String,
        /// The series it names, comma-joined.
        series: String,
    },

    /// A device selects a u-boot patch series but its `rockchip-rkbin` boot method
    /// declares no `patches_url`. The u-boot series has nowhere to be fetched from and
    /// no source to pin beside its commit, mirroring [`MissingPatchesUrl`].
    ///
    /// [`MissingPatchesUrl`]: ConfigError::MissingPatchesUrl
    #[error(
        "device '{device}' selects u-boot series '{series}' but boot method \
         'rockchip-rkbin' declares no patches_url — add `patches_url` to \
         boot-methods/rockchip-rkbin.toml"
    )]
    MissingUbootPatchesUrl {
        /// The device being resolved.
        device: String,
        /// The u-boot series it selects.
        series: String,
    },

    /// The resolved kernel version falls outside the series' declared range —
    /// the "declared intent" mismatch caught before the verify gate runs.
    #[error(
        "series '{series}' does not target kernel {kernel_version} \
         (applies_to_kernel = '{applies_to}')"
    )]
    KernelOutsideSeriesRange {
        /// The patch series.
        series: String,
        /// The resolved kernel version that is out of range.
        kernel_version: String,
        /// The series' declared range.
        applies_to: String,
    },

    /// The resolved u-boot version falls outside the series' declared
    /// `applies_to_uboot` range — the u-boot counterpart of
    /// [`KernelOutsideSeriesRange`](ConfigError::KernelOutsideSeriesRange).
    #[error(
        "series '{series}' does not target u-boot {uboot_version} \
         (applies_to_uboot = '{applies_to}')"
    )]
    UbootOutsideSeriesRange {
        /// The patch series.
        series: String,
        /// The resolved u-boot version that is out of range.
        uboot_version: String,
        /// The series' declared range.
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

    /// A recipe's `[[data_volumes]]` mount path is unusable — not absolute, root
    /// itself, trailing-slashed, or declared twice.
    #[error("data volume mount '{mount}' {why}")]
    DataVolumeMount {
        /// The offending mount path.
        mount: String,
        /// What is wrong with it, as a predicate completing the message.
        why: String,
    },

    /// A recipe's `[[data_volumes]]` label cannot serve as the volume's identity
    /// across a reimage — empty, too long for the filesystem, holding a character
    /// that would need quoting in `/etc/fstab`, or declared twice.
    #[error("data volume label '{label}' {why}")]
    DataVolumeLabel {
        /// The offending label.
        label: String,
        /// What is wrong with it, as a predicate completing the message.
        why: String,
    },

    /// A recipe's `[[data_volumes]]` `match` names something that cannot identify
    /// a disk.
    #[error("data volume match '{value}' {why}")]
    DataVolumeMatch {
        /// The offending match value.
        value: String,
        /// What is wrong with it, as a predicate completing the message.
        why: String,
    },

    /// A recipe declares `[[data_volumes]]` without selecting the `data-volume`
    /// feature, or selects that feature without declaring any volume. The two
    /// halves are useless apart — the feature carries the first-boot hook that
    /// acts on the declarations, and the declarations are inert without it — and
    /// pulling the feature in implicitly would be the provider auto-resolution
    /// this config model does not do.
    #[error(
        "recipe '{recipe}' {problem} — a data volume needs both the 'data-volume' \
         feature (which carries the first-boot hook) and at least one \
         [[data_volumes]] entry (which says what to mount where)"
    )]
    DataVolumeFeatureMismatch {
        /// The recipe with only one of the two halves.
        recipe: String,
        /// Which half is missing, as a phrase completing the message.
        problem: String,
    },

    /// A recipe declares a `[support]` claim while selecting a feature that builds
    /// FFmpeg `--enable-nonfree`. A claim is a statement that a configuration is fit
    /// to publish, and this one may not be published at all, so the two cannot both
    /// stand.
    ///
    /// It is a gate on *authored* recipes only. The nonfree flavour is reached as a
    /// feature-variant reference (`<recipe>+<feature>`), which carries no claim of its
    /// own and appears in no support matrix — that path is unaffected.
    #[error(
        "recipe '{recipe}' declares a [support] claim but selects '{feature}', which \
         builds FFmpeg with --enable-nonfree — the result may be built and used but \
         not redistributed, so it cannot carry a claim that it is fit to publish. \
         Reach the nonfree flavour as '{recipe}+{feature}' instead of authoring a \
         recipe for it, or drop the [support] block"
    )]
    NonfreeSupportClaim {
        /// The recipe carrying the claim.
        recipe: String,
        /// The selected feature that makes the build undistributable.
        feature: String,
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
    ///
    /// `name` and `signed_by` are held to the tighter rule that they be portable
    /// file-name stems (`[A-Za-z0-9._-]`, not `.` or `..`), because each names a
    /// file: `name` the rootfs's `sources.list.d` entry and keyring, `signed_by` the
    /// vendored keyring the repo is verified against. A separator or dot segment in
    /// either would place a file — or read a trust anchor — outside the directory
    /// that holds it.
    #[error("feature '{feature}': apt source '{name}' has an unusable {field}: {value:?}")]
    AptSourceBadField {
        /// The feature contributing the source.
        feature: String,
        /// The apt source's `name`.
        name: String,
        /// Which field is unusable (`name`, `uri`, `suite`, `components`, or
        /// `signed_by`).
        field: &'static str,
        /// The offending value.
        value: String,
    },

    /// The same feature was selected more than once. Features apply their overlay
    /// and packages, so a duplicate would apply an overlay twice — rejected rather
    /// than silently deduplicated.
    ///
    /// Also raised when building a [`BuildPoint`](crate::buildpoint::BuildPoint),
    /// which rejects it earlier still: folding a repeat there would make the
    /// reference disagree with what was asked for, and the reference is what names
    /// the lock, the solved manifest, and the build directory.
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

    /// A selected feature requires a capability no other selected feature provides —
    /// an incomplete composition, caught at resolve because the image it would build
    /// is bootable but broken (`jellyfin` with no FFmpeg provider installs a server
    /// that exits at startup).
    ///
    /// The message names the providers the config tree carries, because the fix is
    /// always to add one and the user cannot be expected to know which features
    /// declare the capability.
    #[error("feature '{feature}' requires capability '{capability}', which no selected feature provides{}", providers_hint(.providers))]
    MissingCapability {
        /// The feature whose requirement is unmet.
        feature: String,
        /// The capability name it asked for.
        capability: String,
        /// Every feature in the config tree providing that capability, sorted. Empty
        /// if the tree has none, which means the requirement is misspelled or the
        /// provider was never authored.
        providers: Vec<String>,
    },

    /// A configured value cannot be carried to the file, line, or command line it has
    /// to reach, so resolution refuses it rather than emitting an image built around it.
    ///
    /// **One variant for eight axes** — locale, timezone, NTP server, keymap field,
    /// hostname, suite, kernel cmdline, depthcharge board profile — because they are one
    /// failure: a value the config authored is not a value its destination can hold. The
    /// message says which axis and why, and its `what` field carries the axis as
    /// data, so a test asserting `what == "hostname"` pins more than a variant name did.
    ///
    /// The value is rendered with `{:?}`, not quoted by hand: several of these are
    /// rejected *for* carrying a newline or a quote, and printing one raw would put the
    /// thing being rejected into the operator's terminal unescaped.
    ///
    /// Axes with more to say keep their own variant — [`InvalidKmod`](Self::InvalidKmod)
    /// names the file, [`InvalidDeviceDts`](Self::InvalidDeviceDts) the device,
    /// [`AptSourceBadField`](Self::AptSourceBadField) the feature and the source. What
    /// is collapsed here is what carried nothing the message did not already say.
    #[error("invalid {what} {value:?}: {why}")]
    InvalidField {
        /// The axis, as the message names it: `locale`, `timezone`, `NTP server`,
        /// `hostname`, `suite`, `kernel cmdline`, `board profile`, or `keymap <field>`.
        what: &'static str,
        /// The offending value.
        value: String,
        /// Why its destination cannot hold it.
        why: &'static str,
    },

    /// The resolved suite is well-formed but not in the device's `supported_suites`.
    /// Separate from [`InvalidField`](Self::InvalidField) with `what = "suite"`: that
    /// one asks whether the string could name a suite at all, this one whether *this
    /// board* is built for it. Catches both a typo and a suite whose kernel predates the SoC — either of
    /// which would otherwise fail minutes into a bootstrap.
    #[error("device '{device}' does not support suite '{suite}' (supported: {supported})")]
    UnsupportedSuite {
        /// The device being resolved.
        device: String,
        /// The requested suite.
        suite: String,
        /// Comma-separated suites the device does support.
        supported: String,
    },

    /// A device's `supported_suites` mixes the `*` wildcard with named codenames.
    /// The wildcard already admits everything, so the named entries can only be a
    /// narrowing the list does not perform — two incompatible claims in one field.
    #[error(
        "device '{device}' lists supported_suites = [{supported}]: the '*' wildcard \
         admits every codename, so naming others alongside it states two different \
         claims — use either ['*'] or the explicit list"
    )]
    SuiteWildcardMixed {
        /// The device whose list is contradictory.
        device: String,
        /// Comma-separated entries as authored.
        supported: String,
    },

    /// A device declares an empty `supported_suites`, which admits nothing: every
    /// suite — including its own `default_suite` — would be rejected, so no image
    /// could ever be built for it.
    #[error(
        "device '{device}' declares an empty supported_suites, so no suite resolves — \
         list the codenames this board is built for, or ['*'] for any"
    )]
    NoSupportedSuites {
        /// The device with the empty list.
        device: String,
    },

    /// A conditional package entry names an empty `suites` list, so it applies to no
    /// build at all. Config that can never take effect is a mistake rather than a
    /// no-op — most likely a list someone emptied instead of deleting the entry.
    #[error(
        "package '{package}' in {layer} names an empty `suites` list, so it is installed \
         on no suite; delete the entry, or list the suites that carry the package"
    )]
    PackageSuitesEmpty {
        /// The package the entry names.
        package: String,
        /// The layer or feature the entry was read from, for a message that points at
        /// the file to edit.
        layer: String,
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
    /// ([`derive_prefix`](crate::series::derive_prefix)), so this remains only for
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

    /// An `ssh_authorized_keys` entry is not a usable `authorized_keys` line. Rejected
    /// at resolution because `sshd` reports a line it cannot parse only in its own log,
    /// on a board that may have no console — so the alternative to this error is an
    /// image whose key silently does not work.
    ///
    /// The entry is quoted back truncated: a key blob is ~70 to ~700 characters, and a
    /// message that reprints one buries its own explanation.
    #[error(
        "invalid ssh_authorized_keys entry {index} ('{}'): {why}",
        crate::error::elide(value, 40)
    )]
    InvalidAuthorizedKey {
        /// Position in the authored list, 1-based — the entry's only name.
        index: usize,
        /// The offending entry.
        value: String,
        /// Why it cannot be used.
        why: &'static str,
    },

    /// A `first_boot_password_length` outside the accepted range. A short generated
    /// password is the one setting whose weakness is invisible on the finished image:
    /// nothing about a booted board reveals how much entropy its first credential had.
    #[error(
        "first_boot_password_length {value} is outside {min}..={max} — \
         {min} is the floor at which guessing the login over the network stays \
         infeasible, and the default {default} keeps that margin even if the image \
         itself (and so the password hash in it) is shared"
    )]
    InvalidPasswordLength {
        /// The offending length.
        value: u32,
        /// Smallest accepted length.
        min: u32,
        /// Largest accepted length.
        max: u32,
        /// The length a config root that says nothing gets.
        default: u32,
    },
}

/// `value` truncated to `max` characters with a trailing `…`, for an error message that
/// must name a long authored value without being swamped by it. Character-, not
/// byte-indexed, so a multi-byte comment cannot split a `char`.
///
/// Control characters are escaped, because one of the values this quotes back is
/// rejected *for* containing a newline — and a diagnostic that printed it raw would
/// break its own explanation across two lines.
fn elide(value: &str, max: usize) -> String {
    let mut out = String::with_capacity(max);
    for c in value.chars().take(max) {
        if c.is_control() {
            out.extend(c.escape_debug());
        } else {
            out.push(c);
        }
    }
    if value.chars().count() > max {
        out.push('…');
    }
    out
}

/// The trailing hint on a [`ConfigError::NotFound`]: the near-miss names, or the
/// command that lists what does exist.
///
/// The two halves answer different questions. Near misses answer "did I typo this?",
/// which is the common case for a name a user typed. When nothing is near, the useful
/// answer is where to look — and that only exists for the kinds the CLI can enumerate,
/// so the rest get no hint rather than a pointer to a command that does not exist.
fn not_found_hint(kind: &str, similar: &[String]) -> String {
    if !similar.is_empty() {
        let names: Vec<String> = similar.iter().map(|n| format!("'{n}'")).collect();
        return format!(" — did you mean {}?", names.join(" or "));
    }
    match kind {
        "device" => " — `boot2deb list-devices` shows what is available".into(),
        "recipe" | "lock" => " — `boot2deb list-recipes` shows what is available".into(),
        "kernel" => " — `boot2deb list-kernels` shows what is available".into(),
        "feature" => " — `boot2deb list-features` shows what is available".into(),
        "kmod" => " — `boot2deb list-kmods` shows what is available".into(),
        _ => String::new(),
    }
}

/// The trailing hint on [`ConfigError::MissingCapability`], naming the features that
/// would satisfy the requirement.
///
/// An empty `providers` is a different failure from a forgotten one — the capability
/// is misspelled, or no provider has been authored — so it points at the listing
/// rather than suggesting an addition that does not exist.
fn providers_hint(providers: &[String]) -> String {
    if providers.is_empty() {
        return " — no feature in this config tree provides it; \
                `boot2deb list-features` shows what is available"
            .into();
    }
    let names: Vec<String> = providers.iter().map(|n| format!("'{n}'")).collect();
    format!(" — add one of {}", names.join(", "))
}

/// Names from `candidates` close enough to `name` to be worth suggesting, best first.
///
/// Two rules, both cheap and both aimed at the mistakes people actually make:
/// a candidate that *contains* `name` (or vice versa) is a truncation or an extra
/// qualifier, and a candidate within a small edit distance is a typo. The distance
/// budget scales with length — one edit in a short name is a different word, three in
/// a long one is still recognisably the same one.
///
/// At most three, because a list long enough to scan is not a suggestion.
pub(crate) fn similar_names(name: &str, candidates: &[String]) -> Vec<String> {
    let budget = match name.chars().count() {
        0..=4 => 1,
        5..=9 => 2,
        _ => 3,
    };
    let mut scored: Vec<(usize, &String)> = candidates
        .iter()
        .filter(|c| c.as_str() != name)
        .filter_map(|c| {
            // A containment match ranks ahead of any edit-distance one: `turing-rk1`
            // against `turing-rk1/forky` is not a typo, it is an under-specified name.
            if c.contains(name) || name.contains(c.as_str()) {
                return Some((0, c));
            }
            let d = edit_distance(name, c);
            (d <= budget).then_some((d, c))
        })
        .collect();
    scored.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    scored.into_iter().take(3).map(|(_, c)| c.clone()).collect()
}

/// Levenshtein distance between `a` and `b`, over characters.
///
/// The two-row form: only the previous row is needed, so the cost is O(len(a)) memory
/// for candidate lists that are at most a few dozen names.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut prev: Vec<usize> = (0..=a.len()).collect();
    let mut cur = vec![0usize; a.len() + 1];
    for (j, bc) in b.iter().enumerate() {
        cur[0] = j + 1;
        for (i, ac) in a.iter().enumerate() {
            let cost = usize::from(ac != bc);
            cur[i + 1] = (prev[i + 1] + 1).min(cur[i] + 1).min(prev[i] + cost);
        }
        std::mem::swap(&mut prev, &mut cur);
    }
    prev[a.len()]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_typo_suggests_the_name_it_is_one_edit_from() {
        let names: Vec<String> = ["turing-rk1", "h96-max-m9", "asus-c201"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(similar_names("turing-rk", &names), vec!["turing-rk1"]);
        assert_eq!(similar_names("asus-c202", &names), vec!["asus-c201"]);
        // Nothing near: no suggestion rather than the least-bad of a bad set.
        assert!(similar_names("nope", &names).is_empty());
        // An exact match is not a suggestion — it would not be an error.
        assert!(similar_names("turing-rk1", &names).is_empty());
    }

    #[test]
    fn an_under_specified_name_suggests_what_extends_it() {
        let recipes: Vec<String> = ["turing-rk1/forky", "turing-rk1/trixie", "asus-c201/forky"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let hits = similar_names("turing-rk1", &recipes);
        assert!(hits.contains(&"turing-rk1/forky".to_string()), "{hits:?}");
        assert!(hits.contains(&"turing-rk1/trixie".to_string()), "{hits:?}");
        assert!(!hits.contains(&"asus-c201/forky".to_string()), "{hits:?}");
    }

    #[test]
    fn the_hint_falls_back_to_the_command_that_lists_the_kind() {
        assert!(not_found_hint("device", &[]).contains("list-devices"));
        assert!(not_found_hint("recipe", &[]).contains("list-recipes"));
        // A kind with no listing command gets no pointer rather than a wrong one.
        assert_eq!(not_found_hint("series", &[]), "");
        // Near misses win over the fallback: they are the more specific answer.
        let hint = not_found_hint("device", &["turing-rk1".to_string()]);
        assert_eq!(hint, " — did you mean 'turing-rk1'?");
    }
}
