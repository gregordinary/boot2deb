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
use std::fmt::Write as _;

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
}
