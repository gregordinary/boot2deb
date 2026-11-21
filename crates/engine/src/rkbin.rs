//! Fetching rkbin blobs from their upstream repository at a pinned commit.
//!
//! The Rockchip blobs are fetched, never stored beside the config. A SoC or device
//! layer names each file and the repository it comes from ([`RkbinUpstream`]). The
//! lock's [`BlobsPin`] pins a commit of that repository plus each file's sha256. This
//! module is the one place those files cross the network:
//!
//! - [`resolve`] is `update`'s half. It picks the commit to pin and returns the named
//!   files' bytes there, for [`blobs::pin`] to hash.
//! - [`ensure_files`] is the u-boot stage's half. It materializes the lock's files
//!   into a commit-addressed cache and refuses any whose hash differs from its pin.
//!
//! The rkbin repository holds every Rockchip SoC's firmware, well over a hundred
//! megabytes at one commit. Both halves therefore fetch the commit depth-1 with no
//! file contents (`--filter=blob:none`), which costs a commit and its trees. They then
//! read only the named files, which git fetches one object at a time. A server without
//! partial-clone support sends the whole commit instead: slower, same bytes.
//!
//! Linux side effects: `git` subprocesses in a scratch repository and writes under the
//! cache root. Every network call is bounded against a stalled transfer, as every
//! other git fetch of the engine is.

use crate::blobs;
use crate::build;
use crate::error::EngineError;
use crate::event::Step;
use boot2deb_core::lock::BlobsPin;
use boot2deb_core::model::RkbinUpstream;
use std::path::{Path, PathBuf};
use std::process::Output;

/// The commit `update` pins and the named files' bytes at it, in the order asked for.
#[derive(Debug)]
pub struct Resolved {
    /// The rkbin commit every file was read at.
    pub commit: String,
    /// `(filename, bytes)` for each requested file.
    pub files: Vec<(String, Vec<u8>)>,
}

/// Pick the rkbin commit to pin for `files` and read each one there.
///
/// `keep` is the commit the previous lock pinned from this same repository and ref.
/// It is kept while it still contains every named file, so an `update` that moves an
/// unrelated axis leaves the rkbin pin where it was. Otherwise the head of
/// `upstream.git_ref` is pinned. rkbin deletes a blob's older versions when it
/// publishes a newer one. Following the branch on every update would therefore make
/// the next blob release fail the update of every recipe still naming the old file.
/// A `keep` commit the server will not serve falls through to the head too: the
/// hashes are recomputed from whichever commit is pinned.
///
/// # Errors
///
/// [`EngineError::RkbinFileMissing`] when the pinned commit lacks a named file: the
/// layer names a file rkbin has deleted, and `outdated` lists what it has now.
/// [`EngineError::GitFailed`] or [`EngineError::GitSpawn`] when the fetch fails.
pub fn resolve(
    upstream: &RkbinUpstream,
    keep: Option<&str>,
    files: &[&str],
) -> Result<Resolved, EngineError> {
    let scratch = Scratch::new()?;
    if let Some(commit) = keep {
        if scratch.fetch_commit(upstream, commit).is_ok()
            && scratch.missing(commit, &upstream.dir, files)?.is_empty()
        {
            return scratch.read_all(upstream, commit, files);
        }
    }
    let head = scratch.fetch(&upstream.git, &upstream.git_ref, &["--depth", "1"])?;
    if let Some(file) = scratch.missing(&head, &upstream.dir, files)?.first() {
        return Err(missing(upstream, &head, file));
    }
    scratch.read_all(upstream, &head, files)
}

/// Make the files `pin` names available under `cache_root/<commit>/` and return that
/// directory, ready for [`blobs::verify_to`].
///
/// The cache is durable and commit-addressed. A file is written there only after its
/// bytes hash to the pin, through a temp sibling of the writer's own and an atomic
/// rename. A present file is therefore always a complete one some lock vouched for, and
/// two builds that fill the same commit at once both succeed. A file already present is
/// re-hashed and reused without touching the network, and a damaged one is fetched
/// again. A build that names one more file of the same commit fetches only that file.
///
/// `dir` is the resolved `[rkbin.upstream].dir` the files live in, and `upstream` the
/// resolved upstream, whose URL the drift gate has already held equal to
/// `pin.source`.
///
/// # Errors
///
/// - [`EngineError::BlobPinMalformed`] for a pin not in `"<file>@sha256:<hex>"` form.
/// - [`EngineError::RkbinFileMissing`] when the commit lacks a named file.
/// - [`EngineError::BlobMismatch`] when the fetched bytes differ from the pin.
/// - A git or I/O error from the fetch or the cache write.
pub fn ensure_files(
    pin: &BlobsPin,
    upstream: &RkbinUpstream,
    cache_root: &Path,
    step: &Step,
) -> Result<PathBuf, EngineError> {
    let dest = cache_root.join(&pin.commit);
    let wanted: Vec<(&str, &str)> = [
        pin.atf.as_deref(),
        Some(pin.tpl.as_str()),
        pin.bl32.as_deref(),
    ]
    .into_iter()
    .flatten()
    .map(|p| {
        blobs::parse_pin(p).ok_or_else(|| EngineError::BlobPinMalformed { pin: p.to_string() })
    })
    .collect::<Result<_, _>>()?;
    let absent: Vec<(&str, &str)> = wanted
        .into_iter()
        .filter(|(file, hex)| !cached(&dest.join(file), hex))
        .collect();
    if absent.is_empty() {
        step.log(format!(
            "rkbin: reusing cached blobs at {}",
            short(&pin.commit)
        ));
        return Ok(dest);
    }
    std::fs::create_dir_all(&dest).map_err(|s| EngineError::io(&dest, s))?;
    crate::gc::sweep_stale_temps(&dest);
    step.log(format!(
        "rkbin: fetching {} from {} at {}",
        absent
            .iter()
            .map(|(f, _)| *f)
            .collect::<Vec<_>>()
            .join(", "),
        pin.source,
        short(&pin.commit)
    ));
    let scratch = Scratch::new()?;
    scratch.fetch_commit(upstream, &pin.commit)?;
    let names: Vec<&str> = absent.iter().map(|(f, _)| *f).collect();
    if let Some(file) = scratch.missing(&pin.commit, &upstream.dir, &names)?.first() {
        return Err(missing(upstream, &pin.commit, file));
    }
    for (file, expected) in absent {
        let bytes = scratch.read(&pin.commit, &upstream.dir, file)?;
        publish(&dest, file, expected, &bytes)?;
    }
    step.log(format!(
        "rkbin: verified and cached at {}",
        short(&pin.commit)
    ));
    Ok(dest)
}

/// Publish `bytes` as `dir/file` once they hash to `expected`, the pin's hex digest.
///
/// Every build of one commit shares `dir`, and two of them can find it cold at once.
/// Each writer therefore stages into a temp of its own, created exclusively beside the
/// target as `.<file>.<random>.partial`, and renames it over the target. The rename
/// replaces atomically, so a reader finds no file or a whole one, never a mix of two
/// writers' bytes. Both writers publish the same verified bytes, so the rename that
/// lands second changes nothing. A rename that fails while the target already hashes
/// to `expected` is a concurrent build's publication, and counts as success.
///
/// The temp is fsynced before the rename and the directory is not. After a crash the
/// name is therefore either absent, a miss the next build fills, or a complete file. A
/// cache owes no more durability than that. A writer killed mid-stage leaves its temp,
/// which [`sweep_stale_temps`](crate::gc::sweep_stale_temps) removes once it is stale.
/// The sweep's age floor keeps it from removing a live writer's temp. Any failure here
/// removes the temp.
///
/// # Errors
///
/// [`EngineError::BlobMismatch`] when `bytes` do not hash to `expected`, with nothing
/// written. An I/O error from staging or renaming the file.
fn publish(dir: &Path, file: &str, expected: &str, bytes: &[u8]) -> Result<(), EngineError> {
    use std::io::Write;
    use std::os::unix::fs::PermissionsExt;
    let actual = blobs::sha256_hex(bytes);
    if actual != expected {
        return Err(EngineError::BlobMismatch {
            filename: file.to_string(),
            expected: expected.to_string(),
            actual,
        });
    }
    let target = dir.join(file);
    // 0o666 under the umask, the mode a plain `fs::write` gives a file, rather than a
    // temp file's 0o600: the temp becomes the cache entry.
    let mut tmp = tempfile::Builder::new()
        .prefix(&format!(".{file}."))
        .suffix(".partial")
        .permissions(std::fs::Permissions::from_mode(0o666))
        .tempfile_in(dir)
        .map_err(|s| EngineError::io(dir, s))?;
    tmp.write_all(bytes)
        .map_err(|s| EngineError::io(tmp.path(), s))?;
    tmp.as_file()
        .sync_all()
        .map_err(|s| EngineError::io(tmp.path(), s))?;
    match tmp.persist(&target) {
        Ok(_) => Ok(()),
        // The error holds the temp, and dropping it removes the file.
        Err(_) if cached(&target, expected) => Ok(()),
        Err(e) => Err(EngineError::io(&target, e.error)),
    }
}

/// Whether `path` exists and hashes to `hex`. Unreadable counts as absent.
fn cached(path: &Path, hex: &str) -> bool {
    blobs::sha256_file(path).is_ok_and(|(_, actual)| actual == hex)
}

/// The error for a named file the commit does not contain.
fn missing(upstream: &RkbinUpstream, commit: &str, file: &str) -> EngineError {
    EngineError::RkbinFileMissing {
        file: file.to_string(),
        dir: upstream.dir.clone(),
        repo: upstream.git.clone(),
        commit: commit.to_string(),
    }
}

/// The first 12 hex digits of a commit, for log lines.
fn short(commit: &str) -> &str {
    commit.get(..12).unwrap_or(commit)
}

/// A throwaway partial-clone repository the fetches land in. Removed on drop.
struct Scratch {
    tmp: tempfile::TempDir,
}

impl Scratch {
    fn new() -> Result<Self, EngineError> {
        let tmp = tempfile::Builder::new()
            .prefix("boot2deb-rkbin-")
            .tempdir()
            .map_err(|s| EngineError::io(&std::env::temp_dir(), s))?;
        let scratch = Scratch { tmp };
        scratch.git(
            &["init", "-q"],
            "init a scratch repository for rkbin",
            false,
        )?;
        Ok(scratch)
    }

    /// Run `git` in the scratch repository. `network` bounds a stalled transfer, for
    /// the commands that fetch: `fetch` itself and the reads that fill in a blob.
    fn git(&self, args: &[&str], context: &str, network: bool) -> Result<Output, EngineError> {
        let mut cmd = crate::git::command(Some(self.tmp.path()));
        if network {
            build::bound_git_network(&mut cmd);
        }
        let out = cmd
            .args(args)
            .output()
            .map_err(|source| EngineError::GitSpawn {
                context: context.to_string(),
                source,
            })?;
        if out.status.success() {
            Ok(out)
        } else {
            Err(EngineError::GitFailed {
                context: context.to_string(),
                status: out.status.code(),
                stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
            })
        }
    }

    /// Fetch `rev` of `url` with no file contents and return the commit it named.
    fn fetch(&self, url: &str, rev: &str, extra: &[&str]) -> Result<String, EngineError> {
        build::reject_optionlike("source", url)?;
        build::reject_optionlike("ref", rev)?;
        let mut args = vec!["fetch", "--quiet", "--filter=blob:none"];
        args.extend_from_slice(extra);
        args.extend_from_slice(&["--end-of-options", url, rev]);
        self.git(&args, &format!("fetch rkbin {rev} from {url}"), true)?;
        let out = self.git(
            &["rev-parse", "--verify", "FETCH_HEAD^{commit}"],
            "read the fetched rkbin commit",
            false,
        )?;
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    /// Fetch exactly `commit`. A server that serves a reachable commit by its id
    /// (GitHub does) takes one depth-1 fetch. One that does not is asked for the
    /// whole history of the upstream's ref, still without file contents, and the
    /// commit must be in it.
    fn fetch_commit(&self, upstream: &RkbinUpstream, commit: &str) -> Result<(), EngineError> {
        if self
            .fetch(&upstream.git, commit, &["--depth", "1"])
            .is_ok_and(|got| got == commit)
        {
            return Ok(());
        }
        self.fetch(&upstream.git, &upstream.git_ref, &[])?;
        self.git(
            &["cat-file", "-e", &format!("{commit}^{{commit}}")],
            &format!("find rkbin commit {commit} in {} history", upstream.git_ref),
            false,
        )
        .map(drop)
    }

    /// The names in `files` that `commit` has no regular file for under `dir`, in the
    /// order given. Read from the fetched trees, so no file content is fetched.
    fn missing<'f>(
        &self,
        commit: &str,
        dir: &str,
        files: &[&'f str],
    ) -> Result<Vec<&'f str>, EngineError> {
        let prefix = format!("{}/", dir.trim_end_matches('/'));
        let out = self.git(
            &["ls-tree", "--end-of-options", commit, "--", &prefix],
            &format!("list {prefix} at rkbin {commit}"),
            false,
        )?;
        let listing = String::from_utf8_lossy(&out.stdout);
        let present: std::collections::BTreeSet<&str> = listing
            .lines()
            .filter_map(|line| {
                // `<mode> <type> <object>\t<path>`: only regular files are blobs to pin.
                let (meta, path) = line.split_once('\t')?;
                (meta.split(' ').nth(1) == Some("blob")).then_some(path)
            })
            .filter_map(|path| path.strip_prefix(&prefix))
            .collect();
        Ok(files
            .iter()
            .copied()
            .filter(|f| !present.contains(f))
            .collect())
    }

    /// The bytes of `dir/file` at `commit`. git fetches the one object on demand.
    fn read(&self, commit: &str, dir: &str, file: &str) -> Result<Vec<u8>, EngineError> {
        let path = format!("{commit}:{}/{file}", dir.trim_end_matches('/'));
        Ok(self
            .git(
                &["cat-file", "blob", &path],
                &format!("read rkbin {path}"),
                true,
            )?
            .stdout)
    }

    /// Read every file, which [`missing`](Self::missing) has already found present.
    fn read_all(
        &self,
        upstream: &RkbinUpstream,
        commit: &str,
        files: &[&str],
    ) -> Result<Resolved, EngineError> {
        let files = files
            .iter()
            .map(|f| Ok((f.to_string(), self.read(commit, &upstream.dir, f)?)))
            .collect::<Result<_, EngineError>>()?;
        Ok(Resolved {
            commit: commit.to_string(),
            files,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;
    use std::process::Command;

    /// Run git in `dir`, asserting success, and return trimmed stdout.
    fn git_in(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// An rkbin-shaped origin: `bin/rk35/` holding the given files, one commit per
    /// call to [`commit`]. It serves partial clones and commits by id, as GitHub does.
    fn origin(dir: &Path) -> RkbinUpstream {
        std::fs::create_dir_all(dir.join("bin/rk35")).unwrap();
        git_in(dir, &["init", "-q", "-b", "master"]);
        git_in(dir, &["config", "user.email", "t@t"]);
        git_in(dir, &["config", "user.name", "t"]);
        git_in(dir, &["config", "uploadpack.allowFilter", "true"]);
        git_in(dir, &["config", "uploadpack.allowAnySHA1InWant", "true"]);
        RkbinUpstream {
            git: format!("file://{}", dir.display()),
            git_ref: "master".into(),
            dir: "bin/rk35".into(),
        }
    }

    /// Replace the origin's `bin/rk35/` with `files` and commit; return the commit.
    fn commit(dir: &Path, files: &[(&str, &[u8])]) -> String {
        let rk35 = dir.join("bin/rk35");
        std::fs::remove_dir_all(&rk35).unwrap();
        std::fs::create_dir_all(&rk35).unwrap();
        for (name, bytes) in files {
            std::fs::write(rk35.join(name), bytes).unwrap();
        }
        std::fs::write(dir.join("README"), files.len().to_string()).unwrap();
        git_in(dir, &["add", "-A"]);
        git_in(dir, &["commit", "-q", "-m", "release"]);
        git_in(dir, &["rev-parse", "HEAD"])
    }

    fn lock_pin(up: &RkbinUpstream, commit: &str, tpl: &str, bytes: &[u8]) -> BlobsPin {
        BlobsPin {
            source: up.git.clone(),
            reference: up.git_ref.clone(),
            commit: commit.to_string(),
            atf: None,
            tpl: blobs::pin(tpl, bytes),
            bl32: None,
        }
    }

    #[test]
    fn update_keeps_a_commit_that_still_has_every_file_and_moves_when_it_does_not() {
        let tmp = tempfile::tempdir().unwrap();
        let o = tmp.path().join("rkbin");
        let up = origin(&o);
        let first = commit(
            &o,
            &[("bl31_v1.elf", b"bl31 one"), ("ddr_v1.bin", b"ddr one")],
        );
        // rkbin replaces a blob's old version when it publishes a new one.
        let second = commit(
            &o,
            &[("bl31_v2.elf", b"bl31 two"), ("ddr_v1.bin", b"ddr one")],
        );

        // The first update has nothing to keep: the head is pinned.
        let r = resolve(&up, None, &["ddr_v1.bin"]).unwrap();
        assert_eq!(r.commit, second);
        assert_eq!(
            r.files,
            vec![("ddr_v1.bin".to_string(), b"ddr one".to_vec())]
        );

        // A later update keeps the locked commit while it has every file, even though
        // the branch has moved on and the old BL31 is gone from the head.
        let r = resolve(&up, Some(&first), &["bl31_v1.elf", "ddr_v1.bin"]).unwrap();
        assert_eq!(r.commit, first);
        assert_eq!(r.files[0].1, b"bl31 one");

        // A layer naming the new file moves the pin to the head.
        let r = resolve(&up, Some(&first), &["bl31_v2.elf", "ddr_v1.bin"]).unwrap();
        assert_eq!(r.commit, second);

        // A file the head no longer has is named in the error.
        match resolve(&up, None, &["bl31_v1.elf"]) {
            Err(EngineError::RkbinFileMissing { file, commit, .. }) => {
                assert_eq!(file, "bl31_v1.elf");
                assert_eq!(commit, second);
            }
            other => panic!("expected RkbinFileMissing, got {other:?}"),
        }
    }

    #[test]
    fn ensure_files_fetches_verifies_caches_and_then_stays_offline() {
        let tmp = tempfile::tempdir().unwrap();
        let o = tmp.path().join("rkbin");
        let up = origin(&o);
        let first = commit(&o, &[("ddr.bin", b"ddr"), ("other.bin", b"not asked for")]);
        commit(&o, &[("ddr2.bin", b"newer")]);
        let cache = tmp.path().join("cache");
        let sink = |_e: Event| {};
        let pin = lock_pin(&up, &first, "ddr.bin", b"ddr");

        // A historical commit, fetched by id; only the named file is materialized.
        let dir = ensure_files(&pin, &up, &cache, &Step::start(&sink, "t")).unwrap();
        assert_eq!(dir, cache.join(&first));
        assert_eq!(std::fs::read(dir.join("ddr.bin")).unwrap(), b"ddr");
        assert!(!dir.join("other.bin").exists());

        // A hit reads no network: the origin is gone and the call still succeeds.
        std::fs::remove_dir_all(&o).unwrap();
        let again = ensure_files(&pin, &up, &cache, &Step::start(&sink, "t")).unwrap();
        assert_eq!(again, dir);

        // A damaged cache entry is not trusted: it is fetched again, which now fails.
        std::fs::write(dir.join("ddr.bin"), b"damaged").unwrap();
        assert!(ensure_files(&pin, &up, &cache, &Step::start(&sink, "t")).is_err());
    }

    #[test]
    fn ensure_files_refuses_bytes_that_differ_from_the_pin() {
        let tmp = tempfile::tempdir().unwrap();
        let o = tmp.path().join("rkbin");
        let up = origin(&o);
        let c = commit(&o, &[("ddr.bin", b"what upstream has")]);
        let cache = tmp.path().join("cache");
        let sink = |_e: Event| {};
        let pin = lock_pin(&up, &c, "ddr.bin", b"what the lock expects");
        assert!(matches!(
            ensure_files(&pin, &up, &cache, &Step::start(&sink, "t")),
            Err(EngineError::BlobMismatch { .. })
        ));
        // Nothing unverified is left for a later build to trust.
        assert!(!cache.join(&c).join("ddr.bin").exists());
    }

    /// Two builds sharing a pin can both find its cache cold. Every round clears the
    /// entry and has two writers publish it at once. Every publish succeeds, the entry
    /// is whole after each round, and no temp outlives the race.
    #[test]
    fn concurrent_cold_cache_publishes_of_one_pin_all_succeed() {
        const ROUNDS: usize = 50;
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let target = dir.join("ddr.bin");
        // Large enough that one writer is still staging while the other renames.
        let bytes: Vec<u8> = (0..256 * 1024u32).map(|i| (i % 251) as u8).collect();
        let hex = blobs::sha256_hex(&bytes);
        let barrier = std::sync::Barrier::new(2);
        let failures: Vec<String> = std::thread::scope(|s| {
            let writers: Vec<_> = (0..2)
                .map(|_| {
                    s.spawn(|| {
                        // Failures are collected, not asserted: a writer that panicked
                        // would leave the other waiting at the barrier for ever.
                        let mut failures = Vec::new();
                        for round in 0..ROUNDS {
                            if barrier.wait().is_leader() {
                                let _ = std::fs::remove_file(&target);
                            }
                            barrier.wait();
                            if let Err(e) = publish(dir, "ddr.bin", &hex, &bytes) {
                                failures.push(format!("round {round}: {e}"));
                            }
                            barrier.wait();
                            if !cached(&target, &hex) {
                                failures.push(format!("round {round}: the entry is not whole"));
                            }
                        }
                        failures
                    })
                })
                .collect();
            writers
                .into_iter()
                .flat_map(|w| w.join().unwrap())
                .collect()
        });
        assert!(failures.is_empty(), "{failures:#?}");
        let left: Vec<String> = std::fs::read_dir(dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(left, vec!["ddr.bin"], "a temp outlived its publish");
    }

    #[test]
    fn publish_refuses_bytes_that_differ_from_the_pin_and_writes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let expected = blobs::sha256_hex(b"what the lock expects");
        assert!(matches!(
            publish(tmp.path(), "ddr.bin", &expected, b"something else"),
            Err(EngineError::BlobMismatch { .. })
        ));
        assert_eq!(std::fs::read_dir(tmp.path()).unwrap().count(), 0);
    }

    #[test]
    fn a_missing_file_and_a_subdirectory_of_the_same_name_are_both_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let o = tmp.path().join("rkbin");
        let up = origin(&o);
        commit(&o, &[("ddr.bin", b"ddr")]);
        std::fs::create_dir_all(o.join("bin/rk35/sub.bin")).unwrap();
        std::fs::write(o.join("bin/rk35/sub.bin/x"), b"x").unwrap();
        git_in(&o, &["add", "-A"]);
        git_in(&o, &["commit", "-q", "-m", "a directory named like a blob"]);
        let head = git_in(&o, &["rev-parse", "HEAD"]);
        let scratch = Scratch::new().unwrap();
        scratch.fetch_commit(&up, &head).unwrap();
        assert_eq!(
            scratch
                .missing(&head, "bin/rk35", &["ddr.bin", "sub.bin", "gone.bin"])
                .unwrap(),
            vec!["sub.bin", "gone.bin"]
        );
    }
}
