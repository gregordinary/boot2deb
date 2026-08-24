//! The solved package manifest's text form: one `name version arch sha256` line
//! per installed package, sorted, under a `#` header.
//!
//! Pure — rendering and parsing a text format, no I/O. The engine owns the
//! *production* of a manifest (projecting a resolved provisioner plan, writing the
//! file, hashing it); this module owns what the bytes are, so the writer and every
//! reader agree on one definition of the format rather than on two that happen to
//! match.
//!
//! Every sha256 is the one the signed archive records for that `.deb`, so a manifest
//! pins a package set by content rather than by name — which is what makes
//! [`render`] a stable identity for the set and not for the run that produced it,
//! and what lets two manifests be compared package-for-package
//! ([`diff`](crate::diff)).

use crate::error::ConfigError;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

/// The filename suffix every solved manifest carries: `<stem>.pkgs.lock`, written
/// beside the `<recipe>.lock` that names it.
///
/// Part of the format this module defines, so a writer and a reader cannot disagree
/// about which files are manifests. That distinction is load-bearing beyond parsing:
/// a manifest shares the `.lock` extension with a recipe lock but is not TOML, so a
/// consumer that walks `recipes/` by extension alone reads one as the other.
pub const MANIFEST_SUFFIX: &str = ".pkgs.lock";

/// The manifest filename for a recipe `stem` — the name `update` and `build` write,
/// and the value a lock's [`RootfsPin::manifest`](crate::lock::RootfsPin::manifest)
/// holds.
pub fn manifest_name(stem: &str) -> String {
    format!("{stem}{MANIFEST_SUFFIX}")
}

/// True when `name` is a solved manifest rather than a recipe lock — for a consumer
/// enumerating `recipes/`, where the two share the `.lock` extension.
pub fn is_manifest_name(name: &str) -> bool {
    name.ends_with(MANIFEST_SUFFIX)
}

/// One package in a solved manifest.
///
/// The four fields a manifest line holds, and no more: this is the projected form of
/// an installed package, not a package's full archive metadata.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Package {
    /// Binary package name (`libc6`).
    pub name: String,
    /// Exact Debian version (`2.41-1`), verbatim as the archive records it.
    pub version: String,
    /// Debian architecture (`arm64`, `all`).
    pub architecture: String,
    /// Lowercase-hex sha256 the signed archive records for the `.deb`. Two
    /// manifests agreeing on a name and version but not on this describe different
    /// bytes.
    pub sha256: String,
}

/// Render manifest text: `header` as a leading `#` comment, then one
/// `name version arch sha256` line per package.
///
/// Sorted by the whole row, name first, so one package set renders to one byte
/// sequence regardless of resolution order.
pub fn render(header: &str, packages: &[Package]) -> String {
    let mut sorted: Vec<&Package> = packages.iter().collect();
    sorted.sort();
    let mut body = format!("# {header}\n");
    for p in sorted {
        let _ = writeln!(
            body,
            "{} {} {} {}",
            p.name, p.version, p.architecture, p.sha256
        );
    }
    body
}

/// Parse manifest text back into its package list, in file order.
///
/// Comment and blank lines are skipped. The list is returned as written rather than
/// re-sorted: [`render`] already sorts, so a manifest boot2deb wrote is in canonical
/// order, and preserving what was read keeps a hand-inspected file's order visible
/// in an error.
///
/// # Errors
///
/// [`ConfigError::Parse`] is not used here, because the format is not TOML: a
/// malformed line yields [`ConfigError::InvalidManifest`] naming the file, the line
/// number, and the line — a manifest is a pin, so a line that does not parse must not
/// be silently dropped from the set it pins.
pub fn parse(text: &str, path: &str) -> Result<Vec<Package>, ConfigError> {
    let mut packages = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut fields = line.split_whitespace();
        let (Some(name), Some(version), Some(architecture), Some(sha256), None) = (
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
            fields.next(),
        ) else {
            return Err(ConfigError::InvalidManifest {
                path: path.to_string(),
                line: i + 1,
                content: line.to_string(),
            });
        };
        packages.push(Package {
            name: name.to_string(),
            version: version.to_string(),
            architecture: architecture.to_string(),
            sha256: sha256.to_string(),
        });
    }
    Ok(packages)
}

/// One package's difference between two solved sets — the unit
/// [`moved`] reports.
///
/// A side is `None` when that set does not hold the package at all, so an addition and
/// a removal are the same shape as a version change and need no separate variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moved {
    /// Binary package name, the identity the two sides are matched on.
    pub name: String,
    /// Debian architecture, matched on alongside the name: `libc6:arm64` and
    /// `libc6:armhf` are different packages that a multi-arch set can hold at once.
    pub architecture: String,
    /// What the first set holds, or `None` if it does not hold this package.
    pub before: Option<Package>,
    /// What the second set holds, or `None` if it does not hold this package.
    pub after: Option<Package>,
}

impl Moved {
    /// A one-line summary for a report: `libc6:arm64 2.42-17 -> 2.43-3`, with `(absent)`
    /// standing in for a side that does not hold the package.
    ///
    /// The version is what a reader acts on, so it leads. Two sides at the same version
    /// render the sha256s instead — that pair differs in the `.deb`'s *bytes*, and
    /// printing one version twice would read as no difference at all.
    pub fn describe(&self) -> String {
        let same_version = matches!(
            (&self.before, &self.after),
            (Some(b), Some(a)) if b.version == a.version
        );
        let side = |p: &Option<Package>| match p {
            None => "(absent)".to_string(),
            Some(p) if same_version => format!("{} sha256:{}", p.version, p.sha256),
            Some(p) => p.version.clone(),
        };
        format!(
            "{}:{} {} -> {}",
            self.name,
            self.architecture,
            side(&self.before),
            side(&self.after)
        )
    }
}

/// What differs between two solved package sets, sorted by name then architecture.
///
/// Packages are matched on `name` + `architecture` and compared on the *whole* row, so
/// a set that agrees on every version but records one different sha256 still reports —
/// that pair names different bytes, which is the case a name-and-version comparison
/// would call equal.
///
/// An empty result is the decisive answer that the two sets are the same set. That is
/// what lets a caller treat a recorded manifest as still describing what an archive
/// would give now, rather than merely as having been true once.
pub fn moved(before: &[Package], after: &[Package]) -> Vec<Moved> {
    let key = |p: &Package| (p.name.clone(), p.architecture.clone());
    let index = |set: &[Package]| -> BTreeMap<(String, String), Package> {
        set.iter().map(|p| (key(p), p.clone())).collect()
    };
    let (before, after) = (index(before), index(after));
    before
        .keys()
        .chain(after.keys())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .filter(|k| before.get(*k) != after.get(*k))
        .map(|k| Moved {
            name: k.0.clone(),
            architecture: k.1.clone(),
            before: before.get(k).cloned(),
            after: after.get(k).cloned(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pkg(name: &str, version: &str, arch: &str, sha: &str) -> Package {
        Package {
            name: name.into(),
            version: version.into(),
            architecture: arch.into(),
            sha256: sha.into(),
        }
    }

    /// The rendered text is sorted and carries each package's archive-recorded
    /// sha256, so one package set has one byte sequence.
    #[test]
    fn render_is_sorted_and_pinned() {
        let body = render(
            "Solved rootfs package manifest.",
            &[
                pkg("libc6", "2.41-1", "arm64", "aaaa"),
                pkg("ffmpeg-rk", "3e53143", "arm64", "cccc"),
            ],
        );
        assert!(body.starts_with("# Solved rootfs package manifest.\n"));
        assert_eq!(
            body.lines()
                .filter(|l| !l.starts_with('#'))
                .collect::<Vec<_>>(),
            vec!["ffmpeg-rk 3e53143 arm64 cccc", "libc6 2.41-1 arm64 aaaa"]
        );
    }

    /// Resolution order does not reach the bytes: the same set rendered in either
    /// order is the same file, which is what the manifest digest identifies.
    #[test]
    fn resolution_order_does_not_reach_the_bytes() {
        let a = pkg("libc6", "2.41-1", "arm64", "aaaa");
        let b = pkg("adduser", "3.157", "all", "bbbb");
        let header = "Solved package manifest.";
        assert_eq!(
            render(header, &[a.clone(), b.clone()]),
            render(header, &[b, a])
        );
    }

    /// The header is part of the file, so two manifests describing different trees
    /// never collide on a digest even where their package sets coincide.
    #[test]
    fn the_header_reaches_the_bytes() {
        let rows = [pkg("libc6", "2.41-1", "arm64", "aaaa")];
        assert_ne!(render("rootfs.", &rows), render("build sandbox.", &rows));
    }

    #[test]
    fn a_rendered_manifest_parses_back_to_the_set_that_wrote_it() {
        let packages = vec![
            pkg("adduser", "3.157", "all", "bbbb"),
            pkg("libc6", "2.41-1", "arm64", "aaaa"),
        ];
        let text = render("Solved rootfs package manifest.", &packages);
        assert_eq!(parse(&text, "m.pkgs.lock").unwrap(), packages);
    }

    /// A manifest is a pin, so a line that does not parse is an error naming where
    /// it is — dropping it would silently shrink the set the file claims to pin.
    #[test]
    fn a_malformed_line_names_its_position_rather_than_being_dropped() {
        let text = "# header\nlibc6 2.41-1 arm64 aaaa\nlibc6-dev 2.41-1 arm64\n";
        let err = parse(text, "turing-rk1-forky.pkgs.lock").unwrap_err();
        let msg = err.to_string();
        assert!(msg.starts_with("turing-rk1-forky.pkgs.lock:3:"), "{msg}");
        assert!(msg.contains("libc6-dev"), "{msg}");

        // A fifth field is malformed for the same reason a third is: the format is
        // exactly four, and anything else means the file is not what it claims.
        assert!(parse("a 1 all aa extra\n", "m").is_err());
    }

    #[test]
    fn an_unchanged_set_moved_nothing_in_any_order() {
        let a = [
            pkg("libc6", "2.41-1", "arm64", "aaaa"),
            pkg("bash", "5.2", "arm64", "bbbb"),
        ];
        let b = [
            pkg("bash", "5.2", "arm64", "bbbb"),
            pkg("libc6", "2.41-1", "arm64", "aaaa"),
        ];
        // Empty is the decisive answer, and resolution order must not disturb it —
        // the whole point is to treat a recorded manifest as still current.
        assert!(moved(&a, &b).is_empty());
    }

    #[test]
    fn a_version_change_names_both_sides() {
        // The §5 case: one base carries libc6 2.42-17, a later archive resolves 2.43-3,
        // and nothing but the version distinguishes the two trees.
        let before = [pkg("libc6", "2.42-17", "arm64", "aaaa")];
        let after = [pkg("libc6", "2.43-3", "arm64", "cccc")];
        let m = moved(&before, &after);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].describe(), "libc6:arm64 2.42-17 -> 2.43-3");
    }

    #[test]
    fn an_added_and_a_removed_package_each_report_one_absent_side() {
        let before = [pkg("gone", "1", "all", "aaaa")];
        let after = [pkg("new", "2", "all", "bbbb")];
        let m = moved(&before, &after);
        // Sorted by name, so `gone` precedes `new` regardless of which side held it.
        assert_eq!(
            m.iter().map(|x| x.describe()).collect::<Vec<_>>(),
            ["gone:all 1 -> (absent)", "new:all (absent) -> 2"]
        );
    }

    #[test]
    fn one_version_at_two_digests_reports_the_bytes_rather_than_one_version_twice() {
        // A name-and-version comparison calls this pair equal; they are not the same
        // `.deb`, and a report printing "1.0 -> 1.0" would read as no difference.
        let before = [pkg("libc6", "1.0", "arm64", "aaaa")];
        let after = [pkg("libc6", "1.0", "arm64", "bbbb")];
        let m = moved(&before, &after);
        assert_eq!(
            m[0].describe(),
            "libc6:arm64 1.0 sha256:aaaa -> 1.0 sha256:bbbb"
        );
    }

    #[test]
    fn the_same_name_at_two_architectures_is_two_packages() {
        // A multi-arch set holds both at once, so matching on the name alone would
        // report a spurious move between them.
        let before = [
            pkg("libc6", "1.0", "arm64", "aaaa"),
            pkg("libc6", "1.0", "armhf", "bbbb"),
        ];
        assert!(moved(&before, &before).is_empty());
        let after = [
            pkg("libc6", "1.0", "arm64", "aaaa"),
            pkg("libc6", "2.0", "armhf", "cccc"),
        ];
        let m = moved(&before, &after);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].architecture, "armhf");
    }
}
