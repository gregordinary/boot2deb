//! rkbin blob hashing and verification.
//!
//! Blobs (the ATF/BL31 ELF and DDR TPL) are vendored under `blobs/<soc>/` and
//! read through a `(filename, sha256)` key. `boot2deb update` records each as a
//! lock pin `"<filename>@sha256:<hex>"` ([`pin`]); the u-boot build [`verify`]s
//! the vendored file against that pin before consuming it, so a swapped or
//! corrupted blob is a typed error, never a silently different bootloader.
//!
//! Hashing is pure-Rust (`sha2`); the pure helpers ([`sha256_hex`],
//! [`parse_pin`]) are unit-tested without I/O.

use crate::error::EngineError;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};

/// Lowercase-hex sha256 of `bytes`.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut out = String::with_capacity(64);
    for byte in digest {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// Format a lock pin for a blob: `"<filename>@sha256:<hex>"`.
pub fn pin(filename: &str, bytes: &[u8]) -> String {
    format!("{filename}@sha256:{}", sha256_hex(bytes))
}

/// Split a pin `"<filename>@sha256:<hex>"` into `(filename, hex)`. Pure, so the
/// parse is testable; returns `None` if the shape is wrong.
pub fn parse_pin(pin: &str) -> Option<(&str, &str)> {
    let (filename, rest) = pin.rsplit_once("@sha256:")?;
    if filename.is_empty() || rest.is_empty() {
        return None;
    }
    Some((filename, rest))
}

/// Verify the blob named by `expected_pin` inside `dir` against its recorded
/// hash, returning the blob's path on success.
///
/// The filename comes from the pin itself, so this checks exactly the file the
/// lock names. A malformed pin is [`EngineError::BlobPinMalformed`]; a hash
/// mismatch is [`EngineError::BlobMismatch`].
pub fn verify(dir: &Path, expected_pin: &str) -> Result<PathBuf, EngineError> {
    let (filename, _) = read_verified(dir, expected_pin)?;
    Ok(dir.join(filename))
}

/// Verify the pinned blob and copy the **verified bytes** into `stage_dir`,
/// returning the staged path. The consumer reads the staged copy, so the
/// bytes it uses are exactly the ones that were hashed — closing the
/// verify-then-read TOCTOU where the vendored source could be swapped between the
/// hash check and `make` re-reading it (SEC-5).
pub fn verify_to(dir: &Path, expected_pin: &str, stage_dir: &Path) -> Result<PathBuf, EngineError> {
    let (filename, bytes) = read_verified(dir, expected_pin)?;
    std::fs::create_dir_all(stage_dir).map_err(|source| EngineError::io(stage_dir, source))?;
    let dest = stage_dir.join(filename);
    std::fs::write(&dest, &bytes).map_err(|source| EngineError::io(&dest, source))?;
    Ok(dest)
}

/// Read the pinned blob, verify its sha256, and return `(filename, verified
/// bytes)`. Shared by [`verify`] and [`verify_to`].
fn read_verified<'a>(dir: &Path, expected_pin: &'a str) -> Result<(&'a str, Vec<u8>), EngineError> {
    let (filename, expected_hex) =
        parse_pin(expected_pin).ok_or_else(|| EngineError::BlobPinMalformed {
            pin: expected_pin.to_string(),
        })?;
    let path = dir.join(filename);
    let bytes = std::fs::read(&path).map_err(|source| EngineError::io(&path, source))?;
    let actual = sha256_hex(&bytes);
    if actual == expected_hex {
        Ok((filename, bytes))
    } else {
        Err(EngineError::BlobMismatch {
            filename: filename.to_string(),
            expected: expected_hex.to_string(),
            actual,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_known_vector() {
        // sha256("") — a fixed vector, no I/O.
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn pin_formats_filename_and_hash() {
        assert_eq!(
            pin("blob.bin", b""),
            "blob.bin@sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
    }

    #[test]
    fn parse_pin_splits_on_last_marker() {
        assert_eq!(parse_pin("a.elf@sha256:dead"), Some(("a.elf", "dead")));
        // rsplit tolerates a filename that itself contains the marker text.
        assert_eq!(
            parse_pin("weird@sha256:name@sha256:beef"),
            Some(("weird@sha256:name", "beef"))
        );
        assert_eq!(parse_pin("no-marker"), None);
        assert_eq!(parse_pin("@sha256:beef"), None);
        assert_eq!(parse_pin("file@sha256:"), None);
    }

    #[test]
    fn verify_accepts_matching_and_rejects_swapped() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("blob.bin"), b"payload").unwrap();
        let good = pin("blob.bin", b"payload");
        assert_eq!(verify(tmp.path(), &good).unwrap(), tmp.path().join("blob.bin"));

        let bad = pin("blob.bin", b"different");
        assert!(matches!(
            verify(tmp.path(), &bad),
            Err(EngineError::BlobMismatch { .. })
        ));
    }

    #[test]
    fn verify_to_stages_the_verified_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("blob.bin"), b"payload").unwrap();
        let stage = tmp.path().join("stage");
        let good = pin("blob.bin", b"payload");
        let staged = verify_to(tmp.path(), &good, &stage).unwrap();
        // The staged copy exists, holds the verified bytes, and lives in stage_dir.
        assert_eq!(staged, stage.join("blob.bin"));
        assert_eq!(std::fs::read(&staged).unwrap(), b"payload");
        // A mismatch stages nothing.
        let bad = pin("blob.bin", b"different");
        assert!(matches!(
            verify_to(tmp.path(), &bad, &stage),
            Err(EngineError::BlobMismatch { .. })
        ));
    }

    #[test]
    fn verify_rejects_malformed_pin() {
        let tmp = tempfile::tempdir().unwrap();
        assert!(matches!(
            verify(tmp.path(), "garbage"),
            Err(EngineError::BlobPinMalformed { .. })
        ));
    }
}
