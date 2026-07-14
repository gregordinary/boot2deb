//! Content-addressed `.deb` store — a small on-disk cache keyed by
//! sha256, holding the pre-built `extra_debs` a layer or feature pulls from outside
//! the Debian mirror.
//!
//! `update` fills the store (fetch → verify → put) so a later `build` is offline; a
//! build materializes from it, re-fetching only to fill a miss ([`crate::extradebs`]).
//! Content addressing makes the store self-verifying: a file at `<sha256>.deb` is
//! written only after its bytes hash back to that name, so a store *hit* is trusted
//! (the deb is re-verified again at install time against the solved manifest),
//! and two layers pulling identical bytes share one entry. The store is durable — it
//! is a build-host cache outside the per-recipe work dir, so it survives `clean` and
//! the build "no longer depends on [the source] staying put".

use crate::blobs::sha256_hex;
use crate::error::EngineError;
use std::path::{Path, PathBuf};

/// A content-addressed store of `.deb` files under one directory, each named
/// `<sha256>.deb`.
pub struct DebStore {
    dir: PathBuf,
}

impl DebStore {
    /// Open the store rooted at `dir`, creating it if needed. Opportunistically sweeps
    /// stale `.partial` temps a hard-killed `put_bytes` may have left.
    pub fn open(dir: &Path) -> Result<DebStore, EngineError> {
        std::fs::create_dir_all(dir).map_err(|s| EngineError::io(dir, s))?;
        crate::gc::sweep_stale_temps(dir);
        Ok(DebStore {
            dir: dir.to_path_buf(),
        })
    }

    /// The path a deb with hash `sha256` occupies, whether or not it is present.
    pub fn path_for(&self, sha256: &str) -> PathBuf {
        self.dir.join(format!("{sha256}.deb"))
    }

    /// Whether the store already holds the deb with hash `sha256`.
    pub fn has(&self, sha256: &str) -> bool {
        self.path_for(sha256).is_file()
    }

    /// Store `bytes` after verifying they hash to `expected_sha256`, returning the
    /// stored path. A hash mismatch is [`EngineError::ExtraDebHashMismatch`] and
    /// stores nothing (`locator` labels the source in the error). The write is
    /// atomic — a uniquely-named temp renamed into place — so an interrupted put
    /// never leaves a truncated `<sha256>.deb` that a later build would trust.
    pub fn put_bytes(
        &self,
        bytes: &[u8],
        expected_sha256: &str,
        locator: &str,
    ) -> Result<PathBuf, EngineError> {
        let actual = sha256_hex(bytes);
        if actual != expected_sha256 {
            return Err(EngineError::ExtraDebHashMismatch {
                locator: locator.to_string(),
                expected: expected_sha256.to_string(),
                actual,
            });
        }
        let dest = self.path_for(expected_sha256);
        // Write to a temp beside dest (same filesystem → rename is atomic), then
        // move it over. Two concurrent puts of the *same* sha write identical bytes,
        // so a temp-name race is harmless; distinct content cannot collide (distinct
        // sha). The pid keeps a stray temp from a crashed run out of the way.
        let tmp = self
            .dir
            .join(format!(".{expected_sha256}.{}.partial", std::process::id()));
        std::fs::write(&tmp, bytes).map_err(|s| EngineError::io(&tmp, s))?;
        std::fs::rename(&tmp, &dest).map_err(|s| {
            let _ = std::fs::remove_file(&tmp);
            EngineError::io(&dest, s)
        })?;
        Ok(dest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_verifies_and_stores_by_hash() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DebStore::open(tmp.path()).unwrap();
        let bytes = b"deb-bytes";
        let sha = sha256_hex(bytes);

        assert!(!store.has(&sha));
        let path = store.put_bytes(bytes, &sha, "vendor/x.deb").unwrap();
        assert_eq!(path, store.path_for(&sha));
        assert!(store.has(&sha));
        assert_eq!(std::fs::read(&path).unwrap(), bytes);

        // A second put of the same content is idempotent (same path, same bytes).
        let again = store.put_bytes(bytes, &sha, "vendor/x.deb").unwrap();
        assert_eq!(again, path);
    }

    #[test]
    fn put_rejects_hash_mismatch_and_leaves_no_files() {
        let tmp = tempfile::tempdir().unwrap();
        let store = DebStore::open(tmp.path()).unwrap();
        // Pin one hash but hand it different bytes.
        let pinned = sha256_hex(b"the-pinned-deb");
        let err = store
            .put_bytes(b"tampered-bytes", &pinned, "https://vendor/x.deb")
            .unwrap_err();
        assert!(matches!(err, EngineError::ExtraDebHashMismatch { .. }));
        assert!(!store.has(&pinned));
        // No stored file and no leftover temp.
        let entries: Vec<_> = std::fs::read_dir(tmp.path()).unwrap().flatten().collect();
        assert!(entries.is_empty(), "mismatch left files behind: {entries:?}");
    }
}
