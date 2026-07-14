//! Shared rootless-bootstrap helpers for the cross sandbox ([`crate::sandbox`])
//! and the rootfs node ([`crate::rootfs`]).
//!
//! Both stand up a Debian userland with `mmdebstrap --mode=unshare`, so they share
//! the archive component set, the default mirror, and the world-traversable
//! staging the in-namespace apt reads its acquire-phase inputs from.
//!
//! Engine side effects (filesystem + temp dirs), not pure config.

use crate::error::EngineError;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Default Debian mirror both bootstraps pull from — also the base mirror the
/// rootfs node's snapshot resolution ([`crate::snapshot`]) layers a snapshot
/// mirror onto, re-exported at the crate root as [`crate::DEFAULT_MIRROR`].
///
/// Plain `http://` is standard Debian practice: integrity comes from apt's
/// `Release`-signature verification against the vendored archive keyring, not
/// the transport, so a tampering mirror or on-path attacker can at worst
/// observe which packages are fetched or deny service — never alter what is
/// installed.
pub const DEFAULT_MIRROR: &str = "http://deb.debian.org/debian";

/// Debian archive components enabled in both bootstraps: `non-free`/
/// `non-free-firmware` carry the codecs (`libfdk-aac-dev`) and firmware the accel
/// stack and NICs need; `contrib` rounds out the standard device set. Default
/// `mmdebstrap` enables only `main`, so this is passed explicitly.
pub(crate) const COMPONENTS: &str = "main,contrib,non-free,non-free-firmware";

/// mmdebstrap `--aptopt`s that bound apt's per-connection network wait: a
/// stalled mirror times the connection out instead of hanging the bootstrap
/// indefinitely. apt retries a timed-out acquire, so a slow-but-live mirror still
/// completes; only a genuinely dead connection is abandoned. Covers both transports
/// since the default mirror is `http` but a feature repo may be `https`.
pub(crate) const APT_TIMEOUT_OPTS: [&str; 2] = [
    "--aptopt=Acquire::http::Timeout \"120\"",
    "--aptopt=Acquire::https::Timeout \"120\"",
];

/// A private, world-traversable staging directory for a bootstrap's acquire-phase
/// inputs (the local apt repo, the archive keyring, the overlay, the hooks).
///
/// `mmdebstrap --mode=unshare` runs apt's acquire methods — and the customize
/// hooks — under an unprivileged mapped uid that is *not* the invoking user, so it
/// cannot traverse a build dir under a `0700` home; the inputs must sit on a path
/// that uid can traverse. This creates a **randomly-named** dir (via `tempfile`,
/// which fails rather than reuses an existing path, so a local user cannot
/// pre-create and own it), mode `0o711` — traversable by exact path but not
/// listable — with an optional `1777` `drop/` subdir a hook running in-namespace
/// can write its results back into. The whole tree is removed on drop.
pub(crate) struct StagingRoot {
    /// The randomly-named staging dir; removed (with its contents) on drop.
    dir: tempfile::TempDir,
}

impl StagingRoot {
    /// Create a fresh staging dir whose name starts with `prefix`. Fails if the
    /// tempfile machinery cannot create a new, exclusively-owned directory.
    pub(crate) fn new(prefix: &str) -> Result<Self, EngineError> {
        let dir = tempfile::Builder::new()
            .prefix(prefix)
            .tempdir()
            .map_err(|s| EngineError::io(&std::env::temp_dir(), s))?;
        // tempfile creates the dir `0700`; widen to `0711` so the mapped acquire uid
        // can traverse in by exact path (files inside are individually
        // world-readable) without being able to enumerate the directory.
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o711))
            .map_err(|s| EngineError::io(dir.path(), s))?;
        Ok(Self { dir })
    }

    /// The staging dir's path (used by tests; the wired callers address it via
    /// [`join`](Self::join)).
    #[cfg(test)]
    pub(crate) fn path(&self) -> &Path {
        self.dir.path()
    }

    /// A path `rel` under the staging dir.
    pub(crate) fn join(&self, rel: &str) -> PathBuf {
        self.dir.path().join(rel)
    }

    /// Create (idempotently) and return the world-writable, sticky (`1777`) `drop/`
    /// subdir. A hook running in the bootstrap's user namespace sees this host-owned
    /// dir as owned by an *unmapped* uid, so the `o+w` sticky bit is what lets it
    /// create its result file; the host reads it back afterward.
    pub(crate) fn drop_dir(&self) -> Result<PathBuf, EngineError> {
        let drop = self.dir.path().join("drop");
        std::fs::create_dir_all(&drop).map_err(|s| EngineError::io(&drop, s))?;
        std::fs::set_permissions(&drop, std::fs::Permissions::from_mode(0o1777))
            .map_err(|s| EngineError::io(&drop, s))?;
        Ok(drop)
    }

    /// Copy `src` into the staging dir as `name`, world-readable (`0644`), so the
    /// mapped acquire uid can read it. Returns the staged path.
    pub(crate) fn stage_file(&self, src: &Path, name: &str) -> Result<PathBuf, EngineError> {
        let dst = self.dir.path().join(name);
        std::fs::copy(src, &dst).map_err(|s| EngineError::io(src, s))?;
        std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o644))
            .map_err(|s| EngineError::io(&dst, s))?;
        Ok(dst)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn staging_root_is_private_traversable_with_sticky_drop() {
        let root = StagingRoot::new("boot2deb-test-").unwrap();
        // Randomly named under the system temp dir, and it actually exists.
        assert!(root.path().starts_with(std::env::temp_dir()));
        assert!(root.path().is_dir());
        // 0o711: traversable by path, not listable.
        let mode = std::fs::metadata(root.path()).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o711);
        // The drop dir is 1777 for the in-namespace hook to write into.
        let drop = root.drop_dir().unwrap();
        let dmode = std::fs::metadata(&drop).unwrap().permissions().mode() & 0o7777;
        assert_eq!(dmode, 0o1777);
    }

    #[test]
    fn stage_file_copies_world_readable() {
        let root = StagingRoot::new("boot2deb-test-").unwrap();
        let src = root.join("src");
        std::fs::write(&src, b"KEY").unwrap();
        let staged = root.stage_file(&src, "keyring.gpg").unwrap();
        assert_eq!(std::fs::read(&staged).unwrap(), b"KEY");
        let mode = std::fs::metadata(&staged).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o644);
    }

    #[test]
    fn dir_is_removed_on_drop() {
        let path = {
            let root = StagingRoot::new("boot2deb-test-").unwrap();
            root.path().to_path_buf()
        };
        assert!(!path.exists());
    }
}
