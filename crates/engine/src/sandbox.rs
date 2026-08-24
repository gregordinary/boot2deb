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
//! `Depends: librga2` only because that deb — and its `shlibs` — is present in the
//! root it compiles in. It gets there through the stage's own
//! [`BuildRootSpec::pool`]: the build publishes its userspace `.deb`s as a trusted
//! `file://` repository, and the stage's build root resolves them out of it like any
//! other package.
//!
//! A stage never mutates the environment it builds in. The base is bootstrapped once
//! and then read-only; each stage declares its build-dependencies, gets a
//! [`BuildRoot`] — the base plus that stage's increment, layered on with an
//! unprivileged overlay — and drops it when it is done. So a build root is a function
//! of what the stage declared, not of which builds ran in the directory before it, and
//! an undeclared build-dependency fails immediately instead of compiling against a
//! leftover.
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
use ferroday_cage::provision::debian::BuildLayer;
use ferroday_cage::provision::debian::{
    Debian, DebianBuilder, DebianEvent, Plan, Repository, Stream as DebianStream,
};
use ferroday_cage::provision::{self, Provisioned};
use ferroday_cage::{Cage, IdentityMap, Network, Observer, ResolvedMount};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};

/// Base packages installed at bootstrap — the minimum to run `dpkg-buildpackage`.
/// Stage-specific build-deps are layered over this set, per stage, by
/// [`BuildSandbox::build_root`].
const BASE_DEPS: &[&str] = &[
    "build-essential",
    "ca-certificates",
    "dpkg-dev",
    "debhelper",
    "fakeroot",
    "pkg-config",
];

/// Header line of the build sandbox base's package manifest
/// ([`BuildSandbox::base_manifest`]).
///
/// Distinct from the rootfs manifest's, so the two files are never mistaken for one
/// another: they describe different trees — what the image *carries* against what
/// *compiled* it — and only one of them is a reproducibility contract.
const BASE_MANIFEST_HEADER: &str =
    "Solved build-sandbox base manifest: the toolchain that compiled this build's \
     target .debs, as name version arch sha256.";

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

/// Rootless sandbox: a Debian userland for the build's suite and arch, bootstrapped
/// and entered without root.
///
/// The rootfs is bootstrapped once by [`ferroday_cage`]'s Debian provisioner and
/// reused as the read-only lower of every [`BuildRoot`]; each command runs in a
/// [`ferroday_cage::Cage`] with that overlay mounted as `/`, so a stage's writes land
/// in its own increment. On a cross host the target's binaries execute via the `F`-flagged
/// `qemu-user` binfmt handler with no interpreter copy; on a matching-arch host they
/// run directly. See
/// the [module docs](self) for why the package stages always build in here rather
/// than on the host.
pub struct RootlessSandbox {
    /// Target-arch rootfs directory — bootstrapped once, reused across builds (the
    /// seed of the base-rootfs cache).
    rootfs: PathBuf,
    /// Directory each stage's overlay upper is created under — one subdirectory per
    /// stage, holding that stage's upper and the overlay's work area beside it.
    ///
    /// Supplied rather than derived from [`rootfs`](Self::rootfs) because it is a
    /// *host requirement*: an unprivileged overlay records whiteouts in `user.*`
    /// extended attributes, which not every filesystem holds, and
    /// [`overlay_check`](crate::checks::overlay_check) probes this exact directory
    /// before a build starts. Passing it in is what keeps the directory `doctor`
    /// cleared and the directory a build uses the same one — both come from
    /// [`build_root_uppers`].
    uppers_dir: PathBuf,
    /// Debian suite to bootstrap (e.g. `forky`).
    suite: String,
    /// Debian architecture to bootstrap (e.g. `arm64`).
    arch: String,
    /// Ordered mirror list the rootfs is bootstrapped from — the same list the rootfs
    /// node fetches the *image's* userland from
    /// ([`snapshot::resolve_mirrors`](crate::snapshot::resolve_mirrors)). Non-empty.
    ///
    /// Shared, not defaulted, because the toolchain that compiles the target `.deb`s
    /// lives in this rootfs: a `--snapshot pin` that fixed the image's userland to a
    /// point in time while this sandbox kept bootstrapping from the live mirror would
    /// pin the *output* packages and leave the *compiler* that produced them free to
    /// move, which is not what "pinned" reads as.
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

impl RootlessSandbox {
    /// A sandbox rooted at `rootfs`, bootstrapping `suite`/`arch` from `mirrors` in
    /// order, verifying the archive with `keyring` (recommended; `None` uses the host
    /// apt trust store).
    ///
    /// `mirrors` is the build's own resolved list
    /// ([`snapshot::resolve_mirrors`](crate::snapshot::resolve_mirrors)) rather than a
    /// fixed default, because the toolchain that compiles the target `.deb`s lives in
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
    pub fn new(
        rootfs: PathBuf,
        uppers_dir: PathBuf,
        suite: impl Into<String>,
        arch: impl Into<String>,
        mirrors: Vec<String>,
        keyring: Option<PathBuf>,
        cache_dir: Option<PathBuf>,
    ) -> Self {
        RootlessSandbox {
            rootfs,
            uppers_dir,
            suite: suite.into(),
            arch: arch.into(),
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
    /// userland a build sees as `/` — a boot2deb file in it would be visible to every
    /// compile — and because the manifest has to be readable without entering it. It
    /// shares the tree's cache key ([`sandbox_rootfs_dir`]), so a base and its record
    /// move together.
    fn base_manifest_path(&self) -> PathBuf {
        let mut name = self.rootfs.file_name().unwrap_or_default().to_os_string();
        name.push(".pkgs");
        self.rootfs.with_file_name(name)
    }

    /// The provisioner configuration **both** the base bootstrap and every build root
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
    /// The caller adds what is its own: [`ensure_ready`](BuildSandbox::ensure_ready)
    /// the base package set, [`build_root`](BuildSandbox::build_root) the base layer,
    /// the component's packages, and any feed-forward pool.
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
}

impl BuildSandbox for RootlessSandbox {
    fn describe(&self) -> String {
        format!("rootless {}", self.arch)
    }

    fn base_manifest(&self) -> Option<PathBuf> {
        let path = self.base_manifest_path();
        path.is_file().then_some(path)
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
        let manifest = self.base_manifest_path();
        if discard_unrecordable_base(&self.rootfs, &manifest)? {
            step.log(format!(
                "discarding the {} rootfs at {}: no package manifest beside it, so it \
                 cannot state what it holds",
                self.arch,
                self.rootfs.display()
            ));
        }
        // The provisioner resolves the whole install closure — the base system (apt
        // included) plus `BASE_DEPS` and their dependencies — with its own resolver,
        // verifies the archive signature against the keyring, lays out and configures
        // the packages in an unprivileged cage, and writes an apt-usable rootfs (the
        // keyring, a `signed-by` sources line for every component, and an apt
        // sandbox-user posture matching the single-identity map). No apt ever runs inside
        // the tree — a build root's increment is resolved from outside it, against the
        // base's dpkg status — but the sources make the published base a usable Debian
        // userland rather than one that only works for this build.
        let mut debian = self
            .debian_builder()
            .include(BASE_DEPS.iter().copied())
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
            let count = crate::manifest::write(BASE_MANIFEST_HEADER, plan, &manifest)?;
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
            self.arch
        ));

        let mut builder = self
            .debian_builder()
            // The base is the overlay's lower and the resolver's already-installed set,
            // read from its own dpkg status — so the increment closes over only what the
            // base lacks, and the ids the resolver assumes match the files on disk.
            .base_layer(&self.rootfs)
            .include(spec.packages.iter().copied());
        // The build's own `.deb`s, when an earlier stage fed them forward. A real
        // repository rather than a push into the tree, so the resolver pulls each
        // package *and its transitive dependencies* in one resolution.
        if let Some(pool) = spec.pool {
            let repo = Repository::builder(&self.suite)
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
            base: self.rootfs.clone(),
            layer,
            plan,
        })
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
        // Declared, not defaulted: an egress surface is not something a build step
        // should acquire because a library default moved.
        .network(Network::Isolated)
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

/// The rootless profile with no root chosen yet: the declared environment and the
/// `PATH` lookup posture that [`baseline`] and [`baseline_overlay`] share.
///
/// Split out so the two rooting modes cannot drift in anything but the root.
fn profile() -> ferroday_cage::CageBuilder {
    let mut builder = Cage::builder()
        // Declared, not inherited: see [`baseline`].
        .base_env(false)
        // The stages pass bare tool names (`dpkg-buildpackage`, `make`, `apt-get`);
        // the cage resolves them against SANDBOX_ENV's `PATH` inside the root, like
        // a shell.
        .path_lookup(true);
    for (key, value) in SANDBOX_ENV {
        builder = builder.env(key, value);
    }
    builder
}

/// Recipe version of the sandbox base — bumped when a tree an earlier version published
/// is no longer a base this one may compile in, for a reason the rest of the path does
/// not already capture.
///
/// v2: the base is immutable. Under v1 the stage that used it installed its build-deps
/// *into* it, so a v1 tree holds an unrecorded superset of [`BASE_DEPS`] — the packages
/// of whichever recipes ran in that work dir — while the manifest beside it states only
/// what the bootstrap resolved. Reusing one would compile in an environment its own
/// record contradicts, and would resolve an increment against packages no declaration
/// asked for.
const BASE_STAGE_VERSION: u32 = 2;

/// Where the build sandbox's rootfs lives, for a build whose scratch tree is `work_dir`.
///
/// Keyed by arch + suite + a digest of **the mirror list it was bootstrapped from, the
/// package set it was bootstrapped with, and a base recipe version** — so one host can
/// serve several targets from one work dir, and a tree is only ever reused for a base it
/// actually is.
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
///   present.
/// - **The recipe version.** A tree can also stop being a valid base for a reason its
///   inputs do not name, which is what the recipe version is for.
///
/// A distinct base therefore gets a distinct tree, and the cost of any of these changing
/// is one extra bootstrap rather than a wrong answer.
pub fn sandbox_rootfs_dir(work_dir: &Path, arch: &str, suite: &str, mirrors: &[String]) -> PathBuf {
    work_dir.join("sandbox").join(base_tree_name(
        arch,
        suite,
        mirrors,
        BASE_DEPS,
        BASE_STAGE_VERSION,
    ))
}

/// The leaf name of a base tree from its ingredients: `<arch>-<suite>-<digest>`.
///
/// Pure and parameterized over the recipe, so what the digest covers is testable without
/// editing the constants it normally reads.
fn base_tree_name(
    arch: &str,
    suite: &str,
    mirrors: &[String],
    base_deps: &[&str],
    version: u32,
) -> String {
    // Short digest, not the ingredients: a mirror list is long and contains characters no
    // directory name should carry. 12 hex characters is 48 bits — far past collision
    // range for the handful of bases one work dir ever sees. The `\0` separators keep two
    // different recipes from spelling one digest input.
    let recipe = format!(
        "{version}\0{}\0{}",
        mirrors.join("\n"),
        base_deps.join("\n")
    );
    let digest = crate::blobs::sha256_hex(recipe.as_bytes());
    format!("{arch}-{suite}-{}", &digest[..12])
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

/// The sandbox profile as provenance data: the declared environment, and the mounts the
/// sandbox library establishes *inside* the root every build command runs in.
///
/// Resolved from the profile the stages themselves run in rather than restated, so the
/// record reports what a command actually sees — including the six `/dev` device nodes
/// and five `/dev` symlinks, which the sandbox library establishes and which no accessor
/// other than [`Cage::resolved_inputs`](ferroday_cage::Cage::resolved_inputs) reports.
///
/// The profile is a function of the builder configuration alone — no mount in it names
/// the root — so it resolves against an empty stand-in root. That is what makes the
/// record the same for every build: a base image bootstraps no build-sandbox rootfs at
/// all, and its provenance still has to state the profile its rootfs customize ran under.
///
/// **The root itself is not among the mounts, and deliberately so.** A package stage
/// compiles in an overlay of the sandbox base plus its own increment (a [`BuildRoot`]),
/// while the rootfs customize uses a plain tree; both are per-build paths, so recording
/// either would make the record a property of the machine. What matters is that the two
/// rooting modes agree on everything this function *does* record, which
/// `an_overlay_root_runs_under_the_same_profile_as_a_plain_one` holds.
///
/// A run's own additions are outside the record for the same reason: its working and
/// artifact binds are per-build paths.
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
                (
                    name.to_string_lossy().into_owned(),
                    value.to_string_lossy().into_owned(),
                )
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

#[cfg(test)]
mod tests {
    use super::*;

    /// A layered build root's environment and managed mounts are a plain rootfs's,
    /// exactly — the two rooting modes differ in the root and in nothing else.
    ///
    /// This is what lets the provenance record one profile for a build whose stages run
    /// in overlay roots: [`resolved_inputs`] resolves [`baseline`], so if
    /// [`baseline_overlay`] carried so much as one different variable or mount, the
    /// record would describe an environment no compile ran in.
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
        let name = |d: &[&str], v: u32| base_tree_name("arm64", "forky", &mirrors, d, v);

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
            base_tree_name("arm64", "forky", &["a".to_string()], &["b"], 1)
        );
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
    fn describe_names_the_target_arch() {
        let sb = RootlessSandbox::new(
            PathBuf::from("/w/rootfs"),
            PathBuf::from("/w/sandbox/layers"),
            "forky",
            "arm64",
            vec![DEFAULT_MIRROR.to_string()],
            None,
            None,
        );
        assert_eq!(sb.describe(), "rootless arm64");
    }

    /// The base's manifest is a sibling of the tree it describes, sharing its cache
    /// key — so a base and the record of what it holds move together, and a second
    /// suite or arch in one work dir gets its own record rather than overwriting one.
    #[test]
    fn the_base_manifest_sits_beside_the_tree_it_describes() {
        let work = Path::new("/w");
        let sb = |arch: &str, suite: &str| {
            RootlessSandbox::new(
                sandbox_rootfs_dir(work, arch, suite, &[DEFAULT_MIRROR.to_string()]),
                PathBuf::from("/w/sandbox/layers"),
                suite,
                arch,
                vec![DEFAULT_MIRROR.to_string()],
                None,
                None,
            )
        };
        let arm64 = sb("arm64", "forky");
        let manifest = arm64.base_manifest_path();
        assert_eq!(manifest.parent(), arm64.rootfs.parent());
        assert_eq!(
            manifest.file_name().unwrap().to_str().unwrap(),
            format!(
                "{}.pkgs",
                arm64.rootfs.file_name().unwrap().to_str().unwrap()
            )
        );
        // Outside the tree, so no boot2deb file is visible to a build that sees it
        // as `/`.
        assert!(!manifest.starts_with(&arm64.rootfs));
        // One per base, not one per work dir.
        assert_ne!(manifest, sb("armhf", "forky").base_manifest_path());
        assert_ne!(manifest, sb("arm64", "sid").base_manifest_path());
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
            rootfs.clone(),
            PathBuf::from("/w/sandbox/layers"),
            "forky",
            "arm64",
            vec![DEFAULT_MIRROR.to_string()],
            None,
            None,
        );
        let manifest = sb.base_manifest_path();
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
                PathBuf::from("/w/rootfs"),
                PathBuf::from("/w/sandbox/layers"),
                "forky",
                "arm64",
                mirrors,
                None,
                None,
            )
        };
        assert_eq!(sb(vec![snapshot.to_string()]).mirrors, vec![snapshot]);
        // A fallback snapshot keeps its order: live first, snapshot backfills.
        assert_eq!(
            sb(vec![DEFAULT_MIRROR.to_string(), snapshot.to_string()]).mirrors,
            vec![DEFAULT_MIRROR, snapshot]
        );
        assert_eq!(sb(vec![]).mirrors, vec![DEFAULT_MIRROR]);
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
        let dir = |m: &[String]| sandbox_rootfs_dir(work, "arm64", "forky", m);

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
            sandbox_rootfs_dir(work, "armhf", "forky", &live)
        );
        assert_ne!(dir(&live), sandbox_rootfs_dir(work, "arm64", "sid", &live));
        let leaf = dir(&live);
        let leaf = leaf.file_name().unwrap().to_str().unwrap();
        assert!(leaf.starts_with("arm64-forky-"), "{leaf}");
        assert!(
            leaf.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-'),
            "{leaf}"
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
