//! The compile stages of the build graph — `git`/`make` steps wrapped as
//! engine subprocess stages that read the resolved [`Lock`] and emit the
//! structured [`Event`](crate::event::Event) stream.
//!
//! These stages build the device's kernel, bootloader, and media stack: [`kernel`]
//! (`git am` series + `make bindeb-pkg`), [`uboot`], the [`userspace`] MPP/RGA
//! `.deb`s, and the [`ffmpeg`] `ffmpeg-rk` `.deb`. The userspace and ffmpeg stages
//! compile arm64 `.deb`s in a target-arch
//! [`BuildSandbox`](crate::sandbox::BuildSandbox). These stages drive the compile
//! invocations directly rather than reimplementing them: the value here is
//! the typed orchestration, the lock-driven pins, and the event stream, not a new
//! build system.
//!
//! [`Lock`]: boot2deb_core::lock::Lock

pub mod ffmpeg;
pub mod kernel;
pub mod uboot;
pub mod userspace;

use crate::error::EngineError;
use crate::event::{Step, Stream};
use crate::{git, patches};
use boot2deb_core::lock::Lock;
use boot2deb_core::PatchProfile;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};

/// How many trailing stderr lines to keep for a [`EngineError::CommandFailed`]
/// message. The full output already reached the caller as [`Event::Log`] events
/// ([`Event`](crate::event::Event)); this is just a self-contained error summary.
const STDERR_TAIL: usize = 40;

/// Host/target build parameters shared by the compile stages.
#[derive(Debug, Clone, Default)]
pub struct BuildEnv {
    /// `CROSS_COMPILE` prefix, `Some` when the host arch differs from the target;
    /// `None` for a native build (no prefix passed to `make`).
    pub cross_compile: Option<String>,
    /// `make -j` parallelism; `None` lets the stage default to the host's
    /// available parallelism.
    pub jobs: Option<usize>,
    /// Identity of the host cross toolchain
    /// ([`toolchain::host_cc_identity`](crate::toolchain::host_cc_identity)), folded
    /// into the kernel/u-boot Tier-2 output signatures so an artifact built
    /// with one compiler is not restored for a build using another. Empty when the
    /// artifact cache is disabled (its value then never keys anything).
    pub toolchain_id: String,
}

impl BuildEnv {
    /// Resolved job count: the configured value or the host's available
    /// parallelism (falling back to 1).
    fn jobs(&self) -> usize {
        self.jobs.unwrap_or_else(|| {
            std::thread::available_parallelism()
                .map(|n| n.get())
                .unwrap_or(1)
        })
    }
}

/// The lock's kernel pin, or a typed error if this build compiles no kernel.
///
/// A lock omits a pin exactly when the build has no such dependency — a
/// distro-package kernel is installed from the mirror, so there is no commit to
/// pin — and the CLI schedules the compile nodes from the *resolved build*, which
/// agrees. So a stage reaching here with no pin means the lock and the config have
/// drifted apart (a lock written before the kernel's flavor changed), and re-running
/// `update` is the fix. These accessors are where that mismatch is named, once,
/// instead of every field access having to cope with an absence that should not
/// happen.
pub(crate) fn kernel_pin(lock: &Lock) -> Result<&boot2deb_core::lock::KernelPin, EngineError> {
    lock.kernel.as_ref().ok_or(EngineError::MissingPin {
        what: "kernel",
        stage: "kernel",
    })
}

/// The lock's u-boot pin, or a typed error if this build's boot method compiles none.
pub(crate) fn uboot_pin(lock: &Lock) -> Result<&boot2deb_core::lock::UbootPin, EngineError> {
    lock.uboot.as_ref().ok_or(EngineError::MissingPin {
        what: "uboot",
        stage: "uboot",
    })
}

/// The lock's rkbin blob pins, or a typed error if this build's boot method uses none.
pub(crate) fn blob_pins(lock: &Lock) -> Result<&boot2deb_core::lock::BlobsPin, EngineError> {
    lock.blobs.as_ref().ok_or(EngineError::MissingPin {
        what: "blobs",
        stage: "uboot",
    })
}

/// Run `command` to completion, relaying every stdout/stderr line to `step` as a
/// [`Event::Log`](crate::event::Event) as it is produced, and mapping a non-zero
/// exit to [`EngineError::CommandFailed`] (with a tail of stderr for context).
///
/// stdout and stderr are read on separate threads so a chatty stage cannot
/// deadlock on a full pipe; the sink is only touched on the calling thread, so it
/// need not be `Send`. `tool` names the program for errors (`make`, `git`),
/// `context` describes the invocation.
///
/// This is the single host-side command choke point (native compiles, the cross
/// `bwrap` wrapper, kernel/u-boot `make`, `git`, the rootfs `mmdebstrap` bootstrap,
/// `dpkg-deb`), so it normalizes the determinism-relevant environment here — `TZ=UTC`
/// and `LC_ALL=C.UTF-8`, matching the cross sandbox's `--clearenv` + `SANDBOX_ENV`
/// discipline so a host's timezone/locale cannot leak into packaged output,
/// and the kbuild-honored flag variables (`KCFLAGS`/`KAFLAGS`/`KCPPFLAGS`) plus
/// `MAKEFLAGS`/`GNUMAKEFLAGS` are removed, so a flag exported in the host shell
/// cannot silently shape the kernel/u-boot bytes a lock-keyed cache entry claims
/// to reproduce. A full `env_clear` is unsafe on the host (it would drop
/// the `PATH`/`HOME` the tools need); the caller's own env (e.g.
/// `SOURCE_DATE_EPOCH`) is already set and preserved.
///
/// **stdin is `/dev/null`.** A build is non-interactive by construction, and a tool
/// that decides to ask a question — kbuild's `conf` dropping into `oldaskconfig` on an
/// out-of-date `.config` is the live example — must fail or take its default rather
/// than block forever on a terminal that may not even be attached.
pub fn run(
    mut command: Command,
    tool: &str,
    context: &str,
    step: &Step,
) -> Result<(), EngineError> {
    command.env("TZ", "UTC").env("LC_ALL", "C.UTF-8");
    for flag_var in ["KCFLAGS", "KAFLAGS", "KCPPFLAGS", "MAKEFLAGS", "GNUMAKEFLAGS"] {
        command.env_remove(flag_var);
    }
    command.stdin(Stdio::null());
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command
        .spawn()
        .map_err(|source| EngineError::CommandSpawn {
            command: tool.to_string(),
            context: context.to_string(),
            source,
        })?;
    let stdout = child.stdout.take().expect("stdout is piped");
    let stderr = child.stderr.take().expect("stderr is piped");

    let (tx, rx) = mpsc::channel::<(Stream, String)>();
    let mut stderr_tail: VecDeque<String> = VecDeque::with_capacity(STDERR_TAIL);
    // Reader threads own their sender and drop it at EOF; when both are gone the
    // channel closes and the receive loop below ends.
    std::thread::scope(|scope| {
        let tx_out = tx.clone();
        scope.spawn(move || forward(stdout, Stream::Stdout, tx_out));
        scope.spawn(move || forward(stderr, Stream::Stderr, tx));
        for (stream, line) in rx {
            if stream == Stream::Stderr {
                if stderr_tail.len() == STDERR_TAIL {
                    stderr_tail.pop_front();
                }
                stderr_tail.push_back(line.clone());
            }
            step.emit(stream, line);
        }
    });

    let status = child.wait().map_err(|source| EngineError::CommandSpawn {
        command: tool.to_string(),
        context: format!("waiting for {context}"),
        source,
    })?;
    if status.success() {
        Ok(())
    } else {
        Err(EngineError::CommandFailed {
            command: tool.to_string(),
            context: context.to_string(),
            status: status.code(),
            stderr: stderr_tail.into_iter().collect::<Vec<_>>().join("\n"),
        })
    }
}

/// git's low-speed stall-abort thresholds for network transfers: a
/// transfer averaging under [`GIT_STALL_BYTES_PER_SEC`] bytes/second for
/// [`GIT_STALL_SECS`] seconds is aborted by git, so a stalled mirror/remote fails
/// the operation instead of hanging `build`/`update` indefinitely. Stall-based
/// rather than a fixed wall-clock cap, so a legitimately slow-but-progressing clone
/// (a large kernel history) still completes.
const GIT_STALL_BYTES_PER_SEC: &str = "1000";
/// Seconds a transfer may stay under [`GIT_STALL_BYTES_PER_SEC`] before git aborts it.
const GIT_STALL_SECS: &str = "60";

/// Apply git's http low-speed stall abort to a network-facing git `Command` — the
/// clone/fetch operations that talk to a remote. Must be called on a fresh
/// `Command::new("git")` before the subcommand args, since `-c` config is only
/// honored ahead of the subcommand. Set both as `-c` config (git's own transport)
/// and via `GIT_HTTP_LOW_SPEED_*` env (read by the `git-remote-https` helper). Local
/// git ops (init/checkout/rev-parse/cat-file) touch no remote and are left unbounded.
pub(crate) fn bound_git_network(cmd: &mut Command) {
    cmd.args(["-c", &format!("http.lowSpeedLimit={GIT_STALL_BYTES_PER_SEC}")])
        .args(["-c", &format!("http.lowSpeedTime={GIT_STALL_SECS}")])
        .env("GIT_HTTP_LOW_SPEED_LIMIT", GIT_STALL_BYTES_PER_SEC)
        .env("GIT_HTTP_LOW_SPEED_TIME", GIT_STALL_SECS);
}

/// Total clone attempts before a transient failure is fatal (initial try + retries).
const CLONE_ATTEMPTS: u32 = 4;

/// Shallow-clone `source` at `reference` into `tree`, retrying transient failures.
///
/// Git hosts flake — a shallow clone can die mid-transfer on an HTTP 5xx, an RPC
/// desync, or a dropped connection (e.g. the RK1's u-boot upstream at denx.de).
/// A *transient* failure is retried (up to a small fixed attempt count) with an
/// increasing backoff; a *non-transient* one (an unknown ref, auth failure, a
/// missing `git`) fails immediately without wasting retries. Because a failed
/// clone leaves a partial checkout that would make the next `git clone` refuse a
/// non-empty target, the partial `tree` is removed between attempts — safe
/// because callers only clone into a fresh path (an existing tree is reused, not
/// re-cloned). On the final failure the underlying [`EngineError`] is returned
/// unchanged, so the real cause is still surfaced.
pub fn clone_shallow(
    source: &str,
    reference: &str,
    tree: &Path,
    step: &Step,
) -> Result<(), EngineError> {
    reject_optionlike("source", source)?;
    reject_optionlike("ref", reference)?;
    let ctx = format!("clone {source} @ {reference}");
    for attempt in 1..=CLONE_ATTEMPTS {
        let mut clone = Command::new("git");
        bound_git_network(&mut clone);
        clone
            // `--end-of-options` stops a `source`/`tree` beginning with `-` from
            // being read as a flag; the value guards above reject the same up front.
            .args(["clone", "--depth", "1", "--branch", reference, "--end-of-options"])
            .arg(source)
            .arg(tree);
        match run(clone, "git", &ctx, step) {
            Ok(()) => return Ok(()),
            Err(e) => {
                let last = attempt == CLONE_ATTEMPTS;
                if last || !is_transient_clone_error(&e) {
                    return Err(e);
                }
                step.log(format!(
                    "clone attempt {attempt}/{CLONE_ATTEMPTS} failed transiently ({}); retrying",
                    error_summary(&e)
                ));
                // Clear the partial checkout so the retry can clone into a fresh path.
                let _ = std::fs::remove_dir_all(tree);
                std::thread::sleep(std::time::Duration::from_secs(2u64.pow(attempt)));
            }
        }
    }
    unreachable!("the final attempt returns Err rather than looping")
}

/// Whether a failed clone looks like a retryable network/transport hiccup rather
/// than a permanent error (bad ref, auth, missing `git`). Classifies on the
/// captured stderr — pure, so the marker set is unit-testable without a network.
fn is_transient_clone_error(e: &EngineError) -> bool {
    let EngineError::CommandFailed { stderr, .. } = e else {
        // A spawn failure (e.g. `git` not installed) is not going to fix itself.
        return false;
    };
    let s = stderr.to_ascii_lowercase();
    /// Substrings that mark a transport-layer failure git can recover from on a retry.
    const MARKERS: &[&str] = &[
        "rpc failed",
        "http 5",                        // 500/502/503/504 from the git host
        "returned error: 5",             // curl's rendering of an HTTP 5xx
        "early eof",
        "unexpected disconnect",
        "remote end hung up",
        "transfer closed",
        "could not resolve host",
        "couldn't connect",
        "failed to connect",
        "connection timed out",
        "connection reset",
        "gnutls_handshake",
        "ssl_error",
        "ssl connect error",
        "expected 'acknowledgments'",    // truncated protocol-v2 response
    ];
    MARKERS.iter().any(|m| s.contains(m))
}

/// The last non-empty stderr line of a failed command, for a one-line retry log.
fn error_summary(e: &EngineError) -> String {
    match e {
        EngineError::CommandFailed { stderr, .. } => stderr
            .lines()
            .rev()
            .find(|l| !l.trim().is_empty())
            .unwrap_or("")
            .to_string(),
        other => other.to_string(),
    }
}

/// Read `reader` line by line, sending each line (newline stripped) on `tx`.
/// Stops on EOF, read error, or a closed channel.
///
/// Reads raw bytes and decodes with [`String::from_utf8_lossy`] rather than
/// `BufRead::lines` (which yields an error and *ends the stream* on the first
/// non-UTF-8 byte): a build tool that prints a stray non-UTF-8 byte must not sever
/// the reader thread and starve the child of its pipe.
fn forward<R: Read>(reader: R, stream: Stream, tx: Sender<(Stream, String)>) {
    let mut reader = BufReader::new(reader);
    let mut buf = Vec::new();
    loop {
        buf.clear();
        match reader.read_until(b'\n', &mut buf) {
            Ok(0) => break, // EOF
            Ok(_) => {
                // Strip the trailing newline (and a CR before it, if any).
                while matches!(buf.last(), Some(b'\n' | b'\r')) {
                    buf.pop();
                }
                let line = String::from_utf8_lossy(&buf).into_owned();
                if tx.send((stream, line)).is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

/// Fetch `source` at `reference` into a fresh `dir`, check out the exact commit,
/// and verify it is `commit` — the same "build reads only the lock" guarantee the
/// kernel/u-boot stages enforce, for sources pinned by commit rather than
/// tag (the userspace/ffmpeg trees).
///
/// `git init` + `fetch --depth 1 <source> <reference>` works uniformly whether
/// `source` is a URL or a local checkout and whether `reference` is a branch, tag,
/// or (reachable) commit, so one path serves the from-URL and fast local-clone
/// cases. `what` labels the tree for a [`EngineError::CommitMismatch`]. On any
/// failure the partial `dir` is removed, so a caller's `dir.exists()` reuse check
/// only ever sees a completed fetch.
pub(crate) fn fetch_commit(
    source: &str,
    reference: &str,
    commit: &str,
    what: &str,
    dir: &Path,
    step: &Step,
) -> Result<(), EngineError> {
    let result = fetch_commit_inner(source, reference, commit, what, dir, step);
    if result.is_err() {
        // Don't leave a half-fetched tree that a reuse check would trust.
        let _ = std::fs::remove_dir_all(dir);
    }
    result
}

fn fetch_commit_inner(
    source: &str,
    reference: &str,
    commit: &str,
    what: &str,
    dir: &Path,
    step: &Step,
) -> Result<(), EngineError> {
    reject_optionlike("source", source)?;
    reject_optionlike("ref", reference)?;
    std::fs::create_dir_all(dir).map_err(|source| EngineError::io(dir, source))?;
    // A local source given as a relative path must be absolutized: `git -C <dir>`
    // resolves it relative to `<dir>`, not our CWD. A URL is left untouched.
    let resolved = resolve_local_source(source);
    let mut init = Command::new("git");
    init.arg("-C").arg(dir).args(["init", "-q"]);
    run(init, "git", &format!("init {}", dir.display()), step)?;

    // Fetch the *exact* locked commit first: a shallow fetch of the reference only
    // gets its current tip, so once upstream moves past the pin the ref no longer
    // reaches the locked commit. A shallow fetch-by-sha works for a local
    // source, an advertised ref tip, or a server honoring SHA1-in-want.
    if try_fetch_commit(dir, &resolved, commit) {
        let mut checkout = Command::new("git");
        // `git checkout --detach <commit>` takes a revision, not a pathspec, so it
        // must NOT be given `--end-of-options`: in detach mode git classifies
        // everything after that marker as a pathspec and rejects it ("--detach does
        // not take a path argument"). The commit is a lock-resolved hex SHA — never
        // option-like — and checkout has no injectable remote-exec option (that
        // vector is fetch/clone's --upload-pack, still guarded), so this is safe.
        checkout
            .arg("-C")
            .arg(dir)
            .args(["checkout", "-q", "--detach", commit]);
        run(checkout, "git", &format!("checkout {commit}"), step)?;
    } else {
        // The server would not serve the bare commit shallowly: GitHub refuses an
        // arbitrary historical SHA ("upload-pack: not our ref"), and the lock records
        // `reference == commit` for these sources, so there is no lighter
        // advertised ref to shallow-fetch. Fetch the full history of every branch and
        // tag so the pinned commit is reachable as an ancestor, then check it out.
        // This is the cost of a from-URL build of a historical pin with no local
        // checkout; `--<pkg>-src <checkout>` takes the shallow path above instead.
        // (Mirrors the gix patch-fetch, which also fetches full history for the same
        // reason,.)
        let mut fetch = Command::new("git");
        bound_git_network(&mut fetch);
        // `--end-of-options` keeps the source/refspec positionals from being read as
        // flags (defence in depth over the value guards above).
        fetch
            .arg("-C")
            .arg(dir)
            .args(["fetch", "--tags", "--end-of-options"])
            .arg(&resolved)
            .arg("+refs/heads/*:refs/remotes/origin/*");
        run(fetch, "git", &format!("fetch (full history) {resolved}"), step)?;
        // Even a full history may not contain the pin if its upstream branch was
        // rebased/force-pushed/deleted (the commit is orphaned upstream). Detect that
        // here and report it actionably, rather than letting `checkout` fail with a
        // cryptic "reference is not a tree". A probe that itself errored (bad repo,
        // git failure) surfaces as a git error with its stderr, not a false
        // "unreachable" verdict.
        match probe_object(dir, commit) {
            ObjectProbe::Present => {}
            ObjectProbe::Absent => {
                return Err(EngineError::CommitUnreachable {
                    what: what.to_string(),
                    url: source.to_string(),
                    commit: commit.to_string(),
                });
            }
            ObjectProbe::Errored(detail) => {
                return Err(EngineError::GitFailed {
                    context: format!("probe for {commit} after full-history fetch of {source}"),
                    status: None,
                    stderr: detail,
                });
            }
        }
        let mut checkout = Command::new("git");
        checkout
            .arg("-C")
            .arg(dir)
            .args(["checkout", "-q", "--detach", commit]);
        run(checkout, "git", &format!("checkout {commit}"), step)?;
    }

    // `rev-parse HEAD` emits lowercase; canonicalize the pin the same way so a
    // sha that entered the lock uppercased (e.g. a hand-edited lock) still matches
    // by object identity rather than raising a spurious mismatch.
    let head = git::rev_parse_head(dir)?;
    let expected = boot2deb_core::sources::normalize_ref(commit);
    if head != expected {
        return Err(EngineError::CommitMismatch {
            what: what.to_string(),
            expected,
            actual: head,
        });
    }
    Ok(())
}

/// Outcome of [`probe_object`]'s reachability check, distinguishing a commit that is
/// genuinely absent from a probe that could not run. Collapsing both to a
/// single `false` would make a git/repo error surface as `CommitUnreachable`/`Orphaned`
/// — a misdiagnosis — so the classifier keeps them apart.
#[derive(Debug)]
pub(crate) enum ObjectProbe {
    /// The commit object is present in the repo.
    Present,
    /// The probe ran cleanly and the object is not in the repo (`git cat-file -e` exit
    /// non-zero with no error output — its designed "absent" signal).
    Absent,
    /// The probe itself failed — git could not be run, or errored for a reason other
    /// than a missing object (bad repo, malformed rev). Carries the stderr/spawn detail
    /// so the caller can report it faithfully instead of as an absence.
    Errored(String),
}

/// Probe whether the object `commit` is present in the repo at `dir`
/// (`git cat-file -e <commit>`), used after a full-history fetch to distinguish
/// "orphaned upstream" from a checkout that would otherwise fail cryptically. Shared
/// with the durability probe ([`crate::sources`]).
///
/// Returns the three-way [`ObjectProbe`] rather than a bare `bool`. The plain
/// (unpeeled) form is deliberate: `git cat-file -e <sha>` exits **1 with empty stderr**
/// when the object is simply absent, but exits **128 with a `fatal:` message** on a
/// real error (a broken repo, an unreadable object db) — so an empty stderr means
/// [`ObjectProbe::Absent`] and a non-empty one means [`ObjectProbe::Errored`]. (The
/// `^{commit}`-peeled form instead prints `fatal: Not a valid object name` on a
/// genuine absence, which would masquerade as an error.) The pin is a full commit sha,
/// so object-presence is equivalent to commit-presence here; a spawn failure is also
/// `Errored`.
pub(crate) fn probe_object(dir: &Path, commit: &str) -> ObjectProbe {
    match Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(["cat-file", "-e", commit])
        .output()
    {
        Ok(o) if o.status.success() => ObjectProbe::Present,
        Ok(o) => {
            let stderr = String::from_utf8_lossy(&o.stderr);
            let stderr = stderr.trim();
            if stderr.is_empty() {
                ObjectProbe::Absent
            } else {
                ObjectProbe::Errored(stderr.to_string())
            }
        }
        Err(e) => ObjectProbe::Errored(format!("could not run git cat-file in {}: {e}", dir.display())),
    }
}

/// Attempt a shallow fetch of the exact `commit` from `source`; `true` on success.
/// Quiet by design — a failure is an expected fallback (a server may forbid
/// fetch-by-sha), not an error to stream, so the reference path can take over.
fn try_fetch_commit(dir: &Path, source: &str, commit: &str) -> bool {
    let mut cmd = Command::new("git");
    bound_git_network(&mut cmd);
    cmd.arg("-C")
        .arg(dir)
        .args(["fetch", "--depth", "1", "--end-of-options", source, commit])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Reject a `git` source/ref that begins with `-`, so it can never be read as a
/// command-line option (`--upload-pack=<cmd>` is remote code execution). `what`
/// labels the argument for the error. Pure, so the guard is unit-testable.
pub(crate) fn reject_optionlike(what: &'static str, value: &str) -> Result<(), EngineError> {
    if value.starts_with('-') {
        Err(EngineError::UnsafeGitArgument {
            what,
            value: value.to_string(),
        })
    } else {
        Ok(())
    }
}

/// Reject a config-derived `make` target (a defconfig name) that GNU make would
/// read as something other than a target: a leading `-` is parsed as an option,
/// and an embedded `=` is a variable assignment (`CC=<cmd>` injects an arbitrary
/// tool). Legitimate defconfig targets are bare identifiers, so both shapes are
/// refused before the value reaches `make`; call sites additionally pass the target
/// after `--` so make cannot reinterpret it. Pure, so it is unit-testable.
pub(crate) fn reject_unsafe_make_target(what: &'static str, value: &str) -> Result<(), EngineError> {
    if value.starts_with('-') || value.contains('=') {
        Err(EngineError::UnsafeMakeTarget {
            what,
            value: value.to_string(),
        })
    } else {
        Ok(())
    }
}

/// Resolve a clone source: an existing local path (possibly relative to the
/// caller's CWD) is canonicalized to absolute so `git -C <dir> fetch` finds it; a
/// URL (or a non-existent path) is returned unchanged for git to interpret.
pub(crate) fn resolve_local_source(source: &str) -> String {
    let p = Path::new(source);
    if p.exists() {
        std::fs::canonicalize(p)
            .map(|abs| abs.to_string_lossy().into_owned())
            .unwrap_or_else(|_| source.to_string())
    } else {
        source.to_string()
    }
}

/// How a pinned source tree is obtained by [`clone_pinned`].
pub(crate) enum CloneMode {
    /// `git clone --depth 1 --branch <ref>` with transient-failure retry
    /// ([`clone_shallow`]), for tag/branch-pinned sources (kernel, u-boot). The
    /// resulting HEAD is verified against the pin afterwards.
    Shallow,
    /// `git init` + `fetch --depth 1 <ref>` ([`fetch_commit`]), for a
    /// commit-reachable reference (the ffmpeg base). The fetch verifies the commit
    /// itself.
    Fetch,
}

/// Which of a [`PatchProfile`]'s per-tree series is applied — by [`clone_pinned`]
/// (kernel/u-boot/ffmpeg) or by the userspace stage via [`apply_profile_scope`].
#[derive(Clone, Copy)]
pub(crate) enum PatchScope {
    /// The kernel-tree series.
    Kernel,
    /// The u-boot-tree series.
    Uboot,
    /// The ffmpeg-tree series.
    Ffmpeg,
    /// The userspace-tree series (the MPP CMA fix). Applies to the MPP
    /// tree; librga/libmali carry no userspace patch and never use this scope.
    Userspace,
}

impl PatchScope {
    /// The profile's ordered patch list for this scope.
    fn series<'a>(&self, profile: &'a PatchProfile) -> &'a [String] {
        match self {
            PatchScope::Kernel => &profile.kernel,
            PatchScope::Uboot => &profile.uboot,
            PatchScope::Ffmpeg => &profile.ffmpeg,
            PatchScope::Userspace => &profile.userspace,
        }
    }

    /// The tree label used in patch-apply messages.
    fn tree_label(&self) -> &'static str {
        match self {
            PatchScope::Kernel => "kernel",
            PatchScope::Uboot => "uboot",
            PatchScope::Ffmpeg => "ffmpeg",
            PatchScope::Userspace => "userspace",
        }
    }
}

/// The inputs for [`clone_pinned`]: where to clone the source from and how, plus
/// the patch series to apply on top.
pub(crate) struct ClonePinned<'a> {
    /// Clone source (git URL or local path).
    pub source: &'a str,
    /// The ref the pin resolved from (tag/branch/commit-reachable ref).
    pub reference: &'a str,
    /// The exact commit the tree must sit at (`lock.<source>.commit`).
    pub commit: &'a str,
    /// How to obtain the tree.
    pub mode: CloneMode,
    /// Destination tree path. The caller checks reuse first, so this must not
    /// already exist.
    pub tree: &'a Path,
    /// Label for a [`EngineError::CommitMismatch`] (e.g. `"kernel"`, `"u-boot"`,
    /// `"ffmpeg base"`).
    pub what: &'a str,
    /// The patch series to apply on top, or `None` when the resolved kernel names no
    /// patch profile — the tree is then compiled exactly as cloned.
    pub patches: Option<PatchSource<'a>>,
    /// Which per-tree series to apply.
    pub scope: PatchScope,
    /// Message label for the patched tree (e.g. `"kernel @ v7.1.1"`).
    pub target: &'a str,
    /// When `Some`, gate the profile's declared kernel range against this ref
    /// before applying — the kernel node's declared-intent gate.
    pub gate_reference: Option<&'a str>,
}

/// A resolved `patches` checkout together with the pin and profile it supplies.
///
/// Bundled rather than carried as four loose fields so that "this build applies no
/// patches" is one `Option::None` the compiler enforces: there is no way to name a
/// profile without a checkout to read it from, nor to resolve a checkout for a build
/// that has no series.
#[derive(Clone, Copy)]
pub struct PatchSource<'a> {
    /// The `patches` checkout the series is read from.
    pub root: &'a Path,
    /// The lock's pin: which profile, at which `patches`-repo commit. Borrowed from
    /// the lock rather than copied field-by-field, so the same value feeds both the
    /// apply step and the signature fold.
    pub pin: &'a boot2deb_core::lock::PatchesPin,
    /// The checkout was chosen explicitly via `--patches-path` for co-development:
    /// a pin mismatch is a loud warning rather than an error.
    pub dev: bool,
}

/// Clone/fetch the pinned source into `tree`, verify it sits at the locked commit,
/// enforce the patches-checkout pin, and apply the locked series in place —
/// leaving `tree` at the fully-patched source the build compiles. Returns the
/// number of patches applied.
///
/// On **any** failure the partial `tree` is removed, so a resume's `tree.exists()`
/// check never trusts a half-cloned or half-patched tree. This is the one
/// place the patches pin is enforced: a drifted `patches` checkout would silently
/// apply a different series than the lock names.
pub(crate) fn clone_pinned(spec: &ClonePinned, step: &Step) -> Result<usize, EngineError> {
    let result = clone_pinned_inner(spec, step);
    if result.is_err() {
        // Never leave a partially-built tree a later run would reuse as "ready".
        let _ = std::fs::remove_dir_all(spec.tree);
    }
    result
}

fn clone_pinned_inner(spec: &ClonePinned, step: &Step) -> Result<usize, EngineError> {
    match spec.mode {
        CloneMode::Shallow => {
            clone_shallow(spec.source, spec.reference, spec.tree, step)?;
            // The build reads only the lock: a clone that lands on a different
            // commit is a hard error, not a silently different tree. Normalize
            // the expected side like the `Fetch` arm does, so both arms accept
            // the same pin spellings (an uppercase-hex hand edit names the same
            // object git prints in lowercase).
            let head = git::rev_parse_head(spec.tree)?;
            let expected = boot2deb_core::sources::normalize_ref(spec.commit);
            if head != expected {
                return Err(EngineError::CommitMismatch {
                    what: spec.what.to_string(),
                    expected,
                    actual: head,
                });
            }
        }
        // fetch_commit verifies the commit itself (and cleans up its own partial dir).
        CloneMode::Fetch => {
            fetch_commit(spec.source, spec.reference, spec.commit, spec.what, spec.tree, step)?;
        }
    }
    apply_profile_scope(
        &ApplyScope {
            tree: spec.tree,
            patches: spec.patches,
            scope: spec.scope,
            target: spec.target,
            gate_reference: spec.gate_reference,
        },
        step,
    )
}

/// The inputs for [`apply_profile_scope`]: an already-checked-out `tree` plus the
/// patches checkout, pin, and which profile scope to apply.
pub(crate) struct ApplyScope<'a> {
    /// The source tree to apply the series onto, in place. The caller has already
    /// checked it out at the locked commit and must have it clean.
    pub tree: &'a Path,
    /// The series to apply, or `None` when the build's kernel names no patch profile.
    pub patches: Option<PatchSource<'a>>,
    /// Which per-tree series to apply.
    pub scope: PatchScope,
    /// Message label for the patched tree (e.g. `"kernel @ v7.1.1"`).
    pub target: &'a str,
    /// When `Some`, gate the profile's declared kernel range against this ref
    /// before applying — the kernel node's declared-intent gate.
    pub gate_reference: Option<&'a str>,
}

/// Enforce the patches-checkout pin, load the profile, optionally gate its
/// declared kernel range, and apply the profile's `scope` series onto an
/// already-checked-out `tree` in place — leaving the fully-patched source the build
/// compiles. Returns the number of patches applied.
///
/// A build whose kernel names no patch profile (`spec.patches` is `None`) applies
/// nothing and reads no `patches` checkout: it returns `0` before any pin check, so a
/// fully-upstream board builds with the `patches` repo absent entirely.
///
/// Shared by [`clone_pinned`] (which clones/fetches first) and the userspace stage
/// (which fetches its own tree but applies its `userspace` scope the same way),
/// so the pin enforcement and verify-applies gate are one implementation.
/// The caller owns removing a partial tree on failure — [`clone_pinned`] and the
/// userspace stage both do (a resume must never reuse a half-patched tree).
pub(crate) fn apply_profile_scope(spec: &ApplyScope, step: &Step) -> Result<usize, EngineError> {
    let Some(patches) = spec.patches else {
        return Ok(0);
    };
    verify_patches_pin(patches.root, &patches.pin.commit, patches.dev, step)?;
    let profile = boot2deb_core::load_profile(patches.root, &patches.pin.profile)?;
    if let Some(reference) = spec.gate_reference {
        // Declared-intent gate before touching the tree.
        profile.ensure_applies(&patches.pin.profile, reference)?;
    }
    let series = spec.scope.series(&profile);
    patches::apply_tree(
        patches.root,
        series,
        spec.tree,
        spec.scope.tree_label(),
        spec.target,
    )
}

/// Enforce the patches-checkout pin: its HEAD must equal the lock's
/// `patches.commit` and its worktree must be clean, so the series read from it is
/// exactly the one the lock names. `dev` (an explicit `--patches-path` override for
/// co-developing the patch series) downgrades a mismatch to a loud warning instead
/// of an error, so a patch author can build against a working checkout.
fn verify_patches_pin(
    patches_root: &Path,
    expected: &str,
    dev: bool,
    step: &Step,
) -> Result<(), EngineError> {
    let head = git::rev_parse_head(patches_root)?;
    let clean = git::is_clean(patches_root)?;
    if head == expected && clean {
        return Ok(());
    }
    if dev {
        step.emit(
            Stream::Stderr,
            format!(
                "warning: patches checkout {} is at {}{} but the lock pins {} — \
                 applying the working tree's series (--patches-path override)",
                patches_root.display(),
                head,
                if clean { "" } else { " (with uncommitted changes)" },
                expected,
            ),
        );
        return Ok(());
    }
    Err(EngineError::PatchesPinMismatch {
        root: patches_root.display().to_string(),
        expected: expected.to_string(),
        // Ahead/behind selects the remedy: an ahead-of-pin or dirty checkout
        // needs `update` (its work is not in the lock yet), a stale one needs a
        // re-checkout at the pin.
        relation: git::pin_relation(patches_root, expected, &head),
        actual: head,
        dirty: !clean,
    })
}

/// Sanitize a raw upstream version into a Debian upstream-version-safe string:
/// keep alphanumerics and `. + ~ -`, replace anything else with `+` (underscore
/// is **not** legal in a Debian version), and guarantee the result starts with a
/// digit (Debian requires it) by prefixing `0` otherwise — which also covers the
/// empty input. Callers strip their own leading tag prefix first (ffmpeg's `n`,
/// u-boot's `v`). Pure, so version derivation is testable without a repo.
pub(crate) fn sanitize_deb_version(raw: &str) -> String {
    let mut out: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '+' | '~' | '-') {
                c
            } else {
                '+'
            }
        })
        .collect();
    if !out.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        out.insert(0, '0');
    }
    out
}

/// The restored path for `role` in an [`artstore`](crate::artstore) restore result,
/// or `None` if the entry did not carry that role (a foreign/older layout — the
/// caller then falls through to a rebuild rather than trusting a partial restore).
pub(crate) fn role_path(restored: &[(String, PathBuf)], role: &str) -> Option<PathBuf> {
    restored.iter().find(|(r, _)| r == role).map(|(_, p)| p.clone())
}

/// Tier-2 early exit shared by the compile stages: restore `node`'s stored
/// outputs into `out_dir` when the store holds signature `sig` with **every**
/// role in `roles`, returning the restored paths in `roles` order. A miss, a
/// partial entry (any role absent), or a disabled store (`None`) returns `None`
/// — the caller then builds and stores.
///
/// One implementation for every stage keeps the restore semantics provably
/// identical (all-roles-or-nothing, same log shape); per-stage copies of this
/// block are how stage behavior drifts.
pub(crate) fn restore_stage_outputs(
    store_root: Option<&Path>,
    node: &str,
    sig: &crate::signature::Signature,
    out_dir: &Path,
    roles: &[&str],
    step: &Step,
) -> Result<Option<Vec<PathBuf>>, EngineError> {
    let Some(root) = store_root else {
        return Ok(None);
    };
    let store = crate::artstore::ArtifactStore::open(root)?;
    let Some(files) = store.restore(node, sig.as_str(), out_dir)? else {
        return Ok(None);
    };
    let mut paths = Vec::with_capacity(roles.len());
    for role in roles {
        match role_path(&files, role) {
            Some(p) => paths.push(p),
            None => return Ok(None),
        }
    }
    step.log(format!(
        "restored {node} outputs from the artifact cache (signature {})",
        sig.short()
    ));
    Ok(Some(paths))
}

/// Tier-2 store side of [`restore_stage_outputs`]: put `node`'s built outputs
/// under signature `sig` so a later build restores instead of recompiling. A
/// disabled store (`None`) is a no-op.
pub(crate) fn store_stage_outputs(
    store_root: Option<&Path>,
    node: &str,
    sig: &crate::signature::Signature,
    files: &[(&str, &Path)],
    step: &Step,
) -> Result<(), EngineError> {
    let Some(root) = store_root else {
        return Ok(());
    };
    let store = crate::artstore::ArtifactStore::open(root)?;
    store.put(node, sig.as_str(), files)?;
    step.log(format!("stored {node} outputs to the artifact cache"));
    Ok(())
}

/// Tier-1 gate shared by the compile stages: keep the fetched+patched tree at
/// `tree` only when it is stamped with `man`'s signature; otherwise remove the
/// stale tree, run `refresh` to re-materialize it, and stamp it.
/// Returns `true` when the tree was reused — a stage whose configure step must
/// clean a previously-built tree keys on it.
///
/// A lock or patch bump changes the signature, so a stale tree is rebuilt
/// rather than silently reused; the compile steps re-run regardless, so the
/// signature covers only the tree-shaping fetch/patch inputs.
pub(crate) fn reuse_or_refresh_tree(
    tree: &Path,
    man: &crate::signature::SignatureManifest,
    what: &str,
    step: &Step,
    refresh: impl FnOnce() -> Result<(), EngineError>,
) -> Result<bool, EngineError> {
    if crate::signature::is_fresh(tree, man) {
        step.log(format!(
            "reusing {what} tree at {} (signature {})",
            tree.display(),
            man.signature().short()
        ));
        return Ok(true);
    }
    if tree.exists() {
        step.log(format!(
            "{what} tree at {} is stale (inputs changed) — rebuilding",
            tree.display()
        ));
        std::fs::remove_dir_all(tree).map_err(|s| EngineError::io(tree, s))?;
    }
    refresh()?;
    crate::signature::write_manifest(tree, man)?;
    Ok(false)
}

/// The lowercase-hex sha256 of a file's contents, for folding an in-repo input
/// (a kconfig fragment) directly into an output signature — git does not
/// pin these for us, so their bytes are hashed.
pub(crate) fn file_fingerprint(path: &Path) -> Result<String, EngineError> {
    let bytes = std::fs::read(path).map_err(|s| EngineError::io(path, s))?;
    Ok(crate::blobs::sha256_hex(&bytes))
}

/// How the applied patch series is identified in a Tier-1 tree signature.
///
/// In pinned mode `lock.patches.commit` content-addresses the whole `patches` repo,
/// so the folded commit alone identifies the exact series. In co-dev
/// (`--patches-path`) mode the pin is advisory — a mismatch only warns
/// (`verify_patches_pin`) — so the on-disk files, not the commit, are what get
/// applied; the ordered content fingerprint of the live series is folded instead so
/// an edited patch restamps the tree rather than restoring a stale one.
#[derive(Clone, Copy)]
pub enum PatchSeries<'a> {
    /// Pinned mode: the folded `patches.commit` is the series identity.
    Pinned,
    /// Co-dev mode: the ordered `label=sha256` fingerprint of the on-disk series
    /// (`patch_series_fingerprint`).
    Dev(&'a [String]),
}

/// The ordered content fingerprint of the patch series a `scope` applies from a live
/// `patches_root` checkout — for each patches-repo-relative label in the profile's
/// scope list, in order, `"<label>=<sha256 of its bytes>"`.
///
/// Folded into a Tier-1 tree signature only in co-dev mode ([`PatchSeries::Dev`]);
/// in pinned mode `lock.patches.commit` already content-addresses the series.
/// Best-effort by design: a profile that cannot be loaded yields an empty
/// fingerprint and an unreadable patch file folds a stable `<unreadable>` sentinel,
/// so computing a signature never fails here — a genuinely broken series fails loudly
/// at apply time ([`apply_profile_scope`]) instead, and no successful build could
/// have stamped a tree for it to falsely reuse.
pub(crate) fn patch_series_fingerprint(
    patches_root: &Path,
    profile: &str,
    scope: PatchScope,
) -> Vec<String> {
    let Ok(profile) = boot2deb_core::load_profile(patches_root, profile) else {
        return Vec::new();
    };
    scope
        .series(&profile)
        .iter()
        .map(|label| {
            let digest = std::fs::read(patches_root.join(label))
                .map(|bytes| crate::blobs::sha256_hex(&bytes))
                .unwrap_or_else(|_| "<unreadable>".to_string());
            format!("{label}={digest}")
        })
        .collect()
}

/// The co-dev content fingerprint of `scope`'s series, or empty when the build is in
/// pinned mode or applies no patches at all. Paired with [`series_identity`], which
/// borrows the result; the two are split so the `Vec` outlives the borrowing
/// [`PatchSeries`]. Every compile stage computes its series identity through this
/// pair, so "no patch profile" is handled once rather than per stage.
pub(crate) fn dev_series_fingerprint(patches: Option<PatchSource>, scope: PatchScope) -> Vec<String> {
    match patches {
        Some(p) if p.dev => patch_series_fingerprint(p.root, &p.pin.profile, scope),
        _ => Vec::new(),
    }
}

/// The [`PatchSeries`] a stage folds into its Tier-1 signature, given `fp` from
/// [`dev_series_fingerprint`]. A build with no patch source reports `Pinned`, which
/// [`fold_patch_series`] then ignores in favour of its `patches = "none"` scalar —
/// there is no series to be pinned or co-developed.
pub(crate) fn series_identity<'a>(patches: Option<PatchSource>, fp: &'a [String]) -> PatchSeries<'a> {
    if patches.is_some_and(|p| p.dev) {
        PatchSeries::Dev(fp)
    } else {
        PatchSeries::Pinned
    }
}

/// The ordered content fingerprint of a board's loose device-tree sources — for each
/// resolved `device_dts` path, in order, `"<basename>=<sha256 of its bytes>"` (§4).
///
/// Folded into the kernel's Tier-1 tree signature, because these files are copied into
/// the tree: editing the board `.dts` must restamp the tree so the next build re-copies
/// and recompiles rather than reusing a stale one. Only the basename is folded — that
/// is what lands in the kernel's DT dir, so moving a source within the config root
/// changes nothing about the resulting tree. Best-effort like the patch-series
/// fingerprint: an unreadable file folds a stable `<unreadable>`
/// sentinel so computing a signature never fails, and the copy then fails loudly at
/// [`kernel::build_kernel`] time.
pub fn device_dts_fingerprint(sources: &[PathBuf]) -> Vec<String> {
    sources
        .iter()
        .map(|path| {
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let digest = std::fs::read(path)
                .map(|bytes| crate::blobs::sha256_hex(&bytes))
                .unwrap_or_else(|_| "<unreadable>".to_string());
            format!("{name}={digest}")
        })
        .collect()
}

/// Fold the applied patch series' identity into a Tier-1 tree signature: the
/// profile name and pinned commit, then either the pinned marker or (co-dev) the
/// live-series fingerprint. Shared by every compile stage's `clone_manifest`
/// so the pinned-vs-co-dev discipline is one implementation. The pinned fold is byte-
/// identical to folding `patches_dev = "0"` alone, so a pinned tree signature is
/// unchanged by co-dev support — only co-dev builds gain the extra fingerprint.
///
/// A build with no patch profile (`pin` is `None`) folds a single `patches = "none"`
/// scalar: it has no profile, commit, or series to identify, and the distinct label
/// keeps its signature from ever colliding with a patched tree's.
pub(crate) fn fold_patch_series(
    b: &mut crate::signature::SignatureBuilder,
    pin: Option<&boot2deb_core::lock::PatchesPin>,
    patches: PatchSeries,
) {
    let Some(pin) = pin else {
        b.fold_scalar("patches", "none");
        return;
    };
    b.fold_scalar("patches.profile", &pin.profile);
    b.fold_scalar("patches.commit", &pin.commit);
    match patches {
        PatchSeries::Pinned => {
            b.fold_scalar("patches_dev", "0");
        }
        PatchSeries::Dev(fingerprint) => {
            b.fold_scalar("patches_dev", "1");
            b.fold_ordered("patch_series", fingerprint);
        }
    }
}

/// Copy `src` into `out_dir` (created if needed), returning the destination path.
/// Used to stage a built artifact out of a scratch tree.
///
/// The publish is atomic: the bytes copy into a sibling `.partial` temp on
/// the same filesystem, then a rename moves it over `dest`. An interrupted copy leaves
/// a `.partial` temp (swept by the cache/out_dir GC), never a truncated `.deb` at a
/// valid name — which would either overwrite a previously-staged good artifact the
/// ledger already trusts or, on a rootfs-only retry, be ingested as a half-written
/// package. Two runs staging the same name use pid-distinct temps.
fn stage_artifact(out_dir: &Path, src: &Path) -> Result<PathBuf, EngineError> {
    std::fs::create_dir_all(out_dir).map_err(|source| EngineError::io(out_dir, source))?;
    let file_name = src
        .file_name()
        .expect("artifact path has a file name");
    let dest = out_dir.join(file_name);
    let tmp = out_dir.join(format!(
        ".{}.{}.partial",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    std::fs::copy(src, &tmp).map_err(|source| {
        let _ = std::fs::remove_file(&tmp);
        EngineError::io(src, source)
    })?;
    std::fs::rename(&tmp, &dest).map_err(|source| {
        let _ = std::fs::remove_file(&tmp);
        EngineError::io(&dest, source)
    })?;
    Ok(dest)
}

/// Set the unix mode of a single staged file/dir, so the host umask does not leak into
/// a `.deb`'s packaged metadata. The rootfs stage forces the same discipline on
/// its generated config.
pub(crate) fn set_mode(path: &Path, mode: u32) -> Result<(), EngineError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|s| EngineError::io(path, s))
}

/// Normalize every mode in a staged package tree so the host umask does not leak into
/// the `.deb` payload: each directory to `0755`, each file to `0644`, symlinks
/// left alone. Valid **only for data-only packages** (no executables or maintainer
/// scripts) — the u-boot deb ships payload blobs plus config text, so a uniform data
/// mode is correct and makes the packaged tree byte-identical regardless of the build
/// host's umask (a package with executables would need per-file modes and must not use
/// this).
pub(crate) fn normalize_data_tree(root: &Path) -> Result<(), EngineError> {
    let meta = std::fs::symlink_metadata(root).map_err(|s| EngineError::io(root, s))?;
    if meta.file_type().is_symlink() {
        return Ok(());
    }
    if meta.is_dir() {
        set_mode(root, 0o755)?;
        let mut children: Vec<PathBuf> = std::fs::read_dir(root)
            .map_err(|s| EngineError::io(root, s))?
            .map(|e| e.map(|e| e.path()).map_err(|s| EngineError::io(root, s)))
            .collect::<Result<_, _>>()?;
        // Deterministic recursion order (cosmetic; modes are order-independent).
        children.sort();
        for child in children {
            normalize_data_tree(&child)?;
        }
    } else {
        set_mode(root, 0o644)?;
    }
    Ok(())
}

/// Pick the highest-versioned entry among `names` whose file name starts with
/// `prefix` and ends with `.deb`, by dpkg-style version ordering. Pure, so the
/// artifact selection is testable without a build.
fn pick_deb(names: &[String], prefix: &str) -> Option<String> {
    names
        .iter()
        .filter(|n| n.starts_with(prefix) && n.ends_with(".deb"))
        .max_by(|a, b| deb_version_cmp(a, b))
        .cloned()
}

/// Compare two `.deb` file names the way `sort -V` would for our purposes: split
/// into runs of digits and non-digits and compare digit runs numerically. Enough
/// to order `linux-image-…-9_…` after `…-10_…` correctly.
fn deb_version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (mut ai, mut bi) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (ai.peek().copied(), bi.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, Some(_)) => return Ordering::Less,
            (Some(_), None) => return Ordering::Greater,
            (Some(ca), Some(cb)) => {
                if ca.is_ascii_digit() && cb.is_ascii_digit() {
                    let na = take_number(&mut ai);
                    let nb = take_number(&mut bi);
                    match na.cmp(&nb) {
                        Ordering::Equal => continue,
                        ord => return ord,
                    }
                } else {
                    match ca.cmp(&cb) {
                        Ordering::Equal => {
                            ai.next();
                            bi.next();
                        }
                        ord => return ord,
                    }
                }
            }
        }
    }
}

/// Consume a leading run of ASCII digits as a `u64` (saturating on overflow).
fn take_number(it: &mut std::iter::Peekable<std::str::Chars>) -> u64 {
    let mut n: u64 = 0;
    while let Some(c) = it.peek().copied() {
        if let Some(d) = c.to_digit(10) {
            n = n.saturating_mul(10).saturating_add(d as u64);
            it.next();
        } else {
            break;
        }
    }
    n
}

/// `.deb` file names directly under `dir` (non-recursive), sorted, for artifact
/// selection with [`pick_deb`].
///
/// Sorted so the enumeration order does not depend on the filesystem's `read_dir`
/// order — the downstream selection ([`pick_deb`]) and dependency install order are
/// stable rather than host-dependent.
fn deb_names(dir: &Path) -> Result<Vec<String>, EngineError> {
    let mut names = Vec::new();
    let entries = std::fs::read_dir(dir).map_err(|source| EngineError::io(dir, source))?;
    for entry in entries.flatten() {
        if let Some(name) = entry.file_name().to_str() {
            if name.ends_with(".deb") {
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    Ok(names)
}

/// Remove every `.deb` under `dir` whose name starts with one of `prefixes`.
///
/// The stale-output sweep every stage runs over a directory it later *scans* for
/// `.deb`s: [`pick_deb`] picks the highest version and `select_debs` copies every
/// match among whatever is present, so a leftover from a build at different pins
/// (e.g. a higher-versioned kernel before a repin down) must be removed before
/// fresh outputs land, or it can be selected and shipped in place of them.
/// Prefix-scoped so one stage's sweep cannot touch another stage's
/// artifacts in a shared directory. An absent `dir` is a no-op.
pub(crate) fn purge_stage_debs(dir: &Path, prefixes: &[&str]) -> Result<(), EngineError> {
    if !dir.exists() {
        return Ok(());
    }
    for name in deb_names(dir)? {
        if prefixes.iter().any(|p| name.starts_with(p)) {
            let path = dir.join(&name);
            std::fs::remove_file(&path).map_err(|s| EngineError::io(&path, s))?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;
    use std::cell::RefCell;

    fn clone_failure(stderr: &str) -> EngineError {
        EngineError::CommandFailed {
            command: "git".into(),
            context: "clone".into(),
            status: Some(128),
            stderr: stderr.into(),
        }
    }

    #[test]
    fn transient_clone_errors_are_retried() {
        // The exact failures the flaky denx.de clone produced.
        assert!(is_transient_clone_error(&clone_failure(
            "error: RPC failed; HTTP 502 curl 22 The requested URL returned error: 502"
        )));
        assert!(is_transient_clone_error(&clone_failure(
            "fatal: expected 'acknowledgments'"
        )));
        // Other common transport hiccups.
        assert!(is_transient_clone_error(&clone_failure(
            "fatal: unable to access '…': Failed to connect to host port 443: Connection timed out"
        )));
        assert!(is_transient_clone_error(&clone_failure(
            "fatal: the remote end hung up unexpectedly"
        )));
    }

    #[test]
    fn permanent_clone_errors_fail_fast() {
        assert!(!is_transient_clone_error(&clone_failure(
            "fatal: Remote branch v9.9.9 not found in upstream origin"
        )));
        assert!(!is_transient_clone_error(&clone_failure(
            "fatal: Authentication failed for 'https://…'"
        )));
        // A spawn failure (git missing) is never transient.
        assert!(!is_transient_clone_error(&EngineError::CommandSpawn {
            command: "git".into(),
            context: "clone".into(),
            source: std::io::Error::from(std::io::ErrorKind::NotFound),
        }));
    }

    #[test]
    fn clone_shallow_clones_a_tagged_local_repo() {
        // Exercise the real clone subprocess (happy path) against a local repo,
        // no network. Set up a source with one commit tagged `v1`, then
        // clone_shallow it at that tag into a fresh dir.
        let base = std::env::temp_dir().join(format!("boot2deb-clone-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let src = base.join("src");
        let dst = base.join("dst");
        std::fs::create_dir_all(&src).unwrap();
        let git = |args: &[&str]| {
            let ok = Command::new("git")
                .current_dir(&src)
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?} failed");
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@boot2deb"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(src.join("f"), "hi").unwrap();
        git(&["add", "f"]);
        git(&["commit", "-qm", "c"]);
        git(&["tag", "v1"]);

        let log = RefCell::new(Vec::new());
        let sink = |e: Event| log.borrow_mut().push(e);
        let step = Step::start(&sink, "t");
        clone_shallow(src.to_str().unwrap(), "v1", &dst, &step).unwrap();

        assert_eq!(std::fs::read_to_string(dst.join("f")).unwrap(), "hi");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn fetch_commit_checks_out_the_exact_sha_via_the_fetch_by_commit_path() {
        // A local repo whose HEAD *is* the pinned commit: `try_fetch_commit` succeeds
        // (the sha is an advertised tip), so `fetch_commit` takes the
        // fetch-exact-commit path and must check it out detached — the path that
        // regressed when `--end-of-options` was wrongly passed to
        // `git checkout --detach` (git then treats the sha as a rejected pathspec).
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::create_dir_all(&src).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let ok = Command::new("git")
                .current_dir(dir)
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?} failed");
        };
        git(&src, &["init", "-q"]);
        git(&src, &["config", "user.email", "t@boot2deb"]);
        git(&src, &["config", "user.name", "t"]);
        std::fs::write(src.join("f"), "hi").unwrap();
        git(&src, &["add", "f"]);
        git(&src, &["commit", "-qm", "c"]);
        let sha = crate::git::rev_parse_head(&src).unwrap();

        let sink = |_: Event| {};
        let step = Step::start(&sink, "t");
        // reference == commit, as the userspace/ffmpeg pins are recorded in the lock.
        fetch_commit(src.to_str().unwrap(), &sha, &sha, "mpp", &dst, &step).unwrap();

        assert_eq!(crate::git::rev_parse_head(&dst).unwrap(), sha);
        assert_eq!(std::fs::read_to_string(dst.join("f")).unwrap(), "hi");
    }

    #[test]
    fn fetch_commit_reports_an_orphaned_pin_after_the_full_history_fallback() {
        // A pin that the source does not hold (its upstream branch was deleted, so
        // the commit is orphaned): the shallow fetch-by-sha fails, the full-history
        // fallback fetches every ref but still cannot reach it, and the reachability
        // probe turns that into a clear CommitUnreachable rather than a cryptic
        // "reference is not a tree" from checkout. Modelled with a real repo the
        // fetch reaches but that lacks the requested (bogus) commit.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::create_dir_all(&src).unwrap();
        let git = |dir: &Path, args: &[&str]| {
            let ok = Command::new("git")
                .current_dir(dir)
                .args(args)
                .output()
                .unwrap()
                .status
                .success();
            assert!(ok, "git {args:?} failed");
        };
        git(&src, &["init", "-q", "-b", "main"]);
        git(&src, &["config", "user.email", "t@boot2deb"]);
        git(&src, &["config", "user.name", "t"]);
        std::fs::write(src.join("f"), "hi").unwrap();
        git(&src, &["add", "f"]);
        git(&src, &["commit", "-qm", "c"]);

        // A well-formed SHA the source does not contain (an orphaned/never-present pin).
        let orphan = "0123456789abcdef0123456789abcdef01234567";
        let sink = |_: Event| {};
        let step = Step::start(&sink, "t");
        let err = fetch_commit(src.to_str().unwrap(), orphan, orphan, "mpp", &dst, &step)
            .unwrap_err();
        match err {
            EngineError::CommitUnreachable { what, commit, .. } => {
                assert_eq!(what, "mpp");
                assert_eq!(commit, orphan);
            }
            other => panic!("expected CommitUnreachable, got {other:?}"),
        }
        // The failed fetch leaves no half-populated tree a reuse check would trust.
        assert!(!dst.exists());
    }

    #[test]
    fn run_normalizes_timezone_and_locale() {
        // Every host-side command runs with a pinned TZ/LC_ALL so the host's does not
        // leak into packaged output.
        let log = RefCell::new(Vec::new());
        let sink = |e: Event| log.borrow_mut().push(e);
        let step = Step::start(&sink, "t");
        let mut cmd = Command::new("sh");
        // Deliberately set a bogus host value to prove run() overrides it.
        cmd.args(["-c", "printf 'TZ=%s LC_ALL=%s\\n' \"$TZ\" \"$LC_ALL\""])
            .env("TZ", "America/New_York");
        run(cmd, "sh", "env probe", &step).unwrap();
        let logged: String = log
            .borrow()
            .iter()
            .filter_map(|e| match e {
                Event::Log { line, .. } => Some(line.clone()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(logged.contains("TZ=UTC LC_ALL=C.UTF-8"), "got: {logged}");
    }

    #[test]
    fn run_streams_stdout_and_stderr_lines() {
        let log = RefCell::new(Vec::new());
        let sink = |e: Event| log.borrow_mut().push(e);
        let step = Step::start(&sink, "t");
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo out1; echo err1 >&2; echo out2"]);
        run(cmd, "sh", "echo test", &step).unwrap();

        let lines: Vec<(Stream, String)> = log
            .borrow()
            .iter()
            .filter_map(|e| match e {
                Event::Log { stream, line, .. } => Some((*stream, line.clone())),
                _ => None,
            })
            .collect();
        // All three lines arrive; stdout ordering is preserved among themselves.
        assert!(lines.contains(&(Stream::Stdout, "out1".into())));
        assert!(lines.contains(&(Stream::Stdout, "out2".into())));
        assert!(lines.contains(&(Stream::Stderr, "err1".into())));
        let stdout_only: Vec<_> = lines
            .iter()
            .filter(|(s, _)| *s == Stream::Stdout)
            .map(|(_, l)| l.clone())
            .collect();
        assert_eq!(stdout_only, vec!["out1", "out2"]);
    }

    #[test]
    fn run_maps_nonzero_exit_to_command_failed_with_stderr_tail() {
        let sink = |_: Event| {};
        let step = Step::start(&sink, "t");
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "echo boom >&2; exit 3"]);
        let err = run(cmd, "sh", "failing", &step).unwrap_err();
        match err {
            EngineError::CommandFailed { status, stderr, .. } => {
                assert_eq!(status, Some(3));
                assert!(stderr.contains("boom"));
            }
            other => panic!("expected CommandFailed, got {other:?}"),
        }
    }

    #[test]
    fn deb_names_returns_sorted_names() {
        let tmp = tempfile::tempdir().unwrap();
        for n in ["c.deb", "a.deb", "b.deb", "notes.txt"] {
            std::fs::write(tmp.path().join(n), b"x").unwrap();
        }
        // Sorted regardless of read_dir order; the non-.deb is excluded.
        assert_eq!(deb_names(tmp.path()).unwrap(), vec!["a.deb", "b.deb", "c.deb"]);
    }

    #[test]
    fn pick_deb_selects_highest_version() {
        let names = vec![
            "linux-image-7.1.1-1-arm64_7.1.1-9_arm64.deb".to_string(),
            "linux-image-7.1.1-1-arm64_7.1.1-10_arm64.deb".to_string(),
            "linux-headers-7.1.1-1-arm64_7.1.1-10_arm64.deb".to_string(),
            "some-unrelated.deb".to_string(),
        ];
        // -10 sorts after -9 numerically, not lexically.
        assert_eq!(
            pick_deb(&names, "linux-image-").as_deref(),
            Some("linux-image-7.1.1-1-arm64_7.1.1-10_arm64.deb")
        );
        assert_eq!(
            pick_deb(&names, "linux-headers-").as_deref(),
            Some("linux-headers-7.1.1-1-arm64_7.1.1-10_arm64.deb")
        );
        assert_eq!(pick_deb(&names, "nonexistent-"), None);
    }

    #[test]
    fn reuse_or_refresh_tree_reuses_stamped_and_rebuilds_stale() {
        let tmp = tempfile::tempdir().unwrap();
        let tree = tmp.path().join("tree");
        let sink = |_e: Event| {};
        let step = Step::start(&sink, "test");
        let man_for = |pin: &str| {
            let mut b = crate::signature::SignatureBuilder::new("t", 1);
            b.fold_scalar("pin", pin);
            b.manifest()
        };
        let ran = std::cell::Cell::new(0);

        // Absent tree: refresh runs and the tree is stamped; not a reuse.
        let man_v1 = man_for("v1");
        let reused = reuse_or_refresh_tree(&tree, &man_v1, "test", &step, || {
            ran.set(ran.get() + 1);
            std::fs::create_dir_all(&tree).map_err(|s| EngineError::io(&tree, s))?;
            std::fs::write(tree.join("f"), "v1").map_err(|s| EngineError::io(&tree, s))
        })
        .unwrap();
        assert!(!reused);
        assert_eq!(ran.get(), 1);

        // Unchanged signature: reused, refresh not called.
        let reused = reuse_or_refresh_tree(&tree, &man_v1, "test", &step, || {
            ran.set(ran.get() + 1);
            Ok(())
        })
        .unwrap();
        assert!(reused);
        assert_eq!(ran.get(), 1);

        // Pin bump: the stale tree is removed *before* refresh re-materializes it.
        let reused = reuse_or_refresh_tree(&tree, &man_for("v2"), "test", &step, || {
            ran.set(ran.get() + 1);
            assert!(!tree.exists(), "stale tree must be removed before refresh");
            std::fs::create_dir_all(&tree).map_err(|s| EngineError::io(&tree, s))
        })
        .unwrap();
        assert!(!reused);
        assert_eq!(ran.get(), 2);
    }

    #[test]
    fn stage_output_store_roundtrip_requires_every_role() {
        let tmp = tempfile::tempdir().unwrap();
        let store_root = tmp.path().join("store");
        let out = tmp.path().join("out");
        let sink = |_e: Event| {};
        let step = Step::start(&sink, "test");
        let sig = crate::signature::SignatureBuilder::new("t", 1).finish();
        let empty: &[(&str, &Path)] = &[];

        // Disabled store (None): storing is a no-op, restoring always misses.
        store_stage_outputs(None, "t", &sig, empty, &step).unwrap();
        assert!(restore_stage_outputs(None, "t", &sig, &out, &["a"], &step)
            .unwrap()
            .is_none());

        // Miss before anything is stored.
        assert!(
            restore_stage_outputs(Some(&store_root), "t", &sig, &out, &["a"], &step)
                .unwrap()
                .is_none()
        );

        // Roundtrip: paths come back in the caller's role order.
        let a = tmp.path().join("a.deb");
        let b = tmp.path().join("b.deb");
        std::fs::write(&a, b"a").unwrap();
        std::fs::write(&b, b"b").unwrap();
        store_stage_outputs(Some(&store_root), "t", &sig, &[("a", &a), ("b", &b)], &step).unwrap();
        let paths = restore_stage_outputs(Some(&store_root), "t", &sig, &out, &["b", "a"], &step)
            .unwrap()
            .expect("full hit");
        assert_eq!(std::fs::read(&paths[0]).unwrap(), b"b");
        assert_eq!(std::fs::read(&paths[1]).unwrap(), b"a");

        // An entry missing a requested role is a miss, never a partial restore.
        assert!(restore_stage_outputs(
            Some(&store_root),
            "t",
            &sig,
            &out,
            &["a", "missing"],
            &step
        )
        .unwrap()
        .is_none());
    }

    #[test]
    fn shallow_clone_accepts_an_uppercase_hex_pin() {
        // Both clone arms must accept the same pin spellings: the Fetch arm
        // normalizes the expected commit before comparing, so the Shallow arm's
        // check has to as well — an uppercase-hex hand edit names the same object.
        let tmp = tempfile::tempdir().unwrap();
        let origin = tmp.path().join("origin");
        std::fs::create_dir_all(&origin).unwrap();
        let git = |args: &[&str]| {
            let out = Command::new("git").arg("-C").arg(&origin).args(args).output().unwrap();
            assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        };
        git(&["init", "-q", "-b", "main"]);
        git(&["config", "user.email", "t@t"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(origin.join("f"), "x").unwrap();
        git(&["add", "-A"]);
        git(&["commit", "-q", "-m", "base"]);
        git(&["tag", "v1"]);
        let commit = git(&["rev-parse", "HEAD"]).to_uppercase();

        let tree = tmp.path().join("tree");
        let sink = |_e: Event| {};
        let step = Step::start(&sink, "test");
        let spec = ClonePinned {
            source: origin.to_str().unwrap(),
            reference: "v1",
            commit: &commit,
            mode: CloneMode::Shallow,
            tree: &tree,
            what: "test",
            patches: None,
            scope: PatchScope::Kernel,
            target: "test @ v1",
            gate_reference: None,
        };
        clone_pinned(&spec, &step).expect("uppercase pin must match the same object");
    }

    #[test]
    fn purge_stage_debs_removes_only_matching_prefixes() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        for n in [
            "linux-image-7.1.2-1-arm64_7.1.2-1_arm64.deb",
            "linux-headers-7.1.2-1-arm64_7.1.2-1_arm64.deb",
            "u-boot-turing-rk1_2026.04_arm64.deb",
            "notes.txt",
        ] {
            std::fs::write(dir.join(n), b"x").unwrap();
        }
        purge_stage_debs(dir, &["linux-image-", "linux-headers-"]).unwrap();
        // The swept prefixes are gone; another stage's deb and non-deb files stay.
        assert!(!dir.join("linux-image-7.1.2-1-arm64_7.1.2-1_arm64.deb").exists());
        assert!(!dir.join("linux-headers-7.1.2-1-arm64_7.1.2-1_arm64.deb").exists());
        assert!(dir.join("u-boot-turing-rk1_2026.04_arm64.deb").exists());
        assert!(dir.join("notes.txt").exists());
        // An absent dir is a no-op, not an error.
        purge_stage_debs(&dir.join("missing"), &["linux-image-"]).unwrap();
    }

    #[test]
    fn verify_patches_pin_enforces_head_and_cleanliness() {
        // A real local git repo, no network: commit once, then check the pin.
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
        std::fs::write(repo.join("f"), "hi").unwrap();
        git(&["add", "f"]);
        git(&["commit", "-qm", "c"]);
        let head = git::rev_parse_head(repo).unwrap();

        let sink = |_: Event| {};
        let step = Step::start(&sink, "t");

        // Matching pin on a clean tree: OK.
        verify_patches_pin(repo, &head, false, &step).unwrap();

        // A pin the checkout does not hold hard-errors, naming the expectation;
        // the relationship is undeterminable, so the remedy spells out both paths.
        let other = "0000000000000000000000000000000000000000";
        let err = verify_patches_pin(repo, other, false, &step).unwrap_err();
        match &err {
            EngineError::PatchesPinMismatch { expected, dirty, .. } => {
                assert_eq!(expected, other);
                assert!(!dirty);
            }
            e => panic!("expected PatchesPinMismatch, got {e:?}"),
        }
        let msg = err.to_string();
        assert!(msg.contains("boot2deb update"), "unknown-relation remedy offers update: {msg}");
        assert!(msg.contains(other), "unknown-relation remedy offers the pin: {msg}");
        // The --patches-path co-dev override downgrades the mismatch to a warning.
        verify_patches_pin(repo, other, true, &step).unwrap();

        // A checkout ahead of the pin (a commit past it) is told to re-pin with
        // `update`, not to re-checkout — that would discard the new commit.
        std::fs::write(repo.join("f"), "newer").unwrap();
        git(&["add", "f"]);
        git(&["commit", "-qm", "newer"]);
        let msg = verify_patches_pin(repo, &head, false, &step).unwrap_err().to_string();
        assert!(msg.contains("ahead of the pin"), "ahead remedy names the state: {msg}");
        assert!(msg.contains("boot2deb update"), "ahead remedy points at update: {msg}");
        let newer_head = git::rev_parse_head(repo).unwrap();

        // A stale checkout (HEAD behind the pin) is told to check out the pin.
        git(&["checkout", "-q", head.as_str()]);
        let msg = verify_patches_pin(repo, &newer_head, false, &step).unwrap_err().to_string();
        assert!(msg.contains("behind the pin"), "behind remedy names the state: {msg}");
        assert!(msg.contains(&format!("checkout {newer_head}")), "behind remedy gives the command: {msg}");
        git(&["checkout", "-q", "-"]);

        // An uncommitted change fails the clean check even at the right commit,
        // and the remedy leads with committing, whatever HEAD's relation is.
        std::fs::write(repo.join("f"), "changed").unwrap();
        let err = verify_patches_pin(repo, &newer_head, false, &step).unwrap_err();
        assert!(
            matches!(err, EngineError::PatchesPinMismatch { dirty: true, .. }),
            "expected dirty PatchesPinMismatch, got {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("commit them"), "dirty remedy leads with commit: {msg}");
        // ...but the override tolerates a dirty co-dev checkout too.
        verify_patches_pin(repo, &newer_head, true, &step).unwrap();
    }

    #[test]
    fn probe_object_distinguishes_present_absent_and_errored() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            Command::new("git").arg("-C").arg(&repo).args(args).output().unwrap()
        };
        if !git(&["init", "-q"]).status.success() {
            eprintln!("skipping probe_object test: git unavailable");
            return;
        }
        git(&["config", "user.email", "t@boot2deb"]);
        git(&["config", "user.name", "t"]);
        std::fs::write(repo.join("f"), "hi").unwrap();
        git(&["add", "f"]);
        git(&["commit", "-qm", "c"]);
        let head = git::rev_parse_head(&repo).unwrap();

        // The real HEAD commit is present.
        assert!(matches!(probe_object(&repo, &head), ObjectProbe::Present));
        // A well-formed but nonexistent sha is a clean absence, not an error —
        // this is what drives CommitUnreachable/Orphaned.
        let absent = "0123456789abcdef0123456789abcdef01234567";
        assert!(matches!(probe_object(&repo, absent), ObjectProbe::Absent));
        // A dir that is not a git repo is an errored probe, carrying git's message —
        // never misreported as an absence.
        let notrepo = tmp.path().join("notrepo");
        std::fs::create_dir(&notrepo).unwrap();
        match probe_object(&notrepo, &head) {
            ObjectProbe::Errored(detail) => assert!(!detail.is_empty()),
            other => panic!("expected Errored for a non-repo, got {other:?}"),
        }
    }

    #[test]
    fn reject_optionlike_guards_git_positionals() {
        // A benign URL/path/ref passes.
        assert!(reject_optionlike("source", "https://git.denx.de/u-boot.git").is_ok());
        assert!(reject_optionlike("source", "../linux").is_ok());
        assert!(reject_optionlike("ref", "v7.1.1").is_ok());
        // An option-looking source/ref is refused (the --upload-pack RCE vector).
        assert!(matches!(
            reject_optionlike("source", "--upload-pack=touch /tmp/pwn"),
            Err(EngineError::UnsafeGitArgument { .. })
        ));
        assert!(matches!(
            reject_optionlike("ref", "-o"),
            Err(EngineError::UnsafeGitArgument { .. })
        ));
    }

    #[test]
    fn reject_unsafe_make_target_guards_defconfig() {
        // Real defconfig targets are bare identifiers.
        assert!(reject_unsafe_make_target("uboot_defconfig", "turing-rk1-rk3588_defconfig").is_ok());
        assert!(reject_unsafe_make_target("make target", "olddefconfig").is_ok());
        // A leading dash would be read as a make option.
        assert!(matches!(
            reject_unsafe_make_target("make target", "-j99"),
            Err(EngineError::UnsafeMakeTarget { .. })
        ));
        // An `=` would be read as a variable assignment (CC=<cmd> tool injection).
        assert!(matches!(
            reject_unsafe_make_target("base_defconfig", "CC=/tmp/evil"),
            Err(EngineError::UnsafeMakeTarget { .. })
        ));
    }

    #[test]
    fn deb_version_cmp_orders_numeric_runs() {
        use std::cmp::Ordering;
        assert_eq!(deb_version_cmp("a-9-b", "a-10-b"), Ordering::Less);
        assert_eq!(deb_version_cmp("a-2-b", "a-2-b"), Ordering::Equal);
        assert_eq!(deb_version_cmp("b", "a"), Ordering::Greater);
    }

    #[test]
    fn normalize_data_tree_forces_0755_dirs_and_0644_files() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("pkg");
        std::fs::create_dir_all(root.join("usr/lib")).unwrap();
        std::fs::write(root.join("usr/lib/blob.img"), b"payload").unwrap();
        std::fs::write(root.join("usr/lib/install.conf"), b"conf").unwrap();
        // Odd starting modes stand in for a permissive host umask.
        set_mode(&root.join("usr"), 0o777).unwrap();
        set_mode(&root.join("usr/lib/blob.img"), 0o600).unwrap();

        normalize_data_tree(&root).unwrap();

        let mode = |p: &str| {
            std::fs::metadata(root.join(p)).unwrap().permissions().mode() & 0o777
        };
        assert_eq!(mode("usr"), 0o755);
        assert_eq!(mode("usr/lib"), 0o755);
        assert_eq!(mode("usr/lib/blob.img"), 0o644);
        assert_eq!(mode("usr/lib/install.conf"), 0o644);
    }

    #[test]
    fn stage_artifact_publishes_atomically_without_leftover_temp() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("linux-image-1_arm64.deb");
        std::fs::write(&src, b"deb-bytes").unwrap();
        let out_dir = tmp.path().join("artifacts");

        let dest = stage_artifact(&out_dir, &src).unwrap();
        assert_eq!(dest, out_dir.join("linux-image-1_arm64.deb"));
        assert_eq!(std::fs::read(&dest).unwrap(), b"deb-bytes");
        // No `.partial` temp survives a successful publish.
        let strays: Vec<_> = std::fs::read_dir(&out_dir)
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().contains(".partial"))
            .collect();
        assert!(strays.is_empty(), "stage left a temp behind: {strays:?}");
    }
}
