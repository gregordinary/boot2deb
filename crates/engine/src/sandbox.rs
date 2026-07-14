//! Target-arch build sandboxes — the environment the userspace and ffmpeg
//! package stages ([`crate::build`]) compile their arm64 `.deb`s in.
//!
//! A [`BuildSandbox`] hides one difference: on a host whose arch already matches
//! the target the build runs directly ([`NativeSandbox`]); cross-building (e.g.
//! arm64 on x86_64) runs inside an arm64 Debian userland ([`RootlessSandbox`]).
//! Both reach the same three-operation contract — bootstrap, install build-deps,
//! run a command with host paths visible — so the stage code reads identically on
//! either path.
//!
//! The cross path is **unprivileged**: the rootfs is bootstrapped with
//! `mmdebstrap --mode=unshare` (user namespaces, no `sudo`) and entered with
//! `bwrap`, under which arm64 binaries execute via the host's `qemu-user` binfmt
//! handler — registered with the `F` (fix-binary) flag, so the interpreter is
//! preloaded and nothing is copied into the rootfs. This is deliberately the same
//! rootless-userland machinery the rootfs assembly is built on: the
//! bootstrapped tree is the seed of the base-rootfs cache, not a throwaway.
//!
//! The sandbox is a rootless *convenience* — a clean, reproducible target-arch
//! userland — not a hard security boundary against malicious build code: it runs
//! as the build user with the build directories bind-mounted read-write. What
//! stops a malicious build script is that every compiled source is pinned to an
//! exact commit by the lock, not the namespace around the compiler.

use crate::bootstrap::{StagingRoot, COMPONENTS, DEFAULT_MIRROR};
use crate::build;
use crate::error::EngineError;
use crate::event::Step;
use std::path::{Path, PathBuf};
use std::process::Command;

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
    /// command reads but must not mutate (a directory of `.deb`s apt installs from,
    /// TRUST-6). Bound `--ro-bind` so a maintainer script running as sandbox-root
    /// cannot write back into the host dir.
    pub ro_binds: &'a [PathBuf],
    /// Whether the command needs host network (`apt` does; an offline compile does
    /// not, TRUST-6). When false the sandbox keeps `--unshare-all`'s fresh network
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
/// Implemented by [`NativeSandbox`] (host arch = target) and [`RootlessSandbox`]
/// (cross, via `mmdebstrap` + `bwrap` + `qemu-user`). A stage selects one from
/// [`preflight`](crate::preflight) and drives it through these three operations;
/// the backend is otherwise opaque, so the rootfs sandbox can satisfy the
/// same contract.
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
    /// later stage build-depends on (the ffmpeg stage needs the `librockchip-mpp-dev`
    /// and `librga-dev` packages). Native is a no-op: host deps are the
    /// caller's responsibility (checked by `doctor`), so the userspace
    /// `.deb`s must already be installed. The cross backend binds each deb's
    /// directory and `apt-get install`s the paths, letting apt pull transitive deps.
    fn install_local_debs(&self, debs: &[PathBuf], step: &Step) -> Result<(), EngineError>;

    /// Run one command in the environment per `spec`, streaming its output to
    /// `step` and mapping a non-zero exit to
    /// [`CommandFailed`](EngineError::CommandFailed).
    fn run(&self, spec: &SandboxRun, step: &Step) -> Result<(), EngineError>;
}

/// Host-native sandbox: the host arch already matches the target, so commands run
/// directly on the host with no rootfs and no emulation.
///
/// A native build does not mutate the host, so [`ensure_ready`](BuildSandbox::ensure_ready)
/// and [`install`](BuildSandbox::install) are no-ops — host build-deps are the
/// caller's responsibility (checked by `doctor`), and a missing one surfaces
/// as the build subprocess's own error.
pub struct NativeSandbox;

impl BuildSandbox for NativeSandbox {
    fn describe(&self) -> String {
        "native".to_string()
    }

    fn ensure_ready(&self, _step: &Step) -> Result<(), EngineError> {
        Ok(())
    }

    fn install(&self, _packages: &[&str], _step: &Step) -> Result<(), EngineError> {
        Ok(())
    }

    fn install_local_debs(&self, _debs: &[PathBuf], _step: &Step) -> Result<(), EngineError> {
        Ok(())
    }

    fn run(&self, spec: &SandboxRun, step: &Step) -> Result<(), EngineError> {
        let (prog, rest) = spec
            .argv
            .split_first()
            .expect("sandbox run argv is non-empty");
        let mut cmd = Command::new(prog);
        cmd.args(rest).current_dir(spec.work);
        for (k, v) in spec.env {
            cmd.env(k, v);
        }
        build::run(cmd, prog, spec.context, step)
    }
}

/// Cross-arch rootless sandbox: an arm64 Debian userland bootstrapped and
/// entered without root.
///
/// The rootfs is created once with `mmdebstrap --mode=unshare` and reused; each
/// command runs under `bwrap` with the rootfs bound as `/`, so arm64 binaries
/// execute via the host's `F`-flagged `qemu-user` binfmt handler with no
/// interpreter copy. See the [module docs](self).
pub struct RootlessSandbox {
    /// arm64 rootfs directory — bootstrapped once, reused across builds (the seed
    /// of the base-rootfs cache).
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

    /// True once the rootfs has been fully bootstrapped. The tarball is extracted
    /// into a sibling `.partial` dir and renamed into place atomically, so the
    /// `rootfs` dir only ever exists complete — an interrupted extraction leaves the
    /// `.partial` behind (cleared on the next run), never a half-populated `rootfs`
    /// a later build would wrongly reuse.
    fn is_bootstrapped(&self) -> bool {
        self.rootfs.join("usr/bin").is_dir()
    }

    /// Sibling dir the tarball extracts into before the atomic rename into
    /// [`rootfs`](Self::rootfs).
    fn partial_rootfs(&self) -> PathBuf {
        self.rootfs.with_extension("partial")
    }

    /// Path of the intermediate bootstrap tarball (a sibling of the rootfs dir).
    /// `mmdebstrap` writes a tarball rather than a directory: in `--mode=unshare`
    /// it opens the output file as the invoking user *before* unsharing, sidestepping
    /// the in-namespace mapped-subuid write restriction that blocks a directory
    /// target under a user-owned parent.
    fn tarball_path(&self) -> PathBuf {
        self.rootfs.with_extension("tar")
    }

    /// `mmdebstrap` argv for the bootstrap — pure, so the (long) invocation is
    /// testable. `target` is the output tarball; `keyring` is the staged,
    /// namespace-readable keyring path (if any).
    fn mmdebstrap_argv(&self, target: &str, keyring: Option<&str>) -> Vec<String> {
        let mut argv = vec![
            "--mode=unshare".to_string(),
            format!("--arch={}", self.arch),
            "--variant=minbase".to_string(),
            format!("--components={COMPONENTS}"),
        ];
        // Bound apt's per-connection network wait so a stalled mirror fails rather
        // than hangs (TRUST-5).
        argv.extend(crate::bootstrap::APT_TIMEOUT_OPTS.iter().map(|s| s.to_string()));
        if let Some(kr) = keyring {
            argv.push(format!("--keyring={kr}"));
        }
        argv.push(format!("--include={}", BASE_DEPS.join(",")));
        // `--` stops option parsing so the positional suite/target/mirror cannot be
        // read as options even if a value begins with `-` (SUB-2).
        argv.push("--".to_string());
        argv.push(self.suite.clone());
        argv.push(target.to_string());
        argv.push(self.mirror.clone());
        argv
    }

    /// Run one `apt-get` invocation inside the sandbox with **direct argv** (no
    /// `sh -c`), so package names and `.deb` paths cannot be reinterpreted by a
    /// shell. `fixed` is the subcommand + flags, `extra` the package names
    /// or paths, `ro_binds` any host dirs apt must *read* from (bound read-only —
    /// apt installs from them but never writes them, TRUST-6).
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
        if self.is_bootstrapped() {
            step.log(format!("reusing {} rootfs at {}", self.arch, self.rootfs.display()));
            return Ok(());
        }
        if let Some(parent) = self.rootfs.parent() {
            std::fs::create_dir_all(parent).map_err(|source| EngineError::io(parent, source))?;
        }
        // The keyring is read by `mmdebstrap`'s in-namespace apt, which runs as a
        // mapped subuid: stage it into a private, world-traversable temp dir since
        // the work dir's ancestors are typically not traversable by that user. The
        // staging root is removed when `staged` drops, after the bootstrap.
        let staged = match self.keyring.as_deref() {
            Some(kr) => {
                let root = StagingRoot::new("boot2deb-sandbox-")?;
                let path = root.stage_file(kr, "keyring.gpg")?;
                Some((root, path))
            }
            None => None,
        };
        let keyring_arg = staged.as_ref().map(|(_, p)| p.to_string_lossy().into_owned());

        let tarball = self.tarball_path();
        step.log(format!(
            "bootstrapping {} {} rootfs at {} (mmdebstrap --mode=unshare)",
            self.arch,
            self.suite,
            self.rootfs.display()
        ));
        let mut cmd = Command::new("mmdebstrap");
        cmd.args(self.mmdebstrap_argv(&tarball.to_string_lossy(), keyring_arg.as_deref()));
        build::run(cmd, "mmdebstrap", "bootstrap rootfs", step)?;
        drop(staged); // remove the temp keyring now that the bootstrap is done

        // Extract into a sibling `.partial` dir, then rename into place: the
        // `rootfs` dir must only ever appear complete, so an interrupted extraction
        // cannot leave a half-populated tree that `is_bootstrapped` would trust
        // (COR-4). Any leftover `.partial` from a prior interrupted run is cleared
        // first. Device nodes are excluded (mknod is not permitted unprivileged;
        // `bwrap` provides `/dev` at run time), so extraction has no privileged step.
        let partial = self.partial_rootfs();
        let _ = std::fs::remove_dir_all(&partial);
        std::fs::create_dir_all(&partial).map_err(|s| EngineError::io(&partial, s))?;
        let mut tar = Command::new("tar");
        tar.arg("-C")
            .arg(&partial)
            .arg("--exclude=./dev/*")
            .arg("-xf")
            .arg(&tarball);
        build::run(tar, "tar", "extract rootfs tarball", step)?;
        // Atomic publish: the extracted tree becomes the cache in one rename.
        std::fs::rename(&partial, &self.rootfs)
            .map_err(|s| EngineError::io(&self.rootfs, s))?;
        // The extracted tree is the cache; the tarball is no longer needed.
        let _ = std::fs::remove_file(&tarball);
        step.log(format!("{} rootfs ready at {}", self.arch, self.rootfs.display()));
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
        // path inside the sandbox without being able to write back into it (TRUST-6;
        // deduplicated — the userspace debs share one dir).
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
        let mut cmd = Command::new("bwrap");
        cmd.args(bwrap_argv(&self.rootfs, spec));
        build::run(cmd, "bwrap", spec.context, step)
    }
}

/// Baseline environment for every sandbox command, set after `--clearenv` so the
/// host env never leaks in (reproducibility, and it avoids `dpkg`/`perl` reading
/// the host `HOME`/locale). Per-run `spec.env` entries are appended afterwards and
/// override these. `TZ=UTC` and `LC_ALL=C.UTF-8` pin timezone and locale so packaged
/// timestamps/collation do not vary with the build host (DET-6); the host-side
/// [`build::run`](crate::build::run) normalizes the same two vars.
const SANDBOX_ENV: &[(&str, &str)] = &[
    ("PATH", "/usr/sbin:/usr/bin:/sbin:/bin"),
    ("HOME", "/root"),
    ("LC_ALL", "C.UTF-8"),
    ("TZ", "UTC"),
    ("DEBIAN_FRONTEND", "noninteractive"),
];

/// Build the `bwrap` argv that enters `rootfs` and runs `spec`. Pure, so the
/// (long, easy-to-get-wrong) container invocation is unit-testable.
///
/// The rootfs is bound as `/`; `--proc`/`--dev`/`--tmpfs` give the build a working
/// `/proc`, minimal `/dev`, and `/tmp`; `resolv.conf` is bound read-only so `apt`
/// resolves DNS. `--unshare-all` makes it rootless; `--share-net` is added only when
/// `spec.net` is set — an `apt` run needs the network, an offline compile keeps the
/// fresh (loopback-only) namespace (TRUST-6). `--uid 0 --gid 0` maps the caller to
/// root inside — `dpkg`/`dpkg-buildpackage` require it, matching "root in the chroot"
/// of the proven build. `--clearenv` plus [`SANDBOX_ENV`] gives a clean, reproducible
/// environment. Each read-write `bind` and the working dir are exposed at their host
/// path so artifacts written beside a source tree land back on the host; each
/// `ro_bind` is exposed read-only (input-only mounts apt reads but must not mutate).
fn bwrap_argv(rootfs: &Path, spec: &SandboxRun) -> Vec<String> {
    let mut argv = vec![
        "--bind".into(),
        rootfs.to_string_lossy().into_owned(),
        "/".into(),
        "--proc".into(),
        "/proc".into(),
        "--dev".into(),
        "/dev".into(),
        "--tmpfs".into(),
        "/tmp".into(),
        "--ro-bind-try".into(),
        "/etc/resolv.conf".into(),
        "/etc/resolv.conf".into(),
        "--unshare-all".into(),
    ];
    if spec.net {
        argv.push("--share-net".into());
    }
    argv.extend([
        "--die-with-parent".into(),
        "--uid".into(),
        "0".into(),
        "--gid".into(),
        "0".into(),
        "--clearenv".into(),
    ]);
    for (key, value) in SANDBOX_ENV {
        argv.push("--setenv".into());
        argv.push((*key).to_string());
        argv.push((*value).to_string());
    }
    for bind in spec.binds {
        let p = bind.to_string_lossy().into_owned();
        argv.push("--bind".into());
        argv.push(p.clone());
        argv.push(p);
    }
    for bind in spec.ro_binds {
        let p = bind.to_string_lossy().into_owned();
        argv.push("--ro-bind".into());
        argv.push(p.clone());
        argv.push(p);
    }
    argv.push("--chdir".into());
    argv.push(spec.work.to_string_lossy().into_owned());
    for (key, value) in spec.env {
        argv.push("--setenv".into());
        argv.push(key.clone());
        argv.push(value.clone());
    }
    argv.push("--".into());
    argv.extend(spec.argv.iter().cloned());
    argv
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mmdebstrap_argv_bootstraps_minbase_arm64_with_base_deps() {
        let sb = RootlessSandbox::new(PathBuf::from("/w/rootfs"), "forky", "arm64", None);
        let argv = sb.mmdebstrap_argv("/w/rootfs.tar", Some("/tmp/kr.gpg"));
        assert_eq!(argv[0], "--mode=unshare");
        assert!(argv.contains(&"--arch=arm64".to_string()));
        assert!(argv.contains(&"--variant=minbase".to_string()));
        // non-free is enabled so libfdk-aac-dev (ffmpeg-rk) resolves.
        assert!(argv
            .iter()
            .any(|a| a.starts_with("--components=") && a.contains("non-free")));
        assert!(argv.contains(&"--keyring=/tmp/kr.gpg".to_string()));
        // Base deps are one comma-joined --include.
        assert!(argv
            .iter()
            .any(|a| a.starts_with("--include=") && a.contains("build-essential") && a.contains("dpkg-dev")));
        // `--` terminates options immediately before the positionals (SUB-2).
        assert_eq!(argv[argv.len() - 4], "--");
        // Suite, target tarball, mirror are the trailing positionals in order.
        let tail = &argv[argv.len() - 3..];
        assert_eq!(tail[0], "forky");
        assert_eq!(tail[1], "/w/rootfs.tar");
        assert_eq!(tail[2], DEFAULT_MIRROR);
        // No --keyring when none is provided.
        let argv2 = sb.mmdebstrap_argv("/w/rootfs.tar", None);
        assert!(!argv2.iter().any(|a| a.starts_with("--keyring")));
    }

    #[test]
    fn interrupted_bootstrap_partial_is_never_treated_as_ready() {
        // Resume-after-interruption (rec-18, COR-4): a bootstrap that dies mid
        // extraction leaves a half-populated `.partial` dir, never a `rootfs` a
        // later build would trust. `is_bootstrapped` checks the real rootfs, which
        // only exists after the atomic rename, so the `.partial` can never fool it.
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("arm64-forky");
        let sb = RootlessSandbox::new(rootfs.clone(), "forky", "arm64", None);

        // Nothing yet → not bootstrapped.
        assert!(!sb.is_bootstrapped());

        // Interrupted extraction: `.partial/usr/bin` exists, the real rootfs does not.
        let partial = sb.partial_rootfs();
        assert_eq!(partial, tmp.path().join("arm64-forky.partial"));
        std::fs::create_dir_all(partial.join("usr/bin")).unwrap();
        assert!(!sb.is_bootstrapped(), "a half-extracted .partial must not read as ready");

        // Completed rename: the real rootfs now carries usr/bin → ready.
        std::fs::create_dir_all(rootfs.join("usr/bin")).unwrap();
        assert!(sb.is_bootstrapped());
    }

    #[test]
    fn bwrap_argv_binds_rootfs_chdir_env_and_command() {
        let binds = vec![PathBuf::from("/host/src")];
        let env = vec![("DEB_CFLAGS_APPEND".to_string(), "-Wno-error".to_string())];
        let argv = vec![
            "dpkg-buildpackage".to_string(),
            "-us".to_string(),
            "-uc".to_string(),
            "-b".to_string(),
        ];
        // An offline compile: no network, output dir bound read-write.
        let spec = SandboxRun {
            work: Path::new("/host/src/mpp"),
            binds: &binds,
            ro_binds: &[],
            net: false,
            env: &env,
            argv: &argv,
            context: "build mpp",
        };
        let a = bwrap_argv(Path::new("/w/rootfs"), &spec);
        let joined = a.join(" ");
        // rootfs is /, the source parent is bound read-write at its host path.
        assert!(joined.contains("--bind /w/rootfs /"));
        assert!(joined.contains("--bind /host/src /host/src"));
        // working dir + env + rootless flags.
        assert!(joined.contains("--chdir /host/src/mpp"));
        assert!(joined.contains("--setenv DEB_CFLAGS_APPEND -Wno-error"));
        // An offline compile gets no network share (TRUST-6).
        assert!(joined.contains("--unshare-all"));
        assert!(!joined.contains("--share-net"), "offline compile must not share net");
        // root inside + clean, reproducible env.
        assert!(joined.contains("--uid 0 --gid 0"));
        assert!(joined.contains("--clearenv"));
        assert!(joined.contains("--setenv HOME /root"));
        // Timezone + locale pinned so packaged timestamps/collation are host-independent
        // (DET-6).
        assert!(joined.contains("--setenv TZ UTC"));
        assert!(joined.contains("--setenv LC_ALL C.UTF-8"));
        // per-run env comes after --clearenv so it is not wiped.
        let clearenv = a.iter().position(|x| x == "--clearenv").unwrap();
        let deb_cflags = a.iter().position(|x| x == "DEB_CFLAGS_APPEND").unwrap();
        assert!(clearenv < deb_cflags);
        // command follows the -- separator, in order.
        let sep = a.iter().position(|x| x == "--").expect("has -- separator");
        assert_eq!(&a[sep + 1..], argv.as_slice());
    }

    #[test]
    fn bwrap_argv_shares_net_and_ro_binds_for_apt() {
        // An apt-style run: needs the network, and its deb-input dir is read-only so
        // a maintainer script cannot write back into the host dir (TRUST-6).
        let ro = vec![PathBuf::from("/out/debs")];
        let argv = vec!["apt-get".to_string(), "update".to_string()];
        let spec = SandboxRun {
            work: Path::new("/"),
            binds: &[],
            ro_binds: &ro,
            net: true,
            env: &[],
            argv: &argv,
            context: "apt-get update",
        };
        let joined = bwrap_argv(Path::new("/w/rootfs"), &spec).join(" ");
        assert!(joined.contains("--unshare-all --share-net"), "apt needs the network");
        assert!(joined.contains("--ro-bind /out/debs /out/debs"), "deb dir is read-only");
        assert!(!joined.contains("--bind /out/debs"), "the deb dir must not be writable");
    }

    #[test]
    fn native_describe_and_noops() {
        let sb = NativeSandbox;
        assert_eq!(sb.describe(), "native");
        // ensure_ready / install do not touch the host on the native path.
        let sink = |_: crate::event::Event| {};
        let step = Step::start(&sink, "userspace");
        assert!(sb.ensure_ready(&step).is_ok());
        assert!(sb.install(&["cmake"], &step).is_ok());
        assert!(sb.install_local_debs(&[PathBuf::from("/x/librga2_2_arm64.deb")], &step).is_ok());
    }
}
