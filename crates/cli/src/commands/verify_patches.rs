//! `verify-patches`: the patch gate — dry-run the locked series with `git am --3way`.
//!
//! Each tree is either an explicit `--<tree>-path` checkout or, when omitted,
//! auto-fetched at its locked pin into a durable cache — so a fresh clone can verify
//! with no hand-cloned trees. The kernel is always verified; ffmpeg/userspace only
//! when the series carries patches for them (an empty scope needs no tree). The
//! patches checkout itself is resolved the same way `build` resolves it (explicit,
//! `../patches`, or auto-fetched at the lock's `patches.commit`).

use crate::args::VerifyArgs;
use crate::config::{fetch_verify_tree, resolve_patches_source, verify_trees_cache};
use crate::render::print_event;
use boot2deb_core::model::Overrides;
use boot2deb_core::series::Scope;
use boot2deb_core::{load_series, resolve_recipe, ConfigRoot, RangeMatch};
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
    let (Some(kernel), Some(kernel_pin)) = (
        build.kernel.as_ref().and_then(|k| k.compiled()),
        lock.kernel.as_ref(),
    ) else {
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
    // Load every composed series; the kernel applies them in order, so verify checks
    // the concatenated series per scope against one tree.
    let mut series = Vec::with_capacity(pin.series.len());
    for name in &pin.series {
        series.push((name.clone(), load_series(&patches_root, name)?));
    }

    // Unreachable entries: ranges that no longer overlap the envelope, so no kernel
    // that series admits can select them. A warning rather than an error, because a
    // dead entry breaks nothing — it is only clutter, and a ledger that only grows
    // becomes unreadable. Retiring one is safe: an old lock names an old `patches`
    // commit whose tree still holds the manifest line and the file.
    for (name, series) in &series {
        for (scope, entry) in series.unreachable(name)? {
            println!(
                "warning: {} entry {} is unreachable — its range {} cannot overlap the series' \
                 {} envelope, so no kernel '{}' admits can select it; retire the entry and its file",
                scope.as_str(),
                entry.path(),
                entry.kernels().unwrap_or("(none)"),
                series.applies_to_kernel.as_deref().unwrap_or("*"),
                name,
            );
        }
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

    // Declared-intent gate: is the kernel under test in every composed series'
    // envelope? On the locked path one series that does not cover it fails the whole
    // set; a candidate is measured instead — see [`outside_envelope`].
    for (name, series) in &series {
        if !series.applies_to_under(name, reference, mode)? {
            let declared = series.applies_to_kernel.as_deref().unwrap_or("*");
            match outside_envelope(candidate.is_some(), reference, name, declared) {
                Envelope::Measure(note) => println!("{note}"),
                Envelope::Refuse(err) => return Err(err.into()),
            }
        }
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
    // Narrow every scope to the entries the kernel under test selects, once, composing
    // each series' entries in series order. These are what get verified and what
    // decide whether a scope needs a tree at all: a scope with no entries from any
    // series contributes nothing to fetch.
    let mut kernel_series: Vec<&str> = Vec::new();
    let mut ffmpeg_series: Vec<&str> = Vec::new();
    let mut userspace_series: Vec<&str> = Vec::new();
    for (name, series) in &series {
        kernel_series.extend(series.series_for(Scope::Kernel, name, reference, mode)?);
        ffmpeg_series.extend(series.series_for(Scope::Ffmpeg, name, reference, mode)?);
        userspace_series.extend(series.series_for(Scope::Userspace, name, reference, mode)?);
    }

    // The ffmpeg/userspace series verify only for a media-accel build, which is the
    // only one carrying those source trees; without them there is nothing to fetch or
    // apply against (the series' ffmpeg/userspace scopes, if any, are moot here).
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
    // The `userspace` scope's patches apply to the MPP tree, so verification needs
    // that tree specifically — a SoC that declares no MPP has no tree to verify
    // against, and (having no MPP) carries no userspace patches either.
    let userspace_tree = match (
        build.userspace.as_ref().and_then(|us| us.mpp.as_ref()),
        lock.userspace.as_ref().and_then(|p| p.mpp.as_ref()),
    ) {
        (Some(mpp), Some(mpp_pin)) => tree_for_scope(
            args.userspace_path,
            &userspace_series,
            args.mpp_src.as_deref().unwrap_or(&mpp.git),
            &mpp_pin.reference,
            &mpp_pin.commit,
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
    let (report, failures) = patches::verify_series(&patches_root, &target, &trees, on_failure)?;
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
        println!(
            "\n{} patch(es) did not apply against {target}:\n",
            failures.len()
        );
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
        None => Ok(Some(fetch_verify_tree(
            source, reference, commit, what, cache_root, sink,
        )?)),
    }
}

/// What a kernel falling outside a series' declared `applies_to_kernel` means for
/// this run.
enum Envelope {
    /// Say so and measure it anyway, carrying the note to print.
    Measure(String),
    /// Refuse, carrying the error.
    Refuse(String),
}

/// Decide how to treat a kernel the series does not claim.
///
/// On the **locked** path this is a real defect: the build would apply a series to a
/// kernel it makes no claim about, so it refuses.
///
/// On the **candidate** path it is the interesting case rather than a refusal.
/// "Would this series survive 7.2?" is asked precisely while the envelope still says
/// `<7.2`, so gating on the envelope would answer the question by assuming it — the
/// only way past would be to widen the claim first, which is the very thing the run
/// exists to test. So the run says the kernel is out of envelope and then measures
/// it, and what `git am` does is the answer. Per-entry ranges still narrow the series
/// ([`series_for`](boot2deb_core::PatchSeries::series_for) does not re-check the
/// envelope), so a patch already marked obsolete at this kernel drops out rather than
/// counting as a failure.
///
/// Pure, so the branch that used to be wrong is unit-testable.
fn outside_envelope(candidate: bool, reference: &str, series: &str, declared: &str) -> Envelope {
    if candidate {
        Envelope::Measure(format!(
            "note: {reference} is outside series '{series}' (declared {declared}) — measuring \
             it anyway; a clean result is the evidence for widening the envelope, not a claim \
             that it already covers this kernel"
        ))
    } else {
        Envelope::Refuse(format!(
            "kernel {reference} is outside series '{series}' (declared {declared})"
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_candidate_outside_the_envelope_is_measured_not_refused() {
        // The regression this guards: refusing here made `--kernel` unable to answer
        // the only question it exists for, since a series is asked about 7.2 exactly
        // while its envelope still excludes 7.2.
        let Envelope::Measure(note) =
            outside_envelope(true, "v7.2-rc5", "rk3588-accel", ">=7.0, <7.2")
        else {
            panic!("a candidate outside the envelope must be measured, not refused");
        };
        assert!(note.contains("v7.2-rc5"), "{note}");
        assert!(note.contains("rk3588-accel"), "{note}");
        assert!(note.contains(">=7.0, <7.2"), "{note}");
    }

    #[test]
    fn the_locked_kernel_outside_the_envelope_still_refuses() {
        // The build path keeps its gate: applying a series to a kernel it makes no
        // claim about is a defect, not a question.
        let Envelope::Refuse(err) = outside_envelope(false, "v7.2", "rk3588-accel", ">=7.0, <7.2")
        else {
            panic!("the locked path must refuse a kernel outside the envelope");
        };
        assert!(err.contains("v7.2"), "{err}");
        assert!(err.contains("rk3588-accel"), "{err}");
    }
}
