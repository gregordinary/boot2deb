//! Rootfs early-cutoff cache — skip the expensive `mmdebstrap` bootstrap
//! when the *solved* package set is unchanged, without ever serving a stale solve.
//!
//! The rootfs node's cost is dominated by the qemu-emulated package configure
//! (~250 s), not by the dependency solve (seconds). So this caches on the **solved
//! manifest** rather than on the input package *names* ("early cutoff"): every
//! build first runs a cheap `mmdebstrap --simulate` to learn the exact versions the
//! current mirror resolves, hashes that solved set into a [`Signature`], and reuses
//! a stored rootfs only when the hash matches. A moved mirror resolves different
//! versions → a different key → an automatic fresh bootstrap, so a cache hit can
//! never reflect an out-of-date mirror. This is the same early-cutoff the `debrepo`
//! backend gets natively from its `resolvo` solve-then-materialize path; here
//! it is realized on the `mmdebstrap` backend via `--simulate`, and the key/store
//! carry over unchanged when `debrepo` is promoted.
//!
//! **Soundness of keying on the solved set.** Debian archive versions are
//! immutable — a given `name version` is byte-identical across every mirror and
//! forever — so `name version arch` uniquely identifies a mirror `.deb`'s content.
//! The build's own accel `.deb`s (from the local repo) are *not* archive
//! packages, so their bytes are folded in directly ([`RootfsStore`] callers pass
//! their sha256s), as is the assembled overlay tree. What is deliberately **not**
//! folded is the per-image first-boot password (SEC-6): it is unique per
//! build by design, so it is applied *after* restore (the rootfs node splices it
//! into `/etc/shadow`, [`splice_shadow`]), keeping the cached tree reusable.
//!
//! Pure except [`dir_fingerprints`] / [`file_fingerprints`] (which hash files) and
//! [`RootfsStore`] (the on-disk store); the parse, key, and splice are deterministic
//! and unit-tested.

use crate::blobs::sha256_hex;
use crate::error::EngineError;
use crate::event::Step;
use crate::signature::{Signature, SignatureBuilder};
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

/// Stage-recipe version for the rootfs cache key. Bump when the node's build
/// logic changes in a way that alters the produced tree for unchanged inputs (e.g.
/// the overlay merge order or the generated-config shape), so a logic change forces
/// a fresh bootstrap rather than a stale hit.
const ROOTFS_STAGE_VERSION: u32 = 2;

/// Parse `mmdebstrap --simulate --verbose` output into the solved package set: one
/// `"name version arch"` line per configured package, sorted and de-duplicated.
///
/// `--simulate` runs `apt-get --simulate`, which emits a `Conf <name> (<version>
/// <origin> [<arch>])` line for every package it would configure — the exact
/// installed set. Non-`Conf` lines (mmdebstrap's own `I:`/`Inst`/progress noise)
/// are ignored. Pure, so the parse is unit-tested against real simulate output.
pub fn parse_solved(output: &str) -> Vec<String> {
    let mut set: Vec<String> = output
        .lines()
        .filter_map(|line| {
            // `Conf <name> (<version> <origin> [<arch>])`
            let rest = line.strip_prefix("Conf ")?;
            let (name, paren) = rest.split_once(" (")?;
            let inside = paren.strip_suffix(')')?;
            let version = inside.split_whitespace().next()?;
            // The arch is the last bracketed token: `... [arm64]`.
            let arch = inside.rsplit_once('[')?.1.strip_suffix(']')?;
            if name.is_empty() || version.is_empty() || arch.is_empty() {
                return None;
            }
            Some(format!("{name} {version} {arch}"))
        })
        .collect();
    set.sort();
    set.dedup();
    set
}

/// The rootfs cache key: a [`Signature`] over the solved package set, the
/// assembled overlay's content, the local-repo `.deb`s' content, and the target
/// `arch`/`suite`. Everything that determines the produced tree *except* the
/// per-image password (applied on restore). Pure.
pub fn cache_key(
    solved: &[String],
    overlay: &[String],
    repo_debs: &[String],
    arch: &str,
    suite: &str,
) -> Signature {
    let mut b = SignatureBuilder::new("rootfs", ROOTFS_STAGE_VERSION);
    b.fold_scalar("arch", arch);
    b.fold_scalar("suite", suite);
    b.fold_set("solved", solved);
    b.fold_set("overlay", overlay);
    b.fold_set("repo_debs", repo_debs);
    b.finish()
}

/// Content fingerprints of every regular file and symlink under `dir`, sorted: a
/// `relpath\0<octal-mode>\0<sha256>` record per file and a `relpath\0L\0<target>`
/// record per symlink (NUL-separated, so no path can forge a field boundary).
/// Directories contribute only through their contents. A non-existent `dir` yields
/// an empty list. Used to fold the assembled overlay tree into [`cache_key`].
pub fn dir_fingerprints(dir: &Path) -> Result<Vec<String>, EngineError> {
    let mut out = Vec::new();
    if dir.exists() {
        walk_fingerprints(dir, dir, &mut out)?;
    }
    out.sort();
    Ok(out)
}

/// Recursive worker for [`dir_fingerprints`]: descend `dir`, recording each entry
/// relative to `base`. Symlinks record their target (not chased); files record mode
/// + content hash.
fn walk_fingerprints(dir: &Path, base: &Path, out: &mut Vec<String>) -> Result<(), EngineError> {
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|s| EngineError::io(dir, s))?
        .map(|e| e.map(|e| e.path()))
        .collect::<Result<_, _>>()
        .map_err(|s| EngineError::io(dir, s))?;
    entries.sort();
    for path in entries {
        let rel = path.strip_prefix(base).unwrap_or(&path).to_string_lossy();
        let meta = std::fs::symlink_metadata(&path).map_err(|s| EngineError::io(&path, s))?;
        if meta.file_type().is_symlink() {
            let target = std::fs::read_link(&path).map_err(|s| EngineError::io(&path, s))?;
            out.push(format!("{rel}\0L\0{}", target.to_string_lossy()));
        } else if meta.is_dir() {
            walk_fingerprints(&path, base, out)?;
        } else {
            let bytes = std::fs::read(&path).map_err(|s| EngineError::io(&path, s))?;
            let mode = meta.permissions().mode() & 0o7777;
            out.push(format!("{rel}\0{mode:o}\0{}", sha256_hex(&bytes)));
        }
    }
    Ok(())
}

/// sha256 of each file in `files`, sorted + de-duplicated — the content identity of
/// the build's local-repo `.deb`s folded into [`cache_key`]. Non-archive packages
/// carry no immutable-version guarantee, so their bytes are the key.
pub fn file_fingerprints(files: &[PathBuf]) -> Result<Vec<String>, EngineError> {
    let mut out = Vec::with_capacity(files.len());
    for f in files {
        let bytes = std::fs::read(f).map_err(|s| EngineError::io(f, s))?;
        out.push(sha256_hex(&bytes));
    }
    out.sort();
    out.dedup();
    Ok(out)
}

/// Replace `user`'s line in an `/etc/shadow` text with a fresh crypt `hash`, forcing
/// a change at first login (last-change field `0`, the [`passwd
/// -e`](crate::rootfs) equivalent). Returns `None` if `user` has no line — the
/// caller treats that as a hard error, never a silent no-op. Pure.
///
/// The rewritten line is `user:hash:0:0:99999:7:::` — hash, last-change 0 (expired),
/// min 0, max 99999, warn 7, the standard remaining defaults. Only the matching
/// line changes; every other account is preserved verbatim.
pub fn splice_shadow(shadow: &str, user: &str, hash: &str) -> Option<String> {
    let prefix = format!("{user}:");
    let mut found = false;
    let mut out = String::with_capacity(shadow.len() + hash.len());
    for line in shadow.split_inclusive('\n') {
        let (content, nl) = match line.strip_suffix('\n') {
            Some(c) => (c, "\n"),
            None => (line, ""),
        };
        if content.starts_with(&prefix) {
            found = true;
            out.push_str(&format!("{user}:{hash}:0:0:99999:7:::{nl}"));
        } else {
            out.push_str(line);
        }
    }
    found.then_some(out)
}

/// A cached rootfs the store restores instead of re-bootstrapping.
pub struct CachedRootfs {
    /// The stored rootfs tarball (password-free — the account is present but locked;
    /// the per-image password is spliced in on restore).
    pub tar: PathBuf,
    /// The stored content-pinned solved manifest for this tarball.
    pub manifest: PathBuf,
}

/// Content-addressed store of bootstrapped rootfs trees under `<cache>/rootfs/`,
/// keyed by the [`cache_key`] signature. A stored entry is a directory
/// `<key>/` holding `rootfs.tar` + `manifest.pkgs`; it is published atomically (a
/// `<key>.partial` staged then renamed), so an interrupted store never leaves a
/// half-written entry a later build would trust — the same fail-safe the sandbox
/// bootstrap uses.
pub struct RootfsStore {
    /// The `<cache>/rootfs` root the entries live under.
    root: PathBuf,
}

impl RootfsStore {
    /// A store rooted at `<cache_dir>/rootfs`. Opportunistically sweeps stale
    /// `<key>.partial` temps a hard-killed `put` may have left (ATOM-3).
    pub fn new(cache_dir: &Path) -> Self {
        let root = cache_dir.join("rootfs");
        crate::gc::sweep_stale_temps(&root);
        RootfsStore { root }
    }

    /// The entry directory for `key`.
    fn entry(&self, key: &Signature) -> PathBuf {
        self.root.join(key.as_str())
    }

    /// The cached rootfs for `key`, or `None` on a miss. A hit requires **both** the
    /// tarball and its manifest present (a partially-written entry is a miss).
    pub fn get(&self, key: &Signature) -> Option<CachedRootfs> {
        let entry = self.entry(key);
        let tar = entry.join("rootfs.tar");
        let manifest = entry.join("manifest.pkgs");
        (tar.is_file() && manifest.is_file()).then_some(CachedRootfs { tar, manifest })
    }

    /// Store `tar` + `manifest` under `key`, replacing any prior entry. Copies into a
    /// `<key>.partial` dir then renames it into place, so the entry only ever appears
    /// complete.
    pub fn put(
        &self,
        key: &Signature,
        tar: &Path,
        manifest: &Path,
        step: &Step,
    ) -> Result<(), EngineError> {
        let entry = self.entry(key);
        let partial = self.root.join(format!("{}.partial", key.as_str()));
        let _ = std::fs::remove_dir_all(&partial);
        std::fs::create_dir_all(&partial).map_err(|s| EngineError::io(&partial, s))?;
        std::fs::copy(tar, partial.join("rootfs.tar")).map_err(|s| EngineError::io(tar, s))?;
        std::fs::copy(manifest, partial.join("manifest.pkgs"))
            .map_err(|s| EngineError::io(manifest, s))?;
        // Replace any prior entry (a --refresh-rootfs rebuild), then publish atomically.
        let _ = std::fs::remove_dir_all(&entry);
        std::fs::rename(&partial, &entry).map_err(|s| EngineError::io(&entry, s))?;
        step.log(format!("cached rootfs {} in the store", key.short()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_solved_extracts_name_version_arch_from_conf_lines() {
        // Real `mmdebstrap --simulate --verbose` output shape (Conf + Inst + noise).
        let output = "\
I: simulate creating tarball...
Inst libc6 (2.41-2 Debian:forky [arm64])
Conf ca-certificates (20260601 Debian:testing [all])
Conf openssl (3.6.3-1 Debian:testing [arm64])
Conf libpopt0 (1.19+dfsg-2+b2 Debian:testing [arm64])
Conf libc6 (2.41-2 Debian:forky [arm64])
I: success in 2.7310 seconds
";
        let solved = parse_solved(output);
        // Only Conf lines, sorted, name+version+arch — the Inst line and I: noise out.
        assert_eq!(
            solved,
            vec![
                "ca-certificates 20260601 all".to_string(),
                "libc6 2.41-2 arm64".to_string(),
                "libpopt0 1.19+dfsg-2+b2 arm64".to_string(),
                "openssl 3.6.3-1 arm64".to_string(),
            ]
        );
    }

    #[test]
    fn cache_key_reacts_to_every_folded_input_but_not_order() {
        let solved = vec!["libc6 2.41-2 arm64".to_string(), "bash 5.2-1 arm64".to_string()];
        let base = cache_key(&solved, &["ov1".into()], &["deb1".into()], "arm64", "forky");
        // Order-insensitive in the solved set (apt resolves the same set either way).
        let reordered = vec!["bash 5.2-1 arm64".to_string(), "libc6 2.41-2 arm64".to_string()];
        assert_eq!(base, cache_key(&reordered, &["ov1".into()], &["deb1".into()], "arm64", "forky"));
        // A different solved version, overlay, repo deb, arch, or suite each moves it.
        let bumped = vec!["libc6 2.41-3 arm64".to_string(), "bash 5.2-1 arm64".to_string()];
        assert_ne!(base, cache_key(&bumped, &["ov1".into()], &["deb1".into()], "arm64", "forky"));
        assert_ne!(base, cache_key(&solved, &["ov2".into()], &["deb1".into()], "arm64", "forky"));
        assert_ne!(base, cache_key(&solved, &["ov1".into()], &["deb2".into()], "arm64", "forky"));
        assert_ne!(base, cache_key(&solved, &["ov1".into()], &["deb1".into()], "amd64", "forky"));
        assert_ne!(base, cache_key(&solved, &["ov1".into()], &["deb1".into()], "arm64", "sid"));
    }

    #[test]
    fn dir_fingerprints_capture_content_mode_and_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::write(root.join("etc/hostname"), "rk1\n").unwrap();
        std::os::unix::fs::symlink("/proc/self/mounts", root.join("etc/mtab")).unwrap();
        let base = dir_fingerprints(root).unwrap();
        // A content change moves the fingerprint set.
        std::fs::write(root.join("etc/hostname"), "changed\n").unwrap();
        assert_ne!(base, dir_fingerprints(root).unwrap());
        // A mode change moves it too.
        std::fs::write(root.join("etc/hostname"), "rk1\n").unwrap();
        std::fs::set_permissions(root.join("etc/hostname"), std::fs::Permissions::from_mode(0o600))
            .unwrap();
        assert_ne!(base, dir_fingerprints(root).unwrap());
        // The symlink target is recorded (a retarget would show up).
        assert!(dir_fingerprints(root).unwrap().iter().any(|f| f.contains("/proc/self/mounts")));
        // A non-existent dir is an empty set, not an error.
        assert!(dir_fingerprints(&root.join("nope")).unwrap().is_empty());
    }

    #[test]
    fn file_fingerprints_hash_content_order_independent() {
        let tmp = tempfile::tempdir().unwrap();
        let a = tmp.path().join("a.deb");
        let b = tmp.path().join("b.deb");
        std::fs::write(&a, b"AAAA").unwrap();
        std::fs::write(&b, b"BBBB").unwrap();
        let ab = file_fingerprints(&[a.clone(), b.clone()]).unwrap();
        let ba = file_fingerprints(&[b.clone(), a.clone()]).unwrap();
        assert_eq!(ab, ba, "order of the deb list must not matter");
        // Changing a deb's bytes changes the set (the whole point: a rebuilt accel
        // deb at the same version still busts the cache).
        std::fs::write(&a, b"AAAB").unwrap();
        assert_ne!(ab, file_fingerprints(&[a, b]).unwrap());
    }

    #[test]
    fn splice_shadow_replaces_only_the_user_line_and_forces_change() {
        let shadow = "root:*:20000:0:99999:7:::\ndebian:!:20000:0:99999:7:::\ndaemon:*:20000::::::\n";
        let out = splice_shadow(shadow, "debian", "$6$salt$hash").unwrap();
        // The debian line carries the hash and last-change 0 (force change at login).
        assert!(out.contains("debian:$6$salt$hash:0:0:99999:7:::\n"));
        // root and daemon are untouched.
        assert!(out.contains("root:*:20000:0:99999:7:::\n"));
        assert!(out.contains("daemon:*:20000::::::\n"));
        // The old locked debian entry is gone.
        assert!(!out.contains("debian:!:"));
        // A missing user is a hard None, never a silent no-op.
        assert!(splice_shadow(shadow, "nobody-here", "$6$x").is_none());
    }

    #[test]
    fn store_round_trips_and_publishes_atomically() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RootfsStore::new(tmp.path());
        let key = cache_key(&["libc6 2.41-2 arm64".into()], &[], &[], "arm64", "forky");
        // Miss before anything is stored.
        assert!(store.get(&key).is_none());
        // Put a (tar, manifest) pair.
        let tar = tmp.path().join("src.tar");
        let manifest = tmp.path().join("src.pkgs");
        std::fs::write(&tar, b"TARBYTES").unwrap();
        std::fs::write(&manifest, b"libc6 2.41-2 arm64 abc\n").unwrap();
        let sink = |_: crate::event::Event| {};
        let step = Step::start(&sink, "rootfs");
        store.put(&key, &tar, &manifest, &step).unwrap();
        // Hit returns both artifacts, byte-identical.
        let hit = store.get(&key).expect("stored entry is a hit");
        assert_eq!(std::fs::read(&hit.tar).unwrap(), b"TARBYTES");
        assert_eq!(std::fs::read(&hit.manifest).unwrap(), b"libc6 2.41-2 arm64 abc\n");
        // No leftover .partial after a successful publish.
        assert!(!tmp.path().join("rootfs").join(format!("{}.partial", key.as_str())).exists());
        // A different key is still a miss.
        let other = cache_key(&["bash 5.2-1 arm64".into()], &[], &[], "arm64", "forky");
        assert!(store.get(&other).is_none());
    }

    #[test]
    fn store_get_requires_both_tar_and_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RootfsStore::new(tmp.path());
        let key = cache_key(&["libc6 2.41-2 arm64".into()], &[], &[], "arm64", "forky");
        // Only the tarball present (a torn write) → miss, never a partial hit.
        let entry = tmp.path().join("rootfs").join(key.as_str());
        std::fs::create_dir_all(&entry).unwrap();
        std::fs::write(entry.join("rootfs.tar"), b"x").unwrap();
        assert!(store.get(&key).is_none());
    }
}
