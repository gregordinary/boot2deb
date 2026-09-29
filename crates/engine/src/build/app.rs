//! The app compile node: builds one feature-declared
//! [`App`] from its pinned trees into a `.deb`. The image
//! installs it in place of the archive's build of the same package.
//!
//! A [`DotnetDeb`](boot2deb_core::model::AppBuild::DotnetDeb) app is built by its own
//! Debian packaging, the way its upstream builds its release packages. The work
//! directory `<work>/app/<name>/` is laid out for that:
//!
//! - `build/` — the packaging tree at its pinned commit, with the series'
//!   `app_packaging` scope applied.
//! - `build/<source_dir>/` — the app's own tree at its pinned commit, with the `app`
//!   scope applied. It sits where the packaging's `debian/rules` expects it.
//! - `feed/` — the NuGet folder feed: exactly the packages the lock's sidecar pins.
//! - `dotnet/` — the SDK's home and the restore's packages folder, discarded after.
//! - `*.deb` — what `dpkg-buildpackage` writes beside the packaging tree.
//!
//! The build runs in the host-architecture cross root. The SDK runs natively there and
//! publishes for the target, and the packaging's `dh_strip` finds the target's
//! binutils. It runs offline like every build command, so the restore resolves against
//! the folder feed alone. `update` runs the one networked restore that produces the
//! pins ([`resolve_nuget`]).
//!
//! Side effects: git fetches, the sandboxed `dpkg-buildpackage`, and the artifact store.

use super::{
    apply_series_scope, deb_names, fetch_commit, fold_patch_series, pick_deb, purge_stage_debs,
    restore_stage_outputs, reuse_or_refresh_tree, sanitize_deb_version, stage_artifact,
    store_stage_outputs, ApplyScope, BuildEnv, PatchScope, PatchSource, SeriesIdentity,
};
use crate::dotnet::{dotnet_env, read_packages_folder, DotnetCache};
use crate::error::EngineError;
use crate::event::{EventSink, Step};
use crate::sandbox::{BuildRootSpec, BuildSandbox, SandboxRun};
use crate::signature::{SignatureBuilder, SignatureManifest};
use boot2deb_core::lock::{AppPin, DotnetPins, GitPin, Lock, PatchesPin};
use boot2deb_core::model::{dotnet_rid, App, AppBuild, Arch, DotnetDebBuild};
use boot2deb_core::nuget::NugetManifest;
use std::path::{Path, PathBuf};

/// Recipe version of the prepared tree, bumped when how the trees are fetched,
/// laid out or patched changes, so a stale tree is not reused.
const TREE_STAGE_VERSION: u32 = 1;

/// Recipe version of the built `.deb`, bumped when how the build runs changes, so an
/// artifact built the old way is not restored.
const OUTPUT_STAGE_VERSION: u32 = 1;

/// The epoch every app's deb version carries. The rootfs solve and a device's
/// `apt upgrade` both prefer the highest version across their repositories, and the
/// archive's own build of the app carries none, so this build is preferred over every
/// release the archive publishes later.
const DEB_EPOCH: &str = "1";

/// The build dependencies layered over the cross root for a `dotnet-deb` app.
///
/// They are the sequencer the packaging's `debian/rules` runs, and the ICU its
/// `dh_gencontrol` override reads the soname from, which the SDK also loads. The cross
/// base carries the rest: `dpkg-dev`, the target's binutils, and the CA store.
pub fn layer_packages() -> &'static [&'static str] {
    &["debhelper", "libicu-dev"]
}

/// The artifact-store node for an app, `app:<name>`: the key the store files the
/// built `.deb` under, and the node `why-rebuild` reports.
pub fn node_name(name: &str) -> String {
    format!("app:{name}")
}

/// The name of the app's build root, `app-<name>`: the directory its overlay upper is
/// staged under. Not the node name, whose `:` an overlay mount's options cannot carry.
fn root_stage(name: &str) -> String {
    format!("app-{name}")
}

/// The app's work directory, `<work>/app/<name>`.
pub fn stage_dir(work_dir: &Path, name: &str) -> PathBuf {
    work_dir.join("app").join(name)
}

/// The prepared packaging tree the build runs in, `<work>/app/<name>/build`.
pub fn tree_dir(work_dir: &Path, name: &str) -> PathBuf {
    stage_dir(work_dir, name).join("build")
}

/// The version of the deb an app's build produces:
/// `<epoch>:<release>+g<commit12>`, and `.p<patches12>` after it when the app applies
/// a series.
///
/// The release is the app's ref with its `v` dropped, which is what the packaging's
/// own version check reads back out. The commit keeps two builds of one release
/// from another tree apart, and the patches commit two builds of one tree with
/// different patches.
pub fn deb_version(pin: &AppPin) -> String {
    let release = pin.reference.strip_prefix('v').unwrap_or(&pin.reference);
    let mut upstream = format!("{release}+g{}", short(&pin.commit));
    if let Some(p) = &pin.patches {
        upstream.push_str(&format!(".p{}", short(&p.commit)));
    }
    format!("{DEB_EPOCH}:{}", sanitize_deb_version(&upstream))
}

/// First twelve characters of a commit.
fn short(commit: &str) -> &str {
    &commit[..commit.len().min(12)]
}

/// The app's lock entry and its `dotnet-deb` pins, or the reason the lock cannot
/// build it.
pub fn dotnet_pins<'a>(
    lock: &'a Lock,
    name: &str,
) -> Result<(&'a AppPin, &'a DotnetPins), EngineError> {
    let missing = |missing| EngineError::MissingAppPin {
        app: name.to_string(),
        missing,
    };
    let pin = lock
        .apps
        .iter()
        .find(|p| p.name == name)
        .ok_or_else(|| missing("no [[apps]] entry"))?;
    let dotnet = pin
        .dotnet
        .as_ref()
        .ok_or_else(|| missing("no [apps.dotnet] table"))?;
    Ok((pin, dotnet))
}

/// Tier-1 signature of an app's prepared tree: the two commits, where the app tree
/// sits, and the applied series. A pin or patch change restamps it, so the tree is
/// re-fetched rather than reused.
pub fn tree_manifest(
    name: &str,
    pin: &AppPin,
    dotnet: &DotnetPins,
    source_dir: &str,
    series: SeriesIdentity,
) -> SignatureManifest {
    let mut b = SignatureBuilder::new(&node_name(name), TREE_STAGE_VERSION);
    b.fold_scalar("commit", &pin.commit)
        .fold_scalar("packaging.commit", &dotnet.packaging.commit)
        .fold_scalar("source_dir", source_dir);
    fold_patch_series(&mut b, pin.patches.as_ref(), series);
    b.manifest()
}

/// What an app's built `.deb` depends on besides its tree, folded into its Tier-2
/// output key by [`output_manifest`].
pub struct OutputKeyInputs<'a> {
    /// The app as resolved.
    pub app: &'a App,
    /// Its `dotnet-deb` build declaration.
    pub build: &'a DotnetDebBuild,
    /// Its lock entry.
    pub pin: &'a AppPin,
    /// Its `dotnet-deb` pins.
    pub dotnet: &'a DotnetPins,
    /// How the applied series is identified (pinned, or a co-dev fingerprint).
    pub series: SeriesIdentity<'a>,
    /// The target's Debian architecture.
    pub arch: &'a str,
    /// The image suite.
    pub suite: &'a str,
    /// The cross root's identity ([`BuildEnv::toolchain_id`]), which the build runs in.
    pub toolchain_id: &'a str,
    /// The build host's Debian architecture, which selects the SDK tarball.
    pub host_arch: &'a str,
}

/// Tier-2 output signature of an app's `.deb`. On a hit the store restores the deb
/// rather than rebuilding.
///
/// It folds the tree signature as a dependency, then everything else that reaches the
/// bytes:
///
/// - The SDK release, and the tarball this host runs.
/// - The NuGet set, by the sidecar digest the build verified the sidecar against.
/// - The project, the deb name, and the version the changelog stamps.
/// - The target and the suite.
/// - The cross root the build ran in, and the packages layered over it.
pub fn output_manifest(inputs: &OutputKeyInputs) -> SignatureManifest {
    let tree = tree_manifest(
        &inputs.app.name,
        inputs.pin,
        inputs.dotnet,
        &inputs.build.source_dir,
        inputs.series,
    )
    .signature();
    let mut b = SignatureBuilder::new(
        &format!("{}:out", node_name(&inputs.app.name)),
        OUTPUT_STAGE_VERSION,
    );
    b.fold_dep(&tree)
        .fold_scalar("deb", &inputs.app.deb)
        .fold_scalar("version", &deb_version(inputs.pin))
        .fold_scalar("project", &inputs.build.project)
        .fold_scalar("sdk.version", &inputs.dotnet.sdk.version)
        .fold_scalar(
            "sdk.sha512",
            inputs
                .dotnet
                .sdk
                .sha512
                .get(inputs.host_arch)
                .map(String::as_str)
                .unwrap_or("none"),
        )
        .fold_scalar("nuget", &inputs.dotnet.nuget.manifest_sha256)
        .fold_scalar("arch", inputs.arch)
        .fold_scalar("suite", inputs.suite)
        .fold_scalar("toolchain", inputs.toolchain_id)
        .fold_scalar("host_arch", inputs.host_arch)
        .fold_set("build_deps", layer_packages());
    b.manifest()
}

/// Where an app's two trees come from, and the series applied to them.
struct TreeSource<'a> {
    name: &'a str,
    app: &'a GitPin,
    packaging: &'a GitPin,
    source_dir: &'a str,
    patches: Option<PatchSource<'a>>,
}

/// Fetch the packaging tree into `tree`, lay the app's tree at its `source_dir`, and
/// apply both of the series' app scopes, gated on the app's ref. Every source file's
/// modification time is then set to the app commit's timestamp, since the build
/// records some of them in what it publishes. Returns that timestamp.
///
/// On failure `tree` is removed, so a later run's reuse check never sees a half-made
/// one.
fn prepare_tree(src: &TreeSource, tree: &Path, step: &Step) -> Result<u64, EngineError> {
    let result = prepare_tree_inner(src, tree, step);
    if result.is_err() {
        let _ = std::fs::remove_dir_all(tree);
    }
    result
}

fn prepare_tree_inner(src: &TreeSource, tree: &Path, step: &Step) -> Result<u64, EngineError> {
    fetch_commit(
        &src.packaging.source,
        &src.packaging.reference,
        &src.packaging.commit,
        &format!("{} packaging", src.name),
        tree,
        step,
    )?;
    // The packaging tree is patched before the app's tree is laid inside it. `git am`
    // refuses a tree with uncommitted changes, and the app's tree sits where the
    // packaging names a submodule, which reads as a modification once it is there.
    let target = format!("{} @ {}", src.name, src.app.reference);
    apply_app_scope(src, tree, PatchScope::AppPackaging, &target, step)?;
    let app_dir = tree.join(src.source_dir);
    // The packaging tree names the app's tree as a submodule, so its checkout leaves
    // an empty directory where the app's tree goes.
    if app_dir.exists() {
        std::fs::remove_dir_all(&app_dir).map_err(|s| EngineError::io(&app_dir, s))?;
    }
    fetch_commit(
        &src.app.source,
        &src.app.reference,
        &src.app.commit,
        src.name,
        &app_dir,
        step,
    )?;
    apply_app_scope(src, &app_dir, PatchScope::App, &target, step)?;
    let epoch = crate::git::commit_epoch(&app_dir, &src.app.commit)?;
    set_source_mtimes(tree, epoch)?;
    Ok(epoch)
}

/// Apply one of the series' two app scopes to `dir`, gated on the app's ref, and log
/// how many patches it took.
fn apply_app_scope(
    src: &TreeSource,
    dir: &Path,
    scope: PatchScope,
    target: &str,
    step: &Step,
) -> Result<(), EngineError> {
    let n = apply_series_scope(
        &ApplyScope {
            tree: dir,
            patches: src.patches,
            scope,
            target,
            gate_reference: Some(&src.app.reference),
        },
        step,
    )?;
    if let Some(p) = src.patches {
        step.log(format!(
            "{}: applied {n} {} patch(es) ({})",
            src.name,
            scope.tree_label(),
            p.pin.series.join(", ")
        ));
    }
    Ok(())
}

/// Set every regular file under `dir` to modification time `epoch`, skipping git
/// metadata.
///
/// A self-contained .NET publish writes the modification time of each static web
/// asset into a manifest it ships (`*.staticwebassets.endpoints.json`, as a
/// `Last-Modified` header). A fresh checkout's times are the time of the checkout.
/// With this, the entries for assets read from the source tree carry the commit's time
/// instead. The `.gz` and `.br` variants the SDK compresses during the build still
/// carry the time they were written.
fn set_source_mtimes(dir: &Path, epoch: u64) -> Result<(), EngineError> {
    let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(epoch);
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        for entry in std::fs::read_dir(&d).map_err(|s| EngineError::io(&d, s))? {
            let entry = entry.map_err(|s| EngineError::io(&d, s))?;
            let path = entry.path();
            let kind = entry.file_type().map_err(|s| EngineError::io(&path, s))?;
            if kind.is_dir() {
                if entry.file_name() != ".git" {
                    stack.push(path);
                }
            } else if kind.is_file() {
                std::fs::File::open(&path)
                    .and_then(|f| f.set_modified(when))
                    .map_err(|s| EngineError::io(&path, s))?;
            }
        }
    }
    Ok(())
}

/// The `debian/changelog` entry the build is versioned from.
///
/// The packaging's own release tooling generates this file rather than committing it,
/// so the build writes it: the source package name the packaging's `debian/control`
/// declares, the [`deb_version`], and a trailer dated at the app commit.
fn changelog(source: &str, version: &str, pin: &AppPin, epoch: u64) -> String {
    let mut what = format!(
        "Built by boot2deb from {} {} ({})",
        pin.name, pin.reference, pin.commit
    );
    if let Some(p) = &pin.patches {
        what.push_str(&format!(
            ", with patch series {} at {}",
            p.series.join(", "),
            p.commit
        ));
    }
    format!(
        "{source} ({version}) unstable; urgency=medium\n\n  * {what}.\n\n -- boot2deb <boot2deb@localhost>  {}\n",
        boot2deb_core::datetime::format_rfc2822(epoch)
    )
}

/// The `Source:` name a packaging tree's `debian/control` declares.
fn control_source(tree: &Path) -> Result<String, EngineError> {
    let control = tree.join("debian/control");
    let text = std::fs::read_to_string(&control).map_err(|s| EngineError::io(&control, s))?;
    text.lines()
        .find_map(|l| l.strip_prefix("Source:").map(|v| v.trim().to_string()))
        .filter(|s| !s.is_empty())
        .ok_or_else(|| EngineError::ArtifactMissing {
            what: "a Source: field".into(),
            location: control.display().to_string(),
        })
}

/// Inputs of [`build_app`].
pub struct AppOptions<'a> {
    /// The app as resolved.
    pub app: &'a App,
    /// The `patches` checkout the app's series is read from, or `None` when it applies
    /// none. Its `version` is the app's pinned ref.
    pub patches: Option<PatchSource<'a>>,
    /// The NuGet sidecar the lock pins, already verified against its digest.
    pub nuget: &'a NugetManifest,
    /// The SDK and NuGet caches.
    pub dotnet: &'a DotnetCache,
    /// The build host's Debian architecture.
    pub host_arch: &'a str,
    /// Recipe work directory.
    pub work_dir: &'a Path,
    /// Where the built `.deb` is staged.
    pub out_dir: &'a Path,
    /// The artifact store, or `None` when it is disabled.
    pub store: Option<&'a Path>,
}

/// What [`build_app`] produced.
pub struct AppArtifacts {
    /// The app's `.deb`, staged in the out dir.
    pub deb: PathBuf,
}

/// Build one app into its `.deb`, or restore it from the artifact store.
///
/// `cross` is the host-architecture root the build runs in. It is acquired only on a
/// store miss, so a fully cached app provisions nothing.
pub fn build_app(
    lock: &Lock,
    opts: &AppOptions,
    arch: Arch,
    env: &BuildEnv,
    cross: &dyn BuildSandbox,
    sink: &dyn EventSink,
) -> Result<AppArtifacts, EngineError> {
    let name = opts.app.name.as_str();
    let node = node_name(name);
    let step = Step::start(sink, node.clone());
    let AppBuild::DotnetDeb(build) = &opts.app.build;
    let (pin, dotnet) = dotnet_pins(lock, name)?;
    let suite = lock
        .rootfs
        .as_ref()
        .expect("an app is built only for an image, which pins a rootfs")
        .suite
        .as_str();
    let target_arch = arch.debian_arch();
    let series_fp = super::dev_series_fingerprint(opts.patches, PatchScope::App)
        .into_iter()
        .chain(super::dev_series_fingerprint(
            opts.patches,
            PatchScope::AppPackaging,
        ))
        .collect::<Vec<_>>();
    let series = super::series_identity(opts.patches, &series_fp);
    let out_sig = output_manifest(&OutputKeyInputs {
        app: opts.app,
        build,
        pin,
        dotnet,
        series,
        arch: target_arch,
        suite,
        toolchain_id: &env.toolchain_id,
        host_arch: opts.host_arch,
    })
    .signature();
    if let Some(restored) =
        restore_stage_outputs(opts.store, &node, &out_sig, opts.out_dir, &["deb"], &step)?
    {
        step.finish();
        return Ok(AppArtifacts {
            deb: restored[0].clone(),
        });
    }

    let stage = stage_dir(opts.work_dir, name);
    let tree = tree_dir(opts.work_dir, name);
    std::fs::create_dir_all(&stage).map_err(|s| EngineError::io(&stage, s))?;
    let sdk = opts
        .dotnet
        .ensure_sdk(name, &dotnet.sdk, opts.host_arch, &step)?;
    step.progress(10);

    let app_pin = GitPin {
        source: pin.source.clone(),
        reference: pin.reference.clone(),
        commit: pin.commit.clone(),
    };
    let src = TreeSource {
        name,
        app: &app_pin,
        packaging: &dotnet.packaging,
        source_dir: &build.source_dir,
        patches: opts.patches,
    };
    let tree_sig = tree_manifest(name, pin, dotnet, &build.source_dir, series);
    let mut epoch = None;
    reuse_or_refresh_tree(&tree, &tree_sig, name, &step, || {
        epoch = Some(prepare_tree(&src, &tree, &step)?);
        Ok(())
    })?;
    let epoch = match epoch {
        Some(e) => e,
        None => crate::git::commit_epoch(&tree.join(&build.source_dir), &pin.commit)?,
    };
    step.progress(25);

    let feed = stage.join("feed");
    opts.dotnet.materialize_feed(opts.nuget, &feed, &step)?;
    step.progress(40);

    let version = deb_version(pin);
    let source = control_source(&tree)?;
    let changelog_path = tree.join("debian/changelog");
    std::fs::write(&changelog_path, changelog(&source, &version, pin, epoch))
        .map_err(|s| EngineError::io(&changelog_path, s))?;

    cross.ensure_ready(&step)?;
    let root = cross.build_root(
        &BuildRootSpec {
            packages: layer_packages(),
            pool: None,
            stage: &root_stage(name),
        },
        &step,
    )?;
    step.progress(50);

    // A fresh home and packages folder per build, so nothing a previous build
    // restored can stand in for a package the feed lacks.
    let scratch = stage.join("dotnet");
    if scratch.exists() {
        std::fs::remove_dir_all(&scratch).map_err(|s| EngineError::io(&scratch, s))?;
    }
    std::fs::create_dir_all(scratch.join("home")).map_err(|s| EngineError::io(&scratch, s))?;
    let deb_prefix = format!("{}_", opts.app.deb);
    purge_stage_debs(&stage, &[&deb_prefix])?;

    let mut build_env = dotnet_env(&sdk, &scratch);
    // An MSBuild property from the environment: the restore resolves against the
    // folder feed and nothing else, whatever sources the tree's own NuGet config names.
    build_env.push(("RestoreSources".into(), feed.display().to_string()));
    build_env.push(("SOURCE_DATE_EPOCH".into(), epoch.to_string()));
    build_env.push((
        "DEB_BUILD_OPTIONS".into(),
        format!("noddebs nocheck parallel={}", env.jobs()),
    ));
    let argv: Vec<String> = [
        "dpkg-buildpackage",
        "-us",
        "-uc",
        // The architecture-dependent packages only: the server, not the web client
        // this build takes from the archive.
        "-B",
        // Build dependencies are the layer above, not the packaging's declared set,
        // which names the web client's Node.js toolchain as well.
        "-d",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain([format!("--host-arch={target_arch}")])
    .collect();
    let binds = [stage.clone(), sdk.clone()];
    let context = format!("dpkg-buildpackage {name}");
    root.run(
        &SandboxRun {
            work: &tree,
            binds: &binds,
            env: &build_env,
            argv: &argv,
            context: &context,
            probe: None,
        },
        &step,
    )?;
    step.progress(90);

    let built =
        pick_deb(&deb_names(&stage)?, &deb_prefix).ok_or_else(|| EngineError::ArtifactMissing {
            what: format!("{} .deb", opts.app.deb),
            location: stage.display().to_string(),
        })?;
    let built = stage.join(built);
    purge_stage_debs(opts.out_dir, &[&deb_prefix])?;
    let deb = stage_artifact(opts.out_dir, &built)?;
    store_stage_outputs(opts.store, &node, &out_sig, &[("deb", &built)], &step)?;
    step.log(format!("{name}: built {}", deb.display()));
    step.progress(100);
    step.finish();
    Ok(AppArtifacts { deb })
}

/// Inputs of [`resolve_nuget`].
pub struct ResolveNuget<'a> {
    /// The app as resolved.
    pub app: &'a App,
    /// The app's tree, as `update` has just pinned it.
    pub app_pin: &'a GitPin,
    /// The packaging tree, as `update` has just pinned it.
    pub packaging: &'a GitPin,
    /// The app's patch-series pin, when it applies one.
    pub patches_pin: Option<&'a PatchesPin>,
    /// The `patches` checkout the series is read from. Required when `patches_pin` is
    /// set.
    pub patches_root: Option<&'a Path>,
    /// The target architecture the restore resolves runtime packs for.
    pub target_arch: Arch,
    /// The build host's Debian architecture, which selects the SDK tarball.
    pub host_arch: &'a str,
    /// Recipe work directory. The restore works under `<work>/app/<name>/resolve`.
    pub work_dir: &'a Path,
    /// The SDK and NuGet caches.
    pub dotnet: &'a DotnetCache,
}

/// Restore a `dotnet-deb` app's packages from its pinned trees with the network. What
/// the restore downloaded becomes the [`NugetManifest`] a build restores from offline.
/// It is the one networked command boot2deb runs, and only `update` runs it.
///
/// The restore evaluates the project exactly as the packaging's publish does: release
/// configuration, the target's runtime identifier, and self-contained. The pinned set
/// is therefore the one the build needs. A build that needed a package outside it
/// would fail its offline restore rather than reach the network.
pub fn resolve_nuget(
    spec: &ResolveNuget,
    cross: &dyn BuildSandbox,
    sink: &dyn EventSink,
) -> Result<NugetManifest, EngineError> {
    let name = spec.app.name.as_str();
    let step = Step::start(sink, format!("{}:nuget", node_name(name)));
    let AppBuild::DotnetDeb(build) = &spec.app.build;
    let target_arch = spec.target_arch.debian_arch();
    let rid = dotnet_rid(target_arch).ok_or_else(|| {
        EngineError::Config(boot2deb_core::ConfigError::AppArchUnsupported {
            app: name.to_string(),
            feature: "(resolved)".to_string(),
            arch: target_arch.to_string(),
        })
    })?;
    let sdk = spec
        .dotnet
        .ensure_sdk(name, &build.sdk, spec.host_arch, &step)?;

    let dir = stage_dir(spec.work_dir, name).join("resolve");
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|s| EngineError::io(&dir, s))?;
    }
    std::fs::create_dir_all(dir.join("dotnet/home")).map_err(|s| EngineError::io(&dir, s))?;
    let patches = match (spec.patches_pin, spec.patches_root) {
        (Some(pin), Some(root)) => Some(PatchSource {
            root,
            pin,
            dev: false,
            version: &spec.app_pin.reference,
        }),
        _ => None,
    };
    let tree = dir.join("build");
    prepare_tree(
        &TreeSource {
            name,
            app: spec.app_pin,
            packaging: spec.packaging,
            source_dir: &build.source_dir,
            patches,
        },
        &tree,
        &step,
    )?;

    cross.ensure_ready(&step)?;
    let root = cross.build_root(
        &BuildRootSpec {
            packages: layer_packages(),
            pool: None,
            stage: &root_stage(name),
        },
        &step,
    )?;
    let scratch = dir.join("dotnet");
    // The properties the packaging's `dotnet publish --configuration Release
    // --self-contained --runtime <rid>` evaluates under. `RuntimeIdentifier` is set
    // directly rather than through `restore --runtime`, which sets only the plural
    // `RuntimeIdentifiers`: a self-contained project with no singular identifier takes
    // the build host's, and the restore would then pin the host's runtime packs too.
    let argv: Vec<String> = [
        "dotnet".to_string(),
        "restore".to_string(),
        build.project.clone(),
        format!("-p:RuntimeIdentifier={rid}"),
        "-p:SelfContained=true".to_string(),
        "-p:Configuration=Release".to_string(),
    ]
    .into_iter()
    .collect();
    let binds = [dir.clone(), sdk.clone()];
    let context = format!("dotnet restore {name}");
    root.run_networked(
        &SandboxRun {
            work: &tree.join(&build.source_dir),
            binds: &binds,
            env: &dotnet_env(&sdk, &scratch),
            argv: &argv,
            context: &context,
            probe: None,
        },
        &step,
    )?;
    let manifest = read_packages_folder(&scratch.join("packages"), rid, name)?;
    step.log(format!(
        "{name}: the restore pinned {} NuGet package(s) for {rid}",
        manifest.packages.len()
    ));
    step.finish();
    Ok(manifest)
}

#[cfg(test)]
mod tests {
    use super::*;
    use boot2deb_core::lock::NugetPin;
    use boot2deb_core::model::{DotnetSdk, GitSource};

    fn app() -> App {
        App {
            name: "jellyfin".into(),
            git: "https://x/jellyfin.git".into(),
            git_ref: "v12.1".into(),
            deb: "jellyfin-server".into(),
            patch_series: vec!["jellyfin".into()],
            patches_url: Some("https://x/patches.git".into()),
            patches_ref: Some("main".into()),
            build: AppBuild::DotnetDeb(DotnetDebBuild {
                packaging: GitSource {
                    git: "https://x/jellyfin-packaging.git".into(),
                    git_ref: "v12.1-1".into(),
                },
                source_dir: "jellyfin-server".into(),
                project: "Jellyfin.Server".into(),
                sdk: sdk(),
            }),
        }
    }

    fn sdk() -> DotnetSdk {
        DotnetSdk {
            version: "10.0.401".into(),
            sha512: [("amd64".to_string(), "a".repeat(128))]
                .into_iter()
                .collect(),
        }
    }

    fn pin() -> AppPin {
        AppPin {
            name: "jellyfin".into(),
            source: "https://x/jellyfin.git".into(),
            reference: "v12.1".into(),
            commit: "ee91c75e7".to_string() + &"0".repeat(31),
            patches: Some(PatchesPin {
                series: vec!["jellyfin".into()],
                source: "https://x/patches.git".into(),
                reference: "main".into(),
                commit: "4350b1a5e405".to_string() + &"0".repeat(28),
            }),
            dotnet: Some(DotnetPins {
                packaging: GitPin {
                    source: "https://x/jellyfin-packaging.git".into(),
                    reference: "v12.1-1".into(),
                    commit: "3".repeat(40),
                },
                sdk: sdk(),
                nuget: NugetPin {
                    manifest: "x.jellyfin.nuget.lock".into(),
                    manifest_sha256: "4".repeat(64),
                },
            }),
        }
    }

    /// The version carries the epoch that keeps the archive's own build from
    /// replacing this one, the release the packaging reads back out, and both
    /// commits that can change the bytes.
    #[test]
    fn the_deb_version_carries_the_epoch_release_and_both_commits() {
        assert_eq!(deb_version(&pin()), "1:12.1+gee91c75e7000.p4350b1a5e405");
        let mut unpatched = pin();
        unpatched.patches = None;
        assert_eq!(deb_version(&unpatched), "1:12.1+gee91c75e7000");
    }

    fn key(pin: &AppPin, host: &str) -> String {
        let app = app();
        let AppBuild::DotnetDeb(build) = &app.build;
        output_manifest(&OutputKeyInputs {
            app: &app,
            build,
            pin,
            dotnet: pin.dotnet.as_ref().unwrap(),
            series: SeriesIdentity::Pinned,
            arch: "arm64",
            suite: "forky",
            toolchain_id: "cross-amd64-arm64-forky-x",
            host_arch: host,
        })
        .signature()
        .as_str()
        .to_string()
    }

    /// Every input that reaches the deb's bytes moves its key: the patches, the NuGet
    /// set, the SDK release. A new packaging commit moves it through the tree. The
    /// build host's architecture moves it too, since it selects the SDK tarball.
    #[test]
    fn every_input_of_the_build_moves_the_output_key() {
        let base = key(&pin(), "amd64");
        assert_eq!(
            base,
            key(&pin(), "amd64"),
            "the key is a function of its inputs"
        );

        let mut patched = pin();
        patched.patches.as_mut().unwrap().commit = "5".repeat(40);
        let mut nuget = pin();
        nuget.dotnet.as_mut().unwrap().nuget.manifest_sha256 = "6".repeat(64);
        let mut sdk = pin();
        sdk.dotnet.as_mut().unwrap().sdk.version = "10.0.402".into();
        let mut packaging = pin();
        packaging.dotnet.as_mut().unwrap().packaging.commit = "7".repeat(40);
        for (what, moved) in [
            ("patches", key(&patched, "amd64")),
            ("nuget", key(&nuget, "amd64")),
            ("sdk", key(&sdk, "amd64")),
            ("packaging", key(&packaging, "amd64")),
            ("host", key(&pin(), "arm64")),
        ] {
            assert_ne!(moved, base, "{what} must move the key");
        }
    }

    /// The build root's name becomes an overlay upper directory, and an overlay
    /// mount's options cannot carry a `:` or a `,`. The node name has a `:`.
    #[test]
    fn the_build_root_is_named_without_what_an_overlay_cannot_carry() {
        assert_eq!(node_name("jellyfin"), "app:jellyfin");
        let stage = root_stage("jellyfin");
        assert!(!stage.contains(':') && !stage.contains(','), "{stage}");
    }

    #[test]
    fn the_changelog_names_the_source_version_and_what_was_built() {
        let text = changelog("jellyfin", "1:12.1+gabc", &pin(), 1_767_225_600);
        let mut lines = text.lines();
        assert_eq!(
            lines.next().unwrap(),
            "jellyfin (1:12.1+gabc) unstable; urgency=medium"
        );
        assert!(text.contains("with patch series jellyfin at 4350b1a5e405"));
        assert!(
            text.ends_with(" -- boot2deb <boot2deb@localhost>  Thu, 01 Jan 2026 00:00:00 +0000\n")
        );
    }

    #[test]
    fn source_mtimes_are_set_to_the_commit_time_outside_git_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("a/b.txt");
        std::fs::create_dir_all(file.parent().unwrap()).unwrap();
        std::fs::write(&file, "x").unwrap();
        let git = tmp.path().join(".git/HEAD");
        std::fs::create_dir_all(git.parent().unwrap()).unwrap();
        std::fs::write(&git, "ref").unwrap();
        let before = std::fs::metadata(&git).unwrap().modified().unwrap();
        set_source_mtimes(tmp.path(), 1_767_225_600).unwrap();
        let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_767_225_600);
        assert_eq!(std::fs::metadata(&file).unwrap().modified().unwrap(), when);
        assert_eq!(std::fs::metadata(&git).unwrap().modified().unwrap(), before);
    }
}
