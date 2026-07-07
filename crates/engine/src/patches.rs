//! The verify-applies gate: dry-run an ordered patch series against a source
//! tree with `git am --3way`, hard-erroring — naming the failing patch and the
//! target — if any patch does not apply. Patches are never silently skipped or
//! fuzzed in; `applies_to_kernel` in the profile is the declared intent, this is
//! the enforcement.
//!
//! "Dry-run" means the tree is restored to its starting commit afterwards, so a
//! verify has no lasting effect. The build stage reuses the same `git am --3way`
//! per but leaves the series applied.

use crate::error::EngineError;
use crate::git;
use std::path::{Path, PathBuf};

/// One tree's patch list resolved to on-disk paths, paired with the
/// patches-repo-relative label used in messages.
struct ResolvedPatch {
    path: PathBuf,
    label: String,
}

/// Resolve a profile's repo-relative patch labels to absolute paths under
/// `patches_root`, preserving order.
fn resolve_paths(patches_root: &Path, labels: &[String]) -> Vec<ResolvedPatch> {
    labels
        .iter()
        .map(|label| ResolvedPatch {
            path: patches_root.join(label),
            label: label.clone(),
        })
        .collect()
}

/// Verify that `labels` (one tree's ordered series from a
/// [`PatchProfile`](boot2deb_core::PatchProfile), e.g. its `kernel` list) applies
/// to the checkout at `repo`.
///
/// - `patches_root` is the patches-repo checkout the labels are relative to.
/// - `tree` labels the tree for messages (`"kernel"`, `"ffmpeg"`, …).
/// - `target` labels what the tree is checked at (`"rk3588-mainline-7.1 @ v7.1.1"`).
///
/// On success the checkout is restored to its starting commit and the count of
/// verified patches is returned. On the first patch that does not apply, the
/// in-progress `am` is aborted, the checkout restored, and
/// [`EngineError::PatchDoesNotApply`] returned naming that patch.
pub fn verify_tree(
    patches_root: &Path,
    labels: &[String],
    repo: &Path,
    tree: &str,
    target: &str,
) -> Result<usize, EngineError> {
    // `git am` runs with `-C <repo>`, so it resolves a relative patch path
    // against the target checkout, not our CWD. Anchor to an absolute patches
    // root up front so the paths are unambiguous.
    let root = std::fs::canonicalize(patches_root)
        .map_err(|source| EngineError::io(patches_root, source))?;
    let patches = resolve_paths(&root, labels);
    // Verify snapshots HEAD and hard-resets afterwards, so refuse a dirty tree
    // rather than risk clobbering uncommitted work.
    if !git::is_clean(repo)? {
        return Err(EngineError::DirtyCheckout {
            repo: repo.display().to_string(),
        });
    }
    let start = git::rev_parse_head(repo)?;
    let outcome = apply_series(repo, tree, target, &patches);
    // Restore the worktree no matter what — this is a pure verify.
    let restore = git::reset_hard(repo, &start);
    match outcome {
        // A verify failure dominates; the restore was best-effort.
        Err(e) => Err(e),
        // On success, a failed restore would leave the tree dirty — surface it.
        Ok(n) => restore.map(|_| n),
    }
}

/// Apply `labels` (one tree's ordered series) to `repo` and **leave the commits
/// in place** — the build path, as opposed to [`verify_tree`] which
/// restores. Used by the compile stages to bring a freshly-cloned tree up to the
/// patched state before configuring and building.
///
/// Refuses a dirty tree (applying onto uncommitted work, or re-applying an
/// already-patched tree, would corrupt it) and hard-errors naming the first
/// patch that does not apply. On success the count of applied patches is
/// returned. Arguments match [`verify_tree`].
pub fn apply_tree(
    patches_root: &Path,
    labels: &[String],
    repo: &Path,
    tree: &str,
    target: &str,
) -> Result<usize, EngineError> {
    let root = std::fs::canonicalize(patches_root)
        .map_err(|source| EngineError::io(patches_root, source))?;
    let patches = resolve_paths(&root, labels);
    if !git::is_clean(repo)? {
        return Err(EngineError::DirtyCheckout {
            repo: repo.display().to_string(),
        });
    }
    apply_series(repo, tree, target, &patches)
}

/// Apply the series, hard-erroring on the first patch that does not apply. Leaves
/// applied commits in place; the caller restores.
fn apply_series(
    repo: &Path,
    tree: &str,
    target: &str,
    patches: &[ResolvedPatch],
) -> Result<usize, EngineError> {
    for patch in patches {
        if !patch.path.exists() {
            return Err(EngineError::PatchNotFound {
                path: patch.path.display().to_string(),
            });
        }
        let out = git::am_3way(repo, &patch.path)?;
        if !out.status.success() {
            // `git am` prints the conflict to stdout ("Applying: …", "error: …")
            // and stderr; combine both for a useful message.
            let mut detail = String::from_utf8_lossy(&out.stdout).trim().to_string();
            let stderr = String::from_utf8_lossy(&out.stderr);
            if !stderr.trim().is_empty() {
                if !detail.is_empty() {
                    detail.push('\n');
                }
                detail.push_str(stderr.trim());
            }
            git::am_abort(repo);
            return Err(EngineError::PatchDoesNotApply {
                tree: tree.to_string(),
                target: target.to_string(),
                patch: patch.label.clone(),
                detail: indent(&detail),
            });
        }
    }
    Ok(patches.len())
}

/// Indent multi-line `git am` output two spaces under the error header.
fn indent(s: &str) -> String {
    s.lines()
        .map(|l| format!("  {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// Verify a set of trees, each `(label, ordered patch list, checkout)`.
///
/// The caller selects which trees to exercise — e.g. only `kernel` before the
/// ffmpeg/MPP checkouts exist — pairing each
/// [`PatchProfile`](boot2deb_core::PatchProfile) list (`kernel`, `ffmpeg`, …) with
/// the checkout to verify it against. `target` labels what the trees are checked
/// at. Returns the per-tree verified counts in order; hard-errors on the first
/// tree that fails.
pub fn verify_profile(
    patches_root: &Path,
    target: &str,
    trees: &[(&str, &[String], &Path)],
) -> Result<Vec<(String, usize)>, EngineError> {
    let mut report = Vec::new();
    for (tree, labels, repo) in trees {
        let n = verify_tree(patches_root, labels, repo, tree, target)?;
        report.push((tree.to_string(), n));
    }
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::process::Command;

    /// Run git in `dir`, asserting success (test helper).
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

    /// Init a repo with an identity and one committed file.
    fn init_repo(dir: &Path, file: &str, contents: &str) {
        fs::create_dir_all(dir).unwrap();
        git_in(dir, &["init", "-q", "-b", "main"]);
        git_in(dir, &["config", "user.email", "t@t"]);
        git_in(dir, &["config", "user.name", "t"]);
        fs::write(dir.join(file), contents).unwrap();
        git_in(dir, &["add", file]);
        git_in(dir, &["commit", "-q", "-m", "base"]);
    }

    /// Produce a one-commit `git format-patch` for a change to `file`, leaving the
    /// repo back at its base commit. Returns the patch file path.
    fn make_patch(repo: &Path, file: &str, new_contents: &str, out: &Path) -> PathBuf {
        let base = git_in(repo, &["rev-parse", "HEAD"]);
        fs::write(repo.join(file), new_contents).unwrap();
        git_in(repo, &["commit", "-q", "-a", "-m", "change"]);
        git_in(repo, &["format-patch", "-1", "-o", out.to_str().unwrap()]);
        git_in(repo, &["reset", "--hard", &base]);
        // format-patch names it 0001-change.patch.
        out.join("0001-change.patch")
    }

    #[test]
    fn clean_series_applies_and_restores() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let gen = tmp.path().join("gen");
        let patches = tmp.path().join("patches");
        fs::create_dir_all(&patches).unwrap();

        init_repo(&src, "hello.txt", "alpha\nbeta\ngamma\n");
        // Generate a patch from an identical base so it applies cleanly.
        init_repo(&gen, "hello.txt", "alpha\nbeta\ngamma\n");
        let p = make_patch(&gen, "hello.txt", "alpha\nBETA\ngamma\n", &patches);
        let label = "good/0001-change.patch".to_string();
        fs::create_dir_all(patches.join("good")).unwrap();
        fs::rename(&p, patches.join(&label)).unwrap();

        let before = git_in(&src, &["rev-parse", "HEAD"]);
        let n = verify_tree(&patches, &[label], &src, "kernel", "test @ base").unwrap();
        assert_eq!(n, 1);
        // Pure verify: HEAD unchanged and worktree clean.
        assert_eq!(git_in(&src, &["rev-parse", "HEAD"]), before);
        assert_eq!(git_in(&src, &["status", "--porcelain"]), "");
    }

    #[test]
    fn apply_tree_leaves_the_series_applied() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let gen = tmp.path().join("gen");
        let patches = tmp.path().join("patches");
        fs::create_dir_all(&patches).unwrap();

        init_repo(&src, "hello.txt", "alpha\nbeta\ngamma\n");
        init_repo(&gen, "hello.txt", "alpha\nbeta\ngamma\n");
        let p = make_patch(&gen, "hello.txt", "alpha\nBETA\ngamma\n", &patches);
        let label = "good/0001-change.patch".to_string();
        fs::create_dir_all(patches.join("good")).unwrap();
        fs::rename(&p, patches.join(&label)).unwrap();

        let before = git_in(&src, &["rev-parse", "HEAD"]);
        let n = apply_tree(&patches, &[label], &src, "kernel", "test @ base").unwrap();
        assert_eq!(n, 1);
        // Unlike verify, apply advances HEAD and leaves the change in the tree.
        assert_ne!(git_in(&src, &["rev-parse", "HEAD"]), before);
        assert_eq!(fs::read_to_string(src.join("hello.txt")).unwrap(), "alpha\nBETA\ngamma\n");
        assert_eq!(git_in(&src, &["status", "--porcelain"]), "");
    }

    #[test]
    fn conflicting_patch_hard_errors_naming_it() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let other = tmp.path().join("other");
        let patches = tmp.path().join("patches");
        fs::create_dir_all(&patches).unwrap();

        init_repo(&src, "hello.txt", "alpha\nbeta\ngamma\n");
        // A patch generated against unrelated content will not apply to `src`.
        init_repo(&other, "hello.txt", "one\ntwo\nthree\n");
        let p = make_patch(&other, "hello.txt", "one\nTWO\nthree\n", &patches);
        let label = "bad/0001-change.patch".to_string();
        fs::create_dir_all(patches.join("bad")).unwrap();
        fs::rename(&p, patches.join(&label)).unwrap();

        let before = git_in(&src, &["rev-parse", "HEAD"]);
        let err = verify_tree(&patches, std::slice::from_ref(&label), &src, "kernel", "test @ base")
            .unwrap_err();
        match err {
            EngineError::PatchDoesNotApply { patch, tree, .. } => {
                assert_eq!(patch, label);
                assert_eq!(tree, "kernel");
            }
            other => panic!("expected PatchDoesNotApply, got {other:?}"),
        }
        // Even on failure the worktree is restored.
        assert_eq!(git_in(&src, &["rev-parse", "HEAD"]), before);
        assert_eq!(git_in(&src, &["status", "--porcelain"]), "");
    }

    #[test]
    fn dirty_checkout_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        init_repo(&src, "hello.txt", "alpha\n");
        // Leave an uncommitted change.
        fs::write(src.join("hello.txt"), "alpha\nbeta\n").unwrap();
        let err = verify_tree(tmp.path(), &[], &src, "kernel", "test @ base").unwrap_err();
        assert!(matches!(err, EngineError::DirtyCheckout { .. }));
    }

    #[test]
    fn missing_patch_file_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        init_repo(&src, "hello.txt", "alpha\n");
        let err = verify_tree(
            tmp.path(),
            &["does-not-exist.patch".to_string()],
            &src,
            "kernel",
            "test @ base",
        )
        .unwrap_err();
        assert!(matches!(err, EngineError::PatchNotFound { .. }));
    }
}
