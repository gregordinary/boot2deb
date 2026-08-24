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

    // Names this build *produces* rather than installs. A media-accel provider feature
    // declares `requires_media_accel`, which is exactly the statement that its packages
    // come from the SoC's source trees and reach the rootfs through the build's own
    // local pool — a pool that does not exist until a build runs, so the archives are
    // rightly silent about them. Set aside rather than queried, so a real miss is not
    // buried among three names that were never going to be there.
    let built_here = locally_built(root, &build)?;
    let query: Vec<String> = build
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
    let apt_repos = apt_source_keyrings(root, &build.apt_sources)?;

    if !json_out {
        println!(
            "asking {} {} for {} package name(s) named by {recipe} (indexes only, no download)\n",
            build.arch,
            build.image_suite(),
            query.len(),
        );
    }
    let report = boot2deb_engine::archive::available(
        &build,
        &mirrors,
        keyring.as_deref(),
        &apt_repos,
        &query,
    )?;

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "recipe": recipe,
                "suite": build.image_suite(),
                "architecture": build.arch.debian_arch(),
                "mirrors": mirrors,
                "present": report.present,
                "provided": report.provided.iter().map(|(name, providers)| json!({
                    "name": name, "providers": providers,
                })).collect::<Vec<_>>(),
                "missing": report.missing,
                "built_here": built_here,
                "result": if report.is_complete() { "pass" } else { "fail" },
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
        println!();
        if report.is_complete() {
            println!(
                "OK: every package {recipe} names is available ({} from the archives, {} of them \
                 with alternative providers, {} built by this build).",
                report.present.len(),
                report.provided.len(),
                built_here.len(),
            );
        } else {
            println!(
                "{} of {} package name(s) are not in {} {}.",
                report.missing.len(),
                query.len(),
                build.arch,
                build.image_suite(),
            );
            if !build.extra_debs.is_empty() {
                println!(
                    "note: this recipe also pulls {} pre-built .deb(s) from outside the mirror. \
                     A name one of those supplies reaches the rootfs through the build's local \
                     pool and is listed above as missing, because the archives genuinely do not \
                     carry it.",
                    build.extra_debs.len()
                );
            }
        }
    }

    if report.is_complete() {
        Ok(())
    } else {
        Err(format!(
            "{} package name(s) named by {recipe} are not available in {} {}: {}",
            report.missing.len(),
            build.arch,
            build.image_suite(),
            report.missing.join(", "),
        )
        .into())
    }
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
    for name in &build.features {
        let feature = root.feature(name)?;
        if feature.requires_media_accel {
            built.extend(feature.packages.iter().cloned());
        }
    }
    Ok(built)
}
