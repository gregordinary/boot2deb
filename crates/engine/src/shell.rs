//! An interactive shell in the root a build stage compiles in — the `shell` command.
//!
//! A failed compile is ordinarily diagnosed from captured output. This is the other
//! way: stand the stage's root up again and enter it, with the same base tree, the same
//! layered build-dependencies, the same mounts, the same declared environment and the
//! same identity map the compile had, and look at it.
//!
//! Linux side effects, like the rest of the engine: it provisions the root
//! ([`crate::sandbox`]) and then holds two file descriptors open between the operator's
//! terminal and the sandbox's own for as long as the session lasts.
//!
//! # What "the same root" means, and what it does not
//!
//! The root is **re-staged, not attached to**. A [`BuildRoot`]
//! is disposable by construction: its overlay upper is discarded when the stage that
//! declared it ends, including when it ends in a failure. So what a session enters is
//! the root that stage's declaration produces — the same immutable base tree, resolved
//! from the same mirrors, plus the same declared build-dependency set — rather than the
//! dead run's writable layer. What the compile *wrote* into its upper is gone; what it
//! wrote into the work dir is bound, at its host path, and is what a diagnosis is
//! usually after.
//!
//! The session's layer is staged into a directory of its own — the stage's name,
//! prefixed — so opening one while a build of the same recipe runs cannot reclaim that
//! build's upper out from under it.
//!
//! # The one deviation from the build profile
//!
//! Every command boot2deb runs in a sandbox launches under the one profile
//! [`crate::sandbox`] defines, and a session launches under that profile with **standard
//! input reset to [`Stdio::Inherit`]**. Two reasons, and they are the same reason:
//!
//! - A terminal launch is *refused* against a stream disposition that names a
//!   destination of its own, since the pseudoterminal's replica is already all three
//!   streams. `Stdio::Null` names one.
//! - The hazard `Stdio::Null` exists to close is not reintroduced. It keeps a build out
//!   of the operator's session, where a maintainer script could read `/dev/tty` and push
//!   characters into the operator's input queue. A terminal launch owns a session *and a
//!   controlling terminal* of its own, so `/dev/tty` inside resolves to the sandbox's own
//!   pseudoterminal — which the operator is not sharing with anything.
//!
//! `stop_with_caller` stays as the profile sets it, which is what an interactive session
//! wants anyway: kill boot2deb and the session goes with it.
//!
//! # What does not work inside
//!
//! The pseudoterminal is allocated on the host before the fork, so it has no device node
//! in the sandbox's own `/dev/pts`. Path resolution for the terminal fails: `tty(1)`,
//! `who`, and `GPG_TTY` (and so `pinentry`) have no answer. Everything that operates on
//! the descriptor — `isatty`, `tcsetattr`, the line discipline, the window size, job
//! control, `/dev/tty`, and nested allocation for `tmux` or `script` — works normally.
//!
//! The user-facing description of the command is the CLI reference page,
//! `docs/src/reference/cli.md`.

use crate::build::{ffmpeg, kernel, kmod, uboot, userspace};
use crate::error::EngineError;
use crate::event::{EventSink, Step};
use crate::repo::LocalDistsRepo;
use crate::sandbox::{BuildRoot, BuildRootSpec, BuildSandbox, PackagingSandbox, SandboxRun};
use boot2deb_core::lock::Lock;
use boot2deb_core::ResolvedBuild;
use ferroday_cage::{Pty, Stdio, Terminal};
use std::io::{Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The command a session runs when the caller names none.
///
/// `bash` rather than `sh`: it is Essential in Debian, so every root here carries it,
/// and an interactive session is the one place its line editing and history are worth
/// having. Resolved against `PATH` inside the root like every other bare tool name the
/// stages pass, so it is the root's own `bash` and never the host's.
const DEFAULT_COMMAND: &str = "bash";

/// Which of a build's roots a session enters, and which stage's build-dependencies it
/// layers over it.
///
/// One variant per root a build command can fail in. The rootfs is deliberately not
/// among them: that tree is a per-run temporary, bootstrapped, customized, exported to a
/// tarball and removed within one stage, so there is no tree for a later session to
/// enter.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ShellStage {
    /// The cross root the kernel compiles in, layered with the kernel stage's
    /// build-dependencies.
    Kernel,
    /// The cross root u-boot compiles in, layered with the u-boot stage's
    /// build-dependencies.
    Uboot,
    /// The cross root an out-of-tree module compiles in. An out-of-tree module build
    /// *is* a kbuild invocation, so it declares the kernel stage's set, exactly as the
    /// kmod stage does.
    Kmod,
    /// The target-arch root the MPP/RGA/Mali packages compile in.
    Userspace,
    /// The target-arch root ffmpeg compiles in, layered with the suite's codec
    /// libraries *and* this build's own userspace `.deb`s from a local pool.
    Ffmpeg,
    /// The host-arch packaging root a staged tree becomes a `.deb` in. Never layered,
    /// so a session enters the base itself.
    Packaging,
}

/// Which root a [`ShellStage`] names — the discriminator [`open`] resolves against the
/// three roots it is given.
///
/// Separate from the stage because several stages share a root: naming the root is what
/// decides which sandbox to enter, and naming the stage is what decides what to layer
/// over it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum RootKind {
    /// The host-arch cross root ([`SandboxRole::Cross`](crate::sandbox::SandboxRole)).
    Cross,
    /// The target-arch build root ([`SandboxRole::Target`](crate::sandbox::SandboxRole)).
    Target,
    /// The host-arch packaging root ([`PackagingSandbox`]).
    Packaging,
}

impl ShellStage {
    /// The stage's name as the CLI spells it.
    pub fn as_str(self) -> &'static str {
        match self {
            ShellStage::Kernel => "kernel",
            ShellStage::Uboot => "uboot",
            ShellStage::Kmod => "kmod",
            ShellStage::Userspace => "userspace",
            ShellStage::Ffmpeg => "ffmpeg",
            ShellStage::Packaging => "packaging",
        }
    }

    /// Which root this stage compiles or packages in.
    fn root(self) -> RootKind {
        match self {
            ShellStage::Kernel | ShellStage::Uboot | ShellStage::Kmod => RootKind::Cross,
            ShellStage::Userspace | ShellStage::Ffmpeg => RootKind::Target,
            ShellStage::Packaging => RootKind::Packaging,
        }
    }

    /// The name the session's overlay upper is staged under
    /// ([`BuildRootSpec::stage`]) — the stage's own, prefixed.
    ///
    /// Deliberately not the stage's own directory. Staging a layer first reclaims the
    /// directory it stages into, so sharing the name with the stage would let a session
    /// pull the ground out from under a build of the same recipe running beside it. The
    /// resolved layer is identical either way: the name reaches nothing but the
    /// directory and a log line.
    fn layer_stage(self) -> &'static str {
        match self {
            ShellStage::Kernel => "shell-kernel",
            ShellStage::Uboot => "shell-uboot",
            ShellStage::Kmod => "shell-kmod",
            ShellStage::Userspace => "shell-userspace",
            ShellStage::Ffmpeg => "shell-ffmpeg",
            ShellStage::Packaging => "shell-packaging",
        }
    }

    /// The stage's own tree under `work_dir` — where a session starts when the tree is
    /// there.
    ///
    /// Read from each stage module's own accessor rather than composed here, so the
    /// session and the compile cannot disagree about where a stage's tree lives.
    /// [`ShellStage::Packaging`] has none: it archives trees the other stages staged
    /// and owns no tree of its own.
    fn tree_dir(self, work_dir: &Path) -> Option<PathBuf> {
        match self {
            ShellStage::Kernel => Some(kernel::tree_dir(work_dir)),
            ShellStage::Uboot => Some(uboot::tree_dir(work_dir)),
            ShellStage::Kmod => Some(kmod::stage_dir(work_dir)),
            ShellStage::Userspace => Some(userspace::stage_dir(work_dir)),
            ShellStage::Ffmpeg => Some(ffmpeg::tree_dir(work_dir)),
            ShellStage::Packaging => None,
        }
    }
}

/// The three roots a build stands up, for [`open`] to choose between.
///
/// All three rather than the one the stage needs, so the caller cannot hand over a root
/// that does not match the stage it asked for — which would be a session in the wrong
/// architecture, reported as nothing at all.
pub struct ShellRoots<'a> {
    /// The target-arch build sandbox, or `None` for a build that resolves no image
    /// suite and so stands one up nowhere. Asking for a stage that needs it is then an
    /// error naming why.
    pub target: Option<&'a dyn BuildSandbox>,
    /// The host-arch cross build sandbox. Unconditional: every deliverable compiles.
    pub cross: &'a dyn BuildSandbox,
    /// The host-arch packaging root. Unconditional, for the same reason.
    pub packaging: &'a PackagingSandbox,
}

/// What a session runs, where, and with what visible to it.
pub struct ShellOptions<'a> {
    /// Which root to enter and what to layer over it.
    pub stage: ShellStage,
    /// The build's scratch tree, bound read-write at its host path — every stage's
    /// tree, scratch and output live under it, so one bind carries all of them.
    pub work_dir: &'a Path,
    /// Where the compile stages stage their `.deb`s. Read only by
    /// [`ShellStage::Ffmpeg`], whose layer resolves this build's own userspace packages
    /// out of a pool assembled from it — the same dependency the ffmpeg stage has.
    pub out_dir: &'a Path,
    /// Further host paths to bind at their host path, for inputs that live outside the
    /// work dir — the config root's kernel fragments and board device trees, which a
    /// compile in this root reads by absolute path.
    ///
    /// Read-write, like every bind a stage makes, because the point is the mounts the
    /// compile had: the kernel stage binds those same fragment files read-write, and a
    /// session that could not re-run `merge_config.sh` the way the stage runs it would
    /// be a different environment wearing the same name.
    pub binds: &'a [PathBuf],
    /// The command and its arguments, or empty for an interactive `bash`.
    pub argv: &'a [String],
    /// Environment entries applied over the stage's own — `TERM` above all, which the
    /// declared sandbox environment does not carry because a build has no terminal to
    /// describe.
    pub env: &'a [(String, String)],
    /// The userspace trees the build being reproduced compiles — the SoC's set narrowed
    /// by its `--userspace` flags. It decides what the layer carries: a tree's own
    /// `build_deps` are layered for the whole stage. Read only by
    /// [`ShellStage::Userspace`] and [`ShellStage::Ffmpeg`].
    pub userspace: &'a [boot2deb_core::model::UserspaceTree],
    /// The `CROSS_COMPILE` prefix the kernel, u-boot and kmod stages compile with, or
    /// `None` where the cross root is already the target's architecture and the compile
    /// is native.
    pub cross_compile: Option<&'a str>,
}

/// Stand up `opts.stage`'s root and hold an interactive session in it, returning the
/// process exit code a caller adopts as its own: the command's code where it exited, and
/// the shell convention `128 + signal` where it was signalled — the number a script
/// wrapping `boot2deb shell` would read from the command run any other way. The
/// convention is [`ferroday_cage::ExitStatus::shell_code`]'s documented promise.
///
/// Provisions the root the stage compiles in — bootstrapping the base if this work dir
/// has none, then staging the stage's declared build-dependencies over it — and relays
/// the caller's terminal to a pseudoterminal inside it until the command exits.
///
/// The `step` is finished before the session starts, so the provisioning it reports and
/// the session's own output never interleave on one terminal.
///
/// Fails with [`ShellNeedsTerminal`](EngineError::ShellNeedsTerminal) when standard
/// input is not a terminal, before any provisioning: standing a root up can take minutes
/// on a cold cache, and there is nothing to relay at either end.
pub fn open(
    build: &ResolvedBuild,
    lock: &Lock,
    opts: &ShellOptions,
    roots: &ShellRoots,
    sink: &dyn EventSink,
) -> Result<u8, EngineError> {
    require_terminal()?;
    let step = Step::start(sink, "shell");

    let argv = argv(opts);
    let env = session_env(build, lock, opts);
    let work = start_dir(opts);
    let mut binds = vec![opts.work_dir.to_path_buf()];
    binds.extend(opts.binds.iter().cloned());
    let context = format!("open a shell in the {} root", opts.stage.as_str());
    let spec = SandboxRun {
        work: &work,
        binds: &binds,
        env: &env,
        argv: &argv,
        context: &context,
        probe: None,
    };

    let sandbox: Option<&dyn BuildSandbox> = match opts.stage.root() {
        RootKind::Packaging => None,
        RootKind::Cross => Some(roots.cross),
        RootKind::Target => Some(roots.target.ok_or(EngineError::StageNotApplicable {
            stage: opts.stage.as_str(),
            why: "this build resolves no image suite, so it stands up no target-arch \
                  build sandbox — the stages that compile in one do not run for it",
        })?),
    };

    // Held for the whole session: dropping a build root discards the overlay upper the
    // session is running in, which would pull the filesystem out from under it.
    let held: Option<BuildRoot>;
    let profile = match sandbox {
        None => {
            held = None;
            step.log(format!("root: {}", roots.packaging.describe()));
            roots.packaging.ensure_ready(&step)?;
            roots.packaging.profile()
        }
        Some(sandbox) => {
            step.log(format!("root: {}", sandbox.describe()));
            sandbox.ensure_ready(&step)?;
            // Assembled before the layer, as the ffmpeg stage assembles it: the layer
            // resolves this build's own `.deb`s out of it like any other package.
            let pool = ffmpeg_pool(build, lock, opts, &step)?;
            let packages = layer_packages(build, lock, opts)?;
            let packages: Vec<&str> = packages.iter().map(String::as_str).collect();
            let root = sandbox.build_root(
                &BuildRootSpec {
                    packages: &packages,
                    pool: pool.as_ref().map(LocalDistsRepo::file_url),
                    stage: opts.stage.layer_stage(),
                },
                &step,
            )?;
            let profile = root.profile();
            held = Some(root);
            profile
        }
    };

    // The one deviation from the build profile — see the [module docs](self).
    let cage = crate::sandbox::build_cage(profile.stdin(Stdio::Inherit), &spec)?;
    step.log(format!(
        "entering {} in {} — exit the shell to leave",
        argv.join(" "),
        work.display()
    ));
    // Before the relay, so the step's rendering and the session's output do not share
    // the terminal.
    step.finish();

    let end = relay(&cage, &context)?;
    // Explicit, and after the session rather than at the end of the function: the upper
    // is reclaimed when the session ends, not when the process does.
    drop(held);
    Ok(end)
}

/// The command to run: the caller's, or [`DEFAULT_COMMAND`] where it named none.
fn argv(opts: &ShellOptions) -> Vec<String> {
    if opts.argv.is_empty() {
        vec![DEFAULT_COMMAND.to_string()]
    } else {
        opts.argv.to_vec()
    }
}

/// Where the session starts: the stage's own tree when this work dir holds one, else
/// the work dir.
///
/// A session opened to diagnose a failed compile wants to start where the compile was,
/// and a session opened before any build has one honest answer left — the work dir,
/// which is bound and exists.
fn start_dir(opts: &ShellOptions) -> PathBuf {
    opts.stage
        .tree_dir(opts.work_dir)
        .filter(|tree| tree.is_dir())
        .unwrap_or_else(|| opts.work_dir.to_path_buf())
}

/// The session's environment: the stage's own build variables, then the caller's
/// entries over them.
///
/// The caller's last, so a `TERM` it passes is the one the shell sees. Both are applied
/// over the profile's declared [`SANDBOX_ENV`](crate::sandbox), which is what a compile
/// in this root sees.
fn session_env(build: &ResolvedBuild, lock: &Lock, opts: &ShellOptions) -> Vec<(String, String)> {
    let mut env = stage_env(
        build,
        opts,
        kernel::source_date_epoch(opts.work_dir, lock),
        opts.cross_compile,
    );
    env.extend(opts.env.iter().cloned());
    env
}

/// The build variables the stage's own `make` invocations carry, so a command re-run by
/// hand in here is the command the stage ran.
///
/// Split from [`session_env`] around the two values that need a lock and a work dir, so
/// the mapping itself is testable from a resolved build alone.
fn stage_env(
    build: &ResolvedBuild,
    opts: &ShellOptions,
    source_date_epoch: Option<u64>,
    cross_compile: Option<&str>,
) -> Vec<(String, String)> {
    let mut env = match opts.stage {
        // A kbuild invocation either way: the kmod stage compiles its modules against
        // the kernel tree with the kernel's own variables.
        ShellStage::Kernel | ShellStage::Kmod => kernel::kbuild_env(build, source_date_epoch),
        _ => Vec::new(),
    };
    // The three stages that compile in the cross root, which is where a toolchain prefix
    // means anything. `None` is a cross root that is already the target's architecture.
    if matches!(
        opts.stage,
        ShellStage::Kernel | ShellStage::Kmod | ShellStage::Uboot
    ) {
        if let Some(prefix) = cross_compile {
            env.push(("CROSS_COMPILE".to_string(), prefix.to_string()));
        }
    }
    env
}

/// The build-dependency set the stage layers over its base — each stage's own
/// declaration, read from the stage module rather than restated.
///
/// `build` supplies the axes a set depends on that the lock does not carry — the ffmpeg
/// stage's licence flavour, whose two forms layer different `-dev` packages. Taken from
/// the resolution rather than repeated as a session option, so the session cannot be
/// told a flavour the build was not.
fn layer_packages(
    build: &ResolvedBuild,
    lock: &Lock,
    opts: &ShellOptions,
) -> Result<Vec<String>, EngineError> {
    let owned = |v: &[&str]| v.iter().map(|s| (*s).to_string()).collect();
    let trees = opts.userspace;
    Ok(match opts.stage {
        ShellStage::Kernel | ShellStage::Kmod => owned(kernel::BUILD_DEPS),
        ShellStage::Uboot => owned(uboot::UBOOT_BUILD_DEPS),
        ShellStage::Userspace => userspace::layer_packages(trees),
        ShellStage::Ffmpeg => {
            // Read for its error, not its value: a session in the ffmpeg root is only
            // meaningful over a lock that pins the userspace `.deb`s it layers.
            userspace_pins(lock)?;
            ffmpeg::layer_packages(
                trees,
                build.image.as_ref().is_some_and(|i| i.ffmpeg_nonfree),
            )
        }
        // Never layered — the packaging root's contents are fixed at bootstrap.
        ShellStage::Packaging => Vec::new(),
    })
}

/// The trusted `file://` pool [`ShellStage::Ffmpeg`]'s layer resolves this build's own
/// userspace `.deb`s from, or `None` for every other stage and for a SoC that declares
/// no userspace trees to build.
///
/// Assembled into the session's own directory rather than reusing the ffmpeg stage's,
/// for the reason the session stages its layer under its own name: a build running
/// beside it owns that one. `None` for the release date, so the pool takes the publish
/// time — the repository is trusted-unsigned and resolved by digest, so nothing reads
/// it, and a session is not an artifact anything reproduces.
fn ffmpeg_pool(
    build: &ResolvedBuild,
    lock: &Lock,
    opts: &ShellOptions,
    step: &Step,
) -> Result<Option<LocalDistsRepo>, EngineError> {
    if opts.stage != ShellStage::Ffmpeg {
        return Ok(None);
    }
    // Read for its error: a session in the ffmpeg root is only meaningful over a lock
    // that pins the userspace `.deb`s the pool holds.
    userspace_pins(lock)?;
    let debs = ffmpeg::required_userspace_debs(opts.out_dir, opts.userspace)?;
    if debs.is_empty() {
        return Ok(None);
    }
    let suite = lock
        .rootfs
        .as_ref()
        .ok_or(EngineError::MissingPin {
            what: "rootfs",
            stage: "ffmpeg",
        })?
        .suite
        .as_str();
    let dir = opts.work_dir.join("shell").join("ffmpeg-pool");
    Ok(Some(LocalDistsRepo::assemble(
        &dir,
        &debs,
        suite,
        build.arch.debian_arch(),
        None,
        step,
    )?))
}

/// The lock's userspace pins, which decide which of this build's own packages the
/// ffmpeg root layers.
///
/// The caller schedules the media-accel stages only for a build whose lock carries
/// them, so reaching this without pins is a scheduling bug rather than a
/// misconfiguration — the same contract the ffmpeg stage itself states.
fn userspace_pins(lock: &Lock) -> Result<&[boot2deb_core::lock::UserspacePin], EngineError> {
    if lock.userspace.is_empty() {
        return Err(EngineError::MissingMediaAccelPins { stage: "ffmpeg" });
    }
    Ok(&lock.userspace)
}

/// Refuse a session with no terminal on the caller's end.
fn require_terminal() -> Result<(), EngineError> {
    // SAFETY: `isatty` reads a descriptor's kind and has no preconditions beyond the
    // descriptor number being an integer; a closed or invalid one answers 0.
    if unsafe { libc::isatty(libc::STDIN_FILENO) } == 1 {
        Ok(())
    } else {
        Err(EngineError::ShellNeedsTerminal)
    }
}

/// Relay the caller's terminal to the sandbox's own until the command exits.
///
/// Three concurrent jobs, because all three block: this thread drains the sandbox's
/// terminal to standard output, one thread carries standard input the other way, and one
/// follows `SIGWINCH`. The drain is on *this* thread deliberately — the primary's buffer
/// is a few kilobytes, so a wait that is not draining deadlocks a command that writes
/// more than that, and draining to end-of-file first means the wait that follows always
/// terminates.
///
/// The caller's terminal is in raw mode for the session: the sandbox's own line
/// discipline is the one that echoes, edits lines and turns `^C` into a signal, and two
/// line disciplines in series would do all of it twice. It is restored on every path out,
/// including a panic, by [`RawMode`]'s drop.
fn relay(cage: &ferroday_cage::Cage, context: &str) -> Result<u8, EngineError> {
    let tty = libc::STDIN_FILENO;
    // Armed before the terminal is allocated: it blocks `SIGWINCH` process-wide, and a
    // resize between the size below and the block would otherwise be a signal delivered
    // to a process whose default action for it is to ignore it, and so lost.
    let winch = WinchWatch::arm()?;
    // A dimension this terminal cannot report is left at the library's stated default
    // rather than propagated: zero is what a terminal answers when its size is
    // *unknown*, and passing it on leaves the sandbox with a terminal that answers "no
    // size" to everything that asks.
    let (rows, cols) = window_size(tty).unwrap_or((0, 0));
    let restore = RawMode::enter(tty)?;

    let (mut running, pty) = cage
        .spawn_terminal(&Terminal::new().size(rows, cols))
        .map_err(|source| EngineError::Sandbox {
            context: context.to_string(),
            source,
        })?;
    let pty = Arc::new(pty);
    winch.follow(Arc::clone(&pty), tty);
    relay_input(Arc::clone(&pty), tty);

    let mut source: &Pty = &pty;
    let mut buf = [0u8; 8192];
    loop {
        let read = source
            .read(&mut buf)
            .map_err(|source| EngineError::Terminal {
                context: "read the sandbox's terminal",
                source,
            })?;
        if read == 0 {
            break;
        }
        write_all(libc::STDOUT_FILENO, &buf[..read]).map_err(|source| EngineError::Terminal {
            context: "write to the terminal",
            source,
        })?;
    }

    let status = running.wait().map_err(|source| EngineError::Sandbox {
        context: context.to_string(),
        source,
    })?;
    // Before the caller prints anything of its own, so what it prints is printed to a
    // terminal in the modes it started in.
    drop(restore);
    Ok(status.shell_code())
}

/// Carry what is typed at the caller's terminal into the sandbox's, on a thread of its
/// own.
///
/// Detached rather than joined: a read blocked on the caller's terminal ends when the
/// process does, and there is nothing to wait for it to finish — the session's end is
/// the *command* exiting, not the operator stopping typing. It ends on end-of-file (a
/// redirected input that ran out) or on the first write the terminal refuses, and in
/// both cases the session continues, since a command that is no longer being typed at
/// is still running.
fn relay_input(pty: Arc<Pty>, tty: RawFd) {
    std::thread::spawn(move || {
        let mut buf = [0u8; 4096];
        loop {
            let Ok(read) = read_interruptibly(tty, &mut buf) else {
                return;
            };
            if read == 0 {
                return;
            }
            let mut sink: &Pty = &pty;
            if sink.write_all(&buf[..read]).is_err() {
                return;
            }
        }
    });
}

/// The `SIGWINCH` follow: a `signalfd` the signal is read off, rather than a handler.
///
/// A handler would have to reach an `Arc` and issue an `ioctl` from signal context,
/// where neither is something the standard library promises. Reading the signal off a
/// descriptor on an ordinary thread makes both ordinary code. `SIGWINCH` is blocked in
/// [`arm`](Self::arm) so nothing else can take it off the queue first, and a thread
/// spawned afterwards inherits that mask.
struct WinchWatch {
    /// The `signalfd`, readable once per queued `SIGWINCH`.
    fd: OwnedFd,
}

impl WinchWatch {
    /// Block `SIGWINCH` and open the descriptor it will be read off.
    fn arm() -> Result<WinchWatch, EngineError> {
        let fail = |source| EngineError::Terminal {
            context: "watch for terminal resizes",
            source,
        };
        // SAFETY: every call takes an initialized `sigset_t` this frame owns, and the
        // signalfd call takes it by shared reference for the duration of the call only.
        unsafe {
            let mut mask: libc::sigset_t = std::mem::zeroed();
            if libc::sigemptyset(&mut mask) != 0 || libc::sigaddset(&mut mask, libc::SIGWINCH) != 0
            {
                return Err(fail(std::io::Error::last_os_error()));
            }
            // `pthread_sigmask` reports through its return value, not `errno`.
            let masked = libc::pthread_sigmask(libc::SIG_BLOCK, &mask, std::ptr::null_mut());
            if masked != 0 {
                return Err(fail(std::io::Error::from_raw_os_error(masked)));
            }
            let fd = libc::signalfd(-1, &mask, libc::SFD_CLOEXEC);
            if fd < 0 {
                return Err(fail(std::io::Error::last_os_error()));
            }
            Ok(WinchWatch {
                fd: OwnedFd::from_raw_fd(fd),
            })
        }
    }

    /// Resize the sandbox's terminal to match `tty` for as long as the process lives.
    ///
    /// Detached, like the input relay and for the same reason. A resize that fails is
    /// dropped: the session is worth more than the ioctl, and the next resize retries it
    /// from the size the terminal reports then.
    fn follow(self, pty: Arc<Pty>, tty: RawFd) {
        std::thread::spawn(move || {
            let mut buf = [0u8; std::mem::size_of::<libc::signalfd_siginfo>()];
            loop {
                match read_interruptibly(self.fd.as_raw_fd(), &mut buf) {
                    Ok(0) | Err(_) => return,
                    Ok(_) => {
                        if let Some((rows, cols)) = window_size(tty) {
                            let _ = pty.resize(rows, cols);
                        }
                    }
                }
            }
        });
    }
}

/// The caller's terminal in raw mode, restored to the modes it had when this drops.
///
/// Drop rather than an explicit restore at each exit: a session can end at a `?`, and a
/// terminal left in raw mode is one the operator's shell no longer echoes into.
struct RawMode {
    /// The terminal's descriptor.
    fd: RawFd,
    /// The modes to put back.
    saved: libc::termios,
}

impl RawMode {
    /// Read `fd`'s modes and put it in raw mode.
    fn enter(fd: RawFd) -> Result<RawMode, EngineError> {
        let fail = |source| EngineError::Terminal {
            context: "put the terminal in raw mode",
            source,
        };
        // SAFETY: both calls take a `termios` this frame owns, initialized by `tcgetattr`
        // before it is read.
        unsafe {
            let mut saved: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(fd, &mut saved) != 0 {
                return Err(fail(std::io::Error::last_os_error()));
            }
            let mut raw = saved;
            libc::cfmakeraw(&mut raw);
            // `TCSADRAIN`, not `TCSAFLUSH`: input already queued when the session starts
            // is relayed, not discarded. It is input the operator typed *at this
            // session*, and discarding it loses type-ahead — and loses everything, for a
            // session driven from a here-document or a pipe, where all of the input can
            // be queued before the first read.
            if libc::tcsetattr(fd, libc::TCSADRAIN, &raw) != 0 {
                return Err(fail(std::io::Error::last_os_error()));
            }
            Ok(RawMode { fd, saved })
        }
    }
}

impl Drop for RawMode {
    fn drop(&mut self) {
        // SAFETY: the descriptor was a terminal when its modes were read, and `saved` is
        // what it reported. A failure here has no recovery and no reader.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSADRAIN, &self.saved);
        }
    }
}

/// The window size `fd` reports, as (rows, columns), or `None` when it reports none —
/// which is what a terminal opened by something that was itself started without a size
/// answers.
fn window_size(fd: RawFd) -> Option<(u16, u16)> {
    // SAFETY: `TIOCGWINSZ` writes a `winsize` this frame owns; a descriptor that is not
    // a terminal fails rather than writing.
    unsafe {
        let mut size: libc::winsize = std::mem::zeroed();
        (libc::ioctl(fd, libc::TIOCGWINSZ, &mut size) == 0).then_some((size.ws_row, size.ws_col))
    }
}

/// Read from a raw descriptor, retrying the interruption a signal causes.
///
/// The relay's threads sit blocked in `read` for the whole session, which is exactly
/// where an unhandled `EINTR` would end one early.
fn read_interruptibly(fd: RawFd, buf: &mut [u8]) -> std::io::Result<usize> {
    loop {
        // SAFETY: the pointer and length describe `buf`, which this call borrows
        // mutably for its duration.
        let read = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
        if read >= 0 {
            return Ok(read as usize);
        }
        let err = std::io::Error::last_os_error();
        if err.kind() != std::io::ErrorKind::Interrupted {
            return Err(err);
        }
    }
}

/// Write every byte to a raw descriptor, retrying a short write and an interruption.
fn write_all(fd: RawFd, mut buf: &[u8]) -> std::io::Result<()> {
    while !buf.is_empty() {
        // SAFETY: the pointer and length describe `buf`, which this call reads for its
        // duration.
        let written = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
        if written < 0 {
            let err = std::io::Error::last_os_error();
            if err.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        buf = &buf[written as usize..];
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{rk1_build, rk1_lock};

    /// Options for the pure helpers: everything a session needs that is not a root.
    fn opts<'a>(
        stage: ShellStage,
        work_dir: &'a Path,
        argv: &'a [String],
        userspace: &'a [boot2deb_core::model::UserspaceTree],
    ) -> ShellOptions<'a> {
        ShellOptions {
            stage,
            work_dir,
            out_dir: work_dir,
            binds: &[],
            argv,
            env: &[],
            userspace,
            cross_compile: None,
        }
    }

    /// The trees the RK1 fixture's SoC declares, less the optional ones — the set a
    /// plain `build` would compile.
    fn rk1_trees(build: &ResolvedBuild) -> Vec<boot2deb_core::model::UserspaceTree> {
        build
            .image
            .iter()
            .flat_map(|i| &i.userspace)
            .filter(|t| !t.optional)
            .cloned()
            .collect()
    }

    /// Each stage layers the set its own stage module declares, so a session's root is
    /// the compile's root and not one that resembles it. Asserted against the stage
    /// modules' own constants rather than literals — a package added to a stage's
    /// build-dependencies must reach the session without anyone editing this module.
    #[test]
    fn every_stage_layers_what_its_stage_declares() {
        let dir = PathBuf::from("/nonexistent");
        let lock = rk1_lock();
        let build = rk1_build();
        let trees = rk1_trees(&build);
        let layer = |stage| layer_packages(&build, &lock, &opts(stage, &dir, &[], &trees)).unwrap();
        let owned = |v: &[&str]| -> Vec<String> { v.iter().map(|s| (*s).to_string()).collect() };

        assert_eq!(layer(ShellStage::Kernel), owned(kernel::BUILD_DEPS));
        // An out-of-tree module build is a kbuild invocation: the same set, deliberately.
        assert_eq!(layer(ShellStage::Kmod), owned(kernel::BUILD_DEPS));
        assert_eq!(layer(ShellStage::Uboot), owned(uboot::UBOOT_BUILD_DEPS));
        assert_eq!(
            layer(ShellStage::Userspace),
            userspace::layer_packages(&trees)
        );
        assert_eq!(
            layer(ShellStage::Ffmpeg),
            ffmpeg::layer_packages(&trees, false)
        );
        // The packaging root is never layered.
        assert!(layer(ShellStage::Packaging).is_empty());
    }

    /// An optional tree's development packages reach the session exactly when the
    /// userspace stage would have built that tree — the set is an input to the layer,
    /// not only to the compile.
    #[test]
    fn an_optional_trees_build_deps_reach_the_userspace_layer() {
        let dir = PathBuf::from("/nonexistent");
        let lock = rk1_lock();
        let build = rk1_build();
        // Every tree, including the optional ones — what `--userspace libmali` asks for.
        let all: Vec<_> = build
            .image
            .iter()
            .flat_map(|i| &i.userspace)
            .cloned()
            .collect();
        let with = opts(ShellStage::Userspace, &dir, &[], &all);
        assert_eq!(
            layer_packages(&build, &lock, &with).unwrap(),
            userspace::layer_packages(&all)
        );
        assert!(layer_packages(&build, &lock, &with)
            .unwrap()
            .contains(&"libwayland-dev".to_string()));
    }

    /// No two stages share an overlay upper, and none shares one with the build stage of
    /// the same name — a session must not reclaim the directory a build beside it is
    /// compiling in.
    #[test]
    fn every_stage_stages_its_layer_under_its_own_name() {
        let stages = [
            ShellStage::Kernel,
            ShellStage::Uboot,
            ShellStage::Kmod,
            ShellStage::Userspace,
            ShellStage::Ffmpeg,
            ShellStage::Packaging,
        ];
        let mut names: Vec<&str> = stages.iter().map(|s| s.layer_stage()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), stages.len(), "two stages share an upper");
        for stage in stages {
            assert_ne!(
                stage.layer_stage(),
                stage.as_str(),
                "the session shares the build stage's upper"
            );
        }
    }

    /// A session starts in the stage's own tree when the work dir holds one, and in the
    /// work dir when it does not — which is every session opened before that stage has
    /// ever run.
    #[test]
    fn a_session_starts_in_the_stages_tree_when_there_is_one() {
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path();
        assert_eq!(start_dir(&opts(ShellStage::Kernel, work, &[], &[])), work);

        std::fs::create_dir_all(kernel::tree_dir(work)).unwrap();
        assert_eq!(
            start_dir(&opts(ShellStage::Kernel, work, &[], &[])),
            kernel::tree_dir(work)
        );
        // The packaging root owns no tree, so it always starts in the work dir.
        assert_eq!(
            start_dir(&opts(ShellStage::Packaging, work, &[], &[])),
            work
        );
    }

    /// The kernel and kmod sessions carry the kbuild variables their stage compiles
    /// with, so a `make` re-run by hand in there is the `make` the stage ran; the
    /// stages that are not kbuild invocations carry none.
    #[test]
    fn a_kbuild_session_carries_the_kbuild_variables() {
        let build = rk1_build();
        let dir = PathBuf::from("/nonexistent");
        let env = |stage| {
            stage_env(
                &build,
                &opts(stage, &dir, &[], &[]),
                Some(1700),
                Some("aarch64-"),
            )
        };
        let has = |env: &[(String, String)], key: &str, value: &str| {
            env.iter().any(|(k, v)| k == key && v == value)
        };

        let kernel = env(ShellStage::Kernel);
        assert!(has(&kernel, "ARCH", "arm64"));
        assert!(has(&kernel, "SOURCE_DATE_EPOCH", "1700"));
        assert!(has(&kernel, "CROSS_COMPILE", "aarch64-"));
        assert_eq!(env(ShellStage::Kmod), kernel);

        // u-boot cross-compiles but is not kbuild: the toolchain prefix, nothing else.
        let uboot = env(ShellStage::Uboot);
        assert_eq!(uboot.len(), 1);
        assert!(has(&uboot, "CROSS_COMPILE", "aarch64-"));

        // The target-arch roots compile natively for the target; a prefix there would
        // name a toolchain nothing in the root has.
        assert!(env(ShellStage::Userspace).is_empty());
        assert!(env(ShellStage::Ffmpeg).is_empty());
        assert!(env(ShellStage::Packaging).is_empty());
    }

    /// A native cross root — one already at the target's architecture — carries no
    /// prefix, since there is no cross toolchain in it to name.
    #[test]
    fn a_native_root_carries_no_toolchain_prefix() {
        let build = rk1_build();
        let dir = PathBuf::from("/nonexistent");
        let env = stage_env(&build, &opts(ShellStage::Uboot, &dir, &[], &[]), None, None);
        assert!(env.is_empty());
    }

    /// The session's own entries win over the stage's, which is what lets a caller pass
    /// the `TERM` the declared build environment has no reason to carry.
    #[test]
    fn the_callers_environment_is_applied_last() {
        let build = rk1_build();
        let tmp = tempfile::tempdir().unwrap();
        let lock = rk1_lock();
        let env = vec![
            ("TERM".to_string(), "xterm-256color".to_string()),
            ("ARCH".to_string(), "mine".to_string()),
        ];
        let mut o = opts(ShellStage::Kernel, tmp.path(), &[], &[]);
        o.env = &env;
        let resolved = session_env(&build, &lock, &o);
        // Applied over the stage's, so the last entry for a key is the caller's — which
        // is the entry `SandboxRun` applies last too.
        assert_eq!(
            resolved.iter().rev().find(|(k, _)| k == "ARCH"),
            Some(&("ARCH".to_string(), "mine".to_string()))
        );
        assert!(resolved.contains(&("TERM".to_string(), "xterm-256color".to_string())));
    }

    /// With no command named, a session runs a shell; with one, it runs exactly that.
    #[test]
    fn the_default_command_is_a_shell() {
        let dir = PathBuf::from("/nonexistent");
        assert_eq!(
            argv(&opts(ShellStage::Kernel, &dir, &[], &[])),
            [DEFAULT_COMMAND]
        );
        let named = vec!["make".to_string(), "olddefconfig".to_string()];
        assert_eq!(argv(&opts(ShellStage::Kernel, &dir, &named, &[])), named);
    }

    /// Only the ffmpeg root resolves a pool, and only where the build has userspace
    /// packages of its own to resolve out of one.
    #[test]
    fn no_stage_but_ffmpeg_assembles_a_pool() {
        let build = rk1_build();
        let tmp = tempfile::tempdir().unwrap();
        let lock = rk1_lock();
        let sink = |_: crate::event::Event| {};
        let step = Step::start(&sink, "test");
        for stage in [
            ShellStage::Kernel,
            ShellStage::Uboot,
            ShellStage::Kmod,
            ShellStage::Userspace,
            ShellStage::Packaging,
        ] {
            let o = opts(stage, tmp.path(), &[], &[]);
            assert!(ffmpeg_pool(&build, &lock, &o, &step).unwrap().is_none());
        }
        // ffmpeg with no built userspace `.deb`s to resolve names the stage to run
        // first rather than assembling an empty pool. The session has to be told which
        // trees the build compiled, since that is what decides the names it looks for.
        let trees = rk1_trees(&build);
        let o = opts(ShellStage::Ffmpeg, tmp.path(), &[], &trees);
        assert!(matches!(
            ffmpeg_pool(&build, &lock, &o, &step),
            Err(EngineError::ArtifactMissing { .. })
        ));
        // Told nothing, it looks for nothing and assembles no pool — which is right for
        // a SoC that declares no userspace tree at all.
        let none = opts(ShellStage::Ffmpeg, tmp.path(), &[], &[]);
        assert!(ffmpeg_pool(&build, &lock, &none, &step).unwrap().is_none());
    }
}
