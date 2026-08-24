//! Composable feature model — a rootfs "add-in": a `features/<name>.toml`
//! manifest plus, by convention, a sibling `features/<name>/overlay/` tree of
//! config files.
//!
//! The rootfs feature axis is a *list* of these, stacked onto the layered
//! substrate — `base ⊕ soc ⊕ boot-method ⊕ device ⊕ Σ features`. A feature
//! declares the Debian packages it adds; its overlay tree carries the config it
//! lays into the rootfs, in the same manifest-plus-ordered-files spirit as a
//! patch series.
//!
//! Pure: parsing plus compatibility checks — the SoC/arch gates, pairwise
//! conflicts, and capability requirements. The last two are the *composition*
//! gates, and they are opposites: `conflicts` rejects a selection holding two
//! features that cannot coexist, while
//! [`requires_capability`](Feature::requires_capability) rejects one missing a
//! feature it needs. Both validate what the recipe named; neither adds to it.
//!
//! A feature reaches the kernel as well as the rootfs. Alongside its packages,
//! overlay, and third-party apt sources, it may contribute
//! [`config_fragments`](Feature::config_fragments) and
//! [`patch_series`](Feature::patch_series) — because a capability is often not
//! purely userspace. A hardware-accel provider whose driver is out-of-tree has to
//! patch and configure the kernel to exist at all, and pinning that on the kernel
//! or device layer would force it on every build of that SoC or board, including
//! ones that never selected the capability. Contributing it from the feature keeps
//! the opt-in and the thing opted into in one place.

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
    pub packages: Vec<crate::model::PackageEntry>,
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
    /// Capabilities this feature supplies to the rest of the selection — free-form
    /// names such as `ffmpeg`, matched literally against other features'
    /// [`requires_capability`](Feature::requires_capability).
    ///
    /// Set on a *provider*: both `media-accel-rockchip` and `media-accel-v4l2`
    /// build an `ffmpeg-rk` `.deb`, so both declare `provides = ["ffmpeg"]`. The
    /// point of naming a capability rather than the providers is that a consumer
    /// does not enumerate them — a new provider for another platform declares the
    /// same capability and every consumer accepts it unchanged.
    #[serde(default)]
    pub provides: Vec<String>,
    /// Capabilities another selected feature must [`provides`](Feature::provides),
    /// or resolution fails with [`ConfigError::MissingCapability`].
    ///
    /// Set on a *consumer* whose packages are useless without a sibling's: `jellyfin`
    /// installs no FFmpeg and Jellyfin exits at startup rather than running with
    /// transcoding disabled, so `jellyfin` alone is a bootable image with a dead
    /// service. The gate turns that into a resolve-time error.
    ///
    /// This validates a composition; it does not complete one. Nothing is added to
    /// the selection to satisfy a requirement — the recipe still names every
    /// feature explicitly (non-goal: no provider auto-resolution).
    #[serde(default)]
    pub requires_capability: Vec<String>,
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
    /// Build the FFmpeg provider under `--enable-nonfree`, admitting the encoders
    /// whose licences FFmpeg cannot combine with the GPL — a class, of which FDK-AAC
    /// is the member this tree has a use for.
    ///
    /// It is a *licence* gate rather than a library switch, and the distinction is
    /// FFmpeg's own: `./configure` refuses a GPL build linking such an encoder unless
    /// the flag is present, so the flag and the encoders it admits move together in
    /// both directions. Setting it makes the resulting binary undistributable — the
    /// combination may be built and used, not passed on — which is why it is opt-in
    /// per build rather than a property of the provider.
    ///
    /// Default `false`: every build produces a redistributable FFmpeg. The feature
    /// that sets it installs no packages of its own and declares
    /// `requires_capability = ["ffmpeg"]`, so selecting it without a provider to
    /// re-flavour is a resolution error rather than a silent no-op.
    ///
    /// Reaches the build as [`ResolvedImage::ffmpeg_nonfree`](crate::model::ResolvedImage::ffmpeg_nonfree),
    /// which decides both the `./configure` flags and the build root's package set —
    /// so the free build does not carry the nonfree encoder's headers either.
    #[serde(default)]
    pub ffmpeg_nonfree: bool,
    /// Kconfig fragments this feature merges into the kernel build, by fragment
    /// path (`accel/rk3576-rga`), appended after the kernel's own and the device's
    /// — so a feature's value wins a conflict, matching the way its packages stack
    /// last in the rootfs merge.
    ///
    /// For a capability whose driver is not in the base kernel: the fragment
    /// compiles it, and lives here rather than on the kernel layer so a build that
    /// did not select the capability does not carry the driver.
    ///
    /// Requires a *compiled* kernel. A distro-package kernel merges no kconfig, so
    /// selecting such a feature against one is a
    /// [`ConfigError::FeatureNeedsCompiledKernel`] rather than a value silently
    /// ignored.
    #[serde(default)]
    pub config_fragments: Vec<String>,
    /// Kernel patch series this feature adds, by name (`rk3576-rga`), appended
    /// after the kernel's `patch_series` and the device's `device_patch_series`
    /// and resolved from the same `patches` checkout at the same pin.
    ///
    /// The patch-series half of [`config_fragments`](Feature::config_fragments): a
    /// fragment can only turn on code the tree contains, so a feature carrying an
    /// out-of-tree driver supplies both — the series that adds the source and the
    /// fragment that compiles it.
    ///
    /// Same compiled-kernel requirement, and the same error, as
    /// [`config_fragments`](Feature::config_fragments).
    #[serde(default)]
    pub patch_series: Vec<String>,
    /// What this feature does *not* deliver, in the operator's terms — one sentence
    /// per limitation, tagged [`CaveatScope::Feature`](crate::model::CaveatScope::Feature)
    /// in the resolved build.
    ///
    /// A capability's limits belong to the capability, not to whichever recipe
    /// happened to name it first: every recipe composing the feature inherits them,
    /// and a limit stated once cannot fall out of step across recipes that all have
    /// it. Reserve the recipe's own `[support].caveats` for what is true of that
    /// build point alone.
    ///
    /// Ordered after the hardware's caveats and before the recipe's, and
    /// de-duplicated by text against both — a feature restating a SoC limitation
    /// keeps the SoC's wider tag.
    #[serde(default)]
    pub caveats: Vec<String>,
    /// Runtime checks images selecting this feature must pass (`[[expect]]`),
    /// compiled into `/etc/boot2deb/selftest.d/` for `boot2deb-selftest`. A
    /// capability's proof belongs to the capability — the render node its stack
    /// opens, the misc device its out-of-tree driver presents — so every recipe
    /// composing the feature inherits the check without restating it. The
    /// mechanically checkable counterpart of [`caveats`](Self::caveats).
    #[serde(default)]
    pub expect: Vec<crate::expect::Expectation>,
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

/// The first selected feature (in recipe order) that declares
/// [`ffmpeg_nonfree`](Feature::ffmpeg_nonfree), or `None` when none do.
///
/// `Some` means the build's FFmpeg is configured `--enable-nonfree` and is therefore
/// undistributable. The *name* is returned rather than a bare bool so a caller
/// rejecting the combination — a recipe claiming support for a build it may not ship
/// — can point at the feature that made it so.
pub fn first_enabling_ffmpeg_nonfree(selected: &[(String, Feature)]) -> Option<&str> {
    selected
        .iter()
        .find(|(_, f)| f.ffmpeg_nonfree)
        .map(|(name, _)| name.as_str())
}

/// The kconfig fragments and kernel patch series a selected feature set
/// contributes, as `(config_fragments, patch_series)`.
///
/// Both lists follow recipe selection order, and each is de-duplicated keeping the
/// first occurrence: two features naming the same series express one requirement,
/// and applying a series twice would fail the second time. The caller appends these
/// after the kernel's and the device's, so a feature is the last word.
pub fn kernel_contributions(selected: &[(String, Feature)]) -> (Vec<String>, Vec<String>) {
    fn dedup<'a>(items: impl Iterator<Item = &'a String>) -> Vec<String> {
        let mut seen = std::collections::HashSet::new();
        items
            .filter(|s| seen.insert((*s).clone()))
            .cloned()
            .collect()
    }
    (
        dedup(selected.iter().flat_map(|(_, f)| &f.config_fragments)),
        dedup(selected.iter().flat_map(|(_, f)| &f.patch_series)),
    )
}

/// The first selected feature (in recipe order) that contributes a kernel input,
/// paired with the field name it used — `None` when the set is rootfs-only.
///
/// Only a compiled kernel can act on either field, so this is what lets resolution
/// name the offending feature *and* field when the resolved kernel is a distro
/// package.
pub fn first_contributing_kernel_input(
    selected: &[(String, Feature)],
) -> Option<(&str, &'static str)> {
    selected.iter().find_map(|(name, f)| {
        if !f.config_fragments.is_empty() {
            Some((name.as_str(), "config_fragments"))
        } else if !f.patch_series.is_empty() {
            Some((name.as_str(), "patch_series"))
        } else {
            None
        }
    })
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

/// Validate that every capability a selected feature requires is provided by one.
///
/// `selected` pairs each chosen feature's name with its loaded manifest. Returns
/// [`ConfigError::MissingCapability`] for the first requirement no selected feature
/// [`provides`](Feature::provides), naming the providers the shipped tree does carry
/// so the message says what to add rather than only what is missing.
///
/// A feature may satisfy its own requirement; that is a provider that also consumes
/// what it supplies, and it needs no special case.
pub fn ensure_capabilities_satisfied(
    selected: &[(String, Feature)],
    known_providers: &[(String, Vec<String>)],
) -> Result<(), ConfigError> {
    for (name, f) in selected {
        for capability in &f.requires_capability {
            if selected
                .iter()
                .any(|(_, o)| o.provides.contains(capability))
            {
                continue;
            }
            // Every feature in the tree that would satisfy this, so the error can
            // name the fix. Sorted for a stable message.
            let mut providers: Vec<String> = known_providers
                .iter()
                .filter(|(_, provides)| provides.contains(capability))
                .map(|(n, _)| n.clone())
                .collect();
            providers.sort();
            return Err(ConfigError::MissingCapability {
                feature: name.clone(),
                capability: capability.clone(),
                providers,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// The names of a package list, for an assertion that is about which packages a
    /// layer contributes rather than about how each entry was spelled.
    fn names(entries: &[crate::model::PackageEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.name()).collect()
    }

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
            provides: vec![],
            requires_capability: vec![],
            caveats: vec![],
            requires_media_accel: false,
            ffmpeg_nonfree: false,
            config_fragments: vec![],
            patch_series: vec![],
            expect: vec![],
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
        assert_eq!(
            names(&f.packages),
            ["ffmpeg-rk", "librockchip-mpp1", "librga2"]
        );
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
        assert!(f
            .ensure_supports_arch("media-accel-rockchip", Arch::Arm64)
            .is_ok());
        let err = f
            .ensure_supports_arch("some-x86-feature", Arch::Riscv64)
            .unwrap_err();
        assert!(matches!(err, ConfigError::IncompatibleFeatureArch { .. }));
    }

    /// A `(name, feature)` pair declaring capabilities, for the gate tests below.
    fn cap(name: &str, provides: &[&str], requires: &[&str]) -> (String, Feature) {
        let mut f = feat(vec![], vec![]);
        f.provides = provides.iter().map(|s| s.to_string()).collect();
        f.requires_capability = requires.iter().map(|s| s.to_string()).collect();
        (name.to_string(), f)
    }

    #[test]
    fn a_required_capability_is_satisfied_by_any_provider() {
        // The point of naming a capability rather than a provider: the consumer is
        // unchanged across providers, so both of these compositions pass with the
        // same `requires_capability = ["ffmpeg"]`.
        let rockchip = cap("media-accel-rockchip", &["ffmpeg"], &[]);
        let v4l2 = cap("media-accel-v4l2", &["ffmpeg"], &[]);
        let jellyfin = cap("jellyfin", &[], &["ffmpeg"]);
        let known = vec![
            (
                "media-accel-rockchip".to_string(),
                vec!["ffmpeg".to_string()],
            ),
            ("media-accel-v4l2".to_string(), vec!["ffmpeg".to_string()]),
        ];

        assert!(
            ensure_capabilities_satisfied(&[jellyfin.clone(), rockchip], &known).is_ok(),
            "the RK3588 provider satisfies it"
        );
        assert!(
            ensure_capabilities_satisfied(&[jellyfin.clone(), v4l2], &known).is_ok(),
            "so does the RK3576 one, with no change to the consumer"
        );

        // Alone it fails, and the message has to name the fix — a user who has not
        // read the feature tree cannot know which features carry the capability.
        let err = ensure_capabilities_satisfied(&[jellyfin], &known).unwrap_err();
        let ConfigError::MissingCapability {
            ref feature,
            ref capability,
            ref providers,
        } = err
        else {
            panic!("expected MissingCapability, got {err:?}");
        };
        assert_eq!(feature, "jellyfin");
        assert_eq!(capability, "ffmpeg");
        assert_eq!(providers, &["media-accel-rockchip", "media-accel-v4l2"]);
        let rendered = err.to_string();
        assert!(
            rendered.contains("add one of 'media-accel-rockchip', 'media-accel-v4l2'"),
            "unhelpful message: {rendered}"
        );
    }

    #[test]
    fn a_capability_no_feature_provides_points_at_the_listing() {
        // Distinct from a forgotten provider: this is a misspelled requirement or one
        // whose provider was never authored, so there is nothing to suggest adding.
        let consumer = cap("consumer", &[], &["ffmpgе"]);
        let err = ensure_capabilities_satisfied(&[consumer], &[]).unwrap_err();
        let rendered = err.to_string();
        assert!(
            rendered.contains("no feature in this config tree provides it"),
            "should not invite adding a provider that does not exist: {rendered}"
        );
        assert!(rendered.contains("list-features"));
    }

    #[test]
    fn a_feature_may_satisfy_its_own_requirement() {
        // A provider that also consumes what it supplies needs no special case, and
        // an implementation scanning "the others" rather than the whole selection
        // would reject this.
        let both = cap("self-sufficient", &["ffmpeg"], &["ffmpeg"]);
        assert!(ensure_capabilities_satisfied(&[both], &[]).is_ok());
    }

    #[test]
    fn capability_fields_default_to_empty() {
        // Every existing feature manifest omits both, so the gate has to be inert
        // unless something opts in.
        let f: Feature = toml::from_str("description = \"x\"\n").unwrap();
        assert!(f.provides.is_empty());
        assert!(f.requires_capability.is_empty());
        assert!(ensure_capabilities_satisfied(&[("x".into(), f)], &[]).is_ok());
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
        let err = f
            .ensure_supports_soc("media-accel-rockchip", Soc::Rk3288)
            .unwrap_err();
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
        let a = (
            "media-accel-rockchip".to_string(),
            feat(vec![Soc::Rk3588], vec![]),
        );
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
    fn ffmpeg_nonfree_defaults_false_and_names_the_feature_that_sets_it() {
        // Absent key → free. Every feature manifest but one omits it, and the default
        // decides what every image ships, so it is the half worth pinning in a test.
        let plain: Feature = toml::from_str("description = \"x\"\n").unwrap();
        assert!(!plain.ffmpeg_nonfree);
        let none = [("media-accel-rockchip".to_string(), feat(vec![], vec![]))];
        assert_eq!(first_enabling_ffmpeg_nonfree(&none), None);

        let mut nonfree = feat(vec![], vec![]);
        nonfree.ffmpeg_nonfree = true;
        let set = [
            ("media-accel-rockchip".to_string(), feat(vec![], vec![])),
            ("ffmpeg-nonfree".to_string(), nonfree),
        ];
        assert_eq!(first_enabling_ffmpeg_nonfree(&set), Some("ffmpeg-nonfree"));
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
        assert_eq!(
            first_requiring_media_accel(&set),
            Some("media-accel-rockchip")
        );
    }
}
