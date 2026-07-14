//! `why-rebuild`: explain, per compile node, whether the next `build` reuses or
//! rebuilds its cached source tree — and which pinned inputs changed if it will
//! rebuild. Offline: reads only the lock and the on-disk build stamps; runs no
//! build, touches no network or hardware.

use crate::args::WhyRebuildArgs;
use crate::config::device_dts_paths;
use crate::fsutil::absolutize;
use boot2deb_core::model::Overrides;
use boot2deb_core::{resolve_recipe, ConfigRoot};
use boot2deb_engine::plan;
use std::path::PathBuf;

/// Run `why-rebuild <recipe>`.
pub(crate) fn run(
    root: &ConfigRoot,
    recipe: &str,
    args: WhyRebuildArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let lock = root.lock(recipe)?;
    // The kernel tree signature folds the board's device-tree sources, so the
    // prediction resolves the recipe to find them — an edited board `.dts` must be
    // reported as a rebuild, not a reuse.
    let build = resolve_recipe(root, recipe, &Overrides::default())?;
    let device_dts = device_dts_paths(root, &build)?;
    let work_dir = absolutize(
        args.work_dir
            .unwrap_or_else(|| PathBuf::from("build").join(recipe)),
    );
    let nodes = plan::plan_nodes(&plan::PlanInputs {
        lock: &lock,
        work_dir: &work_dir,
        patches_dev: args.patches_path.is_some(),
        // Co-dev predictions fold the live-series fingerprint, so pass the checkout
        // the build reads its patches from (CACHE-1); `None` in pinned mode.
        patches_root: args.patches_path.as_deref(),
        include_libmali: args.build_libmali,
        device_dts: &device_dts,
    });

    println!("why-rebuild {recipe} (work {})", work_dir.display());
    // A recipe can legitimately have no compile nodes at all — a board that installs
    // Debian's kernel and boots its own firmware compiles nothing, so there is nothing
    // to rebuild. Say that, rather than printing an empty list that reads as a bug.
    if nodes.is_empty() {
        println!(
            "  this recipe compiles nothing from source (the kernel is a distro package \
             and the boot method builds no bootloader), so it has no compile nodes to \
             reuse or rebuild. Its rootfs is keyed on the live package solve."
        );
        return Ok(());
    }
    for node in &nodes {
        let (verb, reason) = match &node.status {
            plan::NodeStatus::Absent => ("build", "no previous build".to_string()),
            plan::NodeStatus::Unstamped => ("rebuild", "tree present but not stamped".to_string()),
            plan::NodeStatus::Reuse => ("reuse", String::new()),
            plan::NodeStatus::Rebuild(changes) if changes.is_empty() => {
                ("rebuild", "build logic changed".to_string())
            }
            plan::NodeStatus::Rebuild(changes) => (
                "rebuild",
                changes.iter().map(|c| c.summary()).collect::<Vec<_>>().join(", "),
            ),
        };
        if reason.is_empty() {
            println!("  {:<18} {verb}", node.node);
        } else {
            println!("  {:<18} {verb}  ({reason})", node.node);
        }
    }
    // Scope note: the stamp gates only the cloned+patched *tree*; the compile
    // step always re-runs, and the rootfs cache keys on the live package solve.
    println!(
        "note: only each node's source tree is cached; the compile step always re-runs, \
         and the rootfs cache keys on the live package solve."
    );
    Ok(())
}
