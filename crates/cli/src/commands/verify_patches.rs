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
use boot2deb_core::profile::Scope;
use boot2deb_core::{load_profile, resolve_recipe, ConfigRoot, RangeMatch};
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
    // A patch series implies a tree to apply it to, so the lock pins both or neither.
    let (Some(kernel), Some(kernel_pin)) = (build.kernel.compiled(), lock.kernel.as_ref()) else {
        return Err(format!(
            "the lock for '{recipe}' pins a patch series but no kernel to apply it to — \
             re-run `boot2deb update`"
        )
        .into());
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

    // Unreachable entries: ranges that no longer overlap the envelope, so no kernel
    // this profile admits can select them. A warning rather than an error, because a
    // dead entry breaks nothing — it is only clutter, and a ledger that only grows
    // becomes unreadable. Retiring one is safe: an old lock names an old `patches`
    // commit whose tree still holds the manifest line and the file.
    for (scope, entry) in profile.unreachable(&pin.profile)? {
        println!(
            "warning: {} entry {} is unreachable — its range {} cannot overlap the profile's \
             {} envelope, so no kernel '{}' admits can select it; retire the entry and its file",
            scope.as_str(),
            entry.path(),
            entry.kernels().unwrap_or("(none)"),
            profile.applies_to_kernel,
            pin.profile,
        );
    }

    // A candidate kernel answers "would this series survive X?" without mutating the
    // lock. It also relaxes the match: a release candidate is read as its base
    // release, because an RC tree is exactly what the question is about, whereas the
    // locked path stays release-strict (the envelope claims released kernels).
    let candidate = args.kernel.as_deref();
    let reference = candidate.unwrap_or(&kernel_pin.reference);
    let mode = match candidate {
        Some(_) => RangeMatch::Candidate,
        None => RangeMatch::Release,
    };

    // Declared-intent gate: is the kernel under test in the profile's envelope?
    if !profile.applies_to_under(&pin.profile, reference, mode)? {
        return Err(format!(
            "kernel {reference} is outside profile '{}' (declared {}){}",
            pin.profile,
            profile.applies_to_kernel,
            match candidate {
                Some(_) => " — widen the envelope first if this candidate should be in range",
                None => "",
            }
        )
        .into());
    }
    let target = format!("{} @ {reference}", kernel_pin.id);
    let cache_root = verify_trees_cache(root);

    let kernel_tree = match args.kernel_path {
        Some(p) => p,
        None if candidate.is_some() => {
            // A candidate names a ref the lock does not pin, so there is no commit to
            // fetch it by. Requiring an explicit tree keeps this from silently
            // verifying against the locked kernel and reporting a green that answers
            // a different question.
            return Err(format!(
                "--kernel {reference} needs a tree holding it: pass --kernel-path \
                 <checkout> (checked out at {reference}), since the lock pins no \
                 commit for a kernel it does not name"
            )
            .into());
        }
        None => {
            // A `--kernel-src` local checkout/URL overrides the configured upstream
            // for the fetch; the tree still lands at exactly the locked commit.
            let url = match args.kernel_src {
                Some(s) => s,
                None => pins::kernel_source_url(&kernel.source)?,
            };
            fetch_verify_tree(
                &url,
                &kernel_pin.reference,
                &kernel_pin.commit,
                "kernel",
                &cache_root,
                &sink,
            )?
        }
    };
    // Narrow every scope to the entries the kernel under test selects, once. These
    // are what get verified and what decide whether a scope needs a tree at all: a
    // scope whose entries are all out of range contributes nothing to fetch.
    let series_for = |scope| profile.series_for(scope, &pin.profile, reference, mode);
    let kernel_series = series_for(Scope::Kernel)?;
    let ffmpeg_series = series_for(Scope::Ffmpeg)?;
    let userspace_series = series_for(Scope::Userspace)?;

    // The ffmpeg/userspace series verify only for a media-accel build, which is the
    // only one carrying those source trees; without them there is nothing to fetch or
    // apply against (the profile's ffmpeg/userspace scopes, if any, are moot here).
    let ffmpeg_tree = match (&build.ffmpeg, &lock.ffmpeg) {
        (Some(ff), Some(ff_pins)) => tree_for_scope(
            args.ffmpeg_path,
            &ffmpeg_series,
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
            &userspace_series,
            args.mpp_src.as_deref().unwrap_or(&us.mpp.git),
            &us_pins.mpp.reference,
            &us_pins.mpp.commit,
            "mpp",
            &cache_root,
            &sink,
        )?,
        _ => None,
    };

    // Verify the kernel series, plus any tree resolved above — each narrowed to the
    // entries the kernel under test selects.
    let mut trees: Vec<(&str, &[&str], &Path)> =
        vec![("kernel", &kernel_series, kernel_tree.as_path())];
    if let Some(p) = &ffmpeg_tree {
        trees.push(("ffmpeg", &ffmpeg_series, p.as_path()));
    }
    if let Some(p) = &userspace_tree {
        trees.push(("userspace", &userspace_series, p.as_path()));
    }

    let on_failure = if args.keep_going {
        patches::OnFailure::KeepGoing
    } else {
        patches::OnFailure::Stop
    };
    let (report, failures) = patches::verify_profile(&patches_root, &target, &trees, on_failure)?;
    for (tree, n) in &report {
        let failed = failures.iter().filter(|f| &f.tree == tree).count();
        if failed == 0 {
            println!(
                "verify-patches {recipe}: {tree} series applies ({n} patches) against {target}"
            );
        } else {
            println!(
                "verify-patches {recipe}: {tree} series has {failed} patch(es) that do not apply \
                 against {target} ({n} applied)"
            );
        }
    }
    if !failures.is_empty() {
        // The whole point of the keep-going pass: every boundary in one report,
        // rather than one per re-run.
        println!("\n{} patch(es) did not apply against {target}:\n", failures.len());
        for f in &failures {
            println!("  [{}] {}\n{}\n", f.tree, f.patch, f.detail);
        }
        return Err(format!(
            "{} patch(es) do not apply against {target} — each failing patch was skipped, \
             so a rework may change the results after it",
            failures.len()
        )
        .into());
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
    series: &[&str],
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
