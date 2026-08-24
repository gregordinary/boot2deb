//! `verify-packages`: does the archive carry what the recipe asks for?
//!
//! A recipe's package list is config like any other, and it is the one part of it the
//! config tree cannot check on its own — whether `forky` ships `firmware-misc-nonfree`
//! is a fact about an archive, not about a TOML file. Left unchecked it is found at
//! resolve time, after every compile node has already run, and found badly: a top-level
//! include naming nothing fails the *whole* resolve, so the failure says the set was
//! unsatisfiable and never which names were the problem.
//!
//! This asks the question directly, per recipe, before anything is built. It runs the
//! read half of a resolve — release, indexes, stop — so one pass answers every name at
//! once and nothing is downloaded, unpacked, or executed.
//!
//! Read-only and network-only: no build, no sandbox, no hardware. A missing package
//! exits non-zero, so CI can gate a board's recipes on it.

use crate::config::apt_source_keyrings;
use boot2deb_core::model::{Overrides, ResolvedBuild};
use boot2deb_core::{resolve_recipe, ConfigRoot};
use serde_json::json;
use std::collections::BTreeSet;

/// Run `verify-packages <recipe>`.
pub(crate) fn run(
    root: &ConfigRoot,
    recipe: &str,
    json_out: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let build = resolve_recipe(root, recipe, &Overrides::default())?;
    // A package set is an image's; a u-boot deliverable installs nothing from an
    // archive, so there is nothing here to hold one to.
    let ib = build.as_image().ok_or_else(|| {
        format!(
            "recipe '{recipe}' has deliverable = \"uboot\": it resolves no suite and no \
             rootfs package set, so there is nothing to verify against an archive"
        )
    })?;
    let image = ib.image;

    // Names this build *produces* rather than installs. A media-accel provider feature
    // declares `requires_media_accel`, which is exactly the statement that its packages
    // come from the SoC's source trees and reach the rootfs through the build's own
    // local pool — a pool that does not exist until a build runs, so the archives are
    // rightly silent about them. Set aside rather than queried, so a real miss is not
    // buried among three names that were never going to be there.
    let built_here = locally_built(root, &build)?;
    let query: Vec<String> = image
        .rootfs_packages
        .iter()
        .filter(|name| !built_here.contains(*name))
        .cloned()
        .collect();

    // The archives a build would resolve against: the lock's captured snapshot where it
    // has one, else the live mirror. Verifying against a different archive than the
    // build resolves from would not be a verification.
    let lock = root.lock(recipe).ok();
    let mirrors = boot2deb_engine::snapshot::resolve_mirrors(
        boot2deb_engine::DEFAULT_MIRROR,
        lock.as_ref().and_then(|l| l.snapshot.as_ref()),
        lock.as_ref()
            .and_then(|l| l.snapshot.as_ref())
            .map(|s| s.mode)
            .unwrap_or(boot2deb_core::lock::SnapshotMode::Off),
    )?;
    let keyring = {
        let vendored =
            root.find_trust_anchor("blobs/keyrings/debian-archive-keyring.gpg", false)?;
        if let Some(path) = &vendored {
            boot2deb_engine::keyring::verify(path)?;
        }
        vendored
    };
    let apt_repos = apt_source_keyrings(root, &image.apt_sources)?;

    if !json_out {
        println!(
            "asking {} {} for {} package name(s) named by {recipe} (indexes only, no download)\n",
            build.arch,
            image.suite,
            query.len(),
        );
    }
    let report =
        boot2deb_engine::archive::available(ib, &mirrors, keyring.as_deref(), &apt_repos, &query)?;

    // A conditional package entry naming a suite this tree never builds. Reported here
    // because this is the command about whether a recipe's package set is right, and it
    // is the one failure enumerating suites can hide: the entry simply never applies, so
    // the package goes missing with nothing said. Advice rather than a verdict — a tree
    // may legitimately carry a layer ahead of the recipe that will use it — so it does
    // not change the exit code.
    let unreachable = root.unreachable_suites().unwrap_or_default();

    // The second question, asked only when the first passed. A name the archives do not
    // carry at all refuses its own dependency group too, so running the closure over a
    // set with a known-missing name would report the same fact twice and bury whatever
    // else it found. When every name is there, this is the only thing left that can make
    // the set unbuildable.
    let closure = if report.is_complete() {
        Some(boot2deb_engine::archive::closure(
            ib,
            &mirrors,
            keyring.as_deref(),
            &apt_repos,
            &query,
        )?)
    } else {
        None
    };
    // Every name this build puts in its own local pool rather than taking from an
    // archive. The resolve above cannot see any of it — the pool does not exist until a
    // build runs — so a refusal naming one of these is the check's blind spot rather
    // than a real gap, and is reported as such instead of failing the recipe.
    let mut supplied_locally = built_here.clone();
    supplied_locally.extend(extra_deb_names(image));
    let (explained, unexplained): (Vec<_>, Vec<_>) = closure
        .iter()
        .flat_map(|c| &c.refusals)
        .partition(|refusal| satisfiable_by(&refusal.requirement, &supplied_locally));

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "recipe": recipe,
                "suite": &image.suite,
                "architecture": build.arch.debian_arch(),
                "mirrors": mirrors,
                "present": report.present,
                "provided": report.provided.iter().map(|(name, providers)| json!({
                    "name": name, "providers": providers,
                })).collect::<Vec<_>>(),
                "missing": report.missing,
                "built_here": built_here,
                "unreachable_suites": unreachable.iter().map(|u| json!({
                    "layer": u.layer, "package": u.package, "suite": u.suite,
                })).collect::<Vec<_>>(),
                // Absent, not empty, when the name check failed: no closure was
                // resolved, which is a different statement than one that refused nothing.
                "closure": closure.as_ref().map(|c| json!({
                    // Null where the resolution did not finish, so a consumer can tell
                    // "closed at nothing" from "never closed".
                    "installed": c.installed,
                    // Split the way the verdict splits: `refusals` is what the recipe
                    // has to correct, `supplied_locally` is what this check cannot see.
                    "refusals": unexplained.iter().map(|r| json!({
                        "requirement": r.requirement,
                        "required_by": r.required_by,
                        "reason": r.reason,
                    })).collect::<Vec<_>>(),
                    "supplied_locally": explained.iter().map(|r| json!({
                        "requirement": r.requirement,
                        "required_by": r.required_by,
                    })).collect::<Vec<_>>(),
                })),
                "result": if report.is_complete() && closure.is_some() && unexplained.is_empty() {
                    "pass"
                } else {
                    "fail"
                },
            }))?
        );
    } else {
        for (name, providers) in &report.provided {
            println!(
                "provided : {name} — also satisfiable by {}",
                providers.join(", ")
            );
        }
        for name in &built_here {
            println!("built    : {name} (this build produces it; not asked of the archive)");
        }
        for name in &report.missing {
            println!("MISSING  : {name}");
        }
        for u in &unreachable {
            println!(
                "note     : {} in {} names suite '{}', which no recipe in this tree builds \
                 — check the spelling",
                u.package, u.layer, u.suite,
            );
        }
        for refusal in &explained {
            println!(
                "local    : {} requires {} — supplied by this build, not by an archive",
                refusal.required_by, refusal.requirement,
            );
        }
        for refusal in &unexplained {
            println!(
                "UNSATISFIED: {} requires {}, and {}",
                refusal.required_by, refusal.requirement, refusal.reason,
            );
        }
        println!();
        if report.is_complete() && closure.is_some() && unexplained.is_empty() {
            print!(
                "OK: every package {recipe} names is available ({} from the archives, {} of them \
                 with alternative providers, {} built by this build)",
                report.present.len(),
                report.provided.len(),
                built_here.len(),
            );
            // A size only where the resolution actually finished. Where a locally
            // supplied dependency stopped it, there is no closure to have counted — and
            // saying so beats a number that would read as a set closing at nothing.
            match closure.as_ref().and_then(|c| c.installed) {
                Some(installed) => println!(", and the set closes at {installed} package(s)."),
                None => println!(
                    ". The closure was not counted: it stops at the {} requirement(s) above that \
                     this build supplies itself, which a resolve cannot see yet.",
                    explained.len(),
                ),
            }
        } else if !unexplained.is_empty() {
            println!(
                "every name {recipe} lists is in {} {}, but {} of their dependencies cannot be \
                 satisfied from it. A package whose dependency is missing still installs — dpkg \
                 configures with --force-depends — and leaves an image that cannot configure \
                 packages at all, including unrelated ones.",
                build.arch,
                image.suite,
                unexplained.len(),
            );
        } else {
            println!(
                "{} of {} package name(s) are not in {} {}.",
                report.missing.len(),
                query.len(),
                build.arch,
                image.suite,
            );
            if !image.extra_debs.is_empty() {
                println!(
                    "note: this recipe also pulls {} pre-built .deb(s) from outside the mirror. \
                     A name one of those supplies reaches the rootfs through the build's local \
                     pool and is listed above as missing, because the archives genuinely do not \
                     carry it.",
                    image.extra_debs.len()
                );
            }
        }
    }

    if !report.is_complete() {
        return Err(format!(
            "{} package name(s) named by {recipe} are not available in {} {}: {}",
            report.missing.len(),
            build.arch,
            image.suite,
            report.missing.join(", "),
        )
        .into());
    }
    if unexplained.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{} dependency requirement(s) of {recipe}'s package set cannot be satisfied in {} {}: {}",
        unexplained.len(),
        build.arch,
        image.suite,
        unexplained
            .iter()
            .map(|r| r.requirement.as_str())
            .collect::<Vec<_>>()
            .join(", "),
    )
    .into())
}

/// The package names this build compiles rather than installs: those contributed by a
/// selected feature declaring `requires_media_accel`.
///
/// That flag is the config tree's own statement that a feature's `.deb`s come from the
/// SoC's `[userspace]`/`[ffmpeg]` source trees instead of the Debian mirror, so it is
/// the one place this can be read from rather than guessed. A build that selects no such
/// feature returns an empty set and every name it lists is asked of the archives.
fn locally_built(
    root: &ConfigRoot,
    build: &ResolvedBuild,
) -> Result<BTreeSet<String>, Box<dyn std::error::Error>> {
    let mut built = BTreeSet::new();
    // Filtered to the suite being built, like the merged set itself: a conditional entry
    // that does not apply here contributes no package, so naming it as "built by this
    // build" would set aside something the build never installs.
    let suite = build
        .image
        .as_ref()
        .map(|i| i.suite.as_str())
        .unwrap_or_default();
    for name in build.image.iter().flat_map(|i| &i.features) {
        let feature = root.feature(name)?;
        if feature.requires_media_accel {
            built.extend(
                feature
                    .packages
                    .iter()
                    .filter(|entry| entry.applies_to(suite))
                    .map(|entry| entry.name().to_string()),
            );
        }
    }
    Ok(built)
}

/// The package name each `[[extra_debs]]` entry supplies, read off its filename.
///
/// A pre-built `.deb` is pinned by URL or path and a sha256, and nothing in the config
/// states what package is inside it — the name is in the file, and reading it would mean
/// downloading and unpacking every pin, which is exactly what this command promises not
/// to do. The archive's own filename convention carries it instead:
/// `<package>_<version>_<arch>.deb`, so the text before the first `_` is the name.
///
/// Used only to explain a refusal, never to claim a package is present. A filename that
/// does not follow the convention simply explains nothing, and the refusal it would have
/// covered is reported — the safe direction for a heuristic.
fn extra_deb_names(image: &boot2deb_core::model::ResolvedImage) -> BTreeSet<String> {
    image
        .extra_debs
        .iter()
        .filter_map(|deb| deb.locator().ok())
        .map(|locator| match locator {
            boot2deb_core::model::ExtraDebLocator::Url(s)
            | boot2deb_core::model::ExtraDebLocator::Path(s) => s,
        })
        .filter_map(|locator| locator.rsplit('/').next())
        .filter_map(|file| file.split('_').next())
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

/// Whether any name in `supplied` satisfies the dependency group `requirement`.
///
/// The requirement is the archive's own text for the group, so it carries Policy 7.1
/// syntax: alternatives separated by `|`, each optionally followed by a version
/// constraint in parentheses and an architecture qualifier after `:`. Only the name is
/// compared — a local `.deb` is pinned by digest and this cannot know its version, so
/// claiming a *constraint* is met would be a claim the check cannot support. The version
/// is the build's own problem, and the build is where it is checked.
fn satisfiable_by(requirement: &str, supplied: &BTreeSet<String>) -> bool {
    requirement.split('|').any(|alternative| {
        let name = alternative
            .trim()
            .split(['(', ' ', '\t'])
            .next()
            .unwrap_or_default()
            .split(':')
            .next()
            .unwrap_or_default();
        !name.is_empty() && supplied.contains(name)
    })
}
