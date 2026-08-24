//! Target-arch build sandbox — the environment the userspace and ffmpeg
//! package stages ([`crate::build`]) compile their `.deb`s in.
//!
//! The package stages **always** build inside a [`RootlessSandbox`]: a Debian
//! userland bootstrapped for the build's suite and arch. They never build on the
//! host, not even when the host's arch already matches the target's.
//!
//! The suite, not the arch, is what makes this necessary. These stages emit `.deb`s
//! for the target suite, and `dpkg-shlibdeps` derives each one's runtime `Depends`
//! from the libraries present at build time — it maps every `NEEDED` soname to the
//! package that provides it *here*. Building on the host would link against the
//! host's libraries and stamp the host's package names and versions into `Depends`,
//! producing a `.deb` that does not install in the target rootfs even on a
//! matching-arch host. The sandbox is also the only place the stages can see the
//! build's *own* userspace `.deb`s: ffmpeg links against `librga2`/`librockchip-mpp1`,
//! which this build produces, and `dpkg-shlibdeps` resolves `librga.so.2` to
//! `Depends: librga2` only because that deb — and its `shlibs` — is installed in
//! here ([`BuildSandbox::install_local_debs`]).
//!
//! The sandbox is **unprivileged**: the rootfs is bootstrapped and entered entirely
//! in-process by the pure-Rust [`ferroday_cage`] library — its Debian provisioner
//! resolves, verifies, and lays out the target suite/arch userland with no `sudo`
//! and no external bootstrap binary, and each build command then runs in a cage
//! (fresh namespaces, the rootfs mounted as `/`, the caller mapped to root inside).
//! When the host arch differs from the target's, the target's binaries execute via the
//! host's `qemu-user` binfmt handler — registered with the `F` (fix-binary) flag,
//! so the interpreter is preloaded and nothing is copied into the rootfs; when the
//! arches match they simply run, and `qemu-user` is never consulted. The
//! bootstrapped tree is cached and reused across builds — the base-rootfs cache for
//! the build sandbox — not a per-build throwaway. (The *OS* rootfs that becomes the
//! image is a separate tree, bootstrapped by [`crate::rootfs`].)
//!
//! The sandbox is a rootless *convenience* — a clean, reproducible target-arch
//! userland — not a hard security boundary against malicious build code: it runs
//! as the build user with the build directories bind-mounted read-write. What
//! stops a malicious build script is that every compiled source is pinned to an
//! exact commit by the lock, not the namespace around the compiler.

use crate::bootstrap::{COMPONENTS, DEFAULT_MIRROR};
use crate::build;
use crate::error::EngineError;
use crate::event::{Step, Stream};
use boot2deb_core::provenance::{SandboxMount, SandboxProvenance};
use ferroday_cage::provision::debian::{Debian, DebianEvent, Stream as DebianStream};
use ferroday_cage::provision::{self, Provisioned};
use ferroday_cage::{Cage, Network, Observer, ResolvedMount};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

/// Base packages installed at bootstrap — the minimum to run `dpkg-buildpackage`.
/// Stage-specific build-deps are added later via [`BuildSandbox::install`].
const BASE_DEPS: &[&str] = &[
    "build-essential",
    "ca-certificates",
    "dpkg-dev",
    "debhelper",
    "fakeroot",
    "pkg-config",
];

/// One command to run inside a [`BuildSandbox`].
///
/// `work` is the working directory; it and every path in `binds` are host paths
/// made visible inside the sandbox **at the same absolute path**, so a build that
/// drops artifacts beside its source tree writes them back to the host dir. `env`
/// entries are exported for the command.
pub struct SandboxRun<'a> {
    /// Working directory (a host path, exposed inside at the same path). Must be
    /// `work` itself or lie under one of `binds`/`ro_binds`.
    pub work: &'a Path,
    /// Read-write host paths exposed inside at their host path — where the command
    /// writes artifacts back to the host (a build's output/work dir).
    pub binds: &'a [PathBuf],
    /// Read-only host paths exposed inside at their host path — input-only mounts the
    /// command reads but must not mutate (a directory of `.deb`s apt installs from).
    /// Bound `--ro-bind` so a maintainer script running as sandbox-root cannot write
    /// back into the host dir.
    pub ro_binds: &'a [PathBuf],
    /// Whether the command needs host network (`apt` does; an offline compile does
    /// not). When false the sandbox keeps `--unshare-all`'s fresh network
    /// namespace (loopback only), shrinking a build step's egress surface.
    pub net: bool,
    /// Environment variables exported for the command.
    pub env: &'a [(String, String)],
    /// The command and its arguments (`argv[0]` is the program).
    pub argv: &'a [String],
    /// Human-readable description of the invocation, for errors.
    pub context: &'a str,
}

/// An environment in which target-arch package builds run.
///
/// Implemented by [`RootlessSandbox`] — a userland bootstrapped for the build's
/// suite and arch, bootstrapped and entered entirely in-process by [`ferroday_cage`].
/// A stage drives it through these three operations and is otherwise agnostic to
/// the backend, so another rootfs provider can satisfy the same contract.
pub trait BuildSandbox {
    /// Short label for logs (e.g. `native`, `rootless arm64`).
    fn describe(&self) -> String;

    /// Ensure the environment exists with the base build tooling present.
    /// Idempotent — the cross backend bootstraps and caches an arm64 rootfs on the
    /// first call and reuses it thereafter.
    fn ensure_ready(&self, step: &Step) -> Result<(), EngineError>;

    /// Install additional Debian build-dep `packages` into the environment.
    /// Idempotent (`apt-get` no-ops on already-present packages).
    fn install(&self, packages: &[&str], step: &Step) -> Result<(), EngineError>;

    /// Install local `.deb` files into the environment — the userspace packages a
    /// later stage build-depends on (the ffmpeg stage builds against
    /// `librockchip-mpp-dev` + `librga-dev` and links against `librockchip-mpp1` +
    /// `librga2`). Each deb's directory is bound read-only and `apt-get install` is
    /// given the paths, so apt pulls their transitive deps from the suite.
    ///
    /// This build *produces* those `.deb`s, so installing them here is the only way a
    /// later stage can see them — and the reason `dpkg-shlibdeps` can resolve
    /// `librga.so.2` to `Depends: librga2` at all: it maps the soname through the
    /// `shlibs` of the package installed here that provides it. Without this the
    /// linker would fall back to whatever `librga` the *host* happens to carry, and
    /// `dpkg-shlibdeps` would fail on a library no package in the sandbox owns.
    fn install_local_debs(&self, debs: &[PathBuf], step: &Step) -> Result<(), EngineError>;

    /// Run one command in the environment per `spec`, streaming its output to
    /// `step` and mapping a non-zero exit to
    /// [`CommandFailed`](EngineError::CommandFailed).
    fn run(&self, spec: &SandboxRun, step: &Step) -> Result<(), EngineError>;
}

/// Rootless sandbox: a Debian userland for the build's suite and arch, bootstrapped
/// and entered without root.
///
/// The rootfs is bootstrapped once by [`ferroday_cage`]'s Debian provisioner and
/// reused; each command runs in a [`ferroday_cage::Cage`] with the rootfs mounted
/// as `/`. On a cross host the target's binaries execute via the `F`-flagged
/// `qemu-user` binfmt handler with no interpreter copy; on a matching-arch host they
/// run directly. See
/// the [module docs](self) for why the package stages always build in here rather
/// than on the host.
pub struct RootlessSandbox {
    /// Target-arch rootfs directory — bootstrapped once, reused across builds (the
    /// seed of the base-rootfs cache).
    rootfs: PathBuf,
    /// Debian suite to bootstrap (e.g. `forky`).
    suite: String,
    /// Debian architecture to bootstrap (e.g. `arm64`).
    arch: String,
    /// Mirror URL the rootfs is bootstrapped from.
    mirror: String,
    /// Debian archive keyring verifying the suite's `Release` signature. `None`
    /// falls back to the host apt trust store (only works on a Debian host); a
    /// vendored keyring makes the bootstrap portable to non-Debian hosts.
    keyring: Option<PathBuf>,
}

impl RootlessSandbox {
    /// A sandbox rooted at `rootfs`, bootstrapping `suite`/`arch` from the default
    /// Debian mirror, verifying the archive with `keyring` (recommended; `None`
    /// uses the host apt trust store).
    pub fn new(
        rootfs: PathBuf,
        suite: impl Into<String>,
        arch: impl Into<String>,
        keyring: Option<PathBuf>,
    ) -> Self {
        RootlessSandbox {
            rootfs,
            suite: suite.into(),
            arch: arch.into(),
            mirror: DEFAULT_MIRROR.to_string(),
            keyring,
        }
    }

    /// Run one `apt-get` invocation inside the sandbox with **direct argv** (no
    /// `sh -c`), so package names and `.deb` paths cannot be reinterpreted by a
    /// shell. `fixed` is the subcommand + flags, `extra` the package names
    /// or paths, `ro_binds` any host dirs apt must *read* from (bound read-only —
    /// apt installs from them but never writes them).
    ///
    /// apt needs host network to fetch, so this run shares the net; `-o
    /// APT::Sandbox::User=root` keeps apt from dropping to the `_apt` user for
    /// downloads: that uid is not mapped in the single-uid bootstrap namespace, so
    /// the drop would fail with `seteuid`. `DEBIAN_FRONTEND` comes from the sandbox
    /// env ([`SANDBOX_ENV`]).
    fn apt(
        &self,
        fixed: &[&str],
        extra: &[String],
        ro_binds: &[PathBuf],
        context: &str,
        step: &Step,
    ) -> Result<(), EngineError> {
        let mut argv = vec![
            "apt-get".to_string(),
            "-o".to_string(),
            "APT::Sandbox::User=root".to_string(),
        ];
        argv.extend(fixed.iter().map(|s| s.to_string()));
        argv.extend(extra.iter().cloned());
        let spec = SandboxRun {
            work: Path::new("/"),
            binds: &[],
            ro_binds,
            net: true,
            env: &[],
            argv: &argv,
            context,
        };
        self.run(&spec, step)
    }
}

impl BuildSandbox for RootlessSandbox {
    fn describe(&self) -> String {
        format!("rootless {}", self.arch)
    }

    fn ensure_ready(&self, step: &Step) -> Result<(), EngineError> {
        // A published rootfs is a plain directory; `provision::ensure` fast-paths on
        // that and skips the bootstrap, so this is idempotent across builds.
        step.log(format!(
            "ensuring {} {} rootfs at {} (in-process Debian provisioner)",
            self.arch,
            self.suite,
            self.rootfs.display()
        ));
        // The provisioner resolves the whole install closure — the base system (apt
        // included) plus `BASE_DEPS` and their dependencies — with its own resolver,
        // verifies the archive signature against the keyring, lays out and configures
        // the packages in an unprivileged cage, and writes an apt-usable rootfs (the
        // keyring, a `signed-by` sources line for every component, and an apt
        // sandbox-user posture matching the single-identity map). The later
        // `install`/`install_local_debs` build-time apt runs read those sources.
        let mut builder = Debian::builder(&self.suite)
            .architecture(&self.arch)
            .mirror(&self.mirror)
            .components(COMPONENTS.split(','))
            .include(BASE_DEPS.iter().copied());
        // A vendored keyring makes the bootstrap portable to a non-Debian host;
        // without one the provisioner falls back to its embedded Debian archive
        // keyring.
        if let Some(keyring) = &self.keyring {
            builder = builder.keyring(keyring);
        }
        let mut debian = builder.build().map_err(|source| EngineError::Bootstrap {
            context: format!("configure the {} {} bootstrap", self.arch, self.suite),
            message: source.to_string(),
        })?;
        // The sink is bound for this one run rather than for the provisioner's life,
        // so its borrow of `step` ends when `ensure` returns.
        let mut sink = |event: DebianEvent<'_>| forward_bootstrap_event(step, event);
        let outcome =
            provision::ensure(&self.rootfs, &mut debian.observe(&mut sink)).map_err(|source| {
                EngineError::Bootstrap {
                    context: format!("bootstrap the {} {} rootfs", self.arch, self.suite),
                    message: source.to_string(),
                }
            })?;
        // `Existing` means a prior build already published this rootfs; anything
        // else means this call produced it.
        if outcome == Provisioned::Existing {
            step.log(format!("reusing {} rootfs at {}", self.arch, self.rootfs.display()));
        } else {
            step.log(format!("{} rootfs ready at {}", self.arch, self.rootfs.display()));
        }
        Ok(())
    }

    fn install(&self, packages: &[&str], step: &Step) -> Result<(), EngineError> {
        if packages.is_empty() {
            return Ok(());
        }
        step.log(format!("installing build deps: {}", packages.join(" ")));
        self.apt(&["update", "-q"], &[], &[], "apt-get update", step)?;
        let pkgs: Vec<String> = packages.iter().map(|p| p.to_string()).collect();
        self.apt(
            &["install", "-y", "--no-install-recommends"],
            &pkgs,
            &[],
            "apt-get install build deps",
            step,
        )
    }

    fn install_local_debs(&self, debs: &[PathBuf], step: &Step) -> Result<(), EngineError> {
        if debs.is_empty() {
            return Ok(());
        }
        // Read-only-bind each deb's directory so apt can read the files at their host
        // path inside the sandbox without being able to write back into it
        // (deduplicated — the userspace debs share one dir).
        let mut ro_binds: Vec<PathBuf> = Vec::new();
        for deb in debs {
            if let Some(parent) = deb.parent() {
                let p = parent.to_path_buf();
                if !ro_binds.contains(&p) {
                    ro_binds.push(p);
                }
            }
        }
        // apt treats an argument containing a slash as a file path; passing the
        // absolute paths as direct argv (no shell) lets apt resolve transitive
        // runtime deps from the suite while a path with shell metacharacters cannot
        // be reinterpreted.
        step.log(format!("installing {} userspace .deb(s) into the sandbox", debs.len()));
        self.apt(&["update", "-q"], &[], &ro_binds, "apt-get update", step)?;
        let paths: Vec<String> = debs.iter().map(|d| d.to_string_lossy().into_owned()).collect();
        self.apt(
            &["install", "-y", "--no-install-recommends"],
            &paths,
            &ro_binds,
            "apt-get install userspace debs",
            step,
        )
    }

    fn run(&self, spec: &SandboxRun, step: &Step) -> Result<(), EngineError> {
        let cage = self.cage(spec).build().map_err(|source| EngineError::Sandbox {
            context: spec.context.to_string(),
            source,
        })?;
        let mut observer = StepObserver::new(step);
        let status = cage
            .run_with(&mut observer)
            .map_err(|source| EngineError::Sandbox {
                context: spec.context.to_string(),
                source,
            })?;
        observer.flush();
        if status.success() {
            Ok(())
        } else {
            Err(EngineError::CommandFailed {
                command: spec.argv[0].clone(),
                context: spec.context.to_string(),
                status: status.code(),
                stderr: observer.stderr_tail(),
            })
        }
    }
}

impl RootlessSandbox {
    /// Build the [`Cage`] that enters [`rootfs`](Self::rootfs) and runs `spec`.
    ///
    /// The [`baseline`] profile plus what this one run adds to it. [`Network::Host`]
    /// shares the host network only when `spec.net` is set (an `apt` run needs it); an
    /// offline compile keeps the default [`Network::Isolated`] namespace (loopback
    /// only), shrinking a build step's egress surface. Per-run `spec.env` entries
    /// override [`SANDBOX_ENV`] on collision. Each read-write `bind` and each read-only
    /// `ro_bind` is exposed at its host path so artifacts written beside a source tree
    /// land back on the host while input-only mounts stay unwritable.
    fn cage(&self, spec: &SandboxRun) -> ferroday_cage::CageBuilder {
        let mut builder = baseline(&self.rootfs)
            .command(&spec.argv[0])
            .args(&spec.argv[1..])
            .network(if spec.net { Network::Host } else { Network::Isolated })
            .current_dir(spec.work);
        for (key, value) in spec.env {
            builder = builder.env(key, value);
        }
        for bind in spec.binds {
            builder = builder.bind(bind, bind);
        }
        for bind in spec.ro_binds {
            builder = builder.bind_ro(bind, bind);
        }
        builder
    }
}

/// The sandbox profile **every** boot2deb cage runs under, over `rootfs`: the package
/// stages here and the OS rootfs customize in [`crate::rootfs`] both start from this and
/// add only their own command, network, and binds.
///
/// The rootfs is mounted as `/` and the cage's managed mounts give the build a working
/// `/proc`, a minimal `/dev`, and a `/tmp`. `base_env(false)` makes the command's
/// environment exactly [`SANDBOX_ENV`], with nothing composed underneath, so what a
/// compile sees is a function of this file alone: a variable the library adds to its own
/// base in a later release cannot reach a build.
///
/// The identity map is left at the library default, which maps the caller to root inside
/// — `dpkg`/`dpkg-buildpackage` require it. The rootfs customize swaps in the subordinate
/// map, which is the one thing about it that is not this profile: it needs a range of ids
/// to give the provisioned tree its real ownership.
///
/// One definition, because [`resolved_inputs`] records what it resolves to as the
/// image's provenance: a second site configuring the same profile by hand could drift
/// from the record without either changing.
pub(crate) fn baseline(rootfs: &Path) -> ferroday_cage::CageBuilder {
    let mut builder = Cage::builder()
        .rootfs(rootfs)
        // Declared, not inherited: see the note above.
        .base_env(false)
        // The stages pass bare tool names (`dpkg-buildpackage`, `make`, `apt-get`);
        // the cage resolves them against SANDBOX_ENV's `PATH` inside the rootfs, like
        // a shell.
        .path_lookup(true);
    for (key, value) in SANDBOX_ENV {
        builder = builder.env(key, value);
    }
    builder
}

/// Where a build root's overlay upper layer is created, for a build whose scratch tree
/// is `work_dir`.
///
/// Beside the sandbox base it overlays, and deliberately **not** under `TMPDIR`. An
/// unprivileged overlay records its whiteouts and opaque markers in `user.*` extended
/// attributes, which a tmpfs older than Linux 6.6 cannot hold — so an upper placed in a
/// tmpfs `TMPDIR` fails on a host whose work dir would have carried it. Which
/// filesystem this lands on is therefore a host requirement, and
/// [`overlay_check`](crate::checks::overlay_check) probes this directory rather than
/// `/tmp` for exactly that reason.
pub fn build_root_uppers(work_dir: &Path) -> PathBuf {
    work_dir.join("sandbox").join("layers")
}

/// The sandbox profile as provenance data: the environment and the mounts every
/// sandboxed build command runs under.
///
/// Resolved from the profile the stages themselves run in rather than restated, so the
/// record reports what a command actually sees — including the six `/dev` device nodes
/// and five `/dev` symlinks, which the sandbox library establishes and which no accessor
/// other than [`Cage::resolved_inputs`](ferroday_cage::Cage::resolved_inputs) reports.
///
/// The profile is a function of the builder configuration alone — no mount in it names
/// the rootfs — so it resolves against an empty stand-in root. That is what makes the
/// record the same for every build: a base image bootstraps no build-sandbox rootfs at
/// all, and its provenance still has to state the profile its rootfs customize ran under.
///
/// A run's own additions are deliberately outside the record: its working and artifact
/// binds are per-build paths, and the host `/etc/resolv.conf` an `apt` run binds is a
/// host path. Both would make the record a property of the machine rather than of the
/// builder.
pub fn resolved_inputs() -> Result<SandboxProvenance, EngineError> {
    let scratch = tempfile::Builder::new()
        .prefix("boot2deb-sandbox-profile-")
        .tempdir()
        .map_err(|source| EngineError::io(&std::env::temp_dir(), source))?;
    let cage = baseline(scratch.path())
        // A command is required to freeze a launch plan and contributes nothing to the
        // environment or the mounts, so any resolvable name serves.
        .command("true")
        .build()
        .map_err(|source| EngineError::Sandbox {
            context: "resolve the sandbox profile".into(),
            source,
        })?;
    Ok(project(cage.resolved_inputs()))
}

/// Project the sandbox library's resolved inputs onto the manifest's shape.
fn project(inputs: ferroday_cage::ResolvedInputs) -> SandboxProvenance {
    SandboxProvenance {
        // Every variable is declared by SANDBOX_ENV from `&str` constants, so the lossy
        // conversion is exact for the environment this profile carries.
        env: inputs
            .env
            .iter()
            .map(|(name, value)| {
                (name.to_string_lossy().into_owned(), value.to_string_lossy().into_owned())
            })
            .collect(),
        mounts: inputs.mounts.iter().map(project_mount).collect(),
    }
}

/// Project one resolved mount onto the manifest's one flat shape: `target` is always
/// where the mount is established inside the sandbox, `source` always what is exposed
/// there.
///
/// Each arm names every field of its variant rather than eliding the rest with `..` —
/// the opposite of [`forward_bootstrap_event`]'s deliberate `..`, and for the opposite
/// reason. A progress stream may drop a milestone it has no line for; a record whose
/// value is being complete may not drop an input, so a field added to a variant is a
/// compile error here rather than a silent omission from every later manifest.
fn project_mount(mount: &ResolvedMount) -> SandboxMount {
    let base = |kind: &str| SandboxMount {
        kind: kind.to_string(),
        target: mount.get_target().display().to_string(),
        source: None,
        fstype: None,
        flags: None,
        options: None,
        read_only: None,
    };
    match mount {
        ResolvedMount::Tmpfs { target: _, flags, data } => SandboxMount {
            flags: Some(hex_flags(*flags)),
            options: data_string(data),
            ..base("tmpfs")
        },
        ResolvedMount::Procfs { target: _ } => base("procfs"),
        ResolvedMount::Devpts { target: _, data } => SandboxMount {
            options: data_string(data),
            ..base("devpts")
        },
        ResolvedMount::Bind { source, target: _, read_only } => SandboxMount {
            source: Some(source.display().to_string()),
            read_only: Some(*read_only),
            ..base("bind")
        },
        ResolvedMount::Raw { source, target: _, fstype, flags, data } => SandboxMount {
            source: source.as_ref().map(|p| p.display().to_string()),
            fstype: fstype.clone(),
            flags: Some(hex_flags(*flags)),
            options: data.as_deref().and_then(data_string),
            ..base("raw")
        },
        // `get_target()` gives the link itself; the variant's own `target` is what the
        // link points at, which is what this shape calls a source.
        ResolvedMount::Symlink { path: _, target } => SandboxMount {
            source: Some(target.display().to_string()),
            ..base("symlink")
        },
        // The enum is `#[non_exhaustive]`, so a kind this release cannot name is still
        // recorded — by the one thing every mount has — rather than dropped.
        _ => base("unknown"),
    }
}

/// One `MS_*` flag word in the manifest's form: `0x`-prefixed and 8 hex digits, matching
/// how the filesystem pin renders a feature word.
fn hex_flags(flags: u64) -> String {
    format!("{flags:#010x}")
}

/// A mount's filesystem data string, or `None` where it carries none: the library
/// freezes an unset one as the empty string, and `options = ""` would read as a value
/// rather than an absence.
fn data_string(data: &str) -> Option<String> {
    (!data.is_empty()).then(|| data.to_string())
}

/// The environment for every sandbox command. With `base_env(false)` on the builder
/// these entries and the per-run `spec.env` are the whole of it — the host env never
/// leaks in (reproducibility, and it avoids `dpkg`/`perl` reading the host
/// `HOME`/locale), and neither does the cage library's own base. Per-run `spec.env`
/// entries are applied afterwards and override these. `TZ=UTC` and `LC_ALL=C.UTF-8`
/// pin timezone and locale so packaged timestamps/collation do not vary with the
/// build host; the host-side [`build::run`](crate::build::run) normalizes the same
/// two vars.
const SANDBOX_ENV: &[(&str, &str)] = &[
    ("PATH", "/usr/sbin:/usr/bin:/sbin:/bin"),
    ("HOME", "/root"),
    ("LC_ALL", "C.UTF-8"),
    ("TZ", "UTC"),
    ("DEBIAN_FRONTEND", "noninteractive"),
];

/// Relay one Debian-provisioner [`DebianEvent`] to a [`Step`]: the fetch/resolve/
/// download/extract milestones as informational log lines, and a configuring
/// dpkg wave's raw output on its own stream. The dpkg bytes are not
/// line-buffered, so each chunk is logged as one lossy line — good enough for a
/// bootstrap progress channel, which is not the reproducible build output.
pub(crate) fn forward_bootstrap_event(step: &Step, event: DebianEvent<'_>) {
    match event {
        DebianEvent::Fetching { url, .. } => step.log(format!("fetching {url}")),
        DebianEvent::Resolving => step.log("resolving the package set"),
        // The closure the bootstrap itself resolved and is about to install — the
        // build sandbox's only record of what it contains, and the rootfs node's
        // manifest source.
        DebianEvent::Resolved { plan, .. } => {
            step.log(format!("bootstrap resolved {} packages", plan.packages.len()))
        }
        DebianEvent::Downloading { package, index, total, .. } => {
            step.log(format!("downloading {package} ({index}/{total})"))
        }
        DebianEvent::Extracting { package, .. } => step.log(format!("extracting {package}")),
        DebianEvent::CommandOutput { stream, bytes, .. } => {
            let text = String::from_utf8_lossy(bytes);
            let text = text.trim_end_matches(['\n', '\r']);
            if !text.is_empty() {
                let stream = match stream {
                    DebianStream::Stderr => Stream::Stderr,
                    // Stdout and any future stream default to the stdout tag.
                    _ => Stream::Stdout,
                };
                step.emit(stream, text.to_string());
            }
        }
        // Two levels of openness, and each needs its own escape. The wildcard absorbs a
        // future *variant*; the `..` in every struct arm above absorbs a future *field*
        // within a variant already matched here. Both are load-bearing: a milestone this
        // build has no line for is silently ignored rather than breaking the match.
        _ => {}
    }
}

/// An [`Observer`] that relays a cage command's captured output to a [`Step`],
/// line by line, matching the host-side [`build::run`](crate::build::run)
/// behavior: each stdout/stderr line becomes an [`Event::Log`](crate::event::Event)
/// as it is produced, and the last [`STDERR_TAIL`](crate::build::STDERR_TAIL)
/// stderr lines are retained for a [`CommandFailed`](EngineError::CommandFailed)
/// message.
///
/// The cage delivers raw byte chunks whose boundaries carry no meaning, so this
/// buffers each stream and splits on newlines; [`flush`](Self::flush) emits any
/// trailing partial line after the command exits.
///
/// Shared with the OS rootfs provisioner backend ([`crate::rootfs::ProvisionerRootfs`]),
/// whose post-bootstrap customize steps run in cages too.
pub(crate) struct StepObserver<'a> {
    step: &'a Step<'a>,
    stdout_buf: Vec<u8>,
    stderr_buf: Vec<u8>,
    stderr_tail: VecDeque<String>,
}

impl<'a> StepObserver<'a> {
    /// A fresh observer relaying to `step`.
    pub(crate) fn new(step: &'a Step<'a>) -> Self {
        StepObserver {
            step,
            stdout_buf: Vec::new(),
            stderr_buf: Vec::new(),
            stderr_tail: VecDeque::with_capacity(build::STDERR_TAIL),
        }
    }

    /// Emit one complete line, retaining stderr lines in the tail buffer.
    fn emit_line(&mut self, stream: Stream, line: String) {
        if stream == Stream::Stderr {
            if self.stderr_tail.len() == build::STDERR_TAIL {
                self.stderr_tail.pop_front();
            }
            self.stderr_tail.push_back(line.clone());
        }
        self.step.emit(stream, line);
    }

    /// Append `chunk` to `stream`'s buffer and emit every newly complete line.
    fn ingest(&mut self, stream: Stream, chunk: &[u8]) {
        match stream {
            Stream::Stdout => self.stdout_buf.extend_from_slice(chunk),
            Stream::Stderr => self.stderr_buf.extend_from_slice(chunk),
        }
        loop {
            let newline = {
                let buf = match stream {
                    Stream::Stdout => &self.stdout_buf,
                    Stream::Stderr => &self.stderr_buf,
                };
                buf.iter().position(|&b| b == b'\n')
            };
            let Some(pos) = newline else { break };
            let mut line: Vec<u8> = {
                let buf = match stream {
                    Stream::Stdout => &mut self.stdout_buf,
                    Stream::Stderr => &mut self.stderr_buf,
                };
                buf.drain(..=pos).collect()
            };
            // Strip the trailing newline (and a CR before it, if any).
            while matches!(line.last(), Some(b'\n' | b'\r')) {
                line.pop();
            }
            self.emit_line(stream, String::from_utf8_lossy(&line).into_owned());
        }
    }

    /// Emit any trailing bytes not terminated by a newline (a command whose last
    /// line has no final `\n`). Call once after the command exits.
    pub(crate) fn flush(&mut self) {
        for stream in [Stream::Stdout, Stream::Stderr] {
            let mut line = match stream {
                Stream::Stdout => std::mem::take(&mut self.stdout_buf),
                Stream::Stderr => std::mem::take(&mut self.stderr_buf),
            };
            if line.is_empty() {
                continue;
            }
            while matches!(line.last(), Some(b'\r')) {
                line.pop();
            }
            self.emit_line(stream, String::from_utf8_lossy(&line).into_owned());
        }
    }

    /// The retained stderr tail, joined for a failure message.
    pub(crate) fn stderr_tail(&self) -> String {
        self.stderr_tail.iter().cloned().collect::<Vec<_>>().join("\n")
    }
}

impl Observer for StepObserver<'_> {
    fn stdout(&mut self, chunk: &[u8]) {
        self.ingest(Stream::Stdout, chunk);
    }

    fn stderr(&mut self, chunk: &[u8]) {
        self.ingest(Stream::Stderr, chunk);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn step_observer_splits_lines_and_keeps_a_stderr_tail() {
        use crate::event::{Event, EventSink};
        use std::cell::RefCell;

        // A sink that records every emitted (stream, line) so the observer's line
        // splitting and stderr-tail retention can be asserted.
        #[derive(Default)]
        struct Recorder(RefCell<Vec<(Stream, String)>>);
        impl EventSink for Recorder {
            fn emit(&self, event: Event) {
                if let Event::Log { stream, line, .. } = event {
                    self.0.borrow_mut().push((stream, line));
                }
            }
        }

        let sink = Recorder::default();
        let step = Step::start(&sink, "cage");
        let mut obs = StepObserver::new(&step);

        // Chunks split mid-line: "one\ntw" then "o\n" must yield lines "one","two".
        obs.stdout(b"one\ntw");
        obs.stdout(b"o\n");
        // A stderr line with no trailing newline is emitted by flush().
        obs.stderr(b"warn: bad\ntrailing-no-newline");
        obs.flush();

        let events = sink.0.borrow();
        assert!(events.contains(&(Stream::Stdout, "one".to_string())));
        assert!(events.contains(&(Stream::Stdout, "two".to_string())));
        assert!(events.contains(&(Stream::Stderr, "warn: bad".to_string())));
        assert!(events.contains(&(Stream::Stderr, "trailing-no-newline".to_string())));
        // The failure tail carries the stderr lines, in order, and no stdout.
        assert_eq!(obs.stderr_tail(), "warn: bad\ntrailing-no-newline");
    }

    #[test]
    fn describe_names_the_target_arch() {
        let sb = RootlessSandbox::new(PathBuf::from("/w/rootfs"), "forky", "arm64", None);
        assert_eq!(sb.describe(), "rootless arm64");
    }

    /// The recorded profile is what the manifest claims it is: the declared environment
    /// entire, and a mount set that names nothing belonging to the build host — so two
    /// manifests differing here differ because the *builder* changed, not because the
    /// machines did.
    #[test]
    fn the_recorded_profile_is_declared_and_host_independent() {
        let profile = resolved_inputs().expect("the profile resolves");

        // `base_env(false)` in force: SANDBOX_ENV is the whole environment, with nothing
        // composed underneath it. An entry appearing here that this file does not
        // declare means the library's own base reached a compile.
        assert_eq!(profile.env.len(), SANDBOX_ENV.len());
        for (key, value) in SANDBOX_ENV {
            assert_eq!(profile.env.get(*key).map(String::as_str), Some(*value));
        }

        // The only host paths the profile exposes are the six /dev character devices,
        // which cannot be created inside an unprivileged namespace. Anything else — the
        // stand-in root this resolved against above all — would make the record a
        // property of the machine that built the image.
        for mount in &profile.mounts {
            assert!(
                mount.target.starts_with('/'),
                "mount target is not a path inside the sandbox: {mount:?}"
            );
            if mount.kind == "bind" {
                let source = mount.source.as_deref().expect("a bind names its source");
                assert!(source.starts_with("/dev/"), "the profile binds a host path: {source}");
            }
        }

        // The five /dev symlinks, which no other accessor reports and which a consumer
        // cannot re-create by hand — the reason the mount half is the payload.
        let links: Vec<&str> = profile
            .mounts
            .iter()
            .filter(|m| m.kind == "symlink")
            .map(|m| m.target.as_str())
            .collect();
        for link in ["/dev/stdin", "/dev/stdout", "/dev/stderr", "/dev/fd", "/dev/ptmx"] {
            assert!(links.contains(&link), "missing {link} in {links:?}");
        }
    }

    /// The flat shape carries every kind's parameters: `raw` is the widest, and the one
    /// kind whose source, filesystem type, and data string are each optional.
    #[test]
    fn a_raw_mount_projects_its_whole_parameter_set() {
        let full = project_mount(&ResolvedMount::Raw {
            source: Some(PathBuf::from("tmpfs")),
            target: PathBuf::from("/run"),
            fstype: Some("tmpfs".to_string()),
            flags: 6,
            data: Some("mode=0755".to_string()),
        });
        assert_eq!(full.kind, "raw");
        assert_eq!(full.target, "/run");
        assert_eq!(full.source.as_deref(), Some("tmpfs"));
        assert_eq!(full.fstype.as_deref(), Some("tmpfs"));
        // Hex, because the word is a bit set and only hex diffs one bit at a time.
        assert_eq!(full.flags.as_deref(), Some("0x00000006"));
        assert_eq!(full.options.as_deref(), Some("mode=0755"));

        // An unset parameter is absent from the record; a zero flag word is not unset,
        // so it is recorded rather than dropped.
        let bare = project_mount(&ResolvedMount::Raw {
            source: None,
            target: PathBuf::from("/run"),
            fstype: None,
            flags: 0,
            data: None,
        });
        assert_eq!(bare.source, None);
        assert_eq!(bare.fstype, None);
        assert_eq!(bare.options, None);
        assert_eq!(bare.flags.as_deref(), Some("0x00000000"));
    }
}
