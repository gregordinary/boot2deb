//! Rootfs early-cutoff cache — skip the expensive bootstrap when the *solved*
//! package set is unchanged, without ever serving a stale solve.
//!
//! The rootfs node's cost is dominated by the qemu-emulated package configure
//! (~250 s), not by the dependency solve (seconds). So this caches on the **solved
//! manifest** rather than on the input package *names* ("early cutoff"): the
//! provisioner resolves the plan up front — the exact versions the current mirror
//! offers, without downloading — that solved set hashes into a [`Signature`], and a
//! stored rootfs is reused only when the hash matches. A moved mirror resolves
//! different versions → a different key → an automatic fresh bootstrap, so a cache
//! hit can never reflect an out-of-date mirror.
//!
//! **Soundness of keying on the solved set.** Debian archive versions are
//! immutable — a given `name version` is byte-identical across every mirror and
//! forever — so `name version arch` uniquely identifies a mirror `.deb`'s content.
//! The build's own accel `.deb`s (from the local repo) are *not* archive
//! packages, so their bytes are folded in directly ([`RootfsStore`] callers pass
//! their sha256s), as is the assembled overlay tree. What is deliberately **not**
//! folded is the per-image first-boot password: it is unique per
//! build by design, so it is applied *after* restore (the rootfs node splices it
//! into `/etc/shadow`, [`splice_shadow`]), keeping the cached tree reusable.
//!
//! **The tree is not only its packages.** Two further inputs shape it without moving
//! any package version, so both are folded. The **interpreter**: on a cross host every
//! maintainer script runs under the host's `qemu-user`, and so does everything they
//! invoke — `update-initramfs` writing the initrd the image boots, `ldconfig`,
//! `locale-gen`, a depthcharge board's kernel signing — so those bytes are the
//! interpreter's as much as the package's. The **feature apt repositories**: the
//! provisioner writes each one's `sources.list.d` entry and its keyring into the tree,
//! so a re-pointed URI or a rotated key changes the image while the solve stands still.
//!
//! Both fold only when present, and an absent fold contributes nothing rather than an
//! empty record — so a native build can never key alike with a cross build whose
//! interpreter is merely missing, and a build with no feature repository folds no such
//! record at all. Labels and values are length-prefixed
//! ([`SignatureBuilder`]), so an absent fold cannot be forged by a present one.
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

/// Stage-recipe version for the rootfs cache key: the node's own logic, as an input.
///
/// An entry stored under a different version is never served — the key covers the
/// declared inputs, and this covers everything about the node that turns those inputs
/// into a tree. Bump it whenever a change alters the produced tree for unchanged
/// inputs: the overlay merge order, which config files the node generates, how the
/// bootstrap establishes ownership. Without the bump a hit would restore a tree the
/// current node would not produce, which is the one thing a content cache must not do.
///
/// The provisioner library counts as the node's logic: ferroday-cage decides the
/// bootstrapped tree's dpkg state, apt configuration, and configure ordering, so a
/// dependency bump that changes the emitted tree for unchanged inputs is a bump here
/// too.
const ROOTFS_STAGE_VERSION: u32 = 7;

/// Everything that determines the produced rootfs tree *except* the per-image
/// password (applied on restore) — the inputs [`cache_key`] hashes.
///
/// A struct rather than positional arguments because most of these are
/// `&[String]`/`&str` shaped: a swapped pair would silently change every key rather
/// than fail to compile, and a silent key change is the one failure mode a cache
/// cannot recover from on its own.
#[derive(Debug, Clone, Copy)]
pub struct CacheKeyInputs<'a> {
    /// The solved package set, `name version arch` per package, from the resolved
    /// [`Plan`](ferroday_cage::provision::debian::Plan). Order-insensitive: apt reaches
    /// the same set either way.
    pub solved: &'a [String],
    /// Content fingerprints of the assembled overlay trees ([`dir_fingerprints`]),
    /// including the config the node generates into them.
    pub overlay: &'a [String],
    /// Content hashes of the build's local-repo `.deb`s ([`file_fingerprints`]) — the
    /// non-archive packages whose version carries no immutability guarantee.
    pub repo_debs: &'a [String],
    /// One opaque record per feature apt repository: its identity and the content of
    /// the keyring it is verified against, both of which the provisioner writes into
    /// the tree. Empty when the build's features contribute no repository, and an
    /// empty set folds nothing at all.
    pub apt_sources: &'a [String],
    /// Target Debian architecture.
    pub arch: &'a str,
    /// Target Debian suite.
    pub suite: &'a str,
    /// Identity of the interpreter that executes the target's maintainer scripts
    /// ([`RootfsOptions::interpreter_id`](crate::rootfs::RootfsOptions::interpreter_id)),
    /// or `None` on a native host, where nothing is interpreted and no such input
    /// exists.
    pub interpreter: Option<&'a str>,
}

/// The rootfs cache key: a [`Signature`] over [`CacheKeyInputs`]. Pure.
pub fn cache_key(inputs: &CacheKeyInputs) -> Signature {
    let mut b = SignatureBuilder::new("rootfs", ROOTFS_STAGE_VERSION);
    b.fold_scalar("arch", inputs.arch);
    b.fold_scalar("suite", inputs.suite);
    b.fold_set("solved", inputs.solved);
    b.fold_set("overlay", inputs.overlay);
    b.fold_set("repo_debs", inputs.repo_debs);
    // Order-insensitive: each repository writes its own distinct pair of files, so
    // their order is not a property of the tree.
    if !inputs.apt_sources.is_empty() {
        b.fold_set("apt_sources", inputs.apt_sources);
    }
    if let Some(interpreter) = inputs.interpreter {
        b.fold_scalar("interpreter", interpreter);
    }
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
/// `<key>/` holding `rootfs.tar` + `manifest.pkgs`; it is published atomically
/// (staged in a pid-distinct `.partial` temp, then renamed), so an interrupted
/// store never leaves a half-written entry a later build would trust, and two
/// concurrent builds of the same key cannot clobber each other's staging — the same
/// discipline as [`crate::artstore::ArtifactStore`].
pub struct RootfsStore {
    /// The `<cache>/rootfs` root the entries live under.
    root: PathBuf,
}

impl RootfsStore {
    /// A store rooted at `<cache_dir>/rootfs`. Opportunistically sweeps stale
    /// `<key>.partial` temps a hard-killed `put` may have left.
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

    /// Store `tar` + `manifest` under `key`, replacing any prior entry — a
    /// `--refresh-rootfs` rebuild must refresh the stored bytes, so (unlike
    /// [`crate::artstore::ArtifactStore::put`]) an existing entry is not kept.
    ///
    /// Concurrency discipline: staging uses a pid-distinct `.partial`
    /// temp, so two builds of the same key cannot delete each other's in-flight
    /// staging; a prior entry is atomically moved aside rather than deleted in
    /// place, so a concurrent `get`'s exposure is one rename, not the duration
    /// of a recursive delete; and losing the publish rename to a concurrent
    /// `put` keeps the winner's complete entry — content is signature-keyed, so
    /// any complete entry for `key` is equivalent.
    pub fn put(
        &self,
        key: &Signature,
        tar: &Path,
        manifest: &Path,
        step: &Step,
    ) -> Result<(), EngineError> {
        let entry = self.entry(key);
        let pid = std::process::id();
        let partial = self.root.join(format!(".{}.{pid}.partial", key.as_str()));
        let _ = std::fs::remove_dir_all(&partial);
        std::fs::create_dir_all(&partial).map_err(|s| EngineError::io(&partial, s))?;
        std::fs::copy(tar, partial.join("rootfs.tar")).map_err(|s| EngineError::io(tar, s))?;
        std::fs::copy(manifest, partial.join("manifest.pkgs"))
            .map_err(|s| EngineError::io(manifest, s))?;
        // Move any prior entry aside (atomic; the `.partial` infix keeps it
        // sweepable if we crash before the cleanup below).
        let replaced = self
            .root
            .join(format!(".{}.{pid}.partial-replaced", key.as_str()));
        let _ = std::fs::remove_dir_all(&replaced);
        match std::fs::rename(&entry, &replaced) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                let _ = std::fs::remove_dir_all(&partial);
                return Err(EngineError::io(&entry, e));
            }
        }
        match std::fs::rename(&partial, &entry) {
            Ok(()) => {}
            // Lost the publish race to a concurrent put of the same key: theirs
            // is complete and key-equivalent, so drop ours.
            Err(_) if entry.join("rootfs.tar").is_file() => {
                let _ = std::fs::remove_dir_all(&partial);
            }
            Err(e) => {
                let _ = std::fs::remove_dir_all(&partial);
                let _ = std::fs::remove_dir_all(&replaced);
                return Err(EngineError::io(&entry, e));
            }
        }
        let _ = std::fs::remove_dir_all(&replaced);
        step.log(format!("cached rootfs {} in the store", key.short()));
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_replaces_the_entry_and_spares_another_builds_staging() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RootfsStore::new(tmp.path());
        let sink = |_e: crate::event::Event| {};
        let step = crate::event::Step::start(&sink, "rootfs");
        let key = crate::signature::SignatureBuilder::new("t", 1).finish();

        let tar_one = tmp.path().join("a.tar");
        let manifest = tmp.path().join("a.pkgs");
        std::fs::write(&tar_one, b"tar-one").unwrap();
        std::fs::write(&manifest, b"man-one").unwrap();

        // A concurrent build's pid-distinct staging must survive our put: deleting a
        // shared-name partial on entry would clobber the other build's staging.
        let foreign = tmp
            .path()
            .join("rootfs")
            .join(format!(".{}.999999.partial", key.as_str()));
        std::fs::create_dir_all(&foreign).unwrap();
        std::fs::write(foreign.join("rootfs.tar"), b"in-flight").unwrap();

        store.put(&key, &tar_one, &manifest, &step).unwrap();
        let hit = store.get(&key).expect("stored entry");
        assert_eq!(std::fs::read(&hit.tar).unwrap(), b"tar-one");
        assert!(
            foreign.join("rootfs.tar").exists(),
            "foreign staging must survive"
        );

        // A re-put replaces the entry (--refresh-rootfs refreshes stored bytes)...
        let tar_two = tmp.path().join("b.tar");
        std::fs::write(&tar_two, b"tar-two").unwrap();
        store.put(&key, &tar_two, &manifest, &step).unwrap();
        assert_eq!(
            std::fs::read(store.get(&key).unwrap().tar).unwrap(),
            b"tar-two"
        );

        // ...and leaves nothing behind but the entry and the foreign staging.
        let leftovers: Vec<String> = std::fs::read_dir(tmp.path().join("rootfs"))
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != key.as_str() && !n.contains(".999999."))
            .collect();
        assert!(leftovers.is_empty(), "temps left behind: {leftovers:?}");
    }

    /// The inputs the key tests vary one field of at a time (`..base`).
    fn inputs<'a>(
        solved: &'a [String],
        overlay: &'a [String],
        repo_debs: &'a [String],
    ) -> CacheKeyInputs<'a> {
        CacheKeyInputs {
            solved,
            overlay,
            repo_debs,
            apt_sources: &[],
            arch: "arm64",
            suite: "forky",
            interpreter: None,
        }
    }

    #[test]
    fn cache_key_reacts_to_every_folded_input_but_not_order() {
        let solved = vec![
            "libc6 2.41-2 arm64".to_string(),
            "bash 5.2-1 arm64".to_string(),
        ];
        let overlay = vec!["ov1".to_string()];
        let debs = vec!["deb1".to_string()];
        let base = inputs(&solved, &overlay, &debs);
        let key = cache_key(&base);
        // Order-insensitive in the solved set (apt resolves the same set either way).
        let reordered = vec![
            "bash 5.2-1 arm64".to_string(),
            "libc6 2.41-2 arm64".to_string(),
        ];
        assert_eq!(
            key,
            cache_key(&CacheKeyInputs {
                solved: &reordered,
                ..base
            })
        );
        // A different solved version, overlay, repo deb, arch, or suite each moves it.
        let bumped = vec![
            "libc6 2.41-3 arm64".to_string(),
            "bash 5.2-1 arm64".to_string(),
        ];
        let other_overlay = vec!["ov2".to_string()];
        let other_deb = vec!["deb2".to_string()];
        assert_ne!(
            key,
            cache_key(&CacheKeyInputs {
                solved: &bumped,
                ..base
            })
        );
        assert_ne!(
            key,
            cache_key(&CacheKeyInputs {
                overlay: &other_overlay,
                ..base
            })
        );
        assert_ne!(
            key,
            cache_key(&CacheKeyInputs {
                repo_debs: &other_deb,
                ..base
            })
        );
        assert_ne!(
            key,
            cache_key(&CacheKeyInputs {
                arch: "amd64",
                ..base
            })
        );
        assert_ne!(
            key,
            cache_key(&CacheKeyInputs {
                suite: "sid",
                ..base
            })
        );
    }

    #[test]
    fn the_interpreter_and_feature_repositories_key_only_when_present() {
        let solved = vec!["libc6 2.41-2 arm64".to_string()];
        let overlay = vec!["ov1".to_string()];
        let debs = vec!["deb1".to_string()];
        let base = inputs(&solved, &overlay, &debs);
        let native = cache_key(&base);

        // The interpreter that configures the tree is an input: a cross host keys apart
        // from a native one, and a qemu upgrade keys apart from the version before it —
        // a tree configured under one interpreter is never restored for the other.
        let cross = cache_key(&CacheKeyInputs {
            interpreter: Some("qemu-aarch64 version 9.2.0"),
            ..base
        });
        let upgraded = cache_key(&CacheKeyInputs {
            interpreter: Some("qemu-aarch64 version 9.3.0"),
            ..base
        });
        assert_ne!(native, cross, "a cross build's interpreter must key");
        assert_ne!(cross, upgraded, "a qemu upgrade must key");

        // Feature repositories reach the image as a sources entry and a keyring, so
        // their content moves the key — including a rotated key at an unchanged URI.
        let one = vec!["jellyfin\0https://repo.jellyfin.org/debian\0trixie\0main\0aaa".to_string()];
        let rotated =
            vec!["jellyfin\0https://repo.jellyfin.org/debian\0trixie\0main\0bbb".to_string()];
        let with_one = cache_key(&CacheKeyInputs {
            apt_sources: &one,
            ..base
        });
        assert_ne!(native, with_one, "a feature repository must key");
        assert_ne!(
            with_one,
            cache_key(&CacheKeyInputs {
                apt_sources: &rotated,
                ..base
            }),
            "a rotated keyring must key"
        );

        // Their order is not a property of the tree — each writes its own files.
        let second = "extra\0https://example.invalid/debian\0forky\0main\0ccc".to_string();
        let ab = vec![one[0].clone(), second.clone()];
        let ba = vec![second, one[0].clone()];
        assert_eq!(
            cache_key(&CacheKeyInputs {
                apt_sources: &ab,
                ..base
            }),
            cache_key(&CacheKeyInputs {
                apt_sources: &ba,
                ..base
            })
        );
    }

    #[test]
    fn the_key_is_stable_for_a_fixed_input_set() {
        // A golden key, and the guard on the rule above: an absent optional fold must
        // contribute *nothing*, so a build with neither an interpreter nor a feature
        // repository keys on exactly these bytes however many optional inputs are added
        // later. This hash may only move with a deliberate ROOTFS_STAGE_VERSION bump —
        // any other change to it orphans every stored rootfs on every host.
        let solved = vec!["libc6 2.41-2 arm64".to_string()];
        let overlay = vec!["etc/hostname\x00644\0abc".to_string()];
        let debs = vec!["def".to_string()];
        assert_eq!(
            cache_key(&inputs(&solved, &overlay, &debs)).as_str(),
            "980530abb57b9f28c9313a6f66207673f9591539187fc5c07370504139539389"
        );
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
        std::fs::set_permissions(
            root.join("etc/hostname"),
            std::fs::Permissions::from_mode(0o600),
        )
        .unwrap();
        assert_ne!(base, dir_fingerprints(root).unwrap());
        // The symlink target is recorded (a retarget would show up).
        assert!(dir_fingerprints(root)
            .unwrap()
            .iter()
            .any(|f| f.contains("/proc/self/mounts")));
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
        let shadow =
            "root:*:20000:0:99999:7:::\ndebian:!:20000:0:99999:7:::\ndaemon:*:20000::::::\n";
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
        let solved = vec!["libc6 2.41-2 arm64".to_string()];
        let key = cache_key(&inputs(&solved, &[], &[]));
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
        assert_eq!(
            std::fs::read(&hit.manifest).unwrap(),
            b"libc6 2.41-2 arm64 abc\n"
        );
        // No leftover .partial after a successful publish.
        assert!(!tmp
            .path()
            .join("rootfs")
            .join(format!("{}.partial", key.as_str()))
            .exists());
        // A different key is still a miss.
        let other_solved = vec!["bash 5.2-1 arm64".to_string()];
        let other = cache_key(&inputs(&other_solved, &[], &[]));
        assert!(store.get(&other).is_none());
    }

    #[test]
    fn store_get_requires_both_tar_and_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let store = RootfsStore::new(tmp.path());
        let solved = vec!["libc6 2.41-2 arm64".to_string()];
        let key = cache_key(&inputs(&solved, &[], &[]));
        // Only the tarball present (a torn write) → miss, never a partial hit.
        let entry = tmp.path().join("rootfs").join(key.as_str());
        std::fs::create_dir_all(&entry).unwrap();
        std::fs::write(entry.join("rootfs.tar"), b"x").unwrap();
        assert!(store.get(&key).is_none());
    }
}
