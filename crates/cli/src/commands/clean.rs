//! `clean`: remove a recipe's build scratch (or a selected subtree), to reclaim disk
//! or force a clean rebuild. `--dry-run` previews with sizes and removes nothing.
//!
//! Only directories `build` stamped as boot2deb-owned are removed (SEC-7, see
//! [`check_work_dir_removable`]); `--force` overrides. Without that guard a mistyped
//! `--work-dir` would be a recursive delete of an arbitrary tree.

use crate::args::CleanArgs;
use crate::fsutil::{absolutize, dir_size};
use crate::render::human_size;
use crate::workdir::check_work_dir_removable;
use boot2deb_core::ConfigRoot;
use std::path::PathBuf;

/// Run `clean <recipe>`.
pub(crate) fn run(
    root: &ConfigRoot,
    recipe: &str,
    args: CleanArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    // Validate the recipe-name shape (reject `..`/absolute/separators) before it is
    // joined into a filesystem path, consistent with the config write paths (SEC-2).
    root.lock_path(recipe)?;
    let work_dir = absolutize(
        args.work_dir
            .unwrap_or_else(|| PathBuf::from("build").join(recipe)),
    );
    // The whole-tree default and the cache/sandbox selectors all remove within the
    // caller-supplied work dir, so they require the SEC-7 ownership stamp. The
    // artifacts selector is exempt: its target lives under the config root's own
    // cache, not at a path the caller chose.
    if !args.artifacts || args.cache || args.sandbox {
        check_work_dir_removable(&work_dir, args.force)?;
    }
    // Selectors carve out a subtree; with none, the whole work dir goes. The
    // artifact store is a selector too, but lives under the config root (shared
    // across recipes), not the work dir.
    let targets: Vec<PathBuf> = match (args.cache, args.sandbox, args.artifacts) {
        (false, false, false) => vec![work_dir.clone()],
        (cache, sandbox, artifacts) => {
            let mut t = Vec::new();
            if cache {
                t.push(work_dir.join("cache"));
            }
            if sandbox {
                t.push(work_dir.join("sandbox"));
            }
            if artifacts {
                t.push(absolutize(root.path().join("cache").join("artifacts")));
            }
            t
        }
    };

    let mut removed_any = false;
    for target in &targets {
        if !target.exists() {
            println!("  {} (absent)", target.display());
            continue;
        }
        let size = human_size(dir_size(target));
        if args.dry_run {
            println!("  would remove {} ({size})", target.display());
        } else {
            std::fs::remove_dir_all(target)
                .map_err(|e| format!("failed to remove {}: {e}", target.display()))?;
            println!("  removed {} ({size})", target.display());
            removed_any = true;
        }
    }
    if args.dry_run {
        println!("(dry run — nothing removed)");
    } else if !removed_any {
        println!("nothing to remove");
    }
    Ok(())
}
