//! Manifest-as-input (PLAN-1): the solved package manifest is the rootfs
//! reproducibility contract, so once it is committed beside the lock — its sha256
//! pinned in `RootfsPin.manifest_sha256` — a later build verifies that a fresh
//! solve reproduces it.
//!
//! The `mmdebstrap` backend cannot feed the manifest to apt as solver input (that
//! is `debrepo`'s native path), so it verifies *after* the solve: hash the
//! freshly written manifest and compare it to the committed pin. A mismatch means
//! the live mirror moved off the pinned package set — a real reproducibility
//! failure, so it is a hard error by default ([`ManifestDrift`](EngineError::ManifestDrift)),
//! with the captured snapshot (`--snapshot pin`) or an explicit
//! `--save-manifest` re-pin as the remediation.
//!
//! Pure: [`digest`] reads a file and [`verify_reproduced`] compares two digests, so
//! the contract is testable without a bootstrap.

use crate::blobs::sha256_hex;
use crate::error::EngineError;
use std::path::Path;

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
}
