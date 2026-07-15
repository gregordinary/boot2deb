//! Provenance manifest — the resolved build point plus every pin joined
//! into one document answering "exactly what went into this image," for support
//! and security response.
//!
//! Pure: a deterministic join of values the [`Lock`] and [`ResolvedBuild`] already
//! hold, plus the build-time facts the engine supplies ([`BuildFacts`] — the
//! solved manifest's content hash + package count, the host/cross identity, and
//! the per-image first-boot credential). So the assembly and its canonical TOML
//! form are unit-testable without a build. It is a join of pins the build already
//! computes, not new tracking; license/SBOM data rides on the Debian packages
//! themselves and is out of scope.

use crate::lock::Lock;
use crate::model::ResolvedBuild;
use serde::Serialize;

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
    /// Verified rkbin blob pins. Absent when the boot method consumes no rkbin blobs
    /// — a depthcharge board's firmware is its own, so there is no ATF or DDR TPL in
    /// its boot chain to record.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blobs: Option<BlobsProvenance>,
    /// Build host / toolchain identity.
    pub toolchain: ToolchainProvenance,
    /// First-boot credential — the per-image secret.
    pub credentials: CredentialsProvenance,
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
    /// reachability check is the `verify-sources` probe. Declared last so its
    /// array-of-tables serializes after every `[section]` table.
    pub source_durability: Vec<SourceDurability>,
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
    /// The patch profile applied to that kernel. It is the difference between two boards
    /// running the same kernel version and having different hardware working, so it
    /// belongs on the device rather than only in the build's records. Absent when the
    /// kernel applied no series.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch_profile: Option<String>,
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
    /// Patch profile name. Absent — along with
    /// [`patches_commit`](Self::patches_commit) — when the kernel applied no series,
    /// so the record never implies a `patches` dependency the build did not have.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub patch_profile: Option<String>,
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
    /// MPP ref.
    pub mpp_ref: String,
    /// MPP commit.
    pub mpp_commit: String,
    /// librga ref.
    pub librga_ref: String,
    /// librga commit.
    pub librga_commit: String,
    /// libmali ref.
    pub libmali_ref: String,
    /// libmali commit.
    pub libmali_commit: String,
    /// ffmpeg V4L2-base ref.
    pub ffmpeg_base_ref: String,
    /// ffmpeg V4L2-base commit.
    pub ffmpeg_base_commit: String,
    /// ffmpeg Rockchip provenance-tree ref (graft source).
    pub ffmpeg_rockchip_ref: String,
    /// ffmpeg Rockchip provenance-tree commit.
    pub ffmpeg_rockchip_commit: String,
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
    SystemIdentity {
        version: IDENTITY_VERSION,
        image: IdentityImage {
            device: build.device.clone(),
            description: build.description.clone(),
            arch: build.arch.to_string(),
            soc: build.soc.to_string(),
            boot_method: build.boot_method.to_string(),
            board: build.depthcharge_boot().map(|b| b.board.clone()),
            suite: build.suite.clone(),
            features: build.features.clone(),
            layout: build.layout.to_string(),
            hostname: build.hostname.clone(),
        },
        kernel: IdentityKernel {
            // From the resolved build, so they are recorded even for a kernel the lock
            // pins no commit for.
            id: build.kernel.id().to_string(),
            flavor: build.kernel.flavor().to_string(),
            package: match &build.kernel {
                crate::model::ResolvedKernel::Distro(k) => Some(k.package.clone()),
                crate::model::ResolvedKernel::Compiled(_) => None,
            },
            reference: lock.kernel.as_ref().map(|k| k.reference.clone()),
            commit: lock.kernel.as_ref().map(|k| k.commit.clone()),
            patch_profile: lock.patches.as_ref().map(|p| p.profile.clone()),
        },
    }
}

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
    ProvenanceManifest {
        image: ImageProvenance {
            device: build.device.clone(),
            description: build.description.clone(),
            arch: build.arch.to_string(),
            soc: build.soc.to_string(),
            boot_method: build.boot_method.to_string(),
            board: build.depthcharge_boot().map(|b| b.board.clone()),
            suite: build.suite.clone(),
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
            kernel_id: build.kernel.id().to_string(),
            kernel_flavor: build.kernel.flavor().to_string(),
            kernel_ref: lock.kernel.as_ref().map(|k| k.reference.clone()),
            kernel_commit: lock.kernel.as_ref().map(|k| k.commit.clone()),
            kernel_package: match &build.kernel {
                crate::model::ResolvedKernel::Distro(k) => Some(k.package.clone()),
                crate::model::ResolvedKernel::Compiled(_) => None,
            },
            patch_profile: lock.patches.as_ref().map(|p| p.profile.clone()),
            patches_commit: lock.patches.as_ref().map(|p| p.commit.clone()),
            uboot_ref: lock.uboot.as_ref().map(|u| u.reference.clone()),
            uboot_commit: lock.uboot.as_ref().map(|u| u.commit.clone()),
            // Present in lockstep: resolution pins userspace and ffmpeg together or
            // not at all, so a single `zip` yields the whole block or `None`.
            media_accel: lock.userspace.as_ref().zip(lock.ffmpeg.as_ref()).map(|(us, ff)| {
                MediaAccelProvenance {
                    mpp_ref: us.mpp.reference.clone(),
                    mpp_commit: us.mpp.commit.clone(),
                    librga_ref: us.librga.reference.clone(),
                    librga_commit: us.librga.commit.clone(),
                    libmali_ref: us.libmali.reference.clone(),
                    libmali_commit: us.libmali.commit.clone(),
                    ffmpeg_base_ref: ff.base.reference.clone(),
                    ffmpeg_base_commit: ff.base.commit.clone(),
                    ffmpeg_rockchip_ref: ff.rockchip.reference.clone(),
                    ffmpeg_rockchip_commit: ff.rockchip.commit.clone(),
                }
            }),
        },
        rootfs: RootfsProvenance {
            suite: lock.rootfs.suite.clone(),
            manifest: lock.rootfs.manifest.clone(),
            manifest_sha256: facts.manifest_sha256.to_string(),
            package_count: facts.package_count,
        },
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
        credentials: CredentialsProvenance {
            user: facts.user.to_string(),
            password: facts.password.to_string(),
            note: "expired at first login (passwd -e); unique per built image".to_string(),
        },
        extra_debs: lock.extra_debs.clone(),
        // Every source axis the build actually *fetches*, classified offline by pin
        // form. A source the build never fetches has no re-fetch durability to report,
        // so it contributes no row: a distro-package kernel and a boot method with no
        // u-boot both drop out here, as does the ffmpeg `rockchip` pin (provenance
        // only — the graft ships as patches, so that tree is never cloned).
        source_durability: source_durability_rows(lock),
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
        rows.push(source_durability("mpp", &us.mpp.reference, &us.mpp.commit));
        rows.push(source_durability("librga", &us.librga.reference, &us.librga.commit));
        rows.push(source_durability("libmali", &us.libmali.reference, &us.libmali.commit));
    }
    if let Some(ff) = &lock.ffmpeg {
        rows.push(source_durability("ffmpeg-base", &ff.base.reference, &ff.base.commit));
    }
    rows
}

/// Classify one source pin's offline durability form for the manifest.
fn source_durability(source: &str, reference: &str, commit: &str) -> SourceDurability {
    SourceDurability {
        source: source.to_string(),
        reference: reference.to_string(),
        form: crate::sources::PinForm::classify(reference, commit).as_str().to_string(),
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
                profile: "rk3588-accel".into(),
                commit: "pc".into(),
            }),
            uboot: Some(UbootPin {
                source: "us".into(),
                reference: "v2026.04".into(),
                commit: "uc".into(),
            }),
            userspace: Some(UserspacePins {
                mpp: git("mainline-cma-fix", "mc"),
                librga: git("master", "rc"),
                libmali: git("master", "lc"),
            }),
            ffmpeg: Some(FfmpegPins {
                base: git("v4l2-request-n8.1", "fbc"),
                rockchip: git("8.1", "frc"),
            }),
            rootfs: RootfsPin {
                suite: "forky".into(),
                manifest: "turing-rk1-forky.pkgs.lock".into(),
                manifest_sha256: Some("mh".into()),
            },
            blobs: Some(BlobsPin {
                atf: "atf@sha256:0".into(),
                tpl: "tpl@sha256:1".into(),
                bl32: None,
            }),
            extra_debs: vec![],
            snapshot: None,
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
        crate::resolve_recipe(&config_root(), "turing-rk1-forky", &crate::Overrides::default())
            .unwrap()
    }

    /// A depthcharge build — the boot method that *has* a board profile.
    fn depthcharge_build() -> ResolvedBuild {
        crate::resolve_recipe(&config_root(), "asus-c201-forky", &crate::Overrides::default())
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
        let text = system_identity(&sample_build(), &lock).to_toml_string().unwrap();
        // The banner *documents* that the file carries no secret, so it says the words.
        // What must not contain them is the data.
        let body: String = text.lines().filter(|l| !l.trim_start().starts_with('#')).collect();

        // The password `assemble` would have put in the manifest.
        assert!(!body.contains("Kp7rTx"), "identity leaked the first-boot password:\n{text}");
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
        };
        let manifest = assemble(&sample_build(), &lock, &facts).to_toml_string().unwrap();
        assert!(manifest.contains("Kp7rTx"), "the manifest is the document that has it");
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
        let text = system_identity(&depthcharge_build(), &sample_lock()).to_toml_string().unwrap();
        assert!(text.starts_with("# boot2deb image identity"));

        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(parsed["version"].as_integer(), Some(1));
        assert_eq!(parsed["image"]["device"].as_str(), Some("asus-c201"));
        assert_eq!(parsed["image"]["layout"].as_str(), Some("combined"));

        // A distro kernel names its package and pins no commit; that pairing is what
        // tells a reader an upgrade arrives via apt rather than a hand-placed .deb.
        assert_eq!(parsed["kernel"]["flavor"].as_str(), Some("distro-package"));
        assert_eq!(parsed["kernel"]["package"].as_str(), Some("linux-image-armmp"));

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
        };
        let prov = assemble(&build, &lock, &facts);
        assert_eq!(prov.sources.kernel_commit.as_deref(), Some("kc"));
        assert_eq!(prov.sources.kernel_flavor, "mainline");
        let media = prov.sources.media_accel.as_ref().expect("media-accel build has sources");
        assert_eq!(media.ffmpeg_rockchip_ref, "8.1");
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
            "[blobs]",
            "[toolchain]",
            "[credentials]",
            "kernel_commit = \"kc\"",
            "manifest_sha256 = \"abc123\"",
            "password = \"Kp7rTx\"",
        ] {
            assert!(text.contains(needle), "provenance TOML missing {needle}:\n{text}");
        }
        // The emitted document is valid TOML (guards the section field ordering —
        // a scalar after a nested table would be a parse error).
        let parsed: toml::Value = toml::from_str(&text).unwrap();
        assert_eq!(parsed["sources"]["media_accel"]["ffmpeg_base_commit"].as_str(), Some("fbc"));
        assert_eq!(parsed["image"]["features"][0].as_str(), Some("media-accel-rockchip"));
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
        assert_eq!(parsed["extra_debs"][0]["sha256"].as_str().unwrap().len(), 64);
        // The durability list carries every fetched source axis.
        assert_eq!(parsed["source_durability"].as_array().unwrap().len(), 6);
    }
}

