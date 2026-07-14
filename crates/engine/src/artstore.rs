//! Content-addressed store of build-node **output artifacts** (Tier 2).
//!
//! Tier 1 ([`crate::signature`]) caches a compile node's *source tree* so a lock
//! bump does not silently reuse a stale checkout — but it never caches the node's
//! *output*, so the expensive `make`/`dpkg-buildpackage` step re-runs on every
//! build (the ~30 min kernel cross-compile, the ~70 min qemu ffmpeg build). This
//! store closes that gap: each node's produced `.deb`s (and the u-boot raw payloads)
//! are kept under a directory keyed by the node's **output signature** — the full
//! set of inputs that determine the output, not just the tree. On a signature
//! hit `build` restores the files instead of recompiling; on a miss it compiles and
//! [`put`](ArtifactStore::put)s. Because the key covers every output-affecting input,
//! a hit is sound; because the store lives outside any recipe work dir, it survives
//! `clean` and is shared across work dirs — a rebuilt or freshly-cloned checkout
//! restores rather than recompiles.
//!
//! The store is self-verifying like [`crate::debstore`]: an entry is a directory
//! `<node>/<signature>/` holding the artifact files plus a `manifest.toml`, written
//! atomically (assembled in a temp dir, renamed into place) so a present entry is
//! always complete. A restore whose manifest is unreadable or whose files are not
//! all present is treated as a **miss** — the same fail-safe bias as the signature
//! stamps: a spurious miss only wastes time, a spurious hit ships a stale artifact.
//!
//! This is the signature-keyed restore half of Tier 2. The output-hash *early
//! cutoff* (skipping a dependent when a changed input reproduces a byte-identical
//! output) is a separate future addition that waits on byte-reproducible outputs
//! — it is not implemented here.

use crate::error::EngineError;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// One stored artifact: the role its producing node assigns it (e.g. `image_deb`,
/// `idbloader`, `deb`) and the file name it is stored and restored under. The role
/// lets a node reconstruct its typed artifact struct on restore; the file name is
/// the artifact's on-disk name (preserved so downstream stages see the same names a
/// fresh build produces).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredArtifact {
    /// The producing node's role for this file.
    pub role: String,
    /// The file's name (basename), shared by the store copy and the restored copy.
    pub file: String,
}

/// The `manifest.toml` beside a stored node output: which node + output signature
/// it is, and the ordered artifacts it holds. Serialized as TOML so a stale/foreign
/// entry simply fails to parse and is treated as a miss.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
struct StoreManifest {
    /// The build node (e.g. `kernel`, `userspace:mpp`, `ffmpeg`).
    node: String,
    /// The output signature this entry is keyed by.
    signature: String,
    /// The stored artifacts, in the order the producing node listed them.
    artifacts: Vec<StoredArtifact>,
}

/// A content-addressed store of build-node output artifacts, one directory per
/// `(node, signature)` under a single root (`<root>/cache/artifacts`).
pub struct ArtifactStore {
    root: PathBuf,
}

impl ArtifactStore {
    /// Open the store rooted at `root`, creating it if needed. Opportunistically
    /// sweeps stale `.partial` temps left by a hard-killed `put` — including
    /// the per-node subdir temps — before serving this build.
    pub fn open(root: &Path) -> Result<ArtifactStore, EngineError> {
        std::fs::create_dir_all(root).map_err(|s| EngineError::io(root, s))?;
        crate::gc::sweep_stale_temps(root);
        Ok(ArtifactStore {
            root: root.to_path_buf(),
        })
    }

    /// Filesystem-safe directory component for a node name: `userspace:mpp` →
    /// `userspace-mpp`, so a `:` never lands in a path component.
    fn node_dir(&self, node: &str) -> PathBuf {
        self.root.join(node.replace(':', "-"))
    }

    /// The entry directory for one `(node, signature)`, present or not.
    fn entry_dir(&self, node: &str, signature: &str) -> PathBuf {
        self.node_dir(node).join(signature)
    }

    /// Whether the store holds a complete entry for `(node, signature)` — a cheap
    /// stat of its manifest, used to decide up front whether a stage needs to build
    /// anything at all (e.g. skip a sandbox bootstrap when every package is cached).
    pub fn has(&self, node: &str, signature: &str) -> bool {
        self.entry_dir(node, signature).join("manifest.toml").is_file()
    }

    /// Restore a stored `(node, signature)` output into `dest`, returning the ordered
    /// `(role, restored-path)` list, or `None` on a miss.
    ///
    /// A hit whose manifest is unreadable, whose recorded signature does not match, or
    /// whose files are not all present is treated as a **miss** (fail-safe): a
    /// partial or foreign entry is never trusted, so the caller rebuilds. Files are
    /// copied into `dest` (created if needed) under their stored names, matching the
    /// paths a fresh build would stage there.
    pub fn restore(
        &self,
        node: &str,
        signature: &str,
        dest: &Path,
    ) -> Result<Option<Vec<(String, PathBuf)>>, EngineError> {
        let dir = self.entry_dir(node, signature);
        let Ok(text) = std::fs::read_to_string(dir.join("manifest.toml")) else {
            return Ok(None);
        };
        let man: StoreManifest = match toml::from_str(&text) {
            Ok(m) => m,
            Err(_) => return Ok(None),
        };
        if man.signature != signature {
            return Ok(None);
        }
        // Verify every file is present before touching `dest`, so a partial entry
        // never restores half its artifacts.
        if man.artifacts.iter().any(|a| !dir.join(&a.file).is_file()) {
            return Ok(None);
        }
        std::fs::create_dir_all(dest).map_err(|s| EngineError::io(dest, s))?;
        let mut out = Vec::with_capacity(man.artifacts.len());
        for a in &man.artifacts {
            let src = dir.join(&a.file);
            let dst = dest.join(&a.file);
            std::fs::copy(&src, &dst).map_err(|s| EngineError::io(&src, s))?;
            out.push((a.role.clone(), dst));
        }
        Ok(Some(out))
    }

    /// Store `artifacts` (role, source-file pairs) under `(node, signature)`.
    ///
    /// Atomic: the files + manifest are assembled in a uniquely-named sibling temp
    /// dir and renamed into place, so a present entry directory is always complete.
    /// A re-put of an entry that already exists (idempotent rebuild, or a concurrent
    /// build that won the rename race) keeps the existing complete entry and discards
    /// the temp — the content is signature-keyed, so any complete entry is equivalent.
    pub fn put(
        &self,
        node: &str,
        signature: &str,
        artifacts: &[(&str, &Path)],
    ) -> Result<(), EngineError> {
        if self.has(node, signature) {
            return Ok(());
        }
        let node_dir = self.node_dir(node);
        std::fs::create_dir_all(&node_dir).map_err(|s| EngineError::io(&node_dir, s))?;
        let tmp = node_dir.join(format!(".{signature}.{}.partial", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).map_err(|s| EngineError::io(&tmp, s))?;

        let mut entries = Vec::with_capacity(artifacts.len());
        for (role, src) in artifacts {
            let file = src
                .file_name()
                .and_then(|n| n.to_str())
                .expect("staged artifact has a file name")
                .to_string();
            std::fs::copy(src, tmp.join(&file)).map_err(|s| EngineError::io(src, s))?;
            entries.push(StoredArtifact {
                role: role.to_string(),
                file,
            });
        }
        let man = StoreManifest {
            node: node.to_string(),
            signature: signature.to_string(),
            artifacts: entries,
        };
        let man_text = toml::to_string(&man).map_err(|e| EngineError::io(&tmp, std::io::Error::other(e)))?;
        std::fs::write(tmp.join("manifest.toml"), man_text).map_err(|s| EngineError::io(&tmp, s))?;

        let final_dir = self.entry_dir(node, signature);
        match std::fs::rename(&tmp, &final_dir) {
            Ok(()) => Ok(()),
            // Lost the race to a concurrent put of the same key: theirs is complete
            // and equivalent, so drop ours.
            Err(_) if final_dir.join("manifest.toml").is_file() => {
                let _ = std::fs::remove_dir_all(&tmp);
                Ok(())
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&tmp);
                Err(EngineError::io(&final_dir, e))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Write a file with `content` under `dir`, returning its path.
    fn write(dir: &Path, name: &str, content: &[u8]) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, content).unwrap();
        p
    }

    #[test]
    fn put_then_restore_round_trips_roles_and_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(&tmp.path().join("store")).unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(&src).unwrap();
        let image = write(&src, "linux-image_1_arm64.deb", b"image-bytes");
        let headers = write(&src, "linux-headers_1_arm64.deb", b"headers-bytes");

        assert!(!store.has("kernel", "sigA"));
        store
            .put(
                "kernel",
                "sigA",
                &[("image_deb", &image), ("headers_deb", &headers)],
            )
            .unwrap();
        assert!(store.has("kernel", "sigA"));

        let dest = tmp.path().join("out");
        let restored = store.restore("kernel", "sigA", &dest).unwrap().unwrap();
        // Roles preserved in order; files restored under their names, with bytes.
        assert_eq!(restored[0].0, "image_deb");
        assert_eq!(restored[1].0, "headers_deb");
        assert_eq!(restored[0].1, dest.join("linux-image_1_arm64.deb"));
        assert_eq!(std::fs::read(&restored[0].1).unwrap(), b"image-bytes");
        assert_eq!(std::fs::read(&restored[1].1).unwrap(), b"headers-bytes");
    }

    #[test]
    fn restore_is_a_miss_for_absent_or_mismatched_key() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(tmp.path()).unwrap();
        let dest = tmp.path().join("out");
        // Never stored.
        assert!(store.restore("kernel", "sigX", &dest).unwrap().is_none());
        // A different node / signature does not collide.
        let src = write(tmp.path(), "a.deb", b"x");
        store.put("kernel", "sigA", &[("deb", &src)]).unwrap();
        assert!(store.restore("kernel", "sigB", &dest).unwrap().is_none());
        assert!(store.restore("ffmpeg", "sigA", &dest).unwrap().is_none());
    }

    #[test]
    fn node_names_with_colons_are_path_safe() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(tmp.path()).unwrap();
        let src = write(tmp.path(), "librga2_2_arm64.deb", b"rga");
        store.put("userspace:librga", "sig1", &[("deb", &src)]).unwrap();
        // Stored under a `:`-free directory, yet addressed by the colon name.
        assert!(tmp.path().join("userspace-librga").join("sig1").is_dir());
        assert!(store.has("userspace:librga", "sig1"));
        let dest = tmp.path().join("out");
        assert!(store.restore("userspace:librga", "sig1", &dest).unwrap().is_some());
    }

    #[test]
    fn a_partial_entry_missing_a_file_is_a_miss() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(tmp.path()).unwrap();
        let src = write(tmp.path(), "a.deb", b"x");
        store.put("kernel", "sigA", &[("deb", &src)]).unwrap();
        // Corrupt the entry by deleting the stored file but leaving the manifest.
        std::fs::remove_file(tmp.path().join("kernel").join("sigA").join("a.deb")).unwrap();
        let dest = tmp.path().join("out");
        assert!(store.restore("kernel", "sigA", &dest).unwrap().is_none());
    }

    #[test]
    fn put_is_idempotent_and_leaves_no_temp() {
        let tmp = tempfile::tempdir().unwrap();
        let store = ArtifactStore::open(tmp.path()).unwrap();
        let src = write(tmp.path(), "a.deb", b"x");
        store.put("kernel", "sigA", &[("deb", &src)]).unwrap();
        // A second put of the same key is a no-op (kept the existing entry).
        store.put("kernel", "sigA", &[("deb", &src)]).unwrap();
        // No leftover `.partial` temp dirs under the node dir.
        let node_dir = tmp.path().join("kernel");
        let strays: Vec<_> = std::fs::read_dir(&node_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".partial"))
            .collect();
        assert!(strays.is_empty(), "left a partial temp: {strays:?}");
    }
}
