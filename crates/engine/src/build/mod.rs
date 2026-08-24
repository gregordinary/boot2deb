//! The compile stages of the build graph — `git`/`make` steps wrapped as
//! engine subprocess stages that read the resolved [`Lock`] and emit the
//! structured [`Event`](crate::event::Event) stream.
//!
//! These stages build the device's kernel, bootloader, out-of-tree modules, and media
//! stack: [`kernel`] (`git am` series + `make bindeb-pkg`), [`uboot`], [`kmod`], the
//! [`userspace`] MPP/RGA `.deb`s, and the [`ffmpeg`] `ffmpeg-rk` `.deb`. These stages
//! drive the compile invocations directly rather than reimplementing them: the value
//! here is the typed orchestration, the lock-driven pins, and the event stream, not a
//! new build system.
//!
//! **No `.deb` is archived on the host.** The userspace and ffmpeg stages compile *and*
//! package inside a target-arch [`BuildSandbox`](crate::sandbox::BuildSandbox); the
//! u-boot and kmod stages stage their trees on the host — layout, control text and mode
//! normalization are pure and testable there — and archive them through one
//! `dpkg-deb --build` in the host-arch [`PackagingSandbox`]. Only the kernel is still
//! packaged host-side, by its own `make bindeb-pkg`.
//!
//! [`Lock`]: boot2deb_core::lock::Lock

mod elf;
pub mod ffmpeg;
pub mod kernel;
pub mod kmod;
pub(crate) mod probe;
pub mod rkboot;
pub mod uboot;
pub mod userspace;

use crate::error::EngineError;
use crate::event::{Step, Stream};
use crate::sandbox::{CompileRoot, PackagingSandbox, SandboxRole, SandboxRun};
use crate::{git, patches};
use boot2deb_core::lock::Lock;
use boot2deb_core::PatchSeries;
use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc::{self, Sender};

/// How many trailing stderr lines to keep for a [`EngineError::CommandFailed`]
/// message. The full output already reached the caller as [`Event::Log`] events
/// ([`Event`](crate::event::Event)); this is just a self-contained error summary.
pub(crate) const STDERR_TAIL: usize = 40;

/// Host/target build parameters shared by the compile stages.
#[derive(Debug, Clone, Default)]
pub struct BuildEnv {
    /// `CROSS_COMPILE` prefix, `Some` when the host arch differs from the target;
    /// `None` for a native build (no prefix passed to `make`).
    pub cross_compile: Option<String>,
    /// Parallelism cap for the whole build; `None` lets each stage default to the
    /// host's available parallelism.
    ///
    /// It bounds every concurrent thing the build does — `make -j`, the
    /// `DEB_BUILD_OPTIONS=parallel=` a `dpkg-buildpackage` sees, and the image's `.xz`
    /// worker pool — because the flag means "for this build", not "for `make`". On a
    /// shared or constrained machine an unbounded compression pass is as unwelcome as
    /// an unbounded compile.
    ///
    /// Deliberately **not** in any signature. That is a separate question from
    /// bounding concurrency: a build system whose *output* depends on its job count is
    /// a build system with a bug, so folding it would fragment the artifact cache by
    /// machine size to key something that is supposed to be invariant. It is recorded
    /// in the provenance manifest instead, where a difference between two images from
    /// one lock can be seen without being paid for on every cache lookup.
    pub jobs: Option<usize>,
    /// Identity of the **cross root** that compiles the kernel, u-boot and the
    /// out-of-tree modules ([`cross_identity`]), folded into those three stages' Tier-2
    /// output signatures so an artifact built with one compiler is not restored for a
    /// build using another.
    ///
    /// The compiler is a package of that root rather than a host binary, so this is the
    /// root's own identity — its architecture, the target it emits for, its suite and
    /// its mirror list, as the name of its tree. Nothing about the build host is in it,
    /// which is the point: two hosts resolving one lock resolve one compiler.
    pub toolchain_id: String,
    /// Identity of everything host-side that shapes a **sandbox-built** `.deb` — the
    /// userspace and ffmpeg nodes — folded into their output signatures.
    ///
    /// Two inputs, because two things outside the source pins decide those bytes:
    ///
    ///  - The **base the sandbox is provisioned as** — its architecture, suite, ordered
    ///    mirror list and package set, taken as the name of its own tree. The sandbox's
    ///    `gcc` is the compiler for these packages, and the suite alone does not identify
    ///    it: a testing suite's toolchain moves under a fixed suite name. What pins it is
    ///    the mirror, so under `--snapshot pin` two builds at different snapshots key
    ///    differently. Against a *live* mirror the compiler can still move under an
    ///    unchanged key; that residual is what the snapshot exists to close, and it
    ///    cannot be closed from here.
    ///
    ///    Empty for a build that stands up no sandbox at all, which is every build that
    ///    resolves no suite. Such a build runs neither node, so the field is unread
    ///    rather than defaulted — and an empty string is no tree's name, so it cannot
    ///    collide with one.
    ///  - The **`qemu-user` interpreter**, where the host cannot execute the target's
    ///    binaries, which is what then executes that compiler
    ///    ([`qemu_identity`](crate::toolchain::HostToolchain::qemu_identity)). A host
    ///    that runs them directly folds no interpreter segment at all rather than an
    ///    empty one, so it can never key alike with an emulated build whose qemu is
    ///    merely missing.
    ///
    ///    "Cannot execute" is narrower than "cross": an arm64 host building armhf
    ///    compiles through a cross toolchain and then runs the result natively, so it
    ///    folds no interpreter. Keying on the toolchain question instead would name a
    ///    binary that never ran.
    pub sandbox_id: String,
    /// Identity of the [`PackagingSandbox`] that archives the u-boot and kmod `.deb`s
    /// ([`packaging_identity`]), folded into their output signatures.
    ///
    /// `dpkg-deb`'s version and its `liblzma` shape the archive bytes, and both are
    /// packages of that root — so what it resolved to is an input to the output, in the
    /// way [`sandbox_id`](Self::sandbox_id) is for a compiled `.deb`. Two roots with the
    /// same identity hold the same `dpkg`, so their archives are interchangeable and
    /// the cache may serve one for the other; two that differ may not.
    ///
    /// Distinct from `sandbox_id` rather than folded into it because the two roots move
    /// independently: they carry different package sets, at different architectures,
    /// and the packaging root's suite is not always the image's.
    pub packaging_id: String,
}

/// Compose a [`BuildEnv::sandbox_id`] from the build sandbox's architecture, suite and
/// ordered mirror list, plus the interpreter that executes what it compiles.
///
/// The first segment is the tree name the sandbox is provisioned under
/// ([`build_sandbox_dir`](crate::sandbox::build_sandbox_dir)), which is the identity of
/// the base: its digest covers the mirror list, the base package set and the base recipe
/// version, and its prefix carries the arch and suite. Deriving the signature input from
/// the function that *names the directory* is what keeps the two from disagreeing — a
/// build keyed on one claim while compiling in a differently-provisioned tree is exactly
/// the failure that digest exists to prevent. A package added to or removed from the base
/// changes what `./configure` detects, so it has to change the key as well as the path.
///
/// The interpreter segment follows where the host cannot execute the target's binaries,
/// because it then runs every compiler invocation. A build that runs them directly folds
/// no segment at all — not an empty one — so it can never key alike with an emulated
/// build whose `qemu-user` is merely missing. [`packaging_identity`] has no counterpart
/// for it: that root is host-arch, so nothing is ever interpreted there.
///
/// Lives here, beside the field it fills, rather than at the call sites: it decides when
/// two sandbox-built `.deb`s may be restored for each other, and that is a property of
/// the signature, not of the CLI.
pub fn sandbox_identity(
    arch: &str,
    suite: &str,
    mirrors: &[String],
    toolchain: &crate::toolchain::HostToolchain,
) -> String {
    let base = root_identity(crate::sandbox::build_sandbox_dir(
        Path::new(""),
        SandboxRole::Target,
        arch,
        suite,
        mirrors,
    ));
    match toolchain.qemu_identity() {
        Some(qemu) => format!("{base} | {qemu}"),
        None => base,
    }
}

/// Compose a [`BuildEnv::toolchain_id`] from the cross sandbox's own architecture, the
/// `target` its toolchain emits for, the suite and the ordered mirror list.
///
/// The tree name the cross root is provisioned under
/// ([`build_sandbox_dir`](crate::sandbox::build_sandbox_dir) at
/// [`SandboxRole::Cross`]), for the reason the other two identities are their trees'
/// names: the compiler that produces the kernel, u-boot and module bytes *is* a package
/// of that root, so what the root resolved to is what shapes the output, and deriving
/// the key from the function that names the directory is what stops a claim and a tree
/// from disagreeing.
///
/// `target` reaches the name through the toolchain package
/// (`crossbuild-essential-<target>`), so two targets never share a key any more than
/// they share a root.
///
/// No interpreter segment, for the same reason [`packaging_identity`] has none: a cross
/// root is host-arch, so `qemu-user` is never in the path. That it is *derived* rather
/// than probed is what lets `why-rebuild` ask the artifact store this question offline,
/// with no root provisioned and none to provision — and what backs it with a
/// sha256-pinned package manifest instead of a `--version` line.
pub fn cross_identity(arch: &str, target: &'static str, suite: &str, mirrors: &[String]) -> String {
    root_identity(crate::sandbox::build_sandbox_dir(
        Path::new(""),
        SandboxRole::Cross { target },
        arch,
        suite,
        mirrors,
    ))
}

/// The identity of a provisioned root: the leaf name of the directory it lives in.
///
/// The work dir is where a tree *lives*, not part of what it *is*, so only the leaf is
/// taken — an empty stand-in work dir gives the same answer every real one would, which
/// is what makes two machines building one lock key alike.
fn root_identity(dir: PathBuf) -> String {
    dir.file_name()
        .expect("the tree name is the path's last component")
        .to_string_lossy()
        .into_owned()
}

/// Compose a [`BuildEnv::packaging_id`] from the packaging root's architecture, suite
/// and ordered mirror list.
///
/// The tree name the root is provisioned under
/// ([`packaging_root_dir`](crate::sandbox::packaging_root_dir)), which is the identity
/// of the base: its digest already covers the mirrors, the package set and the recipe
/// version, and its prefix carries the arch and suite. Deriving the signature input from
/// the same function that names the directory is what keeps the two from disagreeing —
/// a claim keyed on one while the tree is provisioned under the other is exactly the
/// failure the tree name's own digest exists to prevent.
///
/// No interpreter segment, unlike [`sandbox_identity`]: the packaging root is host-arch,
/// so `qemu-user` is never in the path.
///
/// Takes the pieces rather than a [`PackagingSandbox`], because `why-rebuild` asks the
/// artifact store this question offline, with no root provisioned and none to provision.
pub fn packaging_identity(arch: &str, suite: &str, mirrors: &[String]) -> String {
    root_identity(crate::sandbox::packaging_root_dir(
        Path::new(""),
        arch,
        suite,
        mirrors,
    ))
}

impl BuildEnv {
    /// Resolved job count: the configured value or the host's available
    /// parallelism (falling back to 1). Public because the provenance manifest
    /// records the number the build actually ran at, which is only knowable here
    /// when [`jobs`](Self::jobs) is `None`.
    pub fn jobs(&self) -> usize {
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

/// Run one command inside a provisioned build root, with `work` as its working directory
/// and `binds` the host paths it must see.
///
/// The compile counterpart of [`run`], and the choke point every kernel, u-boot and
/// out-of-tree-module invocation goes through. It exists as a named function rather than
/// as three hand-built [`SandboxRun`]s so those three stages cannot drift in what a
/// compile is: the same root, the same cage profile, the same isolated network, and the
/// same rule that a host path is visible inside at its own absolute path — which is what
/// lets `make` write its output back beside the source tree on the host.
///
/// Nothing normalizes the environment here, unlike [`run`], because there is nothing to
/// normalize: the cage composes the command's environment from
/// [`SANDBOX_ENV`](crate::sandbox) alone, so a variable exported in the build user's
/// shell cannot reach a compile in the first place. `env` is exactly what the stage
/// declares.
pub(crate) fn run_in_root(
    cr: &CompileRoot,
    work: &Path,
    argv: &[String],
    env: &[(String, String)],
    context: &str,
    step: &Step,
) -> Result<(), EngineError> {
    cr.root.run(
        &SandboxRun {
            work,
            binds: cr.binds,
            env,
            argv,
            context,
            probe: None,
        },
        step,
    )
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
/// This is the single host-side command choke point, and what still comes through it is
/// the *git* work — the clone and the `git am` of a patch series — plus `tar` on the
/// image path. Every compile runs in a provisioned root instead and does not pass
/// through here at all. It normalizes the determinism-relevant environment —
/// `TZ=UTC` and `LC_ALL=C.UTF-8`, matching the sandbox's built-from-scratch
/// `SANDBOX_ENV` discipline so a host's timezone/locale cannot leak into packaged output,
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
    for flag_var in [
        "KCFLAGS",
        "KAFLAGS",
        "KCPPFLAGS",
        "MAKEFLAGS",
        "GNUMAKEFLAGS",
    ] {
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
            step.relay(stream, line);
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
/// [`git::command`](crate::git::command) before the subcommand args, since `-c` config
/// is only honored ahead of the subcommand. Set both as `-c` config (git's own
/// transport) and via `GIT_HTTP_LOW_SPEED_*` env (read by the `git-remote-https`
/// helper). Local git ops (init/checkout/rev-parse/cat-file) touch no remote and are
/// left unbounded.
pub(crate) fn bound_git_network(cmd: &mut Command) {
    cmd.args([
        "-c",
        &format!("http.lowSpeedLimit={GIT_STALL_BYTES_PER_SEC}"),
    ])
    .args(["-c", &format!("http.lowSpeedTime={GIT_STALL_SECS}")])
    .env("GIT_HTTP_LOW_SPEED_LIMIT", GIT_STALL_BYTES_PER_SEC)
    .env("GIT_HTTP_LOW_SPEED_TIME", GIT_STALL_SECS);
}

/// Total clone attempts before a transient failure is fatal (initial try + retries).
const CLONE_ATTEMPTS: u32 = 4;

/// Shallow-clone `source` at `reference` into `tree`, retrying transient failures.
///
/// Git hosts flake — a shallow clone can die mid-transfer on an HTTP 5xx, an RPC
/// desync, or a dropped connection. A *transient* failure is retried (up to a small
/// fixed attempt count) with an increasing backoff; a *non-transient* one (an unknown
/// ref, auth failure, a missing `git`) fails immediately without wasting retries.
/// Because a failed clone leaves a partial checkout that would make the next
/// `git clone` refuse a non-empty target, the partial `tree` is removed between
/// attempts — safe because callers only clone into a fresh path (an existing tree is
/// reused, not re-cloned). On the final failure the underlying [`EngineError`] is
/// returned unchanged, so the real cause is still surfaced.
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
        let mut clone = crate::git::command(None);
        bound_git_network(&mut clone);
        clone
            // `--end-of-options` stops a `source`/`tree` beginning with `-` from
            // being read as a flag; the value guards above reject the same up front.
            .args([
                "clone",
                "--depth",
                "1",
                "--branch",
                reference,
                "--end-of-options",
            ])
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
        "http 5",            // 500/502/503/504 from the git host
        "returned error: 5", // curl's rendering of an HTTP 5xx
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
        "expected 'acknowledgments'", // truncated protocol-v2 response
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
    let mut init = crate::git::command(None);
    init.arg("-C").arg(dir).args(["init", "-q"]);
    run(init, "git", &format!("init {}", dir.display()), step)?;

    // Fetch the *exact* locked commit first: a shallow fetch of the reference only
    // gets its current tip, so once upstream moves past the pin the ref no longer
    // reaches the locked commit. A shallow fetch-by-sha works for a local
    // source, an advertised ref tip, or a server honoring SHA1-in-want.
    if try_fetch_commit(dir, &resolved, commit) {
        let mut checkout = crate::git::command(None);
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
        let mut fetch = crate::git::command(None);
        bound_git_network(&mut fetch);
        // `--end-of-options` keeps the source/refspec positionals from being read as
        // flags (defence in depth over the value guards above).
        fetch
            .arg("-C")
            .arg(dir)
            .args(["fetch", "--tags", "--end-of-options"])
            .arg(&resolved)
            .arg("+refs/heads/*:refs/remotes/origin/*");
        run(
            fetch,
            "git",
            &format!("fetch (full history) {resolved}"),
            step,
        )?;
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
        let mut checkout = crate::git::command(None);
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
    match crate::git::command(None)
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
        Err(e) => ObjectProbe::Errored(format!(
            "could not run git cat-file in {}: {e}",
            dir.display()
        )),
    }
}

/// Attempt a shallow fetch of the exact `commit` from `source`; `true` on success.
/// Quiet by design — a failure is an expected fallback (a server may forbid
/// fetch-by-sha), not an error to stream, so the reference path can take over.
fn try_fetch_commit(dir: &Path, source: &str, commit: &str) -> bool {
    let mut cmd = crate::git::command(None);
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
pub(crate) fn reject_unsafe_make_target(
    what: &'static str,
    value: &str,
) -> Result<(), EngineError> {
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

/// Which of a [`PatchSeries`]'s per-tree series is applied — by [`clone_pinned`]
/// (kernel/u-boot/ffmpeg) or by the userspace stage via [`apply_series_scope`].
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
    /// The core [`Scope`](boot2deb_core::series::Scope) this maps onto.
    fn core_scope(&self) -> boot2deb_core::series::Scope {
        use boot2deb_core::series::Scope;
        match self {
            PatchScope::Kernel => Scope::Kernel,
            PatchScope::Uboot => Scope::Uboot,
            PatchScope::Ffmpeg => Scope::Ffmpeg,
            PatchScope::Userspace => Scope::Userspace,
        }
    }

    /// The series' ordered patch list for this scope, narrowed to the entries
    /// `kernel_version` selects (see
    /// [`series_for`](boot2deb_core::PatchSeries::series_for)).
    ///
    /// Release-strict: a build resolves an actual kernel tag, and an `-rc` must not
    /// silently satisfy an envelope that claims released kernels only.
    fn series<'a>(
        &self,
        series: &'a PatchSeries,
        name: &str,
        version: &str,
    ) -> Result<Vec<&'a str>, EngineError> {
        Ok(series.series_for(
            self.core_scope(),
            name,
            version,
            boot2deb_core::RangeMatch::Release,
        )?)
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
    /// patch series — the tree is then compiled exactly as cloned.
    pub patches: Option<PatchSource<'a>>,
    /// Which per-tree series to apply.
    pub scope: PatchScope,
    /// Message label for the patched tree (e.g. `"kernel @ v7.1.1"`).
    pub target: &'a str,
    /// When `Some`, gate the series' declared envelope against this ref before
    /// applying — the declared-intent gate. The ref belongs to the *scope's own*
    /// axis: the kernel tag for the kernel-family scopes, the u-boot tag for
    /// [`PatchScope::Uboot`], since a u-boot series makes no claim about a kernel.
    pub gate_reference: Option<&'a str>,
}

/// A resolved `patches` checkout together with the pin and series it supplies.
///
/// Bundled rather than carried as four loose fields so that "this build applies no
/// patches" is one `Option::None` the compiler enforces: there is no way to name a
/// series without a checkout to read it from, nor to resolve a checkout for a build
/// that has no series.
#[derive(Clone, Copy)]
pub struct PatchSource<'a> {
    /// The `patches` checkout the series is read from.
    pub root: &'a Path,
    /// The lock's pin: which series, at which `patches`-repo commit. Borrowed from
    /// the lock rather than copied field-by-field, so the same value feeds both the
    /// apply step and the signature fold.
    pub pin: &'a boot2deb_core::lock::PatchesPin,
    /// The checkout was chosen explicitly via `--patches-path` for co-development:
    /// a pin mismatch is a loud warning rather than an error.
    pub dev: bool,
    /// The version the series' per-entry ranges are filtered against for this
    /// scope: the resolved **kernel** version for the kernel/ffmpeg/userspace scopes,
    /// the resolved **u-boot** version for the u-boot scope (u-boot is its own axis, so
    /// a u-boot-only build has no kernel version to narrow by). The caller supplies the
    /// one that matches the scope it is applying.
    pub version: &'a str,
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
            fetch_commit(
                spec.source,
                spec.reference,
                spec.commit,
                spec.what,
                spec.tree,
                step,
            )?;
        }
    }
    apply_series_scope(
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

/// The inputs for [`apply_series_scope`]: an already-checked-out `tree` plus the
/// patches checkout, pin, and which series scope to apply.
pub(crate) struct ApplyScope<'a> {
    /// The source tree to apply the series onto, in place. The caller has already
    /// checked it out at the locked commit and must have it clean.
    pub tree: &'a Path,
    /// The series to apply, or `None` when the build's kernel names no patch series.
    pub patches: Option<PatchSource<'a>>,
    /// Which per-tree series to apply.
    pub scope: PatchScope,
    /// Message label for the patched tree (e.g. `"kernel @ v7.1.1"`).
    pub target: &'a str,
    /// When `Some`, gate the series' declared envelope against this ref before
    /// applying — the declared-intent gate. The ref belongs to the *scope's own*
    /// axis: the kernel tag for the kernel-family scopes, the u-boot tag for
    /// [`PatchScope::Uboot`], since a u-boot series makes no claim about a kernel.
    pub gate_reference: Option<&'a str>,
}

/// Enforce the patches-checkout pin, load the series, optionally gate its
/// declared kernel range, and apply the series' `scope` series onto an
/// already-checked-out `tree` in place — leaving the fully-patched source the build
/// compiles. Returns the number of patches applied.
///
/// A build whose kernel names no patch series (`spec.patches` is `None`) applies
/// nothing and reads no `patches` checkout: it returns `0` before any pin check, so a
/// fully-upstream board builds with the `patches` repo absent entirely.
///
/// Shared by [`clone_pinned`] (which clones/fetches first) and the userspace stage
/// (which fetches its own tree but applies its `userspace` scope the same way),
/// so the pin enforcement and verify-applies gate are one implementation.
/// The caller owns removing a partial tree on failure — [`clone_pinned`] and the
/// userspace stage both do (a resume must never reuse a half-patched tree).
pub(crate) fn apply_series_scope(spec: &ApplyScope, step: &Step) -> Result<usize, EngineError> {
    let Some(patches) = spec.patches else {
        return Ok(0);
    };
    verify_patches_pin(patches.root, &patches.pin.commit, patches.dev, step)?;
    // Load every named series up front so each one's borrowed patch list outlives the
    // concatenation below, then apply them in the order the kernel names them: series
    // A's scope list, then B's. All come from the one pinned checkout, so a single pin
    // check and one ordered apply pass cover the composed set.
    let loaded: Vec<(&str, PatchSeries)> = patches
        .pin
        .series
        .iter()
        .map(|name| {
            Ok((
                name.as_str(),
                boot2deb_core::load_series(patches.root, name)?,
            ))
        })
        .collect::<Result<_, EngineError>>()?;
    if let Some(reference) = spec.gate_reference {
        // Declared-intent gate before touching the tree, against the envelope for this
        // scope: the u-boot scope gates on the u-boot version, the rest on the kernel.
        // Every composed series is gated; one that does not cover this version fails here.
        for &(name, ref series) in &loaded {
            match spec.scope {
                PatchScope::Uboot => series.ensure_applies_uboot(name, reference)?,
                _ => series.ensure_applies(name, reference)?,
            }
        }
    }
    let mut labels: Vec<&str> = Vec::new();
    for &(name, ref series) in &loaded {
        labels.extend(spec.scope.series(series, name, patches.version)?);
    }
    patches::apply_tree(
        patches.root,
        &labels,
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
            crate::event::LogOrigin::Stage,
            format!(
                "warning: {} — applying the working tree's series (--patches-path override)",
                crate::pins::describe_patches_drift(patches_root, &head, expected, clean),
            ),
        );
        return Ok(());
    }
    // A checkout on the pin with uncommitted work is not a drifted pin, and saying
    // "is at X, but the lock pins X" would read as a contradiction.
    if head == expected {
        return Err(EngineError::PatchesWorktreeDirty {
            root: patches_root.display().to_string(),
            commit: head,
        });
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
    restored
        .iter()
        .find(|(r, _)| r == role)
        .map(|(_, p)| p.clone())
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
    // This node came back whole, so the step did not compile it: recorded here rather
    // than at each caller, which is what keeps the claim identical across the stages.
    // Its counterpart in [`store_stage_outputs`] records the other half.
    step.restored();
    step.log(format!(
        "restored {node} outputs from the artifact cache (signature {})",
        sig.short()
    ));
    Ok(Some(paths))
}

/// Tier-2 store side of [`restore_stage_outputs`]: put `node`'s built outputs
/// under signature `sig` so a later build restores instead of recompiling. A
/// disabled store (`None`) is a no-op.
///
/// Reaching here means this run produced `files`, so this is also where the step
/// records [`Step::compiled`] — the symmetric half of the [`Step::restored`] that
/// [`restore_stage_outputs`] records on a hit. A multi-node stage that restores one
/// node and builds another therefore reports [`StepOutcome::Mixed`](crate::event::StepOutcome::Mixed)
/// on its own, without each stage having to remember to say so.
///
/// A disabled store returns before that call, which cannot understate the outcome:
/// [`Step::restored`] is reachable only through the store, so with no store nothing
/// is ever restored and the step reports
/// [`Built`](crate::event::StepOutcome::Built) either way.
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
    step.compiled();
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
pub enum SeriesIdentity<'a> {
    /// Pinned mode: the folded `patches.commit` is the series identity.
    Pinned,
    /// Co-dev mode: the ordered `label=sha256` fingerprint of the on-disk series
    /// (`patch_series_fingerprint`).
    Dev(&'a [String]),
}

/// The ordered content fingerprint of the patch series a `scope` applies from a live
/// `patches_root` checkout — for each patches-repo-relative label across every named
/// series' scope list, in series-then-list order, `"<label>=<sha256 of its bytes>"`.
///
/// Folded into a Tier-1 tree signature only in co-dev mode ([`SeriesIdentity::Dev`]);
/// in pinned mode `lock.patches.commit` already content-addresses the series.
/// Best-effort by design: a series that cannot be loaded contributes nothing and an
/// unreadable patch file folds a stable `<unreadable>` sentinel, so computing a
/// signature never fails here — a genuinely broken series fails loudly at apply time
/// ([`apply_series_scope`]) instead, and no successful build could have stamped a
/// tree for it to falsely reuse.
pub(crate) fn patch_series_fingerprint(
    patches_root: &Path,
    series: &[String],
    scope: PatchScope,
) -> Vec<String> {
    let mut out = Vec::new();
    for name in series {
        let Ok(series) = boot2deb_core::load_series(patches_root, name) else {
            continue;
        };
        // Unfiltered on purpose: this keys a cache, and folding every entry — including
        // ones the current kernel does not select — only ever over-invalidates. The
        // range is folded beside the digest because editing a range changes which
        // patches apply without changing any file's content. Labels are repo-relative
        // paths, unique per series, so concatenating series never collides.
        out.extend(series.scope(scope.core_scope()).iter().map(|entry| {
            let label = entry.path();
            let digest = std::fs::read(patches_root.join(label))
                .map(|bytes| crate::blobs::sha256_hex(&bytes))
                .unwrap_or_else(|_| "<unreadable>".to_string());
            match entry.kernels() {
                Some(range) => format!("{label}@{range}={digest}"),
                None => format!("{label}={digest}"),
            }
        }));
    }
    out
}

/// The co-dev content fingerprint of `scope`'s series, or empty when the build is in
/// pinned mode or applies no patches at all. Paired with [`series_identity`], which
/// borrows the result; the two are split so the `Vec` outlives the borrowing
/// [`SeriesIdentity`]. Every compile stage computes its series identity through this
/// pair, so "no patch series" is handled once rather than per stage.
pub(crate) fn dev_series_fingerprint(
    patches: Option<PatchSource>,
    scope: PatchScope,
) -> Vec<String> {
    match patches {
        Some(p) if p.dev => patch_series_fingerprint(p.root, &p.pin.series, scope),
        _ => Vec::new(),
    }
}

/// The [`SeriesIdentity`] a stage folds into its Tier-1 signature, given `fp` from
/// [`dev_series_fingerprint`]. A build with no patch source reports `Pinned`, which
/// [`fold_patch_series`] then ignores in favour of its `patches = "none"` scalar —
/// there is no series to be pinned or co-developed.
pub(crate) fn series_identity<'a>(
    patches: Option<PatchSource>,
    fp: &'a [String],
) -> SeriesIdentity<'a> {
    if patches.is_some_and(|p| p.dev) {
        SeriesIdentity::Dev(fp)
    } else {
        SeriesIdentity::Pinned
    }
}

/// The ordered content fingerprint of a board's loose device-tree sources — for each
/// resolved `device_dts` path, in order, `"<basename>=<sha256 of its bytes>"`.
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

/// Fold the applied patch series' identity into a Tier-1 tree signature: the ordered
/// series names and pinned commit, then either the pinned marker or (co-dev) the
/// live-series fingerprint. Shared by every compile stage's `clone_manifest`
/// so the pinned-vs-co-dev discipline is one implementation. The pinned fold is byte-
/// identical to folding `patches_dev = "0"` alone, so a pinned tree signature is
/// unchanged by co-dev support — only co-dev builds gain the extra fingerprint.
///
/// A build with no patch series (`pin` is `None`) folds a single `patches = "none"`
/// scalar: it has no series, commit, or series to identify, and the distinct label
/// keeps its signature from ever colliding with a patched tree's. Folding the series
/// list in order means adding or reordering a composed series restamps the tree.
pub(crate) fn fold_patch_series(
    b: &mut crate::signature::SignatureBuilder,
    pin: Option<&boot2deb_core::lock::PatchesPin>,
    patches: SeriesIdentity,
) {
    let Some(pin) = pin else {
        b.fold_scalar("patches", "none");
        return;
    };
    b.fold_ordered("patches.series", &pin.series);
    b.fold_scalar("patches.commit", &pin.commit);
    match patches {
        SeriesIdentity::Pinned => {
            b.fold_scalar("patches_dev", "0");
        }
        SeriesIdentity::Dev(fingerprint) => {
            b.fold_scalar("patches_dev", "1");
            b.fold_ordered("patch_series", fingerprint);
        }
    }
}

/// Copy `src` into `out_dir` (created if needed) under its own name, returning the
/// destination path. Used to stage a built artifact out of a scratch tree.
///
/// See [`stage_artifact_as`] for the publish's atomicity contract, and for the case
/// where the published name is not the source's.
fn stage_artifact(out_dir: &Path, src: &Path) -> Result<PathBuf, EngineError> {
    let file_name = src
        .file_name()
        .expect("artifact path has a file name")
        .to_string_lossy()
        .into_owned();
    stage_artifact_as(out_dir, src, &file_name)
}

/// Copy `src` into `out_dir` (created if needed) as `file_name`, returning the
/// destination path.
///
/// The published name is separate from the source's because the two answer different
/// questions: a build tree names a file for what it *is* (`idbloader.img`), while the
/// output dir names it for the build point that owns it
/// ([`BuildPoint::artifact_stem`](boot2deb_core::buildpoint::BuildPoint::artifact_stem)),
/// so several recipes can share one `--out-dir` without overwriting each other. The
/// same split lets the artifact cache store one copy under the canonical name and
/// serve it to every point whose signature matches.
///
/// The publish is atomic: the bytes copy into a sibling `.partial` temp on
/// the same filesystem, then a rename moves it over `dest`. An interrupted copy leaves
/// a `.partial` temp (swept by the cache/out_dir GC), never a truncated `.deb` at a
/// valid name — which would either overwrite a previously-staged good artifact the
/// ledger already trusts or, on a rootfs-only retry, be ingested as a half-written
/// package. Two runs staging the same name use pid-distinct temps.
fn stage_artifact_as(out_dir: &Path, src: &Path, file_name: &str) -> Result<PathBuf, EngineError> {
    std::fs::create_dir_all(out_dir).map_err(|source| EngineError::io(out_dir, source))?;
    let dest = out_dir.join(file_name);
    let tmp = out_dir.join(format!(".{file_name}.{}.partial", std::process::id()));
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

/// The compressor the `.deb`s boot2deb archives *itself* are built with, stated rather
/// than inherited: the u-boot and kmod packages through [`archive_deb`], and the kernel
/// through its `KDEB_COMPRESS` (see [`kernel::kbuild_env`]).
///
/// `dpkg-deb`'s default compressor is a *distribution* choice, not a `dpkg` constant:
/// Debian's dpkg defaults to `xz`, Ubuntu-derived dpkg to `zstd`. Stating it is what
/// makes the archive's structure a property of this file rather than of whichever suite
/// supplies the `dpkg`, alongside the [`normalize_data_tree`] + `SOURCE_DATE_EPOCH` work
/// these packages do to be byte-identical. `xz` is also the compressor every dpkg since
/// 1.15.6 can read, so a stated `xz` is the portable choice as well as the reproducible
/// one.
///
/// **Not** every `.deb` in the image: the userspace and ffmpeg packages are archived by
/// `dpkg-buildpackage`/`dpkg-deb` inside the build sandbox without these flags, so they
/// take that root's own default. That is a weaker guarantee but not a host-dependent
/// one — the root is provisioned from the build's pinned mirror list like any other
/// input.
pub(crate) const DEB_COMPRESSOR: &str = "xz";
/// Compression level for [`DEB_COMPRESSOR`]. Stated for the same reason as the
/// compressor: it is a `dpkg-deb` default, and a default is a thing that can move.
const DEB_COMPRESS_LEVEL: &str = "6";

/// Archive an already-staged package tree into `deb_out`, running `dpkg-deb --build` in
/// the build's [`PackagingSandbox`].
///
/// Everything that shapes the archive bytes beyond the staged content itself is nailed
/// down here:
///
/// - **The tool.** `dpkg-deb` and its `liblzma` come from the packaging root — a
///   sha256-pinned package resolved from the build's own mirror list — so the archiver
///   is an input the lock describes rather than whatever the build host installed.
/// - **Ownership.** The root maps the caller to uid 0, so the host-staged tree stats as
///   `root:root` inside and is archived with the ownership a `.deb` must carry. No
///   `fakeroot`: there is nothing left to fake.
/// - **Time.** `source_date_epoch` (a locked commit's committer date) makes `dpkg-deb`
///   clamp every member's mtime to it, so the `.deb` carries the lock's timestamp rather
///   than the build clock. Passed as an environment variable because that is the only
///   interface `dpkg-deb` has for it.
/// - **The compression.** [`DEB_COMPRESSOR`] and [`DEB_COMPRESS_LEVEL`], stated rather
///   than left to the root's own default.
///
/// `binds` are the host paths the run needs to see — the staged tree and wherever
/// `deb_out` is written — exposed inside at their host path. Mode normalization
/// ([`normalize_data_tree`]) and the control text stay on the host side, in the stage
/// that knows what it is packaging.
///
/// Neither the kernel's `.deb` nor ffmpeg's comes through here: `make bindeb-pkg` runs
/// `dpkg-buildpackage` inside the kernel tree, and the ffmpeg deb is archived by the
/// *build* sandbox's own `dpkg`, in the target-arch root it was compiled in.
pub(crate) fn archive_deb(
    root: &PackagingSandbox,
    pkg_stage: &Path,
    deb_out: &Path,
    binds: &[PathBuf],
    source_date_epoch: Option<u64>,
    context: &str,
    step: &Step,
) -> Result<(), EngineError> {
    let argv = vec![
        "dpkg-deb".to_string(),
        "--build".to_string(),
        "-Z".to_string(),
        DEB_COMPRESSOR.to_string(),
        "-z".to_string(),
        DEB_COMPRESS_LEVEL.to_string(),
        pkg_stage.to_string_lossy().into_owned(),
        deb_out.to_string_lossy().into_owned(),
    ];
    let env: Vec<(String, String)> = source_date_epoch
        .map(|epoch| ("SOURCE_DATE_EPOCH".to_string(), epoch.to_string()))
        .into_iter()
        .collect();
    // Which root archived it, on the line before it happens: the whole point of the
    // move is that this is an input to the output, so a build log that does not name it
    // cannot answer what produced the `.deb` it just published. The compile stages log
    // their sandbox the same way.
    step.log(format!("archiving in the {} root", root.describe()));
    root.run(
        &SandboxRun {
            // The staged tree, which every bind already covers and which `dpkg-deb`
            // never writes into.
            work: pkg_stage,
            binds,
            env: &env,
            argv: &argv,
            context,
            probe: None,
        },
        step,
    )
}

/// The umask every boot2deb build runs under, declared rather than inherited.
///
/// The umask is the one build-host setting no environment variable covers: it is a
/// process attribute, so it passes through `base_env(false)`, through the cage, and
/// into every `mkdir` a build makes. Left inherited it reaches the image. `mkdir -p`
/// asks for `0777`, so a `002` umask (the Ubuntu/Pop!_OS default) creates `0775`
/// directories — which is how a `make install` staging tree comes to ship
/// group-writable directories inside a `.deb`, and how `dpkg`'s own state files come
/// to be group-writable in the rootfs. `022` is Debian's default and the mode every
/// such directory is meant to have.
///
/// This covers modes created *during* the build.
/// [`normalize_overlay_modes`](crate::rootfs) covers the other half — modes that
/// already exist on disk, because a git checkout materializes an overlay tree at the
/// developer's umask before boot2deb runs at all. Neither subsumes the other.
///
/// Called once from the CLI entry point: it mutates process-global state, so it
/// belongs at the top of a program rather than inside a library call that a caller
/// with its own threads did not ask to be reconfigured.
pub fn declare_umask() {
    // Safety: `umask` is always successful, returns the previous mask, and touches no
    // memory. Called before any build thread exists.
    unsafe { libc::umask(0o022) };
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

/// Pick the last entry among `names` whose file name starts with `prefix` and ends with
/// `.deb`, in [`natural_version_cmp`] order. Pure, so the artifact selection is testable
/// without a build.
///
/// The stage directory it scans holds **one** version per prefix — [`purge_stage_debs`]
/// clears the prefix before the compile writes into it — so the ordering breaks a tie
/// that should not arise rather than choosing between real candidates. That matters
/// because the comparison is `sort -V`, not dpkg's: see [`natural_version_cmp`].
fn pick_deb(names: &[String], prefix: &str) -> Option<String> {
    names
        .iter()
        .filter(|n| n.starts_with(prefix) && n.ends_with(".deb"))
        .max_by(|a, b| natural_version_cmp(a, b))
        .cloned()
}

/// Compare two `.deb` file names the way `sort -V` does: split into runs of digits and
/// non-digits and compare digit runs numerically. Enough to order
/// `linux-image-…-9_…` after `…-10_…` correctly.
///
/// **Not dpkg ordering.** It has no `~` rule, so `1.0~rc1` sorts *above* `1.0` where
/// dpkg puts it below. Nothing here needs the difference — [`pick_deb`] scans a
/// directory holding one version per prefix — and reaching for dpkg semantics would mean
/// implementing the epoch/upstream/revision split too. If a caller ever has to choose
/// between two real versions, this is the wrong function.
fn natural_version_cmp(a: &str, b: &str) -> std::cmp::Ordering {
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
    use crate::event::{Event, StepOutcome};
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
        // The stderr a mid-transfer clone failure actually leaves behind.
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
        let err =
            fetch_commit(src.to_str().unwrap(), orphan, orphan, "mpp", &dst, &step).unwrap_err();
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
        assert_eq!(
            deb_names(tmp.path()).unwrap(),
            vec!["a.deb", "b.deb", "c.deb"]
        );
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

    /// A multi-node stage (kmod: a module node and a firmware node) must report what it
    /// actually did per node. Restoring one and building the other is `Mixed`, not
    /// `Restored` — the latter reads as "nothing was compiled", which would make a
    /// genuinely stale output indistinguishable from a freshly built one in the log.
    #[test]
    fn a_stage_that_restores_one_node_and_builds_another_reports_mixed() {
        let tmp = tempfile::tempdir().unwrap();
        let store_root = tmp.path().join("store");
        let out = tmp.path().join("out");
        let sig_a = crate::signature::SignatureBuilder::new("node-a", 1).finish();
        let sig_b = crate::signature::SignatureBuilder::new("node-b", 1).finish();
        let deb = tmp.path().join("x.deb");
        std::fs::write(&deb, b"x").unwrap();

        // Seed only node-a, so a later run hits on it and misses on node-b.
        {
            let sink = |_e: Event| {};
            let seed = Step::start(&sink, "seed");
            store_stage_outputs(Some(&store_root), "a", &sig_a, &[("deb", &deb)], &seed).unwrap();
        }

        let outcome = |f: &dyn Fn(&Step)| {
            let seen = RefCell::new(None);
            let sink = |e: Event| {
                if let Event::StepFinished { outcome, .. } = e {
                    *seen.borrow_mut() = Some(outcome);
                }
            };
            let step = Step::start(&sink, "kmod");
            f(&step);
            step.finish();
            seen.into_inner().expect("the step reports an outcome")
        };

        // node-a restores, node-b misses and is built+stored.
        assert_eq!(
            outcome(&|step| {
                assert!(restore_stage_outputs(
                    Some(&store_root),
                    "a",
                    &sig_a,
                    &out,
                    &["deb"],
                    step
                )
                .unwrap()
                .is_some());
                assert!(restore_stage_outputs(
                    Some(&store_root),
                    "b",
                    &sig_b,
                    &out,
                    &["deb"],
                    step
                )
                .unwrap()
                .is_none());
                store_stage_outputs(Some(&store_root), "b", &sig_b, &[("deb", &deb)], step)
                    .unwrap();
            }),
            StepOutcome::Mixed
        );

        // Both nodes now hit: nothing is compiled, so the claim is a plain restore.
        assert_eq!(
            outcome(&|step| {
                for (node, sig) in [("a", &sig_a), ("b", &sig_b)] {
                    assert!(restore_stage_outputs(
                        Some(&store_root),
                        node,
                        sig,
                        &out,
                        &["deb"],
                        step
                    )
                    .unwrap()
                    .is_some());
                }
            }),
            StepOutcome::Restored
        );

        // A store the stage never restored from is a plain build, not a mixed one.
        assert_eq!(
            outcome(&|step| {
                let fresh = tmp.path().join("store2");
                store_stage_outputs(Some(&fresh), "b", &sig_b, &[("deb", &deb)], step).unwrap();
            }),
            StepOutcome::Built
        );
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
            let out = Command::new("git")
                .arg("-C")
                .arg(&origin)
                .args(args)
                .output()
                .unwrap();
            assert!(
                out.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&out.stderr)
            );
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
        assert!(!dir
            .join("linux-image-7.1.2-1-arm64_7.1.2-1_arm64.deb")
            .exists());
        assert!(!dir
            .join("linux-headers-7.1.2-1-arm64_7.1.2-1_arm64.deb")
            .exists());
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
            EngineError::PatchesPinMismatch {
                expected, dirty, ..
            } => {
                assert_eq!(expected, other);
                assert!(!dirty);
            }
            e => panic!("expected PatchesPinMismatch, got {e:?}"),
        }
        let msg = err.to_string();
        assert!(
            msg.contains("boot2deb update"),
            "unknown-relation remedy offers update: {msg}"
        );
        assert!(
            msg.contains(other),
            "unknown-relation remedy offers the pin: {msg}"
        );
        // The --patches-path co-dev override downgrades the mismatch to a warning.
        verify_patches_pin(repo, other, true, &step).unwrap();

        // A checkout ahead of the pin (a commit past it) is told to re-pin with
        // `update`, not to re-checkout — that would discard the new commit.
        std::fs::write(repo.join("f"), "newer").unwrap();
        git(&["add", "f"]);
        git(&["commit", "-qm", "newer"]);
        let msg = verify_patches_pin(repo, &head, false, &step)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("ahead of the pin"),
            "ahead remedy names the state: {msg}"
        );
        assert!(
            msg.contains("boot2deb update"),
            "ahead remedy points at update: {msg}"
        );
        let newer_head = git::rev_parse_head(repo).unwrap();

        // A stale checkout (HEAD behind the pin) is told to check out the pin.
        git(&["checkout", "-q", head.as_str()]);
        let msg = verify_patches_pin(repo, &newer_head, false, &step)
            .unwrap_err()
            .to_string();
        assert!(
            msg.contains("behind the pin"),
            "behind remedy names the state: {msg}"
        );
        assert!(
            msg.contains(&format!("checkout {newer_head}")),
            "behind remedy gives the command: {msg}"
        );
        git(&["checkout", "-q", "-"]);

        // An uncommitted change fails the clean check even at the right commit — but
        // nothing is *mismatched* there, so it is its own error and must not claim the
        // checkout is at some commit other than the one it pins.
        std::fs::write(repo.join("f"), "changed").unwrap();
        let err = verify_patches_pin(repo, &newer_head, false, &step).unwrap_err();
        assert!(
            matches!(err, EngineError::PatchesWorktreeDirty { .. }),
            "expected PatchesWorktreeDirty, got {err:?}"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("commit them"),
            "dirty remedy leads with commit: {msg}"
        );
        assert!(
            !msg.contains("but the lock pins"),
            "an on-pin dirty tree is not a pin mismatch: {msg}"
        );
        assert_eq!(
            msg.matches(newer_head.as_str()).count(),
            1,
            "the commit is named once, not as both actual and expected: {msg}"
        );
        // ...but the override tolerates a dirty co-dev checkout too.
        verify_patches_pin(repo, &newer_head, true, &step).unwrap();
    }

    /// `git init` + one commit of whatever `dir` already holds, returning its HEAD.
    fn commit_all(dir: &Path) -> String {
        let git = |args: &[&str]| {
            assert!(
                Command::new("git")
                    .arg("-C")
                    .arg(dir)
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
        git(&["add", "-A"]);
        git(&["commit", "-qm", "base", "--allow-empty"]);
        git::rev_parse_head(dir).unwrap()
    }

    /// A committed `patches` checkout holding one series manifest, returned with its
    /// HEAD so a [`PatchSource`] can pin it. The `TempDir` is returned so the caller
    /// keeps the checkout alive.
    fn patches_checkout(name: &str, manifest: &str) -> (tempfile::TempDir, String) {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("series")).unwrap();
        std::fs::write(
            tmp.path().join("series").join(format!("{name}.toml")),
            manifest,
        )
        .unwrap();
        let head = commit_all(tmp.path());
        (tmp, head)
    }

    #[test]
    fn the_uboot_scope_gates_its_envelope_against_the_uboot_ref() {
        // The gate must fire against the *u-boot* ref, and a node that handed over no
        // reference at all would leave this arm — and `applies_to_uboot` with it —
        // unreachable. The kernel envelope below excludes v2026.04, so reading that one
        // here would refuse a u-boot the series does claim.
        let (patches, head) = patches_checkout(
            "rk3576-loader",
            "applies_to_kernel = \">=7.0, <7.2\"\n\
             applies_to_uboot  = \">=2026.01, <2027.01\"\n\
             uboot = []\n",
        );
        let pin = boot2deb_core::lock::PatchesPin {
            series: vec!["rk3576-loader".to_string()],
            source: "https://example.invalid/patches.git".to_string(),
            reference: "main".to_string(),
            commit: head,
        };
        // A real repo: the apply pass runs `git` against the tree it patches, so an
        // in-envelope run has to reach a tree it can actually inspect.
        let tree = tempfile::tempdir().unwrap();
        commit_all(tree.path());
        let sink = |_: Event| {};
        let step = Step::start(&sink, "t");
        let scope = |version: &'static str| ApplyScope {
            tree: tree.path(),
            patches: Some(PatchSource {
                root: patches.path(),
                pin: &pin,
                dev: false,
                version,
            }),
            scope: PatchScope::Uboot,
            target: "u-boot",
            gate_reference: Some(version),
        };
        // In envelope: the (empty) series applies, so nothing is rejected.
        assert_eq!(apply_series_scope(&scope("v2026.04"), &step).unwrap(), 0);
        // Out of envelope: refused before the tree is touched.
        let err = apply_series_scope(&scope("v2027.04"), &step).unwrap_err();
        assert!(
            err.to_string().contains("does not target u-boot v2027.04"),
            "{err}"
        );
    }

    #[test]
    fn probe_object_distinguishes_present_absent_and_errored() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("repo");
        std::fs::create_dir(&repo).unwrap();
        let git = |args: &[&str]| {
            Command::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .output()
                .unwrap()
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
        assert!(
            reject_optionlike("source", "https://git.u-boot-project.org/u-boot/u-boot.git").is_ok()
        );
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
        assert!(
            reject_unsafe_make_target("uboot_defconfig", "turing-rk1-rk3588_defconfig").is_ok()
        );
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
    fn natural_version_cmp_orders_numeric_runs_and_is_not_dpkg() {
        use std::cmp::Ordering;
        assert_eq!(natural_version_cmp("a-9-b", "a-10-b"), Ordering::Less);
        assert_eq!(natural_version_cmp("a-2-b", "a-2-b"), Ordering::Equal);
        assert_eq!(natural_version_cmp("b", "a"), Ordering::Greater);
        // Stated so the name is not read as a promise it does not make: dpkg sorts a
        // `~` suffix *below* the release it precedes, and this sorts it above.
        assert_eq!(
            natural_version_cmp("p_1.0~rc1_a.deb", "p_1.0_a.deb"),
            Ordering::Greater
        );
    }

    /// The sandbox identity separates what must not share an artifact-cache entry.
    ///
    /// A native build folds no interpreter segment at all, so it can never collide
    /// with a cross build whose `qemu-user` is simply absent — the two produce
    /// genuinely different `.deb`s and a shared key would restore the wrong one.
    #[test]
    fn the_sandbox_identity_separates_userland_and_interpreter() {
        use crate::toolchain::HostToolchain;
        let live = vec!["http://deb.debian.org/debian".to_string()];
        let snapshot = vec!["https://snapshot.debian.org/archive/debian/20260628T083000Z/".into()];
        // A cross build whose interpreter cannot be run still folds a fallback naming
        // it; a native build folds nothing, so the two are never equal.
        let cross = HostToolchain::probe(Some("boot2deb-no-such-arch"));
        let native = HostToolchain::probe(None);
        let id = |mirrors: &[String], tc: &HostToolchain| {
            sandbox_identity("arm64", "forky", mirrors, tc)
        };
        assert_ne!(id(&live, &cross), id(&live, &native));
        // A snapshot-pinned userland is a different compiler than the live mirror's.
        assert_ne!(id(&live, &cross), id(&snapshot, &cross));
        // A fallback list is neither of the two single-mirror identities.
        let fallback = [live.clone(), snapshot.clone()].concat();
        for one in [&live, &snapshot] {
            assert_ne!(id(&fallback, &cross), id(one, &cross));
        }
        // And the arch and suite separate too: the same mirrors bootstrap a different
        // compiler for each, and only the tree name carries that.
        assert_ne!(
            id(&live, &native),
            sandbox_identity("armhf", "forky", &live, &native)
        );
        assert_ne!(
            id(&live, &native),
            sandbox_identity("arm64", "trixie", &live, &native)
        );
    }

    /// Both identities are the name of the tree they describe, so a claim can never be
    /// keyed on a base the build did not actually provision.
    ///
    /// The property that matters is the coupling, not the string: asserting a literal
    /// would pass just as well if the two functions computed it independently, which is
    /// the failure mode. So each is compared against the directory function it must
    /// track — including the base package set, which reaches the digest and which
    /// nothing else in either signature covers.
    #[test]
    fn each_identity_is_the_name_of_the_tree_it_describes() {
        use crate::toolchain::HostToolchain;
        let mirrors = vec![crate::DEFAULT_MIRROR.to_string()];
        let native = HostToolchain::probe(None);
        let leaf = |p: std::path::PathBuf| p.file_name().unwrap().to_string_lossy().into_owned();

        assert_eq!(
            sandbox_identity("arm64", "forky", &mirrors, &native),
            leaf(crate::sandbox::build_sandbox_dir(
                Path::new("/some/work/dir"),
                SandboxRole::Target,
                "arm64",
                "forky",
                &mirrors
            )),
            "the build sandbox's identity is its tree's name, whatever work dir holds it"
        );
        assert_eq!(
            packaging_identity("amd64", "forky", &mirrors),
            leaf(crate::sandbox::packaging_root_dir(
                Path::new("/some/work/dir"),
                "amd64",
                "forky",
                &mirrors
            )),
            "and so is the packaging root's"
        );
        assert_eq!(
            cross_identity("amd64", "arm64", "forky", &mirrors),
            leaf(crate::sandbox::build_sandbox_dir(
                Path::new("/some/work/dir"),
                SandboxRole::Cross { target: "arm64" },
                "amd64",
                "forky",
                &mirrors
            )),
            "and so is the cross root's"
        );
        // No two roles key alike even at one arch, suite and mirror list: they are
        // different trees holding different packages. Asserted as a set, because the
        // defect this guards against is two identities computed independently and
        // agreeing by accident.
        let at_amd64 = [
            sandbox_identity("amd64", "forky", &mirrors, &native),
            packaging_identity("amd64", "forky", &mirrors),
            cross_identity("amd64", "arm64", "forky", &mirrors),
        ];
        let distinct: std::collections::BTreeSet<&String> = at_amd64.iter().collect();
        assert_eq!(distinct.len(), at_amd64.len(), "{at_amd64:?}");
    }

    /// The declared umask is what keeps a directory the *build itself* creates at
    /// `0755`, whatever the host's own umask is.
    ///
    /// Asserted through the consequence rather than the mask value, because the
    /// consequence is the defect: `mkdir` asks for `0777` and takes what the umask
    /// leaves, so under a `002` host umask a `make install` staging tree lands `0775`
    /// directories in the shipped `.deb` and no per-file mode in the packaging touches
    /// them.
    #[test]
    fn the_declared_umask_keeps_a_build_made_directory_at_0755() {
        use std::os::unix::fs::PermissionsExt;

        declare_umask();
        let tmp = tempfile::tempdir().unwrap();
        // `std::fs::create_dir` requests 0o777, exactly as `mkdir -p` does.
        let dir = tmp.path().join("made-by-the-build");
        std::fs::create_dir(&dir).unwrap();
        assert_eq!(
            dir.metadata().unwrap().permissions().mode() & 0o7777,
            0o755,
            "a directory the build created followed the host umask into the image"
        );
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
            std::fs::metadata(root.join(p))
                .unwrap()
                .permissions()
                .mode()
                & 0o777
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
