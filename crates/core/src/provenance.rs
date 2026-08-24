//! Provenance manifest — the resolved build point plus every pin joined
//! into one document answering "exactly what went into this image," for support
//! and security response.
//!
//! Pure: a deterministic join of values the [`Lock`] and [`ResolvedBuild`] already
//! hold, plus the build-time facts the engine supplies ([`BuildFacts`] — the
//! solved manifest's content hash + package count, the host/cross identity, the
//! filesystem contract, the sandbox profile every build command ran under, and
//! the per-image first-boot credential). So the assembly and its canonical TOML
//! form are unit-testable without a build. It is a join of pins the build already
//! computes, not new tracking; license/SBOM data rides on the Debian packages
//! themselves and is out of scope.

use crate::lock::Lock;
use crate::model::ResolvedBuild;
use serde::Serialize;
use std::collections::BTreeMap;

/// Banner prepended to a serialized provenance manifest.
const BANNER: &str = "\
# boot2deb provenance manifest: the resolved build point + every pin.
# Emitted per built image. Contains the image's initial first-boot password
# ([credentials]) — treat this file as sensitive.
";

/// Banner prepended to a serialized [`SystemIdentity`].
const IDENTITY_BANNER: &str = "\
# boot2deb image identity. Written at build time, and read by tools that operate on
# this system from outside it — including when it cannot be booted or mounted.
#
# Carries no secrets. The build's provenance manifest holds the first-boot credential
# and the full pin list; it stays with the build and never ships inside the image.
";

/// Schema version of [`SystemIdentity`]. Bumped when a field changes meaning or is
/// removed; adding an optional field does not bump it.
const IDENTITY_VERSION: u32 = 1;

/// The build-time facts the engine supplies to [`assemble`] beyond the [`Lock`]
/// and [`ResolvedBuild`]: the host/cross identity, the solved manifest's digest +
/// size, and the generated first-boot credential. The engine owns these because
/// they are side effects (hashing the manifest, reading the RNG) that the pure
/// core does not perform.
pub struct BuildFacts<'a> {
    /// Detected build-host architecture (e.g. `x86_64`, `arm64`).
    pub host_arch: &'a str,
    /// Whether the build was cross-arch (host arch ≠ target arch).
    pub cross: bool,
    /// Lowercase-hex sha256 of the committed solved package manifest — the same
    /// content the lock's `[rootfs].manifest_sha256` pins.
    pub manifest_sha256: &'a str,
    /// Number of installed packages the solved manifest pins.
    pub package_count: usize,
    /// Default account name the image ships with.
    pub user: &'a str,
    /// The per-image first-boot password. Deliberately unique per
    /// build, so it is not derivable and the rootfs `/etc/shadow` is intentionally
    /// outside the byte-reproducibility claim.
    pub password: &'a str,
    /// boot2deb crate version that ran the build (`CARGO_PKG_VERSION`).
    pub builder_version: &'a str,
    /// Short git commit of the boot2deb checkout that ran the build, or `None` when
    /// built outside a git checkout (e.g. from a source tarball).
    pub builder_commit: Option<&'a str>,
    /// Whether that checkout had uncommitted changes at build time. Only meaningful
    /// when `builder_commit` is `Some`.
    pub builder_dirty: bool,
    /// The on-disk contract the rootfs filesystem was formatted to. The engine owns
    /// it because the values come from the formatter it links, which the pure core
    /// does not depend on.
    pub filesystem: FilesystemProvenance,
    /// The environment and mounts every sandboxed build command ran under. The engine
    /// owns it for the same reason as [`filesystem`](Self::filesystem): the values are
    /// resolved by the sandbox library it links.
    pub sandbox: SandboxProvenance,
}

/// The environment and mounts every sandboxed build command runs under, as the
/// sandbox library resolves them.
///
/// What a compiled package contains depends on the environment its build ran in and
/// the filesystem that build saw. Neither is stated by a source pin, and neither is
/// stable across sandbox-library releases — the base environment and the mount profile
/// are both outside that library's compatibility promise — so the values are recorded
/// rather than inferred from a version. Two images built from one lock that differ can
/// then be compared on the inputs that could explain it.
///
/// This is the profile every command *starts from*: what the sandbox establishes before
/// a run adds anything of its own. A run appends its own working and artifact directories
/// as binds, and an `apt` run additionally shares the host network, which binds the host's
/// `/etc/resolv.conf` read-only — both are per-run and host-specific, so neither is part
/// of the record.
///
/// Carried through [`BuildFacts`] as one value because it is one fact, and split across
/// two manifest keys — [`sandbox_env`](ProvenanceManifest::sandbox_env) and
/// [`sandbox_mounts`](ProvenanceManifest::sandbox_mounts) — because TOML requires every
/// array-of-tables after every table. It is therefore not a section of its own, and
/// carries no `Serialize` of its own.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxProvenance {
    /// The command's complete environment, keyed by variable name.
    pub env: BTreeMap<String, String>,
    /// Every mount the sandbox establishes, in the order it establishes them.
    pub mounts: Vec<SandboxMount>,
}

/// One mount a sandboxed build command runs under, for the manifest's
/// `[[sandbox_mounts]]` list.
///
/// One flat shape covers every kind, so the list diffs a line at a time; a field the
/// kind does not have is absent rather than empty.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SandboxMount {
    /// What the mount is: `tmpfs`, `procfs`, `devpts`, `bind`, `symlink`, or `raw`.
    pub kind: String,
    /// Where it is established, as an absolute path **inside** the sandbox. For a
    /// symlink this is the link itself, not what it points at.
    pub target: String,
    /// What is exposed at [`target`](Self::target): the host path a bind takes, or the
    /// path a symlink points at. Absent for a kind that has neither.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The filesystem type, where the mount names one. Absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fstype: Option<String>,
    /// The kernel's `MS_*` flag word, `0x`-prefixed 8-digit hex. Hex rather than a
    /// decimal integer because it is a bit set, and only hex diffs one bit at a time.
    /// Absent for a kind that passes no flags at all; `0x00000000` where it passes an
    /// empty word.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub flags: Option<String>,
    /// The filesystem data string the kernel receives (`mode=1777`). Absent where the
    /// mount carries none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub options: Option<String>,
    /// Whether a bind is remounted read-only. Absent for every other kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub read_only: Option<bool>,
}

/// The resolved build point + every pin, joined into one document. Each
/// section is a flat table so the manifest reads cleanly and serializes to valid
/// TOML (scalars only, no nested tables within a section).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ProvenanceManifest {
    /// Resolved device / arch / suite / features build point.
    pub image: ImageProvenance,
    /// Every pinned source ref + commit (kernel, patches, u-boot, userspace, ffmpeg).
    pub sources: SourcesProvenance,
    /// Rootfs suite + the content-pinned solved-manifest reference.
    pub rootfs: RootfsProvenance,
    /// The on-disk contract the rootfs filesystem was formatted to — the one
    /// determinant of the image's bytes that no source pin covers.
    pub filesystem: FilesystemProvenance,
    /// Verified rkbin blob pins. Absent when the boot method consumes no rkbin blobs
    /// — a depthcharge board's firmware is its own, so there is no ATF or DDR TPL in
    /// its boot chain to record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blobs: Option<BlobsProvenance>,
    /// Build host / toolchain identity.
    pub toolchain: ToolchainProvenance,
    /// Which boot2deb produced the image — the builder axis of provenance.
    pub built_with: BuiltWithProvenance,
    /// First-boot credential — the per-image secret.
    pub credentials: CredentialsProvenance,
    /// The environment every sandboxed build command carries, variable name to value.
    /// boot2deb declares it in full rather than composing over the sandbox library's
    /// base, so this is the whole of what a compile sees. Declared after the last
    /// `[section]` struct and before the arrays-of-tables, since it serializes as a
    /// table itself. See [`SandboxProvenance`] for what the pair of keys records.
    pub sandbox_env: BTreeMap<String, String>,
    /// Pre-built `extra_debs` pulled from outside the Debian mirror,
    /// each content-pinned by sha256 — part of "exactly what went into this image."
    /// Omitted when none. Declared before the durability list so both arrays-of-tables
    /// serialize after every `[section]` table (valid TOML ordering).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub extra_debs: Vec<crate::model::ExtraDeb>,
    /// Per-source pin durability *form*, derived offline from each fetched
    /// source's `(reference, commit)` — the offline half of "what went into this
    /// image": which pins rest on a durable named ref versus an undurable bare
    /// commit, visible without a network round-trip. The authoritative
    /// reachability check is the `verify-sources` probe. Declared with the other
    /// arrays-of-tables so it serializes after every `[section]` table.
    pub source_durability: Vec<SourceDurability>,
    /// Every mount a sandboxed build command runs under, in the order the sandbox
    /// establishes them — the half of the sandbox profile no other accessor reports,
    /// down to the `/dev` device nodes and symlinks. Declared last, with the other
    /// arrays-of-tables. See [`SandboxProvenance`].
    pub sandbox_mounts: Vec<SandboxMount>,
}

/// The offline durability *form* of one pinned source, for the manifest's
/// `[[source_durability]]` list. Joins the source's lock `reference` with its
/// classified [`PinForm`](crate::sources::PinForm) so a reader sees, per source,
/// whether the image rests on a durable named ref or an undurable bare commit
/// without a network round-trip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceDurability {
    /// Source axis name (`kernel`, `uboot`, `mpp`, `librga`, `libmali`,
    /// `ffmpeg-base`).
    pub source: String,
    /// The pinned ref — a tag/branch name, or the bare commit sha.
    pub reference: String,
    /// The offline durability form
    /// ([`PinForm::as_str`](crate::sources::PinForm::as_str)): `named-ref` or
    /// `bare-commit`.
    pub form: String,
}

/// The image's account of itself, written into the rootfs at
/// `/etc/boot2deb/image.toml`.
///
/// This is what an image tells a tool that operates on it **from outside**: a rescue
/// tool reading the disk from other media, quite possibly without mounting it and on a
/// machine that is not this board. It ships *inside* the image because the image is all
/// such a tool has.
///
/// It is deliberately a **subset** of [`ProvenanceManifest`] rather than the same
/// document, and the line between them is a security boundary: the manifest carries the
/// per-image first-boot password, and nothing that ships inside an image may. The
/// manifest also carries the solved-manifest digest, which *cannot* be here — that
/// digest is an output of the rootfs bootstrap, so it is not yet known when the file
/// being described is written into the rootfs it describes.
///
/// Most fields below are recoverable from the disk by other means, and exist so a
/// reader can cross-check what it inferred against what the image claims.
/// [`board`](IdentityImage::board) is the exception, and the reason the file
/// exists at all: the depthcharge board profile is not derivable from the image, and
/// `depthchargectl` normally recovers it by reading the *running* board's HWID and
/// device-tree compatibles — which is exactly what a tool running somewhere else cannot
/// do.
///
/// [`version`](Self::version) makes this a stable wire format. It is parsed by programs
/// versioned independently of boot2deb, so a reader must be able to tell which schema it
/// is looking at, and must tolerate fields it does not know.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SystemIdentity {
    /// Schema version of this document. Declared first so it serializes ahead of every
    /// `[table]`, which TOML requires of a top-level scalar.
    pub version: u32,
    /// What this system is.
    pub image: IdentityImage,
    /// The kernel it boots, and how a new one reaches it.
    pub kernel: IdentityKernel,
}

/// What the system is: the resolved build point, minus every value that is either
/// meaningless once the image is on a device or must not leave the build host.
///
/// Omitted deliberately, and each for its own reason: the first-boot credential (a
/// secret), the toolchain identity (a property of the build host, not the board),
/// `image_size` (superseded by the first-boot resize), and the locale/timezone/keymap
/// (already queryable from the system itself).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IdentityImage {
    /// Device name.
    pub device: String,
    /// Human-readable board description.
    pub description: String,
    /// Target architecture.
    pub arch: String,
    /// Target SoC.
    pub soc: String,
    /// Selected boot method. A reader detects this from the disk; the value here is a
    /// cross-check, and a disagreement is itself worth reporting.
    pub boot_method: String,
    /// The depthcharge board profile the kernel partition was signed for. **The one
    /// field here that is not recoverable from the disk**, and what an off-board
    /// `depthchargectl --board` needs. Absent under a boot method with no board profile.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub board: Option<String>,
    /// Debian suite.
    pub suite: String,
    /// Selected rootfs features (empty for a plain base image).
    pub features: Vec<String>,
    /// Image layout (`combined` / `split`). On `split` the boot payload and the root
    /// filesystem live on *different media*, so a reader that finds no bootloader beside
    /// this rootfs is looking at an expected state, not a fault.
    pub layout: String,
    /// Image hostname.
    pub hostname: String,
}

/// The kernel the image boots, and the fact that decides how a new one reaches it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct IdentityKernel {
    /// Kernel definition id.
    pub id: String,
    /// `mainline`, `vendor`, or `distro-package` — and the reason this section exists.
    /// It is what tells an outside tool how a kernel upgrade gets here: a distro kernel
    /// arrives through `apt`, a compiled one is a `.deb` that somebody has to hand it.
    pub flavor: String,
    /// The kernel package a distro-package build installs. Absent for a compiled kernel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub package: Option<String>,
    /// The pinned kernel ref. Absent for a distro-package kernel, which is not fetched
    /// from git at all.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    /// The exact kernel commit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// The patch profiles applied to that kernel, in order. They are the difference
    /// between two boards running the same kernel version and having different hardware
    /// working, so they belong on the device rather than only in the build's records.
    /// Empty when the kernel applied no series.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub patch_profiles: Vec<String>,
}

/// The resolved build point (from [`ResolvedBuild`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ImageProvenance {
    /// Device name.
    pub device: String,
    /// Human-readable board description.
    pub description: String,
    /// Target architecture.
    pub arch: String,
    /// Target SoC.
    pub soc: String,
    /// Selected boot method.
    pub boot_method: String,
    /// The depthcharge board profile the kernel partition was signed for, when the
    /// boot method has one. It records *which firmware* this image targets — a stock
    /// C201 and a libreboot'd one take different profiles — which is not otherwise
    /// recoverable from the image. Absent under a boot method with no board profile.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub board: Option<String>,
    /// Debian suite.
    pub suite: String,
    /// Selected rootfs features (empty for a plain base image).
    pub features: Vec<String>,
    /// Image layout (`combined` / `split`).
    pub layout: String,
    /// Image size (authored string).
    pub image_size: String,
    /// Image hostname.
    pub hostname: String,
    /// The `LANG` the image boots with.
    pub locale: String,
    /// Every locale compiled into the image, so a reader can tell — without booting it
    /// — which locales this image can be switched to with no network.
    pub locales_generate: Vec<String>,
    /// The `tzdata` zone the image's `/etc/localtime` points at.
    pub timezone: String,
    /// The console keyboard layout, when the board has a keyboard. Absent on a
    /// headless board, which ships Debian's default rather than a configured one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub keymap: Option<String>,
}

/// Every pinned source, as `ref` + exact `commit` pairs (from the [`Lock`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourcesProvenance {
    /// Kernel definition id.
    pub kernel_id: String,
    /// How the kernel was obtained: `mainline`, `vendor`, or `distro-package`. It is
    /// what tells a reader whether to expect a commit below or a package.
    pub kernel_flavor: String,
    /// Kernel ref that was pinned. Absent — with
    /// [`kernel_commit`](Self::kernel_commit) — for a distro-package kernel, which is
    /// not fetched from git at all: its exact version and hash are pinned in the
    /// solved package manifest, like every other package in the image.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kernel_ref: Option<String>,
    /// Kernel commit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kernel_commit: Option<String>,
    /// The kernel package a distro-package build installs (`linux-image-armmp`).
    /// Absent for a compiled kernel.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kernel_package: Option<String>,
    /// Patch profile names, in order. Empty — along with
    /// [`patches_commit`](Self::patches_commit) being absent — when the kernel applied
    /// no series, so the record never implies a `patches` dependency the build did not
    /// have.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub patch_profiles: Vec<String>,
    /// `patches` repo commit the series is pinned at.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patches_commit: Option<String>,
    /// u-boot ref. Absent — with [`uboot_commit`](Self::uboot_commit) — when the boot
    /// method compiles no u-boot.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uboot_ref: Option<String>,
    /// u-boot commit.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uboot_commit: Option<String>,
    /// The media-accel source pins, present only when the image built the HW
    /// transcode stack (a `requires_media_accel` feature was selected). Omitted
    /// from the manifest for a base image, which has no such sources.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub media_accel: Option<MediaAccelProvenance>,
}

/// The pinned media-accel source trees — the MPP/RGA/Mali userspace forks plus
/// the ffmpeg V4L2 base and its Rockchip graft-provenance tree — as `ref` +
/// exact `commit` pairs (from the [`Lock`]). Present in a [`SourcesProvenance`]
/// only when the image compiled the transcode stack.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct MediaAccelProvenance {
    /// MPP ref. Absent when the SoC declares no such tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mpp_ref: Option<String>,
    /// MPP commit. Absent when the SoC declares no such tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mpp_commit: Option<String>,
    /// librga ref. Absent when the SoC declares no such tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub librga_ref: Option<String>,
    /// librga commit. Absent when the SoC declares no such tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub librga_commit: Option<String>,
    /// libmali ref. Absent when the SoC declares no such tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub libmali_ref: Option<String>,
    /// libmali commit. Absent when the SoC declares no such tree.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub libmali_commit: Option<String>,
    /// ffmpeg V4L2-base ref.
    pub ffmpeg_base_ref: String,
    /// ffmpeg V4L2-base commit.
    pub ffmpeg_base_commit: String,
    /// ffmpeg Rockchip provenance-tree ref (graft source). Absent when no graft applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ffmpeg_rockchip_ref: Option<String>,
    /// ffmpeg Rockchip provenance-tree commit. Absent when no graft applies.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ffmpeg_rockchip_commit: Option<String>,
}

/// The rootfs suite plus the content-pinned solved-manifest reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RootfsProvenance {
    /// Debian suite.
    pub suite: String,
    /// Solved-manifest filename (committed beside the lock).
    pub manifest: String,
    /// sha256 of that manifest file — the same value the lock pins.
    pub manifest_sha256: String,
    /// Number of installed packages the manifest pins.
    pub package_count: usize,
}

/// The rootfs filesystem's on-disk contract: the exact ext4 feature words, block
/// size, and inode size the rootfs was formatted with.
///
/// Every other pin in this manifest answers "which sources went in." This one answers
/// "what shape were they written into," and it is the only such determinant that moves
/// independently of the lock: the feature set is a builder constant chosen by the image
/// stage, not a value resolved from config, so a formatter whose baseline set gains a
/// feature relays a different on-disk layout for an unchanged lock. Recording the
/// *resolved* words makes that a visible difference between two builds rather than a
/// silent one.
///
/// Both spellings are kept, and neither is redundant. [`features`](Self::features)
/// is readable and diffable by a human; the three raw words pin exactly, including a
/// bit whose name the formatter has changed or that it does not name at all.
/// [`block_size`](Self::block_size) and [`inode_size`](Self::inode_size) are not
/// features and appear in no feature word, but change the layout comprehensively — a
/// record of the feature words alone would not be a pin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct FilesystemProvenance {
    /// The filesystem type the words below describe (`ext4`).
    pub kind: String,
    /// On-disk feature names, in the formatter's canonical order: `compat`, then
    /// `incompat`, then `ro_compat`, each ascending by bit.
    pub features: Vec<String>,
    /// The raw `compat` feature word, `0x`-prefixed 8-digit hex. Hex rather than a
    /// decimal integer because it is a bit set, and only hex diffs one bit at a time.
    pub compat: String,
    /// The raw `incompat` feature word, in the same form as [`compat`](Self::compat).
    pub incompat: String,
    /// The raw `ro_compat` feature word, in the same form as [`compat`](Self::compat).
    pub ro_compat: String,
    /// Filesystem block size in bytes.
    pub block_size: u32,
    /// Inode size in bytes (`s_inode_size`).
    pub inode_size: u16,
}

/// Verified rkbin blob pins (`"<filename>@sha256:<hex>"`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BlobsProvenance {
    /// ATF/BL31 blob pin.
    pub atf: String,
    /// DDR TPL blob pin.
    pub tpl: String,
    /// OP-TEE BL32 blob pin, present only when the build has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bl32: Option<String>,
}

/// Build host / toolchain identity — the toolchain *selection* (host+target arch
/// and the cross prefix). Capturing concrete compiler/assembler versions is a
/// follow-up; the selection is what is deterministically known here.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ToolchainProvenance {
    /// Build-host architecture.
    pub host_arch: String,
    /// Target architecture.
    pub target_arch: String,
    /// Whether the build was cross-arch.
    pub cross: bool,
    /// `CROSS_COMPILE` prefix (empty on a native build).
    pub cross_compile: String,
}

/// Which boot2deb built the image — the builder axis of "exactly what went into this
/// image". It is an *as-built* record, not a requirement: the exact version reproduces
/// the image, and later versions do too until some change alters the build output for
/// this lock — a boundary that cannot be known at build time. So it records *when the
/// build worked*, never a forward compatibility range, and a reproduce flow reads it to
/// advise (warn on a mismatch), never to enforce.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BuiltWithProvenance {
    /// boot2deb crate version, from `Cargo.toml` (e.g. `0.1.0`).
    pub version: String,
    /// Short git commit of the boot2deb checkout that built the image. Absent when the
    /// build tree was not a git checkout, leaving `version` the only builder coordinate.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// Whether the boot2deb checkout had uncommitted changes at build time. `true` means
    /// `commit` alone does not identify the builder — the image is not reproducible from
    /// that commit.
    pub dirty: bool,
}

/// The image's initial first-boot credential.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct CredentialsProvenance {
    /// Default account name.
    pub user: String,
    /// The per-image password.
    pub password: String,
    /// How the credential behaves on the shipped image.
    pub note: String,
}

/// Assemble the image's on-device [`SystemIdentity`] from the resolved build and its
/// lock. Pure — no I/O.
///
/// Unlike [`assemble`] this takes no [`BuildFacts`], and that is what makes the document
/// possible at all: every value here is known *before* the rootfs is bootstrapped, so it
/// can be staged into the rootfs it describes. The provenance manifest cannot be — its
/// solved-manifest digest and per-image password are both produced by the bootstrap it
/// would have to be written into.
pub fn system_identity(build: &ResolvedBuild, lock: &Lock) -> SystemIdentity {
    let kernel = build.kernel.as_ref().expect(IMAGE_ONLY);
    SystemIdentity {
        version: IDENTITY_VERSION,
        image: IdentityImage {
            device: build.device.clone(),
            description: build.description.clone(),
            arch: build.arch.to_string(),
            soc: build.soc.to_string(),
            boot_method: build.boot_method.to_string(),
            board: build.depthcharge_boot().map(|b| b.board.clone()),
            suite: build.suite.clone().expect(IMAGE_ONLY),
            features: build.features.clone(),
            layout: build.layout.to_string(),
            hostname: build.hostname.clone(),
        },
        kernel: IdentityKernel {
            // From the resolved build, so they are recorded even for a kernel the lock
            // pins no commit for.
            id: kernel.id().to_string(),
            flavor: kernel.flavor().to_string(),
            package: match kernel {
                crate::model::ResolvedKernel::Distro(k) => Some(k.package.clone()),
                crate::model::ResolvedKernel::Compiled(_) => None,
            },
            reference: lock.kernel.as_ref().map(|k| k.reference.clone()),
            commit: lock.kernel.as_ref().map(|k| k.commit.clone()),
            patch_profiles: lock
                .patches
                .as_ref()
                .map(|p| p.profiles.clone())
                .unwrap_or_default(),
        },
    }
}

/// Provenance and the system-identity file are emitted by the image node, which
/// runs only for an image build — so the kernel, suite, and rootfs pin are present.
const IMAGE_ONLY: &str =
    "provenance is emitted only for an image build (kernel, suite, and rootfs pin present)";

impl SystemIdentity {
    /// Serialize to the canonical form: the banner followed by the TOML body.
    pub fn to_toml_string(&self) -> Result<String, crate::ConfigError> {
        let body = toml::to_string(self).map_err(|source| crate::ConfigError::Serialize {
            what: "image identity",
            source,
        })?;
        Ok(format!("{IDENTITY_BANNER}{body}"))
    }
}

/// Join a resolved build, its lock, and the engine's build-time facts into a
/// [`ProvenanceManifest`]. Pure — no I/O — so the join is unit-testable.
pub fn assemble(build: &ResolvedBuild, lock: &Lock, facts: &BuildFacts) -> ProvenanceManifest {
    let kernel = build.kernel.as_ref().expect(IMAGE_ONLY);
    let rootfs = lock.rootfs.as_ref().expect(IMAGE_ONLY);
    ProvenanceManifest {
        image: ImageProvenance {
            device: build.device.clone(),
            description: build.description.clone(),
            arch: build.arch.to_string(),
            soc: build.soc.to_string(),
            boot_method: build.boot_method.to_string(),
            board: build.depthcharge_boot().map(|b| b.board.clone()),
            suite: build.suite.clone().expect(IMAGE_ONLY),
            features: build.features.clone(),
            layout: build.layout.to_string(),
            image_size: build.image_size.clone(),
            hostname: build.hostname.clone(),
            locale: build.locale.clone(),
            locales_generate: build.locales_generate.clone(),
            timezone: build.timezone.clone(),
            // The layout alone identifies the keymap for a reader; the XKB model,
            // variant, and options are build inputs, recoverable from the config.
            keymap: build.keymap.as_ref().map(|k| k.layout.clone()),
        },
        sources: SourcesProvenance {
            // The id and flavor come from the resolved build, so they are recorded
            // even for a kernel the lock pins no commit for.
            kernel_id: kernel.id().to_string(),
            kernel_flavor: kernel.flavor().to_string(),
            kernel_ref: lock.kernel.as_ref().map(|k| k.reference.clone()),
            kernel_commit: lock.kernel.as_ref().map(|k| k.commit.clone()),
            kernel_package: match kernel {
                crate::model::ResolvedKernel::Distro(k) => Some(k.package.clone()),
                crate::model::ResolvedKernel::Compiled(_) => None,
            },
            patch_profiles: lock
                .patches
                .as_ref()
                .map(|p| p.profiles.clone())
                .unwrap_or_default(),
            patches_commit: lock.patches.as_ref().map(|p| p.commit.clone()),
            uboot_ref: lock.uboot.as_ref().map(|u| u.reference.clone()),
            uboot_commit: lock.uboot.as_ref().map(|u| u.commit.clone()),
            // Present in lockstep: resolution pins userspace and ffmpeg together or
            // not at all, so a single `zip` yields the whole block or `None`.
            media_accel: lock
                .userspace
                .as_ref()
                .zip(lock.ffmpeg.as_ref())
                .map(|(us, ff)| MediaAccelProvenance {
                    mpp_ref: us.mpp.as_ref().map(|p| p.reference.clone()),
                    mpp_commit: us.mpp.as_ref().map(|p| p.commit.clone()),
                    librga_ref: us.librga.as_ref().map(|p| p.reference.clone()),
                    librga_commit: us.librga.as_ref().map(|p| p.commit.clone()),
                    libmali_ref: us.libmali.as_ref().map(|p| p.reference.clone()),
                    libmali_commit: us.libmali.as_ref().map(|p| p.commit.clone()),
                    ffmpeg_base_ref: ff.base.reference.clone(),
                    ffmpeg_base_commit: ff.base.commit.clone(),
                    ffmpeg_rockchip_ref: ff.rockchip.as_ref().map(|p| p.reference.clone()),
                    ffmpeg_rockchip_commit: ff.rockchip.as_ref().map(|p| p.commit.clone()),
                }),
        },
        rootfs: RootfsProvenance {
            suite: rootfs.suite.clone(),
            manifest: rootfs.manifest.clone(),
            manifest_sha256: facts.manifest_sha256.to_string(),
            package_count: facts.package_count,
        },
        filesystem: facts.filesystem.clone(),
        blobs: lock.blobs.as_ref().map(|b| BlobsProvenance {
            atf: b.atf.clone(),
            tpl: b.tpl.clone(),
            bl32: b.bl32.clone(),
        }),
        toolchain: ToolchainProvenance {
            host_arch: facts.host_arch.to_string(),
            target_arch: build.arch.to_string(),
            cross: facts.cross,
            cross_compile: build.cross_compile.clone(),
        },
        built_with: BuiltWithProvenance {
            version: facts.builder_version.to_string(),
            commit: facts.builder_commit.map(str::to_string),
            dirty: facts.builder_dirty,
        },
        credentials: CredentialsProvenance {
            user: facts.user.to_string(),
            password: facts.password.to_string(),
            note: "expired at first login (passwd -e); unique per built image".to_string(),
        },
        // The one fact the engine hands over as a unit, split here because its two
        // halves sit on opposite sides of the table/array-of-tables boundary.
        sandbox_env: facts.sandbox.env.clone(),
        extra_debs: lock.extra_debs.clone(),
        // Every source axis the build actually *fetches*, classified offline by pin
        // form. A source the build never fetches has no re-fetch durability to report,
        // so it contributes no row: a distro-package kernel and a boot method with no
        // u-boot both drop out here, as does the ffmpeg `rockchip` pin (provenance
        // only — the graft ships as patches, so that tree is never cloned).
        source_durability: source_durability_rows(lock),
        sandbox_mounts: facts.sandbox.mounts.clone(),
    }
}

/// The `[[source_durability]]` rows for a lock — one per source the build fetches
/// from git: the kernel and u-boot when they are compiled, plus the four media-accel
/// trees (mpp/librga/libmali/ffmpeg-base) when the transcode stack is built.
fn source_durability_rows(lock: &Lock) -> Vec<SourceDurability> {
    let mut rows = Vec::new();
    if let Some(k) = &lock.kernel {
        rows.push(source_durability("kernel", &k.reference, &k.commit));
    }
    if let Some(u) = &lock.uboot {
        rows.push(source_durability("uboot", &u.reference, &u.commit));
    }
    if let Some(us) = &lock.userspace {
        // Only the trees the SoC declares: a row for an absent tree would claim a
        // fetch this build never makes.
        for (name, pin) in [
            ("mpp", &us.mpp),
            ("librga", &us.librga),
            ("libmali", &us.libmali),
        ] {
            if let Some(p) = pin {
                rows.push(source_durability(name, &p.reference, &p.commit));
            }
        }
    }
    if let Some(ff) = &lock.ffmpeg {
        rows.push(source_durability(
            "ffmpeg-base",
            &ff.base.reference,
            &ff.base.commit,
        ));
    }
    // Each out-of-tree kernel module is fetched from its own pinned repo, so its pin has
    // the same re-fetch durability question as any other source.
    for kmod in &lock.kmods {
        rows.push(source_durability(
            &format!("kmod:{}", kmod.name),
            &kmod.reference,
            &kmod.commit,
        ));
    }
    rows
}

/// Classify one source pin's offline durability form for the manifest.
fn source_durability(source: &str, reference: &str, commit: &str) -> SourceDurability {
    SourceDurability {
        source: source.to_string(),
        reference: reference.to_string(),
        form: crate::sources::PinForm::classify(reference, commit)
            .as_str()
            .to_string(),
    }
}

impl ProvenanceManifest {
    /// Serialize to the canonical form: the sensitivity banner followed by the
    /// TOML body.
    pub fn to_toml_string(&self) -> Result<String, crate::ConfigError> {
        let body = toml::to_string(self).map_err(|source| crate::ConfigError::Serialize {
            what: "provenance manifest",
            source,
        })?;
        Ok(format!("{BANNER}{body}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::*;

    fn sample_lock() -> Lock {
        let git = |r: &str, c: &str| GitPin {
            source: "s".into(),
            reference: r.into(),
            commit: c.into(),
        };
        Lock {
            kernel: Some(KernelPin {
                id: "rk3588-mainline-7.1".into(),
                source: "ks".into(),
                reference: "v7.1.1".into(),
                commit: "kc".into(),
            }),
            patches: Some(PatchesPin {
                profiles: vec!["rk3588-accel".into()],
                source: "https://example.invalid/patches.git".into(),
                reference: "main".into(),
                commit: "pc".into(),
            }),
            uboot: Some(UbootPin {
                source: "us".into(),
                reference: "v2026.04".into(),
                commit: "uc".into(),
            }),
            uboot_patches: None,
            userspace: Some(UserspacePins {
                mpp: Some(git("mainline-cma-fix", "mc")),
                librga: Some(git("master", "rc")),
                libmali: Some(git("master", "lc")),
            }),
            ffmpeg: Some(FfmpegPins {
                base: git("v4l2-request-n8.1", "fbc"),
                rockchip: Some(git("8.1", "frc")),
            }),
            rootfs: Some(RootfsPin {
                suite: "forky".into(),
                manifest: "turing-rk1-media-accel-forky.pkgs.lock".into(),
                manifest_sha256: Some("mh".into()),
            }),
            blobs: Some(BlobsPin {
                atf: "atf@sha256:0".into(),
                tpl: "tpl@sha256:1".into(),
                bl32: None,
            }),
            kmods: vec![],
            extra_debs: vec![],
            snapshot: None,
        }
    }

    /// The filesystem pin as the image stage resolves it — the engine's real values, so
    /// the join is exercised against the shape that actually ships.
    fn sample_filesystem() -> FilesystemProvenance {
        FilesystemProvenance {
            kind: "ext4".into(),
            features: ["has_journal", "extent", "metadata_csum_seed"]
                .iter()
                .map(|s| (*s).to_string())
                .collect(),
            compat: "0x0000003c".into(),
            incompat: "0x000022c2".into(),
            ro_compat: "0x0000046b".into(),
            block_size: 4096,
            inode_size: 256,
        }
    }

    /// The sandbox profile as the engine resolves it, trimmed to the entries that
    /// exercise every optional field — a bind with a host source, a symlink whose
    /// source is its content, a flagged tmpfs with a data string, and a mount with
    /// neither. The real profile is longer; the shape is the same.
    fn sample_sandbox() -> SandboxProvenance {
        let mount = |kind: &str, target: &str| SandboxMount {
            kind: kind.into(),
            target: target.into(),
            source: None,
            fstype: None,
            flags: None,
            options: None,
            read_only: None,
        };
        SandboxProvenance {
            env: [
                ("PATH", "/usr/sbin:/usr/bin:/sbin:/bin"),
                ("HOME", "/root"),
                ("LC_ALL", "C.UTF-8"),
                ("TZ", "UTC"),
                ("DEBIAN_FRONTEND", "noninteractive"),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
            mounts: vec![
                mount("procfs", "/proc"),
                SandboxMount {
                    flags: Some("0x00000002".into()),
                    options: Some("mode=0755".into()),
                    ..mount("tmpfs", "/dev")
                },
                SandboxMount {
                    source: Some("/dev/null".into()),
                    read_only: Some(false),
                    ..mount("bind", "/dev/null")
                },
                SandboxMount {
                    source: Some("pts/ptmx".into()),
                    ..mount("symlink", "/dev/ptmx")
                },
            ],
        }
    }

    fn config_root() -> crate::ConfigRoot {
        crate::ConfigRoot::new(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(2)
                .unwrap()
                .to_path_buf(),
        )
    }

    fn sample_build() -> ResolvedBuild {
        // A resolution over the shipped config gives a real build point to join.
        crate::resolve_recipe(
            &config_root(),
            "turing-rk1/media-accel-forky",
            &crate::Overrides::default(),
        )
        .unwrap()
    }

    /// A depthcharge build — the boot method that *has* a board profile.
    fn depthcharge_build() -> ResolvedBuild {
        crate::resolve_recipe(
            &config_root(),
            "asus-c201/forky",
            &crate::Overrides::default(),
        )
        .unwrap()
    }

    /// The identity document ships **inside** the image, so the one thing it must never
    /// carry is the one thing the provenance manifest exists to record: the per-image
    /// first-boot password. The two documents are assembled from overlapping inputs, so
    /// this asserts the boundary rather than trusting it — a field added to
    /// `SystemIdentity` by copying a line from `assemble` would fail here.
    #[test]
    fn the_on_device_identity_carries_no_secret() {
        let lock = sample_lock();
        let text = system_identity(&sample_build(), &lock)
            .to_toml_string()
            .unwrap();
        // The banner *documents* that the file carries no secret, so it says the words.
        // What must not contain them is the data.
        let body: String = text
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect();

        // The password `assemble` would have put in the manifest.
        assert!(
            !body.contains("Kp7rTx"),
            "identity leaked the first-boot password:\n{text}"
        );
        for forbidden in ["password", "credentials", "shadow", "secret"] {
            assert!(
                !body.contains(forbidden),
                "identity data contains `{forbidden}`, which must not ship inside an image:\n{text}"
            );
        }
        // And it is genuinely a subset — the manifest *does* carry the secret, so the
        // two documents are being compared, not two spellings of the same thing.
        let facts = BuildFacts {
            host_arch: "x86_64",
            cross: true,
            manifest_sha256: "abc",
            package_count: 1,
            user: "debian",
            password: "Kp7rTx",
            builder_version: "0.0.0-test",
            builder_commit: Some("deadbeef1234"),
            builder_dirty: false,
            filesystem: sample_filesystem(),
            sandbox: sample_sandbox(),
        };
        let manifest = assemble(&sample_build(), &lock, &facts)
            .to_toml_string()
            .unwrap();
        assert!(
            manifest.contains("Kp7rTx"),
            "the manifest is the document that has it"
        );
    }

    /// The board profile is the reason the file exists: it is not recoverable from the
    /// disk, and `depthchargectl` otherwise reads it off the *running* board's HWID —
    /// which a tool running somewhere else cannot do.
    #[test]
    fn the_identity_records_the_depthcharge_board_and_omits_it_otherwise() {
        let lock = sample_lock();

        let dc = system_identity(&depthcharge_build(), &lock);
        assert_eq!(dc.image.boot_method, "depthcharge");
        assert_eq!(dc.image.board.as_deref(), Some("speedy"));
        assert!(dc.to_toml_string().unwrap().contains("board = \"speedy\""));

        // A boot method with no board profile records none, rather than an empty string
        // a reader would have to special-case.
        let rk = system_identity(&sample_build(), &lock);
        assert_eq!(rk.image.boot_method, "rockchip-rkbin");
        assert_eq!(rk.image.board, None);
        assert!(!rk.to_toml_string().unwrap().contains("board"));
    }

    /// The document is a wire format read by independently-versioned programs, so the
    /// schema version must be present, must serialize ahead of every table (TOML rejects
    /// a top-level scalar after one), and the whole thing must re-parse.
    #[test]
    fn the_identity_is_a_versioned_parseable_document() {
        let text = system_identity(&depthcharge_build(), &sample_lock())
            .to_toml_string()
            .unwrap();
        assert!(text.starts_with("# boot2deb image identity"));

        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(parsed["version"].as_integer(), Some(1));
        assert_eq!(parsed["image"]["device"].as_str(), Some("asus-c201"));
        assert_eq!(parsed["image"]["layout"].as_str(), Some("combined"));

        // A distro kernel names its package and pins no commit; that pairing is what
        // tells a reader an upgrade arrives via apt rather than a hand-placed .deb.
        assert_eq!(parsed["kernel"]["flavor"].as_str(), Some("distro-package"));
        assert_eq!(
            parsed["kernel"]["package"].as_str(),
            Some("linux-image-armmp")
        );

        // `version` must precede `[image]` in the serialized text, not merely exist.
        let v = text.find("version = 1").expect("version scalar");
        assert!(v < text.find("[image]").expect("image table"));
    }

    #[test]
    fn assembles_and_serializes_to_toml() {
        let build = sample_build();
        let lock = sample_lock();
        let facts = BuildFacts {
            host_arch: "x86_64",
            cross: true,
            manifest_sha256: "abc123",
            package_count: 223,
            user: "debian",
            password: "Kp7rTx",
            builder_version: "0.0.0-test",
            builder_commit: Some("cafef00dbabe"),
            builder_dirty: false,
            filesystem: sample_filesystem(),
            sandbox: sample_sandbox(),
        };
        let prov = assemble(&build, &lock, &facts);
        assert_eq!(prov.sources.kernel_commit.as_deref(), Some("kc"));
        assert_eq!(prov.sources.kernel_flavor, "mainline");
        let media = prov
            .sources
            .media_accel
            .as_ref()
            .expect("media-accel build has sources");
        assert_eq!(media.ffmpeg_rockchip_ref.as_deref(), Some("8.1"));
        assert_eq!(prov.rootfs.manifest_sha256, "abc123");
        assert_eq!(prov.rootfs.package_count, 223);
        assert_eq!(prov.toolchain.host_arch, "x86_64");
        assert!(prov.toolchain.cross);
        assert_eq!(prov.credentials.password, "Kp7rTx");
        // Per-source durability form is recorded offline: the sample pins all
        // use named refs (a ref that is not the bare commit), so every one is named-ref.
        assert_eq!(prov.source_durability.len(), 6);
        assert!(prov
            .source_durability
            .iter()
            .any(|s| s.source == "mpp" && s.form == "named-ref"));

        let text = prov.to_toml_string().unwrap();
        assert!(text.starts_with("# boot2deb provenance manifest"));
        // Every section is present and the join carried the pins through.
        for needle in [
            "[image]",
            "[sources]",
            "[rootfs]",
            "[filesystem]",
            "[blobs]",
            "[toolchain]",
            "[built_with]",
            "[credentials]",
            "kernel_commit = \"kc\"",
            "manifest_sha256 = \"abc123\"",
            "password = \"Kp7rTx\"",
            "version = \"0.0.0-test\"",
        ] {
            assert!(
                text.contains(needle),
                "provenance TOML missing {needle}:\n{text}"
            );
        }
        // The emitted document is valid TOML (guards the section field ordering —
        // a scalar after a nested table would be a parse error).
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(
            parsed["sources"]["media_accel"]["ffmpeg_base_commit"].as_str(),
            Some("fbc")
        );
        assert_eq!(
            parsed["image"]["features"][0].as_str(),
            Some("media-accel-rockchip")
        );
        // The builder stamp: an as-built record of which boot2deb produced the image.
        assert_eq!(parsed["built_with"]["version"].as_str(), Some("0.0.0-test"));
        assert_eq!(
            parsed["built_with"]["commit"].as_str(),
            Some("cafef00dbabe")
        );
        assert_eq!(parsed["built_with"]["dirty"].as_bool(), Some(false));
        // The filesystem pin: the on-disk contract the rootfs was formatted to, which no
        // source pin covers. Both spellings survive the join and the TOML round-trip —
        // the raw words as hex strings (a bit set, not a quantity) and the readable names.
        assert_eq!(parsed["filesystem"]["kind"].as_str(), Some("ext4"));
        assert_eq!(parsed["filesystem"]["compat"].as_str(), Some("0x0000003c"));
        assert_eq!(
            parsed["filesystem"]["incompat"].as_str(),
            Some("0x000022c2")
        );
        assert_eq!(
            parsed["filesystem"]["ro_compat"].as_str(),
            Some("0x0000046b")
        );
        assert_eq!(parsed["filesystem"]["block_size"].as_integer(), Some(4096));
        assert_eq!(parsed["filesystem"]["inode_size"].as_integer(), Some(256));
        assert_eq!(
            parsed["filesystem"]["features"][0].as_str(),
            Some("has_journal")
        );
        // No extra_debs in this build → the array-of-tables is omitted entirely.
        assert!(!text.contains("extra_debs"));
    }

    #[test]
    fn extra_debs_are_joined_and_serialize_after_the_tables() {
        let build = sample_build();
        let mut lock = sample_lock();
        lock.extra_debs = vec![crate::model::ExtraDeb {
            url: Some("https://vendor.example/x_1_arm64.deb".into()),
            path: None,
            sha256: "aa".repeat(32), // a well-formed 64-char hex pin
        }];
        let facts = BuildFacts {
            host_arch: "x86_64",
            cross: true,
            manifest_sha256: "abc",
            package_count: 1,
            user: "debian",
            password: "pw",
            builder_version: "0.0.0-test",
            builder_commit: None,
            builder_dirty: false,
            filesystem: sample_filesystem(),
            sandbox: sample_sandbox(),
        };
        let prov = assemble(&build, &lock, &facts);
        assert_eq!(prov.extra_debs.len(), 1);
        let text = prov.to_toml_string().unwrap();
        // Both arrays-of-tables serialize, and the whole document is still valid TOML
        // (the trailing `[[extra_debs]]` / `[[source_durability]]` sections do not
        // swallow the preceding `[credentials]` table).
        assert!(text.contains("[[extra_debs]]"));
        assert!(text.contains("[[source_durability]]"));
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(parsed["credentials"]["user"].as_str(), Some("debian"));
        assert_eq!(
            parsed["extra_debs"][0]["sha256"].as_str().unwrap().len(),
            64
        );
        // The durability list carries every fetched source axis.
        assert_eq!(parsed["source_durability"].as_array().unwrap().len(), 6);
        // A build outside a git checkout stamps its version but omits the commit
        // entirely (not an empty string a reader would have to special-case).
        assert_eq!(parsed["built_with"]["version"].as_str(), Some("0.0.0-test"));
        assert!(
            parsed["built_with"].get("commit").is_none(),
            "no commit for a non-git build"
        );
    }

    /// The sandbox profile is one fact split across the table/array-of-tables boundary,
    /// so both halves have to land, in the right order, with every mount kind's optional
    /// fields present or absent as the kind dictates.
    #[test]
    fn the_sandbox_profile_is_recorded_on_both_sides_of_the_table_boundary() {
        let facts = BuildFacts {
            host_arch: "x86_64",
            cross: true,
            manifest_sha256: "abc",
            package_count: 1,
            user: "debian",
            password: "pw",
            builder_version: "0.0.0-test",
            builder_commit: None,
            builder_dirty: false,
            filesystem: sample_filesystem(),
            sandbox: sample_sandbox(),
        };
        let text = assemble(&sample_build(), &sample_lock(), &facts)
            .to_toml_string()
            .unwrap();

        // `[sandbox_env]` is a table, so it must precede every array-of-tables or the
        // rows that follow would land inside it.
        let env_table = text.find("[sandbox_env]").expect("the env table");
        assert!(env_table < text.find("[[").expect("an array-of-tables"));

        let parsed: toml::Value = toml::from_str(&text).unwrap();
        // The environment is the whole of what a compile sees, declared by boot2deb
        // rather than composed over the sandbox library's base.
        let env = parsed["sandbox_env"].as_table().unwrap();
        assert_eq!(env.len(), 5);
        assert_eq!(env["LC_ALL"].as_str(), Some("C.UTF-8"));
        assert_eq!(env["PATH"].as_str(), Some("/usr/sbin:/usr/bin:/sbin:/bin"));

        // The mounts keep the order the sandbox establishes them in — a set would lose
        // the fact that `/dev` is a tmpfs the device nodes are then bound into.
        let mounts = parsed["sandbox_mounts"].as_array().unwrap();
        assert_eq!(mounts.len(), 4);
        assert_eq!(mounts[0]["kind"].as_str(), Some("procfs"));
        assert_eq!(mounts[1]["target"].as_str(), Some("/dev"));
        assert_eq!(mounts[1]["flags"].as_str(), Some("0x00000002"));
        assert_eq!(mounts[1]["options"].as_str(), Some("mode=0755"));
        // A bind names its host source and its read-only posture; a symlink names what
        // it points at and has no posture at all.
        assert_eq!(mounts[2]["source"].as_str(), Some("/dev/null"));
        assert_eq!(mounts[2]["read_only"].as_bool(), Some(false));
        assert_eq!(mounts[3]["source"].as_str(), Some("pts/ptmx"));
        assert!(
            mounts[3].get("read_only").is_none(),
            "a symlink is not remounted"
        );
        // A field the kind does not have is absent, not empty — so a reader never has to
        // tell "no flags" apart from "flags 0".
        assert!(
            mounts[0].get("flags").is_none(),
            "procfs passes no flag word"
        );
        assert!(
            mounts[0].get("options").is_none(),
            "procfs passes no data string"
        );
    }
}
