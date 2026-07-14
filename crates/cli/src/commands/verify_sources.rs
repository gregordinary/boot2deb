//! `verify-sources`: the source-pin durability survey.
//!
//! Probes each locked pin against its *configured* upstream URL and reports whether
//! it is a durable tag, an ephemeral branch tip, or ORPHANED (not re-fetchable).
//! Read-only — `git ls-remote` plus a timeout-bounded ancestry check, no build, no
//! checkout, no hardware. An orphaned pin exits non-zero so CI can gate on it.

use crate::config::source_axes;
use crate::render::short;
use boot2deb_core::model::Overrides;
use boot2deb_core::{resolve_recipe, ConfigRoot};
use boot2deb_engine::sources;

/// Run `verify-sources <recipe>`.
pub(crate) fn run(root: &ConfigRoot, recipe: &str) -> Result<(), Box<dyn std::error::Error>> {
    let build = resolve_recipe(root, recipe, &Overrides::default())?;
    let lock = root.lock(recipe)?;
    let axes = source_axes(&build, &lock)?;
    // A recipe can pin nothing from git: a distro kernel comes from the mirror and a
    // board whose firmware is its own builds no bootloader. There is then no upstream
    // to rot, which is a stronger guarantee than "all pins are durable" — so say so
    // rather than reporting on an empty set.
    if axes.is_empty() {
        println!(
            "{recipe} fetches nothing from git (its kernel is a distro package and its boot \
             method builds no bootloader), so no source pin can rot upstream. Its package \
             versions are pinned by sha256 in {}.",
            lock.rootfs.manifest
        );
        return Ok(());
    }
    println!(
        "probing {} source pins for {recipe} against their configured upstreams (read-only)\n",
        axes.len()
    );
    let mut orphaned = 0usize;
    let mut undurable = 0usize;
    for axis in &axes {
        let d = sources::probe(&axis.url, axis.reference, axis.commit);
        // Show the ref only when it is a name; a bare-commit pin's ref is the commit.
        let ref_display = if axis.reference == axis.commit {
            "(bare commit)".to_string()
        } else {
            axis.reference.to_string()
        };
        println!(
            "  {:<12} {:<9} {} @ {}",
            axis.name,
            d.label(),
            ref_display,
            short(axis.commit)
        );
        println!("               {}", d.detail());
        match d {
            sources::Durability::Orphaned(_) => orphaned += 1,
            sources::Durability::Durable(_) => {}
            _ => undurable += 1,
        }
    }
    println!();
    if orphaned > 0 {
        return Err(format!(
            "{orphaned} source pin(s) are ORPHANED — not re-fetchable from their configured URL. \
             Re-pin via `boot2deb update` to a durable tag, or point the source at a mirror that \
             holds the commit (the config `git` field / a `--<pkg>-src` build override). A build \
             from these pins needs a local checkout of the source."
        )
        .into());
    }
    if undurable > 0 {
        println!(
            "{undurable} pin(s) are not durable tags (ephemeral or unconfirmed). They build today \
             but may rot upstream; pin release tags for long-term reproducibility."
        );
    } else {
        println!("all source pins are durable tags.");
    }
    Ok(())
}
