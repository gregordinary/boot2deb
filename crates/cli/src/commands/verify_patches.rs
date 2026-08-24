//! `verify-patches`: the patch gate — dry-run the locked series with `git am --3way`.
//!
//! Both patch axes are covered. The kernel axis verifies the `kernel` scope, plus
//! `ffmpeg`/`userspace` when the series carries patches for them; the u-boot axis
//! verifies the `uboot` scope. A recipe carrying one axis verifies that one; a
//! recipe carrying both verifies both, each against its own version — a u-boot
//! series makes no claim about a kernel, so reporting them at one target would
//! misdescribe one of them.
//!
//! Each tree is either an explicit `--<tree>-path` checkout or, when omitted,
//! auto-fetched at its locked pin into a durable cache — so a fresh clone can verify
//! with no hand-cloned trees. The patches checkout itself is resolved the same way
//! `build` resolves it (explicit, `../patches`, or auto-fetched at the pinned commit
//! from the lock's own record of where it came from) — but, unlike `build`, the pin is
//! reported rather than enforced. A local checkout is read as it stands, so that a
//! patch being written can be gated before it is committed; when that checkout is not
//! the pinned commit, the run says so, because its verdict is then about the working
//! tree and not about the series the lock names.

use crate::args::VerifyArgs;
use crate::config::{fetch_verify_tree, resolve_patches_source, verify_trees_cache};
use crate::render::{print_event_at, Verbosity};
use boot2deb_core::model::{Overrides, ResolvedBuild};
use boot2deb_core::series::Scope;
use boot2deb_core::{load_series, resolve_recipe, ConfigRoot, PatchSeries, RangeMatch};
use boot2deb_engine::event::Event;
use boot2deb_engine::patches::VerifyTree;
use boot2deb_engine::{patches, pins, EventSink};
use serde_json::json;
use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// One tree resolved for verification, owning its checkout and narrowed series so
/// the borrowed [`VerifyTree`] list can be assembled once at the end.
#[derive(Debug)]
struct ResolvedTree {
    /// Tree label for messages (`"kernel"`, `"uboot"`, …).
    label: &'static str,
    /// Patches-repo-relative labels this version selects, in apply order.
    series: Vec<String>,
    /// The checkout to apply them to.
    checkout: PathBuf,
    /// What that checkout is at, for messages.
    target: String,
}

/// Run `verify-patches <recipe>`.
///
/// Under `--json` the *verdict* is one document on stdout — per axis, how many
/// patches applied and every one that did not. The `git am` runs still stream to the
/// terminal like a build stage, because what `git` said is the evidence a failing CI
/// log has to carry.
pub(crate) fn run(
    root: &ConfigRoot,
    recipe: &str,
    args: VerifyArgs,
    json_out: bool,
    verbosity: Verbosity,
) -> Result<()> {
    let build = resolve_recipe(root, recipe, &Overrides::default())?;
    let lock = root.lock(recipe)?;
    let sink = move |e: Event| print_event_at(verbosity, &e);

    // Both axes read the same `patches` checkout at the same commit, so resolve it
    // once from whichever pin this recipe has. Nothing to verify when it has
    // neither: report it and succeed, rather than failing on a checkout the build
    // would never read.
    let Some(checkout_pin) = lock.patches.as_ref().or(lock.uboot_patches.as_ref()) else {
        if json_out {
            // A recipe with no series is a pass over an empty axis list, not a
            // different document: the same fields, all empty.
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "recipe": recipe, "axes": [], "failures": [], "result": "pass",
                }))?
            );
        } else {
            println!(
                "verify-patches {recipe}: this recipe applies no patch series (nothing to verify)"
            );
        }
        return Ok(());
    };
    let (patches_root, _dev) = resolve_patches_source(
        args.patches_path.as_deref(),
        args.patches_url.as_deref(),
        checkout_pin,
        root,
        &sink,
    )?;
    // This gate reads the series off the checkout as it stands, which is what patch
    // co-development needs — but it makes a green ambiguous, because it is then a
    // verdict on the working tree rather than on the series the lock names. Say which
    // one was measured whenever the two can differ. Silent in the ordinary case: an
    // auto-fetched checkout is a detached tree at the pin, so it never drifts.
    if let Some(drift) = pins::patches_drift(&patches_root, &checkout_pin.commit)? {
        println!("warning: {drift} — verifying the working tree's series, not the pinned one");
    }
    let cache_root = verify_trees_cache(root);

    // Unreachable entries across every series either axis names — a warning rather
    // than an error, because a dead entry breaks nothing; it is only clutter, and a
    // ledger that only grows becomes unreadable. Retiring one is safe: an old lock
    // names an old `patches` commit whose tree still holds the manifest line and the
    // file.
    let named: Vec<&String> = lock
        .patches
        .iter()
        .chain(lock.uboot_patches.iter())
        .flat_map(|p| &p.series)
        .collect();
    for name in &named {
        let series = load_series(&patches_root, name)?;
        report_unreachable(name, &series)?;
    }

    let mut trees = Vec::new();
    trees.extend(kernel_axis(
        &build,
        &lock,
        &patches_root,
        &cache_root,
        &args,
        &sink,
    )?);
    trees.extend(uboot_axis(
        &build,
        &lock,
        &patches_root,
        &cache_root,
        &args,
        &sink,
    )?);

    let on_failure = if args.keep_going {
        patches::OnFailure::KeepGoing
    } else {
        patches::OnFailure::Stop
    };
    // `VerifyTree::series` borrows a `&[&str]`, so the per-tree slices are built
    // first and outlive the list that points at them.
    let slices: Vec<Vec<&str>> = trees
        .iter()
        .map(|t| t.series.iter().map(String::as_str).collect())
        .collect();
    let borrowed: Vec<VerifyTree<'_>> = trees
        .iter()
        .zip(&slices)
        .map(|(t, series)| VerifyTree {
            label: t.label,
            series,
            checkout: &t.checkout,
            target: &t.target,
        })
        .collect();

    let (report, failures) = patches::verify_series(&patches_root, &borrowed, on_failure)?;
    if json_out {
        let axes: Vec<_> = report
            .iter()
            .zip(&trees)
            .map(|((label, n), tree)| {
                json!({
                    "axis": label,
                    "target": tree.target,
                    "applied": n,
                    "failed": failures.iter().filter(|f| &f.tree == label).count(),
                })
            })
            .collect();
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "recipe": recipe,
                "axes": axes,
                "failures": failures.iter().map(|f| json!({
                    "axis": f.tree, "patch": f.patch, "detail": f.detail,
                })).collect::<Vec<_>>(),
                "result": if failures.is_empty() { "pass" } else { "fail" },
            }))?
        );
        return if failures.is_empty() {
            Ok(())
        } else {
            Err(format!(
                "{} patch(es) do not apply — each failing patch was skipped, so a rework may \
                 change the results after it",
                failures.len()
            )
            .into())
        };
    }
    for ((label, n), tree) in report.iter().zip(&trees) {
        let failed = failures.iter().filter(|f| &f.tree == label).count();
        if failed == 0 {
            println!(
                "verify-patches {recipe}: {label} series applies ({n} patches) against {}",
                tree.target
            );
        } else {
            println!(
                "verify-patches {recipe}: {label} series has {failed} patch(es) that do not apply \
                 against {} ({n} applied)",
                tree.target
            );
        }
    }
    if !failures.is_empty() {
        // The whole point of the keep-going pass: every boundary in one report,
        // rather than one per re-run.
        println!("\n{} patch(es) did not apply:\n", failures.len());
        for f in &failures {
            println!("  [{}] {}\n{}\n", f.tree, f.patch, f.detail);
        }
        return Err(format!(
            "{} patch(es) do not apply — each failing patch was skipped, so a rework may \
             change the results after it",
            failures.len()
        )
        .into());
    }
    Ok(())
}

/// The kernel-family trees (`kernel`, plus `ffmpeg`/`userspace` where the series
/// carries patches for them), or an empty list when the lock pins no kernel series.
///
/// This is where `--kernel` applies: the candidate question is about a kernel tree,
/// and it relaxes both the envelope verdict and the range match.
fn kernel_axis(
    build: &ResolvedBuild,
    lock: &boot2deb_core::lock::Lock,
    patches_root: &Path,
    cache_root: &Path,
    args: &VerifyArgs,
    sink: &dyn EventSink,
) -> Result<Vec<ResolvedTree>> {
    let Some(pin) = lock.patches.as_ref() else {
        // A candidate kernel is meaningless without a kernel series to measure, and
        // silently ignoring it would report a green that answers a different question.
        if let Some(reference) = args.kernel.as_deref() {
            return Err(format!(
                "--kernel {reference} has nothing to measure: this recipe pins no kernel \
                 patch series"
            )
            .into());
        }
        return Ok(Vec::new());
    };
    // A patch series implies a tree to apply it to, so the lock pins both or neither.
    let (Some(kernel), Some(kernel_pin)) = (
        build.image.as_ref().and_then(|i| i.kernel.compiled()),
        lock.kernel.as_ref(),
    ) else {
        return Err(
            "the lock pins a kernel patch series but no kernel to apply it to — \
                    re-run `boot2deb update`"
                .to_string()
                .into(),
        );
    };
    let series = load_all(patches_root, &pin.series)?;

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
    for (name, s) in &series {
        if !s.applies_to_under(name, reference, mode)? {
            let declared = s.applies_to_kernel.as_deref().unwrap_or("*");
            match outside_envelope(candidate.is_some(), reference, name, declared) {
                Envelope::Measure(note) => println!("{note}"),
                Envelope::Refuse(err) => return Err(err.into()),
            }
        }
    }
    let target = format!("{} @ {reference}", kernel_pin.id);

    let kernel_tree = match args.kernel_path.clone() {
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
            let url = match args.kernel_src.clone() {
                Some(s) => s,
                None => pins::kernel_source_url(&kernel.source)?,
            };
            fetch_verify_tree(
                &url,
                &kernel_pin.reference,
                &kernel_pin.commit,
                "kernel",
                cache_root,
                sink,
            )?
        }
    };
    // Narrow each scope to the entries the kernel under test selects, composing the
    // series in the order the kernel names them. These decide whether a scope needs a
    // tree at all: a scope with no entries from any series contributes nothing to fetch.
    let kernel_series = narrow(&series, Scope::Kernel, reference, mode)?;
    let ffmpeg_series = narrow(&series, Scope::Ffmpeg, reference, mode)?;
    let userspace_series = narrow(&series, Scope::Userspace, reference, mode)?;

    let mut trees = vec![ResolvedTree {
        label: "kernel",
        series: kernel_series,
        checkout: kernel_tree,
        target: target.clone(),
    }];

    // The ffmpeg/userspace series verify only for a media-accel build, which is the
    // only one carrying those source trees; without them there is nothing to fetch or
    // apply against (the series' ffmpeg/userspace scopes, if any, are moot here).
    if let (Some(ff), Some(ff_pins)) = (
        build.image.as_ref().and_then(|i| i.ffmpeg.as_ref()),
        &lock.ffmpeg,
    ) {
        if let Some(checkout) = tree_for_scope(
            args.ffmpeg_path.clone(),
            &ffmpeg_series,
            &VerifySource {
                source: args.ffmpeg_base_src.as_deref().unwrap_or(&ff.base.git),
                reference: &ff_pins.base.reference,
                commit: &ff_pins.base.commit,
                what: "ffmpeg base",
            },
            cache_root,
            sink,
        )? {
            trees.push(ResolvedTree {
                label: "ffmpeg",
                series: ffmpeg_series,
                checkout,
                target: target.clone(),
            });
        }
    }
    // The `userspace` scope's patches apply to whichever tree declares `patched`, so
    // verification needs that tree specifically — a SoC with no patched tree has none to
    // verify against, and carries no userspace patches either.
    let patched = build
        .image
        .iter()
        .flat_map(|i| &i.userspace)
        .find(|t| t.patched);
    if let (Some(tree), Some(pin)) = (
        patched,
        patched.and_then(|t| lock.userspace.iter().find(|p| p.name == t.name)),
    ) {
        if let Some(checkout) = tree_for_scope(
            args.userspace_path.clone(),
            &userspace_series,
            &VerifySource {
                source: args.userspace_src.as_deref().unwrap_or(&tree.git),
                reference: &pin.reference,
                commit: &pin.commit,
                what: &tree.name,
            },
            cache_root,
            sink,
        )? {
            trees.push(ResolvedTree {
                label: "userspace",
                series: userspace_series,
                checkout,
                target,
            });
        }
    }
    Ok(trees)
}

/// The `uboot` tree, or an empty list when the lock pins no u-boot series.
///
/// Gated and narrowed against the *u-boot* version, not the kernel's: the two axes
/// move independently, and a series' `applies_to_uboot` is a claim about the u-boot
/// tag alone. Release-strict — u-boot's own `-rc` tags are not something a lock pins.
fn uboot_axis(
    build: &ResolvedBuild,
    lock: &boot2deb_core::lock::Lock,
    patches_root: &Path,
    cache_root: &Path,
    args: &VerifyArgs,
    sink: &dyn EventSink,
) -> Result<Vec<ResolvedTree>> {
    let Some(pin) = lock.uboot_patches.as_ref() else {
        return Ok(Vec::new());
    };
    // As on the kernel axis, a series implies a tree: the lock pins both or neither.
    let (Some(boot), Some(uboot_pin)) = (build.rkbin_boot(), lock.uboot.as_ref()) else {
        return Err(
            "the lock pins a u-boot patch series but no u-boot to apply it to — \
                    re-run `boot2deb update`"
                .to_string()
                .into(),
        );
    };
    let series = load_all(patches_root, &pin.series)?;
    let reference = &uboot_pin.reference;
    for (name, s) in &series {
        s.ensure_applies_uboot(name, reference)?;
    }
    let uboot_series = narrow(&series, Scope::Uboot, reference, RangeMatch::Release)?;
    let target = format!("u-boot @ {reference}");
    let checkout = match args.uboot_path.clone() {
        Some(p) => p,
        None => fetch_verify_tree(
            args.uboot_src.as_deref().unwrap_or(&boot.uboot_source),
            reference,
            &uboot_pin.commit,
            "u-boot",
            cache_root,
            sink,
        )?,
    };
    Ok(vec![ResolvedTree {
        label: "uboot",
        series: uboot_series,
        checkout,
        target,
    }])
}

/// Load every named series from the pinned checkout, keeping the lock's order.
fn load_all(patches_root: &Path, names: &[String]) -> Result<Vec<(String, PatchSeries)>> {
    names
        .iter()
        .map(|name| Ok((name.clone(), load_series(patches_root, name)?)))
        .collect()
}

/// The composed, version-narrowed patch list for one scope across every series,
/// in series order — what actually gets verified.
fn narrow(
    series: &[(String, PatchSeries)],
    scope: Scope,
    reference: &str,
    mode: RangeMatch,
) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for (name, s) in series {
        out.extend(
            s.series_for(scope, name, reference, mode)?
                .into_iter()
                .map(str::to_string),
        );
    }
    Ok(out)
}

/// Print a warning for every entry a series can never select.
fn report_unreachable(name: &str, series: &PatchSeries) -> Result<()> {
    for (scope, entry) in series.unreachable(name)? {
        println!(
            "warning: {} entry {} is unreachable — its range {} cannot overlap the {} \
             envelope of series '{}', so no version that series admits can select it; \
             retire the entry and its file",
            scope.as_str(),
            entry.path(),
            entry.kernels().unwrap_or("(none)"),
            // Each scope is gated by its own axis's envelope, so the range quoted back
            // must be the one the entry was actually measured against.
            series.envelope(scope).unwrap_or("*"),
            name,
        );
    }
    Ok(())
}

/// The upstream a verify tree is fetched from: where, at what, and what to call it.
///
/// A struct because all four are `&str`: a swapped pair would fetch the right repo at
/// the wrong commit, or cache one tree under another's name, and would compile.
struct VerifySource<'a> {
    /// The configured upstream, or a `--<tree>-src` override the caller applied.
    source: &'a str,
    /// The lock's human-readable ref, for the progress line.
    reference: &'a str,
    /// The exact commit to check out — what the verify actually holds the patches to.
    commit: &'a str,
    /// What this tree is, for the cache path and the step's log ("kernel", "ffmpeg", …).
    what: &'a str,
}

/// Resolve one optional verify tree: an explicit `--<tree>-path` wins; otherwise
/// `source` (the configured upstream, or a `--<tree>-src` override the caller already
/// applied) is auto-fetched at the pin, but only when its `series` is non-empty (an
/// empty scope contributes no tree, so `None`).
fn tree_for_scope(
    explicit: Option<PathBuf>,
    series: &[String],
    src: &VerifySource,
    cache_root: &Path,
    sink: &dyn EventSink,
) -> Result<Option<PathBuf>> {
    match explicit {
        Some(p) => Ok(Some(p)),
        None if series.is_empty() => Ok(None),
        None => Ok(Some(fetch_verify_tree(
            src.source,
            src.reference,
            src.commit,
            src.what,
            cache_root,
            sink,
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
/// There is no candidate path on the u-boot axis, so its envelope gate
/// ([`ensure_applies_uboot`](boot2deb_core::PatchSeries::ensure_applies_uboot))
/// always refuses.
///
/// Pure, so which envelope verdict a given axis reaches is unit-testable.
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
    use crate::testsupport::repo_root;

    /// A patches checkout holding one series manifest, for the axis tests. The
    /// `TempDir` is returned so the caller keeps it alive.
    fn patches_with(name: &str, manifest: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir(tmp.path().join("series")).unwrap();
        std::fs::write(
            tmp.path().join("series").join(format!("{name}.toml")),
            manifest,
        )
        .unwrap();
        tmp
    }

    /// `VerifyArgs` with every tree left to auto-fetch; tests set the fields they
    /// need so no test depends on a default it did not state.
    fn args() -> VerifyArgs {
        VerifyArgs {
            kernel_path: None,
            kernel_src: None,
            ffmpeg_path: None,
            ffmpeg_base_src: None,
            uboot_path: None,
            uboot_src: None,
            userspace_path: None,
            userspace_src: None,
            patches_path: None,
            patches_url: None,
            kernel: None,
            keep_going: false,
        }
    }

    #[test]
    fn a_uboot_only_recipe_resolves_a_uboot_tree_to_dry_run() {
        // The false green this replaces: `verify-patches rk3576-generic/loader` keyed
        // on `lock.patches`, found none, printed "nothing to verify" and exited 0 —
        // for a recipe whose whole deliverable is a patched u-boot, and which has no
        // other verification path at all.
        let root = repo_root();
        let build = resolve_recipe(&root, "rk3576-generic/loader", &Overrides::default()).unwrap();
        let lock = root.lock("rk3576-generic/loader").unwrap();
        assert!(lock.patches.is_none(), "the fixture recipe has no kernel");
        let patches = patches_with(
            "rk3576-loader",
            "uboot = [\"rk3576/loader/0001-a.patch\", \"rk3576/loader/0002-b.patch\"]\n",
        );
        // An explicit checkout keeps the test off the network; what is under test is
        // that the axis is resolved at all, and at the right version.
        let tree = tempfile::tempdir().unwrap();
        let mut args = args();
        args.uboot_path = Some(tree.path().to_path_buf());
        let sink = |_: Event| {};
        let cache = tempfile::tempdir().unwrap();
        let trees = uboot_axis(&build, &lock, patches.path(), cache.path(), &args, &sink).unwrap();

        assert_eq!(trees.len(), 1);
        assert_eq!(trees[0].label, "uboot");
        assert_eq!(trees[0].checkout, tree.path());
        assert_eq!(
            trees[0].series,
            ["rk3576/loader/0001-a.patch", "rk3576/loader/0002-b.patch"]
        );
        // Reported at the u-boot ref the lock pins, not a kernel's.
        assert_eq!(
            trees[0].target,
            format!("u-boot @ {}", lock.uboot.as_ref().unwrap().reference)
        );
    }

    #[test]
    fn the_uboot_axis_refuses_a_series_that_does_not_claim_the_pinned_uboot() {
        // The verify-side half of the envelope gate the build node now runs. A series
        // that claims only 2025 must not report a green against the 2026 u-boot the
        // lock pins — the gate needs a fixture, since every shipped u-boot series
        // deliberately declares no envelope.
        let root = repo_root();
        let build = resolve_recipe(&root, "rk3576-generic/loader", &Overrides::default()).unwrap();
        let lock = root.lock("rk3576-generic/loader").unwrap();
        let patches = patches_with(
            "rk3576-loader",
            "applies_to_uboot = \">=2025.01, <2026.01\"\nuboot = []\n",
        );
        let tree = tempfile::tempdir().unwrap();
        let mut args = args();
        args.uboot_path = Some(tree.path().to_path_buf());
        let sink = |_: Event| {};
        let cache = tempfile::tempdir().unwrap();
        let err = uboot_axis(&build, &lock, patches.path(), cache.path(), &args, &sink)
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not target u-boot"), "{err}");
        assert!(err.contains(">=2025.01, <2026.01"), "{err}");
    }

    #[test]
    fn a_candidate_kernel_is_refused_where_there_is_no_kernel_series_to_measure() {
        // `--kernel` asks "would this series survive X?". On a recipe with no kernel
        // series there is nothing to measure, and silently ignoring the flag would
        // report a green that answers a different question.
        let root = repo_root();
        let build = resolve_recipe(&root, "rk3576-generic/loader", &Overrides::default()).unwrap();
        let lock = root.lock("rk3576-generic/loader").unwrap();
        let patches = patches_with("rk3576-loader", "uboot = []\n");
        let cache = tempfile::tempdir().unwrap();
        let sink = |_: Event| {};
        let mut with_candidate = args();
        with_candidate.kernel = Some("v7.2".to_string());
        let err = kernel_axis(
            &build,
            &lock,
            patches.path(),
            cache.path(),
            &with_candidate,
            &sink,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("nothing to measure"), "{err}");
        // Without the flag the axis simply contributes no trees.
        assert!(
            kernel_axis(&build, &lock, patches.path(), cache.path(), &args(), &sink)
                .unwrap()
                .is_empty()
        );
    }

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
