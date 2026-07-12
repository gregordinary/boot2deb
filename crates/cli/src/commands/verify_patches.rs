//! `verify-patches`: the patch gate — dry-run the locked series with `git am --3way`.
//!
//! Each tree is either an explicit `--<tree>-path` checkout or, when omitted,
//! auto-fetched at its locked pin into a durable cache — so a fresh clone can verify
//! with no hand-cloned trees. The kernel is always verified; ffmpeg/userspace only
//! when the profile carries patches for them (an empty scope needs no tree). The
//! patches checkout itself is resolved the same way `build` resolves it (explicit,
//! `../patches`, or auto-fetched at the lock's `patches.commit`).

use crate::args::VerifyArgs;
use crate::config::{fetch_verify_tree, resolve_patches_source, verify_trees_cache};
use crate::render::print_event;
use boot2deb_core::model::Overrides;
use boot2deb_core::{load_profile, resolve_recipe, ConfigRoot};
use boot2deb_engine::event::Event;
use boot2deb_engine::{patches, pins, EventSink};
use std::path::{Path, PathBuf};

/// Run `verify-patches <recipe>`.
pub(crate) fn run(
    root: &ConfigRoot,
    recipe: &str,
    args: VerifyArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let build = resolve_recipe(root, recipe, &Overrides::default())?;
    let lock = root.lock(recipe)?;
    let sink = |e: Event| print_event(&e);
    // Nothing to verify for a kernel that applies no series: report it and succeed,
    // rather than failing on a `patches` checkout the build would never read.
    let Some(pin) = lock.patches.as_ref() else {
        println!(
            "verify-patches {recipe}: this kernel applies no patch series (nothing to verify)"
        );
        return Ok(());
    };
    let (patches_root, _dev) = resolve_patches_source(
        args.patches_path.as_deref(),
        args.patches_url.as_deref(),
        &build,
        pin,
        root,
        &sink,
    )?;
    let profile = load_profile(&patches_root, &pin.profile)?;
    // Declared-intent gate: is the locked kernel in the profile's range?
    profile.ensure_applies(&pin.profile, &lock.kernel.reference)?;
    let target = format!("{} @ {}", lock.kernel.id, lock.kernel.reference);
    let cache_root = verify_trees_cache(root);

    let kernel_tree = match args.kernel_path {
        Some(p) => p,
        None => {
            // A `--kernel-src` local checkout/URL overrides the configured upstream
            // for the fetch; the tree still lands at exactly the locked commit.
            let url = match args.kernel_src {
                Some(s) => s,
                None => pins::kernel_source_url(&build.kernel.source)?,
            };
            fetch_verify_tree(
                &url,
                &lock.kernel.reference,
                &lock.kernel.commit,
                "kernel",
                &cache_root,
                &sink,
            )?
        }
    };
    // The ffmpeg/userspace series verify only for a media-accel build, which is the
    // only one carrying those source trees; without them there is nothing to fetch or
    // apply against (the profile's ffmpeg/userspace scopes, if any, are moot here).
    let ffmpeg_tree = match (&build.ffmpeg, &lock.ffmpeg) {
        (Some(ff), Some(ff_pins)) => tree_for_scope(
            args.ffmpeg_path,
            &profile.ffmpeg,
            args.ffmpeg_base_src.as_deref().unwrap_or(&ff.base.git),
            &ff_pins.base.reference,
            &ff_pins.base.commit,
            "ffmpeg base",
            &cache_root,
            &sink,
        )?,
        _ => None,
    };
    let userspace_tree = match (&build.userspace, &lock.userspace) {
        (Some(us), Some(us_pins)) => tree_for_scope(
            args.userspace_path,
            &profile.userspace,
            args.mpp_src.as_deref().unwrap_or(&us.mpp.git),
            &us_pins.mpp.reference,
            &us_pins.mpp.commit,
            "mpp",
            &cache_root,
            &sink,
        )?,
        _ => None,
    };

    // Verify the kernel series, plus any tree resolved above.
    let mut trees: Vec<(&str, &[String], &Path)> =
        vec![("kernel", profile.kernel.as_slice(), kernel_tree.as_path())];
    if let Some(p) = &ffmpeg_tree {
        trees.push(("ffmpeg", profile.ffmpeg.as_slice(), p.as_path()));
    }
    if let Some(p) = &userspace_tree {
        trees.push(("userspace", profile.userspace.as_slice(), p.as_path()));
    }

    let report = patches::verify_profile(&patches_root, &target, &trees)?;
    for (tree, n) in &report {
        println!("verify-patches {recipe}: {tree} series applies ({n} patches) against {target}");
    }
    Ok(())
}

/// Resolve one optional verify tree: an explicit `--<tree>-path` wins; otherwise
/// `source` (the configured upstream, or a `--<tree>-src` override the caller already
/// applied) is auto-fetched at the pin, but only when its `series` is non-empty (an
/// empty scope contributes no tree, so `None`).
#[allow(clippy::too_many_arguments)]
fn tree_for_scope(
    explicit: Option<PathBuf>,
    series: &[String],
    source: &str,
    reference: &str,
    commit: &str,
    what: &str,
    cache_root: &Path,
    sink: &dyn EventSink,
) -> Result<Option<PathBuf>, Box<dyn std::error::Error>> {
    match explicit {
        Some(p) => Ok(Some(p)),
        None if series.is_empty() => Ok(None),
        None => Ok(Some(fetch_verify_tree(source, reference, commit, what, cache_root, sink)?)),
    }
}
