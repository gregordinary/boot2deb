//! Thin `git` shell-out helpers shared by the verify gate and the pin resolver.
//!
//! Reimplementing `git am --3way` or remote ref resolution in Rust is not worth
//! it, so these wrap the system `git`. Output parsing that has real logic
//! — peeling an annotated tag to its commit — is factored into a pure function
//! (`pick_commit`) so it is unit-testable without a network.
//!
//! Every `git` this crate runs is constructed by one `command` helper, which
//! neutralizes the build host's git configuration. See it for why.

use crate::error::{EngineError, PinRelation};
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// A `git` command with the build host's git configuration neutralized: no
/// `/etc/gitconfig`, no `~/.gitconfig`, no `$XDG_CONFIG_HOME/git/config`, and no
/// `/etc/gitattributes`.
///
/// **Every** `git` the engine runs comes from here, because host config is build
/// input. The one that decides it is `url.<base>.insteadOf`: it silently rewrites a
/// remote URL, so a host carrying one would fetch the pinned commit from a *different
/// remote than the lock names* — the precise input the lock exists to fix, redirected
/// with nothing in the build to report it. `core.hooksPath` (arbitrary code on
/// `clone`/`checkout`), `am.threeWay` and `apply.whitespace` (which decide whether a
/// patch applies and how), `core.attributesFile` and a system `gitattributes` (which
/// can rewrite line endings in a checked-out kernel tree), and `safe.directory` are the
/// same class. This mirrors the build sandbox's `base_env(false)` posture — the
/// environment a build runs in is declared here, not inherited — and the pure-Rust
/// clone in [`crate::patchfetch`] isolates `gix` for the same reason.
///
/// Transport settings are the deliberate cost. A host whose `~/.gitconfig` carries
/// `http.proxy` must express it as `http_proxy`/`https_proxy` in the environment,
/// which git still reads; credentials for a private source likewise. Config that
/// changes *what is fetched* cannot be honored without giving up the guarantee.
///
/// `repo` roots the command in a checkout via `-C`.
pub(crate) fn command(repo: Option<&Path>) -> Command {
    let mut cmd = Command::new("git");
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_ATTR_NOSYSTEM", "1");
    if let Some(r) = repo {
        cmd.arg("-C").arg(r);
    }
    cmd
}

/// Run `git` and return its raw [`Output`]; does not check the exit status.
fn run(repo: Option<&Path>, args: &[&str], context: &str) -> Result<Output, EngineError> {
    command(repo)
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
/// A leftover `rebase-apply` from a failed `am --abort` is **not** reported by
/// `status --porcelain`, so it is checked directly: without this, the next apply
/// would fail deep inside `git am` far from the real cause.
///
/// The state dirs are looked up under `git rev-parse --absolute-git-dir` rather than
/// `<repo>/.git`, because in a linked worktree or a submodule `.git` is a *file*
/// pointing at the real gitdir. Joining onto it would name a path that never exists,
/// and the in-progress check would answer "clean" for every such checkout — the
/// silent half of a check whose whole point is to be loud. A `patches` checkout kept
/// as a worktree is an ordinary way to co-develop two series at once.
pub(crate) fn is_clean(repo: &Path) -> Result<bool, EngineError> {
    let ctx = format!("status in {}", repo.display());
    let out = checked(Some(repo), &["status", "--porcelain"], &ctx)?;
    if !out.stdout.is_empty() {
        return Ok(false);
    }
    let ctx = format!("rev-parse --absolute-git-dir in {}", repo.display());
    let git_dir = PathBuf::from(stdout_of(checked(
        Some(repo),
        &["rev-parse", "--absolute-git-dir"],
        &ctx,
    )?));
    // An in-progress `git am` (`rebase-apply`) or rebase (`rebase-merge`) leaves
    // these state dirs behind; either means the tree is not safe to apply onto.
    let in_progress =
        git_dir.join("rebase-apply").exists() || git_dir.join("rebase-merge").exists();
    Ok(!in_progress)
}

/// Whether `repo`'s tracked content differs from `HEAD`.
///
/// This is the *identity* notion of dirty — "does a commit still name what is on
/// disk" — and it is deliberately narrower than this module's `is_clean`. Untracked
/// files are not counted: they are not build input, so a scratch file beside a config
/// tree must not make an otherwise-identified build report itself unidentifiable.
/// `is_clean` answers a different question for a different caller, refusing untracked
/// files and in-progress `am`/rebase state because it guards a tree about to be patched
/// and reset.
///
/// Matches what the CLI's build script stamps as `BOOT2DEB_GIT_DIRTY`, so the binary's
/// dirty flag and a config tree's mean the same thing in one provenance record.
///
/// `paths` narrows the question to the part of the tree the caller's identity depends
/// on; empty asks about the whole checkout. Scoping matters where one repo holds both
/// a program and its data: editing a device `.toml` does not change a compiled binary,
/// so a check about the binary that answered "dirty" for it would fire on ordinary
/// work.
///
/// Best-effort, and false on anything that is not a clean question: `git diff --quiet`
/// exits 1 for "differences" but 128 for "not a repository", and only the former is
/// dirtiness. A path that is not a checkout has no commit to differ from.
pub fn has_tracked_changes(repo: &Path, paths: &[&str]) -> bool {
    let ctx = format!("diff --quiet HEAD in {}", repo.display());
    let mut args = vec!["diff", "--quiet", "HEAD"];
    if !paths.is_empty() {
        args.push("--");
        args.extend_from_slice(paths);
    }
    run(Some(repo), &args, &ctx)
        .map(|out| out.status.code() == Some(1))
        .unwrap_or(false)
}

/// Whether `repo` holds `commit` as a commit object.
///
/// The question a read-only query over a *historical* pin has to ask first: a
/// checkout at one commit need not carry another, and asking for a blob under a
/// commit that is not there fails deep inside the read rather than at its premise.
/// Best-effort — a path that is not a repository at all answers `false`, since from
/// the caller's side that is the same situation.
pub fn has_commit(repo: &Path, commit: &str) -> bool {
    run(
        Some(repo),
        &["cat-file", "-e", &format!("{commit}^{{commit}}")],
        "cat-file -e",
    )
    .is_ok_and(|out| out.status.success())
}

/// The contents of `path` at `commit`, or `None` when the commit does not carry it.
///
/// Reads out of the object store rather than the worktree, so a query about a
/// historical pin does not need — and cannot disturb — a checkout at that commit.
/// A file absent at that commit is `None` rather than an error: for a series that
/// did not exist yet, absence is the answer.
pub fn show_file(repo: &Path, commit: &str, path: &str) -> Option<String> {
    let out = run(
        Some(repo),
        &["show", &format!("{commit}:{path}")],
        "show <commit>:<path>",
    )
    .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// The object id of `path` at `commit`, or `None` when the commit does not carry it.
///
/// Comparing two blob ids is how two revisions of one file are told apart without
/// reading either: git already content-addresses them, so equal ids are equal bytes.
pub fn blob_id(repo: &Path, commit: &str, path: &str) -> Option<String> {
    let out = run(
        Some(repo),
        &["rev-parse", &format!("{commit}:{path}")],
        "rev-parse <commit>:<path>",
    )
    .ok()?;
    out.status.success().then(|| stdout_of(out))
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

/// A throwaway committer identity, supplied inline to every `git am` invocation
/// without touching the repo config.
///
/// Needed by `am --abort` as much as by `am --3way`: the abort unwinds the commits an
/// interrupted apply made, and git refuses the whole command without an identity. A
/// host with no global `user.email` would otherwise fail the abort and leave
/// `.git/rebase-apply` behind, which [`is_clean`] reports as dirty forever — so these
/// args belong to one constant rather than to each call site.
const AM_IDENTITY: [&str; 4] = [
    "-c",
    "user.email=build@boot2deb",
    "-c",
    "user.name=boot2deb verify",
];

/// Apply one patch with `git am --3way`, returning the raw [`Output`] so the
/// caller can distinguish a clean apply from a conflict.
pub(crate) fn am_3way(repo: &Path, patch: &Path) -> Result<Output, EngineError> {
    command(Some(repo))
        .args(AM_IDENTITY)
        .args(["am", "--3way"])
        .arg(patch)
        .output()
        .map_err(|source| EngineError::GitSpawn {
            context: format!("am {}", patch.display()),
            source,
        })
}

/// Abort an in-progress `git am`, returning whether the abort succeeded.
///
/// Best-effort by design — callers invoke it while already carrying the error that
/// matters, and a failed abort must not displace it. The return value lets a caller
/// that *owns* the checkout (a commit-addressed cache tree) escalate to a harder
/// cleanup instead of leaving the tree wedged.
/// A `false` return also covers "there was nothing to abort", which is the ordinary
/// answer on a clean tree — so it means "the tree may still be mid-apply", not "the
/// tree is wedged". Ask [`is_clean`] for that.
pub(crate) fn am_abort(repo: &Path) -> bool {
    let mut args: Vec<&str> = AM_IDENTITY.to_vec();
    args.extend(["am", "--abort"]);
    run(Some(repo), &args, "am --abort")
        .map(|out| out.status.success())
        .unwrap_or(false)
}

/// Remove untracked files and directories, so a tree reset to a commit matches it
/// exactly. [`reset_hard`] alone leaves untracked leftovers, which [`is_clean`]
/// counts as dirty.
pub(crate) fn clean_untracked(repo: &Path) -> Result<(), EngineError> {
    let ctx = format!("clean -fdq in {}", repo.display());
    checked(Some(repo), &["clean", "-fdq"], &ctx).map(|_| ())
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
                Command::new("git")
                    .arg("-C")
                    .arg(repo)
                    .args(args)
                    .output()
                    .unwrap()
                    .status
                    .success(),
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

    /// The build's `git` must not read the host's git configuration.
    ///
    /// Asserted against `url.<base>.insteadOf`, the setting that decides this: it
    /// rewrites a remote URL, so a host carrying one would fetch a pinned commit from a
    /// remote the lock does not name. A bare `git` is probed first as the positive
    /// control — without it the assertion below could pass because the fixture never
    /// took effect, not because [`command`] excluded it.
    #[test]
    fn the_host_git_config_does_not_reach_the_build() {
        let home = tempfile::tempdir().unwrap();
        std::fs::write(
            home.path().join(".gitconfig"),
            "[url \"https://example.invalid/\"]\n\tinsteadOf = https://github.com/\n",
        )
        .unwrap();
        // A HOME with nothing else in it, and no XDG global config to confuse the
        // reading: whether the key is visible then depends only on the command.
        let probe = |mut cmd: Command| {
            cmd.env("HOME", home.path())
                .env_remove("XDG_CONFIG_HOME")
                .args(["config", "--get", "url.https://example.invalid/.insteadOf"])
                .output()
                .expect("spawn git")
        };
        assert!(
            probe(Command::new("git")).status.success(),
            "the fixture ~/.gitconfig was not read at all — the check below would be vacuous"
        );
        assert!(
            !probe(command(None)).status.success(),
            "the engine's git read the host's ~/.gitconfig; a `url.insteadOf` there would \
             redirect a locked source URL with nothing in the build to report it"
        );
    }

    /// The in-progress check has to survive a linked worktree, where `.git` is a file
    /// pointing at the real gitdir rather than a directory. Under the naive
    /// `<repo>/.git/rebase-apply` join it silently answers "clean" there, which is the
    /// one answer this check exists to prevent.
    #[test]
    fn an_in_progress_am_is_seen_in_a_linked_worktree() {
        let tmp = tempfile::tempdir().unwrap();
        let main = tmp.path().join("main");
        std::fs::create_dir_all(&main).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let out = command(Some(dir))
                .args(args)
                .env("GIT_AUTHOR_NAME", "t")
                .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
                .env("GIT_COMMITTER_NAME", "t")
                .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
                .output()
                .expect("spawn git");
            assert!(out.status.success(), "git {args:?}: {out:?}");
        };
        git(&main, &["init", "-q", "-b", "main", "."]);
        std::fs::write(main.join("f"), "one\n").unwrap();
        git(&main, &["add", "f"]);
        git(&main, &["commit", "-qm", "one"]);

        let linked = tmp.path().join("linked");
        git(&main, &["worktree", "add", "-q", "-b", "side", "../linked"]);
        assert!(linked.join(".git").is_file(), "a worktree's .git is a file");
        assert!(is_clean(&linked).unwrap());

        // The state dir a failed `git am` leaves behind, in the worktree's *real*
        // gitdir — which is under the main repo, not at `linked/.git/`.
        let git_dir = tmp.path().join("main/.git/worktrees/linked");
        std::fs::create_dir_all(git_dir.join("rebase-apply")).unwrap();
        assert!(
            !is_clean(&linked).unwrap(),
            "an in-progress am in a linked worktree must not read as clean"
        );

        // And the ordinary checkout still works the same way.
        assert!(is_clean(&main).unwrap());
        std::fs::create_dir_all(main.join(".git/rebase-merge")).unwrap();
        assert!(!is_clean(&main).unwrap());
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
