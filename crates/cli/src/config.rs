//! Helpers over the [`ConfigRoot`] shared by several commands: the root's own
//! structural check, target resolution, the early config preflight, and the
//! search-path lookups (fragments, board `.dts`, apt keyrings, overlay trees) plus
//! the durable cache locations and the patches-checkout resolution.
//!
//! Everything here reads config; the side effects belong to the commands that call it.

use crate::fsutil::{absolutize, normalize};
use boot2deb_core::model::{Overrides, ResolvedBuild};
use boot2deb_core::series::Scope;
use boot2deb_core::{resolve_device, resolve_recipe, BuildPoint, ConfigRoot, RangeMatch};
use boot2deb_engine::event::Step;
use boot2deb_engine::rootfs;
use boot2deb_engine::{image, patchfetch, pins, EngineError, EventSink};
use std::path::{Path, PathBuf};

/// Every lookup here fails with a message for the operator, not a type the caller
/// matches on — a missing fragment or an unvendored keyring is a config mistake to
/// print, not a variant to branch on. Shadows the prelude `Result`; [`resolve`], which
/// returns a [`ConfigError`](boot2deb_core::ConfigError) callers do inspect, spells out
/// `std::result::Result`.
type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Structural check that the primary `--root` points at a boot2deb config tree —
/// `base.toml` plus a `devices/` directory — run before any command dispatches
/// against it. Only the primary root is checked: overlays are partial by design
/// (a single retuned layer file is a valid overlay). The message shows the
/// absolutized path, since the offending value is usually the implicit default
/// `.` and "`.` not found" names nothing.
pub(crate) fn ensure_config_root(root: &ConfigRoot) -> Result<()> {
    let primary = root.path();
    if primary.join("base.toml").is_file() && primary.join("devices").is_dir() {
        return Ok(());
    }
    Err(format!(
        "{} does not look like a boot2deb config root (no base.toml + devices/) — \
         run from the boot2deb repo root or pass --root <dir>",
        absolutize(primary.to_path_buf()).display()
    )
    .into())
}

/// Fold a positional recipe argument and any `--feature` flags into one
/// [`BuildPoint`], whose [reference](BuildPoint::reference) names every path the
/// build derives.
///
/// The two spellings are equivalent — `build turing-rk1/forky --feature jellyfin` and
/// `build turing-rk1/forky+jellyfin` are the same point — so the flag form is sugar
/// over the reference, not a second mechanism. Giving *both* is rejected rather than
/// merged: the two lists would have to be concatenated in some order, and feature
/// order changes the build, so there is no answer that is obviously what was meant.
pub(crate) fn build_point(reference: &str, features: Vec<String>) -> Result<BuildPoint> {
    if features.is_empty() {
        return Ok(BuildPoint::parse(reference)?);
    }
    let point = BuildPoint::parse(reference)?;
    if point.is_variant() {
        return Err(format!(
            "'{reference}' already selects features ({}), so --feature has nothing \
             unambiguous to add — put the whole selection in one place, either in the \
             reference or in --feature flags",
            point.features().join(", ")
        )
        .into());
    }
    Ok(BuildPoint::new(point.recipe(), features)?)
}

/// Resolve `target` as a recipe if one exists or it is a nested `<device>/<leaf>`
/// reference, else as a device.
pub(crate) fn resolve(
    root: &ConfigRoot,
    target: &str,
    overrides: Overrides,
) -> std::result::Result<ResolvedBuild, boot2deb_core::ConfigError> {
    let is_recipe = root.list_recipes()?.iter().any(|n| n == target);
    // A `/` is unambiguously a recipe reference — devices are flat — so route a
    // slashed target to recipe resolution even when it names no recipe: a "recipe not
    // found" error reads better than device validation rejecting the separator.
    if is_recipe || target.contains('/') {
        // A bare name that is both a recipe and a device resolves as the recipe;
        // surface the ambiguity rather than silently preferring one. A slashed target
        // can never be a device, so this only fires for a bare recipe.
        if is_recipe && root.list("devices")?.iter().any(|n| n == target) {
            eprintln!("note: '{target}' is both a recipe and a device — resolving as the recipe");
        }
        resolve_recipe(root, target, &overrides)
    } else {
        resolve_device(root, target, &overrides)
    }
}

/// The composed series whose declared envelope for `scope` does not admit
/// `version`, as `(series name, declared range)` in compose order. Empty when every
/// series claims the version — the ordinary case.
///
/// This is the **cheap half** of the patch question, and the half that can be asked
/// before committing to anything: it reads the series manifests and compares version
/// ranges, needing no source tree and running no `git am`. Whether the patches
/// actually apply is `verify-patches`, which needs a checkout and is therefore
/// the expensive half.
///
/// `scope` selects the axis: [`Scope::Kernel`] asks `applies_to_kernel` about a
/// kernel tag, [`Scope::Uboot`] asks `applies_to_uboot` about a u-boot tag. The two
/// axes move independently, so each is asked about its own version.
///
/// Release-strict, matching the build gate rather than the candidate path: a series'
/// envelope is a claim about released versions, so a pinned prerelease is reported
/// here exactly as the build would refuse it.
pub(crate) fn series_outside_envelope(
    patches_root: &Path,
    series: &[String],
    scope: Scope,
    version: &str,
) -> Result<Vec<(String, String)>> {
    let mut outside = Vec::new();
    for name in series {
        let loaded = boot2deb_core::load_series(patches_root, name)?;
        if !loaded.applies_to_scope(name, scope, version, RangeMatch::Release)? {
            outside.push((
                name.clone(),
                loaded.envelope(scope).unwrap_or("*").to_string(),
            ));
        }
    }
    Ok(outside)
}

/// Validate the resolved build's cheap, local config invariants: the whole
/// image geometry (offset ordering, alignment, GPT/rootfs fit — via the engine),
/// that every referenced kernel `config_fragments` file and `device_dts` source
/// exists under the config path, and that every declared apt source's signing
/// keyring is vendored.
///
/// Run by `resolve` (the documented first coherence gate), `update` (so a malformed
/// axis fails before the lock is committed), and `build` (so it fails before any
/// stage compiles) — a bad `rootfs_offset`, a typo'd fragment name, or a missing
/// keyring surfaces at resolution rather than deep in the build, the same
/// fail-early discipline as the device/kernel/suite checks.
pub(crate) fn preflight_config(root: &ConfigRoot, build: &ResolvedBuild) -> Result<()> {
    image::validate_geometry(build)?;
    // Resolve each fragment purely to assert it exists; the paths are re-resolved where
    // the kernel stage actually consumes them.
    fragment_paths(root, build)?;
    // Likewise the board's device-tree sources: a missing `.dts` must fail here, not
    // after the kernel has cloned and patched.
    device_dts_paths(root, build)?;
    // Resolve each keyring purely to assert it exists; the rootfs stage re-resolves
    // the paths it verifies each feature repository against.
    apt_source_keyrings(
        root,
        build
            .image
            .as_ref()
            .map(|i| i.apt_sources.as_slice())
            .unwrap_or(&[]),
    )?;
    Ok(())
}

/// The userspace trees a build actually compiles: every tree the SoC declares, less the
/// [`optional`](boot2deb_core::model::UserspaceTree::optional) ones this run did not name
/// with `--userspace`.
///
/// The narrowing is here rather than in resolution because it is a *build-time* choice:
/// the lock pins every tree the part has, and a run decides which of the optional ones
/// to spend the minutes on. Every consumer of the set — the userspace stage's layer, the
/// ffmpeg key, `why-rebuild`'s prediction, `shell`'s root — has to agree on it, which is
/// why they all call this rather than filtering their own way.
///
/// An `--userspace` naming a tree the SoC does not declare is an error rather than a
/// silent no-op: it is the same mistake `--kmod-src` on an undeclared module is, and
/// left unreported it would compile nothing and say nothing.
pub(crate) fn enabled_userspace(
    trees: &[boot2deb_core::model::UserspaceTree],
    asked: &[String],
) -> Result<Vec<boot2deb_core::model::UserspaceTree>> {
    if let Some(name) = asked.iter().find(|n| !trees.iter().any(|t| &&t.name == n)) {
        let declared: Vec<&str> = trees
            .iter()
            .filter(|t| t.optional)
            .map(|t| t.name.as_str())
            .collect();
        return Err(format!(
            "--userspace names '{name}', which this SoC does not declare. Optional trees: {}",
            if declared.is_empty() {
                "none".to_string()
            } else {
                declared.join(", ")
            }
        )
        .into());
    }
    Ok(trees
        .iter()
        .filter(|t| !t.optional || asked.iter().any(|n| n == &t.name))
        .cloned()
        .collect())
}

/// Resolve a build's kernel fragment names to `fragments/<name>.config` paths
/// along the config search path, erroring if any is missing. An overlay may
/// ship the fragments for a device/kernel it adds; the highest-precedence copy
/// wins.
pub(crate) fn fragment_paths(root: &ConfigRoot, build: &ResolvedBuild) -> Result<Vec<PathBuf>> {
    // A distro kernel merges no fragments — Debian owns its config — so it resolves
    // to an empty list rather than an error.
    let Some(kernel) = build.image.as_ref().and_then(|i| i.kernel.compiled()) else {
        return Ok(Vec::new());
    };
    let mut paths = Vec::new();
    for name in &kernel.config_fragments {
        let rel = format!("fragments/{name}.config");
        let path = root
            .find_asset(&rel)
            .ok_or_else(|| format!("fragment not found: {rel} (searched the config path)"))?;
        paths.push(path);
    }
    Ok(paths)
}

/// Resolve a build's `device_dts` entries to files along the config search path,
/// erroring if any is missing. The entries are already validated at resolution
/// to be contained, relative `.dts`/`.dtsi` paths; an overlay commonly ships them for
/// the device it adds, and the highest-precedence copy wins as for any other asset.
pub(crate) fn device_dts_paths(root: &ConfigRoot, build: &ResolvedBuild) -> Result<Vec<PathBuf>> {
    let mut paths = Vec::new();
    for rel in &build.device_dts {
        let path = root.find_asset(rel).ok_or_else(|| {
            format!("device_dts source not found: {rel} (searched the config path)")
        })?;
        paths.push(path);
    }
    Ok(paths)
}

/// Resolve each kmod's boot2deb-side `local_patches` to files along the config search
/// path, keyed by kmod name, in declared apply order. Each entry is a bare filename
/// (guaranteed by resolution) taken from `kmods/<name>/patches/`, so it lives beside the
/// driver that needs it and an overlay shipping the same path replaces it — the
/// highest-precedence copy wins, as for any other asset. A kmod with no `local_patches`
/// yields an empty vec — the compat-shim step is skipped for it. The engine's
/// [`build_kmods`](boot2deb_engine::build::kmod::build_kmods) consumes this as its
/// `local_patches` input.
pub(crate) fn kmod_local_patches(
    root: &ConfigRoot,
    build: &ResolvedBuild,
) -> Result<Vec<(String, Vec<PathBuf>)>> {
    let kmods = build
        .image
        .as_ref()
        .map(|i| i.device_kmods.as_slice())
        .unwrap_or(&[]);
    let mut out = Vec::with_capacity(kmods.len());
    for kmod in kmods {
        let mut paths = Vec::with_capacity(kmod.local_patches.len());
        for file in &kmod.local_patches {
            let rel = format!("kmods/{}/patches/{file}", kmod.name);
            let path = root.find_asset(&rel).ok_or_else(|| {
                format!(
                    "kmod '{}' local_patch not found: {rel} (searched the config path)",
                    kmod.name
                )
            })?;
            // Absolute: the kmod stage applies these with `git -C <driver_tree> apply`,
            // whose cwd is the driver tree, so a config-root-relative path would resolve
            // against the wrong directory.
            paths.push(absolutize(path));
        }
        out.push((kmod.name.clone(), paths));
    }
    Ok(out)
}

/// The directory vendored apt keyrings live in, relative to a config root. Named
/// once so the boundary [`apt_source_keyring`] enforces, the one its failure message
/// describes, and the one it points an operator at cannot drift apart.
const KEYRING_DIR: &str = "blobs/keyrings";

/// Resolve one apt source's `signed_by` to the vendored keyring it names, or `None`
/// if no root along the config search path ships it.
///
/// The value is a bare file name by construction — resolution rejects a separator or
/// dot segment with [`ConfigError::AptSourceBadField`](boot2deb_core::ConfigError) —
/// so the *string* cannot aim this out of the keyring directory. What is enforced
/// here is the symlink half: the resolved file is canonicalized and must still lie
/// inside some root's `blobs/keyrings/`, so a link planted in the tree cannot make an
/// arbitrary host file the trust anchor a third-party repo is verified against. A
/// path that escapes is an error, never a silent fall-through to the host.
///
/// Containment is checked against every root's keyring directory, not only the
/// primary's, because an overlay may legitimately vendor a keyring for a repo it
/// adds — unlike the Debian archive keyring, which
/// [`find_trust_anchor`](ConfigRoot::find_trust_anchor) pins to the shipped root.
pub(crate) fn apt_source_keyring(root: &ConfigRoot, signed_by: &str) -> Result<Option<PathBuf>> {
    let rel = format!("{KEYRING_DIR}/{signed_by}");
    let Some(path) = root.find_asset(&rel) else {
        return Ok(None);
    };
    let canonical = path
        .canonicalize()
        .map_err(|source| format!("{}: {source}", path.display()))?;
    let contained = root.search_paths().iter().any(|base| {
        base.join(KEYRING_DIR)
            .canonicalize()
            .map(|dir| canonical.starts_with(dir))
            .unwrap_or(false)
    });
    if !contained {
        return Err(format!(
            "keyring '{signed_by}' resolves to {}, outside {KEYRING_DIR}/ — a trust \
             anchor must be a file vendored in the config tree, not a link out of it",
            canonical.display()
        )
        .into());
    }
    Ok(Some(path))
}

/// Resolve each declared apt source's signing keyring to a vendored host path,
/// erroring on the first source whose keyring is missing, escapes the keyring
/// directory ([`apt_source_keyring`]), or does not match its fingerprint manifest:
/// the repo is verified during the rootfs solve, not trusted blindly, so its key is a
/// build-host prerequisite like the Debian archive keyring — and the key itself is
/// held to the fingerprints a human vetted, so a swapped blob cannot quietly become a
/// trust anchor. Called from [`preflight_config`] as the early gate and from the
/// rootfs stage for the paths it actually mounts.
pub(crate) fn apt_source_keyrings<'a>(
    root: &ConfigRoot,
    sources: &'a [boot2deb_core::model::AptSource],
) -> Result<Vec<rootfs::AptRepo<'a>>> {
    let mut repos = Vec::with_capacity(sources.len());
    for source in sources {
        let keyring = apt_source_keyring(root, &source.signed_by)?.ok_or_else(|| {
            format!(
                "apt source '{}' requires signing keyring '{}', but it is not vendored \
                 — add it under {KEYRING_DIR}/ (see {KEYRING_DIR}/README.md)",
                source.name, source.signed_by
            )
        })?;
        boot2deb_engine::keyring::verify(&keyring)?;
        repos.push(rootfs::AptRepo { source, keyring });
    }
    Ok(repos)
}

/// Overlay directories for a build's rootfs, in merge order:
/// base → soc → boot-method → device lineage → each feature. Each logical layer is
/// expanded along the config search path (shipped copy first, then any overlay's
/// copy of the same tree), so an overlay's overlay-tree stacks right after — and
/// thus wins over — the shipped one, matching the layer merge semantics. Absent
/// dirs contribute nothing.
///
/// The device contributes one tree per entry in its
/// [`device_lineage`](ResolvedBuild::device_lineage), base-most first, so a variant
/// board inherits the runtime config of what it extends and overrides any file of it
/// — the same relationship its TOML keys have. A device that extends nothing
/// contributes exactly one tree.
///
/// The two hardware layers may also carry an [`OVERLAY_NONFREE`] tree, which stacks
/// directly after their own and is skipped entirely on a
/// [`libre`](boot2deb_core::ResolvedImage::libre) build.
///
/// `stage` selects *when* the tree is laid into the rootfs, which is a different
/// question from what is in it (see [`OverlayStage`]).
pub(crate) fn overlay_dirs(
    root: &ConfigRoot,
    b: &ResolvedBuild,
    stage: OverlayStage,
) -> Vec<PathBuf> {
    let dir = stage.dir_name();
    // A hardware layer contributes its own tree, then — unless this build is libre —
    // its vendored-blob tree, so a board's own file still wins over what it extends.
    let libre = b.image.as_ref().is_some_and(|i| i.libre);
    let hardware = |prefix: String| {
        let mut trees = vec![format!("{prefix}/{dir}")];
        if !libre && stage == OverlayStage::Customize {
            trees.push(format!("{prefix}/{OVERLAY_NONFREE}"));
        }
        trees
    };
    let mut rels = vec![format!("base/{dir}")];
    rels.extend(hardware(format!("socs/{}", b.soc.as_str())));
    rels.push(format!("boot-methods/{}/{dir}", b.boot_method.as_str()));
    for device in &b.device_lineage {
        rels.extend(hardware(format!("devices/{device}")));
    }
    for feature in b.image.iter().flat_map(|i| &i.features) {
        rels.push(format!("features/{feature}/{dir}"));
    }
    rels.iter()
        .flat_map(|rel| root.find_asset_all(rel))
        .collect()
}

/// The tree a SoC or device layer vendors **nonfree firmware** in: files Debian does
/// not package, laid into the rootfs exactly like that layer's `overlay/` and left out
/// of a [`libre`](boot2deb_core::ResolvedImage::libre) image, whose kernel could not
/// load them.
///
/// It is a tree of its own rather than a subtraction from `overlay/` because the
/// blobs are then visible as blobs — one directory to audit, and a `libre` build that
/// skips it cannot miss one by spelling a path wrong.
///
/// Only the *customize* stage has one. Firmware is read by a driver binding real
/// hardware, long after the initramfs has handed off; nothing in this repo needs a
/// blob before the first package is unpacked, and a second gated tree that no layer
/// fills would be a mechanism with no user.
pub(crate) const OVERLAY_NONFREE: &str = "overlay-nonfree";

/// When a layer's overlay tree is laid into the rootfs.
///
/// Most config belongs *after* the packages, where it wins over whatever they
/// shipped — that is [`Customize`](OverlayStage::Customize), the `overlay/` tree, and
/// it is where nearly everything goes.
///
/// [`PreInstall`](OverlayStage::PreInstall) — the `overlay-pre/` tree — exists for the
/// config a package's own maintainer scripts have to *see while they run*. Three
/// things need it, and one of them is a safety property:
///
///  - `depthcharge-tools` registers a kernel hook that re-signs and re-flashes a
///    ChromeOS kernel partition. Installed with no config present, it runs at its
///    defaults and looks for that partition on **the build host's** disks. Its config
///    must exist, saying `enable-system-hooks = False`, before the package does.
///  - The initramfs settings (`MODULES=list`) must precede the kernel package, or the
///    first initramfs is built at `MODULES=most` — three times the size budget the
///    signed payload has — and then thrown away and rebuilt.
///  - Jellyfin's `/etc/jellyfin/encoding.xml` has to end up owned by the service
///    user, which rewrites it on every start. An overlay file lands root-owned and
///    that uid is allocated at install time, so the tree cannot pre-assign it —
///    but `jellyfin-server.postinst` chowns the whole directory while it is still
///    root's, and a file laid in beforehand is inside that sweep.
///
/// All three are cases where "config wins over the package" is not enough, because
/// the package *acted* before the config arrived.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OverlayStage {
    /// `overlay-pre/` — laid in before any package is installed.
    PreInstall,
    /// `overlay/` — laid in after every package, so it wins over package files.
    Customize,
}

impl OverlayStage {
    /// The layer subdirectory this stage reads.
    fn dir_name(self) -> &'static str {
        match self {
            OverlayStage::PreInstall => "overlay-pre",
            OverlayStage::Customize => "overlay",
        }
    }
}

/// The config root's durable cache tree (`<root>/cache`), parent of every store
/// below. Root-scoped and shared across recipes, so it is untouched by a `clean` of
/// one recipe's work dir; `clean --all-caches` names this directory.
///
/// Always absolute: a store path is handed to sandboxes that chdir elsewhere, and is
/// printed in `clean`'s removal report.
pub(crate) fn cache_dir(root: &ConfigRoot) -> PathBuf {
    absolutize(root.path().join("cache"))
}

/// The durable Tier-2 artifact store (`<root>/cache/artifacts`): completed node
/// outputs keyed by node signature, written and read by `build` and consulted by
/// `why-rebuild`. Shared across recipes — it is what makes a revalidation build cheap.
pub(crate) fn artifact_cache(root: &ConfigRoot) -> PathBuf {
    cache_dir(root).join("artifacts")
}

/// The durable, shared cache of auto-fetched verify checkouts (`<root>/cache/verify-trees`),
/// commit-addressed by [`boot2deb_engine::srcfetch::ensure_tree`]. Sibling to the
/// patches and artifact caches; it outlives every recipe work dir and is reused across
/// recipes and verify runs. `clean --verify-trees` prunes it to the checkouts the locks
/// still pin, and `clean --all-caches` takes it entire.
pub(crate) fn verify_trees_cache(root: &ConfigRoot) -> PathBuf {
    cache_dir(root).join("verify-trees")
}

/// The commit-addressed cache of auto-fetched `patches` checkouts
/// (`<root>/cache/patches`), filled by [`resolve_patches_source`] when no local
/// checkout is co-located. Keyed on the lock's `[patches] commit`, so it is swept by
/// the same liveness rule as [`verify_trees_cache`].
pub(crate) fn patches_cache(root: &ConfigRoot) -> PathBuf {
    cache_dir(root).join("patches")
}

/// `verify-config`'s scratch tree (`<root>/cache/kconfig`), one work dir per recipe
/// slug. Pure scratch — each holds a provisioned cross root and an out-of-tree kbuild
/// output dir, both re-created on the next run — which is why `clean --kconfig`
/// removes the whole tree rather than pruning within it.
///
/// Under the config root's cache and deliberately **not** under `TMPDIR`: the config
/// builds run inside a cage that mounts its own `/tmp`, so a scratch dir in the host's
/// temp dir is shadowed by that tmpfs and everything kbuild writes there is discarded.
pub(crate) fn kconfig_cache(root: &ConfigRoot) -> PathBuf {
    cache_dir(root).join("kconfig")
}

/// Auto-fetch a pinned source tree for verification, wrapping
/// [`boot2deb_engine::srcfetch::ensure_tree`] in a build step so the fetch streams.
/// Shared by the two gates that materialize a tree they were not handed:
/// `verify-patches` and `verify-config`.
pub(crate) fn fetch_verify_tree(
    source: &str,
    reference: &str,
    commit: &str,
    what: &str,
    cache_root: &Path,
    sink: &dyn EventSink,
) -> Result<PathBuf> {
    let step = Step::start(sink, "fetch-source");
    let tree =
        boot2deb_engine::srcfetch::ensure_tree(source, reference, commit, what, cache_root, &step)?;
    step.finish();
    Ok(tree)
}

/// The content-addressed store for pre-built `extra_debs`: a durable
/// build-host cache under the config root, shared by `update` (which fills it) and
/// `build` (which reads it). It sits outside any recipe work dir, so cleaning one leaves
/// it intact — the build no longer depends on the source staying put. `clean
/// --all-caches` is the only selector that reaches it.
pub(crate) fn extra_debs_store(root: &ConfigRoot) -> PathBuf {
    cache_dir(root).join("extra-debs")
}

/// The default `patches` repo checkout: the config root's sibling `patches/`
/// directory, the side-by-side layout the two repos are developed in.
///
/// Anchored to `--root` rather than the process CWD, because which checkout is
/// "co-located" is a property of where the config tree lives. The divergence this
/// prevents is silent in the case where silence matters most: when the default misses,
/// `build` does not fail — it takes a *different* series source (auto-fetch at the
/// pinned commit), so a run with `--root` from elsewhere would read patches the
/// operator did not stage.
///
/// Normalized, since the value is shown in `update`'s and `patch import`'s
/// missing-checkout errors and in the latter's next-step hints.
pub(crate) fn default_patches_checkout(root: &ConfigRoot) -> PathBuf {
    normalize(absolutize(root.path().to_path_buf()).join("../patches"))
}

/// Resolve the patches source for a build, returning the checkout path and
/// whether it is a co-development checkout (a pin mismatch is downgraded to a
/// warning rather than a hard error). Precedence:
///
/// 1. An explicit `--patches-path <dir>` — co-development from a working checkout.
/// 2. The [default sibling checkout](default_patches_checkout) if it is a git
///    checkout — the pin is enforced.
/// 3. Auto-fetch the series at the lock's `patches.commit` from `--patches-url`,
///    else from the pin's own [`source`], into a durable commit-addressed cache
///    (`<root>/cache/patches/<commit>`), so a build with no local checkout resolves
///    automatically (the North-Star "selecting a device auto-fetches the right
///    patches"). With no URL available this is a hard [`EngineError::PatchesNoSource`]
///    naming the pinned commit — patches are never silently skipped.
///
/// The URL comes from the *pin*, not from the current config, for two reasons.
/// The commit is the lock's, so the repo must be too: re-pointing a kernel's
/// `patches_url` after a lock was written would otherwise fetch the pinned commit
/// from a different repo. And the pin is the only source both axes carry — a
/// `deliverable = "uboot"` recipe resolves no kernel at all
/// ([`resolve_device`]), so a config-derived kernel URL leaves it with nothing to
/// fetch from.
///
/// [`source`]: boot2deb_core::lock::PatchesPin::source
pub(crate) fn resolve_patches_source(
    patches_path: Option<&Path>,
    patches_url: Option<&str>,
    pin: &boot2deb_core::lock::PatchesPin,
    root: &ConfigRoot,
    sink: &dyn EventSink,
) -> Result<(PathBuf, bool)> {
    if let Some(path) = patches_path {
        return Ok((path.to_path_buf(), true));
    }
    let default_local = default_patches_checkout(root);
    if default_local.join(".git").exists() {
        return Ok((default_local, false));
    }
    let url = patches_url
        .map(str::trim)
        .filter(|u| !u.is_empty())
        .or_else(|| Some(pin.source.trim()).filter(|s| !s.is_empty()))
        .ok_or_else(|| EngineError::PatchesNoSource {
            commit: pin.commit.clone(),
        })?;
    let cache_root = patches_cache(root);
    let step = Step::start(sink, "patches");
    let dir = patchfetch::fetch_series(url, &pin.commit, &cache_root, &step)?;
    step.finish();
    Ok((dir, false))
}

/// The fetched source axes as `(name, configured upstream URL, locked ref,
/// locked commit)` — the set `verify-sources` probes and `update` warns on, always
/// against the *configured* URL (never a `--<pkg>-src` override). The ffmpeg
/// `rockchip` pin is provenance-only (never fetched at build), so it is omitted.
pub(crate) struct SourceAxis<'a> {
    /// Human name for the report (`kernel`, `u-boot`, `mpp`, …). Owned so a per-name
    /// axis like a kmod (`kmod:aic8800`) can carry a formatted label; the fixed axes
    /// are cheap borrowed literals.
    pub(crate) name: std::borrow::Cow<'static, str>,
    /// The configured upstream clone URL.
    pub(crate) url: String,
    /// The pinned ref (tag/branch name, or the bare commit).
    pub(crate) reference: &'a str,
    /// The exact pinned commit.
    pub(crate) commit: &'a str,
}

/// Build the [`SourceAxis`] list from a resolved build (for the configured URLs) and
/// its lock (for the pins). The kernel URL resolution is the only fallible step.
pub(crate) fn source_axes<'a>(
    build: &ResolvedBuild,
    lock: &'a boot2deb_core::lock::Lock,
) -> Result<Vec<SourceAxis<'a>>> {
    // Only sources the build actually fetches from git have a re-fetch durability to
    // report. A distro-package kernel is installed from the mirror and a depthcharge
    // board builds no bootloader, so neither contributes an axis.
    let mut axes = Vec::new();
    if let (Some(kernel), Some(pin)) = (
        build.image.as_ref().and_then(|i| i.kernel.compiled()),
        &lock.kernel,
    ) {
        axes.push(SourceAxis {
            name: "kernel".into(),
            url: pins::kernel_source_url(&kernel.source)?,
            reference: &pin.reference,
            commit: &pin.commit,
        });
    }
    if let (Some(boot), Some(pin)) = (build.rkbin_boot(), &lock.uboot) {
        axes.push(SourceAxis {
            name: "u-boot".into(),
            url: boot.uboot_source.clone(),
            reference: &pin.reference,
            commit: &pin.commit,
        });
    }
    // The fetched media-accel trees, present only for a build that compiles the
    // transcode stack. URLs come from the resolved build, pins from the lock — both
    // `Some` together.
    // One axis per tree the SoC declares; a tree it does not have is fetched by
    // nobody, so listing it would name a source this build never reads.
    for tree in build.image.iter().flat_map(|i| &i.userspace) {
        if let Some(p) = lock.userspace.iter().find(|p| p.name == tree.name) {
            axes.push(SourceAxis {
                name: tree.name.clone().into(),
                url: tree.git.clone(),
                reference: &p.reference,
                commit: &p.commit,
            });
        }
    }
    if let (Some(ff), Some(ff_pins)) = (
        build.image.as_ref().and_then(|i| i.ffmpeg.as_ref()),
        &lock.ffmpeg,
    ) {
        axes.push(SourceAxis {
            name: "ffmpeg-base".into(),
            url: ff.base.git.clone(),
            reference: &ff_pins.base.reference,
            commit: &ff_pins.base.commit,
        });
    }
    // The patches axis, which needs this check more than any other: `update` takes
    // its commit from a local checkout's HEAD rather than resolving a remote ref, so
    // it is the pin most likely to name something that exists nowhere else — a series
    // committed locally and not yet pushed pins fine and fails for everyone else.
    // Every other axis was already graded; this one could not be until the pin
    // carried a source.
    if let Some(pin) = &lock.patches {
        axes.push(SourceAxis {
            name: "patches".into(),
            url: pin.source.clone(),
            reference: &pin.reference,
            commit: &pin.commit,
        });
    }
    // The u-boot patch series shares the kernel patches' durability risk — its commit is
    // also taken from a local HEAD — so it joins the check the same way.
    if let Some(pin) = &lock.uboot_patches {
        axes.push(SourceAxis {
            name: "u-boot patches".into(),
            url: pin.source.clone(),
            reference: &pin.reference,
            commit: &pin.commit,
        });
    }
    // Each out-of-tree kernel module is fetched from its own pinned repo — a source pin
    // with the same durability question, probed under a `kmod:<name>` label.
    for pin in &lock.kmods {
        axes.push(SourceAxis {
            name: format!("kmod:{}", pin.name).into(),
            url: pin.source.clone(),
            reference: &pin.reference,
            commit: &pin.commit,
        });
    }
    Ok(axes)
}

#[cfg(test)]
mod tests {

    /// The image half of a fixture build. Every fixture here resolves a shipped image
    /// recipe, so the axis is there; the unwrap states that rather than threading an
    /// `Option` through every assertion.
    fn image_of(build: &boot2deb_core::ResolvedBuild) -> &boot2deb_core::ResolvedImage {
        pair_of(build).image
    }

    /// The same fixture build as an [`ImageBuild`] pair, for the stages that take one.
    fn pair_of(build: &boot2deb_core::ResolvedBuild) -> boot2deb_core::ImageBuild<'_> {
        build.as_image().expect("the fixture recipes build images")
    }
    use super::*;
    use crate::testsupport::{repo_root, repo_root_path};

    #[test]
    fn preflight_accepts_shipped_config_and_rejects_bad_geometry_or_fragment() {
        // Geometry + fragment existence are validated up front (by both update
        // and build), so a bad axis fails at resolution, not deep in the build.
        let root = repo_root();
        let resolved = resolve_recipe(&root, "turing-rk1/forky", &Overrides::default()).unwrap();
        // The shipped RK1 config passes.
        preflight_config(&root, &resolved).unwrap();

        // A nonsensical rootfs offset (the review's own probe value) is rejected.
        let mut bad_geom = resolved.clone();
        if let boot2deb_core::model::ResolvedBoot::RockchipRkbin(boot) = &mut bad_geom.boot {
            boot.offsets.rootfs = "1".to_string();
        }
        assert!(
            preflight_config(&root, &bad_geom).is_err(),
            "bad geometry must fail preflight"
        );

        // A referenced-but-missing kernel fragment is rejected.
        let mut bad_frag = resolved.clone();
        if let boot2deb_core::model::ResolvedKernel::Compiled(k) =
            &mut bad_frag.image.as_mut().unwrap().kernel
        {
            k.config_fragments
                .push("definitely-no-such-fragment".to_string());
        }
        let err = preflight_config(&root, &bad_frag).unwrap_err().to_string();
        assert!(
            err.contains("fragment not found"),
            "expected a fragment error, got: {err}"
        );

        // A declared apt source whose signing keyring is not vendored is rejected at
        // preflight, not after the compile stages.
        let mut bad_key = resolved.clone();
        bad_key
            .image
            .as_mut()
            .unwrap()
            .apt_sources
            .push(boot2deb_core::model::AptSource {
                name: "third-party".into(),
                uri: "https://example.invalid/debian".into(),
                suite: "trixie".into(),
                components: vec!["main".into()],
                signed_by: "no-such-keyring.gpg".into(),
            });
        let err = preflight_config(&root, &bad_key).unwrap_err().to_string();
        assert!(
            err.contains("no-such-keyring.gpg") && err.contains("not vendored"),
            "expected a keyring error naming the file, got: {err}"
        );
    }

    /// Resolution holds `signed_by` to a bare file name, so the string cannot aim the
    /// lookup out of `blobs/keyrings/`. A symlink can, and that is the half checked
    /// here: a link inside the keyring directory pointing at a host file is refused
    /// rather than silently becoming the trust anchor a third-party repo is verified
    /// against.
    #[test]
    fn a_keyring_symlinked_out_of_the_keyring_directory_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let root_dir = tmp.path().join("config");
        std::fs::create_dir_all(root_dir.join(KEYRING_DIR)).unwrap();
        std::fs::create_dir_all(root_dir.join("devices")).unwrap();
        std::fs::write(root_dir.join("base.toml"), "").unwrap();

        // A key elsewhere on the host, and a link to it planted in the keyring dir.
        let outside = tmp.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("attacker.gpg"), b"KEY").unwrap();
        std::os::unix::fs::symlink(
            outside.join("attacker.gpg"),
            root_dir.join(KEYRING_DIR).join("vendor.gpg"),
        )
        .unwrap();

        let root = ConfigRoot::new(&root_dir);
        let err = apt_source_keyring(&root, "vendor.gpg")
            .expect_err("a keyring resolving outside the keyring directory must fail")
            .to_string();
        assert!(
            err.contains("outside") && err.contains("vendor.gpg"),
            "the failure names the escape and the keyring, got: {err}"
        );

        // A real file in the same place resolves — containment refuses the link, not
        // the directory.
        std::fs::remove_file(root_dir.join(KEYRING_DIR).join("vendor.gpg")).unwrap();
        std::fs::write(root_dir.join(KEYRING_DIR).join("vendor.gpg"), b"KEY").unwrap();
        assert_eq!(
            apt_source_keyring(&root, "vendor.gpg").unwrap(),
            Some(root_dir.join(KEYRING_DIR).join("vendor.gpg"))
        );

        // A keyring no root ships is absent, not an escape: the caller that needs it
        // says so, naming the source that asked.
        assert_eq!(apt_source_keyring(&root, "absent.gpg").unwrap(), None);
    }

    #[test]
    fn preflight_accepts_the_shipped_jellyfin_composition() {
        // The jellyfin recipes declare a third-party apt source; its signing
        // keyring is vendored, so the shipped composition passes the same gate
        // that rejects a missing one — `resolve turing-rk1/jellyfin-forky` stays green.
        let root = repo_root();
        let resolved =
            resolve_recipe(&root, "turing-rk1/jellyfin-forky", &Overrides::default()).unwrap();
        assert!(
            image_of(&resolved)
                .apt_sources
                .iter()
                .any(|s| s.name == "jellyfin"),
            "the jellyfin feature declares its apt source"
        );
        preflight_config(&root, &resolved).unwrap();
    }

    #[test]
    fn the_default_patches_checkout_is_the_config_roots_sibling() {
        // Anchored to --root, not the CWD: when this default misses, `build` does not
        // fail — it auto-fetches a different series — so a run with --root from
        // elsewhere would silently read patches the operator did not stage.
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("work/boot2deb");
        std::fs::create_dir_all(&nested).unwrap();
        assert_eq!(
            default_patches_checkout(&ConfigRoot::new(&nested)),
            tmp.path().join("work/patches")
        );
        // The shipped default `--root .` normalizes rather than showing `./../patches`
        // to the operator in a missing-checkout error.
        let shown = default_patches_checkout(&ConfigRoot::new("."))
            .display()
            .to_string();
        assert!(shown.ends_with("/patches"), "{shown}");
        assert!(!shown.contains("/./") && !shown.contains(".."), "{shown}");
    }

    /// A pin at `commit` naming `source`, for the auto-fetch precedence tests.
    fn patches_pin(source: &str) -> boot2deb_core::lock::PatchesPin {
        boot2deb_core::lock::PatchesPin {
            series: vec!["rk3576-loader".to_string()],
            source: source.to_string(),
            reference: "main".to_string(),
            commit: "7b3bcb9d59040f0f1a5c1f0b2e3d4c5a69788899".to_string(),
        }
    }

    /// A config root with no sibling `../patches`, so auto-fetch is the only path
    /// left. Returns the `TempDir` so the caller keeps it alive.
    fn root_without_sibling_patches() -> (tempfile::TempDir, ConfigRoot) {
        let tmp = tempfile::tempdir().unwrap();
        let nested = tmp.path().join("isolated/cfg");
        std::fs::create_dir_all(&nested).unwrap();
        let root = ConfigRoot::new(&nested);
        assert!(!default_patches_checkout(&root).join(".git").exists());
        (tmp, root)
    }

    #[test]
    fn the_patch_pin_supplies_the_fetch_url_without_any_kernel() {
        // A `deliverable = "uboot"` recipe resolves no kernel at all, so a
        // config-derived kernel URL would leave both shipped u-boot-only recipes
        // unable to fetch the series they pin. The pin carries the repo itself, and
        // that is what the fetch takes — no ResolvedBuild is consulted.
        let (_tmp, root) = root_without_sibling_patches();
        let pin = patches_pin("https://example.invalid/patches.git");
        // Pre-seed the commit-addressed cache so the fetch is a hit and the test
        // stays hermetic; reaching a hit at all is what today's kernel-only fallback
        // could not do.
        let cached = root.path().join("cache/patches").join(&pin.commit);
        std::fs::create_dir_all(&cached).unwrap();
        let sink = |_: boot2deb_engine::Event| {};
        let (dir, dev) = resolve_patches_source(None, None, &pin, &root, &sink).unwrap();
        assert_eq!(dir, cached);
        assert!(!dev, "an auto-fetched checkout is not a co-dev checkout");
    }

    #[test]
    fn the_fetch_url_comes_from_the_pin_and_an_explicit_flag_outranks_it() {
        // The commit is the lock's, so the repo must be too: re-pointing a kernel's
        // `patches_url` after a lock was written must not fetch the pinned commit
        // from somewhere else. `--patches-url` is the one deliberate override.
        let (tmp, root) = root_without_sibling_patches();
        let sink = |_: boot2deb_engine::Event| {};

        // Both URLs are local paths that do not exist, so the clone fails without
        // touching the network — and names the URL it tried.
        let pinned = tmp.path().join("pinned-repo.git");
        let pin = patches_pin(&pinned.display().to_string());
        let err = resolve_patches_source(None, None, &pin, &root, &sink)
            .unwrap_err()
            .to_string();
        assert!(err.contains(&pinned.display().to_string()), "{err}");

        let overridden = tmp.path().join("overridden-repo.git");
        let err = resolve_patches_source(
            None,
            Some(&overridden.display().to_string()),
            &pin,
            &root,
            &sink,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains(&overridden.display().to_string()), "{err}");
        assert!(!err.contains(&pinned.display().to_string()), "{err}");
    }

    #[test]
    fn a_pin_with_no_source_names_both_axes_in_its_remediation() {
        // The only way to reach this now is a lock whose pin records no repo. A
        // u-boot-only recipe has no kernel definition to set `patches_url` on, so a
        // message naming only that one is unactionable for half the recipes.
        let (_tmp, root) = root_without_sibling_patches();
        let sink = |_: boot2deb_engine::Event| {};
        let err = resolve_patches_source(None, None, &patches_pin("  "), &root, &sink)
            .unwrap_err()
            .to_string();
        assert!(err.contains("no patches source"), "{err}");
        assert!(err.contains("kernel definition"), "{err}");
        assert!(err.contains("rockchip-rkbin.toml"), "{err}");
    }

    #[test]
    fn ensure_config_root_accepts_a_config_tree_and_names_a_wrong_dir() {
        // The shipped repo root passes.
        ensure_config_root(&repo_root()).unwrap();
        // A directory that is not a config root fails, naming the path and the
        // --root remedy — the one clear error that replaces the per-command
        // "not found" cascade.
        let dir = tempfile::tempdir().unwrap();
        let err = ensure_config_root(&ConfigRoot::new(dir.path()))
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("does not look like a boot2deb config root"),
            "{err}"
        );
        assert!(err.contains("--root"), "remedy names the flag: {err}");
        // base.toml alone is not enough — devices/ must exist too.
        std::fs::write(dir.path().join("base.toml"), "packages = []\n").unwrap();
        assert!(ensure_config_root(&ConfigRoot::new(dir.path())).is_err());
        std::fs::create_dir(dir.path().join("devices")).unwrap();
        ensure_config_root(&ConfigRoot::new(dir.path())).unwrap();
    }

    #[test]
    fn source_axes_cover_every_fetched_tree_of_a_media_accel_build() {
        // The probed set is exactly what a build fetches: the two base trees, the
        // media-accel ones, and both patch series — the kernel's and u-boot's, which
        // are pinned against the `patches` repo independently. The ffmpeg `rockchip`
        // pin is provenance-only, so it is not an axis — pinning it against a URL
        // nothing clones would be a false report.
        let root = repo_root();
        let build =
            resolve_recipe(&root, "turing-rk1/media-accel-forky", &Overrides::default()).unwrap();
        let lock = root.lock("turing-rk1/media-accel-forky").unwrap();
        let axes = source_axes(&build, &lock).unwrap();
        let names: Vec<&str> = axes.iter().map(|a| a.name.as_ref()).collect();
        assert_eq!(
            names,
            [
                "kernel",
                "u-boot",
                "mpp",
                "librga",
                "libmali",
                "ffmpeg-base",
                "patches",
                "u-boot patches"
            ]
        );
        assert!(axes
            .iter()
            .all(|a| !a.url.is_empty() && !a.commit.is_empty()));
        // The patches axis is the reason the pin carries a source at all: `update`
        // takes its commit from a local HEAD, so it is the likeliest of any axis to
        // name something unreachable, and it could not be graded until now.
        let patches = axes.iter().find(|a| a.name == "patches").unwrap();
        assert!(patches.url.contains("patches"));
        assert_eq!(patches.reference, "main");
    }

    /// A config root whose overlay adds `h96-max-m9-variant`, a device that extends
    /// the shipped H96 and states nothing but its identity.
    ///
    /// The variant is synthetic rather than shipped because the thing under test is
    /// the `extends` merge itself, which must hold whether or not the tree happens to
    /// ship a variant board at the time. The `TempDir` is returned so the caller keeps
    /// the overlay alive for the length of the test.
    fn root_with_variant() -> (tempfile::TempDir, ConfigRoot) {
        let overlay = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(overlay.path().join("devices")).unwrap();
        std::fs::write(
            overlay.path().join("devices/h96-max-m9-variant.toml"),
            "extends     = \"h96-max-m9\"\n\
             description = \"H96 MAX M9 variant, for the extends contract\"\n\
             hostname    = \"h96-max-m9-variant\"\n",
        )
        .unwrap();
        let root =
            ConfigRoot::with_overlays(repo_root_path(), [overlay.path().to_path_buf()]).unwrap();
        (overlay, root)
    }

    #[test]
    fn a_variant_board_lays_in_the_overlay_of_what_it_extends() {
        // The failure this guards is silent and total: a device's overlay tree is found
        // by its *name*, so a variant that ships no tree of its own would get none at
        // all -- no driver tuning, no services, no keymaps -- and still build a
        // plausible image.
        let (_overlay, root) = root_with_variant();
        let variant = resolve_device(&root, "h96-max-m9-variant", &Overrides::default()).unwrap();
        assert_eq!(variant.device_lineage, ["h96-max-m9", "h96-max-m9-variant"]);

        let dirs = overlay_dirs(&root, &variant, OverlayStage::Customize);
        let base_tree = root
            .find_asset("devices/h96-max-m9/overlay")
            .expect("the extended board ships an overlay tree");
        assert!(
            dirs.contains(&base_tree),
            "the extended board's overlay tree is missing from {dirs:?}"
        );

        // The tree carries the files whose absence produced a verbose console and a
        // dead 5 GHz band on this board; name one so the assertion is about reaching
        // real config, not about a directory existing.
        assert!(base_tree.join("etc/modprobe.d/aic8800.conf").is_file());

        // Ordering is the whole contract: what a device extends comes *before* it, so a
        // variant can override a file rather than lose to it. The parent ships the only
        // tree here, so the check is that it precedes where the variant's would sit.
        let parent_at = dirs.iter().position(|d| d == &base_tree).unwrap();
        assert!(
            dirs[..parent_at]
                .iter()
                .all(|d| !d.ends_with("devices/h96-max-m9-variant/overlay")),
            "the variant's own tree must not precede what it extends"
        );

        // And a device that extends nothing still contributes exactly its own tree, so
        // the no-inheritance case is not a special case.
        let base = resolve_device(&root, "h96-max-m9", &Overrides::default()).unwrap();
        assert_eq!(base.device_lineage, ["h96-max-m9"]);
        let base_dirs = overlay_dirs(&root, &base, OverlayStage::Customize);
        assert!(base_dirs.contains(&base_tree));
        assert!(
            !base_dirs
                .iter()
                .any(|d| d.ends_with("devices/h96-max-m9-variant/overlay")),
            "a board must not pick up its variant's tree"
        );
    }

    #[test]
    fn jellyfin_clears_the_ffmpeg_argument_that_outranks_its_configured_encoder() {
        // Another silent-at-build, fatal-at-boot one. `jellyfin-server` puts
        // `--ffmpeg=/usr/lib/jellyfin-ffmpeg/ffmpeg` on the service command line, the
        // `jellyfin` feature declines to install the package owning that path, and
        // Jellyfin reads the command line *before* `<EncoderAppPath>` — so without this
        // tree the seeded encoding.xml is never consulted and the server throws
        // `Failed to find valid ffmpeg` during startup. The build cannot notice: the
        // overlay is found by path, and a misplaced one contributes nothing.
        let root = ConfigRoot::new(repo_root_path());
        let b = resolve_recipe(&root, "turing-rk1/jellyfin-forky", &Overrides::default()).unwrap();
        let dirs = overlay_dirs(&root, &b, OverlayStage::Customize);

        let tree = root
            .find_asset("features/jellyfin/overlay")
            .expect("the jellyfin feature ships a customize tree");
        assert!(
            dirs.contains(&tree),
            "the jellyfin feature's overlay tree is missing from {dirs:?}"
        );

        // Both halves, because either alone is inert: the drop-in only orders a file
        // that has to exist, and the file is only read because the drop-in names it.
        let dropin = tree.join("etc/systemd/system/jellyfin.service.d/no-bundled-ffmpeg.conf");
        let env = tree.join("etc/default/jellyfin-encoder");
        let dropin_text = std::fs::read_to_string(&dropin).expect("the drop-in is a real file");
        let env_text = std::fs::read_to_string(&env).expect("the env file is a real file");
        assert!(
            dropin_text.contains("EnvironmentFile=/etc/default/jellyfin-encoder"),
            "the drop-in must point at {env:?}"
        );
        assert!(
            env_text.contains(r#"JELLYFIN_FFMPEG_OPT="""#),
            "the argument must be cleared, not repointed -- a path here would outrank \
             the dashboard's own FFmpeg path field"
        );

        // It has to be an `EnvironmentFile=`. systemd applies environment files after
        // `Environment=` whatever order they appear in, so an `Environment=` line would
        // lose to /etc/default/jellyfin and the drop-in would do nothing at all.
        assert!(
            !dropin_text.contains("\nEnvironment="),
            "an Environment= line loses to /etc/default/jellyfin"
        );

        // The glue feature supplies the value the cleared argument falls through to, so
        // the two have to agree on the encoder path or the server still dies.
        let encoding = root
            .find_asset("features/jellyfin-rockchip/overlay-pre")
            .expect("the glue feature ships a pre-install tree")
            .join("etc/jellyfin/encoding.xml");
        let encoding_text = std::fs::read_to_string(&encoding).expect("the seed is a real file");
        assert!(
            encoding_text.contains("<EncoderAppPath>/opt/ffmpeg-rk/bin/ffmpeg</EncoderAppPath>"),
            "the seeded encoder path is what the cleared argument falls through to"
        );
    }

    #[test]
    fn the_libreboot_c201_ships_its_display_stack_into_the_initramfs() {
        // Also a silent failure: a modules.d drop-in that is never collected costs
        // nothing at build time and simply does not exist at boot, leaving the board on
        // the firmware's blank screen exactly as if the file had never been written.
        let root = ConfigRoot::new(repo_root_path());
        let b = resolve_device(&root, "asus-c201-libreboot", &Overrides::default()).unwrap();
        let dirs = overlay_dirs(&root, &b, OverlayStage::PreInstall);

        let display = root
            .find_asset("devices/asus-c201-libreboot/overlay-pre")
            .expect("the libreboot device ships a pre-install tree");
        assert!(
            dirs.contains(&display),
            "the libreboot display stack is missing from {dirs:?}"
        );
        let list = display.join("usr/share/initramfs-tools/modules.d/veyron-display");
        let modules = std::fs::read_to_string(&list).expect("the module list is a real file");
        for module in ["rockchipdrm", "panel-simple", "pwm_bl", "pwm-rockchip"] {
            assert!(modules.contains(module), "{module} missing from {list:?}");
        }

        // It adds to the SoC family's list rather than replacing it — mkinitramfs reads
        // every file in the directory — so the drivers that reach the root device have
        // to still be coming from the layer that owns them.
        let family = root
            .find_asset("socs/rk3288/overlay-pre")
            .expect("the SoC layer ships the family's list");
        let family_at = dirs.iter().position(|d| d == &family).unwrap();
        let display_at = dirs.iter().position(|d| d == &display).unwrap();
        assert!(family_at < display_at, "the family's list comes first");
        assert!(family
            .join("usr/share/initramfs-tools/modules.d/veyron")
            .is_file());

        // And the stock board does not pick it up: its 16 MiB payload has no room for a
        // display stack, which is the whole reason the two are different devices.
        let stock = resolve_device(&root, "asus-c201", &Overrides::default()).unwrap();
        assert!(!overlay_dirs(&root, &stock, OverlayStage::PreInstall).contains(&display));
    }

    #[test]
    fn a_libre_build_lays_in_no_vendored_blob() {
        // The failure this guards is the one that matters most on this axis and is the
        // least visible: an image advertised as free that quietly carries two Broadcom
        // blobs. Nothing at build time would complain — the files copy fine and the
        // kernel simply never reads them — so the only place it can be caught is here.
        let root = ConfigRoot::new(repo_root_path());
        let blobs = root
            .find_asset("socs/rk3288/overlay-nonfree")
            .expect("the SoC layer vendors the BCM4354 firmware");
        for blob in ["BCM4354.hcd", "brcmfmac4354-sdio.txt"] {
            assert!(blobs.join("usr/lib/firmware/brcm").join(blob).is_file());
        }

        // The blobbed kernel gets them, directly after the tree they belong beside.
        let ov = |kernel: &str| Overrides {
            kernel: Some(kernel.to_string()),
            ..Default::default()
        };
        let blobbed = resolve_device(&root, "asus-c201", &ov("rk3288-mainline-7.2")).unwrap();
        let dirs = overlay_dirs(&root, &blobbed, OverlayStage::Customize);
        assert!(dirs.contains(&blobs), "blobs missing from {dirs:?}");
        let soc = root.find_asset("socs/rk3288/overlay").unwrap();
        assert_eq!(
            dirs.iter().position(|d| d == &blobs),
            dirs.iter().position(|d| d == &soc).map(|i| i + 1),
            "the nonfree tree stacks directly after its own layer's"
        );

        // The libre kernel gets neither the tree nor the package — and still gets the
        // layer's ordinary overlay, so this is a gate and not a lost SoC layer.
        let libre = resolve_device(&root, "asus-c201", &ov("rk3288-libre-7.2")).unwrap();
        assert!(image_of(&libre).libre);
        let dirs = overlay_dirs(&root, &libre, OverlayStage::Customize);
        assert!(!dirs.contains(&blobs), "a libre image carries {blobs:?}");
        assert!(dirs.contains(&soc));
        assert!(!image_of(&libre)
            .rootfs_packages
            .contains(&"firmware-brcm80211".to_string()));
        // The Free firmware is not gated with it: an AR9271 adapter is the only way
        // onto a network here, so its firmware has to be on the image.
        assert!(image_of(&libre)
            .rootfs_packages
            .contains(&"firmware-ath9k-htc".to_string()));
    }

    #[test]
    fn a_variant_board_inherits_the_kmods_of_what_it_extends() {
        // The same failure shape as the overlay tree, one layer over: a variant that
        // states only its identity has to get its Wi-Fi through `extends`. If it did
        // not, the variant would build a plausible image with no wlan0 at all and
        // nothing would say so.
        let (_overlay, root) = root_with_variant();
        let base = resolve_device(&root, "h96-max-m9", &Overrides::default()).unwrap();
        let variant = resolve_device(&root, "h96-max-m9-variant", &Overrides::default()).unwrap();
        assert_eq!(
            image_of(&base).device_kmods,
            image_of(&variant).device_kmods,
            "the variant's kmods must be exactly what it extends"
        );
        assert_eq!(
            image_of(&base)
                .device_kmods
                .iter()
                .map(|k| k.name.as_str())
                .collect::<Vec<_>>(),
            ["aic8800"]
        );
    }

    #[test]
    fn a_variant_board_inherits_the_patch_series_of_what_it_extends() {
        // `device_patch_series` is the field that carries the H96's NPU series, and a
        // variant silently losing it would build a kernel whose `rocket` driver has no
        // RK3576 support compiled in -- an image that boots fine and has no /dev/accel.
        let (_overlay, root) = root_with_variant();
        let series = |name: &str| {
            image_of(&resolve_device(&root, name, &Overrides::default()).unwrap())
                .kernel
                .patch_series()
                .to_vec()
        };
        let base = series("h96-max-m9");
        assert_eq!(base, series("h96-max-m9-variant"));
        assert!(
            base.iter().any(|p| p == "rk3576-npu"),
            "the H96 carries its NPU series on the device layer: {base:?}"
        );
    }

    #[test]
    fn kmod_local_patches_resolve_under_the_kmods_tree_in_apply_order() {
        // A local patch is a bare filename resolved against the *kmod's* directory, not
        // the device's — so a second board naming the same kmod reads the same shims
        // rather than reaching into another board's folder. The paths must come out
        // absolute: the kmod stage applies them with `git -C <driver_tree> apply`.
        let root = repo_root();
        let build = resolve_device(&root, "h96-max-m9", &Overrides::default()).unwrap();
        let resolved = kmod_local_patches(&root, &build).unwrap();
        let (name, paths) = resolved.first().expect("the H96 carries one kmod");
        assert_eq!(name, "aic8800");
        let names: Vec<&str> = paths
            .iter()
            .map(|p| p.file_name().unwrap().to_str().unwrap())
            .collect();
        assert_eq!(
            names,
            [
                "0001-sdio-linux-7.1.patch",
                "0002-quiet-log-level.patch",
                "0003-quiet-bare-printk.patch",
                "0004-suspend-quiesce-sdio.patch"
            ]
        );
        assert!(paths.iter().all(|p| p.is_absolute() && p.is_file()));
        assert!(paths
            .iter()
            .all(|p| p.parent().unwrap().ends_with("kmods/aic8800/patches")));
    }

    #[test]
    fn a_feature_flag_and_a_reference_suffix_are_the_same_point() {
        // The two spellings must agree exactly, because they name the same lock: if
        // they diverged, `update --feature X` would pin one file and
        // `build <recipe>+X` would look for another, and the second would report a
        // missing lock the first had just written.
        let flags = build_point("turing-rk1/forky", vec!["jellyfin".into()]).unwrap();
        let suffix = build_point("turing-rk1/forky+jellyfin", vec![]).unwrap();
        assert_eq!(flags, suffix);
        assert_eq!(flags.reference(), "turing-rk1/forky+jellyfin");
    }

    #[test]
    fn no_features_leaves_the_recipe_name_untouched() {
        // Every existing lock, manifest, and build directory is named for the recipe.
        // A plain build must keep resolving to exactly that name.
        let point = build_point("turing-rk1/forky", vec![]).unwrap();
        assert_eq!(point.reference(), "turing-rk1/forky");
        assert!(!point.is_variant());
    }

    #[test]
    fn giving_both_spellings_at_once_is_refused() {
        // Merging them would mean concatenating two lists in some order, and feature
        // order decides which fragment wins a kconfig conflict — so there is no
        // reading that is obviously what was meant. Refuse and say where to put it.
        let err = build_point(
            "turing-rk1/forky+jellyfin",
            vec!["media-accel-rockchip".into()],
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("already selects features"), "{err}");
        assert!(
            err.contains("jellyfin"),
            "the error names what is there: {err}"
        );
    }

    #[test]
    fn a_variant_reference_names_a_lock_beside_the_recipes_own() {
        // The variant's lock must land in the recipe's directory (so it is versioned
        // and found by every lock-reading command) without colliding with the
        // recipe's, and its work dir must be distinct or two selections would compile
        // over each other.
        let root = repo_root();
        let point = build_point("h96-max-m9/forky", vec!["media-accel-v4l2".into()]).unwrap();
        let variant = root.lock_path(&point.reference()).unwrap();
        let recipe = root.lock_path(point.recipe()).unwrap();
        assert_ne!(variant, recipe);
        assert_eq!(variant.parent(), recipe.parent());
        assert_eq!(variant.file_name().unwrap(), "forky+media-accel-v4l2.lock");
        assert_ne!(
            crate::workdir::work_dir_for(&root, &point.reference(), None),
            crate::workdir::work_dir_for(&root, point.recipe(), None)
        );
    }

    #[test]
    fn a_variant_reference_resolves_the_recipes_axes_with_its_own_features() {
        // The point of the whole mechanism: everything but the feature list comes from
        // the recipe, so a variant cannot drift from the board it names.
        let root = repo_root();
        let recipe = resolve_recipe(&root, "h96-max-m9/forky", &Overrides::default()).unwrap();
        let variant = resolve_recipe(
            &root,
            "h96-max-m9/forky+media-accel-v4l2",
            &Overrides::default(),
        )
        .unwrap();
        assert_eq!(recipe.device, variant.device);
        assert_eq!(image_of(&recipe).suite, image_of(&variant).suite);
        assert!(image_of(&recipe).features.is_empty());
        assert_eq!(
            image_of(&variant)
                .features
                .iter()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["media-accel-v4l2"]
        );
        // And the feature reaches the kernel, which is what makes a variant more than
        // a package list — the RGA series is on the variant and not on the recipe.
        fn series(b: &ResolvedBuild) -> Vec<&str> {
            image_of(b)
                .kernel
                .patch_series()
                .iter()
                .map(String::as_str)
                .collect()
        }
        assert!(!series(&recipe).contains(&"rk3576-rga"));
        assert!(series(&variant).contains(&"rk3576-rga"));
    }

    #[test]
    fn series_outside_envelope_names_only_the_series_that_do_not_claim_the_kernel() {
        // The prerequisite `update` and `build` both run before committing to a clone.
        // It has to be exact in both directions: a false positive nags on every routine
        // re-pin, and a false negative is the case this exists to stop -- a lock that
        // pins a kernel no series claims, discovered only after the tree is on disk.
        let dir = tempfile::tempdir().unwrap();
        let series = dir.path().join("series");
        std::fs::create_dir_all(&series).unwrap();
        std::fs::write(
            series.join("narrow.toml"),
            "applies_to_kernel = \">=7.0, <7.2\"\nkernel = []\nffmpeg = []\nuserspace = []\nuboot = []\n",
        )
        .unwrap();
        std::fs::write(
            series.join("wide.toml"),
            "applies_to_kernel = \">=7.0, <8.0\"\nkernel = []\nffmpeg = []\nuserspace = []\nuboot = []\n",
        )
        .unwrap();
        // No envelope at all: claims every kernel, so it is never reported.
        std::fs::write(
            series.join("unbounded.toml"),
            "kernel = []\nffmpeg = []\nuserspace = []\nuboot = []\n",
        )
        .unwrap();
        let names = [
            "narrow".to_string(),
            "wide".to_string(),
            "unbounded".to_string(),
        ];

        // In range for every series: nothing to say.
        assert!(
            series_outside_envelope(dir.path(), &names, Scope::Kernel, "v7.1.5")
                .unwrap()
                .is_empty()
        );

        // Past `narrow`'s cap: it alone is reported, carrying its declared range.
        let out = series_outside_envelope(dir.path(), &names, Scope::Kernel, "v7.2").unwrap();
        assert_eq!(out, vec![("narrow".to_string(), ">=7.0, <7.2".to_string())]);

        // Release-strict, matching the build gate rather than the candidate path: an RC
        // satisfies neither bound, so every bounded series is reported and the
        // unbounded one still is not.
        let rc = series_outside_envelope(dir.path(), &names, Scope::Kernel, "v7.2-rc5").unwrap();
        assert_eq!(
            rc.iter().map(|(n, _)| n.as_str()).collect::<Vec<_>>(),
            ["narrow", "wide"]
        );
    }

    #[test]
    fn series_outside_envelope_asks_the_uboot_axis_about_the_uboot_version() {
        // The two axes move independently: a series that claims a narrow kernel range
        // and a wide u-boot one is in envelope for u-boot exactly when its
        // `applies_to_uboot` says so, and the kernel range must not leak into that
        // answer. u-boot tags are zero-padded (`v2026.04`), which is the shape the
        // range match has to accept on both sides.
        let dir = tempfile::tempdir().unwrap();
        let series = dir.path().join("series");
        std::fs::create_dir_all(&series).unwrap();
        std::fs::write(
            series.join("display.toml"),
            "applies_to_kernel = \">=7.0, <7.2\"\n\
             applies_to_uboot  = \">=2026.01, <2027.01\"\n\
             kernel = []\nffmpeg = []\nuserspace = []\nuboot = []\n",
        )
        .unwrap();
        // No u-boot envelope: claims every u-boot, so it is never reported.
        std::fs::write(
            series.join("loader.toml"),
            "kernel = []\nffmpeg = []\nuserspace = []\nuboot = []\n",
        )
        .unwrap();
        let names = ["display".to_string(), "loader".to_string()];

        // Inside the u-boot envelope, and the narrow kernel range does not interfere.
        assert!(
            series_outside_envelope(dir.path(), &names, Scope::Uboot, "v2026.04")
                .unwrap()
                .is_empty()
        );
        // Past its cap: reported, carrying the declared range as authored.
        let out = series_outside_envelope(dir.path(), &names, Scope::Uboot, "v2027.04").unwrap();
        assert_eq!(
            out,
            vec![("display".to_string(), ">=2026.01, <2027.01".to_string())]
        );
        // The same series against a kernel tag answers the kernel question instead.
        let out = series_outside_envelope(dir.path(), &names, Scope::Kernel, "v7.3").unwrap();
        assert_eq!(
            out,
            vec![("display".to_string(), ">=7.0, <7.2".to_string())]
        );
    }
}
