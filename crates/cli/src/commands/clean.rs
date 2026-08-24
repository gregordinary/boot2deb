//! `clean`: remove a recipe's build scratch (or a selected subtree), and sweep the
//! durable caches every recipe shares, to reclaim disk or force a clean rebuild.
//! `--dry-run` previews with sizes and removes nothing.
//!
//! Two scopes of selector, distinguished by what they can address. The **work-dir**
//! selectors (`--cache`, `--sandbox`, `--build-roots`, and the no-selector whole-tree
//! default) name a subtree of one recipe's scratch, so they need a recipe and they
//! need the ownership stamp: without that guard a mistyped `--work-dir` would be a
//! recursive delete of an arbitrary tree (see [`check_work_dir_removable`];
//! `--force` overrides). The **root-scoped** selectors (`--artifacts`,
//! `--verify-trees`, `--kconfig`, `--all-caches`) name stores under the config root
//! that every recipe shares, at paths derived from `--root` rather than chosen by the
//! caller — so they sweep without a recipe and without the stamp, which is what makes
//! a routine disk sweep one command instead of one per slug.
//!
//! `--build-roots` is the narrow work-dir selector, and the only one whose targets are
//! not a fixed path: it sweeps the build roots and their overlay layers while sparing
//! the packaging root, which is what a build root aged past the archive needs.
//!
//! Every removal goes through [`boot2deb_engine::sandbox::reclaim_tree`], which is the
//! only route that gets past the mode-`0` work area an overlay leaves and past a
//! subuid-owned tree. It carries one consequence worth stating: the provisioner's
//! removal also takes the `<target>.lock` a published rootfs carries beside it. For a
//! base tree that is right — the lock is the provisioner's own, and `--build-roots`
//! names it as a target too. For a target that merely *contains* such trees (a work
//! dir, `cache/`, `sandbox/`) the `.lock` sibling is someone else's file, and for the
//! whole-tree default it sits outside the stamped directory. Nothing writes those paths
//! today, so this costs nothing; closing it needs a seam in `ferroday-cage`'s `Remove`,
//! which exposes only the identity map.
//!
//! `--verify-trees` is the only selector that prunes *within* a store rather than
//! removing it. Both auto-fetch caches are commit-addressed, so liveness is decidable:
//! a checkout whose commit no lock in the config tree names can only be re-fetched,
//! never read back from, and is dead. That decision is only sound if the pinned set is
//! complete, so an unreadable or unparseable lock aborts the sweep rather than
//! narrowing it. The one narrowing this cannot detect is a missing `--overlay`: the
//! locks are read from the search paths, so a sweep invoked without the overlays a
//! build uses never sees their pins. The run reports how many locks it read for that
//! reason — a count short of the tree the operator knows is the signal.

use crate::args::CleanArgs;
use crate::config::{artifact_cache, cache_dir, kconfig_cache, patches_cache, verify_trees_cache};
use crate::fsutil::dir_size;
use crate::render::human_size;
use crate::workdir::{check_work_dir_removable, work_dir_for};
use boot2deb_core::ConfigRoot;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Every commit any lock in the config tree pins — the liveness set
/// [`unpinned_checkouts`] sweeps a commit-addressed cache against.
///
/// Reads `recipes/*.lock` and `recipes/<device>/*.lock` under every search path — but
/// not the `<recipe>.pkgs.lock` manifests beside them, which share the extension and
/// are not TOML. An overlay's recipes count exactly as the shipped ones do. Locks are
/// read by *path* rather than through the recipe inventory on purpose: a lock whose
/// `.toml` was deleted still pins its checkouts, and listing recipes would miss it and
/// call those trees dead.
///
/// Returns the commits and how many locks were read for them. The count is reported to
/// the operator because the one way this set can be silently narrow is outside the
/// command's reach: the search paths come from `--overlay`, so a sweep invoked without
/// the overlays a build uses reads fewer locks and calls their checkouts dead. A count
/// that does not match the tree the operator knows is the signal for that.
///
/// # Errors
///
/// Any unreadable directory or unparseable lock — the sweep must not proceed on a
/// partial answer, because a commit missing from this set is a live checkout deleted.
fn pinned_commits(
    root: &ConfigRoot,
) -> Result<(BTreeSet<String>, usize), Box<dyn std::error::Error>> {
    let mut pinned = BTreeSet::new();
    let mut locks = 0usize;
    let mut visit = |path: &Path| -> Result<(), Box<dyn std::error::Error>> {
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            return Ok(());
        };
        // A solved package manifest sits beside the lock it belongs to and shares the
        // `.lock` extension, but it is a line-oriented package list, not TOML — so
        // selecting by extension alone would abort every sweep on the first one.
        if !name.ends_with(".lock") || boot2deb_core::manifest::is_manifest_name(name) {
            return Ok(());
        }
        let text = std::fs::read_to_string(path)
            .map_err(|e| format!("failed to read {}: {e}", path.display()))?;
        let lock = boot2deb_core::lock::Lock::from_toml_str(&text, &path.display().to_string())?;
        pinned.extend(lock.pinned_commits().into_iter().map(str::to_owned));
        locks += 1;
        Ok(())
    };
    for search_root in root.search_paths() {
        let recipes = search_root.join("recipes");
        let entries = match std::fs::read_dir(&recipes) {
            Ok(entries) => entries,
            // A root with no recipes contributes nothing; anything else is a partial
            // read, and a partial read must not be mistaken for "nothing pinned".
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(format!("failed to read {}: {e}", recipes.display()).into()),
        };
        for entry in entries {
            let path = entry
                .map_err(|e| format!("failed to read {}: {e}", recipes.display()))?
                .path();
            if !path.is_dir() {
                visit(&path)?;
                continue;
            }
            // A device directory: `recipes/<device>/<leaf>.lock`, the shipped layout.
            for leaf in std::fs::read_dir(&path)
                .map_err(|e| format!("failed to read {}: {e}", path.display()))?
            {
                let leaf = leaf.map_err(|e| format!("failed to read {}: {e}", path.display()))?;
                visit(&leaf.path())?;
            }
        }
    }
    Ok((pinned, locks))
}

/// The dead entries of one commit-addressed checkout cache: those named for a commit
/// no lock pins, in directory order made deterministic by sorting.
///
/// Only full-sha entries are candidates. Anything else in the store — a `.fetch-*`
/// staging dir a hard-killed clone left, or a file an operator dropped there — is not
/// commit-addressed, so its liveness is not decidable by this rule and it is left
/// alone. (The fetchers sweep their own stale staging dirs.)
fn unpinned_checkouts(store: &Path, pinned: &BTreeSet<String>) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(store) else {
        return Vec::new();
    };
    let mut dead: Vec<PathBuf> = entries
        .flatten()
        .filter(|e| e.path().is_dir())
        .filter(|e| {
            e.file_name().to_str().is_some_and(|name| {
                boot2deb_core::sources::is_full_sha(name) && !pinned.contains(&name.to_lowercase())
            })
        })
        .map(|e| e.path())
        .collect();
    dead.sort();
    dead
}

/// The publication lock a provisioned rootfs at `path` carries — `<path>.lock`, beside
/// the tree rather than inside it.
///
/// Named here because [`boot2deb_engine::sandbox::reclaim_tree`] removes it with the
/// tree it belongs to, so a run that also names it as a target of its own has to know
/// it is already gone rather than never present.
fn publication_lock(path: &Path) -> Option<PathBuf> {
    let mut name = path.file_name()?.to_os_string();
    name.push(".lock");
    Some(path.with_file_name(name))
}

/// Run `clean [RECIPE]`.
pub(crate) fn run(
    root: &ConfigRoot,
    recipe: Option<&str>,
    args: CleanArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let root_scoped = args.artifacts || args.verify_trees || args.kconfig || args.all_caches;
    let work_scoped = args.cache || args.sandbox || args.build_roots;
    // With no selector at all the target is the whole work dir, which is work-scoped.
    let work_scoped = work_scoped || !root_scoped;

    let mut targets: Vec<PathBuf> = Vec::new();

    if work_scoped {
        let Some(recipe) = recipe else {
            return Err(
                "clean needs a RECIPE to say whose build scratch to remove; \
                        `--artifacts`, `--verify-trees`, `--kconfig` and `--all-caches` \
                        sweep the shared caches without one"
                    .into(),
            );
        };
        // Validate the recipe-name shape (reject `..`/absolute/separators) before it is
        // joined into a filesystem path, consistent with the config write paths.
        root.lock_path(recipe)?;
        let work_dir = work_dir_for(root, recipe, args.work_dir);
        // Every work-dir target sits inside a path the caller may have chosen, so the
        // ownership stamp gates all of them. The root-scoped stores are exempt: their
        // paths come from `--root`, not from a caller-supplied directory name.
        check_work_dir_removable(&work_dir, args.force)?;
        if !(args.cache || args.sandbox || args.build_roots) {
            targets.push(work_dir);
        } else {
            if args.cache {
                targets.push(work_dir.join("cache"));
            }
            if args.sandbox {
                targets.push(work_dir.join("sandbox"));
            }
            // Enumerated by the module that names these trees rather than reconstructed
            // here, so what is swept and what is spared cannot drift from how they are
            // named. It answers with the trees that exist, which is why this extends
            // rather than pushes.
            if args.build_roots {
                targets.extend(boot2deb_engine::sandbox::build_root_trees(&work_dir));
            }
        }
    }

    if args.all_caches {
        targets.push(cache_dir(root));
    }
    if args.artifacts {
        targets.push(artifact_cache(root));
    }
    if args.kconfig {
        targets.push(kconfig_cache(root));
    }
    if args.verify_trees {
        // Resolved before anything is removed: a lock that will not parse must abort
        // the whole run, not leave it half-swept.
        let (pinned, locks) = pinned_commits(root)?;
        // Said before any removal, because this is the one input to the liveness rule
        // the command cannot check for itself: a sweep run without the `--overlay`
        // flags a build uses reads fewer locks and calls their checkouts dead.
        println!(
            "  {} commit(s) pinned by {locks} lock(s) across {} config root(s)",
            pinned.len(),
            root.search_paths().len()
        );
        for store in [verify_trees_cache(root), patches_cache(root)] {
            targets.extend(unpinned_checkouts(&store, &pinned));
        }
    }

    let mut removed_any = false;
    // Every path an earlier removal in this run already took. `reclaim_tree` goes
    // through the provisioner's own removal, which takes the `<tree>.lock` a published
    // rootfs carries along with the tree — and `--build-roots` names that lock as a
    // target in its own right. Reporting it "absent" would read as a target that was
    // never there, and would contradict the `--dry-run` line that listed it.
    let mut taken: BTreeSet<PathBuf> = BTreeSet::new();
    for target in &targets {
        if !target.exists() {
            if taken.contains(target) {
                println!("  removed {} (with its tree)", target.display());
                removed_any = true;
            } else {
                println!("  {} (absent)", target.display());
            }
            continue;
        }
        let size = human_size(dir_size(target));
        if args.dry_run {
            println!("  would remove {} ({size})", target.display());
        } else {
            // `--build-roots` names the `.lock` and `.pkgs` files beside each tree as
            // well as the trees, and a base is only reusable with its manifest — so
            // leaving the files would leave a base that re-provisions anyway, having
            // spared nothing.
            // A hard-killed build leaves two kinds of directory the caller cannot
            // delete on its own: a provisioned rootfs whose files belong to
            // subordinate uids, and the mode-`0` work area an unprivileged overlay
            // leaves under every layer. `sweep_provisioned` reclaims the first through
            // the id-map that owns it; `reclaim_tree` then removes what is left by the
            // same route, which is what makes `clean` proof against both. The kconfig
            // scratch needs exactly this: each work dir holds a provisioned cross root.
            boot2deb_engine::rootfs::sweep_provisioned(target);
            boot2deb_engine::sandbox::reclaim_tree(target)
                .map_err(|e| format!("failed to remove {}: {e}", target.display()))?;
            if let Some(lock) = publication_lock(target) {
                taken.insert(lock);
            }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::repo_root;
    use crate::workdir::mark_work_dir;

    /// A stamped work dir holding one build root, one packaging root, the files a
    /// bootstrap leaves beside each, and an overlay upper.
    fn stamped_work_dir() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        mark_work_dir(tmp.path()).unwrap();
        let sandbox = tmp.path().join("sandbox");
        for leaf in ["build-arm64-forky-0123456789ab", "package-amd64-forky-ba98"] {
            std::fs::create_dir_all(sandbox.join(leaf)).unwrap();
            std::fs::write(sandbox.join(leaf).join("marker"), "x").unwrap();
            std::fs::write(sandbox.join(format!("{leaf}.lock")), "").unwrap();
            std::fs::write(sandbox.join(format!("{leaf}.pkgs")), "manifest").unwrap();
        }
        std::fs::create_dir_all(sandbox.join("layers/ffmpeg/upper")).unwrap();
        // What an unprivileged overlay leaves beside the upper: a work area holding a
        // directory the kernel sets to mode `0`. It is owned by the caller and still
        // undeletable, because a recursive remove cannot descend into it.
        let work = sandbox.join("layers/ffmpeg/.upper.work/work");
        std::fs::create_dir_all(&work).unwrap();
        std::fs::set_permissions(&work, std::os::unix::fs::PermissionsExt::from_mode(0o000))
            .unwrap();
        tmp
    }

    /// A work-dir-scoped invocation: every root-scoped selector off, so the only
    /// thing under test is what happens inside `work_dir`.
    fn args(work_dir: &std::path::Path, build_roots: bool, sandbox: bool) -> CleanArgs {
        CleanArgs {
            work_dir: Some(work_dir.to_path_buf()),
            cache: false,
            sandbox,
            build_roots,
            artifacts: false,
            verify_trees: false,
            kconfig: false,
            all_caches: false,
            dry_run: false,
            force: false,
        }
    }

    /// A root-scoped invocation: no work dir, no recipe, one cache selector.
    fn cache_args(verify_trees: bool, kconfig: bool, all_caches: bool) -> CleanArgs {
        CleanArgs {
            work_dir: None,
            cache: false,
            sandbox: false,
            build_roots: false,
            artifacts: false,
            verify_trees,
            kconfig,
            all_caches,
            dry_run: false,
            force: false,
        }
    }

    #[test]
    fn build_roots_removes_the_build_roots_and_layers_and_spares_the_packaging_root() {
        let tmp = stamped_work_dir();
        let sandbox = tmp.path().join("sandbox");
        run(
            &repo_root(),
            Some("turing-rk1/forky"),
            args(tmp.path(), true, false),
        )
        .unwrap();

        assert!(!sandbox.join("build-arm64-forky-0123456789ab").exists());
        // The files beside the tree go with it: a base kept without its manifest is
        // not reusable, so leaving them would spare nothing and confuse the next read.
        assert!(!sandbox.join("build-arm64-forky-0123456789ab.lock").exists());
        assert!(!sandbox.join("build-arm64-forky-0123456789ab.pkgs").exists());
        assert!(!sandbox.join("layers").exists());
        // The whole point of the selector: the packaging root is never layered, so it
        // cannot skew, and re-bootstrapping it is the cost `--sandbox` charges.
        assert!(sandbox.join("package-amd64-forky-ba98/marker").exists());
        assert!(sandbox.join("package-amd64-forky-ba98.pkgs").exists());
    }

    #[test]
    fn sandbox_removes_the_packaging_root_too() {
        // The contrast that makes the narrow selector worth having, asserted rather
        // than assumed — `--sandbox` is the coarse answer to the same skew.
        let tmp = stamped_work_dir();
        run(
            &repo_root(),
            Some("turing-rk1/forky"),
            args(tmp.path(), false, true),
        )
        .unwrap();
        assert!(!tmp.path().join("sandbox").exists());
    }

    #[test]
    fn clean_removes_the_mode_zero_overlay_work_area() {
        // The regression: `remove_dir_all` runs as the caller and stops at the mode-`0`
        // work directory, so `clean` failed with `Permission denied` on exactly the
        // builds that layer — which is every image build.
        let tmp = stamped_work_dir();
        let work_dir = tmp.path().to_path_buf();
        run(
            &repo_root(),
            Some("turing-rk1/forky"),
            args(&work_dir, false, false),
        )
        .unwrap();
        assert!(!work_dir.exists(), "the whole work dir goes");
    }

    /// A config root holding one recipe lock that pins `pinned`, plus a
    /// commit-addressed checkout store holding both a pinned and an unpinned tree.
    fn config_root_with_checkouts(pinned: &str, dead: &str) -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let recipes = tmp.path().join("recipes/turing-rk1");
        std::fs::create_dir_all(&recipes).unwrap();
        std::fs::write(
            recipes.join("forky.lock"),
            format!(
                "[kernel]\n\
                 id = \"k\"\n\
                 source = \"https://k.example/k.git\"\n\
                 ref = \"v1\"\n\
                 commit = \"{pinned}\"\n"
            ),
        )
        .unwrap();
        for commit in [pinned, dead] {
            let tree = tmp.path().join("cache/verify-trees").join(commit);
            std::fs::create_dir_all(&tree).unwrap();
            std::fs::write(tree.join("Makefile"), "x").unwrap();
        }
        tmp
    }

    #[test]
    fn verify_trees_prunes_the_unpinned_and_keeps_the_pinned() {
        let pinned = "a".repeat(40);
        let dead = "b".repeat(40);
        let tmp = config_root_with_checkouts(&pinned, &dead);
        let trees = tmp.path().join("cache/verify-trees");
        // Not commit-addressed, so its liveness is not decidable by this rule: a
        // staging dir a hard-killed clone left, and a file an operator dropped in.
        std::fs::create_dir_all(trees.join(".fetch-abc123/repo")).unwrap();
        std::fs::write(trees.join("NOTES"), "keep me").unwrap();
        // The sibling patches cache is swept by the same rule, off the same pinned set.
        let patches = tmp.path().join("cache/patches");
        std::fs::create_dir_all(patches.join(&dead)).unwrap();

        run(
            &ConfigRoot::new(tmp.path()),
            None,
            cache_args(true, false, false),
        )
        .unwrap();

        assert!(trees.join(&pinned).exists(), "a pinned checkout is live");
        assert!(!trees.join(&dead).exists(), "an unpinned checkout is dead");
        assert!(!patches.join(&dead).exists(), "and so is its patches twin");
        assert!(trees.join(".fetch-abc123").exists());
        assert!(trees.join("NOTES").exists());
    }

    #[test]
    fn verify_trees_counts_a_lock_in_any_root_and_at_either_depth() {
        // Liveness is a property of the whole config tree, so an overlay's lock keeps a
        // checkout just as a shipped one does — and a stray top-level `recipes/*.lock`
        // counts too, since the sweep reads lock *files* rather than the recipe
        // inventory (a lock whose `.toml` was deleted still pins its checkouts).
        let overlay_pin = "c".repeat(40);
        let toplevel_pin = "d".repeat(40);
        let tmp = config_root_with_checkouts(&"a".repeat(40), &overlay_pin);
        let overlay = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(overlay.path().join("recipes/h96-max-m9")).unwrap();
        std::fs::write(
            overlay.path().join("recipes/h96-max-m9/forky.lock"),
            format!("[uboot]\nsource = \"u://s\"\nref = \"v2\"\ncommit = \"{overlay_pin}\"\n"),
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("recipes/stray.lock"),
            format!("[uboot]\nsource = \"u://s\"\nref = \"v2\"\ncommit = \"{toplevel_pin}\"\n"),
        )
        .unwrap();
        let trees = tmp.path().join("cache/verify-trees");
        std::fs::create_dir_all(trees.join(&toplevel_pin)).unwrap();

        let root =
            ConfigRoot::with_overlays(tmp.path().to_path_buf(), vec![overlay.path().to_path_buf()])
                .unwrap();
        run(&root, None, cache_args(true, false, false)).unwrap();

        assert!(trees.join(&overlay_pin).exists(), "pinned by the overlay");
        assert!(trees.join(&toplevel_pin).exists(), "pinned by a stray lock");
    }

    #[test]
    fn verify_trees_reads_past_the_solved_package_manifests() {
        // A `<recipe>.pkgs.lock` sits beside the lock it belongs to and shares the
        // extension, but holds `name version arch sha256` lines rather than TOML. Read
        // as a lock it is a parse error, which under the abort rule would make the
        // sweep a no-op on every real config tree.
        let pinned = "a".repeat(40);
        let dead = "b".repeat(40);
        let tmp = config_root_with_checkouts(&pinned, &dead);
        std::fs::write(
            tmp.path().join("recipes/turing-rk1/forky.pkgs.lock"),
            "# solved package manifest\nadduser 3.157 all ".to_string() + &"3".repeat(64) + "\n",
        )
        .unwrap();

        run(
            &ConfigRoot::new(tmp.path()),
            None,
            cache_args(true, false, false),
        )
        .unwrap();

        assert!(!tmp.path().join("cache/verify-trees").join(&dead).exists());
        assert!(tmp.path().join("cache/verify-trees").join(&pinned).exists());
    }

    #[test]
    fn verify_trees_aborts_rather_than_sweep_on_an_unreadable_lock() {
        // The safety property the whole selector rests on: a pinned set that is
        // *narrower* than the truth deletes a live checkout. A lock that will not parse
        // means the set cannot be known, so nothing is removed — not even the entries
        // the readable locks already proved dead.
        let dead = "b".repeat(40);
        let tmp = config_root_with_checkouts(&"a".repeat(40), &dead);
        std::fs::write(
            tmp.path().join("recipes/turing-rk1/trixie.lock"),
            "[kernel]\nthis is not a lock\n",
        )
        .unwrap();

        let err = run(
            &ConfigRoot::new(tmp.path()),
            None,
            cache_args(true, false, false),
        )
        .unwrap_err();

        assert!(err.to_string().contains("trixie.lock"), "{err}");
        assert!(
            tmp.path().join("cache/verify-trees").join(&dead).exists(),
            "nothing is removed when the pinned set is unknown"
        );
    }

    #[test]
    fn kconfig_removes_the_scratch_tree_and_needs_no_recipe() {
        // The shape the by-hand sweep lacked: one command for every slug, with no
        // `--work-dir` and no ownership stamp, because the path comes from `--root`.
        let tmp = tempfile::tempdir().unwrap();
        let scratch = tmp.path().join("cache/kconfig/turing-rk1-forky");
        std::fs::create_dir_all(scratch.join("build")).unwrap();
        let artifacts = tmp.path().join("cache/artifacts/node");
        std::fs::create_dir_all(&artifacts).unwrap();

        run(
            &ConfigRoot::new(tmp.path()),
            None,
            cache_args(false, true, false),
        )
        .unwrap();

        assert!(!tmp.path().join("cache/kconfig").exists());
        assert!(artifacts.exists(), "a sibling store is not collateral");
    }

    #[test]
    fn all_caches_takes_the_pinned_checkouts_and_every_sibling_store() {
        // The contrast that makes `--verify-trees` worth having: liveness stops
        // mattering, and the whole tree goes.
        let pinned = "a".repeat(40);
        let tmp = config_root_with_checkouts(&pinned, &"b".repeat(40));
        std::fs::create_dir_all(tmp.path().join("cache/artifacts/node")).unwrap();
        std::fs::create_dir_all(tmp.path().join("cache/extra-debs")).unwrap();

        run(
            &ConfigRoot::new(tmp.path()),
            None,
            cache_args(false, false, true),
        )
        .unwrap();

        assert!(!tmp.path().join("cache").exists());
        assert!(
            tmp.path().join("recipes/turing-rk1/forky.lock").exists(),
            "the config tree is not a cache"
        );
    }

    #[test]
    fn a_work_dir_scoped_clean_without_a_recipe_is_refused() {
        // The whole-tree default and the work-dir selectors have nothing to address
        // without a recipe, so this is a usage error and not an empty sweep.
        let tmp = tempfile::tempdir().unwrap();
        let root = ConfigRoot::new(tmp.path());
        for args in [cache_args(false, false, false), {
            let mut a = cache_args(false, false, false);
            a.sandbox = true;
            a
        }] {
            let err = run(&root, None, args).unwrap_err();
            assert!(err.to_string().contains("needs a RECIPE"), "{err}");
        }
    }

    #[test]
    fn a_trees_publication_lock_is_named_beside_it_not_inside_it() {
        // What `reclaim_tree` takes along with a tree, and therefore what the removal
        // report has to recognize as already gone rather than never present.
        assert_eq!(
            publication_lock(Path::new("/w/sandbox/build-arm64-forky-abc")),
            Some(PathBuf::from("/w/sandbox/build-arm64-forky-abc.lock"))
        );
        // A path with no final component names no tree, so it carries no lock.
        assert_eq!(publication_lock(Path::new("/")), None);
    }

    #[test]
    fn build_roots_refuses_a_work_dir_it_did_not_stamp() {
        // The guard covers every selector that removes within a caller-named path,
        // and this one names paths it discovered by reading that directory.
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("sandbox")).unwrap();
        let err = run(
            &repo_root(),
            Some("turing-rk1/forky"),
            args(tmp.path(), true, false),
        )
        .unwrap_err();
        assert!(err.to_string().contains("not stamped"), "{err}");
    }
}
