//! Helpers over the [`ConfigRoot`] shared by several commands: the root's own
//! structural check, target resolution, the early config preflight, and the
//! search-path lookups (fragments, board `.dts`, apt keyrings, overlay trees) plus
//! the durable cache locations and the patches-checkout resolution.
//!
//! Everything here reads config; the side effects belong to the commands that call it.

use crate::fsutil::absolutize;
use boot2deb_core::model::{Overrides, ResolvedBuild};
use boot2deb_core::{resolve_device, resolve_recipe, ConfigRoot};
use boot2deb_engine::event::Step;
use boot2deb_engine::rootfs;
use boot2deb_engine::{image, patchfetch, pins, EngineError, EventSink};
use std::path::{Path, PathBuf};

/// Structural check that the primary `--root` points at a boot2deb config tree —
/// `base.toml` plus a `devices/` directory — run before any command dispatches
/// against it. Only the primary root is checked: overlays are partial by design
/// (a single retuned layer file is a valid overlay). The message shows the
/// absolutized path, since the offending value is usually the implicit default
/// `.` and "`.` not found" names nothing.
pub(crate) fn ensure_config_root(root: &ConfigRoot) -> Result<(), Box<dyn std::error::Error>> {
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

/// Resolve `target` as a recipe if one exists, else as a device.
pub(crate) fn resolve(
    root: &ConfigRoot,
    target: &str,
    overrides: Overrides,
) -> Result<ResolvedBuild, boot2deb_core::ConfigError> {
    if root.list("recipes")?.iter().any(|n| n == target) {
        // A name that is both a recipe and a device resolves as the recipe; surface
        // the ambiguity rather than silently preferring one.
        if root.list("devices")?.iter().any(|n| n == target) {
            eprintln!("note: '{target}' is both a recipe and a device — resolving as the recipe");
        }
        resolve_recipe(root, target, &overrides)
    } else {
        resolve_device(root, target, &overrides)
    }
}

/// Validate the resolved build's cheap, local config invariants: the whole
/// image geometry (offset ordering, alignment, GPT/rootfs fit — via the engine),
/// that every referenced kernel `config_fragments` file and `device_dts` source
/// exists under the config path, and that every declared apt source's signing
/// keyring is vendored.
/// Run by `resolve` (the documented first coherence gate), `update` (so a malformed
/// axis fails before the lock is committed), and `build` (so it fails before any
/// stage compiles) — a bad `rootfs_offset`, a typo'd fragment name, or a missing
/// keyring surfaces at resolution rather than deep in the build, the same
/// fail-early discipline as the device/kernel/suite checks.
pub(crate) fn preflight_config(
    root: &ConfigRoot,
    build: &ResolvedBuild,
) -> Result<(), Box<dyn std::error::Error>> {
    image::validate_geometry(build)?;
    // Resolve each fragment purely to assert it exists; the paths are re-resolved where
    // the kernel stage actually consumes them.
    fragment_paths(root, build)?;
    // Likewise the board's device-tree sources: a missing `.dts` must fail here, not
    // after the kernel has cloned and patched.
    device_dts_paths(root, build)?;
    // Resolve each keyring purely to assert it exists; the rootfs stage re-resolves
    // the paths it hands to mmdebstrap.
    apt_source_keyrings(root, &build.apt_sources)?;
    Ok(())
}

/// Resolve a build's kernel fragment names to `fragments/<name>.config` paths
/// along the config search path, erroring if any is missing. An overlay may
/// ship the fragments for a device/kernel it adds; the highest-precedence copy
/// wins.
pub(crate) fn fragment_paths(
    root: &ConfigRoot,
    build: &ResolvedBuild,
) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    // A distro kernel merges no fragments — Debian owns its config — so it resolves
    // to an empty list rather than an error.
    let Some(kernel) = build.kernel.compiled() else {
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
pub(crate) fn device_dts_paths(
    root: &ConfigRoot,
    build: &ResolvedBuild,
) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let mut paths = Vec::new();
    for rel in &build.device_dts {
        let path = root.find_asset(rel).ok_or_else(|| {
            format!("device_dts source not found: {rel} (searched the config path)")
        })?;
        paths.push(path);
    }
    Ok(paths)
}

/// Resolve each declared apt source's signing keyring to a vendored host path,
/// erroring on the first source whose keyring is missing: the repo is
/// verified during the rootfs solve, not trusted blindly, so its key is a
/// build-host prerequisite like the Debian archive keyring. Called from
/// [`preflight_config`] as the early existence gate and from the rootfs stage for
/// the paths it actually mounts.
pub(crate) fn apt_source_keyrings<'a>(
    root: &ConfigRoot,
    sources: &'a [boot2deb_core::model::AptSource],
) -> Result<Vec<rootfs::AptRepo<'a>>, Box<dyn std::error::Error>> {
    let mut repos = Vec::with_capacity(sources.len());
    for source in sources {
        let rel = format!("blobs/keyrings/{}", source.signed_by);
        let keyring = root.find_asset(&rel).ok_or_else(|| {
            format!(
                "apt source '{}' requires signing keyring '{}', but it is not vendored \
                 — add it under blobs/keyrings/ (see blobs/keyrings/README.md)",
                source.name, rel
            )
        })?;
        repos.push(rootfs::AptRepo { source, keyring });
    }
    Ok(repos)
}

/// Overlay directories for a build's rootfs, in merge order:
/// base → soc → boot-method → device → each feature. Each logical layer is
/// expanded along the config search path (shipped copy first, then any overlay's
/// copy of the same tree), so an overlay's overlay-tree stacks right after — and
/// thus wins over — the shipped one, matching the layer merge semantics. Absent
/// dirs contribute nothing.
///
/// `stage` selects *when* the tree is laid into the rootfs, which is a different
/// question from what is in it (see [`OverlayStage`]).
pub(crate) fn overlay_dirs(
    root: &ConfigRoot,
    b: &ResolvedBuild,
    stage: OverlayStage,
) -> Vec<PathBuf> {
    let dir = stage.dir_name();
    let mut rels = vec![
        format!("base/{dir}"),
        format!("socs/{}/{dir}", b.soc.as_str()),
        format!("boot-methods/{}/{dir}", b.boot_method.as_str()),
        format!("devices/{}/{dir}", b.device),
    ];
    for feature in &b.features {
        rels.push(format!("features/{feature}/{dir}"));
    }
    rels.iter().flat_map(|rel| root.find_asset_all(rel)).collect()
}

/// When a layer's overlay tree is laid into the rootfs.
///
/// Most config belongs *after* the packages, where it wins over whatever they
/// shipped — that is [`Customize`](OverlayStage::Customize), the `overlay/` tree, and
/// it is where nearly everything goes.
///
/// [`PreInstall`](OverlayStage::PreInstall) — the `overlay-pre/` tree — exists for the
/// config a package's own maintainer scripts have to *see while they run*. Two things
/// on a depthcharge board need it, and one of them is a safety property:
///
///  - `depthcharge-tools` registers a kernel hook that re-signs and re-flashes a
///    ChromeOS kernel partition. Installed with no config present, it runs at its
///    defaults and looks for that partition on **the build host's** disks. Its config
///    must exist, saying `enable-system-hooks = False`, before the package does.
///  - The initramfs settings (`MODULES=list`) must precede the kernel package, or the
///    first initramfs is built at `MODULES=most` — three times the size budget the
///    signed payload has — and then thrown away and rebuilt.
///
/// Both are cases where "config wins over the package" is not enough, because the
/// package *acted* before the config arrived.
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

/// The durable, shared cache of auto-fetched verify checkouts (`<root>/cache/verify-trees`),
/// commit-addressed by [`boot2deb_engine::srcfetch::ensure_tree`]. Sibling to the
/// patches and artifact caches; survives `clean` and is reused across recipes and
/// verify runs.
pub(crate) fn verify_trees_cache(root: &ConfigRoot) -> PathBuf {
    root.path().join("cache").join("verify-trees")
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
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let step = Step::start(sink, "fetch-source");
    let tree =
        boot2deb_engine::srcfetch::ensure_tree(source, reference, commit, what, cache_root, &step)?;
    step.finish();
    Ok(tree)
}

/// The content-addressed store for pre-built `extra_debs`: a durable
/// build-host cache under the config root, shared by `update` (which fills it) and
/// `build` (which reads it). It sits outside any recipe work dir, so `clean` leaves
/// it intact — the build no longer depends on the source staying put.
pub(crate) fn extra_debs_store(root: &ConfigRoot) -> PathBuf {
    root.path().join("cache").join("extra-debs")
}

/// Resolve the patches source for a build, returning the checkout path and
/// whether it is a co-development checkout (a pin mismatch is downgraded to a
/// warning rather than a hard error). Precedence:
///
/// 1. An explicit `--patches-path <dir>` — co-development from a working checkout.
/// 2. The default `../patches` if it is a git checkout — the pin is enforced.
/// 3. Auto-fetch the series at the lock's `patches.commit` from `--patches-url` or
///    the kernel definition's `patches_url`, into a durable commit-addressed cache
///    (`<root>/cache/patches/<commit>`), so a build with no local checkout resolves
///    automatically (the North-Star "selecting a device auto-fetches the right
///    patches"). With no URL available this is a hard [`EngineError::PatchesNoSource`]
///    naming the pinned commit — patches are never silently skipped.
pub(crate) fn resolve_patches_source(
    patches_path: Option<&Path>,
    patches_url: Option<&str>,
    resolved: &ResolvedBuild,
    pin: &boot2deb_core::lock::PatchesPin,
    root: &ConfigRoot,
    sink: &dyn EventSink,
) -> Result<(PathBuf, bool), Box<dyn std::error::Error>> {
    if let Some(path) = patches_path {
        return Ok((path.to_path_buf(), true));
    }
    let default_local = PathBuf::from("../patches");
    if default_local.join(".git").exists() {
        return Ok((default_local, false));
    }
    let url = patches_url
        .map(str::to_string)
        .or_else(|| resolved.kernel.compiled().and_then(|k| k.patches_url.clone()))
        .ok_or_else(|| EngineError::PatchesNoSource {
            commit: pin.commit.clone(),
        })?;
    let cache_root = root.path().join("cache").join("patches");
    let step = Step::start(sink, "patches");
    let dir = patchfetch::fetch_profile(&url, &pin.commit, &cache_root, &step)?;
    step.finish();
    Ok((dir, false))
}

/// The fetched source axes as `(name, configured upstream URL, locked ref,
/// locked commit)` — the set `verify-sources` probes and `update` warns on, always
/// against the *configured* URL (never a `--<pkg>-src` override). The ffmpeg
/// `rockchip` pin is provenance-only (never fetched at build), so it is omitted.
pub(crate) struct SourceAxis<'a> {
    /// Human name for the report (`kernel`, `u-boot`, `mpp`, …).
    pub(crate) name: &'static str,
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
) -> Result<Vec<SourceAxis<'a>>, Box<dyn std::error::Error>> {
    // Only sources the build actually fetches from git have a re-fetch durability to
    // report. A distro-package kernel is installed from the mirror and a depthcharge
    // board builds no bootloader, so neither contributes an axis.
    let mut axes = Vec::new();
    if let (Some(kernel), Some(pin)) = (build.kernel.compiled(), &lock.kernel) {
        axes.push(SourceAxis {
            name: "kernel",
            url: pins::kernel_source_url(&kernel.source)?,
            reference: &pin.reference,
            commit: &pin.commit,
        });
    }
    if let (Some(boot), Some(pin)) = (build.rkbin_boot(), &lock.uboot) {
        axes.push(SourceAxis {
            name: "u-boot",
            url: boot.uboot_source.clone(),
            reference: &pin.reference,
            commit: &pin.commit,
        });
    }
    // The fetched media-accel trees, present only for a build that compiles the
    // transcode stack. URLs come from the resolved build, pins from the lock — both
    // `Some` together.
    if let (Some(us), Some(us_pins)) = (&build.userspace, &lock.userspace) {
        axes.push(SourceAxis {
            name: "mpp",
            url: us.mpp.git.clone(),
            reference: &us_pins.mpp.reference,
            commit: &us_pins.mpp.commit,
        });
        axes.push(SourceAxis {
            name: "librga",
            url: us.librga.git.clone(),
            reference: &us_pins.librga.reference,
            commit: &us_pins.librga.commit,
        });
        axes.push(SourceAxis {
            name: "libmali",
            url: us.libmali.git.clone(),
            reference: &us_pins.libmali.reference,
            commit: &us_pins.libmali.commit,
        });
    }
    if let (Some(ff), Some(ff_pins)) = (&build.ffmpeg, &lock.ffmpeg) {
        axes.push(SourceAxis {
            name: "ffmpeg-base",
            url: ff.base.git.clone(),
            reference: &ff_pins.base.reference,
            commit: &ff_pins.base.commit,
        });
    }
    Ok(axes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::repo_root;

    #[test]
    fn preflight_accepts_shipped_config_and_rejects_bad_geometry_or_fragment() {
        // Geometry + fragment existence are validated up front (by both update
        // and build), so a bad axis fails at resolution, not deep in the build.
        let root = repo_root();
        let resolved = resolve_recipe(&root, "turing-rk1-forky", &Overrides::default()).unwrap();
        // The shipped RK1 config passes.
        preflight_config(&root, &resolved).unwrap();

        // A nonsensical rootfs offset (the review's own probe value) is rejected.
        let mut bad_geom = resolved.clone();
        if let boot2deb_core::model::ResolvedBoot::RockchipRkbin(boot) = &mut bad_geom.boot {
            boot.offsets.rootfs = "1".to_string();
        }
        assert!(preflight_config(&root, &bad_geom).is_err(), "bad geometry must fail preflight");

        // A referenced-but-missing kernel fragment is rejected.
        let mut bad_frag = resolved.clone();
        if let boot2deb_core::model::ResolvedKernel::Compiled(k) = &mut bad_frag.kernel {
            k.config_fragments.push("definitely-no-such-fragment".to_string());
        }
        let err = preflight_config(&root, &bad_frag).unwrap_err().to_string();
        assert!(err.contains("fragment not found"), "expected a fragment error, got: {err}");

        // A declared apt source whose signing keyring is not vendored is rejected at
        // preflight, not after the compile stages.
        let mut bad_key = resolved.clone();
        bad_key.apt_sources.push(boot2deb_core::model::AptSource {
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

    #[test]
    fn preflight_accepts_the_shipped_jellyfin_composition() {
        // The jellyfin recipe declares a third-party apt source; its signing
        // keyring is vendored, so the shipped composition passes the same gate
        // that rejects a missing one — `resolve turing-rk1-jellyfin` stays green.
        let root = repo_root();
        let resolved = resolve_recipe(&root, "turing-rk1-jellyfin", &Overrides::default()).unwrap();
        assert!(
            resolved.apt_sources.iter().any(|s| s.name == "jellyfin"),
            "the jellyfin feature declares its apt source"
        );
        preflight_config(&root, &resolved).unwrap();
    }

    #[test]
    fn ensure_config_root_accepts_a_config_tree_and_names_a_wrong_dir() {
        // The shipped repo root passes.
        ensure_config_root(&repo_root()).unwrap();
        // A directory that is not a config root fails, naming the path and the
        // --root remedy — the one clear error that replaces the per-command
        // "not found" cascade.
        let dir = tempfile::tempdir().unwrap();
        let err = ensure_config_root(&ConfigRoot::new(dir.path())).unwrap_err().to_string();
        assert!(err.contains("does not look like a boot2deb config root"), "{err}");
        assert!(err.contains("--root"), "remedy names the flag: {err}");
        // base.toml alone is not enough — devices/ must exist too.
        std::fs::write(dir.path().join("base.toml"), "packages = []\n").unwrap();
        assert!(ensure_config_root(&ConfigRoot::new(dir.path())).is_err());
        std::fs::create_dir(dir.path().join("devices")).unwrap();
        ensure_config_root(&ConfigRoot::new(dir.path())).unwrap();
    }

    #[test]
    fn source_axes_cover_every_fetched_tree_of_a_media_accel_build() {
        // The probed set is exactly what a build fetches: the two base trees plus the
        // media-accel ones. The ffmpeg `rockchip` pin is provenance-only, so it is not
        // an axis — pinning it against a URL nothing clones would be a false report.
        let root = repo_root();
        let build = resolve_recipe(&root, "turing-rk1-forky", &Overrides::default()).unwrap();
        let lock = root.lock("turing-rk1-forky").unwrap();
        let axes = source_axes(&build, &lock).unwrap();
        let names: Vec<&str> = axes.iter().map(|a| a.name).collect();
        assert_eq!(names, ["kernel", "u-boot", "mpp", "librga", "libmali", "ffmpeg-base"]);
        assert!(axes.iter().all(|a| !a.url.is_empty() && !a.commit.is_empty()));
    }
}
