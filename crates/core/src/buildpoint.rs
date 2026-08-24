//! Build points — a recipe plus the feature selection a build was asked for.
//!
//! A [`Recipe`](crate::model::Recipe) names one curated point across the build axes
//! and carries a support claim. The feature axis, though, is a *list*, and the set a
//! caller wants is often not one anybody curated: "the shipped H96 image, plus
//! transcode, plus Jellyfin" is a legal selection with no recipe behind it. A
//! [`BuildPoint`] is that pairing — the recipe supplying every other axis, and the
//! features replacing the recipe's own list.
//!
//! The pairing has a canonical textual form, its **reference**:
//!
//! ```text
//! turing-rk1/forky                              the recipe as authored
//! turing-rk1/forky+jellyfin                      that recipe, features replaced
//! turing-rk1/forky+media-accel-rockchip+jellyfin  ... with two, in this order
//! ```
//!
//! A point that selects no features of its own *is* its recipe: the reference is the
//! recipe name unchanged, so every existing lock, work directory, and artifact path
//! keeps the name it already has. Only a variant grows the suffix, and it grows one
//! everywhere at once — the lock, the solved package manifest, and the build
//! directory all key off the reference, so two selections cannot collide.
//!
//! **Order is preserved, not sorted.** Feature order is significant in resolution:
//! `config_fragments` and `patch_series` compose in selection order, so a later
//! feature's value wins a kconfig conflict. Sorting the reference would give two
//! materially different builds one identity. Two orderings of the same set are
//! therefore two references, which is the honest answer. A repeated feature is
//! rejected outright rather than silently folded, since it can only be a mistake.
//!
//! Pure: parsing and validation only, no filesystem access.

use crate::error::ConfigError;

/// The separator between the recipe and each feature in a [reference](BuildPoint::reference).
///
/// `+` is deliberately outside the bare-identifier alphabet (`[A-Za-z0-9._-]`), so no
/// recipe or feature name can contain one and the split is unambiguous.
pub const FEATURE_SEP: char = '+';

/// A recipe paired with the feature selection to build it with.
///
/// Construct with [`new`](BuildPoint::new) from a recipe name and a (possibly empty)
/// feature list, or with [`parse`](BuildPoint::parse) from a
/// [reference](BuildPoint::reference). Both validate; an instance is always
/// well-formed, so [`reference`](BuildPoint::reference) round-trips through `parse`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildPoint {
    recipe: String,
    features: Vec<String>,
}

impl BuildPoint {
    /// Pair `recipe` with `features`.
    ///
    /// `recipe` is held to the recipe-reference rule (at most one interior `/`, both
    /// halves bare identifiers) and each feature to the bare-identifier rule, because
    /// the resulting [reference](Self::reference) is joined into filesystem paths.
    /// A repeated feature is [`ConfigError::DuplicateFeature`].
    pub fn new(
        recipe: impl Into<String>,
        features: impl IntoIterator<Item = String>,
    ) -> Result<Self, ConfigError> {
        let recipe = recipe.into();
        crate::loader::check_recipe_ref(&recipe)?;
        let features: Vec<String> = features.into_iter().collect();
        let mut seen = std::collections::HashSet::new();
        for feature in &features {
            crate::loader::check_feature_name(feature)?;
            if !seen.insert(feature.as_str()) {
                return Err(ConfigError::DuplicateFeature {
                    feature: feature.clone(),
                });
            }
        }
        Ok(Self { recipe, features })
    }

    /// Parse a [reference](Self::reference) — `<recipe>` or
    /// `<recipe>+<feature>[+<feature>...]`.
    ///
    /// The inverse of [`reference`](Self::reference): a reference produced by this
    /// type always parses back to an equal value. Every command that takes a recipe
    /// accepts a reference, so a variant built by `build --feature` can afterwards be
    /// inspected by name (`why-rebuild turing-rk1/forky+jellyfin`).
    pub fn parse(reference: &str) -> Result<Self, ConfigError> {
        let mut parts = reference.split(FEATURE_SEP);
        let recipe = parts.next().unwrap_or_default();
        Self::new(recipe, parts.map(str::to_string))
    }

    /// The recipe this point starts from — the one whose `.toml` supplies every axis
    /// but the features.
    pub fn recipe(&self) -> &str {
        &self.recipe
    }

    /// The selected features, in selection order. Empty when the recipe's own
    /// `features` list stands unreplaced.
    pub fn features(&self) -> &[String] {
        &self.features
    }

    /// Whether this point replaces the recipe's feature list.
    ///
    /// A variant has no `[support]` claim of its own — the claim belongs to the
    /// recipe, and a different feature set is a different build — so callers that
    /// report or record support state on it should say so rather than inherit.
    pub fn is_variant(&self) -> bool {
        !self.features.is_empty()
    }

    /// The canonical textual form, and the name every derived path uses: the lock,
    /// the solved package manifest, and the build work directory.
    ///
    /// Equal to [`recipe`](Self::recipe) when no features are selected, so an
    /// ordinary recipe build is untouched by this type existing.
    pub fn reference(&self) -> String {
        if self.features.is_empty() {
            self.recipe.clone()
        } else {
            format!(
                "{}{FEATURE_SEP}{}",
                self.recipe,
                self.features.join(&FEATURE_SEP.to_string())
            )
        }
    }

    /// The feature selection as an [`Overrides`](crate::model::Overrides) value:
    /// `Some(list)` for a variant, `None` when the recipe's own list stands.
    pub fn feature_override(&self) -> Option<Vec<String>> {
        (!self.features.is_empty()).then(|| self.features.clone())
    }

    /// The stem every artifact this point publishes is named for — the whole
    /// [reference](Self::reference) with its `/` flattened:
    /// `turing-rk1/media-accel-forky` → `turing-rk1-media-accel-forky`.
    ///
    /// Artifacts land in one flat output directory, so the separator has to go — but
    /// the *device* half cannot go with it. A recipe leaf is unique only within its
    /// device folder, and the leaves that repeat are exactly the ones a reader most
    /// needs told apart: `forky.img` names both `asus-c201/forky` (Debian's armmp
    /// kernel) and `turing-rk1/forky`, and sits one letter from
    /// `asus-c201/mainline-forky`. An image outlives the directory it was built in —
    /// it gets copied to a flashing host, kept beside three others — so its name has to
    /// carry the whole point on its own.
    ///
    /// **Every artifact the build point owns is named for this**, `.deb`s aside: the
    /// image, the rootfs tarball, the boot payloads, the solved package manifest, the
    /// provenance document. That is what lets several recipes share one `--out-dir`
    /// without one silently folding another's bootloader or rootfs into its image.
    /// (`.deb`s carry a package name and version instead, and are scoped by the output
    /// dir's artifact ledger.)
    ///
    /// A variant keeps its `+feature` suffixes, so a selection never publishes over the
    /// recipe it starts from.
    pub fn artifact_stem(&self) -> String {
        self.reference().replace('/', "-")
    }
}

impl std::fmt::Display for BuildPoint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.reference())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_point_with_no_features_is_its_recipe() {
        // The compatibility contract: introducing this type must not move a single
        // existing lock, manifest, or build directory, all of which key off the
        // reference. So a featureless point's reference is the bare recipe name.
        let point = BuildPoint::new("turing-rk1/forky", []).unwrap();
        assert_eq!(point.reference(), "turing-rk1/forky");
        assert!(!point.is_variant());
        assert_eq!(point.feature_override(), None);
    }

    #[test]
    fn a_reference_round_trips_through_parse() {
        for reference in [
            "turing-rk1/forky",
            "turing-rk1/forky+jellyfin",
            "h96-max-m9/forky+media-accel-v4l2+jellyfin",
            "bare-leaf+one",
        ] {
            let point = BuildPoint::parse(reference).unwrap();
            assert_eq!(point.reference(), reference);
            assert_eq!(BuildPoint::parse(&point.reference()).unwrap(), point);
        }
    }

    #[test]
    fn feature_order_is_preserved_because_resolution_honours_it() {
        // Two orderings of one set are two different builds: `config_fragments` and
        // `patch_series` compose in selection order, so the later feature wins a
        // kconfig conflict. Canonicalizing by sorting would give both the same lock
        // path and let one silently serve the other.
        let a = BuildPoint::parse("d/r+alpha+beta").unwrap();
        let b = BuildPoint::parse("d/r+beta+alpha").unwrap();
        assert_ne!(a.reference(), b.reference());
        assert_eq!(a.features(), ["alpha", "beta"]);
        assert_eq!(b.features(), ["beta", "alpha"]);
    }

    #[test]
    fn a_repeated_feature_is_an_error_not_a_silent_fold() {
        // Selecting a feature twice cannot mean anything: it contributes its packages
        // and fragments once either way. Folding it would make the reference disagree
        // with what was typed, so it is rejected at the point of construction.
        let err = BuildPoint::parse("d/r+jellyfin+jellyfin").unwrap_err();
        assert!(
            err.to_string().contains("jellyfin"),
            "the error names the feature: {err}"
        );
    }

    #[test]
    fn the_artifact_stem_names_the_whole_point_not_just_its_leaf() {
        // The stem travels with the file, so it carries the device as well as the
        // recipe — and a variant carries its features, or a `--feature` build would
        // publish over the recipe it started from.
        for (reference, stem) in [
            ("turing-rk1/forky", "turing-rk1-forky"),
            (
                "turing-rk1/media-accel-forky",
                "turing-rk1-media-accel-forky",
            ),
            ("turing-rk1/forky+jellyfin", "turing-rk1-forky+jellyfin"),
            ("h96-max-m9/util", "h96-max-m9-util"),
            ("bare-leaf", "bare-leaf"),
        ] {
            assert_eq!(BuildPoint::parse(reference).unwrap().artifact_stem(), stem);
        }
        // Two boards' `forky` recipes are the case the leaf alone could not tell
        // apart, and the reason the device half stays.
        assert_ne!(
            BuildPoint::parse("asus-c201/forky")
                .unwrap()
                .artifact_stem(),
            BuildPoint::parse("turing-rk1/forky")
                .unwrap()
                .artifact_stem()
        );
        // The stem is a bare file-name component: it is joined into artifact names, so
        // it must never reintroduce the separator the reference had.
        assert!(!BuildPoint::parse("turing-rk1/forky")
            .unwrap()
            .artifact_stem()
            .contains('/'));
    }

    #[test]
    fn a_reference_cannot_traverse_out_of_recipes() {
        // The reference is joined into `recipes/<ref>.lock` and `build/<ref>`, so
        // every segment is held to the bare-identifier rule the recipe reference
        // already enforced — the feature suffix does not open a new path.
        for bad in [
            "d/r+../escape",
            "d/r+/abs",
            "d/r+.",
            "d/r+..",
            "d/r+",
            "../d/r+f",
            "d/r+a/b",
        ] {
            assert!(
                BuildPoint::parse(bad).is_err(),
                "{bad} must not parse into a path"
            );
        }
    }
}
