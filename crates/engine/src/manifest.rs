//! Solved package manifests: the text form of a provisioner [`Plan`], and the
//! rootfs manifest's role as a reproducibility contract.
//!
//! A manifest is one `name version arch sha256` line per installed package, sorted.
//! Every sha256 is the one the signed archive records for that `.deb`, so the file
//! pins a package set by content rather than by name. Two trees are provisioned this
//! way and each writes one: the image's rootfs ([`crate::rootfs`]) and the build
//! sandbox's immutable base ([`crate::sandbox`]).
//!
//! **Manifest-as-input** applies to the rootfs manifest alone: once it is committed
//! beside the lock — its sha256 pinned in `RootfsPin.manifest_sha256` — a later build
//! verifies that a fresh solve reproduces it. Verification happens *after* the solve:
//! hash the freshly written manifest and compare it to the committed pin. A mismatch
//! means the live mirror moved off the pinned package set — a real reproducibility
//! failure, so it is a hard error by default
//! ([`ManifestDrift`](EngineError::ManifestDrift)), with the captured snapshot
//! (`--snapshot pin`) or an explicit `--save-manifest` re-pin as the remediation. The
//! sandbox base's manifest is a *record*, not a contract: it states what the toolchain
//! that compiled the target `.deb`s was, and nothing verifies a later solve against it.
//!
//! [`digest`], [`verify_reproduced`], and [`render`] are pure, so the contract and the
//! text form are testable without a bootstrap.

use crate::blobs::sha256_hex;
use crate::error::EngineError;
use ferroday_cage::provision::debian::Plan;
use std::fmt::Write as _;
use std::path::Path;

/// One package in a manifest: name, version, architecture, and the sha256 the
/// archive records for its `.deb`.
///
/// The projected form of a [`Plan`]'s package, carrying exactly the four fields a
/// manifest line holds. [`render`] takes these rather than a [`Plan`] so the text
/// form is derivable — and testable — without resolving anything.
pub type ManifestRow = (String, String, String, String);

/// Project a resolved plan onto manifest rows, in the plan's own order.
///
/// The plan *is* the installed set, resolved through the same path the provisioner
/// installs, and it carries each `.deb`'s archive-recorded sha256 — so a manifest
/// needs neither a dpkg-status parse nor an in-bootstrap hash hook.
pub fn rows(plan: &Plan) -> Vec<ManifestRow> {
    plan.packages
        .iter()
        .map(|p| {
            (
                p.name.clone(),
                p.version.clone(),
                p.architecture.clone(),
                p.sha256.clone(),
            )
        })
        .collect()
}

/// Render manifest text: `header` as a leading `#` comment, then one
/// `name version arch sha256` line per package.
///
/// Sorted by the whole row, name first, so one package set renders to one byte
/// sequence regardless of resolution order — which is what makes [`digest`] a stable
/// identity for the set rather than for the run that produced it.
pub fn render(header: &str, rows: &[ManifestRow]) -> String {
    let mut rows: Vec<&ManifestRow> = rows.iter().collect();
    rows.sort();
    let mut body = format!("# {header}\n");
    for (name, version, arch, sha) in rows {
        let _ = writeln!(body, "{name} {version} {arch} {sha}");
    }
    body
}

/// Write `plan` as a manifest at `out`, creating the parent directory, and return
/// the number of packages written.
pub fn write(header: &str, plan: &Plan, out: &Path) -> Result<usize, EngineError> {
    let rows = rows(plan);
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|s| EngineError::io(parent, s))?;
    }
    std::fs::write(out, render(header, &rows)).map_err(|s| EngineError::io(out, s))?;
    Ok(rows.len())
}

/// sha256 of a manifest file's exact bytes — the identity `RootfsPin.manifest_sha256`
/// pins. The rootfs node writes canonically-sorted manifest content, so this
/// digest is stable across builds that solve the same package set.
pub fn digest(path: &Path) -> Result<String, EngineError> {
    let bytes = std::fs::read(path).map_err(|s| EngineError::io(path, s))?;
    Ok(sha256_hex(&bytes))
}

/// Verify that a freshly-solved manifest reproduces the committed pin. `expected`
/// is the lock's `manifest_sha256`; `actual` is the fresh solve's [`digest`]. A
/// mismatch is [`ManifestDrift`](EngineError::ManifestDrift) — the mirror no longer
/// serves the pinned package set.
pub fn verify_reproduced(expected: &str, actual: &str) -> Result<(), EngineError> {
    if expected == actual {
        Ok(())
    } else {
        Err(EngineError::ManifestDrift {
            expected: expected.to_string(),
            actual: actual.to_string(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn digest_hashes_file_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let f = tmp.path().join("m.pkgs.lock");
        std::fs::write(&f, b"libc6 2.41-1 arm64 aaaa\n").unwrap();
        // Same bytes as sha256_hex over the content.
        assert_eq!(
            digest(&f).unwrap(),
            sha256_hex(b"libc6 2.41-1 arm64 aaaa\n")
        );
    }

    #[test]
    fn reproduced_manifest_passes_and_drift_errors() {
        verify_reproduced("abc123", "abc123").unwrap();
        match verify_reproduced("abc123", "def456") {
            Err(EngineError::ManifestDrift { expected, actual }) => {
                assert_eq!(expected, "abc123");
                assert_eq!(actual, "def456");
            }
            other => panic!("expected ManifestDrift, got {other:?}"),
        }
    }

    /// The rendered text is sorted by name and carries each package's
    /// archive-recorded sha256, so one package set has one digest.
    #[test]
    fn render_is_sorted_and_pinned() {
        let rows = vec![
            (
                "libc6".into(),
                "2.41-1".into(),
                "arm64".into(),
                "aaaa".into(),
            ),
            (
                "ffmpeg-rk".into(),
                "3e53143".into(),
                "arm64".into(),
                "cccc".into(),
            ),
        ];
        let body = render("Solved rootfs package manifest.", &rows);
        assert!(body.starts_with("# Solved rootfs package manifest.\n"));
        let lines: Vec<&str> = body.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(
            lines,
            vec!["ffmpeg-rk 3e53143 arm64 cccc", "libc6 2.41-1 arm64 aaaa"]
        );
    }

    /// Resolution order does not reach the bytes: the same set rendered in either
    /// order is the same file, which is what [`digest`] identifies.
    #[test]
    fn resolution_order_does_not_reach_the_digest() {
        let a: ManifestRow = (
            "libc6".into(),
            "2.41-1".into(),
            "arm64".into(),
            "aaaa".into(),
        );
        let b: ManifestRow = (
            "adduser".into(),
            "3.157".into(),
            "all".into(),
            "bbbb".into(),
        );
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
        let rows = vec![(
            "libc6".into(),
            "2.41-1".into(),
            "arm64".into(),
            "aaaa".into(),
        )];
        assert_ne!(render("rootfs.", &rows), render("build sandbox.", &rows));
    }
}
