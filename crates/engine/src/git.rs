//! Thin `git` shell-out helpers shared by the verify gate and the pin resolver.
//!
//! Reimplementing `git am --3way` or remote ref resolution in Rust is not worth
//! it, so these wrap the system `git`. Output parsing that has real logic
//! — peeling an annotated tag to its commit — is factored into a pure function
//! (`pick_commit`) so it is unit-testable without a network.

use crate::error::{EngineError, PinRelation};
use std::path::Path;
use std::process::{Command, Output};

/// A `git` command, optionally rooted in `repo` via `-C`.
fn base(repo: Option<&Path>) -> Command {
    let mut cmd = Command::new("git");
    if let Some(r) = repo {
        cmd.arg("-C").arg(r);
    }
    cmd
}

/// Run `git` and return its raw [`Output`]; does not check the exit status.
fn run(repo: Option<&Path>, args: &[&str], context: &str) -> Result<Output, EngineError> {
    base(repo)
        .args(args)
        .output()
        .map_err(|source| EngineError::GitSpawn {
            context: context.to_string(),
            source,
        })
}

/// Run `git` and fail with [`EngineError::GitFailed`] on a non-zero exit.
fn checked(repo: Option<&Path>, args: &[&str], context: &str) -> Result<Output, EngineError> {
    let out = run(repo, args, context)?;
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

/// Trimmed stdout of a successful `git` command.
fn stdout_of(out: Output) -> String {
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// The current commit of a checkout.
pub fn rev_parse_head(repo: &Path) -> Result<String, EngineError> {
    let ctx = format!("rev-parse HEAD in {}", repo.display());
    checked(Some(repo), &["rev-parse", "HEAD"], &ctx).map(stdout_of)
}

/// True when the worktree has no staged or unstaged changes (no tracked-file
/// modifications and no untracked files) *and* no `git am`/rebase is in progress —
/// safe to apply and reset around.
///
/// A leftover `.git/rebase-apply` from a failed `am --abort` is **not** reported by
/// `status --porcelain`, so it is checked directly: without this, the next apply
/// would fail deep inside `git am` far from the real cause.
pub(crate) fn is_clean(repo: &Path) -> Result<bool, EngineError> {
    let ctx = format!("status in {}", repo.display());
    let out = checked(Some(repo), &["status", "--porcelain"], &ctx)?;
    if !out.stdout.is_empty() {
        return Ok(false);
    }
    // An in-progress `git am` (`rebase-apply`) or rebase (`rebase-merge`) leaves
    // these state dirs behind; either means the tree is not safe to apply onto.
    let git_dir = repo.join(".git");
    let in_progress =
        git_dir.join("rebase-apply").exists() || git_dir.join("rebase-merge").exists();
    Ok(!in_progress)
}

/// Whether `ancestor` is an ancestor of (or equal to) `descendant` in `repo`,
/// via `git merge-base --is-ancestor`. `None` when the relationship cannot be
/// determined — e.g. a commit absent from the local object store — so callers
/// decorating an error can degrade to generic wording instead of masking it.
fn is_ancestor(repo: &Path, ancestor: &str, descendant: &str) -> Option<bool> {
    let out = run(
        Some(repo),
        &["merge-base", "--is-ancestor", ancestor, descendant],
        "merge-base --is-ancestor",
    )
    .ok()?;
    match out.status.code() {
        Some(0) => Some(true),
        Some(1) => Some(false),
        _ => None,
    }
}

/// Classify how a checkout's HEAD (`actual`) relates to a locked pin
/// (`expected`), for [`PinRelation`]-driven remedy wording. Best-effort: any
/// git failure lands on [`PinRelation::Unknown`] rather than replacing the
/// pin-mismatch error this decorates.
pub(crate) fn pin_relation(repo: &Path, expected: &str, actual: &str) -> PinRelation {
    match is_ancestor(repo, expected, actual) {
        Some(true) => PinRelation::Ahead,
        _ => match is_ancestor(repo, actual, expected) {
            Some(true) => PinRelation::Behind,
            _ => PinRelation::Unknown,
        },
    }
}

/// Resolve a tag/branch/ref on a remote to its exact commit, peeling annotated
/// tags. A value that is already a full 40-hex commit is canonicalized to lowercase
/// and returned — the form git's own `rev-parse HEAD` emits, so the build stage's
/// `HEAD == pinned` check holds even for an uppercase sha a user pins.
pub fn resolve_ref(url: &str, reference: &str) -> Result<String, EngineError> {
    if boot2deb_core::sources::is_full_sha(reference) {
        return Ok(boot2deb_core::sources::normalize_ref(reference));
    }
    // A URL beginning with `-` would be read as an option by `git ls-remote`.
    crate::build::reject_optionlike("source", url)?;
    // Query the peeled tag, the tag object, and a branch in one round-trip; the
    // peeled form (`^{}`) is what dereferences an annotated tag to its commit.
    let peeled = format!("refs/tags/{reference}^{{}}");
    let tag = format!("refs/tags/{reference}");
    let head = format!("refs/heads/{reference}");
    let ctx = format!("ls-remote {url} {reference}");
    // `--end-of-options` keeps the URL positional from being parsed as a flag.
    let out = checked(
        None,
        &["ls-remote", "--end-of-options", url, &peeled, &tag, &head],
        &ctx,
    )?;
    pick_commit(&String::from_utf8_lossy(&out.stdout), reference).ok_or_else(|| {
        EngineError::RefNotFound {
            url: url.to_string(),
            reference: reference.to_string(),
        }
    })
}

/// Apply one patch with `git am --3way`, returning the raw [`Output`] so the
/// caller can distinguish a clean apply from a conflict. A throwaway committer
/// identity is supplied inline (`git am` refuses without one) without touching
/// the repo config.
pub(crate) fn am_3way(repo: &Path, patch: &Path) -> Result<Output, EngineError> {
    base(Some(repo))
        .args([
            "-c",
            "user.email=build@boot2deb",
            "-c",
            "user.name=boot2deb verify",
            "am",
            "--3way",
        ])
        .arg(patch)
        .output()
        .map_err(|source| EngineError::GitSpawn {
            context: format!("am {}", patch.display()),
            source,
        })
}

/// Abort an in-progress `git am` (best-effort cleanup after a failed apply).
pub(crate) fn am_abort(repo: &Path) {
    let _ = run(Some(repo), &["am", "--abort"], "am --abort");
}

/// The committer timestamp (Unix seconds) of `commit` in `repo`, for a
/// deterministic `SOURCE_DATE_EPOCH`.
///
/// Reads the *locked base* commit explicitly — not HEAD, which after `git am`
/// is a patch commit stamped at build time and so differs every run. The base
/// commit object is still reachable by sha after patches apply, so its committer
/// date is a stable per-lock timestamp.
pub(crate) fn commit_epoch(repo: &Path, commit: &str) -> Result<u64, EngineError> {
    let ctx = format!("show -s --format=%ct {commit}");
    let out = checked(Some(repo), &["show", "-s", "--format=%ct", commit], &ctx)?;
    let text = stdout_of(out);
    text.parse::<u64>().map_err(|_| EngineError::GitFailed {
        context: ctx,
        status: None,
        stderr: format!("could not parse committer epoch from '{text}'"),
    })
}

/// Reset a checkout hard to `commit`, discarding any applied patches.
pub(crate) fn reset_hard(repo: &Path, commit: &str) -> Result<(), EngineError> {
    let ctx = format!("reset --hard {commit}");
    checked(Some(repo), &["reset", "--hard", commit], &ctx).map(|_| ())
}

/// Pick the commit for `reference` from `git ls-remote` output, preferring the
/// peeled annotated-tag line (`refs/tags/<ref>^{}`) over the tag object, and a
/// tag over a branch of the same name. Pure, so the peel precedence is testable.
fn pick_commit(stdout: &str, reference: &str) -> Option<String> {
    let peeled_ref = format!("refs/tags/{reference}^{{}}");
    let tag_ref = format!("refs/tags/{reference}");
    let head_ref = format!("refs/heads/{reference}");
    let (mut peeled, mut tag, mut head) = (None, None, None);
    for line in stdout.lines() {
        let Some((sha, name)) = line.split_once('\t') else {
            continue;
        };
        match name {
            n if n == peeled_ref => peeled = Some(sha.to_string()),
            n if n == tag_ref => tag = Some(sha.to_string()),
            n if n == head_ref => head = Some(sha.to_string()),
            _ => {}
        }
    }
    peeled.or(tag).or(head)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn peels_annotated_tag_over_object() {
        // Annotated tag: ls-remote returns the tag object and the peeled commit.
        let out = "\
1111111111111111111111111111111111111111\trefs/tags/v7.1.1\n\
c9acdc466e9aa96352f658b9276aa8a45b8e817d\trefs/tags/v7.1.1^{}\n";
        assert_eq!(
            pick_commit(out, "v7.1.1").as_deref(),
            Some("c9acdc466e9aa96352f658b9276aa8a45b8e817d")
        );
    }

    #[test]
    fn lightweight_tag_uses_object_line() {
        // Lightweight tag: only the tag line, which already points at the commit.
        let out = "88dc2788777babfd6322fa655df549a019aa1e69\trefs/tags/v2026.04\n";
        assert_eq!(
            pick_commit(out, "v2026.04").as_deref(),
            Some("88dc2788777babfd6322fa655df549a019aa1e69")
        );
    }

    #[test]
    fn falls_back_to_branch() {
        let out = "abc1230000000000000000000000000000000000\trefs/heads/main\n";
        assert_eq!(
            pick_commit(out, "main").as_deref(),
            Some("abc1230000000000000000000000000000000000")
        );
    }

    #[test]
    fn unknown_ref_is_none() {
        assert_eq!(pick_commit("", "v9.9.9"), None);
    }

    #[test]
    fn full_sha_shape_is_recognized() {
        use boot2deb_core::sources::is_full_sha;
        assert!(is_full_sha("c9acdc466e9aa96352f658b9276aa8a45b8e817d"));
        // Uppercase is still sha-shaped; resolve_ref lowercases it before pinning.
        assert!(is_full_sha("C9ACDC466E9AA96352F658B9276AA8A45B8E817D"));
        assert!(!is_full_sha("v7.1.1"));
        assert!(!is_full_sha("c9acdc46")); // short
    }

    #[test]
    fn pin_relation_classifies_ahead_behind_and_unknown() {
        // Two commits in a real local repo: parent -> child. HEAD relative to a
        // pin at the parent is Ahead; relative to a pin at the child (after
        // checking out the parent) it is Behind; a commit git does not hold is
        // Unknown.
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        let git = |args: &[&str]| {
            assert!(
                Command::new("git").arg("-C").arg(repo).args(args).output().unwrap().status.success(),
                "git {args:?} failed"
            );
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@boot2deb"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(repo.join("f"), "one").unwrap();
        git(&["add", "f"]);
        git(&["commit", "-qm", "one"]);
        let parent = rev_parse_head(repo).unwrap();
        std::fs::write(repo.join("f"), "two").unwrap();
        git(&["add", "f"]);
        git(&["commit", "-qm", "two"]);
        let child = rev_parse_head(repo).unwrap();

        // HEAD (child) has commits past the pin (parent): the lock is behind the work.
        assert_eq!(pin_relation(repo, &parent, &child), PinRelation::Ahead);
        // HEAD (parent) is an ancestor of the pin (child): a stale checkout.
        assert_eq!(pin_relation(repo, &child, &parent), PinRelation::Behind);
        // A pin the local object store does not hold cannot be classified.
        let absent = "0123456789abcdef0123456789abcdef01234567";
        assert_eq!(pin_relation(repo, absent, &child), PinRelation::Unknown);
    }

    #[test]
    fn resolve_ref_lowercases_a_full_sha_without_network() {
        // A full sha short-circuits ls-remote, so this hits no network. An uppercase
        // sha is canonicalized to lowercase so the build-stage HEAD check matches.
        let upper = "C9ACDC466E9AA96352F658B9276AA8A45B8E817D";
        assert_eq!(
            resolve_ref("unused://url", upper).unwrap(),
            "c9acdc466e9aa96352f658b9276aa8a45b8e817d"
        );
    }
}
