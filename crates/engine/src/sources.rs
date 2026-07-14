//! Source-pin durability probe — a read-only network classification of
//! whether a locked git pin is still re-fetchable from its *configured* URL.
//!
//! A commit is only re-fetchable if the remote still holds it. This probe
//! answers, per pin, one of `durable | ephemeral | ORPHANED | skipped`:
//!
//! - **durable** — the commit is a tag target on the remote (immutable,
//!   shallow-fetchable forever). The target for every shipped pin.
//! - **ephemeral** — the commit is fetchable now but not tag-anchored: a current
//!   branch tip, or a past commit still reachable via history. A force-push /
//!   rebase / delete can orphan it; pin a tag for durability.
//! - **ORPHANED** — no tag and no branch reaches the commit; it may exist only in a
//!   local checkout (the mpp anti-pattern). Not re-fetchable from the URL.
//! - **skipped** — the probe could not complete: a network error, or the bounded
//!   ancestry check timed out on a huge-history repo (the FFmpeg base). Reported,
//!   never hung on.
//!
//! The classification of `git ls-remote` output ([`classify_refs`]) is pure and
//! unit-tested; the network (the `ls-remote` and the bounded ancestry fetch) is the
//! side-effecting shell. This never mutates anything and never resolves "latest" —
//! it is diagnostic only, alongside `verify-sources` and `update`'s pin-time warning.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

/// Wall-clock cap on the `git ls-remote` ref-advertisement probe.
const LS_REMOTE_TIMEOUT: Duration = Duration::from_secs(30);
/// Wall-clock cap on the cheap fetch-by-sha reachability attempt (one commit).
const FETCH_BY_SHA_TIMEOUT: Duration = Duration::from_secs(30);
/// Wall-clock cap on the full-history ancestry fetch — bounded so a huge-history
/// repo (the FFmpeg base) reports `skipped` rather than hanging.
const HISTORY_TIMEOUT: Duration = Duration::from_secs(60);

/// The re-fetch durability of a source pin, from a network probe of its configured
/// URL. `detail` carries the human explanation (which tag/branch, or
/// why it is orphaned/skipped).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Durability {
    /// The commit is a tag target on the remote — immutable, shallow-fetchable
    /// forever. The durable target for a shipped pin.
    Durable(String),
    /// The commit is fetchable now but not tag-anchored (a branch tip, or a past
    /// commit still reachable via history) — a move upstream can orphan it.
    Ephemeral(String),
    /// No tag and no branch reaches the commit — it may exist only in a local
    /// checkout. Not re-fetchable from the configured URL.
    Orphaned(String),
    /// The probe could not complete (network error, or the ancestry check exceeded
    /// its timeout on a huge repo) — durability unconfirmed.
    Skipped(String),
}

impl Durability {
    /// The short status token: `durable` / `ephemeral` / `ORPHANED` / `skipped`.
    /// `ORPHANED` is upper-cased as the one status demanding action.
    pub fn label(&self) -> &'static str {
        match self {
            Durability::Durable(_) => "durable",
            Durability::Ephemeral(_) => "ephemeral",
            Durability::Orphaned(_) => "ORPHANED",
            Durability::Skipped(_) => "skipped",
        }
    }

    /// The human explanation accompanying the status.
    pub fn detail(&self) -> &str {
        match self {
            Durability::Durable(d)
            | Durability::Ephemeral(d)
            | Durability::Orphaned(d)
            | Durability::Skipped(d) => d,
        }
    }

    /// Whether the pin is a durable tag — the only status that needs no follow-up.
    pub fn is_durable(&self) -> bool {
        matches!(self, Durability::Durable(_))
    }
}

/// One advertised ref from `git ls-remote`: its commit and full ref name. A peeled
/// annotated-tag line (`refs/tags/X^{}`) keeps its `^{}` suffix in `name` so
/// [`classify_refs`] can prefer it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LsRef {
    /// The commit (or tag object) sha the ref points at.
    pub sha: String,
    /// The full ref name (`refs/tags/v7.1.1`, `refs/heads/master`, …).
    pub name: String,
}

/// The ref-only verdict from the ls-remote advertisement, before any ancestry
/// check. `NotAdvertised` records whether the pin's own `reference` still names a
/// branch, which decides orphaned-vs-historical once ancestry is (or is not) shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefVerdict {
    /// The commit is the target of a tag — durable.
    TagTarget(String),
    /// The commit is the current tip of a branch — ephemeral.
    BranchTip(String),
    /// The commit is advertised by no tag and no current tip. `ref_branch` is the
    /// still-present branch the pin's `reference` names (if any), used to phrase the
    /// result after the ancestry check.
    NotAdvertised {
        /// The branch the pin's `reference` still names on the remote, if it exists.
        ref_branch: Option<String>,
    },
}

/// Classify `refs` (parsed `git ls-remote` output) for `commit`, pinned from
/// `reference`. Pure, so the tag/tip precedence is unit-testable without a network.
///
/// A tag pointing at the commit wins (durable); else a branch whose tip is the
/// commit (ephemeral); else the commit is not advertised, and we note whether the
/// pin's `reference` is itself a still-present branch (so a historical-but-reachable
/// commit can be told from a truly orphaned one after the ancestry probe).
pub fn classify_refs(refs: &[LsRef], reference: &str, commit: &str) -> RefVerdict {
    // A tag anchoring the commit is the durable case. Match either the peeled
    // annotated-tag line (`refs/tags/X^{}` → the commit) or a lightweight tag whose
    // object *is* the commit; skip the unpeeled annotated-tag object line (it is the
    // tag object sha, not the commit).
    let mut peeled_tag: Option<String> = None;
    let mut light_tag: Option<String> = None;
    for r in refs {
        if r.sha != commit {
            continue;
        }
        if let Some(tag) = r.name.strip_prefix("refs/tags/") {
            if let Some(base) = tag.strip_suffix("^{}") {
                peeled_tag.get_or_insert_with(|| base.to_string());
            } else {
                light_tag.get_or_insert_with(|| tag.to_string());
            }
        }
    }
    if let Some(tag) = peeled_tag.or(light_tag) {
        return RefVerdict::TagTarget(tag);
    }

    // A branch whose current tip is the commit is fetchable now but ephemeral.
    for r in refs {
        if r.sha == commit {
            if let Some(branch) = r.name.strip_prefix("refs/heads/") {
                return RefVerdict::BranchTip(branch.to_string());
            }
        }
    }

    // Not advertised: note whether the pin's own reference still names a branch, to
    // phrase the post-ancestry result (a deleted branch is the orphaned signal).
    let ref_branch = refs
        .iter()
        .find(|r| r.name == format!("refs/heads/{reference}"))
        .map(|_| reference.to_string());
    RefVerdict::NotAdvertised { ref_branch }
}

/// Parse `git ls-remote` stdout into [`LsRef`]s (`<sha>\t<refname>` lines). Pure.
pub fn parse_ls_remote(stdout: &str) -> Vec<LsRef> {
    stdout
        .lines()
        .filter_map(|line| {
            let (sha, name) = line.split_once('\t')?;
            Some(LsRef {
                sha: sha.trim().to_string(),
                name: name.trim().to_string(),
            })
        })
        .collect()
}

/// The cheap, ls-remote-only durability signal for `update`'s pin-time warning.
/// One round-trip per source, no ancestry fetch — so `update`, which
/// resolves several sources, stays fast; the deep reachability check is
/// `verify-sources`' [`probe`] job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PinWarning {
    /// The commit is a tag target — durable, no warning.
    Durable,
    /// The commit is an ephemeral branch tip — a force-push/rebase orphans it; the
    /// string names the branch. Warn (pin a tag).
    Ephemeral(String),
    /// The commit is advertised by no tag and no current branch tip — a hard note:
    /// it may exist only in a local checkout (the mpp anti-pattern) and is not
    /// reproducible from upstream. `verify-sources` confirms reachable-vs-orphaned.
    Unadvertised,
    /// The ls-remote could not run (network/timeout) — noted, never blocks `update`.
    Skipped(String),
}

/// Run `git ls-remote <url>` bounded by [`LS_REMOTE_TIMEOUT`] and parse its refs.
/// `Err` carries a human skip reason (network error / timeout / option-like URL).
fn run_ls_remote(url: &str) -> Result<Vec<LsRef>, String> {
    // A URL beginning with `-` could be read as an option by git; refuse it.
    crate::build::reject_optionlike("source", url).map_err(|e| e.to_string())?;
    match run_bounded(ls_remote_cmd(url), LS_REMOTE_TIMEOUT) {
        Bounded::Ok(out) if out.status.success() => {
            Ok(parse_ls_remote(&String::from_utf8_lossy(&out.stdout)))
        }
        Bounded::Ok(out) => Err(format!(
            "ls-remote failed: {}",
            String::from_utf8_lossy(&out.stderr)
                .lines()
                .next()
                .unwrap_or("unknown error")
                .trim()
        )),
        Bounded::TimedOut => Err(format!("ls-remote exceeded {}s", LS_REMOTE_TIMEOUT.as_secs())),
        Bounded::Spawn(e) => Err(format!("could not run git: {e}")),
    }
}

/// Classify a pin's durability cheaply (ls-remote only) for `update`'s warning.
pub fn pin_warning(url: &str, reference: &str, commit: &str) -> PinWarning {
    let refs = match run_ls_remote(url) {
        Ok(refs) => refs,
        Err(reason) => return PinWarning::Skipped(reason),
    };
    match classify_refs(&refs, reference, commit) {
        RefVerdict::TagTarget(_) => PinWarning::Durable,
        RefVerdict::BranchTip(branch) => PinWarning::Ephemeral(branch),
        RefVerdict::NotAdvertised { .. } => PinWarning::Unadvertised,
    }
}

/// Probe one pin's durability against its configured `url`.
///
/// One `ls-remote` classifies tag/tip cheaply; a commit that is neither is put
/// through a bounded ancestry check — a cheap fetch-by-sha, then a
/// timeout-bounded full-history fetch — so a reachable-but-untagged commit is told
/// from an orphaned one, while a huge-history repo reports `skipped` rather than
/// hanging. Never mutates the configured remote and never writes the lock.
pub fn probe(url: &str, reference: &str, commit: &str) -> Durability {
    let refs = match run_ls_remote(url) {
        Ok(refs) => refs,
        Err(reason) => return Durability::Skipped(reason),
    };
    match classify_refs(&refs, reference, commit) {
        RefVerdict::TagTarget(tag) => Durability::Durable(format!("tag {tag}")),
        RefVerdict::BranchTip(branch) => {
            Durability::Ephemeral(format!("tip of branch {branch} (pin a tag for durability)"))
        }
        RefVerdict::NotAdvertised { ref_branch } => ancestry_probe(url, reference, commit, ref_branch),
    }
}

/// The bounded ancestry check for a commit no ref advertises: first a cheap
/// fetch-by-sha (one commit), then a timeout-bounded full-history fetch. Reports the
/// commit reachable (ephemeral), orphaned, or — when the history fetch times out on
/// a huge repo — skipped.
fn ancestry_probe(url: &str, reference: &str, commit: &str, ref_branch: Option<String>) -> Durability {
    let Ok(tmp) = tempfile::tempdir() else {
        return Durability::Skipped("could not create a scratch dir for the ancestry probe".into());
    };
    let dir = tmp.path();
    if !matches!(run_bounded(git_init_cmd(dir), FETCH_BY_SHA_TIMEOUT), Bounded::Ok(o) if o.status.success())
    {
        return Durability::Skipped("could not init a scratch repo for the ancestry probe".into());
    }

    // Cheap first: a shallow fetch of the exact commit. Servers that honor
    // SHA1-in-want answer immediately; others (GitHub) refuse, and we fall through.
    if matches!(
        run_bounded(fetch_by_sha_cmd(dir, url, commit), FETCH_BY_SHA_TIMEOUT),
        Bounded::Ok(o) if o.status.success()
    ) && matches!(crate::build::probe_object(dir, commit), crate::build::ObjectProbe::Present)
    {
        return Durability::Ephemeral(
            "reachable by direct commit fetch, but not tag-anchored (pin a tag for durability)".into(),
        );
    }

    // Bounded full-history fetch: definitive for a small repo, timed-out (→ skipped)
    // for a huge one so the probe never hangs.
    match run_bounded(fetch_history_cmd(dir, url), HISTORY_TIMEOUT) {
        Bounded::Ok(o) if o.status.success() => match crate::build::probe_object(dir, commit) {
            crate::build::ObjectProbe::Present => Durability::Ephemeral(
                "a past commit still reachable via history, but not tag-anchored \
                 (pin a tag for durability)"
                    .into(),
            ),
            crate::build::ObjectProbe::Absent => {
                let gone = match &ref_branch {
                    Some(_) => String::new(),
                    None => match crate::build::reject_optionlike("ref", reference) {
                        // A bare-commit or deleted-ref pin: name that as the orphan cause.
                        _ if boot2deb_core::sources::is_full_sha(reference) => {
                            " (pinned by bare commit; no branch or tag reaches it)".into()
                        }
                        _ => format!(" (the pinned ref '{reference}' is gone from the remote)"),
                    },
                };
                Durability::Orphaned(format!(
                    "no tag or branch reaches the commit — it may exist only in a local checkout{gone}"
                ))
            }
            // A probe error is not evidence of an orphan — report it as a skipped probe
            // with git's own message, not a false ORPHANED verdict.
            crate::build::ObjectProbe::Errored(detail) => Durability::Skipped(format!(
                "could not probe the commit after a full-history fetch: {detail}"
            )),
        },
        Bounded::Ok(o) => Durability::Skipped(format!(
            "history fetch failed: {}",
            String::from_utf8_lossy(&o.stderr)
                .lines()
                .next()
                .unwrap_or("unknown error")
                .trim()
        )),
        Bounded::TimedOut => Durability::Skipped(format!(
            "ancestry check exceeded {}s (huge-history repo); pin a tag or verify by hand",
            HISTORY_TIMEOUT.as_secs()
        )),
        Bounded::Spawn(e) => Durability::Skipped(format!("could not run git fetch: {e}")),
    }
}

/// `git ls-remote --end-of-options <url>` (all advertised refs, peeled tags
/// included by default).
fn ls_remote_cmd(url: &str) -> Command {
    let mut cmd = Command::new("git");
    crate::build::bound_git_network(&mut cmd);
    cmd.args(["ls-remote", "--end-of-options"]).arg(url);
    cmd
}

/// `git -C <dir> init -q`.
fn git_init_cmd(dir: &Path) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(dir).args(["init", "-q"]);
    cmd
}

/// `git -C <dir> fetch --depth 1 <url> <commit>` — a shallow fetch of one commit.
fn fetch_by_sha_cmd(dir: &Path, url: &str, commit: &str) -> Command {
    let mut cmd = Command::new("git");
    crate::build::bound_git_network(&mut cmd);
    cmd.arg("-C")
        .arg(dir)
        .args(["fetch", "--quiet", "--depth", "1", "--end-of-options"])
        .arg(url)
        .arg(commit);
    cmd
}

/// `git -C <dir> fetch --tags <url> +refs/heads/*:refs/remotes/origin/*` — full
/// history of every branch and tag, for the ancestry check.
fn fetch_history_cmd(dir: &Path, url: &str) -> Command {
    let mut cmd = Command::new("git");
    crate::build::bound_git_network(&mut cmd);
    cmd.arg("-C")
        .arg(dir)
        .args(["fetch", "--quiet", "--tags", "--end-of-options"])
        .arg(url)
        .arg("+refs/heads/*:refs/remotes/origin/*");
    cmd
}

/// The outcome of a wall-clock-bounded subprocess.
enum Bounded {
    /// The process completed within the deadline; its captured output.
    Ok(Output),
    /// The deadline elapsed and the process was killed.
    TimedOut,
    /// The process could not be spawned.
    Spawn(std::io::Error),
}

/// Run `command` with a wall-clock `timeout`, killing it (and reporting
/// [`Bounded::TimedOut`]) if it overruns. stdout/stderr are drained on scoped
/// threads so a chatty child cannot deadlock on a full pipe, and are returned on
/// completion. This is what keeps the ancestry probe from hanging on a huge repo
/// — `git` has no native transfer timeout that bounds every stall.
///
/// The child runs in its **own process group** so that on timeout the whole group
/// is `SIGKILL`ed: `git` fetches over a `git-remote-https` child that inherits the
/// captured pipes, and killing only `git` would leave that helper holding the pipe
/// open — the reader threads would block on it and the "timeout" would not return.
/// Killing the group closes every write end, so the readers hit EOF and join.
fn run_bounded(mut command: Command, timeout: Duration) -> Bounded {
    use std::os::unix::process::CommandExt;
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    // New process group led by the child (pgid == child pid), so a group kill on
    // timeout reaches git's transport helpers too.
    command.process_group(0);
    let mut child = match command.spawn() {
        Ok(c) => c,
        Err(e) => return Bounded::Spawn(e),
    };
    let pgid = child.id() as i32;
    let mut stdout = child.stdout.take().expect("stdout piped");
    let mut stderr = child.stderr.take().expect("stderr piped");
    let deadline = Instant::now() + timeout;

    std::thread::scope(|scope| {
        let out_h = scope.spawn(move || {
            let mut buf = Vec::new();
            let _ = stdout.read_to_end(&mut buf);
            buf
        });
        let err_h = scope.spawn(move || {
            let mut buf = Vec::new();
            let _ = stderr.read_to_end(&mut buf);
            buf
        });
        // Poll for exit until the deadline, then kill the whole group. The reader
        // threads drain the pipes throughout and hit EOF once every group member
        // (git + its transport helper) is gone.
        let mut timed_out = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) => {
                    if Instant::now() >= deadline {
                        kill_group(pgid);
                        // Reap the killed child so its status is collected (the
                        // reader threads then finish as the pipes close).
                        let status = child.wait();
                        timed_out = true;
                        break status.unwrap_or_else(|_| exit_placeholder());
                    }
                    std::thread::sleep(Duration::from_millis(50));
                }
                Err(_) => break exit_placeholder(),
            }
        };
        let stdout = out_h.join().unwrap_or_default();
        let stderr = err_h.join().unwrap_or_default();
        if timed_out {
            Bounded::TimedOut
        } else {
            Bounded::Ok(Output { status, stdout, stderr })
        }
    })
}

/// `SIGKILL` an entire process group by its leader pid (a negative target selects
/// the group). Best-effort — an already-exited group yields `ESRCH`, harmless.
fn kill_group(pgid: i32) {
    // Safety: `kill` is async-signal-safe and takes only integers; a negative pid
    // addresses the process group. No memory is touched.
    unsafe {
        libc::kill(-pgid, libc::SIGKILL);
    }
}

/// A placeholder failing exit status for the unreachable-in-practice case where
/// `wait` itself errors after a kill.
fn exit_placeholder() -> std::process::ExitStatus {
    // A non-zero exit via a trivially-failing command; only used if `wait` errors.
    Command::new("false")
        .status()
        .unwrap_or_else(|_| std::process::Command::new("sh").arg("-c").arg("exit 1").status().expect("shell exits"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(sha: &str, name: &str) -> LsRef {
        LsRef { sha: sha.into(), name: name.into() }
    }

    #[test]
    fn parses_ls_remote_lines() {
        let out = "\
c9acdc466e9aa96352f658b9276aa8a45b8e817d\trefs/tags/v7.1.1\n\
c9acdc466e9aa96352f658b9276aa8a45b8e817d\trefs/tags/v7.1.1^{}\n\
abc1230000000000000000000000000000000000\trefs/heads/master\n";
        let refs = parse_ls_remote(out);
        assert_eq!(refs.len(), 3);
        assert_eq!(refs[1].name, "refs/tags/v7.1.1^{}");
    }

    #[test]
    fn tag_target_is_durable() {
        // An annotated tag: the peeled line carries the commit; the unpeeled line is
        // the tag object (different sha) and must not be mistaken for the commit.
        let commit = "c9acdc466e9aa96352f658b9276aa8a45b8e817d";
        let refs = vec![
            r("1111111111111111111111111111111111111111", "refs/tags/v7.1.1"),
            r(commit, "refs/tags/v7.1.1^{}"),
            r("abc1230000000000000000000000000000000000", "refs/heads/master"),
        ];
        assert_eq!(classify_refs(&refs, "v7.1.1", commit), RefVerdict::TagTarget("v7.1.1".into()));
    }

    #[test]
    fn lightweight_tag_target_is_durable() {
        // The mpp durable tag is lightweight: the tag line points straight at the commit.
        let commit = "750e76ec2d9287babfaf08c8bf395ebc5e8778ea";
        let refs = vec![r(commit, "refs/tags/v1.5.0-1-20260121-750e76e")];
        assert_eq!(
            classify_refs(&refs, "v1.5.0-1-20260121-750e76e", commit),
            RefVerdict::TagTarget("v1.5.0-1-20260121-750e76e".into())
        );
    }

    #[test]
    fn branch_tip_is_ephemeral() {
        let commit = "abc1230000000000000000000000000000000000";
        let refs = vec![
            r("ffff000000000000000000000000000000000000", "refs/tags/v1.0"),
            r(commit, "refs/heads/develop"),
        ];
        assert_eq!(
            classify_refs(&refs, "develop", commit),
            RefVerdict::BranchTip("develop".into())
        );
    }

    #[test]
    fn a_tag_wins_over_a_same_commit_branch_tip() {
        // A commit that is both a tag target and a branch tip is reported durable.
        let commit = "abc1230000000000000000000000000000000000";
        let refs = vec![
            r(commit, "refs/tags/v2.0"),
            r(commit, "refs/heads/release"),
        ];
        assert_eq!(classify_refs(&refs, "v2.0", commit), RefVerdict::TagTarget("v2.0".into()));
    }

    #[test]
    fn unadvertised_commit_notes_whether_its_branch_survives() {
        let commit = "dead0000000000000000000000000000000000ff";
        // The mpp anti-pattern: pinned by bare commit, its branch deleted → no branch
        // named by the reference remains, and the commit is not a tip.
        let refs = vec![r("aaaa000000000000000000000000000000000000", "refs/heads/develop")];
        assert_eq!(
            classify_refs(&refs, commit, commit),
            RefVerdict::NotAdvertised { ref_branch: None }
        );
        // A historical commit whose branch still exists (tip moved on): the reference
        // still names a live branch.
        assert_eq!(
            classify_refs(&refs, "develop", commit),
            RefVerdict::NotAdvertised { ref_branch: Some("develop".into()) }
        );
    }

    #[test]
    fn durability_labels_and_detail() {
        assert_eq!(Durability::Durable("tag v1".into()).label(), "durable");
        assert_eq!(Durability::Ephemeral("tip".into()).label(), "ephemeral");
        assert_eq!(Durability::Orphaned("gone".into()).label(), "ORPHANED");
        assert_eq!(Durability::Skipped("timeout".into()).label(), "skipped");
        assert_eq!(Durability::Orphaned("gone".into()).detail(), "gone");
        assert!(Durability::Durable("t".into()).is_durable());
        assert!(!Durability::Ephemeral("t".into()).is_durable());
    }

    #[test]
    fn run_bounded_captures_output_and_enforces_the_deadline() {
        // A fast command completes and its output is captured.
        let mut c = Command::new("sh");
        c.args(["-c", "echo hi; echo err >&2"]);
        match run_bounded(c, Duration::from_secs(5)) {
            Bounded::Ok(out) => {
                assert!(out.status.success());
                assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
                assert_eq!(String::from_utf8_lossy(&out.stderr).trim(), "err");
            }
            _ => panic!("expected completion"),
        }
        // A slow command is killed at the deadline rather than hanging.
        let mut slow = Command::new("sh");
        slow.args(["-c", "sleep 30"]);
        let start = Instant::now();
        assert!(matches!(run_bounded(slow, Duration::from_millis(300)), Bounded::TimedOut));
        assert!(start.elapsed() < Duration::from_secs(5), "should not wait for the full sleep");
    }

    #[test]
    fn probe_skips_an_optionlike_url() {
        // Defence in depth: a URL that looks like a git option is refused, not run.
        let d = probe("--upload-pack=touch /tmp/x", "v1", "0".repeat(40).as_str());
        assert_eq!(d.label(), "skipped");
    }
}
