//! The provisioned Debian userlands boot2deb's package work happens in — every step
//! that compiles a `.deb` or archives one runs inside one of them, never on the host.
//!
//! There are three, in two kinds. Two are **build sandboxes** ([`RootlessSandbox`]) and
//! differ only in their [`SandboxRole`]; the third archives and compiles nothing:
//!
//! - The **target-arch build sandbox** ([`SandboxRole::Target`]) is where the userspace
//!   and ffmpeg stages ([`crate::build`]) compile their `.deb`s, bootstrapped for the
//!   build's suite and **target** arch.
//! - The **cross build sandbox** ([`SandboxRole::Cross`]) is where the kernel, u-boot
//!   and kmod stages compile, bootstrapped for the build's suite and the **host** arch
//!   and carrying a cross toolchain that emits the target's objects.
//! - The **packaging root** ([`PackagingSandbox`]) is where an already-staged tree
//!   becomes a `.deb`. It is bootstrapped for the build's
//!   [`packaging_suite`](boot2deb_core::model::ResolvedBuild::packaging_suite) and the
//!   **host** arch, carries `dpkg` and `xz-utils` and nothing else, and is never
//!   layered — so it has no `build_root` operation at all.
//!
//! Both build sandboxes are layered: a stage declares its build-dependencies, gets a
//! [`BuildRoot`] — the shared base plus that stage's increment, on an unprivileged
//! overlay — and drops it when it is done.
//!
//! **Why two compile roots rather than one, and why each sits where it does.** A compile
//! that must *link against the target's libraries* has to happen at the target's
//! architecture, and one that merely *emits the target's objects* does not. The
//! userspace and ffmpeg stages are the first case; the kernel, u-boot and kmod stages
//! are the second, so they compile natively in a host-arch root through a cross
//! toolchain — the standard distro cross-build shape — and pay no emulation for a
//! multi-minute kernel build. Archiving resolves nothing at all, which is why the
//! packaging root is host-arch too.
//!
//! What all three buy is the same: the tool that shapes the output — compiler, linker,
//! `dpkg-deb` — is a sha256-pinned package resolved from the build's own mirror list,
//! rather than whatever the build host happened to have installed.
//!
//! The suite, not the arch, is what makes the target-arch sandbox necessary. Those stages
//! emit `.deb`s for the target suite, and `dpkg-shlibdeps` derives each one's runtime
//! `Depends` from the libraries present at build time — it maps every `NEEDED` soname
//! to the package that provides it *here*. Building on the host would link against the
//! host's libraries and stamp the host's package names and versions into `Depends`,
//! producing a `.deb` that does not install in the target rootfs even on a
//! matching-arch host. The sandbox is also the only place the stages can see the
//! build's *own* userspace `.deb`s: ffmpeg links against `librga2`/`librockchip-mpp1`,
//! which this build produces, and `dpkg-shlibdeps` resolves `librga.so.2` to
//! `Depends: librga2` only because that deb — and its `shlibs` — is present in the
//! root it compiles in. It gets there through the stage's own
//! [`BuildRootSpec::pool`]: the build publishes its userspace `.deb`s as a trusted
//! `file://` repository, and the stage's build root resolves them out of it like any
//! other package.
//!
//! A compile stage never mutates the environment it builds in. The base is
//! bootstrapped once and then read-only; each stage declares its build-dependencies,
//! gets a [`BuildRoot`] — the base plus that stage's increment, layered on with an
//! unprivileged overlay — and drops it when it is done. So a build root is a function
//! of what the stage declared, not of which builds ran in the directory before it, and
//! an undeclared build-dependency fails immediately instead of compiling against a
//! leftover. The packaging root needs none of that machinery, because it never acquires
//! a package after its base is published: what it holds is fixed at bootstrap, so there
//! is nothing for a declaration to get wrong.
//!
//! That is also where the host requirement for an unprivileged overlay comes from, and
//! why it is a requirement of *compiling* rather than of every build: a build that
//! compiles nothing — a board that installs Debian's kernel, or a rebuild whose
//! artifacts all restore from the cache — stands up no build root and needs only user
//! namespaces.
//!
//! Every root is **unprivileged**: the rootfs is bootstrapped and entered entirely
//! in-process by the pure-Rust [`ferroday_cage`] library — its Debian provisioner
//! resolves, verifies, and lays out the suite/arch userland with no `sudo`
//! and no external bootstrap binary, and each command then runs in a cage
//! (fresh namespaces, the rootfs mounted as `/`, the caller mapped to root inside).
//! When the root's arch differs from the host's, its binaries execute via the
//! host's `qemu-user` binfmt handler — registered with the `F` (fix-binary) flag,
//! so the interpreter is preloaded and nothing is copied into the rootfs; when the
//! arches match they simply run, and `qemu-user` is never consulted. Only the
//! target-arch sandbox is ever in the first case, which is what makes `qemu-user` a
//! requirement of building *target-arch packages* rather than of cross-building at all.
//! Each bootstrapped tree is cached and reused across builds — the base-rootfs cache —
//! not a per-build throwaway. (The *OS* rootfs that becomes the image is a further tree,
//! bootstrapped by [`crate::rootfs`].)
//!
//! Mapping the caller to root inside is what retires `fakeroot` — from every root, and
//! so from boot2deb entirely. It is uid 0 that a Debian packaging tool wants, and the
//! cage supplies the real thing, so each tool takes the branch it takes when run by
//! root: a tree the build user staged on the host stats as `root:root` inside, and
//! `dpkg-deb` archives it with the ownership a `.deb` must carry; `dpkg-buildpackage`
//! selects no gain-root command at all. No base set carries the package, because
//! nothing on any path would execute it.
//!
//! No root is a hard security boundary against malicious build code: each runs
//! as the build user with the build directories bind-mounted read-write. What
//! stops a malicious build script is that every compiled source is pinned to an
//! exact commit by the lock, not the namespace around the compiler.

use crate::bootstrap::{COMPONENTS, DEFAULT_MIRROR};
use crate::build;
use crate::error::EngineError;
use crate::event::{Step, Stream};
use boot2deb_core::provenance::{
    SandboxLandlockFs, SandboxLandlockNet, SandboxMount, SandboxPosture, SandboxProvenance,
    SandboxRlimit, SandboxStreams,
};
use ferroday_cage::provision::debian::BuildLayer;
use ferroday_cage::provision::debian::{
    Debian, DebianBuilder, DebianEvent, Plan, Repository, Stream as DebianStream,
};
use ferroday_cage::provision::{self, Provisioned};
use ferroday_cage::{
    Cage, IdentityMap, Network, Observer, ResolvedHardening, ResolvedIdentity, ResolvedMount,
    ResolvedRoot, ResolvedStdio, ResolvedStreams, Stdio,
};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

/// Base packages installed in a [`SandboxRole::Target`] sandbox at bootstrap — the
/// minimum to run `dpkg-buildpackage`. Stage-specific build-deps are layered over this
/// set, per stage, by [`BuildSandbox::build_root`].
///
/// No `fakeroot`, which is the one absence a reader of a Debian build environment would
/// question. `dpkg-buildpackage` reaches for it only when it is *not* already uid 0, and
/// in here it always is (see the [module docs](self)) — so it takes the no-gain-root
/// branch and the package would never be executed. Leaving it in the set would state a
/// dependency the build does not have, in the manifest that records what compiled the
/// image's `.deb`s.
const BASE_DEPS: &[&str] = &[
    "build-essential",
    "ca-certificates",
    "dpkg-dev",
    "debhelper",
    "pkg-config",
];

/// Base packages installed in a [`SandboxRole::Cross`] sandbox at bootstrap, *besides*
/// the toolchain the role adds — the tooling every stage that compiles for the target
/// needs, whichever of them is running.
///
/// `bc`, `bison` and `flex` are kbuild's and u-boot's shared generators; `libssl-dev` is
/// what both link their host-side signing and image tools against. Each is small next to
/// the toolchain, and each is wanted by more than one stage — a package only one stage
/// needs belongs in that stage's [`BuildRootSpec`], not here.
///
/// `build-essential` is stated alongside the cross toolchain rather than assumed from
/// it: kbuild and u-boot both compile *host* programs during a cross build (`objtool`,
/// `mkimage`, `dtc`, the `scripts/` tree), so the root needs a working native compiler
/// as well as a cross one, and which of the two a build reaches for is not something
/// the base should leave to a transitive dependency.
const CROSS_DEPS: &[&str] = &[
    "build-essential",
    "ca-certificates",
    "dpkg-dev",
    "pkg-config",
    "bc",
    "bison",
    "flex",
    "libssl-dev",
];

/// Base packages installed in the **packaging root** at bootstrap — the whole of it,
/// since nothing is ever layered over this base.
///
/// `dpkg` for `dpkg-deb`, and `xz-utils` for the compressor
/// [`DEB_COMPRESSOR`](crate::build::DEB_COMPRESSOR) names. Deliberately neither compile
/// set: archiving a staged tree compiles nothing, so a toolchain, `debhelper` and
/// `pkg-config` in here would be a few hundred megabytes of tree that no packaging step
/// can reach for. No set carries `fakeroot`, for the reason the [module docs](self)
/// give.
const PACKAGING_DEPS: &[&str] = &["dpkg", "xz-utils"];

/// What a [`RootlessSandbox`] is *for*: which architecture it is provisioned at, which
/// packages its base carries, and which token names its tree.
///
/// One type serves both because a compile root is a compile root — same bootstrap, same
/// layering, same cage. What differs is which side of the compile the root stands on,
/// and that is exactly one value rather than a second implementation of everything
/// around it. Contrast [`PackagingSandbox`], which is a distinct type because "never
/// layered" is a difference in *contract*, not in configuration.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SandboxRole {
    /// The **target-arch** root the userspace and ffmpeg stages compile in.
    ///
    /// Those stages link against the target's libraries and let `dpkg-shlibdeps` derive
    /// each `.deb`'s runtime `Depends` from what is present at build time, which is only
    /// correct at the target's architecture. On a host that cannot execute the target's
    /// binaries every command in here runs under `qemu-user`.
    Target,
    /// The **host-arch** root the kernel, u-boot and kmod stages compile in, carrying a
    /// cross toolchain that emits `target`'s objects.
    ///
    /// These stages emit target objects but link nothing against the target's
    /// libraries — the kernel and u-boot are freestanding, and an out-of-tree module
    /// links against the kernel tree beside it. So the compile can be native, which is
    /// what keeps a multi-minute kernel build off `qemu-user`.
    Cross {
        /// Debian architecture the toolchain in this root emits for — the *build's*
        /// target, not the root's own.
        ///
        /// It picks the toolchain package, and it reaches the tree name through that, so
        /// two targets never share a cross root. Where it equals the root's own
        /// architecture there is no cross toolchain to add: the base's own
        /// `build-essential` — carried for the *host* programs kbuild and u-boot build
        /// during any cross build — is then the whole compiler.
        target: &'static str,
    },
}

impl SandboxRole {
    /// Token for this role in a base tree's leaf name. See [`base_tree_name`].
    fn token(self) -> &'static str {
        match self {
            SandboxRole::Target => "build",
            SandboxRole::Cross { .. } => "cross",
        }
    }

    /// Recipe version of this role's base, versioned independently so a reason to retire
    /// one role's trees does not retire the other's.
    fn stage_version(self) -> u32 {
        match self {
            SandboxRole::Target => BASE_STAGE_VERSION,
            SandboxRole::Cross { .. } => CROSS_STAGE_VERSION,
        }
    }

    /// The packages this role's base is bootstrapped with, at a root provisioned for
    /// `arch`.
    ///
    /// Only the cross role reads `arch`, and only to answer whether a cross toolchain is
    /// wanted at all: an arm64 host building an arm64 board compiles natively, and
    /// `crossbuild-essential-arm64` is not the package for that. Such a root takes
    /// [`CROSS_DEPS`] alone, whose `build-essential` is exactly the compiler it wants.
    fn base_packages(self, arch: &str) -> Vec<String> {
        fn owned(set: &[&str]) -> Vec<String> {
            set.iter().map(|p| (*p).to_string()).collect()
        }
        match self {
            SandboxRole::Target => owned(BASE_DEPS),
            SandboxRole::Cross { target } => {
                let mut packages = Vec::new();
                // The cross toolchain first: it is what the root is for, and the leaf
                // name's digest covers the list as written, so a stable position keeps a
                // tree from being re-provisioned over a reordering.
                if arch != target {
                    packages.push(format!("crossbuild-essential-{target}"));
                }
                packages.extend(owned(CROSS_DEPS));
                packages
            }
        }
    }

    /// Header line of this role's base manifest ([`BuildSandbox::base_manifest`]).
    ///
    /// Distinct per role, and distinct from the rootfs manifest's, so no two of the
    /// files a build can publish are mistaken for one another: they describe different
    /// trees answering different questions, and a build can publish all of them.
    fn manifest_header(self) -> &'static str {
        match self {
            SandboxRole::Target => {
                "Solved build-sandbox base manifest: the toolchain that compiled this \
                 build's target .debs, as name version arch sha256."
            }
            SandboxRole::Cross { .. } => {
                "Solved cross-sandbox base manifest: the toolchain that compiled this \
                 build's kernel, u-boot and modules, as name version arch sha256."
            }
        }
    }
}

/// Header line of the packaging root's package manifest
/// ([`PackagingSandbox::base_manifest`]).
///
/// Distinct from each build sandbox's ([`SandboxRole::manifest_header`]) and from the
/// rootfs manifest's, for the reason those are distinct from each other: they describe
/// different trees answering different questions, and a build can publish all of them.
const PACKAGING_MANIFEST_HEADER: &str =
    "Solved packaging-root manifest: the dpkg that archived this build's staged .debs, \
     as name version arch sha256.";

/// One command to run inside a [`BuildRoot`].
///
/// `work` is the working directory; it and every path in `binds` are host paths
/// made visible inside the sandbox **at the same absolute path**, so a build that
/// drops artifacts beside its source tree writes them back to the host dir. `env`
/// entries are exported for the command.
pub struct SandboxRun<'a> {
    /// Working directory (a host path, exposed inside at the same path). Must be
    /// `work` itself or lie under one of `binds`.
    pub work: &'a Path,
    /// Read-write host paths exposed inside at their host path — where the command
    /// writes artifacts back to the host (a build's output/work dir).
    pub binds: &'a [PathBuf],
    /// Environment variables exported for the command.
    pub env: &'a [(String, String)],
    /// The command and its arguments (`argv[0]` is the program). Must be non-empty;
    /// [`BuildRoot::run`] rejects an empty one with [`EngineError::EmptyArgv`] rather
    /// than indexing past the end.
    pub argv: &'a [String],
    /// Human-readable description of the invocation, for errors.
    pub context: &'a str,
}

/// A build root and the host paths a command in it must see — the pair every compile
/// invocation carries.
///
/// One value because neither half means anything alone: a root with no binds sees no
/// source tree, and a bind list with no root has nowhere to be exposed. Passing them
/// together is also what keeps a stage's several `make` runs agreeing on what is
/// visible, which a per-call bind list would leave to each call site to get right.
pub struct CompileRoot<'a> {
    /// The layered root the command runs in: the shared base plus this stage's
    /// increment.
    pub root: &'a BuildRoot,
    /// Host paths exposed inside at their own absolute path, so a build that writes
    /// beside its source tree writes back to the host.
    pub binds: &'a [PathBuf],
}

/// What a stage needs in its build root — the packages to layer over the base, and
/// where they may be resolved from.
///
/// A stage declares its requirements rather than mutating a shared environment, which
/// is what makes the root a function of the declaration instead of a function of which
/// builds ran in the directory before it.
pub struct BuildRootSpec<'a> {
    /// Debian packages to layer over the base — this stage's build-dependencies.
    ///
    /// Resolved as a delta against the base's own configured set, so only what the
    /// base lacks is downloaded and configured. A package the base already carries
    /// contributes nothing, and an empty delta stages no increment at all.
    pub packages: &'a [&'a str],
    /// A trusted `file://` repository the increment resolves against **in addition**
    /// to the suite mirrors — the build's own `.deb`s, fed forward from an earlier
    /// stage ([`LocalDistsRepo::file_url`](crate::repo::LocalDistsRepo::file_url)).
    ///
    /// `None` resolves against the suite alone. This is how a stage build-depends on a
    /// package *this build produced*: the pool is a real repository, so the resolver
    /// pulls the package and its transitive dependencies through one resolution rather
    /// than having the `.deb` pushed into the tree behind the resolver's back.
    pub pool: Option<&'a str>,
    /// The stage this root belongs to (`userspace`, `ffmpeg`) — names the overlay
    /// upper's directory and appears in log lines. One root serves a whole stage: the
    /// packages a stage declares are its own, and distinct stages get distinct roots.
    pub stage: &'a str,
}

/// A disposable build root: the shared immutable base overlaid with one stage's
/// increment.
///
/// Returned by [`BuildSandbox::build_root`]. Commands run through
/// [`run`](Self::run) see the merged `base + increment` view; dropping the value
/// discards the increment and leaves the base as it was.
///
/// `run` lives here rather than on the sandbox because a run has to name the root it
/// happens in, and a build root is the only root a build command runs in — the base
/// itself is never entered.
pub struct BuildRoot {
    /// The base tree — the overlay's read-only lower.
    base: PathBuf,
    /// The staged increment. Held for its [`Drop`], which removes the upper and the
    /// overlay's work directory beside it; the field is otherwise read only for
    /// [`path`](BuildLayer::path).
    layer: BuildLayer,
    /// The increment's resolved plan — every package the layer added, with the
    /// archive-recorded sha256 of each. Reported by the resolution that installed
    /// them, so it describes the set the build actually ran against.
    plan: Plan,
}

impl BuildRoot {
    /// Run one command in this build root per `spec`, streaming its output to `step`
    /// and mapping a non-zero exit to
    /// [`CommandFailed`](EngineError::CommandFailed).
    ///
    /// The command's `/` is the overlay: the base's files plus this stage's packages,
    /// with writes landing in the increment. `spec`'s binds still expose host paths at
    /// their host path, so artifacts land back on the host rather than in the upper.
    pub fn run(&self, spec: &SandboxRun, step: &Step) -> Result<(), EngineError> {
        let cage = build_cage(baseline_overlay(&self.base, self.layer.path()), spec)?;
        run_cage(cage, spec, step)
    }

    /// The increment's resolved plan: what this root holds over the base, sha256-pinned
    /// per package.
    ///
    /// The layered counterpart of the base's own manifest
    /// ([`BuildSandbox::base_manifest`]). Together they state the whole of what compiled
    /// a stage's `.deb`s, which is what makes a build root recordable rather than merely
    /// reproducible-in-principle.
    pub fn plan(&self) -> &Plan {
        &self.plan
    }
}

/// An environment in which target-arch package builds run.
///
/// Implemented by [`RootlessSandbox`] — a userland bootstrapped for the build's
/// suite and arch, bootstrapped and entered entirely in-process by [`ferroday_cage`].
/// A stage drives it through these operations and is otherwise agnostic to
/// the backend, so another rootfs provider can satisfy the same contract.
///
/// The base is immutable by construction: nothing on this trait writes into it after
/// [`ensure_ready`](Self::ensure_ready) publishes it, and the only way a stage acquires
/// a package is [`build_root`](Self::build_root), which layers it into an overlay the
/// stage then drops.
pub trait BuildSandbox {
    /// Short label for logs (e.g. `native`, `rootless arm64`).
    fn describe(&self) -> String;

    /// Ensure the environment exists with the base build tooling present.
    /// Idempotent — the cross backend bootstraps and caches an arm64 rootfs on the
    /// first call and reuses it thereafter.
    fn ensure_ready(&self, step: &Step) -> Result<(), EngineError>;

    /// The manifest of the packages the base carries: one
    /// `name version arch sha256` line per package, sha256-pinned from the plan the
    /// bootstrap resolved ([`crate::manifest`]).
    ///
    /// The base is the toolchain that compiles the build's target `.deb`s, and no
    /// source pin covers it — so this is the record of what produced them, and it is
    /// what the image's provenance reports. `None` until
    /// [`ensure_ready`](Self::ensure_ready) has published a base, which a build with
    /// no package stage never does.
    fn base_manifest(&self) -> Option<PathBuf>;

    /// A disposable build root: the immutable base plus `spec`'s packages, resolved
    /// against the suite and `spec.pool`, layered on with an unprivileged overlay.
    ///
    /// The increment lives in the overlay's upper layer and is discarded when the
    /// returned [`BuildRoot`] drops, so the base is never mutated and a stage's
    /// build-deps cannot leak into the next stage or into the next build. Requires
    /// [`ensure_ready`](Self::ensure_ready) to have published the base.
    fn build_root(&self, spec: &BuildRootSpec, step: &Step) -> Result<BuildRoot, EngineError>;
}

/// A provisioned Debian userland and everything needed to provision it: the shared
/// half of [`RootlessSandbox`] and [`PackagingSandbox`].
///
/// The two roots differ in what they are *for* — one is layered and compiles, the other
/// is fixed and archives — and in nothing about how a suite, an architecture and a
/// mirror list become a tree on disk. That half lives here so the two cannot drift in
/// which archive they resolve from, which keyring they trust, or how a base states its
/// own contents. A role-specific type supplies the package set and the manifest header;
/// this supplies the rest.
struct SandboxBase {
    /// Rootfs directory — bootstrapped once, reused across builds (the seed of the
    /// base-rootfs cache).
    rootfs: PathBuf,
    /// Debian suite to bootstrap (e.g. `forky`).
    suite: String,
    /// Debian architecture to bootstrap (e.g. `arm64`). The *target's* for a build
    /// sandbox, the *host's* for a packaging root.
    arch: String,
    /// Ordered mirror list the rootfs is bootstrapped from — the same list the rootfs
    /// node fetches the *image's* userland from
    /// ([`snapshot::resolve_mirrors`](crate::snapshot::resolve_mirrors)). Non-empty.
    ///
    /// Shared, not defaulted, because the tools that produce the build's `.deb`s live
    /// in these rootfs trees: a `--snapshot pin` that fixed the image's userland to a
    /// point in time while a sandbox kept bootstrapping from the live mirror would
    /// pin the *output* packages and leave the *compiler and archiver* that produced
    /// them free to move, which is not what "pinned" reads as.
    mirrors: Vec<String>,
    /// Debian archive keyring verifying the suite's `Release` signature. `None`
    /// falls back to the host apt trust store (only works on a Debian host); a
    /// vendored keyring makes the bootstrap portable to non-Debian hosts.
    keyring: Option<PathBuf>,
    /// Content-addressed directory downloaded `.deb`s are cached in, reused across
    /// bootstraps. `None` downloads to a temporary directory the provisioner discards.
    ///
    /// Shared with the rootfs node's cache rather than kept separate: both provision
    /// the same suite and architecture, so their package sets overlap heavily and each
    /// entry is content-addressed, verified against its digest before reuse, and
    /// published by rename — a cache two provisioners write is the same file they both
    /// name.
    cache_dir: Option<PathBuf>,
}

impl SandboxBase {
    /// A base rooted at `rootfs` for `suite`/`arch`, resolved from `mirrors` in order
    /// and verified with `keyring`.
    ///
    /// An empty `mirrors` falls back to [`crate::DEFAULT_MIRROR`] rather than failing:
    /// a caller that resolved no mirror expressed no preference. Every other argument
    /// is taken as given.
    fn new(
        rootfs: PathBuf,
        suite: String,
        arch: String,
        mirrors: Vec<String>,
        keyring: Option<PathBuf>,
        cache_dir: Option<PathBuf>,
    ) -> Self {
        SandboxBase {
            rootfs,
            suite,
            arch,
            mirrors: if mirrors.is_empty() {
                vec![DEFAULT_MIRROR.to_string()]
            } else {
                mirrors
            },
            keyring,
            cache_dir,
        }
    }

    /// Where the base's package manifest lives: beside the rootfs tree, named for it.
    ///
    /// A sibling rather than a file inside the tree, because the tree is a Debian
    /// userland a command sees as `/` — a boot2deb file in it would be visible to every
    /// step that runs there — and because the manifest has to be readable without
    /// entering it. It shares the tree's cache key ([`build_sandbox_dir`],
    /// [`packaging_root_dir`]), so a base and its record move together.
    fn manifest_path(&self) -> PathBuf {
        let mut name = self.rootfs.file_name().unwrap_or_default().to_os_string();
        name.push(".pkgs");
        self.rootfs.with_file_name(name)
    }

    /// The manifest path if a published base stands behind it, else `None` — the
    /// accessor behind both roots' `base_manifest`.
    fn published_manifest(&self) -> Option<PathBuf> {
        let path = self.manifest_path();
        path.is_file().then_some(path)
    }

    /// The provisioner configuration the base bootstrap and every build root over it
    /// resolve against: mirror and fallbacks, components, keyring, stale-release
    /// posture, download cache, and identity map.
    ///
    /// One definition because a base and a layer over it must agree on all of it. They
    /// resolve from the same archive against the same trust anchor, and
    /// [`Debian::stage_layer`] additionally *requires* the map to match the one the
    /// base was configured under — a layer configured differently is not an increment
    /// over this base, it is a second bootstrap that happens to share a directory.
    /// Two sites configuring this by hand would drift silently, since the divergence
    /// shows up as a resolution result rather than as an error.
    ///
    /// The caller adds what is its own: [`ensure`](Self::ensure) the base package set,
    /// [`build_root`](BuildSandbox::build_root) the base layer, the component's
    /// packages, and any feed-forward pool.
    fn debian_builder(&self) -> DebianBuilder<'_> {
        let (primary, fallbacks) = self
            .mirrors
            .split_first()
            .expect("the constructor guarantees a non-empty mirror list");
        let mut builder = Debian::builder(&self.suite)
            .architecture(&self.arch)
            .mirror(primary)
            .components(COMPONENTS.split(','))
            // Declared, not defaulted. It is the library's default, but `stage_layer`
            // rejects a layer whose map differs from its base's, so stating it here is
            // what makes the two agree by construction rather than by both happening to
            // take the same default. `dpkg` needs the caller mapped to root inside.
            .identity_map(IdentityMap::Single);
        for fallback in fallbacks {
            builder = builder.mirror_fallback(fallback);
        }
        // A snapshot backstop's release is expired by design; accepting a
        // signed-but-stale release is a repository-wide posture, taken only when one
        // is in the list (as the rootfs node does).
        if !fallbacks.is_empty() {
            builder = builder.allow_stale_release(true);
        }
        // A vendored keyring makes the bootstrap portable to a non-Debian host;
        // without one the provisioner falls back to its embedded Debian archive
        // keyring.
        if let Some(keyring) = &self.keyring {
            builder = builder.keyring(keyring);
        }
        // Cached downloads survive the bootstrap, so re-provisioning a discarded base
        // — or staging a layer whose packages an earlier build already fetched —
        // refetches only what the cache does not already hold.
        if let Some(cache) = &self.cache_dir {
            builder = builder.cache_dir(cache);
        }
        builder
    }

    /// Bootstrap this base with `packages` if it is not already published, and record
    /// what it holds in a manifest headed `header`. Idempotent across builds.
    ///
    /// The package set is the caller's because it is the one thing that makes a base a
    /// *build sandbox* or a *packaging root*; everything else about provisioning one is
    /// the same, and is here.
    fn ensure<S: AsRef<str>>(
        &self,
        packages: &[S],
        header: &str,
        step: &Step,
    ) -> Result<(), EngineError> {
        // A published rootfs is a plain directory; `provision::ensure` fast-paths on
        // that and skips the bootstrap, so this is idempotent across builds.
        step.log(format!(
            "ensuring {} {} rootfs at {} (in-process Debian provisioner)",
            self.arch,
            self.suite,
            self.rootfs.display()
        ));
        let manifest = self.manifest_path();
        if discard_unrecordable_base(&self.rootfs, &manifest)? {
            step.log(format!(
                "discarding the {} rootfs at {}: no package manifest beside it, so it \
                 cannot state what it holds",
                self.arch,
                self.rootfs.display()
            ));
        }
        // The provisioner resolves the whole install closure — the base system (apt
        // included) plus `packages` and their dependencies — with its own resolver,
        // verifies the archive signature against the keyring, lays out and configures
        // the packages in an unprivileged cage, and writes an apt-usable rootfs (the
        // keyring, a `signed-by` sources line for every component, and an apt
        // sandbox-user posture matching the single-identity map). No apt ever runs inside
        // the tree — a build root's increment is resolved from outside it, against the
        // base's dpkg status — but the sources make the published base a usable Debian
        // userland rather than one that only works for this build.
        let mut debian = self
            .debian_builder()
            .include(packages.iter().map(AsRef::as_ref))
            .build()
            .map_err(|source| EngineError::Bootstrap {
                context: format!("configure the {} {} bootstrap", self.arch, self.suite),
                message: source.to_string(),
            })?;
        // The sink is bound for this one run rather than for the provisioner's life,
        // so its borrow of `step` and of `installed` ends when `ensure` returns.
        //
        // `Resolved` carries the closure the bootstrap itself resolved and is about to
        // install, and it is the only report of it — a resolution taken separately
        // would be a claim about a set that was never installed. It fires only when
        // this call bootstraps; a reused base already has its manifest on disk.
        let mut installed: Option<Plan> = None;
        let mut sink = |event: DebianEvent<'_>| {
            if let DebianEvent::Resolved { plan, .. } = &event {
                installed = Some((*plan).clone());
            }
            forward_bootstrap_event(step, event);
        };
        let outcome =
            provision::ensure(&self.rootfs, &mut debian.observe(&mut sink)).map_err(|source| {
                EngineError::Bootstrap {
                    context: format!("bootstrap the {} {} rootfs", self.arch, self.suite),
                    message: source.to_string(),
                }
            })?;
        if let Some(plan) = &installed {
            let count = crate::manifest::write(header, plan, &manifest)?;
            step.log(format!(
                "recorded the base's {count} packages, sha256-pinned from the plan, in {}",
                manifest.display()
            ));
        }
        // `Existing` means a prior build already published this rootfs; anything
        // else means this call produced it.
        if outcome == Provisioned::Existing {
            step.log(format!(
                "reusing {} rootfs at {}",
                self.arch,
                self.rootfs.display()
            ));
        } else {
            step.log(format!(
                "{} rootfs ready at {}",
                self.arch,
                self.rootfs.display()
            ));
        }
        Ok(())
    }
}

/// Rootless build sandbox: a Debian userland for the build's suite, bootstrapped and
/// entered without root, and layered per stage.
///
/// Its [`SandboxRole`] decides what it is — a target-arch root the userspace and ffmpeg
/// stages compile in, or a host-arch root carrying a cross toolchain the kernel, u-boot
/// and kmod stages compile in. Everything else about it is the same either way, which is
/// why it is one type.
///
/// The rootfs is bootstrapped once by [`ferroday_cage`]'s Debian provisioner and
/// reused as the read-only lower of every [`BuildRoot`]; each command runs in a
/// [`ferroday_cage::Cage`] with that overlay mounted as `/`, so a stage's writes land
/// in its own increment. Where the root's arch differs from the host's its binaries
/// execute via the `F`-flagged `qemu-user` binfmt handler with no interpreter copy;
/// where they match they run directly, which a cross root always does. See
/// the [module docs](self) for why the package stages always compile in here rather
/// than on the host, and [`PackagingSandbox`] for the root that archives a staged tree.
pub struct RootlessSandbox {
    /// What this sandbox is for, and so which architecture it is provisioned at and
    /// which packages its base carries.
    role: SandboxRole,
    /// The provisioned userland every [`BuildRoot`] overlays.
    base: SandboxBase,
    /// Directory each stage's overlay upper is created under — one subdirectory per
    /// stage, holding that stage's upper and the overlay's work area beside it.
    ///
    /// Supplied rather than derived from the base's tree because it is a *host
    /// requirement*: an unprivileged overlay records whiteouts in `user.*`
    /// extended attributes, which not every filesystem holds, and
    /// [`overlay_check`](crate::checks::overlay_check) probes this exact directory
    /// before a build starts. Passing it in is what keeps the directory `doctor`
    /// cleared and the directory a build uses the same one — both come from
    /// [`build_root_uppers`].
    uppers_dir: PathBuf,
}

impl RootlessSandbox {
    /// A build sandbox in `role`, rooted at `rootfs`, bootstrapping `suite`/`arch` from
    /// `mirrors` in order, verifying the archive with `keyring` (recommended; `None`
    /// uses the host apt trust store).
    ///
    /// `arch` is the architecture the root itself is provisioned at, which must be the
    /// one `role` implies — the target's for [`SandboxRole::Target`], the host's for
    /// [`SandboxRole::Cross`] — since `rootfs` has to be [`build_sandbox_dir`] of the
    /// same role and arch for the tree and its contents to agree.
    ///
    /// `mirrors` is the build's own resolved list
    /// ([`snapshot::resolve_mirrors`](crate::snapshot::resolve_mirrors)) rather than a
    /// fixed default, because the toolchain that compiles this build's `.deb`s lives in
    /// this rootfs: a `--snapshot pin` that fixed the image's userland to a point in
    /// time while this sandbox kept bootstrapping from the live mirror would pin the
    /// *output* packages and leave the *compiler* that produced them free to move,
    /// which is not what "pinned" reads as. An empty list falls back to
    /// [`crate::DEFAULT_MIRROR`] rather than failing: a caller that resolved no mirror
    /// expressed no preference.
    ///
    /// `cache_dir` is where downloaded `.deb`s are cached; `None` discards them with
    /// the bootstrap. `uppers_dir` is where each stage's overlay upper is created, and
    /// must be [`build_root_uppers`] of the build's work dir — the directory
    /// [`overlay_check`](crate::checks::overlay_check) probes.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        role: SandboxRole,
        rootfs: PathBuf,
        uppers_dir: PathBuf,
        suite: impl Into<String>,
        arch: impl Into<String>,
        mirrors: Vec<String>,
        keyring: Option<PathBuf>,
        cache_dir: Option<PathBuf>,
    ) -> Self {
        RootlessSandbox {
            role,
            base: SandboxBase::new(
                rootfs,
                suite.into(),
                arch.into(),
                mirrors,
                keyring,
                cache_dir,
            ),
            uppers_dir,
        }
    }
}

impl BuildSandbox for RootlessSandbox {
    fn describe(&self) -> String {
        match self.role {
            SandboxRole::Target => format!("rootless {}", self.base.arch),
            // Both arches, because which is which is the whole point of a cross root and
            // a log line naming one would read as the other.
            SandboxRole::Cross { target } => format!("cross {} -> {target}", self.base.arch),
        }
    }

    fn base_manifest(&self) -> Option<PathBuf> {
        self.base.published_manifest()
    }

    fn ensure_ready(&self, step: &Step) -> Result<(), EngineError> {
        self.base.ensure(
            &self.role.base_packages(&self.base.arch),
            self.role.manifest_header(),
            step,
        )
    }

    fn build_root(&self, spec: &BuildRootSpec, step: &Step) -> Result<BuildRoot, EngineError> {
        // The stage owns a directory, and the upper sits inside it. The overlay's work
        // area is a sibling of the upper (`.<name>.work`), so one directory holds both
        // and a single removal reclaims the pair — the library's own naming is then not
        // something this side has to reproduce.
        let stage_dir = self.uppers_dir.join(spec.stage);
        let upper = stage_dir.join("upper");
        // `stage_layer` creates the upper but does not clear it, so a leftover from a
        // hard-killed run would present that run's increment as this one's.
        discard_upper(&stage_dir);
        std::fs::create_dir_all(&stage_dir)
            .map_err(|source| EngineError::io(&stage_dir, source))?;
        step.log(format!(
            "staging the {} build root: {} package(s) over the {} base",
            spec.stage,
            spec.packages.len(),
            self.base.arch
        ));

        let mut builder = self
            .base
            .debian_builder()
            // The base is the overlay's lower and the resolver's already-installed set,
            // read from its own dpkg status — so the increment closes over only what the
            // base lacks, and the ids the resolver assumes match the files on disk.
            .base_layer(&self.base.rootfs)
            .include(spec.packages.iter().copied());
        // The build's own `.deb`s, when an earlier stage fed them forward. A real
        // repository rather than a push into the tree, so the resolver pulls each
        // package *and its transitive dependencies* in one resolution.
        if let Some(pool) = spec.pool {
            let repo = Repository::builder(&self.base.suite)
                .mirror(pool)
                // The component the pool publishes under; stated rather than left to the
                // builder's default so the two sides of the `dists/` layout are written
                // and read from the same word.
                .components(["main"])
                // A local pool of this build's own freshly-produced `.deb`s: apt's own
                // `file://` `[trusted=yes]` case. The provisioner refuses
                // `trust_unsigned` over `http://`, never a local path.
                .trust_unsigned(true)
                .name(spec.stage)
                .build()
                .map_err(|source| EngineError::Bootstrap {
                    context: format!("configure the {} build pool at {pool}", spec.stage),
                    message: source.to_string(),
                })?;
            builder = builder.repository(repo);
        }
        let mut debian = builder.build().map_err(|source| EngineError::Bootstrap {
            context: format!("configure the {} build root", spec.stage),
            message: source.to_string(),
        })?;

        // The increment's own resolution reports the plan it is about to install, the
        // same way the base bootstrap does — so the record describes the set the build
        // ran against rather than a second resolution's answer.
        let mut resolved: Option<Plan> = None;
        let mut sink = |event: DebianEvent<'_>| {
            if let DebianEvent::Resolved { plan, .. } = &event {
                resolved = Some((*plan).clone());
            }
            forward_bootstrap_event(step, event);
        };
        let layer = debian
            .observe(&mut sink)
            .stage_layer(&upper)
            .map_err(|source| EngineError::Bootstrap {
                context: format!("stage the {} build root at {}", spec.stage, upper.display()),
                message: source.to_string(),
            })?;
        // Every path into `stage_layer` resolves before it stages, so a staged layer
        // always reported its plan — including the empty-delta case, whose plan is
        // legitimately empty because the base already carried everything asked for.
        let plan = resolved.ok_or_else(|| EngineError::Bootstrap {
            context: format!("stage the {} build root", spec.stage),
            message: "the layer resolution reported no plan".into(),
        })?;
        step.log(format!(
            "{} build root ready: {} package(s) layered over the base",
            spec.stage,
            plan.packages.len()
        ));
        Ok(BuildRoot {
            base: self.base.rootfs.clone(),
            layer,
            plan,
        })
    }
}

/// The packaging root: a host-arch Debian userland carrying `dpkg` and `xz-utils`, in
/// which a staged tree becomes a `.deb`.
///
/// The u-boot and kmod stages assemble their package trees on the host — the layout,
/// the control text and the mode normalization are all host-side and pure — and then run
/// one `dpkg-deb --build` in here. That is the whole of what this root does, which is
/// why it has neither [`BuildSandbox::build_root`] nor an implementation of that trait:
///
/// - **It is never layered.** Its contents are fixed at bootstrap
///   (`dpkg` and `xz-utils`), so there is no per-stage increment to declare and no
///   undeclared-dependency hazard for an overlay to catch. Commands therefore run
///   directly in the base, under the same cage profile a [`BuildRoot`] run uses — and
///   an unprivileged overlay, which the compile sandbox requires, is not a host
///   requirement for a build that only packages.
/// - **It is host-arch.** `dpkg-deb` does not care what architecture the payload
///   targets, so packaging runs natively with no `qemu-user` in the path. Contrast the
///   build sandbox, which is target-arch because `dpkg-shlibdeps` must see the target's
///   libraries; archiving a pre-staged tree resolves nothing.
///
/// What it buys is that `dpkg-deb`'s version and its `liblzma` — which do shape the
/// archive bytes — become sha256-pinned packages resolved from the build's own mirror
/// list, like every other input, instead of a property of whichever distribution ran
/// the build.
///
/// The base is entered read-write, as every cage root is. Nothing written during a
/// packaging run lands in it: the archive goes to a bind-mounted host path and
/// `dpkg-deb`'s scratch to the cage's own `/tmp` tmpfs, so the tree stays what its
/// manifest says it is.
pub struct PackagingSandbox {
    /// The provisioned host-arch userland packaging commands run in.
    base: SandboxBase,
}

impl PackagingSandbox {
    /// A packaging root at `rootfs` for `suite`/`arch`, resolved from `mirrors` in order
    /// and verified with `keyring` (recommended; `None` uses the host apt trust store).
    ///
    /// `arch` is the **host's** Debian architecture, so the root runs natively;
    /// `rootfs` must be [`packaging_root_dir`] of the build's work dir, whose key
    /// covers everything below. `suite` is the build's
    /// [`packaging_suite`](boot2deb_core::model::ResolvedBuild::packaging_suite) — its
    /// image suite where it has one, the device's default otherwise — so a
    /// bootloader-only build and that board's image builds share one provisioned tree.
    ///
    /// `mirrors` is the build's own resolved list, for the reason
    /// [`RootlessSandbox::new`] takes one: under `--snapshot pin` the tool that archives
    /// the `.deb`s has to come from the same point-in-time archive their contents do.
    /// `cache_dir` is where downloaded `.deb`s are cached; `None` discards them with the
    /// bootstrap.
    pub fn new(
        rootfs: PathBuf,
        suite: impl Into<String>,
        arch: impl Into<String>,
        mirrors: Vec<String>,
        keyring: Option<PathBuf>,
        cache_dir: Option<PathBuf>,
    ) -> Self {
        PackagingSandbox {
            base: SandboxBase::new(
                rootfs,
                suite.into(),
                arch.into(),
                mirrors,
                keyring,
                cache_dir,
            ),
        }
    }

    /// Short label for logs, e.g. `packaging amd64 forky`.
    pub fn describe(&self) -> String {
        format!("packaging {} {}", self.base.arch, self.base.suite)
    }

    /// Ensure the root exists with `dpkg` and `xz-utils` present. Idempotent — the
    /// first call bootstraps and caches the tree, later calls reuse it.
    ///
    /// Called by the stages that package rather than once up front, so a build that
    /// packages nothing never provisions one.
    pub fn ensure_ready(&self, step: &Step) -> Result<(), EngineError> {
        self.base
            .ensure(PACKAGING_DEPS, PACKAGING_MANIFEST_HEADER, step)
    }

    /// The manifest of the packages this root carries: one `name version arch sha256`
    /// line per package, sha256-pinned from the plan the bootstrap resolved.
    ///
    /// The record of what archived the build's `.deb`s, and the counterpart of
    /// [`BuildSandbox::base_manifest`] for the compile side. `None` until
    /// [`ensure_ready`](Self::ensure_ready) has published a root, which a build that
    /// packages nothing never does.
    pub fn base_manifest(&self) -> Option<PathBuf> {
        self.base.published_manifest()
    }

    /// Run one command in this root per `spec`, streaming its output to `step` and
    /// mapping a non-zero exit to [`CommandFailed`](EngineError::CommandFailed).
    ///
    /// The command's `/` is the root itself — there is no overlay, because there is no
    /// increment — under the same cage profile and the same isolated network namespace
    /// every build command runs under. `spec`'s binds expose host paths at their host
    /// path, which is how the staged tree is read and the finished `.deb` written
    /// back.
    ///
    /// Requires [`ensure_ready`](Self::ensure_ready) to have published the root.
    pub fn run(&self, spec: &SandboxRun, step: &Step) -> Result<(), EngineError> {
        let cage = build_cage(baseline(&self.base.rootfs), spec)?;
        run_cage(cage, spec, step)
    }
}

/// Freeze one run into a [`Cage`] over a rooted [`baseline`]/[`baseline_overlay`]
/// profile: the command, its working directory, its extra environment, and its binds.
///
/// The argv check comes first. `cage` indexes `argv[0]` and slices `argv[1..]`, so
/// [`SandboxRun`]'s stated invariant is enforced at the boundary rather than reached as
/// a panic three frames in — and the struct is public, so a spec can arrive from outside
/// this crate.
///
/// Every run keeps the profile's [`Network::Isolated`] namespace (loopback only): a
/// build root resolves its packages before it is entered and compiles offline, so
/// nothing that runs in one has a reason to reach the network. Per-run `spec.env`
/// entries are applied after the profile's and override [`SANDBOX_ENV`] on collision,
/// and each bind is exposed at its host path, so artifacts written beside a source tree
/// land back on the host.
///
/// Takes the rooted profile rather than a root path, leaving "which root is this" with
/// the caller that knows the answer.
fn build_cage(profile: ferroday_cage::CageBuilder, spec: &SandboxRun) -> Result<Cage, EngineError> {
    if spec.argv.is_empty() {
        return Err(EngineError::EmptyArgv {
            context: spec.context.to_string(),
        });
    }
    let mut builder = profile
        .command(&spec.argv[0])
        .args(&spec.argv[1..])
        .current_dir(spec.work);
    for (key, value) in spec.env {
        builder = builder.env(key, value);
    }
    for bind in spec.binds {
        builder = builder.bind(bind, bind);
    }
    builder.build().map_err(|source| EngineError::Sandbox {
        context: spec.context.to_string(),
        source,
    })
}

/// Run a built [`Cage`], relaying its output to `step` and mapping a non-zero exit to
/// [`CommandFailed`](EngineError::CommandFailed) carrying the stderr tail.
fn run_cage(cage: Cage, spec: &SandboxRun, step: &Step) -> Result<(), EngineError> {
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

/// Reclaim a stage's build-root directory — its overlay upper and the work area beside
/// it — before a fresh layer is staged into it.
///
/// Best-effort and quiet: nothing here is a reason to fail a build. A build root is
/// disposable by construction, so the only thing a failure costs is the disk the stale
/// increment occupies, and the fresh `stage_layer` that follows reports any real problem
/// with the directory.
///
/// Through [`provision::remove`] rather than a plain recursive delete, because an
/// overlay leaves a mode-`0` directory in its work area that a plain delete cannot
/// descend into. This is the same public route [`BuildLayer`]'s own drop documents for a
/// caller that wants the removal to happen at a point it chooses.
fn discard_upper(stage_dir: &Path) {
    if stage_dir.exists() {
        let _ = provision::remove(stage_dir);
    }
}

/// Remove a base tree standing without the manifest that records what it holds,
/// reporting whether one was removed.
///
/// A base and its manifest are published together: only the bootstrap that installs
/// the packages reports the plan they were resolved from, so a tree left without one
/// can never state its own contents. Discarding it costs a re-bootstrap; keeping it
/// would compile the image's target `.deb`s in a toolchain the provenance cannot
/// describe, which is the thing the record exists to prevent.
///
/// Every file in the tree is owned by the caller — the sandbox bootstraps under the
/// single-identity map — so a plain recursive remove suffices, with no delegate to
/// re-enter a subordinate map.
///
/// Split out of [`BuildSandbox::ensure_ready`] so the condition is exercised without a
/// bootstrap: the call that follows it in `ensure_ready` re-creates the tree, which
/// makes the removal unobservable from outside.
fn discard_unrecordable_base(rootfs: &Path, manifest: &Path) -> Result<bool, EngineError> {
    if !rootfs.is_dir() || manifest.is_file() {
        return Ok(false);
    }
    std::fs::remove_dir_all(rootfs).map_err(|source| EngineError::io(rootfs, source))?;
    Ok(true)
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
/// from the record without either changing. [`baseline_overlay`] is the same profile
/// over an overlay root, and shares [`profile`] with this one for that reason.
pub(crate) fn baseline(rootfs: &Path) -> ferroday_cage::CageBuilder {
    profile().rootfs(rootfs)
}

/// The same profile as [`baseline`], rooted on an **overlay** of `lower` (a sandbox
/// base, read-only) and `upper` (a staged increment, writable) — the root a
/// [`BuildRoot`] runs its commands in.
///
/// Identical to [`baseline`] in every respect but how the root is composed, which is
/// what keeps a layered build's environment and managed mounts the same as a plain
/// one's. Expressed as a sibling rather than by calling `overlay_rootfs` on
/// [`baseline`]'s result: that would work — the two roots are mutually exclusive
/// builder fields and the last call wins — but it would mean naming a rootfs that is
/// then discarded, and "which root is this" is exactly the thing the layered shape
/// exists to make explicit.
pub(crate) fn baseline_overlay(lower: &Path, upper: &Path) -> ferroday_cage::CageBuilder {
    profile().overlay_rootfs(lower, upper)
}

/// The rootless profile with no root chosen yet: the declared environment, the network
/// and terminal postures, the lifetime, and the `PATH` lookup posture that [`baseline`]
/// and [`baseline_overlay`] share.
///
/// Split out so the two rooting modes cannot drift in anything but the root.
fn profile() -> ferroday_cage::CageBuilder {
    let mut builder = Cage::builder()
        // Declared, not inherited: see [`baseline`].
        .base_env(false)
        // Declared, not defaulted, and here rather than at each call site: an egress
        // surface is not something a build step should acquire because a library default
        // moved, and stating it in the profile is what puts it in the provenance record
        // — a value only the run sites set is a value [`resolved_inputs`] would report
        // the library's default for.
        .network(Network::Isolated)
        // A build is non-interactive by construction, which is already what the host
        // commands in `crate::build` state; this is the same statement for the
        // sandboxed ones. `/dev/null` gives standard input immediate end-of-file, and
        // — the reason it is here rather than a tidiness — puts the command in a
        // *session of its own*. Inherited, it stays in the caller's, where a maintainer
        // script or a build rule can open `/dev/tty` to read what is typed at the
        // operator's terminal and push characters into its input queue with `TIOCSTI`
        // for the operator's shell to run afterwards. Out of that session `/dev/tty`
        // fails with ENXIO. It also makes `isatty` deterministically false, which is one
        // fewer host property a compile's output can depend on. The output pair is
        // untouched: `run_with` supplies pipes, so it was never the operator's terminal.
        .stdin(Stdio::Null)
        // The cost of the session above, paid back: outside the caller's session a `^C`
        // no longer reaches the command, so the sandbox's lifetime is tied to boot2deb's
        // through a held descriptor instead. `^C` kills boot2deb, which is still in the
        // foreground process group, and the supervisor tears the sandbox down. Without
        // it an abandoned sandbox runs its compile to completion with nobody waiting.
        .stop_with_caller(true)
        // The stages pass bare tool names (`dpkg-buildpackage`, `make`, `apt-get`);
        // the cage resolves them against SANDBOX_ENV's `PATH` inside the root, like
        // a shell.
        .path_lookup(true);
    for (key, value) in SANDBOX_ENV {
        builder = builder.env(key, value);
    }
    builder
}

/// Recipe version of a [`SandboxRole::Target`] base — this module's own logic, folded
/// into the base key so a tree published under a different version is never reused.
///
/// Bump it when a change means a tree an earlier version published is no longer a base
/// this one may compile in, for a reason the rest of the key does not already capture.
/// The base is immutable and its manifest states exactly what the bootstrap resolved,
/// so what this guards is compiling in an environment whose own record contradicts it.
const BASE_STAGE_VERSION: u32 = 2;

/// Recipe version of a [`SandboxRole::Cross`] base, versioned independently of
/// [`BASE_STAGE_VERSION`] for the reason [`PACKAGING_STAGE_VERSION`] is: a reason to
/// retire one kind of tree is rarely a reason to retire another.
const CROSS_STAGE_VERSION: u32 = 1;

/// Recipe version of the packaging root, versioned independently of
/// [`BASE_STAGE_VERSION`] so a reason to retire one tree does not retire the other.
/// The two are provisioned from the same archive but hold different package sets for
/// different jobs.
const PACKAGING_STAGE_VERSION: u32 = 1;

/// Role token in a base tree's leaf name: the **packaging root**'s. The build sandboxes'
/// come from [`SandboxRole::token`].
///
/// The digest already separates the roots — it covers each one's package set, and no two
/// agree — so the token is for the reader: a work dir can hold three trees, and which is
/// which should be legible without hashing anything.
const PACKAGING_ROLE: &str = "package";

/// Where a build sandbox's rootfs lives, for a build whose scratch tree is `work_dir`.
///
/// `arch` is the architecture the root itself is provisioned at, which the role decides:
/// the *target's* for [`SandboxRole::Target`], the *host's* for [`SandboxRole::Cross`].
///
/// Keyed by role + arch + suite + a digest of **the mirror list it was bootstrapped
/// from, the package set it was bootstrapped with, and a base recipe version** — so one
/// host can serve several targets from one work dir, and a tree is only ever reused for
/// a base it actually is.
///
/// The digest is what makes the path honest rather than merely unique:
/// [`BuildSandbox::ensure_ready`] fast-paths on an existing directory and never re-checks
/// its contents. So each ingredient stops a specific wrong answer.
///
/// - **The mirrors.** Turning on `--snapshot pin` would otherwise reuse the sandbox a
///   previous live-mirror build left behind, while the output signature
///   ([`BuildEnv::sandbox_id`](crate::build::BuildEnv::sandbox_id)) asserted the
///   snapshot's toolchain. The tree and the claim would disagree, with the claim being
///   the one that keys the artifact cache.
/// - **The base package set.** Adding a package to it would otherwise leave every tree
///   in place without it, and the stages would fail on a tool the declaration says is
///   present. This is also what keeps the three roots for one arch and suite apart, and
///   what keeps two cross roots for different targets apart: each is a different package
///   set, and a tree holding one is not another.
/// - **The recipe version.** A tree can also stop being a valid base for a reason its
///   inputs do not name, which is what the recipe version is for.
///
/// A distinct base therefore gets a distinct tree, and the cost of any of these changing
/// is one extra bootstrap rather than a wrong answer.
pub fn build_sandbox_dir(
    work_dir: &Path,
    role: SandboxRole,
    arch: &str,
    suite: &str,
    mirrors: &[String],
) -> PathBuf {
    work_dir.join("sandbox").join(base_tree_name(
        role.token(),
        arch,
        suite,
        mirrors,
        &role.base_packages(arch),
        role.stage_version(),
    ))
}

/// Where the packaging root's rootfs lives, for a build whose scratch tree is `work_dir`.
///
/// Keyed exactly as [`build_sandbox_dir`] is, and for the same reasons — these are the
/// same kind of provisioned tree keyed by the same ingredients, differing in the package
/// set and the recipe version they name. `arch` here is the **host's**, since packaging
/// runs natively.
///
/// It sits beside the build sandboxes rather than under a directory of its own, so
/// `clean --sandbox` reclaims every provisioned tree a build made in one sweep.
pub fn packaging_root_dir(work_dir: &Path, arch: &str, suite: &str, mirrors: &[String]) -> PathBuf {
    work_dir.join("sandbox").join(base_tree_name(
        PACKAGING_ROLE,
        arch,
        suite,
        mirrors,
        PACKAGING_DEPS,
        PACKAGING_STAGE_VERSION,
    ))
}

/// The leaf name of a base tree from its ingredients: `<role>-<arch>-<suite>-<digest>`.
///
/// Pure and parameterized over the recipe, so what the digest covers is testable without
/// editing the constants it normally reads.
fn base_tree_name<S: AsRef<str>>(
    role: &str,
    arch: &str,
    suite: &str,
    mirrors: &[String],
    base_deps: &[S],
    version: u32,
) -> String {
    // Short digest, not the ingredients: a mirror list is long and contains characters no
    // directory name should carry. 12 hex characters is 48 bits — far past collision
    // range for the handful of bases one work dir ever sees. The `\0` separators keep two
    // different recipes from spelling one digest input. The role is in the *name* and not
    // in the digest on purpose: it is a label for the reader, and the package set it
    // labels is already covered — so two roles that somehow declared the same set would
    // share a tree, which is the correct answer rather than a collision.
    let deps: Vec<&str> = base_deps.iter().map(AsRef::as_ref).collect();
    let recipe = format!("{version}\0{}\0{}", mirrors.join("\n"), deps.join("\n"));
    let digest = crate::blobs::sha256_hex(recipe.as_bytes());
    format!("{role}-{arch}-{suite}-{}", &digest[..12])
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

/// The sandbox profile as provenance data: the posture it launches under, the declared
/// environment, and the mounts the sandbox library establishes *inside* the root every
/// build command runs in.
///
/// Resolved from the profile the stages themselves run in rather than restated, so the
/// record reports what a command actually sees — including the six `/dev` device nodes
/// and five `/dev` symlinks, and the resource limits and hardening controls in force,
/// none of which any accessor other than
/// [`Cage::resolved_inputs`](ferroday_cage::Cage::resolved_inputs) reports.
///
/// The profile is a function of the builder configuration alone — no mount in it names
/// the root — so it resolves against an empty stand-in root. That is what makes the
/// record the same for every build: a base image bootstraps no build-sandbox rootfs at
/// all, and its provenance still has to state the profile its rootfs customize ran under.
///
/// **No path a build chose is in the record, and deliberately so.** A package stage
/// compiles in an overlay of the sandbox base plus its own increment (a [`BuildRoot`]),
/// while the rootfs customize uses a plain tree; both are per-build paths, so recording
/// either would make the record a property of the machine. So the root contributes its
/// *kind* and nothing else, the mounts omit it entirely, and what matters is that the two
/// rooting modes agree on everything this function does record — which
/// `an_overlay_root_runs_under_the_same_profile_as_a_plain_one` holds.
///
/// A run's own additions are outside the record for the same reason: its working and
/// artifact binds are per-build paths, and the subordinate identity map the rootfs
/// customize adds is the one posture it does not take from this profile.
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
        posture: project_posture(&inputs),
        // Every variable is declared by SANDBOX_ENV from `&str` constants, so the lossy
        // conversion is exact for the environment this profile carries.
        env: inputs
            .env
            .iter()
            .map(|(name, value)| {
                (
                    name.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
            })
            .collect(),
        mounts: inputs.mounts.iter().map(project_mount).collect(),
    }
}

/// Project the resolved rooting, identity, network, streams, limits and hardening onto
/// the manifest's `[sandbox]`.
///
/// Every *enum arm* below names every field of its variant, for the reason
/// [`project_mount`]'s do: a field the sandbox library adds to a variant is a compile
/// error here rather than a silent omission from every later manifest. Where a named
/// field is bound to `_` that is a decision, not an oversight, and each such decision is
/// the same one — the value is a path the build chose or an id range the host allocated,
/// so it would make the record a property of the machine. The *kind* is what the record
/// can state, and it is what a reader can act on: `overlay` versus `plain` is the
/// difference between a layered build and a flat one whatever the paths were.
///
/// That promise covers the variants and not `ResolvedInputs` itself, which is a struct
/// read field by field — a field added there compiles here and is simply absent from the
/// manifest until this function reads it. Keeping the two apart is why the sentence
/// names arms rather than the projection as a whole.
fn project_posture(inputs: &ferroday_cage::ResolvedInputs) -> SandboxPosture {
    let root = match &inputs.root {
        ResolvedRoot::Host => "host",
        ResolvedRoot::Plain { path: _ } => "plain",
        ResolvedRoot::Overlay {
            lower: _,
            upper: _,
            work: _,
        } => "overlay",
        // The enum is `#[non_exhaustive]`, so a rooting mode this release cannot name is
        // still recorded as one it does not know rather than silently rendered as a mode
        // it is not.
        _ => "unknown",
    };
    let identity = match &inputs.identity {
        ResolvedIdentity::Caller => "caller",
        ResolvedIdentity::Single => "single",
        ResolvedIdentity::Ranged { uid: _, gid: _ } => "ranged",
        _ => "unknown",
    };
    let network = match inputs.network {
        Network::Isolated => "isolated",
        Network::Host => "host",
        Network::None => "none",
        _ => "unknown",
    };
    let mut posture = SandboxPosture {
        root: root.to_string(),
        identity: identity.to_string(),
        network: network.to_string(),
        streams: project_streams(&inputs.streams),
        // Overwritten below when the layer is compiled in; `unavailable` is the honest
        // value when it is not, and it is written either way — an absent key could not
        // be told from one written before the key existed.
        hardening: "unavailable".to_string(),
        seccomp_instructions: None,
        keep_capabilities: None,
        rlimits: inputs
            .rlimits
            .iter()
            .map(|limit| SandboxRlimit {
                resource: limit.resource.spelling().to_string(),
                soft: limit.soft.to_string(),
                hard: limit.hard.to_string(),
            })
            .collect(),
        landlock_fs: Vec::new(),
        landlock_net: Vec::new(),
    };
    match &inputs.hardening {
        ResolvedHardening::Unavailable => {}
        ResolvedHardening::Applied {
            landlock_fs,
            landlock_net,
            seccomp_instructions,
            keep_capabilities,
        } => {
            posture.hardening = "applied".to_string();
            posture.seccomp_instructions = *seccomp_instructions;
            posture.keep_capabilities = keep_capabilities.map(hex_access);
            posture.landlock_fs = landlock_fs
                .iter()
                .map(|grant| SandboxLandlockFs {
                    path: grant.path.display().to_string(),
                    access: hex_access(grant.access),
                })
                .collect();
            posture.landlock_net = landlock_net
                .iter()
                .map(|grant| SandboxLandlockNet {
                    port: grant.port,
                    access: hex_access(grant.access),
                })
                .collect();
        }
        // A hardening disposition this release cannot name still says so, rather than
        // reading as the compiled-out case it is not.
        _ => posture.hardening = "unknown".to_string(),
    }
    posture
}

/// Project the three standard streams onto the manifest's `[sandbox.streams]`.
///
/// A kind per stream and nothing more: [`ResolvedStdio::Fd`] names a descriptor the
/// launch supplied, which is a live resource of this run's and not something a record
/// can carry forward — the same rule the root and the identity map are recorded under.
fn project_streams(streams: &ResolvedStreams) -> SandboxStreams {
    SandboxStreams {
        stdin: stdio_kind(streams.stdin),
        stdout: stdio_kind(streams.stdout),
        stderr: stdio_kind(streams.stderr),
    }
}

/// One stream's disposition as the manifest spells it.
fn stdio_kind(stdio: ResolvedStdio) -> String {
    match stdio {
        ResolvedStdio::Inherit => "inherit",
        ResolvedStdio::Null => "null",
        ResolvedStdio::Fd => "fd",
        // `#[non_exhaustive]`, so a disposition this release cannot name is recorded as
        // one it does not know rather than rendered as a kind it is not.
        _ => "unknown",
    }
    .to_string()
}

/// One 64-bit kernel access mask in the manifest's form: `0x`-prefixed and 16 hex
/// digits, matching how [`hex_flags`] renders a mount's flag word — hex because these
/// are bit sets, and only hex diffs one bit at a time.
fn hex_access(bits: u64) -> String {
    format!("{bits:#018x}")
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
        ResolvedMount::Tmpfs {
            target: _,
            flags,
            data,
        } => SandboxMount {
            flags: Some(hex_flags(*flags)),
            options: data_string(data),
            ..base("tmpfs")
        },
        ResolvedMount::Procfs { target: _ } => base("procfs"),
        ResolvedMount::Devpts { target: _, data } => SandboxMount {
            options: data_string(data),
            ..base("devpts")
        },
        ResolvedMount::Bind {
            source,
            target: _,
            read_only,
        } => SandboxMount {
            source: Some(source.display().to_string()),
            read_only: Some(*read_only),
            ..base("bind")
        },
        ResolvedMount::Raw {
            source,
            target: _,
            fstype,
            flags,
            data,
        } => SandboxMount {
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
        DebianEvent::Resolved { plan, .. } => step.log(format!(
            "bootstrap resolved {} packages",
            plan.packages.len()
        )),
        DebianEvent::Downloading {
            package,
            index,
            total,
            ..
        } => step.log(format!("downloading {package} ({index}/{total})")),
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
                step.relay(stream, text.to_string());
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
        self.step.relay(stream, line);
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
        self.stderr_tail
            .iter()
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
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

/// A real, provisioned packaging root for the end-to-end tests that archive a `.deb`
/// through one.
///
/// Shared rather than per-test because provisioning is the expensive part: the tree
/// lives at a stable path under `target/`, so a second run reuses it, and the mutex
/// keeps two parallel tests from bootstrapping into the same directory at once. It is
/// the *build's* configuration — the shipped suite, the default mirror, the same
/// [`PACKAGING_DEPS`] — so what the tests exercise is the root a build would get.
///
/// `None` where the host cannot provision one (no network, no unprivileged
/// namespaces), through [`hosttool::require_ok`](crate::hosttool::require_ok) — so a
/// tool-minimal dev host skips with a note while a CI job that sets
/// `BOOT2DEB_REQUIRE_HOST_TOOLS` panics rather than passing quietly.
#[cfg(test)]
pub(crate) fn packaging_root_for_tests(step: &Step) -> Option<PackagingSandbox> {
    use std::sync::Mutex;
    static PROVISIONING: Mutex<()> = Mutex::new(());

    // Under `target/` rather than a `tempdir`, so the ~30 s bootstrap is paid once per
    // machine instead of once per run. `CARGO_TARGET_TMPDIR` would be the idiomatic
    // spelling but is set only for integration tests, and these are unit tests.
    let work = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../target/test-roots");
    // The suite the shipped boards package under, so the tests exercise the real
    // configuration rather than a stand-in.
    let (suite, mirrors) = ("forky", vec![DEFAULT_MIRROR.to_string()]);
    let arch = boot2deb_core::HostInfo::detect().debian_arch()?;
    let root = PackagingSandbox::new(
        packaging_root_dir(&work, arch, suite, &mirrors),
        suite,
        arch,
        mirrors,
        // No keyring: the provisioner falls back to its own embedded Debian archive
        // keyring, so the tests need no vendored blob resolved from the config root.
        None,
        Some(work.join("cache")),
    );
    // A poisoned lock means another test panicked mid-bootstrap; the tree it left is
    // still either published or absent, and `ensure_ready` handles both.
    let _guard = PROVISIONING.lock().unwrap_or_else(|e| e.into_inner());
    crate::hosttool::require_ok("provisioning a packaging root", root.ensure_ready(step))?;
    Some(root)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A layered build root's environment, managed mounts and launch posture are a plain
    /// rootfs's, exactly — the two rooting modes differ in the root and in nothing else.
    ///
    /// This is what lets the provenance record one profile for a build whose stages run
    /// in overlay roots: [`resolved_inputs`] resolves [`baseline`], so if
    /// [`baseline_overlay`] carried so much as one different variable, mount, limit or
    /// identity, the record would describe an environment no compile ran in.
    ///
    /// The rooting *kind* is the one field that must differ, and the test asserts that
    /// too: a record in which an overlay build and a flat one read identically would be
    /// hiding the difference rather than proving there is none.
    #[test]
    fn an_overlay_root_runs_under_the_same_profile_as_a_plain_one() {
        let tmp = tempfile::tempdir().unwrap();
        let (lower, upper) = (tmp.path().join("base"), tmp.path().join("upper"));
        std::fs::create_dir_all(&lower).unwrap();
        // An unprivileged overlay is a host capability `doctor` gates a media-accel
        // build on; where it is absent this assertion cannot be made at all.
        if let Some(blocker) = ferroday_cage::host::overlay_blocker(tmp.path()) {
            assert!(
                std::env::var_os("BOOT2DEB_REQUIRE_HOST_TOOLS").is_none(),
                "BOOT2DEB_REQUIRE_HOST_TOOLS is set but this host cannot establish an \
                 unprivileged overlay ({blocker}) — a CI job that gates media-accel \
                 builds on the overlay check must be able to establish one"
            );
            eprintln!("skipping: no unprivileged overlay on this host: {blocker}");
            return;
        }

        let resolve = |builder: ferroday_cage::CageBuilder| {
            project(
                builder
                    .command("true")
                    .build()
                    .expect("the profile resolves")
                    .resolved_inputs(),
            )
        };
        let plain = resolve(baseline(&lower));
        let overlaid = resolve(baseline_overlay(&lower, &upper));

        assert_eq!(plain.env, overlaid.env, "the declared environment differs");
        // Compared as the record renders them, since that is what a reader of two
        // images' provenance would diff.
        assert_eq!(plain.mounts, overlaid.mounts, "the managed mounts differ");
        assert_eq!(plain.posture.root, "plain");
        assert_eq!(plain.posture.identity, overlaid.posture.identity);
        assert_eq!(plain.posture.network, overlaid.posture.network);
        assert_eq!(plain.posture.hardening, overlaid.posture.hardening);
        assert_eq!(plain.posture.rlimits, overlaid.posture.rlimits);
        assert_eq!(
            overlaid.posture.root, "overlay",
            "the rooting kind is the one field the two modes must differ in"
        );
    }

    /// The posture the provenance records is the one boot2deb declares, resolved rather
    /// than restated: a network the profile leaves at a library default, or a hardening
    /// key omitted because the feature is compiled out, would each be a record that
    /// silently stopped describing the build.
    ///
    /// `network` is the load-bearing assertion. It is a *declared* posture — [`profile`]
    /// sets it so that this record can state it — and a build step acquiring an egress
    /// surface because a library default moved is exactly what declaring it prevents. The
    /// hardening key is the other: boot2deb selects no `hardening` feature, so
    /// `unavailable` is the honest value, and writing it is what keeps a record readable
    /// without knowing which builder wrote it.
    #[test]
    fn the_recorded_posture_is_the_one_boot2deb_declares() {
        let recorded = resolved_inputs().expect("the profile resolves on any Linux host");
        assert_eq!(
            recorded.posture.network, "isolated",
            "every sandboxed command runs with loopback only"
        );
        assert_eq!(
            recorded.posture.identity, "single",
            "the package stages keep the profile's single-identity map; the rootfs \
             customize adds the subordinate one per run"
        );
        assert_eq!(
            recorded.posture.root, "plain",
            "the stand-in root is a tree"
        );
        assert_eq!(
            recorded.posture.hardening, "unavailable",
            "boot2deb selects no hardening feature, and the record says so rather than \
             omitting the key"
        );
        assert!(recorded.posture.landlock_fs.is_empty());
        assert!(recorded.posture.landlock_net.is_empty());
        assert_eq!(recorded.posture.seccomp_instructions, None);
        assert_eq!(recorded.posture.keep_capabilities, None);
        // Declared for the same reason `network` is, and load-bearing for a second: a
        // `/dev/null` standard input puts the command in a session of its own, out of
        // reach of the operator's controlling terminal. Inherited, a maintainer script
        // could open `/dev/tty` and read from it — or push characters into its input
        // queue for the operator's shell to run afterwards.
        assert_eq!(
            recorded.posture.streams.stdin, "null",
            "a build is non-interactive by construction, and out of the caller's session"
        );
        // The profile's plan, not a claim about a compile: `run_with` attaches pipes at
        // each launch, which supersedes this pair at that call site.
        assert_eq!(recorded.posture.streams.stdout, "inherit");
        assert_eq!(recorded.posture.streams.stderr, "inherit");
    }

    /// A tree is reused only for a base it actually is: the package set and the recipe
    /// version reach the path alongside the mirrors.
    ///
    /// `ensure_ready` fast-paths on an existing directory and never inspects its
    /// contents, so anything that changes what a base *holds* has to change where it
    /// lives. Adding to `BASE_DEPS` otherwise leaves every existing tree standing
    /// without the new package, and a version bump otherwise cannot retire a tree at all
    /// — which is what v2 needs, since a v1 tree was mutated in place by the stage that
    /// used it and holds an unrecorded superset of its own manifest.
    #[test]
    fn the_base_recipe_reaches_the_tree_name() {
        let mirrors = vec![DEFAULT_MIRROR.to_string()];
        let deps: Vec<&str> = BASE_DEPS.to_vec();
        let name = |d: &[&str], v: u32| {
            base_tree_name(
                SandboxRole::Target.token(),
                "arm64",
                "forky",
                &mirrors,
                d,
                v,
            )
        };

        let base = name(&deps, BASE_STAGE_VERSION);
        assert_eq!(
            base,
            name(&deps, BASE_STAGE_VERSION),
            "stable for one recipe"
        );

        // A package added to the base set is a different base.
        let mut grown = deps.clone();
        grown.push("cmake");
        assert_ne!(base, name(&grown, BASE_STAGE_VERSION));
        // So is one removed, and order is not a set: the digest covers the list as
        // written, which is the conservative answer for a path that gates a rebuild.
        assert_ne!(base, name(&deps[1..], BASE_STAGE_VERSION));
        let mut reordered = deps.clone();
        reordered.reverse();
        assert_ne!(base, name(&reordered, BASE_STAGE_VERSION));

        // The version retires a tree whose ingredients are unchanged — the one thing
        // only it can express.
        assert_ne!(base, name(&deps, BASE_STAGE_VERSION - 1));

        // And the separators are load-bearing: a package set cannot be spelled as part
        // of the mirror list.
        assert_ne!(
            name(&["a", "b"], 1),
            base_tree_name(
                SandboxRole::Target.token(),
                "arm64",
                "forky",
                &["a".to_string()],
                &["b"],
                1,
            )
        );
    }

    /// A cross base names exactly one compiler for the target, and the choice of it is
    /// what keeps two targets — and a native root — in trees of their own.
    ///
    /// Asserted through [`build_sandbox_dir`] as well as the set, because the set only
    /// matters here insofar as it reaches the tree name: that is what decides whether a
    /// build re-provisions or silently compiles in the wrong root.
    #[test]
    fn a_cross_base_names_one_compiler_for_its_target() {
        let arm64 = SandboxRole::Cross { target: "arm64" }.base_packages("amd64");
        assert_eq!(arm64.first().unwrap(), "crossbuild-essential-arm64");
        // Plus the shared set, whose own `build-essential` is the *host* compiler kbuild
        // and u-boot build their `scripts/` and `mkimage` with.
        assert_eq!(arm64.len(), CROSS_DEPS.len() + 1);
        assert!(arm64.iter().any(|p| p == "build-essential"));

        // A root whose target is its own architecture adds nothing: `build-essential` is
        // already the compiler it wants, and declaring it twice would be a package set
        // that does not describe a tree.
        let native = SandboxRole::Cross { target: "amd64" }.base_packages("amd64");
        assert_eq!(
            native,
            CROSS_DEPS.iter().map(|p| p.to_string()).collect::<Vec<_>>()
        );

        let work = Path::new("/w");
        let mirrors = vec![DEFAULT_MIRROR.to_string()];
        let dir = |role| build_sandbox_dir(work, role, "amd64", "forky", &mirrors);
        let cross_arm64 = dir(SandboxRole::Cross { target: "arm64" });
        assert_ne!(cross_arm64, dir(SandboxRole::Cross { target: "armhf" }));
        assert_ne!(cross_arm64, dir(SandboxRole::Cross { target: "amd64" }));
        // And no cross tree is ever a target tree, whatever their package sets digest to.
        assert_ne!(cross_arm64, dir(SandboxRole::Target));
        let leaf = cross_arm64
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .to_string();
        assert!(leaf.starts_with("cross-amd64-forky-"), "{leaf}");
    }

    /// An empty argv is refused before anything is built, rather than reaching the
    /// `argv[0]` index inside the cage builder as a panic. [`SandboxRun`] is public, so
    /// a spec can arrive from outside this crate.
    #[test]
    fn an_empty_argv_is_an_error_not_a_panic() {
        let spec = SandboxRun {
            work: Path::new("/"),
            binds: &[],
            env: &[],
            argv: &[],
            context: "a spec with no program",
        };
        // Fails on the empty argv, not on the bogus root — the check runs first.
        match build_cage(baseline(Path::new("/nonexistent")), &spec).unwrap_err() {
            EngineError::EmptyArgv { context } => assert_eq!(context, "a spec with no program"),
            other => panic!("expected EmptyArgv, got {other}"),
        }
    }

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
    fn describe_names_the_arch_a_root_is_provisioned_at() {
        let sandbox = |role, arch: &str| {
            RootlessSandbox::new(
                role,
                PathBuf::from("/w/rootfs"),
                PathBuf::from("/w/sandbox/layers"),
                "forky",
                arch,
                vec![DEFAULT_MIRROR.to_string()],
                None,
                None,
            )
        };
        assert_eq!(
            sandbox(SandboxRole::Target, "arm64").describe(),
            "rootless arm64"
        );
        // A cross root names both arches, because naming one would read as the other:
        // this is an amd64 tree that emits arm64.
        assert_eq!(
            sandbox(SandboxRole::Cross { target: "arm64" }, "amd64").describe(),
            "cross amd64 -> arm64"
        );
    }

    /// The base's manifest is a sibling of the tree it describes, sharing its cache
    /// key — so a base and the record of what it holds move together, and a second
    /// suite or arch in one work dir gets its own record rather than overwriting one.
    #[test]
    fn the_base_manifest_sits_beside_the_tree_it_describes() {
        let work = Path::new("/w");
        let sb = |arch: &str, suite: &str| {
            RootlessSandbox::new(
                SandboxRole::Target,
                build_sandbox_dir(
                    work,
                    SandboxRole::Target,
                    arch,
                    suite,
                    &[DEFAULT_MIRROR.to_string()],
                ),
                PathBuf::from("/w/sandbox/layers"),
                suite,
                arch,
                vec![DEFAULT_MIRROR.to_string()],
                None,
                None,
            )
        };
        let arm64 = sb("arm64", "forky");
        let manifest = arm64.base.manifest_path();
        assert_eq!(manifest.parent(), arm64.base.rootfs.parent());
        assert_eq!(
            manifest.file_name().unwrap().to_str().unwrap(),
            format!(
                "{}.pkgs",
                arm64.base.rootfs.file_name().unwrap().to_str().unwrap()
            )
        );
        // Outside the tree, so no boot2deb file is visible to a build that sees it
        // as `/`.
        assert!(!manifest.starts_with(&arm64.base.rootfs));
        // One per base, not one per work dir.
        assert_ne!(manifest, sb("armhf", "forky").base.manifest_path());
        assert_ne!(manifest, sb("arm64", "sid").base.manifest_path());
    }

    /// A base with no manifest beside it cannot state its own contents, so it is not a
    /// base this build compiles in: `base_manifest` reports nothing for it, and the
    /// tree is discarded so the next `ensure_ready` bootstraps one that does.
    #[test]
    fn a_base_without_its_manifest_is_not_reusable() {
        let tmp = tempfile::tempdir().unwrap();
        let rootfs = tmp.path().join("arm64-forky-0123456789ab");
        std::fs::create_dir_all(rootfs.join("usr/bin")).unwrap();
        let sb = RootlessSandbox::new(
            SandboxRole::Target,
            rootfs.clone(),
            PathBuf::from("/w/sandbox/layers"),
            "forky",
            "arm64",
            vec![DEFAULT_MIRROR.to_string()],
            None,
            None,
        );
        let manifest = sb.base.manifest_path();
        assert_eq!(sb.base_manifest(), None);

        assert!(discard_unrecordable_base(&rootfs, &manifest).unwrap());
        assert!(!rootfs.exists(), "the unrecordable tree survived");

        // With a manifest beside it the tree is a base: it is kept, and it reports
        // itself.
        std::fs::create_dir_all(rootfs.join("usr/bin")).unwrap();
        std::fs::write(&manifest, "# empty\n").unwrap();
        assert!(!discard_unrecordable_base(&rootfs, &manifest).unwrap());
        assert!(
            rootfs.join("usr/bin").is_dir(),
            "a recorded base was removed"
        );
        assert_eq!(sb.base_manifest(), Some(manifest.clone()));

        // Nothing to discard where no tree stands, whether or not a manifest is left
        // over — so a first build is not a special case.
        std::fs::remove_dir_all(&rootfs).unwrap();
        assert!(!discard_unrecordable_base(&rootfs, &manifest).unwrap());
        std::fs::remove_file(&manifest).unwrap();
        assert!(!discard_unrecordable_base(&rootfs, &manifest).unwrap());
    }

    /// The sandbox bootstraps from the build's mirror list, not a fixed default.
    ///
    /// Under `--snapshot pin` the rootfs node fetches the image's userland from a
    /// point-in-time archive; the compiler that produces the target `.deb`s lives in
    /// *this* rootfs, so it has to come from the same place or "pinned" only covers
    /// half the image. An empty list is the one case that falls back, since a caller
    /// that resolved no mirror expressed no preference.
    #[test]
    fn the_sandbox_bootstraps_from_the_builds_own_mirrors() {
        let snapshot = "https://snapshot.debian.org/archive/debian/20260628T083000Z/";
        let sb = |mirrors: Vec<String>| {
            RootlessSandbox::new(
                SandboxRole::Target,
                PathBuf::from("/w/rootfs"),
                PathBuf::from("/w/sandbox/layers"),
                "forky",
                "arm64",
                mirrors,
                None,
                None,
            )
        };
        assert_eq!(sb(vec![snapshot.to_string()]).base.mirrors, vec![snapshot]);
        // A fallback snapshot keeps its order: live first, snapshot backfills.
        assert_eq!(
            sb(vec![DEFAULT_MIRROR.to_string(), snapshot.to_string()])
                .base
                .mirrors,
            vec![DEFAULT_MIRROR, snapshot]
        );
        assert_eq!(sb(vec![]).base.mirrors, vec![DEFAULT_MIRROR]);
    }

    /// A sandbox bootstrapped from one mirror set must never be reused for another.
    ///
    /// `ensure_ready` fast-paths on an existing directory and never asks where its
    /// contents came from, so the mirror list has to reach the *path* — otherwise
    /// turning on `--snapshot pin` would silently keep compiling in the tree a
    /// live-mirror build left behind, while the output signature claimed the
    /// snapshot's toolchain.
    #[test]
    fn a_different_mirror_set_gets_a_different_sandbox_tree() {
        let work = Path::new("/w");
        let live = vec![DEFAULT_MIRROR.to_string()];
        let snapshot = vec!["https://snapshot.debian.org/archive/debian/20260628T083000Z/".into()];
        let dir = |m: &[String]| build_sandbox_dir(work, SandboxRole::Target, "arm64", "forky", m);

        assert_eq!(dir(&live), dir(&live), "stable for one mirror set");
        assert_ne!(dir(&live), dir(&snapshot));
        // A fallback list is a third tree: it can resolve packages neither of the
        // single-mirror sandboxes can.
        let fallback = [live.clone(), snapshot.clone()].concat();
        assert_ne!(dir(&fallback), dir(&live));
        assert_ne!(dir(&fallback), dir(&snapshot));
        // Order matters — `fallback` and `pin` are different postures, not a set.
        let reversed = [snapshot.clone(), live.clone()].concat();
        assert_ne!(dir(&fallback), dir(&reversed));
        // Arch and suite still separate, and the name stays a readable path segment.
        assert_ne!(
            dir(&live),
            build_sandbox_dir(work, SandboxRole::Target, "armhf", "forky", &live)
        );
        assert_ne!(
            dir(&live),
            build_sandbox_dir(work, SandboxRole::Target, "arm64", "sid", &live)
        );
        let leaf = dir(&live);
        let leaf = leaf.file_name().unwrap().to_str().unwrap();
        assert!(leaf.starts_with("build-arm64-forky-"), "{leaf}");
        assert!(
            leaf.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "{leaf}"
        );

        // The packaging root is keyed the same way and lives in the same directory, so
        // the two must not collide even at one arch and suite — a tree holding `dpkg`
        // and `xz-utils` is not one a compile may run in, and vice versa. Their package
        // sets are what the digest separates them by; the role prefix is what tells a
        // reader which is which.
        let pkg = |m: &[String]| packaging_root_dir(work, "arm64", "forky", m);
        assert_eq!(pkg(&live).parent(), dir(&live).parent());
        assert_ne!(pkg(&live), dir(&live));
        let pkg_leaf = pkg(&live);
        let pkg_leaf = pkg_leaf.file_name().unwrap().to_str().unwrap();
        assert!(pkg_leaf.starts_with("package-arm64-forky-"), "{pkg_leaf}");
        // And it keys on its own inputs, not the build sandbox's.
        assert_ne!(pkg(&live), pkg(&snapshot));
        assert_ne!(
            pkg(&live),
            packaging_root_dir(work, "amd64", "forky", &live)
        );
        assert_ne!(pkg(&live), packaging_root_dir(work, "arm64", "sid", &live));
    }

    /// The signature input and the directory are the same string, because they are the
    /// same claim: "this `.deb` was archived by *that* root".
    ///
    /// Two functions deriving it independently could drift, and the drift would be
    /// invisible — a build would provision one tree while the artifact cache keyed on
    /// another, which is exactly what the tree name's digest exists to prevent.
    #[test]
    fn the_packaging_identity_is_the_tree_it_names() {
        let mirrors = vec![DEFAULT_MIRROR.to_string()];
        let id = crate::build::packaging_identity("amd64", "forky", &mirrors);
        assert_eq!(
            id,
            packaging_root_dir(Path::new("/w"), "amd64", "forky", &mirrors)
                .file_name()
                .unwrap()
                .to_str()
                .unwrap(),
            "the identity and the tree name disagree"
        );
        // Independent of the work dir: the same root on two machines is the same root.
        assert_eq!(
            id,
            crate::build::packaging_identity("amd64", "forky", &mirrors)
        );
        // And it moves with every ingredient that changes what the root holds.
        assert_ne!(
            id,
            crate::build::packaging_identity("arm64", "forky", &mirrors)
        );
        assert_ne!(
            id,
            crate::build::packaging_identity("amd64", "sid", &mirrors)
        );
        assert_ne!(
            id,
            crate::build::packaging_identity(
                "amd64",
                "forky",
                &["https://snapshot.debian.org/archive/debian/20260628T083000Z/".to_string()]
            )
        );
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
                assert!(
                    source.starts_with("/dev/"),
                    "the profile binds a host path: {source}"
                );
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
        for link in [
            "/dev/stdin",
            "/dev/stdout",
            "/dev/stderr",
            "/dev/fd",
            "/dev/ptmx",
        ] {
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
