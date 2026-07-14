//! Auto-fetch of a pinned *source* tree (kernel, ffmpeg, userspace) into a durable,
//! commit-addressed cache, so the verify gates run on a fresh clone with no
//! hand-cloned checkout — the "select a device, everything it needs is fetched"
//! ergonomic extended from the build to `verify-patches`/`verify-config`.
//!
//! Mirrors [`crate::patchfetch`] (which does the same for the `patches` repo) but for
//! the build's source trees, reusing the git shell-out `fetch_commit` (the same one
//! the compile stages use) so a tag, branch, or reachable-commit pin all resolve
//! uniformly and land the tree at exactly the locked commit.

use crate::build;
use crate::error::EngineError;
use crate::event::Step;
use std::path::{Path, PathBuf};

/// Materialize `source` at the locked `commit` into a commit-addressed directory
/// under `cache_root`, returning that clean, detached checkout's path.
///
/// Durable and content-addressed like [`crate::patchfetch::fetch_profile`]: a present
/// `cache_root/<commit>` is always a complete checkout at that commit — the fetch
/// stages into a temp sibling and atomically renames on success, so an interrupted
/// clone never leaves a half-materialized tree a later run trusts. A hit returns
/// immediately without touching the network. The resulting tree sits at `commit`
/// with a clean worktree, so a verify gate can apply a series onto it and hard-reset
/// around it (see [`apply_kernel_series`] / [`restore_tree`]).
///
/// `reference` is the pin's ref (tag/branch) for the shallow fetch; `what` labels the
/// tree in a [`EngineError::CommitMismatch`] (e.g. `"kernel"`, `"ffmpeg base"`).
pub fn ensure_tree(
    source: &str,
    reference: &str,
    commit: &str,
    what: &str,
    cache_root: &Path,
    step: &Step,
) -> Result<PathBuf, EngineError> {
    let dest = cache_root.join(commit);
    if dest.exists() {
        step.log(format!("{what}: reusing cached checkout at {}", short(commit)));
        return Ok(dest);
    }
    std::fs::create_dir_all(cache_root).map_err(|s| EngineError::io(cache_root, s))?;
    // Sweep `.fetch-*` staging dirs a hard-killed clone may have left; the cache is
    // durable (it survives `clean`), so leftovers would otherwise accrue.
    crate::gc::sweep_stale_temps(cache_root);

    // Stage into a temp sibling on the same filesystem, then rename atomically so
    // `dest` only ever appears fully checked out.
    let staging = tempfile::Builder::new()
        .prefix(".fetch-")
        .tempdir_in(cache_root)
        .map_err(|s| EngineError::io(cache_root, s))?;
    let repo_dir = staging.path().join("repo");

    step.log(format!("{what}: fetching {source} at {reference}"));
    build::fetch_commit(source, reference, commit, what, &repo_dir, step)?;
    std::fs::rename(&repo_dir, &dest).map_err(|s| EngineError::io(&dest, s))?;
    step.log(format!("{what}: checked out {}", short(commit)));
    Ok(dest)
}

/// Prepare a cached kernel tree for the config gate: reset it to the locked
/// `base_commit` (clearing patches a prior run left, and aborting any interrupted
/// `git am`), then apply the profile's kernel `series` in place — leaving it patched
/// for `verify-config`'s out-of-tree kconfig run. Returns the number of patches
/// applied.
///
/// The cache tree is shared with `verify-patches` (which needs a clean base), so the
/// caller restores it with [`restore_tree`] once kconfig has run. `target` labels the
/// tree in apply messages.
pub fn apply_kernel_series(
    tree: &Path,
    base_commit: &str,
    patches_root: &Path,
    series: &[String],
    target: &str,
) -> Result<usize, EngineError> {
    // Clear any leftover in-progress `am` and reset to a clean base, so an apply onto
    // a tree a prior (crashed) run left patched or mid-am starts from the pin.
    crate::git::am_abort(tree);
    crate::git::reset_hard(tree, base_commit)?;
    crate::patches::apply_tree(patches_root, series, tree, "kernel", target)
}

/// Restore a shared cache tree to its clean `base_commit` after the config gate, so
/// the next `verify-patches` reuse sees a clean base rather than a patched tree.
pub fn restore_tree(tree: &Path, base_commit: &str) -> Result<(), EngineError> {
    crate::git::reset_hard(tree, base_commit)
}

/// First 12 characters of a commit id for a log line. Truncates on a character
/// boundary so a malformed value renders short instead of panicking.
fn short(commit: &str) -> &str {
    match commit.char_indices().nth(12) {
        Some((i, _)) => &commit[..i],
        None => commit,
    }
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

    /// A local origin repo with a tagged commit; returns (repo path, tag, commit).
    fn tagged_origin(dir: &Path) -> (String, String) {
        std::fs::create_dir_all(dir).unwrap();
        git_in(dir, &["init", "-q", "-b", "main"]);
        git_in(dir, &["config", "user.email", "t@t"]);
        git_in(dir, &["config", "user.name", "t"]);
        std::fs::write(dir.join("Makefile"), "all:\n").unwrap();
        git_in(dir, &["add", "-A"]);
        git_in(dir, &["commit", "-q", "-m", "base"]);
        git_in(dir, &["tag", "v1.0"]);
        let commit = git_in(dir, &["rev-parse", "HEAD"]);
        ("v1.0".to_string(), commit)
    }

    #[test]
    fn ensures_a_clean_commit_addressed_checkout_then_reuses_it() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin");
        let (tag, commit) = tagged_origin(&origin);
        let cache = tmp.path().join("cache");
        let sink = |_e: Event| {};

        let step = Step::start(&sink, "test");
        let tree = ensure_tree(origin.to_str().unwrap(), &tag, &commit, "kernel", &cache, &step)
            .expect("fetch");
        // Commit-addressed, at the pinned commit, worktree clean.
        assert_eq!(tree, cache.join(&commit));
        assert_eq!(git_in(&tree, &["rev-parse", "HEAD"]), commit);
        assert_eq!(git_in(&tree, &["status", "--porcelain"]), "");

        // Remove the origin so a re-fetch would fail: a cache hit must not touch it.
        std::fs::remove_dir_all(&origin).unwrap();
        let step2 = Step::start(&sink, "test");
        let again = ensure_tree("unused://url", &tag, &commit, "kernel", &cache, &step2).unwrap();
        assert_eq!(again, tree);
    }

    #[test]
    fn apply_then_restore_leaves_a_clean_base() {
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin");
        let (tag, commit) = tagged_origin(&origin);
        let cache = tmp.path().join("cache");
        // A patches repo with one patch that changes the Makefile.
        let patches = tmp.path().join("patches");
        std::fs::create_dir_all(patches.join("k")).unwrap();
        // Build the patch from a sibling clone so it applies to the base commit.
        let gen = tmp.path().join("gen");
        git_in(tmp.path(), &["clone", "-q", origin.to_str().unwrap(), gen.to_str().unwrap()]);
        git_in(&gen, &["config", "user.email", "t@t"]);
        git_in(&gen, &["config", "user.name", "t"]);
        std::fs::write(gen.join("Makefile"), "all:\n\techo hi\n").unwrap();
        git_in(&gen, &["commit", "-qam", "change"]);
        git_in(&gen, &["format-patch", "-1", "-o", patches.join("k").to_str().unwrap()]);
        let label = std::fs::read_dir(patches.join("k"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .file_name()
            .into_string()
            .unwrap();

        let sink = |_e: Event| {};
        let step = Step::start(&sink, "test");
        let tree = ensure_tree(origin.to_str().unwrap(), &tag, &commit, "kernel", &cache, &step)
            .unwrap();

        let n = apply_kernel_series(
            &tree,
            &commit,
            &patches,
            &[format!("k/{label}")],
            "test @ v1.0",
        )
        .unwrap();
        assert_eq!(n, 1);
        // Patched: HEAD advanced past the base, the change is in the tree.
        assert_ne!(git_in(&tree, &["rev-parse", "HEAD"]), commit);

        restore_tree(&tree, &commit).unwrap();
        // Restored: back at the base commit, worktree clean for verify-patches reuse.
        assert_eq!(git_in(&tree, &["rev-parse", "HEAD"]), commit);
        assert_eq!(git_in(&tree, &["status", "--porcelain"]), "");
    }
}
