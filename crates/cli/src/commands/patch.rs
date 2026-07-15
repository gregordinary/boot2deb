//! `patch import`: fetch a patch, normalize it, and slot it into a profile.
//!
//! Fetches from a URL/file/stdin, normalizes to canonical `git am`-ready mbox, writes
//! it into the patches repo, inserts its label into the profile's scope at the
//! requested position, and — with `--verify-tree` — dry-run `git am`-verifies the
//! resulting series. The file write and the profile edit are rolled back if the
//! verify fails, so a rejected patch leaves the repo untouched.
//!
//! A successful import is deliberately inert: builds read the series at the lock's
//! pinned commit, so the patch reaches a build only after a commit in the patches repo
//! and a `boot2deb update` re-pin. The success output prints that exact loop (naming
//! the recipes whose locks the import invalidates) rather than leaving it to surface
//! as a build-time pin mismatch.

use crate::args::PatchImportArgs;
use boot2deb_core::mbox::{self, ImportMeta};
use boot2deb_core::model::Overrides;
use boot2deb_core::profile::derive_prefix;
use boot2deb_core::{load_profile, resolve_recipe, ConfigRoot};
use boot2deb_engine::{patches, patchimport, EngineError};

/// Run `patch import <source>`.
pub(crate) fn import(
    root: &ConfigRoot,
    source: &str,
    args: PatchImportArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    // `patch import` writes into a local clone (the patch file + the profile
    // edit), so a missing checkout is a setup error with a remedy — unlike
    // `build`, which auto-fetches pinned commits and needs no checkout.
    if !args.patches_path.join(".git").exists() {
        return Err(format!(
            "no patches checkout at {}: `patch import` writes into a local clone of \
             the patches repo; clone it there or point --patches-path at one",
            args.patches_path.display()
        )
        .into());
    }
    // Fetch: `-` reads stdin; otherwise a URL is fetched or a file is read.
    let bytes = if source == "-" {
        let mut buf = Vec::new();
        std::io::Read::read_to_end(&mut std::io::stdin(), &mut buf)?;
        buf
    } else {
        patchimport::fetch(source)?
    };
    let text = String::from_utf8_lossy(&bytes);

    // Normalize (pure): classify the shape and produce canonical mbox + subject.
    let meta = ImportMeta {
        author: Some(args.author.clone()),
        subject: args.subject.clone(),
        origin: args.origin.clone(),
    };
    let normalized = mbox::normalize(&text, &meta)?;

    // The current scope list fixes the insertion index and the derived prefix.
    let profile = load_profile(&args.patches_path, &args.profile)?;
    let scope_list = profile.scope(args.scope);
    let index = insert_index(args.position, scope_list.len())
        .map_err(|e| format!("--position {e} (the {} scope)", args.scope.as_str()))?;

    // The destination label: `--as` verbatim, else <dest-dir>/<prefix>-<slug>.patch.
    let label = match &args.label {
        Some(explicit) => explicit.clone(),
        None => {
            let dest_dir = args
                .dest_dir
                .clone()
                .unwrap_or_else(|| format!("media-accel/{}", args.scope.as_str()));
            let slug = args
                .name
                .clone()
                .unwrap_or_else(|| mbox::slugify(&normalized.subject));
            let prefix = derive_prefix(scope_list, index)?;
            format!("{dest_dir}/{prefix}-{slug}.patch")
        }
    };
    patchimport::safe_label(&label)?;

    let dest_path = args.patches_path.join(&label);
    if dest_path.exists() && !args.force {
        return Err(EngineError::PatchImportExists {
            path: dest_path.display().to_string(),
        }
        .into());
    }

    println!(
        "patch import: detected {}, subject \"{}\"",
        normalized.kind.label(),
        normalized.subject
    );

    // Write the normalized patch into the repo.
    if let Some(parent) = dest_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&dest_path, &normalized.mbox)?;
    println!("patch import: wrote {} ({} bytes)", label, normalized.mbox.len());

    // Recipes whose kernel uses this profile — named in both the "verify it now" hint
    // (when the import is unverified) and the re-pin follow-up. Computed once.
    let recipes = recipes_using_profile(root, &args.profile);

    // Verify the resulting series (with the new patch spliced in at `index`) against
    // a source checkout, if one was supplied. A failure rolls back the written file.
    match &args.verify_tree {
        Some(tree) => {
            let mut spliced = scope_list.to_vec();
            spliced.insert(index, label.clone());
            let target = format!("{} ({})", args.profile, tree.display());
            match patches::verify_tree(&args.patches_path, &spliced, tree, args.scope.as_str(), &target)
            {
                Ok(n) => println!(
                    "patch import: git am-verified the {} series ({n} patches) against {}",
                    args.scope.as_str(),
                    tree.display()
                ),
                Err(e) => {
                    let _ = std::fs::remove_file(&dest_path);
                    return Err(e.into());
                }
            }
        }
        None => {
            // Unverified: the patch is on disk but has not been dry-run against a
            // kernel tree, so a non-applying patch would surface only at build time.
            // Name the exact command that verifies it now — `verify-patches`
            // auto-fetches the locked kernel, so no hand-cloned checkout is needed —
            // and point at `--verify-tree` for verifying during import next time.
            let verify_cmd = match recipes.first() {
                Some(r) => format!("boot2deb verify-patches {r}"),
                None => "boot2deb verify-patches <recipe>".to_string(),
            };
            eprintln!("\n!! patch written but NOT verified — it has not been dry-run against a kernel tree.");
            eprintln!("   verify it now:   {verify_cmd}");
            eprintln!("                    (auto-fetches the locked kernel at its pin — no checkout needed)");
            eprintln!("   next time:        add --verify-tree <kernel-checkout> to verify during import.");
        }
    }

    // Slot the label into the profile manifest, preserving its comments/layout. If
    // this fails, roll back the written patch so no partial import survives.
    let profile_path = args
        .patches_path
        .join("profiles")
        .join(&args.profile)
        .join("profile.toml");
    if let Err(e) = patchimport::insert_into_profile(&profile_path, args.scope, index, &label) {
        let _ = std::fs::remove_file(&dest_path);
        return Err(e.into());
    }
    println!(
        "patch import: {}/{} now lists the patch at position {} of {}",
        args.profile,
        args.scope.as_str(),
        index + 1,
        scope_list.len() + 1
    );

    // The two follow-ups without which the import never reaches a build: the
    // series is read at the lock's pinned patches commit, so an uncommitted or
    // unpinned import surfaces later as a build-time pin mismatch.
    let patches = args.patches_path.display();
    println!("\nnext steps — no build reads the patch until the series is committed and re-pinned:");
    println!("  1. commit it:      git -C {patches} add -A && git -C {patches} commit");
    if recipes.is_empty() {
        println!(
            "  2. re-pin locks:   boot2deb update <recipe>   (each recipe whose kernel uses profile '{}')",
            args.profile
        );
    } else {
        for (i, recipe) in recipes.iter().enumerate() {
            let head = if i == 0 { "2. re-pin locks:  " } else { "                  " };
            println!("  {head} boot2deb update {recipe}");
        }
    }
    Ok(())
}

/// Map a 1-based `--position` onto an insertion index for a list of `len`
/// entries; `None` appends. 0 and anything past one-beyond-the-end are errors
/// naming the valid range — a silent clamp would put the patch somewhere other
/// than where the caller asked.
fn insert_index(position: Option<usize>, len: usize) -> Result<usize, String> {
    match position {
        None => Ok(len),
        Some(0) => Err(format!("is 1-based; use 1..={}, or omit it to append", len + 1)),
        Some(p) if p > len + 1 => Err(format!(
            "{p} is past the end of the {len}-entry list; use 1..={}",
            len + 1
        )),
        Some(p) => Ok(p - 1),
    }
}

/// Recipes whose resolved kernel uses patch profile `profile` — the locks a
/// `patch import` invalidates, named in its follow-up hint. Best-effort: a recipe
/// that fails to resolve, or a cwd outside any config root, contributes nothing
/// rather than failing the import (which itself needs only the patches repo).
fn recipes_using_profile(root: &ConfigRoot, profile: &str) -> Vec<String> {
    let Ok(names) = root.list("recipes") else {
        return Vec::new();
    };
    names
        .into_iter()
        .filter(|name| {
            resolve_recipe(root, name, &Overrides::default())
                .is_ok_and(|build| build.kernel.patch_profile() == Some(profile))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::repo_root;

    #[test]
    fn insert_index_validates_the_one_based_position() {
        // None appends.
        assert_eq!(insert_index(None, 3), Ok(3));
        // In-range 1-based values map to 0-based indices, up to one past the end.
        assert_eq!(insert_index(Some(1), 3), Ok(0));
        assert_eq!(insert_index(Some(4), 3), Ok(3));
        // 0 is not a position (the flag is 1-based)...
        assert!(insert_index(Some(0), 3).unwrap_err().contains("1-based"));
        // ...and past-the-end values are errors, not a silent clamp-to-append.
        assert!(insert_index(Some(5), 3).unwrap_err().contains("past the end"));
    }

    #[test]
    fn recipes_using_profile_finds_the_locks_an_import_invalidates() {
        // Every shipped RK1 recipe — base, media-accel, and jellyfin — resolves to the
        // rk3588-accel profile, because the patch profile lives on the shared kernel
        // axis. A `patch import` into it names each recipe's update command; an unknown
        // profile (or an unusable root) degrades to the generic hint.
        let root = repo_root();
        let recipes = recipes_using_profile(&root, "rk3588-accel");
        assert!(recipes.contains(&"turing-rk1-forky".to_string()), "{recipes:?}");
        assert!(recipes.contains(&"turing-rk1-media-accel-forky".to_string()), "{recipes:?}");
        assert!(recipes.contains(&"turing-rk1-jellyfin".to_string()), "{recipes:?}");
        assert!(recipes_using_profile(&root, "no-such-profile").is_empty());
        let empty = tempfile::tempdir().unwrap();
        assert!(recipes_using_profile(&ConfigRoot::new(empty.path()), "rk3588-accel").is_empty());
    }
}
