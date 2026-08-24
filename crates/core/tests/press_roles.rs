//! Resolve every shipped recipe — and every device under both layouts — and hold
//! [`boot2deb_core::press::roles`] to a well-formed answer for each.
//!
//! An integration test because the property is totality over the *shipped*
//! configuration: the mapping in `press.rs` is small and its unit tests pin the
//! shapes, but only resolving the real config tree proves no build point reaches
//! `press` with the wrong artifact set — the failure that would otherwise live in
//! board-page prose ("split is two files") and must be an error.

use boot2deb_core::model::{Layout, Overrides};
use boot2deb_core::press::{roles, ArtifactRole};
use boot2deb_core::{resolve_recipe, ConfigRoot};
use std::path::PathBuf;

/// The config tree, two directories above this crate.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate sits two levels under the config root")
        .to_path_buf()
}

/// Every shipped recipe resolves to an artifact set matching its layout and
/// deliverable, with the rootfs-bearing artifact — the one seed keys and tree
/// additions apply to — exactly where each layout puts it.
#[test]
fn every_shipped_recipe_has_a_well_formed_artifact_set() {
    let root = ConfigRoot::new(repo_root());
    let recipes = root.list_recipes().expect("the shipped recipes load");
    assert!(!recipes.is_empty(), "the tree ships recipes");

    for recipe in &recipes {
        let build = resolve_recipe(&root, recipe, &Overrides::default())
            .unwrap_or_else(|e| panic!("{recipe}: shipped recipe must resolve: {e}"));
        let r = roles(&build);

        if !build.produces_image() {
            assert_eq!(
                r,
                vec![ArtifactRole::Boot],
                "{recipe}: a u-boot deliverable is one boot image"
            );
            assert!(
                !r[0].carries_rootfs(),
                "{recipe}: a boot image takes no seed keys and no additions"
            );
            continue;
        }
        match build.layout {
            Layout::Combined => {
                assert_eq!(
                    r,
                    vec![ArtifactRole::Combined],
                    "{recipe}: combined is one image"
                );
                assert!(r[0].carries_rootfs(), "{recipe}");
            }
            Layout::Split => {
                assert_eq!(
                    r,
                    vec![ArtifactRole::Boot, ArtifactRole::Rootfs],
                    "{recipe}: split is boot then rootfs"
                );
            }
        }
    }
}

/// The layout override changes the roles the way it changes the build: every
/// device that can split resolves to the two-artifact set under `--layout split`,
/// and the depthcharge boards — which resolution refuses to split — never reach
/// `roles` with a split build at all.
#[test]
fn split_layout_maps_to_two_artifacts_wherever_it_resolves() {
    let root = ConfigRoot::new(repo_root());
    let recipes = root.list_recipes().expect("the shipped recipes load");

    let mut split_seen = 0;
    for recipe in &recipes {
        let overrides = Overrides {
            layout: Some(Layout::Split),
            ..Overrides::default()
        };
        // A board that cannot split refuses at resolution — which is itself the
        // property the totality rests on: `roles` never sees the combination.
        let Ok(build) = resolve_recipe(&root, recipe, &overrides) else {
            continue;
        };
        if !build.produces_image() {
            continue;
        }
        split_seen += 1;
        let r = roles(&build);
        assert_eq!(r.len(), 2, "{recipe}");
        // Exactly one of the two carries the rootfs, and it is the second: seed
        // keys and additions land on the disk the OS reads, never the bootloader
        // medium.
        assert!(!r[0].carries_rootfs(), "{recipe}");
        assert!(r[1].carries_rootfs(), "{recipe}");
    }
    assert!(
        split_seen > 0,
        "at least one shipped recipe must resolve under --layout split"
    );
}
