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
    /// The per-image first-boot password (SEC-6). Deliberately unique per
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
    /// Verified rkbin blob pins.
    pub blobs: BlobsProvenance,
    /// Build host / toolchain identity.
    pub toolchain: ToolchainProvenance,
    /// First-boot credential — the per-image secret (SEC-6).
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
}

/// Every pinned source, as `ref` + exact `commit` pairs (from the [`Lock`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourcesProvenance {
    /// Kernel definition id.
    pub kernel_id: String,
    /// Kernel ref that was pinned.
    pub kernel_ref: String,
    /// Kernel commit.
    pub kernel_commit: String,
    /// Patch profile name.
    pub patch_profile: String,
    /// `patches` repo commit the series is pinned at.
    pub patches_commit: String,
    /// u-boot ref.
    pub uboot_ref: String,
    /// u-boot commit.
    pub uboot_commit: String,
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
    /// The per-image password (SEC-6).
    pub password: String,
    /// How the credential behaves on the shipped image.
    pub note: String,
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
            suite: build.suite.clone(),
            features: build.features.clone(),
            layout: build.layout.to_string(),
            image_size: build.image_size.clone(),
            hostname: build.hostname.clone(),
        },
        sources: SourcesProvenance {
            kernel_id: lock.kernel.id.clone(),
            kernel_ref: lock.kernel.reference.clone(),
            kernel_commit: lock.kernel.commit.clone(),
            patch_profile: lock.patches.profile.clone(),
            patches_commit: lock.patches.commit.clone(),
            uboot_ref: lock.uboot.reference.clone(),
            uboot_commit: lock.uboot.commit.clone(),
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
        blobs: BlobsProvenance {
            atf: lock.blobs.atf.clone(),
            tpl: lock.blobs.tpl.clone(),
        },
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
        // The fetched source axes, each classified offline by pin form. The kernel
        // and u-boot rows are always present; the media-accel rows only when the
        // stack was built. The ffmpeg `rockchip` pin is provenance-only (never
        // fetched at build), so its re-fetch durability is moot and it is omitted.
        source_durability: source_durability_rows(lock),
    }
}

/// The `[[source_durability]]` rows for a lock: kernel and u-boot always, plus the
/// four fetched media-accel trees (mpp/librga/libmali/ffmpeg-base) when the build
/// compiled the transcode stack.
fn source_durability_rows(lock: &Lock) -> Vec<SourceDurability> {
    let mut rows = vec![
        source_durability("kernel", &lock.kernel.reference, &lock.kernel.commit),
        source_durability("uboot", &lock.uboot.reference, &lock.uboot.commit),
    ];
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
            reference: r.into(),
            commit: c.into(),
        };
        Lock {
            kernel: KernelPin {
                id: "rk3588-mainline-7.1".into(),
                reference: "v7.1.1".into(),
                commit: "kc".into(),
            },
            patches: PatchesPin {
                profile: "rk3588-accel".into(),
                commit: "pc".into(),
            },
            uboot: UbootPin {
                reference: "v2026.04".into(),
                commit: "uc".into(),
            },
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
            blobs: BlobsPin {
                atf: "atf@sha256:0".into(),
                tpl: "tpl@sha256:1".into(),
            },
            extra_debs: vec![],
            snapshot: None,
        }
    }

    fn sample_build() -> ResolvedBuild {
        // A resolution over the shipped config gives a real build point to join.
        let root = crate::ConfigRoot::new(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(2)
                .unwrap()
                .to_path_buf(),
        );
        crate::resolve_recipe(&root, "turing-rk1-forky", &crate::Overrides::default()).unwrap()
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
        assert_eq!(prov.sources.kernel_commit, "kc");
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
