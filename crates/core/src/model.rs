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

/// Console log level every build boots with: show `KERN_ERR` and worse, keep the
/// rest in the kernel ring buffer.
///
/// A console at the kernel's default level shows everything down to `KERN_INFO`,
/// which lets a single chatty driver print faster than a login can be typed — the
/// console becomes unusable exactly when a first boot needs it. Out-of-tree vendor
/// drivers are the usual source: a bare `printk()` carries no severity, so it lands
/// at `CONFIG_MESSAGE_LOGLEVEL_DEFAULT` (`KERN_WARNING`) no matter how trivial the
/// message, and such calls are often ungated by any of the driver's own debug knobs.
/// Gating the *console* bounds that noise for every driver at once, including the
/// ones whose verbosity nothing else can reach.
///
/// Nothing is lost: suppressed lines still reach the ring buffer and the journal, so
/// `dmesg` and `journalctl -k` show the full boot. A board that needs a louder
/// console appends its own `loglevel=` to
/// [`kernel_cmdline`](DeviceLayer::kernel_cmdline) — device arguments are appended
/// after this one and the kernel takes the last value.
pub const CONSOLE_LOGLEVEL_ARG: &str = "loglevel=4";

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
    /// The Debian architecture name for this ISA — what `dpkg`, the archive, and
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

    /// The `qemu-user` token for this ISA — the `<token>` in `qemu-<token>`, naming
    /// both the interpreter binary and its `binfmt_misc` handler.
    ///
    /// A property of the ISA rather than of any layer, hence here and not in config:
    /// qemu names 32-bit Arm `arm` and 64-bit Arm `aarch64`, matching neither
    /// [`as_str`](Arch::as_str) nor [`debian_arch`](Arch::debian_arch). On a cross host
    /// this interpreter executes every target binary a package build runs, so it is a
    /// build input as much as the compiler is.
    pub fn qemu_arch(&self) -> &'static str {
        match self {
            Arch::Arm64 => "aarch64",
            Arch::Armv7 => "arm",
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

/// How a board boots: what the boot payload is, where it is written, and what the
/// firmware expects to find. A device selects one from its
/// `supported_boot_methods`, and the method's [`BootMethodLayer`] variant owns the
/// details.
///
/// This is the closed set of *implemented* methods — every variant has a layer
/// struct and an engine path, so adding a board is adding config, and adding a boot
/// method is a variant plus its struct.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BootMethod {
    /// Rockchip idbloader + `u-boot.itb` in the raw gap, with rkbin ATF/TPL.
    RockchipRkbin,
    /// ChromeOS depthcharge: a vboot-signed kernel FIT in a ChromeOS kernel
    /// partition, selected by GPT attribute bits. The firmware is the board's own
    /// (coreboot in SPI flash), so nothing bootloader-shaped is built or written.
    Depthcharge,
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

/// Provenance of a kernel — and, since two of these are compiled from source and
/// one is not, which shape its definition takes ([`KernelDef`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum KernelFlavor {
    /// Upstream/mainline (or `linux-stable`); compiled from source, patched by a
    /// series.
    Mainline,
    /// Vendor / out-of-tree BSP tree; compiled from source, typically shipped
    /// pre-patched.
    Vendor,
    /// The distribution's own kernel package (`linux-image-armmp`), installed from
    /// the Debian mirror like any other package. Nothing is compiled, patched, or
    /// configured: there is no source ref, no defconfig, no fragments, and no patch
    /// series, and the exact version is pinned by name+version+sha256 in the rootfs
    /// package manifest rather than by a commit in the lock.
    DistroPackage,
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
kebab_enum!(BootMethod { RockchipRkbin => "rockchip-rkbin", Depthcharge => "depthcharge" });
kebab_enum!(Layout { Combined => "combined", Split => "split" });
kebab_enum!(KernelFlavor {
    Mainline => "mainline", Vendor => "vendor", DistroPackage => "distro-package" });

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
/// `.deb`s the userspace build node produces. They live at the SoC layer because
/// which of them exist is a property of the SoC.
///
/// Every tree is optional, and an absent one is a statement about the hardware, not
/// an omission: the SoC declares what it has, and the userspace node builds exactly
/// that. A part whose codec block has no mainline `mpp_service` declares no
/// [`mpp`](Self::mpp) and gets no `librockchip-mpp1`; a part whose GPU is driven by
/// mainline panfrost declares no [`libmali`](Self::libmali), because Mesa is its
/// userspace and comes from the Debian mirror. The set also decides ffmpeg's
/// `./configure` surface — see [`SocLayer::ffmpeg`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UserspaceSources {
    /// Rockchip Media Process Platform (`librockchip-mpp1`). Present only where the
    /// kernel offers the vendor `mpp_service` the library talks to; on a
    /// mainline-only part there is nothing for it to bind, so it is `None` and
    /// ffmpeg builds without `--enable-rkmpp`.
    #[serde(default)]
    pub mpp: Option<GitSource>,
    /// Rockchip 2D raster graphics accelerator library (`librga2`), speaking the
    /// vendor `/dev/rga` ABI.
    #[serde(default)]
    pub librga: Option<GitSource>,
    /// Mali GPU userspace blob. Present only for a GPU with no mainline driver — a
    /// CSF part such as the RK3588's G610. A Bifrost part driven by panfrost takes
    /// Mesa from the mirror instead and leaves this `None`.
    #[serde(default)]
    pub libmali: Option<GitSource>,
}

/// Default in-repo directory a [`KmodLayer`]'s quilt patches live in.
fn default_kmod_patch_dir() -> String {
    "debian/patches".to_string()
}

/// One out-of-tree kernel-module set, as authored in `kmods/<name>.toml`: a pinned git
/// repo, the in-repo subdirectory the `make M=` build runs in, the patches to apply
/// first, and the module objects to ship.
///
/// Its own config layer rather than a block on the device, because every field here is
/// a property of the *driver*, not of the board — two boards carrying the same chip name
/// the same kmod instead of copying its declaration. A device selects one by name
/// ([`device_kmods`](DeviceLayer::device_kmods)) and cannot override any field: the deb
/// is `<name>-modules-<kver>` and the artifact-cache node is `kmod:<name>`, so two boards
/// tuning one name differently would put different content behind the same key. A board
/// needing different `make_args` authors its own `kmods/<name>.toml`.
///
/// The kmod build node fetches the repo at the locked commit, applies
/// [`repo_patches`](Self::repo_patches) (from the repo's own quilt) then
/// [`local_patches`](Self::local_patches) (boot2deb-authored, e.g. a per-kernel compat
/// shim), builds the module against the freshly built kernel tree, and stages the
/// resulting `.ko`s into `/lib/modules/<kver>/updates/`. §4.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KmodLayer {
    /// One-line description, shown by `list-kmods` — what chip or subsystem the driver
    /// serves, since the name alone is usually a part number.
    pub description: String,
    /// Clone URL; the exact commit is pinned in the lock like a [`GitSource`].
    pub git: String,
    /// Default branch/tag/commit; the exact commit is pinned in the lock (`ref` in
    /// TOML).
    #[serde(rename = "ref")]
    pub git_ref: String,
    /// `make M=<subdir>` path, repo-root-relative (e.g.
    /// `src/SDIO/driver_fw/driver/aic8800`). Resolution guarantees it is relative and
    /// `..`-free.
    pub subdir: String,
    /// In-repo directory holding [`repo_patches`](Self::repo_patches) (default
    /// `debian/patches`).
    #[serde(default = "default_kmod_patch_dir")]
    pub patch_dir: String,
    /// Ordered subset of the *fetched repo's own* quilt under
    /// [`patch_dir`](Self::patch_dir) to `git apply -p1`, in apply order. Each is a bare
    /// filename (no path separator). Empty builds the repo as fetched.
    #[serde(default)]
    pub repo_patches: Vec<String>,
    /// Ordered boot2deb-authored patches applied *after* the repo's own, each a bare
    /// filename under `kmods/<name>/patches/` (resolved along the overlay search path
    /// like a fragment, so an out-of-tree overlay can replace a shim without forking the
    /// kmod). This is where a per-kernel compat shim we maintain — and intend to
    /// upstream — lives until the tracked fork carries it.
    #[serde(default)]
    pub local_patches: Vec<String>,
    /// Extra `make` variables for the module build, each a bare `KEY=VALUE`
    /// (e.g. `CONFIG_FDRV_NO_REG_SDIO=y` to select the direct-probe SDIO path, or
    /// `CONFIG_AIC8800_BTLPM_SUPPORT=n` to drop a module that needs no build). No
    /// whitespace or shell metacharacters — they are passed as argv, not a shell line.
    #[serde(default)]
    pub make_args: Vec<String>,
    /// `.ko` basenames to collect and ship (with or without the `.ko` suffix). Empty
    /// ships every `.ko` the build produces under [`subdir`](Self::subdir).
    #[serde(default)]
    pub modules: Vec<String>,
    /// Firmware shipped alongside the module, taken from the *same* pinned repo — so a
    /// driver and the exact firmware it expects move together on one pin. `None` for a
    /// module that needs no firmware. Packaged as a separate `<name>-firmware` deb
    /// (see [`KmodFirmware`]), never vendored into the config tree.
    #[serde(default)]
    pub firmware: Option<KmodFirmware>,
}

/// A [`KmodLayer`] with the name resolution took it from — the `kmods/<name>.toml` stem,
/// which is the whole identity of a kmod (the deb is `<name>-modules-<kver>`, the cache
/// node `kmod:<name>`, the lock pin keyed on it). Carrying the name only here, and not in
/// the authored layer, keeps a file and the thing it declares from disagreeing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedKmod {
    /// The `kmods/<name>.toml` stem. Resolution guarantees it is dpkg-package-name-safe
    /// (lowercase alphanumeric plus `-`/`+`/`.`, starting alphanumeric).
    pub name: String,
    /// One-line description of the driver, carried so a resolved build reads as what it
    /// builds rather than a list of part numbers.
    pub description: String,
    /// Clone URL; the exact commit is pinned in the lock.
    pub git: String,
    /// Default branch/tag/commit the lock pins a commit from.
    pub git_ref: String,
    /// `make M=<subdir>` path. Resolution guarantees it is relative and `..`-free.
    pub subdir: String,
    /// In-repo directory holding [`repo_patches`](Self::repo_patches). Resolution
    /// guarantees it is relative and `..`-free.
    pub patch_dir: String,
    /// The fetched repo's own quilt entries to apply, in order; each a bare filename.
    pub repo_patches: Vec<String>,
    /// boot2deb-authored patches applied after the repo's own, in order; each a bare
    /// filename the CLI resolves to `kmods/<name>/patches/<file>` along the search path.
    pub local_patches: Vec<String>,
    /// Extra `make` variables. Resolution guarantees each is a bare `KEY=VALUE` with no
    /// whitespace or shell metacharacters.
    pub make_args: Vec<String>,
    /// `.ko` basenames to ship; each a bare name. Empty ships everything built.
    pub modules: Vec<String>,
    /// Firmware from the same pin, or `None`. Resolution guarantees both its paths are
    /// relative and `..`-free.
    pub firmware: Option<KmodFirmware>,
}

/// Firmware a [`KmodLayer`] ships from its own pinned repo. Kept out of the per-kernel
/// modules deb deliberately: firmware is not kernel-version-specific, and two coexisting
/// kernels (A/B slots, an in-progress upgrade) would each own the same firmware path and
/// collide in dpkg. So it becomes its own `Architecture: all` `<name>-firmware` deb with
/// no kernel dependency, versioned by the driver commit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct KmodFirmware {
    /// Repo-root-relative directory whose files are shipped (e.g. a vendor `fw/`
    /// directory). Resolution guarantees it is relative and `..`-free. All regular files
    /// directly under it are staged; subdirectories are not descended.
    pub subdir: String,
    /// Rootfs-relative install directory the files land in (e.g.
    /// `usr/lib/firmware/aic8800_fw/SDIO/aic8800D80`) — the path the driver's compiled-in
    /// loader reads. Resolution guarantees it is relative and `..`-free.
    pub install: String,
}

/// The ffmpeg source pair: a mainline V4L2-stateless decode base with the
/// Rockchip rkmpp-encode / rkrga-filter graft applied on top.
///
/// The graft is intentional: decode stays on the mainline V4L2 path from `base`,
/// while only the encode + scale commits are taken from `rockchip` — `rockchip`'s
/// own (vendor-MPP) decode path is *not* wanted, as mainline lacks its HAL. The
/// graft is materialized as an ordered `git am` series in the series' `ffmpeg`
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
    /// from — provenance, pinned in the lock; not fetched at build time. `None` on a
    /// SoC whose ffmpeg is the base tree unmodified, which has no graft to attribute.
    #[serde(default)]
    pub rockchip: Option<GitSource>,
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

/// Bootloader-method invariants (`boot-methods/<method>.toml`), tagged per method.
///
/// Boot methods describe genuinely different things — one board's bootloader is a
/// pair of blobs we compile and write into a raw gap, another's is firmware in an
/// SPI chip that loads a signed kernel out of a GPT partition — so the layer is a
/// variant per [`BootMethod`] rather than one struct whose fields half apply.
///
/// The variant is chosen by the *filename*: [`ConfigRoot::boot_method`] is handed
/// the [`BootMethod`] and deserializes `boot-methods/<method>.toml` into that
/// method's struct. So each variant keeps `deny_unknown_fields` (a serde
/// internally-tagged enum would forfeit it) and an impossible layer — rkbin
/// offsets on a depthcharge board — cannot be authored at all.
///
/// [`ConfigRoot::boot_method`]: crate::loader::ConfigRoot::boot_method
#[derive(Debug, Clone)]
pub enum BootMethodLayer {
    /// Rockchip idbloader + `u-boot.itb` written into the raw gap, with rkbin
    /// ATF/TPL.
    RockchipRkbin(RockchipRkbinLayer),
    /// ChromeOS depthcharge: a vboot-signed FIT in a ChromeOS kernel partition.
    Depthcharge(DepthchargeLayer),
}

impl BootMethodLayer {
    /// Which method this layer describes.
    pub fn method(&self) -> BootMethod {
        match self {
            BootMethodLayer::RockchipRkbin(_) => BootMethod::RockchipRkbin,
            BootMethodLayer::Depthcharge(_) => BootMethod::Depthcharge,
        }
    }

    /// Human-readable description.
    pub fn description(&self) -> &str {
        match self {
            BootMethodLayer::RockchipRkbin(l) => &l.description,
            BootMethodLayer::Depthcharge(l) => &l.description,
        }
    }

    /// Rootfs packages this boot method's wiring needs (`depthcharge-tools` for
    /// the ChromeOS method; none for `rockchip-rkbin`, whose boot wiring is
    /// overlay files rather than packages).
    pub fn packages(&self) -> &[String] {
        match self {
            BootMethodLayer::RockchipRkbin(l) => &l.packages,
            BootMethodLayer::Depthcharge(l) => &l.packages,
        }
    }

    /// Packages this boot method drops from the merged rootfs set, unioned with
    /// every other layer's `exclude` (exclude wins).
    pub fn exclude(&self) -> &[String] {
        match self {
            BootMethodLayer::RockchipRkbin(l) => &l.exclude,
            BootMethodLayer::Depthcharge(l) => &l.exclude,
        }
    }

    /// Pre-built `.deb`s this boot method pulls from outside the Debian mirror.
    pub fn extra_debs(&self) -> &[ExtraDeb] {
        match self {
            BootMethodLayer::RockchipRkbin(l) => &l.extra_debs,
            BootMethodLayer::Depthcharge(l) => &l.extra_debs,
        }
    }
}

/// The `rockchip-rkbin` boot method: where the u-boot source comes from and the
/// raw offsets its payloads are written to.
///
/// The bootloader lives *outside* any filesystem, in the gap ahead of the rootfs
/// partition, so the offsets are the whole contract between the u-boot build and
/// the image node.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RockchipRkbinLayer {
    /// Human-readable description.
    pub description: String,
    /// Upstream u-boot git URL.
    pub uboot_source: String,
    /// Default u-boot ref (a constraint; the exact commit is pinned in the lock).
    pub uboot_ref: String,
    /// Clone URL of the `patches` repo the u-boot series is fetched from and pinned
    /// against, for a device that selects a real u-boot series. The boot method owns
    /// u-boot, so it owns u-boot's patch source — the counterpart of a kernel
    /// definition's [`patches_url`](CompiledKernelDef::patches_url). Required at
    /// resolution only when a device on this method selects a series other than
    /// [`NO_PATCH_SERIES`](crate::series::NO_PATCH_SERIES).
    #[serde(default)]
    pub patches_url: Option<String>,
    /// The `patches` ref the u-boot series is pinned at, defaulting to
    /// [`DEFAULT_PATCHES_REF`] when a series is
    /// selected and this is omitted.
    #[serde(default)]
    pub patches_ref: Option<String>,
    /// Byte offset of `idbloader.img` in the raw gap (authored string, e.g.
    /// `32KiB`).
    pub idbloader_offset: String,
    /// Byte offset of `u-boot.itb` in the raw gap (e.g. `8MiB`).
    pub uboot_itb_offset: String,
    /// Start offset of the rootfs partition (e.g. `16MiB`).
    pub rootfs_offset: String,
    /// Boot-method-specific rootfs packages added to the base set; empty here,
    /// since the boot wiring is overlay files, not packages.
    #[serde(default)]
    pub packages: Vec<String>,
    /// Packages this boot method drops from the merged rootfs set.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Pre-built `.deb`s this boot method pulls from outside the Debian mirror.
    #[serde(default)]
    pub extra_debs: Vec<ExtraDeb>,
}

/// The `depthcharge` boot method: a vboot-signed FIT written into a **ChromeOS
/// kernel partition**, which the board's firmware (coreboot + depthcharge, in an
/// SPI chip that is not ours to build) finds by GPT type GUID and selects by the
/// partition's attribute bits.
///
/// Nothing here is a bootloader we produce — the payload *is* the kernel. It is
/// built by `depthchargectl` **inside the rootfs**, so the same packaged hooks
/// re-sign and re-flash it when the kernel is upgraded on the running board, and
/// the image node only has to place the blob it produced.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DepthchargeLayer {
    /// Human-readable description.
    pub description: String,
    /// Byte offset of the **first** ChromeOS kernel slot (authored string, e.g.
    /// `12MiB`). The firmware scans every medium's GPT for the type GUID and never
    /// looks at a partition's number or start, so this is a free choice — but it
    /// must clear the 8 MiB region a Veyron eMMC reserves at its head, which
    /// `12MiB` does on eMMC and costs nothing on SD/USB.
    pub kpart_offset: String,
    /// Size of **each** ChromeOS kernel slot (e.g. `16MiB`). It bounds the signed
    /// payload the image can carry; the *firmware's* own ceiling is a property of
    /// the board profile and is enforced by `depthchargectl`.
    pub kpart_size: String,
    /// How many kernel slots the image lays down, back to back from
    /// [`kpart_offset`](Self::kpart_offset). Range
    /// 1..=[`MAX_KPART_SLOTS`](crate::chromeos::MAX_KPART_SLOTS).
    ///
    /// **Two is what makes a kernel upgrade survivable.** The first slot carries the
    /// signed payload; the rest ship empty at
    /// [`SPARE_KPART_FLAGS`](crate::chromeos::SPARE_KPART_FLAGS). An on-device
    /// upgrade then writes the *spare* and leaves the running kernel intact as a
    /// fallback the firmware returns to on its own if the new one does not boot. At
    /// one slot there is no spare, so `depthchargectl` overwrites the running kernel
    /// in place and a bad upgrade needs external media to recover. See
    /// [`chromeos`](crate::chromeos) for the protocol.
    pub kpart_slots: u8,
    /// Start offset of the rootfs partition (e.g. `44MiB`) — at or after the end of
    /// the **last** kernel slot.
    pub rootfs_offset: String,
    /// GPT attribute bits 51:48 — boot priority of the slot that ships the payload.
    /// 15 is highest; 0 means never boot. Range 0-15. Spare slots are not authored:
    /// they take [`SPARE_KPART_FLAGS`](crate::chromeos::SPARE_KPART_FLAGS).
    pub kpart_priority: u8,
    /// GPT attribute bits 55:52 — remaining boot attempts for the slot that ships
    /// the payload, decremented by the firmware on each attempt unless
    /// [`kpart_successful`](Self::kpart_successful) is set. Range 0-15.
    pub kpart_tries: u8,
    /// GPT attribute bit 56 — mark the shipped slot known-good, so the firmware
    /// stops decrementing `tries` and never gives up on it.
    pub kpart_successful: bool,
    /// Kernel command line baked into the signed FIT — **without** `root=`, which
    /// `depthchargectl` derives from the image's `/etc/fstab` and appends itself
    /// (it strips any `root=` that disagrees with fstab, and re-derives it on every
    /// on-device kernel upgrade).
    ///
    /// It must contain **no `%`**: `depthchargectl` round-trips the computed
    /// cmdline through a `ConfigParser` whose interpolation rejects a raw `%`. The
    /// `kern_guid=%U` the firmware substitutes is prepended later, by
    /// `mkdepthcharge`, past that round-trip.
    pub cmdline: String,
    /// Rootfs packages this boot method needs — `depthcharge-tools`, which both
    /// builds the signed payload at image time and re-signs it on the running board
    /// through its `/etc/kernel/postinst.d` hook.
    #[serde(default)]
    pub packages: Vec<String>,
    /// Packages this boot method drops from the merged rootfs set.
    #[serde(default)]
    pub exclude: Vec<String>,
    /// Pre-built `.deb`s this boot method pulls from outside the Debian mirror.
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
            && self
                .sha256
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
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
        matches!(
            c,
            Component::RootDir | Component::Prefix(_) | Component::ParentDir
        )
    });
    if unsafe_component {
        Err(ConfigError::ExtraDebUnsafePath {
            value: rel.to_string(),
        })
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

/// A device's depthcharge board-profile selection (`[depthcharge]` on the device
/// layer).
///
/// A *board profile* is `depthcharge-tools`' codename for a firmware behaviour set
/// — its payload ceiling, and whether the firmware loads a FIT ramdisk or needs the
/// initramfs address patched into every DTB's `/chosen`. It is a property of the
/// **firmware the unit runs**, not of the board model, which is why it is a
/// selectable axis rather than a constant: the same C201 has one profile on stock
/// firmware and another with libreboot installed.
///
/// The default is the *stock* profile, deliberately: a stock-profile image boots on
/// stock firmware **and** on a libreboot unit, while the reverse is not true.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceDepthcharge {
    /// Board profile used when `--board` is not given.
    pub board: String,
    /// Board profiles this device can use; a `--board` override must be one of
    /// these. Each is passed verbatim to `depthchargectl`, which resolves it against
    /// its own board database — the payload ceiling and DTB-patching policy live
    /// there and are deliberately not duplicated here.
    pub supported_boards: Vec<String>,
}

/// A device: hardware invariants plus the defaults that let `boot2deb build
/// <device>` resolve a complete build with no other input. A device
/// states only its deltas; everything else comes from its soc/arch/boot-method
/// layers.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DeviceLayer {
    /// Another device this one is a variant of, by name.
    ///
    /// A variant board — the same hardware with one block enabled, a different DTB, a
    /// different memory fitting — states only its deltas, exactly as a device states
    /// only its deltas from its soc/arch/boot-method layers. The parent's file is
    /// merged under this one with the same last-wins rules the overlay search path
    /// uses: tables merge key-by-key, and a scalar or array is replaced wholesale.
    /// Chains are walked to the base-most device, and a cycle is
    /// [`crate::error::ConfigError::DeviceExtendsCycle`].
    ///
    /// The parent's *assets* come too: its `overlay/` tree is laid in before this
    /// device's, so a variant inherits the parent's runtime config and can override
    /// any file of it. This is the half a hand-copied variant cannot express, and the
    /// reason a variant extends rather than duplicates — a board's driver tuning,
    /// services, and keymaps are as much a part of it as its TOML keys.
    #[serde(default)]
    pub extends: Option<String>,
    /// Human-readable board name.
    pub description: String,
    /// The SoC this board uses; resolves arch, DT dir, and module list.
    pub soc: Soc,
    /// Default boot method (must appear in `supported_boot_methods`).
    pub boot_method: BootMethod,
    /// Boot methods this board can use; an override must be one of these.
    pub supported_boot_methods: Vec<BootMethod>,
    /// u-boot defconfig for this board. Required by the `rockchip-rkbin` boot
    /// method, which compiles u-boot from source; absent on a board whose firmware
    /// is not ours to build (a depthcharge Chromebook boots coreboot out of an SPI
    /// chip). Resolution enforces it per method, so an omission is a typed error
    /// only where it matters.
    #[serde(default)]
    pub uboot_defconfig: Option<String>,
    /// Depthcharge board-profile selection. Required by the `depthcharge` boot
    /// method and absent otherwise. §3.2.
    #[serde(default)]
    pub depthcharge: Option<DeviceDepthcharge>,
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
    /// Extra kernel command-line arguments for this board, space-separated
    /// (e.g. a workaround that disables a broken output). Appended to the boot
    /// path's generated command line: the extlinux path ships them in
    /// `/etc/boot2deb/board.conf` (`EXTL_CMD_LINE`), the depthcharge path appends
    /// them to the boot method's signing cmdline. Base arguments stay generated —
    /// `root=` in particular is derived from `/etc/fstab` on device and is rejected
    /// here. Empty/absent means the generated command line stands alone.
    #[serde(default)]
    pub kernel_cmdline: Option<String>,
    /// Board-specific kconfig fragments (board deltas only; SoC/accel fragments
    /// belong to the kernel definition).
    pub device_config_fragments: Vec<String>,
    /// Board-specific kernel patch series, applied *after* the kernel definition's
    /// own [`patch_series`](CompiledKernelDef::patch_series) — the patch-series
    /// analogue of [`device_config_fragments`](Self::device_config_fragments). A driver
    /// only one board carries (an out-of-tree wireless part, say) lives here rather than
    /// on the SoC-wide kernel, so sibling boards on the same kernel never patch it in.
    /// The series come from the kernel's `patches_url` checkout like the kernel's own;
    /// a distro-package kernel compiles nothing, so naming any here is a typed error.
    /// Empty/absent for a board that adds no series of its own.
    #[serde(default)]
    pub device_patch_series: Vec<String>,
    /// Names of the out-of-tree kernel-module sets this board builds against its own
    /// kernel and stages into `/lib/modules/<kver>/updates/`, each resolved from
    /// `kmods/<name>.toml` ([`KmodLayer`]). Board opt-in like
    /// [`device_patch_series`](Self::device_patch_series) — a driver only one board
    /// carries never rides its siblings — while the driver's own declaration is shared,
    /// so a second board with the same chip names it rather than copying it. A
    /// distro-package kernel compiles nothing, so naming any here is a typed error.
    /// Empty/absent for a board that carries no out-of-tree module.
    ///
    /// Arrays replace wholesale across [`extends`](Self::extends), so a variant that
    /// wants its parent's drivers plus one more restates the whole list.
    #[serde(default)]
    pub device_kmods: Vec<String>,
    /// Kernel definitions valid for this board; an override must be one of these.
    pub supported_kernels: Vec<String>,
    /// Kernel used when none is specified.
    pub default_kernel: String,
    /// u-boot patch series valid for this board (e.g. `rk3576-display`,
    /// `rk3576-util`, `rk3576-loader`) — the u-boot analogue of
    /// [`supported_kernels`](Self::supported_kernels). Selecting one applies its
    /// `uboot`-scope series over the compiled u-boot; the kernel tree is untouched, so
    /// a u-boot variant costs a series here, not a whole kernel definition. Empty on a
    /// board whose u-boot ships pristine (the RK1) or whose firmware is not ours to
    /// build (a depthcharge Chromebook). Only meaningful under `rockchip-rkbin`.
    #[serde(default)]
    pub supported_uboot_series: Vec<String>,
    /// u-boot series used when none is specified. Required when
    /// [`supported_uboot_series`](Self::supported_uboot_series) is non-empty, and
    /// must name one of them.
    #[serde(default)]
    pub default_uboot_series: Option<String>,
    /// Debian suite used when none is specified (RK1: `forky`).
    pub default_suite: String,
    /// Image layout used when none is specified.
    pub default_layout: Layout,
    /// Default image hostname.
    pub hostname: String,
    /// Default image size (authored string, e.g. `2G`).
    pub image_size: String,
    /// Console keyboard layout, for a board that *has* a keyboard. Absent on a
    /// headless board — a server or a TV box has no console anyone types at, and a
    /// layout declared for it would configure nothing. Overridable per recipe or with
    /// `--keymap`, since a board can always gain a USB keyboard on its HDMI console.
    #[serde(default)]
    pub keymap: Option<Keymap>,
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
    /// System locale — the `LANG=` an image boots with, written to
    /// `/etc/locale.conf`. Distro policy rather than a hardware property, so it lives
    /// here and not on a device; a recipe or `--locale` overrides it.
    ///
    /// Defaults to `C.UTF-8`, which glibc builds in: it is a complete UTF-8 locale
    /// that resolves on an image carrying no locale data at all, so a config root that
    /// omits this still yields a working system.
    #[serde(default = "default_locale")]
    pub locale: String,
    /// Locales generated into the image *in addition to* [`locale`](Self::locale),
    /// which resolution always generates — so this lists only the extras (see
    /// [`ResolvedBuild::locales_generate`]).
    ///
    /// Each becomes a line in `/etc/locale.gen`, and `locale-gen` compiles it into the
    /// image at build time. That is what lets a *pre-built* image switch to one of them
    /// with no network: the locale data is already there.
    #[serde(default)]
    pub locales_generate: Vec<String>,
    /// System timezone, as a `tzdata` zone name (`UTC`, `America/New_York`). Sets the
    /// `/etc/localtime` symlink, which is the only interface `tzdata` and `systemd`
    /// still read — forky's `tzdata` deletes `/etc/timezone` outright.
    #[serde(default = "default_timezone")]
    pub timezone: String,
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
// Localization
// ---------------------------------------------------------------------------

/// The system locale a config root falls back to: `C.UTF-8`, which glibc builds into
/// `libc-bin` — so it resolves even on an image carrying no locale data at all.
fn default_locale() -> String {
    "C.UTF-8".to_string()
}

/// The system timezone a config root falls back to.
fn default_timezone() -> String {
    "UTC".to_string()
}

/// The XKB model `keyboard-configuration` assumes for a generic keyboard, and the
/// value it writes when nothing says otherwise.
const DEFAULT_XKB_MODEL: &str = "pc105";

/// The codeset half of a locale's `/etc/locale.gen` line: `en_US.UTF-8` → `UTF-8`,
/// `sr_RS.UTF-8@latin` → `UTF-8`.
///
/// `locale-gen` reads `<name> <codeset>` pairs, and the codeset is not free-standing
/// config — it is carried inside the locale name, after the `.` and before any
/// `@modifier`. Returns `None` for a name with no codeset (`de_DE`), which resolution
/// rejects: `locale-gen` could not act on it.
pub fn locale_codeset(locale: &str) -> Option<&str> {
    let (_, after_dot) = locale.rsplit_once('.')?;
    let codeset = after_dot.split('@').next().unwrap_or(after_dot);
    (!codeset.is_empty()).then_some(codeset)
}

/// Console keyboard layout — the four XKB variables Debian's
/// `keyboard-configuration` reads out of `/etc/default/keyboard`.
///
/// Authored as a bare layout code, which takes Debian's defaults for the rest:
///
/// ```toml
/// keymap = "us"
/// ```
///
/// or as a table, when the layout alone is not enough:
///
/// ```toml
/// keymap = { layout = "gb", variant = "extd", options = "ctrl:nocaps" }
/// ```
///
/// It sits at the **device** layer because whether a console keymap means anything is
/// a property of the hardware: a laptop has a keyboard under the user's hands, a
/// headless server has none. A board that omits it gets no generated
/// `/etc/default/keyboard`, leaving `keyboard-configuration`'s own default (`pc105` /
/// `us`) in place — the right outcome for an image nobody types at.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Keymap {
    /// `XKBLAYOUT` — the layout code (`us`, `gb`, `de`), or a comma-separated list of
    /// them. The one field a keymap cannot omit.
    pub layout: String,
    /// `XKBMODEL` — the physical keyboard model. Defaults to `pc105`, which is what
    /// Debian writes for a keyboard it was told nothing about.
    pub model: String,
    /// `XKBVARIANT` — the layout variant (`dvorak`, `nodeadkeys`); empty for none.
    pub variant: String,
    /// `XKBOPTIONS` — comma-separated XKB options (`ctrl:nocaps`); empty for none.
    pub options: String,
}

impl Keymap {
    /// A keymap from a bare layout code, with Debian's defaults for the other three.
    pub fn from_layout(layout: &str) -> Self {
        Self {
            layout: layout.to_string(),
            model: DEFAULT_XKB_MODEL.to_string(),
            variant: String::new(),
            options: String::new(),
        }
    }
}

/// `XKBMODEL`'s default, as a `serde` field default.
fn default_xkb_model() -> String {
    DEFAULT_XKB_MODEL.to_string()
}

// Manual `Deserialize` rather than `#[serde(untagged)]`: the bare-string form is what
// a device author writes 95% of the time, but an untagged enum ignores unknown fields
// regardless of `deny_unknown_fields`, so a table with a misspelled `varient` would be
// silently dropped. Dispatching on the TOML value kind — string → layout-only, table →
// a `deny_unknown_fields` helper — keeps both shapes and makes the typo a named error.
impl<'de> Deserialize<'de> for Keymap {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct Table {
            layout: String,
            #[serde(default = "default_xkb_model")]
            model: String,
            #[serde(default)]
            variant: String,
            #[serde(default)]
            options: String,
        }

        struct KeymapVisitor;

        impl<'de> serde::de::Visitor<'de> for KeymapVisitor {
            type Value = Keymap;

            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a layout code (\"us\") or a table with a `layout` key")
            }

            fn visit_str<E>(self, v: &str) -> Result<Keymap, E>
            where
                E: serde::de::Error,
            {
                Ok(Keymap::from_layout(v))
            }

            fn visit_map<A>(self, map: A) -> Result<Keymap, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let t = Table::deserialize(serde::de::value::MapAccessDeserializer::new(map))?;
                Ok(Keymap {
                    layout: t.layout,
                    model: t.model,
                    variant: t.variant,
                    options: t.options,
                })
            }
        }

        deserializer.deserialize_any(KeymapVisitor)
    }
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

/// A kernel definition (`kernels/<id>.toml`), tagged by [`KernelFlavor`].
///
/// A kernel is a versioned entity that owns everything version-coupled, so bumping
/// one means authoring a *new* definition rather than editing a device. What it
/// owns depends on where it comes from: a compiled kernel owns a source ref, a base
/// defconfig, config fragments, and a patch series; a distribution kernel owns
/// only its package name, because Debian owns everything else.
///
/// The variant is chosen by the file's `flavor` key: [`ConfigRoot::kernel`] reads it
/// and deserializes into that variant's struct, so each keeps `deny_unknown_fields`
/// — a `config_fragments` on a distro kernel, or a missing `source` on a mainline
/// one, is a parse error naming the file.
///
/// [`ConfigRoot::kernel`]: crate::loader::ConfigRoot::kernel
#[derive(Debug, Clone)]
pub enum KernelDef {
    /// A kernel compiled from source (`mainline` or `vendor`).
    Compiled(CompiledKernelDef),
    /// The distribution's own kernel package (`distro-package`).
    Distro(DistroKernelDef),
}

impl KernelDef {
    /// SoCs this kernel supports; resolution rejects a mismatched device.
    pub fn supported_socs(&self) -> &[Soc] {
        match self {
            KernelDef::Compiled(k) => &k.supported_socs,
            KernelDef::Distro(k) => &k.supported_socs,
        }
    }
}

/// The distribution's own kernel package — no source, no defconfig, no fragments,
/// no patches. The build installs it from the mirror, and the rootfs package
/// manifest pins its exact version and hash, so the lock records no kernel commit.
///
/// One definition serves every suite: the *suite* decides the version (forky
/// resolves 7.1.x, trixie 6.12.x from the same `linux-image-armmp`), and that
/// resolution is already captured, per-recipe, in the solved package manifest.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DistroKernelDef {
    /// Always [`KernelFlavor::DistroPackage`]; the key that selected this variant.
    pub flavor: KernelFlavor,
    /// The kernel package to install (e.g. `linux-image-armmp`). Resolution adds it
    /// to the rootfs package set, so it installs — and pins — like any other package.
    pub package: String,
    /// SoCs this kernel supports; resolution rejects a mismatched device.
    pub supported_socs: Vec<Soc>,
}

/// A kernel compiled from source: it owns its source ref, base defconfig, config
/// fragments, and patch series.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CompiledKernelDef {
    /// Mainline vs. vendor provenance; the key that selected this variant.
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
    /// Patch series in the `patches` repo, applied to the kernel tree in listed
    /// order; an empty list for a kernel that applies no series — a fully-upstream
    /// SoC or a pre-patched vendor tree. Every series a kernel names comes from the
    /// one [`patches_url`](Self::patches_url) checkout at one pinned commit, so
    /// composing them (e.g. a SoC-fix series plus an out-of-tree driver series) adds
    /// series *names*, not sources. A kernel with an empty list never reads the
    /// `patches` repo; each named series' declared kernel range is gated at build
    /// time. Authored explicitly (`patch_series = []` states "no series" on purpose,
    /// so it cannot be forgotten).
    pub patch_series: Vec<String>,
    /// Clone URL of the `patches` repo the series live in. Used to auto-fetch the
    /// series at the lock-pinned commit when no local checkout is present, and
    /// recorded in the lock so the pin names the repo its commit is meaningful in.
    ///
    /// Required whenever [`patch_series`](Self::patch_series) is non-empty —
    /// resolution rejects named series without one — and omitted only by a kernel
    /// that applies no series. An explicit `--patches-path`/`--patches-url` overrides
    /// it for a given run without changing what the lock records.
    #[serde(default)]
    pub patches_url: Option<String>,
    /// The `patches` ref to pin the series at: a release tag once the series has one,
    /// otherwise the default branch. Recorded in the lock beside the commit as the
    /// human-legible half of the pin.
    ///
    /// Defaults to [`DEFAULT_PATCHES_REF`].
    #[serde(default)]
    pub patches_ref: Option<String>,
    /// SoCs this kernel supports; resolution rejects a mismatched device.
    pub supported_socs: Vec<Soc>,
}

/// The `patches` ref recorded when a kernel definition names none.
///
/// A patches release marks a validation event ("this series is validated against
/// kernel X on these boards"), not a commit, so tags are rare and there is a real
/// stretch with none. Naming the branch is the honest value for that stretch: it says
/// the pin was taken from the tip of development rather than implying a release that
/// was never cut.
pub const DEFAULT_PATCHES_REF: &str = "main";

// ---------------------------------------------------------------------------
// Recipe
// ---------------------------------------------------------------------------

/// How far a recipe has been taken on real hardware.
///
/// The claim is per *recipe*, not per device, because it varies within a device: a
/// board can have one build point booted and another — a different kernel, suite, or
/// feature set — never built. Ordered from strongest to weakest claim.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum SupportStatus {
    /// An image built from this recipe has booted on the hardware. The strongest
    /// claim the project makes, and the only one that required a physical board.
    Validated,
    /// Derived from a validated sibling — it differs along an axis expected not to
    /// change the outcome (a suite, an added feature) — but this exact point was
    /// never built, or was built and never booted. Honest for a configuration
    /// believed good on reasoning rather than evidence.
    Expected,
    /// Under active bring-up: it may not build, and no validated sibling backs it.
    Experimental,
}

kebab_enum!(SupportStatus {
    Validated => "validated", Expected => "expected", Experimental => "experimental" });

/// A recipe's support claim: what the maintainer asserts about this build point,
/// and when the assertion was last established.
///
/// This is the *declared* half of the project's support story; the generated
/// support matrix is the other half, joining this claim to the exact pins the
/// sibling lock records. The two agree by construction because the matrix reads the
/// pins from the lock rather than restating them — and `update` warns when it moves
/// pins out from under a [`Validated`](SupportStatus::Validated) claim, which is the
/// only moment the pair can be driven apart.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Support {
    /// How far this build point has been taken.
    pub status: SupportStatus,
    /// `YYYY-MM-DD` the claim was last established: for
    /// [`Validated`](SupportStatus::Validated) the day the image booted, otherwise
    /// the day the claim was last assessed. Validated as a real calendar date at
    /// load, so a published claim cannot carry an impossible one.
    ///
    /// A date is unavoidably an assertion — nothing mechanical knows when a board
    /// was booted — so it lives here, with the rest of the assertion, rather than
    /// in the lock, which records only what a build resolved.
    #[serde(deserialize_with = "de_iso_date")]
    pub date: String,
}

/// Deserialize a support-claim date, enforcing `YYYY-MM-DD` *and* real-calendar
/// validity (month, day-in-month, leap years) at the parse boundary.
///
/// Shape alone would admit `2026-13-45`. A support claim is published — it renders
/// into the matrix and the docs — so an impossible date must fail attributably at
/// load rather than reach a reader.
fn de_iso_date<'de, D>(d: D) -> Result<String, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let s = String::deserialize(d)?;
    check_iso_date(&s)
        .map_err(|why| serde::de::Error::custom(format!("support date '{s}' {why}")))?;
    Ok(s)
}

/// Validate one `YYYY-MM-DD` calendar date, returning the failure reason for a
/// deserializer to wrap. Proleptic Gregorian: a leap year is divisible by 4,
/// except centuries, except those divisible by 400.
fn check_iso_date(s: &str) -> Result<(), String> {
    let parts: Vec<&str> = s.split('-').collect();
    let [y, m, d] = parts[..] else {
        return Err("is not in YYYY-MM-DD form".into());
    };
    if (y.len(), m.len(), d.len()) != (4, 2, 2)
        || !s.bytes().all(|b| b.is_ascii_digit() || b == b'-')
    {
        return Err("is not in YYYY-MM-DD form".into());
    }
    // Widths and digit-ness are established above, so these cannot fail.
    let (year, month, day) = (
        y.parse::<u32>().unwrap(),
        m.parse::<u32>().unwrap(),
        d.parse::<u32>().unwrap(),
    );
    if !(1..=12).contains(&month) {
        return Err(format!("has month {month}, which is not 1-12"));
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let last = match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if leap => 29,
        _ => 28,
    };
    if !(1..=last).contains(&day) {
        return Err(format!(
            "has day {day}, but month {month} of {year} has {last}"
        ));
    }
    Ok(())
}

/// What a recipe produces.
///
/// Most recipes build a full device→Debian image. A u-boot-only recipe — a
/// bring-up loader or a recovery u-boot — builds just the bootloader: it resolves no
/// suite, no rootfs, no kernel, and no image node, and its deliverable is the
/// [`--stage uboot`](crate) artifacts. The marker is what lets such a recipe drop the
/// suite from its identity (a `loader` is the same tool whatever Debian release it
/// sits next to), so its `.toml` carries no `suite`/`kernel`/`image_size`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Deliverable {
    /// A full image: a kernel (compiled or distro), a rootfs, and the assembled
    /// `.img`. The default when a recipe names none.
    #[default]
    Image,
    /// The bootloader alone: the u-boot node's artifacts, with no suite, rootfs,
    /// kernel, or image node.
    Uboot,
}

/// A recipe (`recipes/<name>.toml`): a named, buildable point across the
/// device, kernel, suite, features, and image-knob axes. Holds *constraints*;
/// the exact resolution is written to the sibling lock. Every axis but `device`
/// is optional and falls back to the device default.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Recipe {
    /// The device this recipe builds.
    pub device: String,
    /// What this recipe produces; [`Deliverable::Image`] unless it declares
    /// `deliverable = "uboot"`. A [`Deliverable::Uboot`] recipe leaves `suite`,
    /// `kernel`, `layout`, and `image_size` unset — resolution ignores them and the
    /// lock records no rootfs.
    #[serde(default)]
    pub deliverable: Deliverable,
    /// Kernel override; `None` → device `default_kernel`. Ignored for a
    /// [`Deliverable::Uboot`] recipe, which resolves no kernel.
    #[serde(default)]
    pub kernel: Option<String>,
    /// u-boot patch series override; `None` → device `default_uboot_series`. Must
    /// name one of the device's [`supported_uboot_series`](DeviceLayer::supported_uboot_series).
    #[serde(default)]
    pub uboot_series: Option<String>,
    /// Suite override; `None` → device `default_suite`. Ignored for a
    /// [`Deliverable::Uboot`] recipe, which resolves no suite.
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
    /// System-locale override; `None` → base `locale`.
    #[serde(default)]
    pub locale: Option<String>,
    /// Extra-locale override; `None` → base `locales_generate`. `Some` **replaces**
    /// the base list rather than adding to it, so a recipe can drop a locale the base
    /// generates as well as add one.
    #[serde(default)]
    pub locales_generate: Option<Vec<String>>,
    /// Timezone override; `None` → base `timezone`.
    #[serde(default)]
    pub timezone: Option<String>,
    /// Keymap override; `None` → device `keymap`.
    #[serde(default)]
    pub keymap: Option<Keymap>,
    /// The maintainer's support claim for this build point.
    ///
    /// Optional because a claim is only meaningful from someone in a position to
    /// make it: every *shipped* recipe declares one (a test enforces it, and the
    /// generated support matrix covers exactly those that do), while a recipe
    /// authored locally against your own board asserts nothing until you say so.
    /// `None` is therefore "no claim made", not a fourth status.
    #[serde(default)]
    pub support: Option<Support>,
}

/// Per-axis overrides applied during resolution.
///
/// Populated from CLI flags, and also used internally to fold a recipe's fields
/// in (CLI flag wins over recipe value, which wins over device default). A field
/// left `None` defers to the layer below it.
#[derive(Debug, Clone, Default)]
pub struct Overrides {
    /// What the build produces. Folded in from the recipe's
    /// [`deliverable`](Recipe::deliverable); [`Deliverable::Image`] for a direct
    /// device build. A [`Deliverable::Uboot`] resolution skips the kernel, suite,
    /// features, and rootfs axes.
    pub deliverable: Deliverable,
    /// Override the kernel definition id.
    pub kernel: Option<String>,
    /// Override the u-boot patch series (must be in the device's
    /// `supported_uboot_series`).
    pub uboot_series: Option<String>,
    /// Override the Debian suite.
    pub suite: Option<String>,
    /// Override the image layout.
    pub layout: Option<Layout>,
    /// Override the boot method (must be in the device's supported set).
    pub boot_method: Option<BootMethod>,
    /// Override the depthcharge board profile (must be in the device's
    /// `supported_boards`). Ignored by boot methods that have no board profile.
    pub board: Option<String>,
    /// Override the feature set (`None` defers to the recipe; `Some` replaces it).
    pub features: Option<Vec<String>>,
    /// Override the image size.
    pub image_size: Option<String>,
    /// Override the system locale (the image's `LANG`).
    pub locale: Option<String>,
    /// Override the *extra* locales generated into the image (`Some` replaces the
    /// base list). The system locale is folded in by resolution either way, so this
    /// never has to repeat it.
    pub locales_generate: Option<Vec<String>>,
    /// Override the system timezone.
    pub timezone: Option<String>,
    /// Override the console keymap. Accepted even for a device that declares none:
    /// `console-setup` ships on every image, so a keymap is always actionable — a
    /// headless board simply has no reason to *default* one.
    pub keymap: Option<Keymap>,
}

// ---------------------------------------------------------------------------
// Resolved build (output of resolution)
// ---------------------------------------------------------------------------

/// The kernel axis of a [`ResolvedBuild`]: a [`KernelDef`] resolved against the
/// device, tagged the same way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "flavor", rename_all = "kebab-case")]
pub enum ResolvedKernel {
    /// A kernel this build compiles, patches, and configures.
    Compiled(ResolvedCompiledKernel),
    /// A kernel this build installs from the Debian mirror.
    Distro(ResolvedDistroKernel),
}

impl ResolvedKernel {
    /// Kernel definition id (e.g. `rk3588-mainline-7.1`).
    pub fn id(&self) -> &str {
        match self {
            ResolvedKernel::Compiled(k) => &k.id,
            ResolvedKernel::Distro(k) => &k.id,
        }
    }

    /// Mainline / vendor / distro-package provenance.
    pub fn flavor(&self) -> KernelFlavor {
        match self {
            ResolvedKernel::Compiled(k) => k.flavor,
            ResolvedKernel::Distro(_) => KernelFlavor::DistroPackage,
        }
    }

    /// The compile inputs, or `None` for a distro-package kernel. `Some` is exactly
    /// the condition under which the kernel node builds, the patch series applies,
    /// and the lock pins a kernel commit.
    pub fn compiled(&self) -> Option<&ResolvedCompiledKernel> {
        match self {
            ResolvedKernel::Compiled(k) => Some(k),
            ResolvedKernel::Distro(_) => None,
        }
    }

    /// The patch series this kernel applies, in order; empty when it applies no
    /// series — either an empty authored list, or a distro-package kernel, which never
    /// reads the `patches` repo at all.
    pub fn patch_series(&self) -> &[String] {
        self.compiled()
            .map(|k| k.patch_series.as_slice())
            .unwrap_or(&[])
    }
}

/// A resolved compiled kernel: the definition flattened with its merged fragment
/// list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedCompiledKernel {
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
    /// Patch series names, applied to the kernel tree in listed order; empty when
    /// this kernel applies no series. An empty list means the build never reads the
    /// `patches` repo: no checkout is resolved, no series is applied, and the lock
    /// records no `[patches]` table. All series share the one `patches` checkout
    /// ([`patches_url`](Self::patches_url)) at one pinned commit.
    pub patch_series: Vec<String>,
    /// Clone URL of the `patches` repo, for auto-fetching the series at the
    /// lock-pinned commit when no local checkout is present, and for the lock's
    /// `[patches] source`. Resolution guarantees this is `Some` exactly when
    /// [`patch_series`](Self::patch_series) is non-empty: a series always names
    /// the repo it comes from, and a kernel with no series has nothing to fetch.
    pub patches_url: Option<String>,
    /// The `patches` ref the lock pins the series at — the definition's
    /// `patches_ref`, or [`DEFAULT_PATCHES_REF`].
    /// `Some` under the same condition as [`patches_url`](Self::patches_url).
    pub patches_ref: Option<String>,
    /// Kernel-owned fragments followed by device fragments, in apply order.
    pub config_fragments: Vec<String>,
}

/// A resolved distro-package kernel: nothing but which package installs it. Its
/// exact version and hash ride the rootfs package manifest, like any other package.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedDistroKernel {
    /// Kernel definition id (e.g. `debian-armmp`).
    pub id: String,
    /// The kernel package, which resolution has already added to
    /// [`ResolvedBuild::rootfs_packages`].
    pub package: String,
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

/// The resolved ChromeOS kernel slots: where they sit and the attribute bits that
/// make the firmware boot one of them.
///
/// An image lays down [`slots`](Self::slots) partitions of the ChromeOS kernel type,
/// back to back from [`offset`](Self::offset), each [`size`](Self::size) long. The
/// **first** carries the signed payload and the attributes below; every other ships
/// empty at [`SPARE_KPART_FLAGS`](crate::chromeos::SPARE_KPART_FLAGS), waiting for the
/// first on-device kernel upgrade to write it. The spare is the entire reason an
/// upgrade can be rolled back — see [`chromeos`](crate::chromeos).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Kpart {
    /// Start of the first slot (authored string, parsed to bytes by the image node).
    pub offset: String,
    /// Size of each slot (authored string).
    pub size: String,
    /// Number of slots. Resolution guarantees `1..=MAX_KPART_SLOTS`; the image node
    /// derives each slot's start from `offset + i * size` and need not re-check it.
    pub slots: u8,
    /// Boot priority of the payload slot (GPT attribute bits 51:48).
    pub priority: u8,
    /// Remaining boot attempts for the payload slot (bits 55:52).
    pub tries: u8,
    /// Known-good flag for the payload slot (bit 56).
    pub successful: bool,
    /// The three fields above packed into the payload slot's 64-bit GPT attribute
    /// word — what actually lands on disk. Computed at resolution by
    /// [`kpart_flags`](crate::chromeos::kpart_flags), which also range-checks
    /// `priority` and `tries`, so the image node writes a value it does not have to
    /// re-validate. Spare slots carry
    /// [`SPARE_KPART_FLAGS`](crate::chromeos::SPARE_KPART_FLAGS) instead.
    pub flags: u64,
}

/// The boot-method-specific half of a [`ResolvedBuild`].
///
/// Resolution has already enforced each method's own requirements — rkbin blobs and
/// a `uboot_defconfig` for `rockchip-rkbin`, a board profile for `depthcharge` — so
/// every field here is guaranteed present for the method that owns it, and the
/// engine matches once rather than testing for absent fields.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "method", rename_all = "kebab-case")]
pub enum ResolvedBoot {
    /// Compile u-boot, write idbloader + `u-boot.itb` into the raw gap.
    RockchipRkbin(ResolvedRkbinBoot),
    /// Place a `depthchargectl`-signed kernel FIT in a ChromeOS kernel partition.
    Depthcharge(ResolvedDepthchargeBoot),
}

impl ResolvedBoot {
    /// Start offset of the rootfs partition. Both methods place the rootfs after
    /// whatever they own at the head of the medium, so the image node reads this
    /// without caring which method resolved it.
    pub fn rootfs_offset(&self) -> &str {
        match self {
            ResolvedBoot::RockchipRkbin(b) => &b.offsets.rootfs,
            ResolvedBoot::Depthcharge(b) => &b.rootfs_offset,
        }
    }
}

/// The resolved `rockchip-rkbin` boot configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedRkbinBoot {
    /// u-boot defconfig for this board (from the device; required by this method).
    pub uboot_defconfig: String,
    /// u-boot git source (from the boot method).
    pub uboot_source: String,
    /// u-boot ref constraint (from the boot method); pinned exactly in the lock.
    pub uboot_ref: String,
    /// The selected u-boot patch series, or `None` when u-boot ships pristine (the
    /// authored [`NO_PATCH_SERIES`](crate::series::NO_PATCH_SERIES) sentinel, or a
    /// board that declares no series). `Some` is exactly when the u-boot node
    /// applies a `uboot`-scope series and the lock records a `[uboot_patches]` table.
    pub uboot_series: Option<String>,
    /// Clone URL of the `patches` repo the u-boot series comes from (from the boot
    /// method). Resolution guarantees this is `Some` exactly when
    /// [`uboot_series`](Self::uboot_series) is `Some`.
    pub uboot_patches_url: Option<String>,
    /// The `patches` ref the u-boot series is pinned at — the boot method's
    /// `patches_ref`, or [`DEFAULT_PATCHES_REF`].
    /// `Some` under the same condition as [`uboot_patches_url`](Self::uboot_patches_url).
    pub uboot_patches_ref: Option<String>,
    /// rkbin blob set — the SoC's defaults merged with the device's overrides.
    /// Resolution guarantees `atf` and `tpl` are present.
    pub rkbin: Rkbin,
    /// Raw-gap offsets (from the boot method).
    pub offsets: Offsets,
}

/// The resolved `depthcharge` boot configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedDepthchargeBoot {
    /// The selected board profile, passed verbatim to `depthchargectl` (its `board`
    /// codename). Validated at resolution against the device's `supported_boards`.
    pub board: String,
    /// The ChromeOS kernel partition this build writes.
    pub kpart: Kpart,
    /// Kernel command line baked into the signed FIT, minus `root=` — which
    /// `depthchargectl` derives from `/etc/fstab`. The boot method's `cmdline`
    /// with the device's extra [`kernel_cmdline`](DeviceLayer::kernel_cmdline)
    /// (if any) appended; the merged value passes the same validation.
    pub cmdline: String,
    /// Start offset of the rootfs partition.
    pub rootfs_offset: String,
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
    /// The resolved device and everything it extends, base-most first, ending with
    /// [`device`](Self::device).
    ///
    /// Carried because the merged layers no longer show the chain, and the engine
    /// needs it: a device's `overlay/` tree is found by its *name*, so laying in only
    /// the resolved device's would silently drop the parent's runtime config from a
    /// variant image. Laid in lineage order, so a variant's copy of a file wins. A
    /// device that extends nothing has a one-entry lineage, which makes the
    /// no-inheritance case the same code path rather than a special case.
    pub device_lineage: Vec<String>,
    /// Device description.
    pub description: String,
    /// Target architecture (from the SoC layer).
    pub arch: Arch,
    /// Target SoC.
    pub soc: Soc,
    /// Selected boot method.
    pub boot_method: BootMethod,
    /// Resolved kernel axis, or `None` for a [`Deliverable::Uboot`] build, which
    /// resolves and builds no kernel. `Some` for every image build (a compiled or
    /// distro kernel).
    pub kernel: Option<ResolvedKernel>,
    /// Debian suite, or `None` for a [`Deliverable::Uboot`] build, which resolves no
    /// rootfs. `Some` for every image build. Present exactly when
    /// [`kernel`](Self::kernel) is — the two absences travel together, and
    /// [`produces_image`](Self::produces_image) reads this.
    pub suite: Option<String>,
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
    /// System locale — the image's `LANG`, written to `/etc/locale.conf`. Resolution
    /// guarantees it also appears in [`locales_generate`](Self::locales_generate), so
    /// the `LANG` an image boots with can never name a locale that image does not have.
    pub locale: String,
    /// Every locale generated into the image, in `/etc/locale.gen` order: the system
    /// [`locale`](Self::locale) first, then the configured extras, de-duplicated.
    ///
    /// The build compiles these with `locale-gen`, which is what makes a *pre-built*
    /// image reconfigurable offline: switching to one of them needs no network,
    /// because the data is already on the disk.
    pub locales_generate: Vec<String>,
    /// System timezone (a `tzdata` zone name), materialized as the `/etc/localtime`
    /// symlink.
    pub timezone: String,
    /// Console keyboard layout, or `None` on a board with no keyboard — in which case
    /// the build writes no `/etc/default/keyboard` and Debian's own default stands.
    pub keymap: Option<Keymap>,
    /// The boot-method-specific configuration, tagged by method: what the
    /// bootloader/boot payload is, where it goes, and what it needs. Every field is
    /// guaranteed present for the method it belongs to.
    pub boot: ResolvedBoot,
    /// Board DTB path, relative to the DT output dir.
    pub kernel_dtb: String,
    /// Config-root-relative device-tree sources the kernel stage copies into the
    /// in-tree DT dir before `make` (from the device). Empty when the board's DTB is
    /// already upstream. Resolution guarantees each path is contained (relative, no
    /// `..`), names a `.dts`/`.dtsi`, and that [`kernel_dtb`](Self::kernel_dtb) is
    /// compiled from one of them. §4.
    pub device_dts: Vec<String>,
    /// Out-of-tree kernel-module sets, in the order the device named them: each
    /// `kmods/<name>.toml` loaded and validated, built against this build's kernel tree
    /// and staged into `/lib/modules/<kver>/updates/`. Empty for a board that carries
    /// none. Resolution guarantees each name is unique and dpkg-safe, each
    /// `subdir`/`patch_dir` is contained, each patch/module entry is a bare name, and —
    /// since the modules need a tree to build against — that the kernel is compiled, not
    /// a distro package.
    pub device_kmods: Vec<ResolvedKmod>,
    /// Extra kernel command-line arguments (from the device), space-separated and
    /// trimmed; empty when the board declares none. Resolution guarantees the value
    /// is a single line of plain arguments, safe to embed in the sourced
    /// `/etc/boot2deb/board.conf` (no quote/escape/expansion characters), and free
    /// of `root=`. The extlinux path emits it as `EXTL_CMD_LINE`; the depthcharge
    /// path has already folded it into [`ResolvedDepthchargeBoot::cmdline`].
    pub kernel_cmdline: String,
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

impl ResolvedBuild {
    /// The `rockchip-rkbin` boot configuration, or `None` under another boot
    /// method. `Some` is exactly the condition under which this build compiles
    /// u-boot, consumes rkbin blobs, and writes a raw-gap bootloader — so the
    /// u-boot node, the blob pins, and the raw-gap image path all key on it.
    pub fn rkbin_boot(&self) -> Option<&ResolvedRkbinBoot> {
        match &self.boot {
            ResolvedBoot::RockchipRkbin(b) => Some(b),
            _ => None,
        }
    }

    /// The `depthcharge` boot configuration, or `None` under another boot method.
    pub fn depthcharge_boot(&self) -> Option<&ResolvedDepthchargeBoot> {
        match &self.boot {
            ResolvedBoot::Depthcharge(b) => Some(b),
            _ => None,
        }
    }

    /// Whether this build compiles a kernel from source. False for a
    /// distro-package kernel, whose `linux-image-*` comes from the Debian mirror
    /// like any other package, and for a u-boot-only build, which has no kernel at
    /// all — so the kernel compile node, its patch series, and its config fragments
    /// are all skipped, and the lock pins no kernel commit.
    pub fn compiles_kernel(&self) -> bool {
        matches!(self.kernel, Some(ResolvedKernel::Compiled(_)))
    }

    /// Whether this build produces a full image (kernel + rootfs + `.img`), as
    /// opposed to a [`Deliverable::Uboot`] build that emits only the bootloader.
    /// `true` is exactly when [`suite`](Self::suite) is present, which resolution
    /// keeps in lockstep with [`kernel`](Self::kernel): the rootfs, image, and
    /// sandbox nodes all key on it.
    pub fn produces_image(&self) -> bool {
        self.suite.is_some()
    }

    /// The Debian suite, for a caller that only runs on the image path (the rootfs
    /// and image nodes). Panics on a u-boot-only build, which resolves no suite —
    /// [`produces_image`](Self::produces_image) is the check for a caller that might
    /// see either kind.
    pub fn image_suite(&self) -> &str {
        self.suite
            .as_deref()
            .expect("image_suite is read only on the image path, which resolves a suite")
    }
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
        assert_eq!(
            with_url.locator().unwrap(),
            ExtraDebLocator::Url("https://x/a.deb")
        );
        let with_path = ExtraDeb {
            url: None,
            path: Some("vendor/a.deb".into()),
            sha256: hex64.clone(),
        };
        assert_eq!(
            with_path.locator().unwrap(),
            ExtraDebLocator::Path("vendor/a.deb")
        );

        // Neither / both locators is ExtraDebLocator.
        let neither = ExtraDeb {
            url: None,
            path: None,
            sha256: hex64.clone(),
        };
        assert!(matches!(
            neither.validate(),
            Err(ConfigError::ExtraDebLocator { .. })
        ));
        let both = ExtraDeb {
            url: Some("u".into()),
            path: Some("p".into()),
            sha256: hex64.clone(),
        };
        assert!(matches!(
            both.validate(),
            Err(ConfigError::ExtraDebLocator { .. })
        ));

        // A malformed hash (wrong length, uppercase, non-hex) is ExtraDebBadHash —
        // uppercase would spuriously mismatch the lowercase content hash.
        for bad in ["", "abc", &"A".repeat(64), &"g".repeat(64)] {
            let d = ExtraDeb {
                url: None,
                path: Some("p".into()),
                sha256: bad.to_string(),
            };
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
                matches!(
                    unsafe_deb(bad).validate(),
                    Err(ConfigError::ExtraDebUnsafePath { .. })
                ),
                "expected an unsafe-path error for {bad:?}"
            );
        }
        // A plain nested relative path is contained and validates.
        assert!(unsafe_deb("vendor/sub/a.deb").validate().is_ok());
        // A `.` segment does not escape and is allowed.
        assert!(unsafe_deb("./vendor/a.deb").validate().is_ok());
    }

    #[test]
    fn a_support_claim_parses_and_rejects_impossible_dates() {
        let claim: Support =
            toml::from_str("status = \"validated\"\ndate = \"2026-07-14\"\n").unwrap();
        assert_eq!(claim.status, SupportStatus::Validated);
        assert_eq!(claim.date, "2026-07-14");

        // A recipe may omit the claim entirely (`None` = no claim made), but an
        // unknown status is a typo, not a fourth state.
        let none: Recipe = toml::from_str("device = \"turing-rk1\"\n").unwrap();
        assert!(none.support.is_none());
        assert!(
            toml::from_str::<Support>("status = \"probably-fine\"\ndate = \"2026-07-14\"\n")
                .is_err()
        );

        // Shape failures.
        for bad in [
            "2026-7-14",
            "26-07-14",
            "2026/07/14",
            "2026-07-14T00:00:00Z",
            "",
            "x",
        ] {
            assert!(
                check_iso_date(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
        // Calendar failures: a date that parses but cannot exist.
        for bad in [
            "2026-13-01",
            "2026-00-10",
            "2026-04-31",
            "2026-02-30",
            "2026-01-00",
        ] {
            assert!(
                check_iso_date(bad).is_err(),
                "expected {bad:?} to be rejected"
            );
        }
        // February follows the proleptic-Gregorian leap rule: 2024 and 2000 are
        // leap years, 2026 and 1900 are not.
        assert!(check_iso_date("2024-02-29").is_ok());
        assert!(check_iso_date("2000-02-29").is_ok());
        assert!(check_iso_date("2026-02-29").is_err());
        assert!(check_iso_date("1900-02-29").is_err());
    }
}
