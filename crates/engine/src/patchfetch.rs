//! Auto-fetch of the `patches` repo at the lock-pinned commit via `gix`.
//!
//! When a build has no local patches checkout, this materializes the exact
//! `lock.patches.commit` into a durable, commit-addressed cache so the series can
//! be verified and applied like any local checkout — the North-Star "selecting a
//! device auto-fetches the right patches". The clone is pure-Rust (`gix` over
//! rustls): a full history fetch (the repo is small, so no sparse checkout),
//! then a **detached** worktree checkout of the pinned commit's tree, so a later
//! `git rev-parse HEAD` returns that commit and `git status` is clean — exactly
//! what the verify gate (`build::verify_patches_pin`) expects of a pinned checkout.
//!
//! Patches are never silently skipped: an unreachable commit or an offline fetch
//! is a hard [`EngineError::PatchesFetch`], and the caller surfaces the pinned
//! commit so the user can retry or point `--patches-path` at a local checkout.

use crate::error::EngineError;
use crate::event::Step;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// Wall-clock cap on the `gix` patches fetch. `gix` polls an interrupt
/// flag during transfer, so a watchdog trips it after this deadline and a stalled
/// remote aborts with an error instead of hanging `build`/`update` forever. Generous
/// enough for a full-history clone of the small `patches` repo on a slow link.
const GIX_FETCH_TIMEOUT: Duration = Duration::from_secs(180);

/// Materialize the `patches` repo at `commit` from `url` into a commit-addressed
/// directory under `cache_root`, returning that checkout's path.
///
/// The cache is durable (it lives outside any recipe work dir, so it survives
/// `clean` and is shared across builds) and content-addressed by the commit, so a
/// present `cache_root/<commit>` is always a complete checkout at that commit —
/// the fetch stages into a temporary sibling and atomically renames on success, so
/// an interrupted clone never leaves a half-materialized tree a later run trusts.
///
/// A hit (the directory already exists) returns immediately without touching the
/// network. A miss performs a full pure-Rust `gix` clone (all history, so an
/// arbitrary pinned ancestor is reachable), checks out the pinned commit detached,
/// and renames it into place.
pub fn fetch_profile(
    url: &str,
    commit: &str,
    cache_root: &Path,
    step: &Step,
) -> Result<PathBuf, EngineError> {
    // A URL beginning with `-` could be read as an option by a downstream tool
    // (defence in depth, matching the git-source guards).
    crate::build::reject_optionlike("patches-url", url)?;
    let dest = cache_root.join(commit);
    if dest.exists() {
        step.log(format!("patches: reusing cached checkout at {commit}"));
        return Ok(dest);
    }
    std::fs::create_dir_all(cache_root).map_err(|source| EngineError::io(cache_root, source))?;
    // Sweep `.fetch-*` staging dirs a hard-killed clone may have left before starting a
    // fresh one; the durable patches cache survives `clean`, so leftovers
    // would otherwise accrue.
    crate::gc::sweep_stale_temps(cache_root);

    // Stage into a temp sibling on the same filesystem, then rename atomically so
    // `dest` only ever appears fully checked out.
    let staging = tempfile::Builder::new()
        .prefix(".fetch-")
        .tempdir_in(cache_root)
        .map_err(|source| EngineError::io(cache_root, source))?;
    let repo_dir = staging.path().join("repo");

    step.log(format!("patches: fetching {url} at {commit}"));
    clone_at_commit(url, &repo_dir, commit).map_err(|detail| EngineError::PatchesFetch {
        url: url.to_string(),
        commit: commit.to_string(),
        detail,
    })?;

    std::fs::rename(&repo_dir, &dest).map_err(|source| EngineError::io(&dest, source))?;
    step.log(format!("patches: checked out {commit}"));
    Ok(dest)
}

/// Full pure-Rust `gix` clone of `url` into `dir`, then a **detached** checkout of
/// `commit`'s tree into the worktree. Errors are flattened to a `String` (gix has
/// many distinct error types) for [`EngineError::PatchesFetch`].
///
/// The fetch is not shallow: a normal clone downloads every branch tip's full
/// history, so any commit reachable from a fetched branch — including an arbitrary
/// ancestor the lock pins — is present in the object database. `want <sha>` is
/// deliberately not used (most servers gate it behind `uploadpack.allow*SHA1InWant`).
fn clone_at_commit(url: &str, dir: &Path, commit: &str) -> Result<(), String> {
    // Shared interrupt flag: gix polls it during the fetch/checkout, and a watchdog
    // thread trips it after GIX_FETCH_TIMEOUT so a stalled remote aborts rather than
    // hangs. `done` stops the watchdog once the work finishes.
    let interrupt = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let watchdog = {
        let interrupt = Arc::clone(&interrupt);
        let done = Arc::clone(&done);
        std::thread::spawn(move || {
            let start = Instant::now();
            while !done.load(Ordering::Relaxed) {
                if start.elapsed() >= GIX_FETCH_TIMEOUT {
                    interrupt.store(true, Ordering::Relaxed);
                    break;
                }
                std::thread::sleep(Duration::from_millis(200));
            }
        })
    };
    let result = clone_at_commit_inner(url, dir, commit, &interrupt);
    done.store(true, Ordering::Relaxed);
    let _ = watchdog.join();
    // A tripped interrupt surfaces as a gix error; name the real cause.
    if interrupt.load(Ordering::Relaxed) {
        return Err(format!(
            "fetch exceeded the {}s timeout (remote stalled)",
            GIX_FETCH_TIMEOUT.as_secs()
        ));
    }
    result
}

/// The fetch + detached checkout itself, cancellable via `interrupt`.
fn clone_at_commit_inner(
    url: &str,
    dir: &Path,
    commit: &str,
    interrupt: &AtomicBool,
) -> Result<(), String> {
    // Fetch the pack + refs but leave the worktree untouched — `main_worktree`
    // would check out the remote's default HEAD, not our pinned commit.
    let mut prepare = gix::prepare_clone(url, dir).map_err(|e| format!("clone setup: {e}"))?;
    let (repo, _outcome) = prepare
        .fetch_only(gix::progress::Discard, interrupt)
        .map_err(|e| format!("fetch: {e}"))?;

    // Resolve the pinned commit to its tree. A commit not present after a full
    // fetch means the pin is unreachable from any branch (a stray tag/PR ref) —
    // reported so the user knows the URL is wrong or the pin needs a branch.
    let oid = gix::ObjectId::from_hex(commit.as_bytes())
        .map_err(|e| format!("invalid commit '{commit}': {e}"))?;
    let commit_obj = repo
        .find_commit(oid)
        .map_err(|e| format!("commit {commit} not reachable from fetched history: {e}"))?;
    let tree = commit_obj
        .tree_id()
        .map_err(|e| format!("tree of {commit}: {e}"))?
        .detach();

    // Build an index from that tree and materialize it into the (empty) worktree.
    let mut index = repo
        .index_from_tree(&tree)
        .map_err(|e| format!("index from tree: {e}"))?;
    let workdir = repo
        .workdir()
        .ok_or_else(|| "clone has no work dir".to_string())?
        .to_owned();
    let opts = gix::worktree::state::checkout::Options {
        destination_is_initially_empty: true,
        ..Default::default()
    };
    // The checkout parallelizes, so it needs a thread-safe (`Arc`-backed) object
    // store; the default handle is `Rc`-backed and not `Send`.
    let objects = repo
        .objects
        .clone()
        .into_arc()
        .map_err(|e| format!("thread-safe odb: {e}"))?;
    gix::worktree::state::checkout(
        &mut index,
        workdir,
        objects,
        &gix::progress::Discard,
        &gix::progress::Discard,
        interrupt,
        opts,
    )
    .map_err(|e| format!("checkout: {e}"))?;
    index
        .write(gix::index::write::Options::default())
        .map_err(|e| format!("write index: {e}"))?;

    // Detach HEAD at the pinned commit: a detached HEAD is just the raw sha in the
    // `HEAD` file, which is what `git rev-parse HEAD` reads. With the worktree and
    // index both built from this commit's tree, `git status` is then clean.
    let git_dir = repo.git_dir();
    std::fs::write(git_dir.join("HEAD"), format!("{commit}\n"))
        .map_err(|e| format!("detach HEAD: {e}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;
    use std::process::Command;

    /// Run git in `dir`, asserting success.
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

    /// A local origin repo with two commits; returns (repo path, first-commit sha).
    /// The pinned commit is the *first* one, so the test proves an ancestor (not
    /// just the tip) is checked out.
    fn origin_with_history(dir: &Path) -> String {
        std::fs::create_dir_all(dir).unwrap();
        git_in(dir, &["init", "-q", "-b", "main"]);
        git_in(dir, &["config", "user.email", "t@t"]);
        git_in(dir, &["config", "user.name", "t"]);
        std::fs::create_dir_all(dir.join("kernel")).unwrap();
        std::fs::write(dir.join("kernel/0001-a.patch"), "one\n").unwrap();
        git_in(dir, &["add", "-A"]);
        git_in(dir, &["commit", "-q", "-m", "first"]);
        let first = git_in(dir, &["rev-parse", "HEAD"]);
        std::fs::write(dir.join("kernel/0002-b.patch"), "two\n").unwrap();
        git_in(dir, &["add", "-A"]);
        git_in(dir, &["commit", "-q", "-m", "second"]);
        first
    }

    #[test]
    fn fetches_pinned_ancestor_detached_and_clean() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin");
        let first = origin_with_history(&origin);
        let cache = tmp.path().join("cache");

        let sink = |_e: Event| {};
        let step = Step::start(&sink, "test");
        let checkout = fetch_profile(
            origin.to_str().unwrap(),
            &first,
            &cache,
            &step,
        )
        .expect("fetch");

        // Commit-addressed: the checkout lives under cache/<commit>.
        assert_eq!(checkout, cache.join(&first));
        // HEAD is the pinned ancestor, detached, worktree clean.
        assert_eq!(git_in(&checkout, &["rev-parse", "HEAD"]), first);
        assert_eq!(git_in(&checkout, &["status", "--porcelain"]), "");
        // The pinned commit's tree is materialized: file from commit 1 present,
        // file added in commit 2 absent.
        assert!(checkout.join("kernel/0001-a.patch").exists());
        assert!(!checkout.join("kernel/0002-b.patch").exists());
    }

    #[test]
    fn second_call_reuses_cache_without_network() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin");
        let first = origin_with_history(&origin);
        let cache = tmp.path().join("cache");
        let sink = |_e: Event| {};

        let step = Step::start(&sink, "test");
        let a = fetch_profile(origin.to_str().unwrap(), &first, &cache, &step).unwrap();
        // Remove the origin so a re-fetch would fail: a hit must not touch it.
        std::fs::remove_dir_all(&origin).unwrap();
        let step2 = Step::start(&sink, "test");
        let b = fetch_profile(origin.to_str().unwrap(), &first, &cache, &step2).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn option_like_url_is_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let sink = |_e: Event| {};
        let step = Step::start(&sink, "test");
        let err = fetch_profile(
            "--upload-pack=touch /tmp/pwn",
            "0000000000000000000000000000000000000000",
            tmp.path(),
            &step,
        )
        .unwrap_err();
        assert!(matches!(err, EngineError::UnsafeGitArgument { .. }));
    }
}
