//! Composable feature model — a rootfs "add-in": a `features/<name>.toml`
//! manifest plus, by convention, a sibling `features/<name>/overlay/` tree of
//! config files.
//!
//! The rootfs feature axis is a *list* of these, stacked onto the layered
//! substrate — `base ⊕ soc ⊕ boot-method ⊕ device ⊕ Σ features`. A feature
//! declares the Debian packages it adds; its overlay tree carries the config it
//! lays into the rootfs, in the same manifest-plus-ordered-files spirit as a
//! patch profile.
//!
//! Pure: parsing plus compatibility checks (the SoC/arch gates and pairwise
//! conflicts). A feature is rootfs-only in v1 (packages + overlay + third-party
//! apt sources); the slot for a feature that also contributes kernel fragments or
//! a patch-profile addend is reserved and intentionally not modeled here
//! yet.

use crate::error::ConfigError;
use crate::model::{AptSource, Arch, Soc};
use serde::Deserialize;

/// A feature manifest (`features/<name>.toml`).
///
/// The feature's name is its file stem, not a field — it is how a recipe's
/// `features` list and other features' `conflicts` refer to it.
///
/// Features come in two conventional shapes, distinguished by naming and
/// gate, not by a type field:
/// - **Capability features** provide a platform-specific stack (a HW accel
///   provider such as `media-accel-rockchip`). Named `<capability>-<provider>`
///   and gated by hardware compat — [`requires_soc`](Feature::requires_soc) for
///   SoC-integrated accel, [`requires_arch`](Feature::requires_arch) for a
///   discrete-GPU stack.
/// - **Application features** install an app/service (e.g. `jellyfin`). Named for
///   the app, portable (no HW gate), and often carrying an
///   [`apt_sources`](Feature::apt_sources) entry because the app ships from its
///   own repo rather than the Debian mirror.
///
/// The "accelerated Jellyfin" *use case* is not a feature — it is a recipe
/// composing an app feature with the matching capability feature; there is
/// no provider auto-resolution.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Feature {
    /// One-line description, shown when listing features and in build output.
    pub description: String,
    /// Debian packages this feature adds to the rootfs. Installed from the local
    /// apt repo (the build's own `.deb`s), the suite mirror, and any
    /// [`apt_sources`](Feature::apt_sources) this feature adds — apt resolves
    /// their dependencies; order is not significant — apt solves the set.
    #[serde(default)]
    pub packages: Vec<String>,
    /// Packages this feature drops from the merged rootfs set — e.g. a
    /// feature that replaces a base package with its own variant. Unioned with
    /// every layer's `exclude`; any name in that union is removed from the include
    /// set (exclude wins).
    #[serde(default)]
    pub exclude: Vec<String>,
    /// SoCs this feature supports; empty means *any* SoC. Resolution rejects a
    /// feature whose non-empty list excludes the resolved SoC. The gate for
    /// a SoC-integrated capability feature (e.g. `media-accel-rockchip`).
    #[serde(default)]
    pub requires_soc: Vec<Soc>,
    /// Architectures this feature supports; empty means *any* arch. Resolution
    /// rejects a feature whose non-empty list excludes the resolved arch.
    /// The gate for a discrete-GPU capability feature (e.g. a hypothetical
    /// `media-accel-vaapi` on `x86_64`); orthogonal to `requires_soc`, and both
    /// gates must pass. GPU *vendor* within an arch (Intel vs AMD vs NVIDIA) is
    /// not modeled — the user picks the matching provider feature explicitly, and
    /// `conflicts` catches a clashing pair (non-goal: no provider resolution).
    #[serde(default)]
    pub requires_arch: Vec<Arch>,
    /// Third-party apt repositories this feature adds to the rootfs solve —
    /// how an application feature pulls an app that is not in the Debian mirror
    /// (Jellyfin, Plex, …). Empty for a feature whose packages all come from the
    /// mirror or the local repo.
    #[serde(default)]
    pub apt_sources: Vec<AptSource>,
    /// Pre-built `.deb`s this feature pulls from outside the Debian mirror
    /// — a content-pinned vendor download or on-disk file. Provides the
    /// *bytes* into the local apt repo, the way [`apt_sources`](Feature::apt_sources)
    /// provides a *source*; the feature's [`packages`](Feature::packages) (or another
    /// package's dependency) is what names them for install. Unioned across all
    /// layers + features and de-duplicated by sha256 at resolution.
    #[serde(default)]
    pub extra_debs: Vec<crate::model::ExtraDeb>,
    /// Other features, by name, that cannot be combined with this one. The check
    /// is symmetric — resolution rejects a selection holding this feature and any
    /// it names, or that names it — so declaring the conflict on either side is
    /// enough.
    #[serde(default)]
    pub conflicts: Vec<String>,
    /// This feature's packages are produced by building the SoC's media-accel
    /// source trees — the `[userspace]` (MPP/RGA/Mali) and `[ffmpeg]` stanzas at
    /// the SoC layer. Set on a provider feature like `media-accel-rockchip`, whose
    /// `.deb`s (`librockchip-mpp1`, `librga2`, `ffmpeg-rk`) come from the compile
    /// nodes, not the Debian mirror.
    ///
    /// A `true` here is a resolve-time requirement on the *SoC*: the resolved SoC
    /// must provide those sources, else resolution fails with
    /// [`ConfigError::FeatureRequiresMediaAccel`]. It is also the build-plan signal —
    /// a build with no such feature carries no sources and skips the userspace/ffmpeg
    /// nodes entirely. Default `false`: most features (an app like `jellyfin`, a
    /// mirror-only add-in) need no source build.
    #[serde(default)]
    pub requires_media_accel: bool,
}

impl Feature {
    /// True when this feature can run on `soc`: its `requires_soc` is empty (any)
    /// or contains `soc`.
    pub fn supports_soc(&self, soc: Soc) -> bool {
        self.requires_soc.is_empty() || self.requires_soc.contains(&soc)
    }

    /// True when this feature can run on `arch`: its `requires_arch` is empty
    /// (any) or contains `arch`.
    pub fn supports_arch(&self, arch: Arch) -> bool {
        self.requires_arch.is_empty() || self.requires_arch.contains(&arch)
    }

    /// [`supports_soc`](Feature::supports_soc) as a hard gate: a feature whose
    /// `requires_soc` excludes `soc` is a [`ConfigError::IncompatibleFeatureSoc`],
    /// failing an incompatible selection before any build. `name` labels
    /// the feature in the message.
    pub fn ensure_supports_soc(&self, name: &str, soc: Soc) -> Result<(), ConfigError> {
        if self.supports_soc(soc) {
            Ok(())
        } else {
            Err(ConfigError::IncompatibleFeatureSoc {
                feature: name.to_string(),
                soc: soc.to_string(),
                supported: self
                    .requires_soc
                    .iter()
                    .map(|s| s.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            })
        }
    }

    /// [`supports_arch`](Feature::supports_arch) as a hard gate: a feature whose
    /// `requires_arch` excludes `arch` is a
    /// [`ConfigError::IncompatibleFeatureArch`]. `name` labels the feature.
    pub fn ensure_supports_arch(&self, name: &str, arch: Arch) -> Result<(), ConfigError> {
        if self.supports_arch(arch) {
            Ok(())
        } else {
            Err(ConfigError::IncompatibleFeatureArch {
                feature: name.to_string(),
                arch: arch.to_string(),
                supported: self
                    .requires_arch
                    .iter()
                    .map(|a| a.to_string())
                    .collect::<Vec<_>>()
                    .join(", "),
            })
        }
    }
}

/// The first selected feature (in recipe order) that declares
/// [`requires_media_accel`](Feature::requires_media_accel), or `None` when none
/// do. `Some` means the build compiles the SoC's media-accel source trees;
/// resolution then requires the SoC to provide them, and the build schedules the
/// userspace/ffmpeg compile nodes. Returning the *name* lets the resolve error
/// point at the specific feature that imposed the requirement.
pub fn first_requiring_media_accel(selected: &[(String, Feature)]) -> Option<&str> {
    selected
        .iter()
        .find(|(_, f)| f.requires_media_accel)
        .map(|(name, _)| name.as_str())
}

/// Validate a selected feature set for pairwise conflicts.
///
/// `selected` pairs each chosen feature's name with its loaded manifest. Returns
/// [`ConfigError::ConflictingFeatures`] for the first pair where either feature
/// names the other in its `conflicts`; the check is symmetric, so declaring the
/// conflict on one side suffices.
pub fn ensure_no_conflicts(selected: &[(String, Feature)]) -> Result<(), ConfigError> {
    for (i, (a_name, a)) in selected.iter().enumerate() {
        for (b_name, b) in &selected[i + 1..] {
            if a.conflicts.contains(b_name) || b.conflicts.contains(a_name) {
                return Err(ConfigError::ConflictingFeatures {
                    feature: a_name.clone(),
                    conflicts_with: b_name.clone(),
                });
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feat(requires_soc: Vec<Soc>, conflicts: Vec<&str>) -> Feature {
        Feature {
            description: "test".into(),
            packages: vec!["pkg".into()],
            exclude: vec![],
            requires_soc,
            requires_arch: vec![],
            apt_sources: vec![],
            extra_debs: vec![],
            conflicts: conflicts.into_iter().map(String::from).collect(),
            requires_media_accel: false,
        }
    }

    #[test]
    fn parses_manifest_toml() {
        let text = r#"
            description  = "Rockchip HW video transcode"
            packages     = ["ffmpeg-rk", "librockchip-mpp1", "librga2"]
            requires_soc = ["rk3588", "rk3576", "rk3566"]
        "#;
        let f: Feature = toml::from_str(text).unwrap();
        assert_eq!(f.packages, vec!["ffmpeg-rk", "librockchip-mpp1", "librga2"]);
        assert_eq!(f.requires_soc, vec![Soc::Rk3588, Soc::Rk3576, Soc::Rk3566]);
        assert!(f.requires_arch.is_empty());
        assert!(f.apt_sources.is_empty());
        assert!(f.conflicts.is_empty());
    }

    #[test]
    fn parses_app_feature_with_apt_source() {
        // An application feature: portable (no HW gate) with a third-party repo.
        let text = r#"
            description = "Jellyfin media server"
            packages    = ["jellyfin"]

            [[apt_sources]]
            name       = "jellyfin"
            uri        = "https://repo.jellyfin.org/debian"
            suite      = "trixie"
            components  = ["main"]
            signed_by   = "jellyfin.gpg"
        "#;
        let f: Feature = toml::from_str(text).unwrap();
        assert!(f.requires_soc.is_empty() && f.requires_arch.is_empty());
        assert_eq!(f.apt_sources.len(), 1);
        assert_eq!(f.apt_sources[0].name, "jellyfin");
        assert_eq!(f.apt_sources[0].components, vec!["main"]);
        // Portable: passes both gates on any target.
        assert!(f.supports_soc(Soc::Rk3588) && f.supports_arch(Arch::Riscv64));
    }

    #[test]
    fn apt_source_rejects_unknown_field() {
        let text = "description = \"x\"\n\
            [[apt_sources]]\nname=\"j\"\nuri=\"u\"\nsuite=\"s\"\ncomponents=[\"main\"]\n\
            signed_by=\"k.gpg\"\nbogus=1\n";
        assert!(toml::from_str::<Feature>(text).is_err());
    }

    #[test]
    fn requires_arch_gates_unlisted_arch() {
        let mut f = feat(vec![], vec![]);
        f.requires_arch = vec![Arch::Arm64];
        assert!(f.supports_arch(Arch::Arm64));
        assert!(!f.supports_arch(Arch::Riscv64));
        assert!(f.ensure_supports_arch("media-accel-rockchip", Arch::Arm64).is_ok());
        let err = f.ensure_supports_arch("some-x86-feature", Arch::Riscv64).unwrap_err();
        assert!(matches!(err, ConfigError::IncompatibleFeatureArch { .. }));
    }

    #[test]
    fn unknown_field_is_rejected() {
        let text = "description = \"x\"\nbogus = 1\n";
        assert!(toml::from_str::<Feature>(text).is_err());
    }

    #[test]
    fn empty_requires_soc_supports_any() {
        let f = feat(vec![], vec![]);
        assert!(f.supports_soc(Soc::Rk3588));
        assert!(f.supports_soc(Soc::Rk3288));
        assert!(f.ensure_supports_soc("any", Soc::Rk3288).is_ok());
    }

    #[test]
    fn requires_soc_gates_unlisted_soc() {
        let f = feat(vec![Soc::Rk3588, Soc::Rk3576], vec![]);
        assert!(f.supports_soc(Soc::Rk3588));
        assert!(!f.supports_soc(Soc::Rk3288));
        let err = f.ensure_supports_soc("media-accel-rockchip", Soc::Rk3288).unwrap_err();
        assert!(matches!(err, ConfigError::IncompatibleFeatureSoc { .. }));
    }

    #[test]
    fn conflicts_are_detected_symmetrically() {
        // Only npu-rocket declares the conflict; npu-rknn need not.
        let rocket = ("npu-rocket".to_string(), feat(vec![], vec!["npu-rknn"]));
        let rknn = ("npu-rknn".to_string(), feat(vec![], vec![]));
        let err = ensure_no_conflicts(&[rocket.clone(), rknn.clone()]).unwrap_err();
        assert!(matches!(err, ConfigError::ConflictingFeatures { .. }));
        // Order-independent.
        assert!(ensure_no_conflicts(&[rknn, rocket]).is_err());
    }

    #[test]
    fn compatible_set_passes() {
        let a = ("media-accel-rockchip".to_string(), feat(vec![Soc::Rk3588], vec![]));
        let b = ("crypto-accel".to_string(), feat(vec![], vec![]));
        assert!(ensure_no_conflicts(&[a, b]).is_ok());
    }

    #[test]
    fn requires_media_accel_defaults_false_and_parses() {
        // Absent key → false (an app/mirror feature needs no source build).
        let plain: Feature = toml::from_str("description = \"x\"\npackages = [\"p\"]\n").unwrap();
        assert!(!plain.requires_media_accel);
        // A provider feature opts in explicitly.
        let provider: Feature =
            toml::from_str("description = \"x\"\nrequires_media_accel = true\n").unwrap();
        assert!(provider.requires_media_accel);
    }

    #[test]
    fn first_requiring_media_accel_names_the_feature() {
        let plain = feat(vec![], vec![]);
        let mut provider = feat(vec![Soc::Rk3588], vec![]);
        provider.requires_media_accel = true;
        // None when no feature opts in.
        let none = [("jellyfin".to_string(), plain.clone())];
        assert_eq!(first_requiring_media_accel(&none), None);
        // The requiring feature's name is returned, in recipe order.
        let set = [
            ("jellyfin".to_string(), plain),
            ("media-accel-rockchip".to_string(), provider),
        ];
        assert_eq!(first_requiring_media_accel(&set), Some("media-accel-rockchip"));
    }
}
