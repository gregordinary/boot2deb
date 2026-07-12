//! Typed config model — hardware layers, kernel definitions, recipes, and the
//! resolved build.
//!
//! Every type here is deserialized from a TOML config layer and validated at
//! load, so a malformed or incomplete config is a typed error *before* any build
//! work starts. The axis enums ([`Arch`], [`Soc`], [`BootMethod`]) are
//! Rust enums rather than strings so the compiler enforces completeness as new
//! targets are added.

use crate::error::ConfigError;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::str::FromStr;

/// Instruction-set architecture of a target.
///
/// Serialized in kebab-case (`arm64`, `armv7`, `riscv64`), which is also the
/// stem of the file under `arches/`. New architectures are added as variants so
/// the compiler flags every match that must handle them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Arch {
    /// 64-bit Arm (AArch64) — the RK35xx family.
    Arm64,
    /// 32-bit Arm v7 — e.g. RK3288.
    Armv7,
    /// 64-bit RISC-V — e.g. Milk-V Mars.
    Riscv64,
}

impl Arch {
    /// The Debian architecture name for this ISA — what `dpkg`, `mmdebstrap`, and
    /// deb `Architecture:` fields expect. This differs from [`as_str`](Arch::as_str)
    /// for 32-bit Arm, whose Debian architecture is `armhf` (hard-float), not the
    /// `armv7` ISA spelling used for the config file stem and kbuild `ARCH`.
    pub fn debian_arch(&self) -> &'static str {
        match self {
            Arch::Arm64 => "arm64",
            Arch::Armv7 => "armhf",
            Arch::Riscv64 => "riscv64",
        }
    }
}

/// System-on-chip. Selects the shared SoC layer (`socs/<soc>.toml`) that in turn
/// names the [`Arch`], device-tree directory, and accel module list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Soc {
    /// Rockchip RK3588 (RK1's SoC).
    Rk3588,
    /// Rockchip RK3576.
    Rk3576,
    /// Rockchip RK3566.
    Rk3566,
    /// Rockchip RK3288 (armv7).
    Rk3288,
}

/// How the bootloader is produced and written to the medium. Owns the raw-gap
/// offsets and payload format; a device selects one from its
/// `supported_boot_methods`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BootMethod {
    /// Rockchip idbloader + `u-boot.itb` in the raw gap, with rkbin ATF/TPL.
    RockchipRkbin,
    /// ChromeOS depthcharge: a vboot-signed kernel partition (planned).
    Depthcharge,
    /// RISC-V OpenSBI (planned).
    Opensbi,
}

/// Image packaging topology.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Layout {
    /// One image: bootloader in the raw gap ahead of the rootfs partition.
    Combined,
    /// Two artifacts: a bootloader-only image plus a bootloader-agnostic rootfs
    /// image for a separate disk.
    Split,
}

/// Provenance of a kernel tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KernelFlavor {
    /// Upstream/mainline (or `linux-stable`); patched by a profile.
    Mainline,
    /// Vendor / out-of-tree BSP tree, typically shipped pre-patched.
    Vendor,
}

/// Implements `as_str` / [`Display`](fmt::Display) / [`FromStr`] for a config
/// enum, keeping the wire string, the file stem, and error/parsing text in a
/// single source of truth alongside the serde `rename_all`.
macro_rules! kebab_enum {
    ($ty:ty { $($variant:ident => $s:literal),+ $(,)? }) => {
        impl $ty {
            /// The canonical kebab-case string — matches the serialized TOML
            /// value and the config file stem (e.g. `arches/arm64.toml`).
            pub fn as_str(&self) -> &'static str {
                match self { $(<$ty>::$variant => $s),+ }
            }
            /// Every variant, in declaration order — the closed set of valid
            /// values for this axis. Drives discovery (listing) and the
            /// `new-device` scaffold's menus, which offer exactly these.
            pub fn all() -> &'static [$ty] {
                &[$(<$ty>::$variant),+]
            }
        }
        impl fmt::Display for $ty {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }
        impl FromStr for $ty {
            type Err = String;
            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s { $($s => Ok(<$ty>::$variant),)+
                    other => Err(format!("unknown {}: '{}'", stringify!($ty), other)) }
            }
        }
    };
}

kebab_enum!(Arch { Arm64 => "arm64", Armv7 => "armv7", Riscv64 => "riscv64" });
kebab_enum!(Soc { Rk3588 => "rk3588", Rk3576 => "rk3576", Rk3566 => "rk3566", Rk3288 => "rk3288" });
kebab_enum!(BootMethod {
    RockchipRkbin => "rockchip-rkbin", Depthcharge => "depthcharge", Opensbi => "opensbi" });
kebab_enum!(Layout { Combined => "combined", Split => "split" });
kebab_enum!(KernelFlavor { Mainline => "mainline", Vendor => "vendor" });

// ---------------------------------------------------------------------------
// Hardware layers
// ---------------------------------------------------------------------------

/// Invariants shared by every target of one [`Arch`] (`arches/<arch>.toml`).
///
/// These are toolchain/kbuild facts that never vary within an architecture, so
/// they live once at the arch layer rather than being repeated per device.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ArchLayer {
    /// `ARCH=` passed to kbuild (e.g. `arm64`).
    pub kernel_arch: String,
    /// `ARCH=` for the u-boot build (RK3588 builds u-boot as `arm`).
    pub uboot_arch: String,
    /// `KBUILD_IMAGE` — the built kernel image path within the tree.
    pub kbuild_image: String,
    /// `CROSS_COMPILE` prefix, used only when the host arch differs from the
    /// target; ignored on native builds.
    pub cross_compile: String,
}

/// An explicit git source: a clone URL plus a default ref (branch/tag/commit),
/// resolved to an exact commit in the lock. Used for the media-accel userspace
/// and ffmpeg trees, which are always concrete forks (unlike a kernel, which may
/// use a named-tree indirection — see [`KernelSource`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GitSource {
    /// Clone URL.
    pub git: String,
    /// Default branch/tag/commit constraint; the exact commit is pinned in the
    /// lock (`ref` in TOML).
    #[serde(rename = "ref")]
    pub git_ref: String,
}

/// The media-accel userspace source trees — the MPP/RGA/Mali forks whose
/// `.deb`s the userspace build node produces. Shared across the RK35xx
/// family, so they live at the SoC layer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserspaceSources {
    /// Rockchip Media Process Platform (`librockchip-mpp1`).
    pub mpp: GitSource,
    /// Rockchip 2D raster graphics accelerator library (`librga2`).
    pub librga: GitSource,
    /// Mali GPU userspace blob (built only when requested; unused on a headless
    /// transcode box, where the pipeline rides the VPU + RGA, not the GPU).
    pub libmali: GitSource,
}

/// The ffmpeg source pair: a mainline V4L2-stateless decode base with the
/// Rockchip rkmpp-encode / rkrga-filter graft applied on top.
///
/// The graft is intentional: decode stays on the mainline V4L2 path from `base`,
/// while only the encode + scale commits are taken from `rockchip` — `rockchip`'s
/// own (vendor-MPP) decode path is *not* wanted, as mainline lacks its HAL. The
/// graft is materialized as an ordered `git am` series in the profile's `ffmpeg`
/// scope: one graft commit (the RKMPP hwcontext) needs a 3-way conflict
/// resolution that a plain cherry-pick cannot reproduce, so the resolved commits
/// are shipped as patches. `rockchip` records the provenance tree those patches
/// were derived from; the build fetches only `base` and applies the series.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FfmpegSources {
    /// Base tree carrying mainline V4L2-request stateless decode.
    pub base: GitSource,
    /// Rockchip rkmpp encoder + rkrga filter tree the graft patches were derived
    /// from — provenance, pinned in the lock; not fetched at build time.
    pub rockchip: GitSource,
}

/// SoC-level invariants shared across every board using one [`Soc`]
/// (`socs/<soc>.toml`).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SocLayer {
    /// Human-readable SoC name.
    pub description: String,
    /// The architecture this SoC implements.
    pub arch: Arch,
    /// Device-tree subdirectory under `arch/<arch>/boot/dts/` (e.g. `rockchip`).
    pub dt_dir: String,
    /// rkbin blob defaults shared by boards on this SoC: the SoC-generic ATF and a
    /// common-memory DDR TPL, plus BL32 where the boot chain needs OP-TEE. A device
    /// inherits these and overrides per field (typically just the TPL for different
    /// DRAM); resolution requires the merged `atf` and `tpl` to be present. §3.6.
    #[serde(default)]
    pub rkbin: RkbinLayer,
    /// Accel/media modules force-loaded at boot via `/etc/modules-load.d/`, so
    /// they are present on first boot even where device-tree auto-probe would
    /// otherwise be enough.
    pub modules: Vec<String>,
    /// SoC-specific rootfs packages added to the base set; empty for the
    /// RK1, whose accel userspace ships via features, not the SoC layer.
    #[serde(default)]
    pub packages: Vec<String>,
    /// Packages this SoC layer drops from the merged rootfs set — the
    /// scoped subtraction a pure package union cannot express. Unioned with every
    /// other layer's `exclude`; any name in that union is removed from the include
    /// set (exclude wins). Empty for the RK1.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Pre-built `.deb`s this SoC layer pulls from outside the Debian mirror;
    /// empty for the RK1, whose accel userspace builds from source.
    #[serde(default)]
    pub extra_debs: Vec<ExtraDeb>,
    /// Media-accel userspace source trees (MPP/RGA/Mali), common to the RK35xx
    /// family. Optional: a SoC that never builds the HW transcode stack omits
    /// them, and resolution rejects a build that selects a `requires_media_accel`
    /// feature (e.g. `media-accel-rockchip`) on a SoC that does. Present alongside
    /// [`ffmpeg`](Self::ffmpeg) — the media-accel stack is built as a unit.
    #[serde(default)]
    pub userspace: Option<UserspaceSources>,
    /// ffmpeg source pair (V4L2 base + Rockchip rkmpp/rkrga), common to RK35xx.
    /// Optional under the same contract as [`userspace`](Self::userspace).
    #[serde(default)]
    pub ffmpeg: Option<FfmpegSources>,
}

/// Bootloader-method invariants (`boot-methods/<method>.toml`): where the u-boot
/// source comes from and the raw offsets its payloads are written to.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BootMethodLayer {
    /// Human-readable description.
    pub description: String,
    /// Upstream u-boot git URL.
    pub uboot_source: String,
    /// Default u-boot ref (a constraint; the exact commit is pinned in the lock).
    pub uboot_ref: String,
    /// Byte offset of `idbloader.img` in the raw gap (authored string, e.g.
    /// `32KiB`).
    pub idbloader_offset: String,
    /// Byte offset of `u-boot.itb` in the raw gap (e.g. `8MiB`).
    pub uboot_itb_offset: String,
    /// Start offset of the rootfs partition (e.g. `16MiB`).
    pub rootfs_offset: String,
    /// Boot-method-specific rootfs packages added to the base set; empty
    /// for `rockchip-rkbin`, whose boot wiring is overlay files, not packages.
    #[serde(default)]
    pub packages: Vec<String>,
    /// Packages this boot method drops from the merged rootfs set, unioned
    /// with every other layer's `exclude` (exclude wins). Empty for
    /// `rockchip-rkbin`.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Pre-built `.deb`s this boot method pulls from outside the Debian mirror;
    /// empty for `rockchip-rkbin`.
    #[serde(default)]
    pub extra_debs: Vec<ExtraDeb>,
}

/// A third-party (non-Debian-mirror) apt repository a feature adds to the rootfs
/// solve. An application whose package is not in Debian — Jellyfin, Plex,
/// Docker — ships from its own signed apt repo; a feature declares that repo here
/// so apt can *resolve* the app and its dependencies during the bootstrap solve,
/// rather than a post-install `dpkg -i` that resolves nothing.
///
/// Fields mirror a deb822 `.sources` stanza. `signed_by` names the repository's
/// signing keyring (a filename resolved against the build host's vendored keyring
/// set, the same convention as the Debian archive keyring) — an unsigned
/// third-party repo is not accepted, since the local repo the engine assembles is
/// the trust boundary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct AptSource {
    /// Stable identifier for this source (the `.sources` file stem and the dedup
    /// key when features are unioned). Two features naming the same `name` with
    /// differing definitions is a resolution error.
    pub name: String,
    /// Repository base URL (deb822 `URIs`), e.g. `https://repo.jellyfin.org/debian`.
    pub uri: String,
    /// Distribution/suite (deb822 `Suites`), e.g. the Debian codename the vendor
    /// keys their pockets on.
    pub suite: String,
    /// Components (deb822 `Components`), e.g. `["main"]`.
    pub components: Vec<String>,
    /// Signing keyring filename (deb822 `Signed-By`), resolved against the build
    /// host's vendored keyrings. Mandatory — the repo is verified, not trusted
    /// blindly.
    pub signed_by: String,
}

/// A pre-built `.deb` a layer or feature pulls in from outside the Debian mirror
/// — a vendor download or a file on disk — content-pinned by its
/// mandatory sha256.
///
/// Exactly one locator is set: `url` (fetched over HTTP(S)) or `path` (a file
/// relative to the config root). The **sha256, not the locator, is the identity**
/// the build and the [signature](crate) key on: moving byte-identical
/// bytes is not a rebuild, while a URL that later serves different bytes is a
/// verification failure, not a silent swap. The pin gives *integrity*, not
/// *authenticity* — an arbitrary-HTTP deb carries no signed `Release` chain, so it
/// reaches the image only through the local apt repo the engine assembles, which is
/// its trust boundary, never a `dpkg -i`.
///
/// Declared on any hardware layer or feature; the union across all of them is
/// de-duplicated by sha256 at resolution ([`ResolvedBuild::extra_debs`]). The lock
/// records the same shape verbatim (the sha256 is already exact, so there is
/// nothing to resolve): `update` fetches every entry, verifies its bytes hash to
/// `sha256`, and copies them into the content store; `build` materializes from that
/// store — trusting only the locked hash, re-fetching only to fill a miss — and
/// drops the deb into the local apt repo before the rootfs solve.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExtraDeb {
    /// HTTP(S) source URL. Mutually exclusive with [`path`](Self::path).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// On-disk source path, resolved along the config search path (an overlay may
    /// ship the file). Mutually exclusive with [`url`](Self::url).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Mandatory content hash (lowercase hex sha256) — the pin the build trusts and
    /// the signature keys on. The shape is enforced by [`validate`](Self::validate)
    /// at resolution (and re-checked in the engine), not at the parse boundary, so
    /// the failure carries the typed [`ConfigError::ExtraDebBadHash`] context.
    pub sha256: String,
}

/// Where an [`ExtraDeb`]'s bytes come from — the validated single locator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExtraDebLocator<'a> {
    /// Fetched over HTTP(S) from this URL.
    Url(&'a str),
    /// Read from this path, resolved along the config search path.
    Path(&'a str),
}

impl ExtraDeb {
    /// The single validated locator, or [`ConfigError::ExtraDebLocator`] if not
    /// exactly one of `url`/`path` is set.
    pub fn locator(&self) -> Result<ExtraDebLocator<'_>, ConfigError> {
        match (self.url.as_deref(), self.path.as_deref()) {
            (Some(u), None) => Ok(ExtraDebLocator::Url(u)),
            (None, Some(p)) => Ok(ExtraDebLocator::Path(p)),
            _ => Err(ConfigError::ExtraDebLocator {
                sha256: self.sha256.clone(),
            }),
        }
    }

    /// Validate the entry: exactly one locator, a `path` locator that stays within
    /// the config root, and a 64-char lowercase-hex sha256 (the canonical form the
    /// content hash is compared against — an uppercase digit would spuriously
    /// mismatch). Called at resolution so a malformed pin fails before any
    /// build, and re-checked in the engine since a lock is hand-editable.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if let ExtraDebLocator::Path(rel) = self.locator()? {
            reject_unsafe_path(rel)?;
        }
        let ok = self.sha256.len() == 64
            && self.sha256.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if ok {
            Ok(())
        } else {
            Err(ConfigError::ExtraDebBadHash {
                value: self.sha256.clone(),
            })
        }
    }

    /// A short human label (the locator string) for build output and errors.
    pub fn locator_label(&self) -> String {
        match (&self.url, &self.path) {
            (Some(u), _) => u.clone(),
            (_, Some(p)) => p.clone(),
            _ => format!("extra_deb {}", self.sha256),
        }
    }
}

/// Reject a `path` locator that would escape the config root: an absolute path (or
/// one with a drive/root prefix) or one containing a `..` component. With neither,
/// `root.join(rel)` provably stays under `root`, so the deb is read from within a
/// config root as intended rather than an arbitrary host location.
fn reject_unsafe_path(rel: &str) -> Result<(), ConfigError> {
    use std::path::{Component, Path};
    let unsafe_component = Path::new(rel).components().any(|c| {
        matches!(c, Component::RootDir | Component::Prefix(_) | Component::ParentDir)
    });
    if unsafe_component {
        Err(ConfigError::ExtraDebUnsafePath { value: rel.to_string() })
    } else {
        Ok(())
    }
}

/// rkbin blob references as authored at a config layer (SoC or device). Every
/// field is optional so a layer states only its deltas: the SoC supplies the
/// defaults and a device overrides per field. Resolution merges SoC `(+)` device
/// (device wins per field) into a resolved [`Rkbin`], where `atf` and `tpl` are
/// required and `bl32` stays optional. §3.6.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RkbinLayer {
    /// ARM Trusted Firmware (BL31) ELF filename. SoC-generic, so it is normally
    /// set once at the SoC layer.
    #[serde(default)]
    pub atf: Option<String>,
    /// DDR init TPL filename. Board-memory-specific: the SoC layer supplies a
    /// common-memory default and a board with different DRAM overrides it here.
    #[serde(default)]
    pub tpl: Option<String>,
    /// OP-TEE secure-payload (BL32) filename. Set on SoCs whose u-boot expects
    /// OP-TEE (e.g. RK3576, which hangs after "Starting kernel" without it);
    /// omitted on BL31-only boots (RK3588/RK1).
    #[serde(default)]
    pub bl32: Option<String>,
}

/// The resolved rkbin blob set a Rockchip u-boot build consumes: ATF/BL31 and the
/// DDR TPL (both required — resolution guarantees them present) plus an optional
/// OP-TEE BL32. Referenced by filename here and verified by sha256 against the lock.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Rkbin {
    /// ARM Trusted Firmware (BL31) ELF filename.
    pub atf: String,
    /// DDR init TPL filename (board-memory-specific — a SoC default the device
    /// layer may override).
    pub tpl: String,
    /// OP-TEE secure-payload (BL32) filename when the boot chain needs one;
    /// `None` on BL31-only SoCs (RK3588/RK1), and then omitted from the serialized
    /// form.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bl32: Option<String>,
}

/// A device: hardware invariants plus the defaults that let `boot2deb build
/// <device>` resolve a complete build with no other input. A device
/// states only its deltas; everything else comes from its soc/arch/boot-method
/// layers.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceLayer {
    /// Human-readable board name.
    pub description: String,
    /// The SoC this board uses; resolves arch, DT dir, and module list.
    pub soc: Soc,
    /// Default boot method (must appear in `supported_boot_methods`).
    pub boot_method: BootMethod,
    /// Boot methods this board can use; an override must be one of these.
    pub supported_boot_methods: Vec<BootMethod>,
    /// u-boot defconfig for this board.
    pub uboot_defconfig: String,
    /// Board device-tree blob path, relative to the DT output dir.
    pub kernel_dtb: String,
    /// Device-tree sources for a board whose `.dts` is not yet in the kernel: the
    /// board `.dts` plus any board-specific `.dtsi` it includes, as paths relative
    /// to the config root (e.g. `devices/h96-max-m9/dts/rk3576-h96-max-m9.dts`),
    /// resolved along the overlay search path like a fragment or blob. The kernel
    /// stage copies them into the in-tree DT dir and teaches that dir's Makefile to
    /// build the DTB. Empty for a board whose DTB is already upstream, which is the
    /// case a plain mainline build already covers. Resolution requires
    /// [`kernel_dtb`](Self::kernel_dtb) to name one of these sources. §4.
    #[serde(default)]
    pub device_dts: Vec<String>,
    /// Board-specific kconfig fragments (board deltas only; SoC/accel fragments
    /// belong to the kernel definition).
    pub device_config_fragments: Vec<String>,
    /// Kernel definitions valid for this board; an override must be one of these.
    pub supported_kernels: Vec<String>,
    /// Kernel used when none is specified.
    pub default_kernel: String,
    /// Debian suite used when none is specified (RK1: `forky`).
    pub default_suite: String,
    /// Image layout used when none is specified.
    pub default_layout: Layout,
    /// Default image hostname.
    pub hostname: String,
    /// Default image size (authored string, e.g. `2G`).
    pub image_size: String,
    /// rkbin blob overrides for this board's memory configuration, merged over the
    /// SoC layer's defaults (device wins per field). A board on standard memory
    /// omits this block entirely and inherits the SoC's blobs; a board with
    /// different DRAM overrides `tpl`. §3.6.
    #[serde(default)]
    pub rkbin: RkbinLayer,
    /// Board-specific rootfs packages added to the base set; empty for the
    /// RK1.
    #[serde(default)]
    pub packages: Vec<String>,
    /// Packages this board drops from the merged rootfs set, unioned with
    /// every other layer's `exclude` (exclude wins). Empty for the RK1.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Pre-built `.deb`s this board pulls from outside the Debian mirror;
    /// empty for the RK1.
    #[serde(default)]
    pub extra_debs: Vec<ExtraDeb>,
}

/// The distro-generic rootfs substrate (`base.toml`): the base Debian
/// package set every image installs, plus packages excluded from the base
/// system. Layer- and feature-specific packages stack on top at resolution.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BaseLayer {
    /// Base Debian packages installed into every rootfs (the bootstrap
    /// `--include` set) — device- and feature-independent distro policy.
    #[serde(default)]
    pub packages: Vec<String>,
    /// Packages excluded from the base system, e.g. `isc-dhcp-client` where a
    /// lighter DHCP client is used instead. Unioned at resolution with the
    /// soc/boot-method/device/feature `exclude` sets into the bootstrap
    /// `--exclude` set; a name in that union is also dropped from the include set
    /// (exclude wins).
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Pre-built `.deb`s pulled from outside the Debian mirror, unioned
    /// across every layer + feature and de-duplicated by sha256. Empty for the
    /// distro-generic base.
    #[serde(default)]
    pub extra_debs: Vec<ExtraDeb>,
}

// ---------------------------------------------------------------------------
// Kernel layer
// ---------------------------------------------------------------------------

/// Where a kernel's source comes from.
///
/// A bare string is a well-known tree resolved to a URL by the engine (e.g.
/// `"linux-stable"`); a table is an explicit `{ git, ref }` for vendor /
/// out-of-tree trees. The TOML shape selects the variant (string → [`Named`],
/// table → [`Git`]).
///
/// [`Named`]: KernelSource::Named
/// [`Git`]: KernelSource::Git
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(untagged)]
pub enum KernelSource {
    /// A well-known tree name the engine maps to a URL.
    Named(String),
    /// An explicit git source for a vendor/out-of-tree kernel.
    Git {
        /// Git clone URL.
        git: String,
        /// Branch, tag, or commit.
        #[serde(rename = "ref")]
        git_ref: String,
    },
}

// Manual `Deserialize` rather than `#[serde(untagged)]`: an untagged enum ignores
// unknown fields regardless of `deny_unknown_fields`, so `{ git, ref, branch }`
// would silently drop `branch`, and a non-matching table gives an opaque "did not
// match any variant" error. Dispatching on the TOML value kind — string → `Named`,
// table → a `deny_unknown_fields` helper — makes both cases a precise error.
impl<'de> Deserialize<'de> for KernelSource {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct SourceVisitor;

        impl<'de> serde::de::Visitor<'de> for SourceVisitor {
            type Value = KernelSource;

            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                f.write_str("a kernel source: a tree-name string or a { git, ref } table")
            }

            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<KernelSource, E> {
                Ok(KernelSource::Named(v.to_string()))
            }

            fn visit_map<A>(self, map: A) -> Result<KernelSource, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                /// The explicit-git table shape; `deny_unknown_fields` so a stray
                /// key (e.g. `branch`) is a hard error, not a silent drop.
                #[derive(Deserialize)]
                #[serde(deny_unknown_fields)]
                struct GitTable {
                    git: String,
                    #[serde(rename = "ref")]
                    git_ref: String,
                }
                let t = GitTable::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                Ok(KernelSource::Git {
                    git: t.git,
                    git_ref: t.git_ref,
                })
            }
        }

        deserializer.deserialize_any(SourceVisitor)
    }
}

/// A kernel definition (`kernels/<id>.toml`): a versioned entity that owns
/// everything version-coupled — its source ref, base defconfig, config
/// fragments, and patch profile. Bumping a kernel means authoring a *new*
/// definition, never editing a device.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KernelDef {
    /// Mainline vs. vendor provenance.
    pub flavor: KernelFlavor,
    /// Where to fetch the tree.
    pub source: KernelSource,
    /// Branch/version track (e.g. `"7.1.y"`); resolved to an exact tag in the
    /// lock. Absent for vendor trees pinned directly by git ref.
    #[serde(default)]
    pub track: Option<String>,
    /// In-tree base defconfig that fragments merge onto.
    pub base_defconfig: String,
    /// Version-coupled kconfig fragments (SoC drivers + accel enables), in merge
    /// order.
    pub config_fragments: Vec<String>,
    /// Patch profile in the `patches` repo, or
    /// [`NO_PATCH_PROFILE`](crate::profile::NO_PATCH_PROFILE) (`"none"`) for a kernel
    /// that applies no series — a fully-upstream SoC or a pre-patched vendor tree.
    /// Resolution maps the sentinel to
    /// [`ResolvedKernel::patch_profile`]`= None`.
    pub patch_profile: String,
    /// Clone URL of the `patches` repo the profile lives in. Used to
    /// auto-fetch the series at the lock-pinned commit when no local checkout is
    /// present — the North-Star "selecting a device auto-fetches the right
    /// patches." Optional: a kernel with no patch profile omits it,
    /// and an explicit `--patches-path`/`--patches-url` overrides it.
    #[serde(default)]
    pub patches_url: Option<String>,
    /// SoCs this kernel supports; resolution rejects a mismatched device.
    pub supported_socs: Vec<Soc>,
}

// ---------------------------------------------------------------------------
// Recipe
// ---------------------------------------------------------------------------

/// A recipe (`recipes/<name>.toml`): a named, buildable point across the
/// device, kernel, suite, features, and image-knob axes. Holds *constraints*;
/// the exact resolution is written to the sibling lock. Every axis but `device`
/// is optional and falls back to the device default.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    /// The device this recipe builds.
    pub device: String,
    /// Kernel override; `None` → device `default_kernel`.
    #[serde(default)]
    pub kernel: Option<String>,
    /// Suite override; `None` → device `default_suite`.
    #[serde(default)]
    pub suite: Option<String>,
    /// Composable rootfs features — add-in module names; empty (or
    /// omitted) means a plain base image, merged onto the layered substrate at
    /// resolution.
    #[serde(default)]
    pub features: Vec<String>,
    /// Layout override; `None` → device `default_layout`.
    #[serde(default)]
    pub layout: Option<Layout>,
    /// Image-size override; `None` → device `image_size`.
    #[serde(default)]
    pub image_size: Option<String>,
}

/// Per-axis overrides applied during resolution.
///
/// Populated from CLI flags, and also used internally to fold a recipe's fields
/// in (CLI flag wins over recipe value, which wins over device default). A field
/// left `None` defers to the layer below it.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    /// Override the kernel definition id.
    pub kernel: Option<String>,
    /// Override the Debian suite.
    pub suite: Option<String>,
    /// Override the image layout.
    pub layout: Option<Layout>,
    /// Override the boot method (must be in the device's supported set).
    pub boot_method: Option<BootMethod>,
    /// Override the feature set (`None` defers to the recipe; `Some` replaces it).
    pub features: Option<Vec<String>>,
    /// Override the image size.
    pub image_size: Option<String>,
}

// ---------------------------------------------------------------------------
// Resolved build (output of resolution)
// ---------------------------------------------------------------------------

/// The kernel axis of a [`ResolvedBuild`]: a [`KernelDef`] flattened with its
/// merged fragment list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedKernel {
    /// Kernel definition id (e.g. `rk3588-mainline-7.1`).
    pub id: String,
    /// Mainline vs. vendor.
    pub flavor: KernelFlavor,
    /// Source tree.
    pub source: KernelSource,
    /// Version track, if any.
    pub track: Option<String>,
    /// In-tree base defconfig.
    pub base_defconfig: String,
    /// Patch profile name, or `None` when this kernel applies no series (the
    /// authored [`NO_PATCH_PROFILE`](crate::profile::NO_PATCH_PROFILE) sentinel). A
    /// `None` profile means the build never reads the `patches` repo: no checkout is
    /// resolved, no series is applied, and the lock records no `[patches]` table.
    pub patch_profile: Option<String>,
    /// Clone URL of the `patches` repo, for auto-fetching the series at the
    /// lock-pinned commit when no local checkout is present. `None` when
    /// [`patch_profile`](Self::patch_profile) is `None` (nothing to fetch).
    pub patches_url: Option<String>,
    /// Kernel-owned fragments followed by device fragments, in apply order.
    pub config_fragments: Vec<String>,
}

/// Raw-gap layout offsets for a build, carried as authored strings (parsed to
/// bytes only when the image is written).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Offsets {
    /// `idbloader.img` offset.
    pub idbloader: String,
    /// `u-boot.itb` offset.
    pub uboot_itb: String,
    /// Rootfs partition start.
    pub rootfs: String,
}

/// A complete, validated build point — the single input the engine consumes.
///
/// Produced by [`resolve_device`](crate::resolve::resolve_device) /
/// [`resolve_recipe`](crate::resolve::resolve_recipe): every axis is chosen and
/// every referenced layer merged, so the engine never re-reads config. Fields
/// with no default (e.g. blobs) are guaranteed present because resolution
/// validated them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedBuild {
    /// Device name that was resolved.
    pub device: String,
    /// Device description.
    pub description: String,
    /// Target architecture (from the SoC layer).
    pub arch: Arch,
    /// Target SoC.
    pub soc: Soc,
    /// Selected boot method.
    pub boot_method: BootMethod,
    /// Resolved kernel axis.
    pub kernel: ResolvedKernel,
    /// Debian suite.
    pub suite: String,
    /// Composable rootfs features, in recipe order; empty means a plain
    /// base image. Validated at resolution: each is known, compatible with the
    /// resolved SoC, and non-conflicting.
    pub features: Vec<String>,
    /// The merged rootfs package set: base ∪ soc ∪ boot-method ∪ device ∪
    /// Σ features, de-duplicated with order preserved (base first), then with every
    /// name in [`rootfs_exclude`](Self::rootfs_exclude) removed (exclude wins).
    /// Installed into the rootfs from the local apt repo plus the suite mirror,
    /// which resolves each package's dependencies.
    pub rootfs_packages: Vec<String>,
    /// The union of every layer's and feature's `exclude`, de-duplicated,
    /// passed as the rootfs bootstrap's `--exclude` set. Reconciled against the
    /// include set: no name appears in both [`rootfs_packages`](Self::rootfs_packages)
    /// and here.
    pub rootfs_exclude: Vec<String>,
    /// Image layout.
    pub layout: Layout,
    /// Image size.
    pub image_size: String,
    /// Image hostname.
    pub hostname: String,
    /// u-boot defconfig (from the device).
    pub uboot_defconfig: String,
    /// u-boot git source (from the boot method).
    pub uboot_source: String,
    /// u-boot ref constraint (from the boot method).
    pub uboot_ref: String,
    /// Board DTB path, relative to the DT output dir.
    pub kernel_dtb: String,
    /// Config-root-relative device-tree sources the kernel stage copies into the
    /// in-tree DT dir before `make` (from the device). Empty when the board's DTB is
    /// already upstream. Resolution guarantees each path is contained (relative, no
    /// `..`), names a `.dts`/`.dtsi`, and that [`kernel_dtb`](Self::kernel_dtb) is
    /// compiled from one of them. §4.
    pub device_dts: Vec<String>,
    /// Device-tree subdirectory (from the SoC).
    pub dt_dir: String,
    /// Force-loaded accel modules (from the SoC).
    pub modules: Vec<String>,
    /// `ARCH=` for kbuild (from the arch).
    pub kernel_arch: String,
    /// `ARCH=` for u-boot (from the arch).
    pub uboot_arch: String,
    /// `CROSS_COMPILE` prefix (from the arch; used only when cross-building).
    pub cross_compile: String,
    /// `KBUILD_IMAGE` path (from the arch).
    pub kbuild_image: String,
    /// rkbin blob set.
    pub rkbin: Rkbin,
    /// Raw-gap offsets (from the boot method).
    pub offsets: Offsets,
    /// Media-accel userspace source trees (from the SoC layer). `Some` iff this
    /// build compiles the HW transcode stack — i.e. a selected feature declares
    /// [`requires_media_accel`](crate::feature::Feature::requires_media_accel);
    /// resolution guarantees the SoC provides the sources in that case. `None` for
    /// a base build, and the userspace/ffmpeg compile + plan nodes are then skipped.
    pub userspace: Option<UserspaceSources>,
    /// ffmpeg source pair (from the SoC layer). `Some`/`None` in lockstep with
    /// [`userspace`](Self::userspace) — the media-accel stack is built as a unit.
    pub ffmpeg: Option<FfmpegSources>,
    /// Third-party apt repositories the selected features contribute,
    /// unioned across features and de-duplicated by `name`. The rootfs bootstrap
    /// activates these before the package solve so an out-of-mirror app (e.g.
    /// Jellyfin) resolves; empty when no feature adds one.
    pub apt_sources: Vec<AptSource>,
    /// Pre-built `.deb`s the layers and features pull from outside the Debian
    /// mirror, unioned and de-duplicated by sha256 (the content
    /// identity), first-appearance order. `update` fetches + content-pins these into
    /// the lock; `build` materializes them into the local apt repo before the
    /// solve. Empty when no layer or feature adds one.
    pub extra_debs: Vec<ExtraDeb>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debian_arch_maps_armv7_to_armhf() {
        // Debian's 32-bit Arm architecture is `armhf`, not the ISA spelling.
        assert_eq!(Arch::Armv7.as_str(), "armv7");
        assert_eq!(Arch::Armv7.debian_arch(), "armhf");
        // The others match their ISA name.
        assert_eq!(Arch::Arm64.debian_arch(), "arm64");
        assert_eq!(Arch::Riscv64.debian_arch(), "riscv64");
    }

    #[test]
    fn kernel_source_string_is_named() {
        let s: KernelSource = toml::from_str("source = \"linux-stable\"")
            .map(|t: toml::Table| t["source"].clone().try_into().unwrap())
            .unwrap();
        assert_eq!(s, KernelSource::Named("linux-stable".into()));
    }

    #[test]
    fn kernel_source_table_is_git() {
        let toml = "git = \"https://example/linux.git\"\nref = \"v7.1.1\"\n";
        let s: KernelSource = toml::from_str(toml).unwrap();
        assert_eq!(
            s,
            KernelSource::Git {
                git: "https://example/linux.git".into(),
                git_ref: "v7.1.1".into(),
            }
        );
    }

    #[test]
    fn kernel_source_table_rejects_unknown_field() {
        // `branch` is not a valid key — a manual deny_unknown_fields catches it
        // rather than silently dropping it as `#[serde(untagged)]` would.
        let toml = "git = \"https://example/linux.git\"\nref = \"v7.1.1\"\nbranch = \"main\"\n";
        assert!(toml::from_str::<KernelSource>(toml).is_err());
    }

    #[test]
    fn extra_deb_validate_checks_locator_and_hash() {
        let hex64 = "a".repeat(64);
        // Exactly one locator + a 64-char lowercase-hex hash validates.
        let with_url = ExtraDeb {
            url: Some("https://x/a.deb".into()),
            path: None,
            sha256: hex64.clone(),
        };
        assert!(with_url.validate().is_ok());
        assert_eq!(with_url.locator().unwrap(), ExtraDebLocator::Url("https://x/a.deb"));
        let with_path = ExtraDeb {
            url: None,
            path: Some("vendor/a.deb".into()),
            sha256: hex64.clone(),
        };
        assert_eq!(with_path.locator().unwrap(), ExtraDebLocator::Path("vendor/a.deb"));

        // Neither / both locators is ExtraDebLocator.
        let neither = ExtraDeb { url: None, path: None, sha256: hex64.clone() };
        assert!(matches!(neither.validate(), Err(ConfigError::ExtraDebLocator { .. })));
        let both = ExtraDeb {
            url: Some("u".into()),
            path: Some("p".into()),
            sha256: hex64.clone(),
        };
        assert!(matches!(both.validate(), Err(ConfigError::ExtraDebLocator { .. })));

        // A malformed hash (wrong length, uppercase, non-hex) is ExtraDebBadHash —
        // uppercase would spuriously mismatch the lowercase content hash.
        for bad in ["", "abc", &"A".repeat(64), &"g".repeat(64)] {
            let d = ExtraDeb { url: None, path: Some("p".into()), sha256: bad.to_string() };
            assert!(
                matches!(d.validate(), Err(ConfigError::ExtraDebBadHash { .. })),
                "expected a bad-hash error for {bad:?}"
            );
        }

        // Unknown key rejected (deny_unknown_fields).
        assert!(toml::from_str::<ExtraDeb>("sha256 = \"x\"\nbogus = 1\n").is_err());
    }

    #[test]
    fn extra_deb_path_must_stay_within_config_root() {
        let hex64 = "a".repeat(64);
        let unsafe_deb = |p: &str| ExtraDeb {
            url: None,
            path: Some(p.into()),
            sha256: hex64.clone(),
        };
        // Absolute paths and `..` traversal escape the config root and are rejected
        // before any read — an out-of-root file is not a valid deb source.
        for bad in ["/etc/passwd", "../../etc/passwd", "vendor/../../x.deb"] {
            assert!(
                matches!(unsafe_deb(bad).validate(), Err(ConfigError::ExtraDebUnsafePath { .. })),
                "expected an unsafe-path error for {bad:?}"
            );
        }
        // A plain nested relative path is contained and validates.
        assert!(unsafe_deb("vendor/sub/a.deb").validate().is_ok());
        // A `.` segment does not escape and is allowed.
        assert!(unsafe_deb("./vendor/a.deb").validate().is_ok());
    }
}
