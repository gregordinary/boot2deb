//! How an upstream tag is spelled, and the version read out of it.
//!
//! Pure: string handling over `semver`, no I/O. One place decides what
//! `sources/v7.1.6-gnu`, `v2026.04`, and `v7.2-rc1` mean, because two places
//! deciding it differently is a silent wrong answer — a range gate that admits a
//! tag an upgrade survey calls incomparable, or the reverse.
//!
//! Two questions live here:
//!
//! - **What version is this tag?** [`parse_tag`], used by the patch-series range
//!   gate ([`crate::series`]) to decide whether a series claims a kernel, and by the
//!   upgrade survey ([`crate::outdated`]) to order releases.
//! - **Is this tag spelled like that one?** [`TagShape`], used only by the survey:
//!   a repo advertises tags from several naming schemes at once, and only the ones
//!   spelled like the pin are candidates to upgrade it to.

use crate::error::ConfigError;
use semver::Version;

/// The suffix GNU Linux-libre appends to the kernel version it deblobs — its
/// `EXTRAVERSION`, and the tail of every tag it publishes (`sources/v7.1.6-gnu`).
const LIBRE_SUFFIX: &str = "-gnu";

/// Parse a version tag into a [`Version`], tolerating a namespaced tag
/// (`sources/v7.1.6-gnu` → `7.1.6`), a leading `v`, a missing patch component
/// (`v7.1` → `7.1.0`), zero-padded components (`v2026.04` → `2026.4.0`), and the
/// GNU Linux-libre `-gnu` suffix. Prerelease suffixes (`-rc2`) are preserved as
/// semver prereleases, so a release-only range excludes them and an upgrade survey
/// can decline to offer one.
///
/// Serves every axis: kernel tags (`v7.1.3`), u-boot's `vYYYY.MM` release tags, and
/// the `patches` repo's release tags.
///
/// # Errors
///
/// [`ConfigError::InvalidVersion`] when what remains after normalization is not a
/// semver version — a branch name, say, or a tag naming no version at all.
///
/// ```
/// use boot2deb_core::version::parse_tag;
/// assert_eq!(parse_tag("v7.1.6").unwrap().to_string(), "7.1.6");
/// assert_eq!(parse_tag("v7.1").unwrap().to_string(), "7.1.0");
/// assert_eq!(parse_tag("v2026.04").unwrap().to_string(), "2026.4.0");
/// assert_eq!(parse_tag("sources/v7.1.6-gnu").unwrap().to_string(), "7.1.6");
/// assert!(parse_tag("master").is_err());
/// ```
pub fn parse_tag(s: &str) -> Result<Version, ConfigError> {
    // A tag may live under a namespace — GNU Linux-libre publishes its trees as
    // `refs/tags/sources/v7.1.6-gnu` — and the version is its last segment. The
    // whole ref stays the lock's `reference` (it is what git resolves); only the
    // version read out of it is narrowed.
    let stripped = s.rsplit('/').next().unwrap_or(s);
    let stripped = stripped.strip_prefix('v').unwrap_or(stripped);
    // `-gnu` is a *variant* marker, not a prerelease: linux-libre 7.1.6-gnu **is**
    // 7.1.6 with the nonfree-firmware loaders removed, released after it rather
    // than ahead of it. Left in place, semver would read it as a prerelease of
    // 7.1.6 and exclude it from every range whose bounds name none — so a series
    // declaring `>=7.0, <7.2` would silently decline the deblobbed 7.1.6 it
    // applies to perfectly well.
    let stripped = stripped.strip_suffix(LIBRE_SUFFIX).unwrap_or(stripped);
    let (core, rest) = split_core(stripped);
    let core = normalize_core(core);
    // Pad a two-component core to three, so `v7.1` parses; a core that is already
    // three parts (or one) is left as it is.
    let padded = if core.split('.').count() == 2 {
        format!("{core}.0")
    } else {
        core
    };
    Version::parse(&format!("{padded}{rest}")).map_err(|source| ConfigError::InvalidVersion {
        value: s.to_string(),
        source,
    })
}

/// How a tag is spelled, apart from its numbers: the namespace it sits under, the
/// `v` prefix, and the Linux-libre `-gnu` marker.
///
/// A repo advertises tags from more than one scheme at once — `linux-stable` carries
/// `v7.1.6` beside `v2.6.11`, and the Linux-libre mirror carries `sources/v7.1.6-gnu`
/// beside upstream's own — so an upgrade survey that compared every parseable tag
/// would offer a deblobbed tree to a build that pinned an ordinary one, or the
/// reverse. Comparing only tags spelled like the pin keeps the candidate set to the
/// release line the pin actually came from, without a per-axis table of patterns:
/// the pin states its own scheme.
///
/// The component *count* is deliberately not part of the shape. Upstream drops the
/// patch component on a `.0` release (`v7.2`, then `v7.2.1`), and u-boot publishes
/// the occasional `vYYYY.MM.NN` point release, so requiring the same count would
/// hide exactly the upgrade being looked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TagShape {
    /// Everything before the last `/`, if the tag is namespaced (`sources` in
    /// `sources/v7.1.6-gnu`).
    namespace: Option<String>,
    /// Whether the version is prefixed with `v`.
    v_prefix: bool,
    /// Whether the tag carries the Linux-libre `-gnu` marker.
    libre: bool,
}

impl TagShape {
    /// The shape of `tag`. Total — every string has one, including a branch name,
    /// which simply has no namespace, no `v`, and no `-gnu`.
    ///
    /// ```
    /// use boot2deb_core::version::TagShape;
    /// let stable = TagShape::of("v7.1.6");
    /// assert!(stable.matches("v7.2"));
    /// assert!(!stable.matches("sources/v7.2-gnu"));
    /// ```
    pub fn of(tag: &str) -> TagShape {
        let (namespace, leaf) = match tag.rsplit_once('/') {
            Some((ns, leaf)) => (Some(ns.to_string()), leaf),
            None => (None, tag),
        };
        TagShape {
            namespace,
            v_prefix: leaf.starts_with('v'),
            libre: leaf.ends_with(LIBRE_SUFFIX),
        }
    }

    /// Whether `tag` is spelled this way — same namespace, same `v` prefix, same
    /// `-gnu` marker.
    pub fn matches(&self, tag: &str) -> bool {
        &TagShape::of(tag) == self
    }
}

/// Split a version string at its first prerelease/build delimiter, into the numeric
/// core and the untouched remainder (`""` when there is none).
fn split_core(s: &str) -> (&str, &str) {
    match s.find(['-', '+']) {
        Some(i) => s.split_at(i),
        None => (s, ""),
    }
}

/// Strip leading zeros from each numeric component of a version core.
///
/// u-boot releases as `vYYYY.MM` with a zero-padded month (`v2026.04`), which semver
/// rejects outright — "invalid leading zero in minor version number" — so without
/// this the u-boot axis could be range-matched only by writing its versions in a
/// spelling that appears on no tag. Non-numeric components (a `*` or `x` wildcard in
/// a requirement) are left alone, and an all-zero component collapses to a single
/// `0` rather than to nothing.
fn normalize_core(core: &str) -> String {
    core.split('.')
        .map(|c| {
            if c.is_empty() || !c.bytes().all(|b| b.is_ascii_digit()) {
                return c;
            }
            let trimmed = c.trim_start_matches('0');
            if trimmed.is_empty() {
                "0"
            } else {
                trimmed
            }
        })
        .collect::<Vec<_>>()
        .join(".")
}

/// Apply [`normalize_core`] to every comparator in a comma-separated semver
/// requirement, leaving each comparator's operator prefix intact — so a range may
/// bound u-boot tags in the spelling they actually carry (`">=2026.04, <2027.01"`).
///
/// Used by [`crate::series`] when it parses a declared envelope; the version side of
/// the same normalization is [`parse_tag`].
pub(crate) fn normalize_req(range: &str) -> String {
    range
        .split(',')
        .map(|comparator| {
            let trimmed = comparator.trim();
            let rest = trimmed.trim_start_matches(['=', '>', '<', '^', '~']);
            let (op, version) = trimmed.split_at(trimmed.len() - rest.len());
            let version = version.trim_start();
            let (core, suffix) = split_core(version.strip_prefix('v').unwrap_or(version));
            format!("{op}{}{suffix}", normalize_core(core))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tags_parse_across_every_axis_spelling() {
        // The kernel axis: a `v` prefix, and a `.0` release that states two
        // components rather than three.
        assert_eq!(parse_tag("v7.1.6").unwrap(), Version::new(7, 1, 6));
        assert_eq!(parse_tag("v7.2").unwrap(), Version::new(7, 2, 0));
        // The u-boot axis: `vYYYY.MM` with a zero-padded month, which semver refuses
        // outright until the padding is stripped.
        assert_eq!(parse_tag("v2026.04").unwrap(), Version::new(2026, 4, 0));
        assert_eq!(parse_tag("v2026.04.1").unwrap(), Version::new(2026, 4, 1));
        // Linux-libre: namespaced, and `-gnu` is a variant marker rather than a
        // prerelease — 7.1.6-gnu *is* 7.1.6 with the loaders removed, released after
        // it, so it must not order below 7.1.6.
        assert_eq!(
            parse_tag("sources/v7.1.6-gnu").unwrap(),
            Version::new(7, 1, 6)
        );
        // A real prerelease keeps its suffix, so it orders below the release and a
        // release-only comparison declines it.
        let rc = parse_tag("v7.2-rc1").unwrap();
        assert!(!rc.pre.is_empty());
        assert!(rc < Version::new(7, 2, 0));
        // A name that is not a version at all fails, naming the value.
        let err = parse_tag("mainline-cma-fix").unwrap_err().to_string();
        assert!(err.contains("mainline-cma-fix"), "{err}");
    }

    #[test]
    fn a_shape_admits_its_own_scheme_and_no_other() {
        // The failure this prevents is a survey offering a deblobbed tree to a build
        // that pinned an ordinary kernel: both schemes live in the same repo, both
        // parse, and their versions interleave.
        let stable = TagShape::of("v7.1.6");
        assert!(stable.matches("v7.1.9"));
        // The component count is not part of the shape — a `.0` release states two,
        // and requiring three would hide the very upgrade being looked for.
        assert!(stable.matches("v7.2"));
        assert!(!stable.matches("sources/v7.2-gnu"));
        assert!(!stable.matches("7.1.9"), "the `v` prefix is part of it");

        let libre = TagShape::of("sources/v7.1.6-gnu");
        assert!(libre.matches("sources/v7.1.9-gnu"));
        assert!(!libre.matches("v7.1.9"));
        assert!(!libre.matches("sources/v7.1.9"), "the marker is part of it");

        // Total: a branch name has a shape too, and it matches other bare names.
        let branch = TagShape::of("master");
        assert!(branch.matches("develop"));
        assert!(!branch.matches("v1.0"));
    }

    #[test]
    fn requirement_normalization_leaves_operators_alone() {
        // A range may be authored in the spelling its tags carry; only the numeric
        // core is rewritten, so the comparator survives intact.
        assert_eq!(normalize_req(">=2026.04, <2027.01"), ">=2026.4, <2027.1");
        assert_eq!(normalize_req(">=7.0, <7.2"), ">=7.0, <7.2");
        assert_eq!(normalize_req("=v7.1"), "=7.1");
    }
}
