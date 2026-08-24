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
//! [`digest`] and [`verify_reproduced`] are pure, so the contract is testable
//! without a bootstrap. The text form itself lives in
//! [`boot2deb_core::manifest`], so the writer here and every reader of a written
//! manifest share one definition of what the bytes are.

use crate::blobs::sha256_hex;
use crate::error::EngineError;
use boot2deb_core::manifest::{render, Moved, Package};
use ferroday_cage::provision::debian::Plan;
use std::path::Path;

/// Project a resolved plan onto manifest packages, in the plan's own order.
///
/// The plan *is* the installed set, resolved through the same path the provisioner
/// installs, and it carries each `.deb`'s archive-recorded sha256 — so a manifest
/// needs neither a dpkg-status parse nor an in-bootstrap hash hook.
pub fn packages(plan: &Plan) -> Vec<Package> {
    plan.packages
        .iter()
        .map(|p| Package {
            name: p.name.clone(),
            version: p.version.clone(),
            architecture: p.architecture.clone(),
            sha256: p.sha256.clone(),
        })
        .collect()
}

/// Write `plan` as a manifest at `out`, creating the parent directory, and return
/// the number of packages written.
pub fn write(header: &str, plan: &Plan, out: &Path) -> Result<usize, EngineError> {
    let packages = packages(plan);
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|s| EngineError::io(parent, s))?;
    }
    std::fs::write(out, render(header, &packages)).map_err(|s| EngineError::io(out, s))?;
    Ok(packages.len())
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

/// What a tree's recorded manifest holds that a fresh solve no longer gives, and the
/// reverse — empty when the record still describes what the archive would resolve now.
///
/// A base sandbox's manifest is a *record* of what a bootstrap installed, and a record
/// stays true about the past however far the archive moves on. This is the question the
/// record cannot answer on its own: whether the tree it sits beside is still the tree
/// the current archive would produce. Answering it is what keeps a reused base from
/// being one the archive has since left behind — see
/// [`ensure`](crate::sandbox) on the build sandbox base.
///
/// Compared as package sets rather than as bytes, so the answer is a list a report can
/// name rather than a pair of digests. Unlike [`verify_reproduced`], a difference here
/// is not a failure: the caller's remedy is to re-provision, not to stop.
///
/// # Errors
///
/// [`EngineError::Io`] if the manifest cannot be read,
/// and [`EngineError::Config`] if it does not parse
/// — a manifest that cannot be read is not evidence that the tree is current.
pub fn diverged(recorded: &Path, solved: &[Package]) -> Result<Vec<Moved>, EngineError> {
    let text = std::fs::read_to_string(recorded).map_err(|s| EngineError::io(recorded, s))?;
    let held = boot2deb_core::manifest::parse(&text, &recorded.display().to_string())?;
    Ok(boot2deb_core::manifest::moved(&held, solved))
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

    /// Write a manifest holding `rows` of `name version arch sha256`.
    fn recorded(dir: &std::path::Path, rows: &[&str]) -> std::path::PathBuf {
        let f = dir.join("base.pkgs");
        std::fs::write(
            &f,
            format!("# Solved base package manifest.\n{}\n", rows.join("\n")),
        )
        .unwrap();
        f
    }

    fn pkg(name: &str, version: &str, sha256: &str) -> Package {
        Package {
            name: name.into(),
            version: version.into(),
            architecture: "arm64".into(),
            sha256: sha256.into(),
        }
    }

    #[test]
    fn a_record_the_archive_still_agrees_with_has_not_diverged() {
        let tmp = tempfile::tempdir().unwrap();
        let f = recorded(
            tmp.path(),
            &["libc6 2.42-17 arm64 aaaa", "bash 5.2 arm64 bbbb"],
        );
        // The fresh solve in resolution order, which is not the file's sorted order —
        // an empty answer must not depend on how the resolver happened to list them.
        let solved = [pkg("bash", "5.2", "bbbb"), pkg("libc6", "2.42-17", "aaaa")];
        assert!(diverged(&f, &solved).unwrap().is_empty());
    }

    #[test]
    fn a_base_the_archive_moved_past_names_what_moved() {
        // The §5 case exactly: one directory name, two different libc6.
        let tmp = tempfile::tempdir().unwrap();
        let f = recorded(tmp.path(), &["libc6 2.42-17 arm64 aaaa"]);
        let solved = [pkg("libc6", "2.43-3", "cccc")];
        let moved = diverged(&f, &solved).unwrap();
        assert_eq!(
            moved.iter().map(|m| m.describe()).collect::<Vec<_>>(),
            ["libc6:arm64 2.42-17 -> 2.43-3"]
        );
    }

    #[test]
    fn an_unreadable_record_errors_rather_than_reading_as_current() {
        // An empty answer means "this tree is what the archive would give now". A
        // manifest that cannot be read is not evidence of that, so it must not
        // produce one — silently reusing an unverifiable base is the skew itself.
        let tmp = tempfile::tempdir().unwrap();
        assert!(diverged(&tmp.path().join("absent.pkgs"), &[]).is_err());

        let bad = tmp.path().join("bad.pkgs");
        std::fs::write(&bad, b"libc6 2.42-17 arm64\n").unwrap();
        assert!(diverged(&bad, &[]).is_err());
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
}
