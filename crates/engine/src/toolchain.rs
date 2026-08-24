//! The one host binary that still shapes a build's compiled bytes: the `qemu-user`
//! interpreter, for the Tier-2 output signature and the provenance manifest.
//!
//! Every compiler a build runs is a package of a provisioned root — the target-arch
//! sandbox's for the userspace and ffmpeg `.deb`s, the cross root's for the kernel,
//! u-boot and out-of-tree modules ([`crate::sandbox`]). Each root's identity is the name
//! of the tree it was provisioned under, and its manifest states every package
//! sha256-pinned, so "which compiler built this" is answered by
//! [`sandbox_identity`](crate::build::sandbox_identity) and
//! [`cross_identity`](crate::build::cross_identity) rather than by a `--version` line.
//!
//! `qemu-user` is the exception, and it is a real one: it is registered with the host
//! *kernel's* binfmt handler and executes from the host filesystem, so no root can carry
//! it and nothing but a probe can describe it. Where the host cannot execute the target's
//! binaries it interprets every compiler invocation in the target-arch sandbox, which
//! makes it an input to every `.deb` produced there.
//!
//! # The interpreter is the kernel's, not `PATH`'s
//!
//! What runs is whatever the kernel's binfmt registration names, and that is not
//! necessarily what a `PATH` lookup finds. Debian registers a wrapper path under
//! `/usr/libexec/qemu-binfmt/` rather than the `qemu-<arch>-static` on `PATH`; the two
//! normally resolve to one file, and nothing makes them. A build whose `PATH` carries no
//! interpreter at all still runs every target binary, because binfmt reaches the
//! registered path directly — so a `PATH` probe can report an absence that is not one,
//! and can name a binary that never ran.
//!
//! The registration is read by [`foreign_interpreter`], the same reader the
//! provisioner's own preflight uses: the rootfs bootstrap and this module ask one
//! question of one `/proc` file, so they cannot drift about whether a handler exists or
//! which binary it names.
//!
//! What this module adds on top is the **identity**: the content of the registered file
//! rather than a version line. A digest survives a rebuild at an unchanged version, and
//! it can be taken from a binary that cannot be run — which the registered path
//! generally cannot, since `qemu` refuses to execute under its own binfmt wrapper name.
//! The digest is taken over the registered path rather than the canonicalized one
//! because `open` follows the symlink either way, so the bytes are the same and the
//! provenance-faithful path is the one the kernel recorded. The canonical path is
//! recorded beside it and is what a version probe is run against.
//!
//! Folding it into the [artifact store](crate::artstore) key keeps a build on one
//! interpreter from restoring an artifact another produced — "bias toward hashing more,
//! not less". This is a build-time host probe, so it lives in the engine, not in the pure
//! lock-only `why-rebuild` plan.

use ferroday_cage::provision::debian::foreign_interpreter;
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The interpreter a build's target-arch commands run under, identified by content.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Interpreter {
    /// The Debian architecture whose binaries this interprets, for a message that names
    /// what was looked for when nothing was found.
    arch: String,
    /// The interpreter path exactly as the kernel recorded it, when a handler is
    /// registered and names one. This is what [`sha256`](Self::sha256) is taken over.
    path: Option<PathBuf>,
    /// That path with symlinks resolved, when it resolves. A separate fact worth keeping:
    /// the registration usually names a wrapper symlink, and repointing it changes the
    /// interpreter without changing the registration.
    resolved: Option<PathBuf>,
    /// sha256 of the interpreter's bytes, lowercase hex.
    sha256: Option<String>,
    /// First line of its `--version` output. Absent where the binary could not be read
    /// or could not be run — the digest above is the identity, and this is for a human.
    version: Option<String>,
}

impl Interpreter {
    /// Probe the registered interpreter for the Debian architecture `arch`.
    ///
    /// Every failure is recorded rather than raised: a build that interprets nothing
    /// needs no interpreter, and for one that does, an honest "registered but
    /// unreadable" is more useful than a refusal to describe the host.
    fn probe(arch: &str) -> Self {
        let registered = foreign_interpreter(arch);
        let path = registered.as_ref().map(|i| i.path.clone());
        let resolved = registered.and_then(|i| i.resolved);
        Interpreter {
            arch: arch.to_string(),
            sha256: path.as_deref().and_then(digest_of),
            // The wrapper name refuses to run, so the version line comes from the
            // canonical path or from nowhere.
            version: resolved.as_deref().and_then(version_of),
            path,
            resolved,
        }
    }

    /// The signature form: total, because a cache key cannot have a hole, and distinct
    /// per binary even when the binary cannot be read — so two hosts whose interpreters
    /// are both missing still yield different identities from two that differ.
    ///
    /// The digest where there is one; otherwise a value that says which of the two ways
    /// it was absent, so a host with no registration never keys alike with one whose
    /// registered file could not be read.
    fn identity(&self) -> String {
        match (&self.sha256, &self.path) {
            (Some(sha256), _) => format!("sha256:{sha256}"),
            (None, Some(path)) => format!("unreadable-interpreter:{}", path.display()),
            (None, None) => format!("unregistered:{}", self.arch),
        }
    }
}

/// sha256 of the file at `path`, lowercase hex; `None` when it cannot be read.
fn digest_of(path: &Path) -> Option<String> {
    let bytes = std::fs::read(path).ok()?;
    Some(format!("{:x}", Sha256::digest(&bytes)))
}

/// First line of `path --version`; `None` when it cannot be run or reports nothing.
fn version_of(path: &Path) -> Option<String> {
    let out = Command::new(path).arg("--version").output().ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .next()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
}

/// The build host's `qemu-user` interpreter, probed once per build.
///
/// Held by [`BuildEnv`](crate::build::BuildEnv) and read from two directions: the
/// userspace and ffmpeg stages fold [`qemu_identity`](Self::qemu_identity) into their
/// output signatures, and the CLI reads [`qemu`](Self::qemu) into the provenance
/// manifest's `[toolchain.qemu]` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostToolchain {
    /// The registered `qemu-<arch>` interpreter, present only where the host cannot
    /// execute the target's binaries (a host that runs them directly consults none).
    qemu: Option<Interpreter>,
}

impl HostToolchain {
    /// Probe the host. `arch` is the target's
    /// [Debian architecture](boot2deb_core::model::Arch::debian_arch), and `None` on a
    /// host that executes the target's binaries directly — where nothing is interpreted
    /// at all.
    ///
    /// That is a narrower question than "is this build cross": an arm64 host building
    /// armhf compiles through a cross toolchain and then runs the result natively
    /// (`CONFIG_COMPAT=y`), so it interprets nothing and passes `None` here. **The
    /// caller owns that decision**, which is why it is a parameter rather than something
    /// probed here: the reader this delegates to has its own, more conservative rule for
    /// which pairs are native, and boot2deb's is the one that decides whether an
    /// interpreter is an input to this build.
    pub fn probe(arch: Option<&str>) -> Self {
        HostToolchain {
            qemu: arch.map(Interpreter::probe),
        }
    }

    /// The identity of the interpreter that executes every target-arch sandbox compile,
    /// folded into the userspace/ffmpeg output signatures. `None` where the host runs
    /// target binaries directly and no interpreter is an input at all.
    pub fn qemu_identity(&self) -> Option<String> {
        self.qemu.as_ref().map(Interpreter::identity)
    }

    /// What the provenance manifest records about the interpreter: the path the kernel
    /// registered, what that path resolves to, its digest, and its version line where it
    /// could be run.
    ///
    /// `None` where nothing is interpreted, and where the registration named nothing
    /// readable — the honest absence rather than
    /// [`qemu_identity`](Self::qemu_identity)'s total-for-a-cache-key fallback.
    ///
    /// Both paths are recorded because they are two facts. The registered one is what
    /// the kernel holds; the resolved one is the file it reaches, and on the common
    /// Debian layout repointing the wrapper symlink between them changes the interpreter
    /// without changing the registration.
    ///
    /// The digest is taken at plan time from the path the registration names. The `F`
    /// flag means the kernel opened and holds the interpreter at *registration* time, so
    /// a file replaced since is one this digest describes and the kernel is not running.
    /// That is the assumption; it is stated rather than defended, and no better one is
    /// available to a process that is not the kernel.
    pub fn qemu(&self) -> Option<boot2deb_core::provenance::QemuProvenance> {
        let qemu = self.qemu.as_ref()?;
        Some(boot2deb_core::provenance::QemuProvenance {
            interpreter: qemu.path.as_ref()?.display().to_string(),
            resolved: qemu.resolved.as_ref().map(|p| p.display().to_string()),
            sha256: qemu.sha256.clone()?,
            version: qemu.version.clone(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing is interpreted where the host runs target binaries directly, so qemu is
    /// not an input to any signature there — distinct from an interpreting host whose
    /// interpreter is missing, which still folds a fallback naming it, so the two never
    /// key alike.
    ///
    /// Reading the registration itself is the sandbox library's job and is tested there;
    /// what is boot2deb's here is the identity built on top of it, and the total value a
    /// cache key needs when there is no registration to build one from.
    #[test]
    fn a_host_that_interprets_nothing_has_nothing_to_fold() {
        assert_eq!(HostToolchain::probe(None).qemu_identity(), None);
        assert_eq!(
            HostToolchain::probe(Some("boot2deb-no-such-arch"))
                .qemu_identity()
                .as_deref(),
            Some("unregistered:boot2deb-no-such-arch")
        );
    }

    #[test]
    fn the_provenance_accessor_reports_absence_rather_than_the_fallback() {
        // The cache key cannot have a hole, so `qemu_identity` invents a total value;
        // the manifest can, and says so. A reader must not see `unregistered:...` as if
        // it were an interpreter.
        let tc = HostToolchain::probe(Some("boot2deb-no-such-arch"));
        assert!(tc.qemu_identity().is_some());
        assert_eq!(tc.qemu(), None);
    }

    /// On a host that registers a handler, the whole chain has to compose: registration
    /// read through the sandbox library, digest taken over the registered path, wrapper
    /// canonicalized, identity and provenance derived. Each piece works alone; what this
    /// asserts is that they fit — and the failure the single reader exists to prevent was
    /// two probes that each worked and disagreed.
    ///
    /// Skipped where the host registers no `arm64` handler, which is a legitimate host
    /// rather than a broken one — including an arm64 host, where nothing is interpreted
    /// and the library reports no interpreter at all.
    #[test]
    fn a_registered_handler_yields_a_digest_identity_and_both_paths() {
        let tc = HostToolchain::probe(Some("arm64"));
        let identity = tc.qemu_identity().expect("an arch was asked for");
        if identity.starts_with("unregistered:") {
            return;
        }
        let qemu = tc.qemu().expect("a registered, readable interpreter");
        assert_eq!(identity, format!("sha256:{}", qemu.sha256));
        assert_eq!(qemu.sha256.len(), 64, "lowercase hex sha256");
        assert!(qemu.sha256.chars().all(|c| c.is_ascii_hexdigit()));
        // The recorded path is the one the kernel holds — the wrapper, on the common
        // Debian layout — and the digest is of the file `open` reaches through it.
        let registered = Path::new(&qemu.interpreter);
        assert!(registered.is_absolute(), "{}", qemu.interpreter);
        assert_eq!(
            digest_of(registered).as_deref(),
            Some(qemu.sha256.as_str()),
            "the digest is taken over the registered path, symlink and all"
        );
        // And the canonical form is recorded beside it, since repointing the wrapper
        // swaps the interpreter with the registration unchanged.
        let resolved = qemu.resolved.as_deref().expect("the wrapper resolves");
        assert_eq!(
            std::fs::canonicalize(Path::new(resolved)).unwrap(),
            Path::new(resolved)
        );
        assert_eq!(
            digest_of(Path::new(resolved)).as_deref(),
            Some(qemu.sha256.as_str())
        );
    }

    /// The identity is the file's content, so it moves when the file does even at an
    /// unchanged version — which is the reason it is not the version line.
    #[test]
    fn the_identity_is_the_interpreters_content() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("qemu-fake-static");
        std::fs::write(&path, b"one").unwrap();
        let first = digest_of(&path).unwrap();
        std::fs::write(&path, b"two").unwrap();
        assert_ne!(first, digest_of(&path).unwrap(), "content moved the digest");
        assert_eq!(digest_of(Path::new("/no/such/interpreter")), None);
    }
}
