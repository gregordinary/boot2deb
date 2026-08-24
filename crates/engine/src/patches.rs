//! The verify-applies gate: dry-run an ordered patch series against a source tree
//! with `git am --3way`, naming the failing patch and the target when one does not
//! apply. Patches are never fuzzed in; the series' ranges are the declared
//! intent, this is the enforcement.
//!
//! "Dry-run" means the tree is restored to its starting commit afterwards, so a
//! verify has no lasting effect. The build stage reuses the same `git am --3way`
//! pass but leaves the series applied.
//!
//! Two failure behaviours ([`OnFailure`]): a build stops at the first patch that
//! does not apply, because there is no point compiling a tree with a hole in it; a
//! survey keeps going and reports every boundary at once, because one boundary
//! usually spawns adjacent ones.

use crate::error::EngineError;
use crate::git;
use std::path::{Path, PathBuf};

/// One tree's patch list resolved to on-disk paths, paired with the
/// patches-repo-relative label used in messages.
struct ResolvedPatch {
    path: PathBuf,
    label: String,
}

/// Resolve a series' repo-relative patch labels to absolute paths under
/// `patches_root`, preserving order.
fn resolve_paths(patches_root: &Path, labels: &[&str]) -> Vec<ResolvedPatch> {
    labels
        .iter()
        .map(|label| ResolvedPatch {
            path: patches_root.join(label),
            label: (*label).to_string(),
        })
        .collect()
}

/// Verify that `labels` (one tree's ordered series from a
/// [`SeriesIdentity`](boot2deb_core::PatchSeries), e.g. its `kernel` list) applies
/// to the checkout at `repo`.
///
/// - `patches_root` is the patches-repo checkout the labels are relative to.
/// - `tree` labels the tree for messages (`"kernel"`, `"ffmpeg"`, …).
/// - `target` labels what the tree is checked at (`"rk3588-mainline-7.1 @ v7.1.1"`).
///
/// The checkout is restored to its starting commit either way, so a verify has no
/// lasting effect. Returns the count that applied plus the collected failures —
/// empty under [`OnFailure::Stop`], which instead returns
/// [`EngineError::PatchDoesNotApply`] naming the first patch to fail.
pub fn verify_tree(
    patches_root: &Path,
    labels: &[&str],
    repo: &Path,
    tree: &str,
    target: &str,
    on_failure: OnFailure,
) -> Result<(usize, Vec<PatchFailure>), EngineError> {
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
    let outcome = apply_series(repo, tree, target, &patches, on_failure);
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
    labels: &[&str],
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
    apply_series(repo, tree, target, &patches, OnFailure::Stop).map(|(n, _)| n)
}

/// How a series pass reacts to a patch that does not apply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OnFailure {
    /// Stop at the first patch that does not apply. What a **build** wants: the
    /// tree is being brought to a compilable state, and continuing past a hole
    /// would compile something nobody described.
    Stop,
    /// Skip a failing patch and keep going, collecting every failure.
    ///
    /// What **candidate verification** wants. One boundary usually spawns
    /// adjacent ones — reworking a patch shifts the context every later patch
    /// applies against — so stopping at the first turns a survey into serial
    /// discovery: fix, re-run, find the next, re-run.
    ///
    /// Later results are measured against a tree missing the skipped patch, so a
    /// batch pass is a map of the damage rather than a final verdict; a rework can
    /// still change what comes after it.
    KeepGoing,
}

/// What a multi-tree verify produces: the per-tree `(label, applied count)` in the
/// order the trees were given, plus every failure collected across all of them.
pub type ProfileReport = (Vec<(String, usize)>, Vec<PatchFailure>);

/// One patch that did not apply, from a [`OnFailure::KeepGoing`] pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchFailure {
    /// The tree the series was applied to (`"kernel"`, `"uboot"`, …).
    pub tree: String,
    /// The patches-repo-relative label of the patch that failed.
    pub patch: String,
    /// Indented `git am` output explaining the rejection.
    pub detail: String,
}

/// Apply the series, leaving applied commits in place; the caller restores.
///
/// Returns the number of patches that applied, plus the failures collected under
/// [`OnFailure::KeepGoing`] (always empty under [`OnFailure::Stop`], which returns
/// [`EngineError::PatchDoesNotApply`] instead).
fn apply_series(
    repo: &Path,
    tree: &str,
    target: &str,
    patches: &[ResolvedPatch],
    on_failure: OnFailure,
) -> Result<(usize, Vec<PatchFailure>), EngineError> {
    let mut applied = 0;
    let mut failures = Vec::new();
    for patch in patches {
        if !patch.path.exists() {
            return Err(EngineError::PatchNotFound {
                path: patch.path.display().to_string(),
            });
        }
        let out = git::am_3way(repo, &patch.path)?;
        if out.status.success() {
            applied += 1;
            continue;
        }
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
        // Always abort the half-finished am: the tree must be usable for the next
        // patch (KeepGoing) or for the caller's restore (Stop).
        git::am_abort(repo);
        match on_failure {
            OnFailure::Stop => {
                return Err(EngineError::PatchDoesNotApply {
                    tree: tree.to_string(),
                    target: target.to_string(),
                    patch: patch.label.clone(),
                    detail: indent(&detail),
                })
            }
            OnFailure::KeepGoing => failures.push(PatchFailure {
                tree: tree.to_string(),
                patch: patch.label.clone(),
                detail: indent(&detail),
            }),
        }
    }
    Ok((applied, failures))
}

/// Indent multi-line `git am` output two spaces under the error header.
fn indent(s: &str) -> String {
    s.lines()
        .map(|l| format!("  {l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

/// One tree in a [`verify_series`] run: what to apply, where, and at what version.
///
/// The target rides with the tree rather than being one label for the whole run
/// because a verify spans two independent axes. The kernel-family trees are checked
/// at the kernel tag and the `uboot` tree at the u-boot tag, so a single target
/// would have to misreport one of them.
pub struct VerifyTree<'a> {
    /// Tree label for messages (`"kernel"`, `"ffmpeg"`, `"uboot"`, …).
    pub label: &'a str,
    /// The ordered patch labels, already narrowed to the version under test.
    pub series: &'a [&'a str],
    /// The checkout to apply them to.
    pub checkout: &'a Path,
    /// What that checkout is at (`"rk3588-mainline-7.1 @ v7.1.1"`), for messages.
    pub target: &'a str,
}

/// Verify a set of [`VerifyTree`]s.
///
/// The caller selects which trees to exercise — e.g. only `kernel` before the
/// ffmpeg/MPP checkouts exist — pairing each
/// [`SeriesIdentity`](boot2deb_core::PatchSeries) series (already filtered to the
/// version under test) with the checkout to verify it against.
///
/// Returns the per-tree verified counts in order, plus every failure collected
/// across all trees. Under [`OnFailure::Stop`] it hard-errors on the first tree
/// that fails; under [`OnFailure::KeepGoing`] it visits every tree and the failure
/// list is the report.
pub fn verify_series(
    patches_root: &Path,
    trees: &[VerifyTree<'_>],
    on_failure: OnFailure,
) -> Result<ProfileReport, EngineError> {
    let mut report = Vec::new();
    let mut failures = Vec::new();
    for t in trees {
        let (n, failed) = verify_tree(
            patches_root,
            t.series,
            t.checkout,
            t.label,
            t.target,
            on_failure,
        )?;
        report.push((t.label.to_string(), n));
        failures.extend(failed);
    }
    Ok((report, failures))
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
    fn keep_going_reports_every_failure_and_applies_the_rest() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let patches = tmp.path().join("patches");
        fs::create_dir_all(patches.join("s")).unwrap();
        init_repo(&src, "hello.txt", "alpha\nbeta\ngamma\n");

        // One patch generated against a *different* base, so it cannot apply, and one
        // generated against the real base, so it can. Ordered failure-first: under
        // Stop the second would never be reached.
        let bad_gen = tmp.path().join("bad");
        init_repo(&bad_gen, "hello.txt", "wholly\nunrelated\ncontent\n");
        let bad = make_patch(
            &bad_gen,
            "hello.txt",
            "wholly\nCHANGED\ncontent\n",
            &patches,
        );
        fs::rename(&bad, patches.join("s/0001-bad.patch")).unwrap();

        let good_gen = tmp.path().join("good");
        init_repo(&good_gen, "hello.txt", "alpha\nbeta\ngamma\n");
        let good = make_patch(&good_gen, "hello.txt", "alpha\nBETA\ngamma\n", &patches);
        fs::rename(&good, patches.join("s/0002-good.patch")).unwrap();

        let series = ["s/0001-bad.patch", "s/0002-good.patch"];

        // Stop: the first failure is the whole report, and 0002 is never tried.
        let err = verify_tree(&patches, &series, &src, "kernel", "t", OnFailure::Stop).unwrap_err();
        assert!(matches!(
            err,
            EngineError::PatchDoesNotApply { ref patch, .. } if patch == "s/0001-bad.patch"
        ));

        // KeepGoing: 0001 is reported and skipped, and 0002 still applies -- which is
        // the point, since a boundary is usually not the last one.
        let (applied, failures) =
            verify_tree(&patches, &series, &src, "kernel", "t", OnFailure::KeepGoing).unwrap();
        assert_eq!(applied, 1);
        assert_eq!(failures.len(), 1);
        assert_eq!(failures[0].patch, "s/0001-bad.patch");
        assert_eq!(failures[0].tree, "kernel");
        assert!(!failures[0].detail.is_empty());
        // Still a pure verify: the skipped am left nothing behind.
        assert_eq!(git_in(&src, &["status", "--porcelain"]), "");
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
        let (n, failed) = verify_tree(
            &patches,
            &[&label],
            &src,
            "kernel",
            "test @ base",
            OnFailure::Stop,
        )
        .unwrap();
        assert_eq!(n, 1);
        assert!(failed.is_empty());
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
        let n = apply_tree(&patches, &[&label], &src, "kernel", "test @ base").unwrap();
        assert_eq!(n, 1);
        // Unlike verify, apply advances HEAD and leaves the change in the tree.
        assert_ne!(git_in(&src, &["rev-parse", "HEAD"]), before);
        assert_eq!(
            fs::read_to_string(src.join("hello.txt")).unwrap(),
            "alpha\nBETA\ngamma\n"
        );
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
        let err = verify_tree(
            &patches,
            &[&label],
            &src,
            "kernel",
            "test @ base",
            OnFailure::Stop,
        )
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
        let err = verify_tree(
            tmp.path(),
            &[],
            &src,
            "kernel",
            "test @ base",
            OnFailure::Stop,
        )
        .unwrap_err();
        assert!(matches!(err, EngineError::DirtyCheckout { .. }));
    }

    #[test]
    fn missing_patch_file_is_reported() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        init_repo(&src, "hello.txt", "alpha\n");
        let err = verify_tree(
            tmp.path(),
            &["does-not-exist.patch"],
            &src,
            "kernel",
            "test @ base",
            OnFailure::Stop,
        )
        .unwrap_err();
        assert!(matches!(err, EngineError::PatchNotFound { .. }));
    }
}
