//! Which build points a change to the tree reaches — the pure half of `affected`.
//!
//! A build reads its inputs from the config tree through [`ConfigRoot`] lookups. Those
//! lookups can be recorded ([`ConfigRoot::record_probes`]). The record is each build
//! point's set of inputs: every file it read and every directory it walked, by path.
//! A change reaches a point when a changed path is one of those inputs, or lies under
//! one of the directories.
//!
//! A change is a path whose content differs between two trees. For TOML that is too
//! coarse: a comment or a re-wrapped line is a different file and the same config. So
//! two versions of a `.toml` or `.lock` file are compared as parsed documents
//! ([`toml_equivalent`]). A recipe's own `[support]` table is left out of that
//! comparison, because it is the claim *about* the build rather than an input *to* it.
//! Editing it does not change what the recipe builds.
//!
//! Pure: the caller supplies the recorded inputs, the changed paths and the file
//! texts. Asking git what changed is the engine's job.
//!
//! [`ConfigRoot`]: crate::ConfigRoot
//! [`ConfigRoot::record_probes`]: crate::ConfigRoot::record_probes

use std::collections::BTreeSet;

/// Whether `changed` is one of `inputs`, or lies under one of them. Both are paths
/// relative to the same directory, `/`-separated.
pub fn reaches(inputs: &BTreeSet<String>, changed: &str) -> bool {
    inputs.contains(changed)
        || changed
            .match_indices('/')
            .any(|(i, _)| inputs.contains(&changed[..i]))
}

/// The changed paths that reach a build point with these `inputs`, in `changed`'s
/// order.
pub fn touched<'a>(inputs: &BTreeSet<String>, changed: &'a [String]) -> Vec<&'a str> {
    changed
        .iter()
        .map(String::as_str)
        .filter(|c| reaches(inputs, c))
        .collect()
}

/// Whether `path` is `root` or lies under it, for a list of top-level paths such as the
/// builder's own sources.
pub fn under_any(path: &str, roots: &[&str]) -> bool {
    roots.iter().any(|r| {
        path == *r
            || path
                .strip_prefix(r)
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// Whether two versions of the TOML file at `path` say the same thing to a build.
///
/// Both are parsed and compared as documents, so formatting and comments are not a
/// difference. Under `recipes/`, a `.toml` file's top-level `support` table is removed
/// from both first, since the claim is not an input. A version that does not parse is
/// never equivalent to anything: the comparison cannot vouch for it.
pub fn toml_equivalent(path: &str, old: &str, new: &str) -> bool {
    let parse = |text: &str| -> Option<toml::Table> {
        let mut table: toml::Table = toml::from_str(text).ok()?;
        if path.starts_with("recipes/") && path.ends_with(".toml") {
            table.remove("support");
        }
        Some(table)
    };
    match (parse(old), parse(new)) {
        (Some(a), Some(b)) => a == b,
        _ => false,
    }
}

/// Whether `path` names a file [`toml_equivalent`] compares: a layer, recipe or lock.
/// A solved package manifest shares the `.lock` extension and is not TOML, so the
/// caller compares it as text.
pub fn is_toml_input(path: &str) -> bool {
    path.ends_with(".toml") || (path.ends_with(".lock") && !crate::manifest::is_manifest_name(path))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inputs(paths: &[&str]) -> BTreeSet<String> {
        paths.iter().map(|p| p.to_string()).collect()
    }

    #[test]
    fn a_change_reaches_a_point_through_a_file_or_a_directory_it_read() {
        let read = inputs(&[
            "devices/turing-rk1.toml",
            "devices/turing-rk1/overlay",
            "blobs/rk3588",
        ]);
        assert!(reaches(&read, "devices/turing-rk1.toml"));
        assert!(reaches(&read, "devices/turing-rk1/overlay/etc/motd"));
        assert!(reaches(&read, "blobs/rk3588/rk3588_bl31_v1.51.elf"));
        // A sibling that merely shares a prefix is not under the directory.
        assert!(!reaches(&read, "devices/turing-rk1-lite.toml"));
        assert!(!reaches(&read, "blobs/rk3588x/a"));
        assert!(!reaches(&read, "docs/src/boards/turing-rk1.md"));
        let changed = vec![
            "docs/src/introduction.md".to_string(),
            "devices/turing-rk1/overlay/etc/motd".to_string(),
        ];
        assert_eq!(
            touched(&read, &changed),
            ["devices/turing-rk1/overlay/etc/motd"]
        );
    }

    #[test]
    fn the_builder_paths_match_whole_components() {
        let roots = ["crates", "Cargo.toml", "Cargo.lock"];
        assert!(under_any("crates/engine/src/lib.rs", &roots));
        assert!(under_any("Cargo.lock", &roots));
        assert!(!under_any("crates-notes.md", &roots));
        assert!(!under_any("recipes/x/Cargo.toml", &roots));
    }

    #[test]
    fn toml_is_compared_as_a_document_and_a_recipes_claim_is_not_an_input() {
        let old = "# a comment\ndevice = \"turing-rk1\"\nfeatures = [\"a\", \"b\"]\n";
        let reflowed = "device   = \"turing-rk1\"\nfeatures = [\n  \"a\",\n  \"b\",\n]\n";
        assert!(toml_equivalent("devices/x.toml", old, reflowed));
        assert!(!toml_equivalent(
            "devices/x.toml",
            old,
            "device = \"turing-rk1\"\nfeatures = [\"a\"]\n"
        ));

        let claimed = "device = \"d\"\n[support]\nstatus = \"expected\"\ndate = \"2026-07-20\"\n";
        let validated =
            "device = \"d\"\n[support]\nstatus = \"validated\"\ndate = \"2026-10-08\"\n\
                         commit = \"7d1c6f5ec599\"\n";
        assert!(toml_equivalent("recipes/d/forky.toml", claimed, validated));
        // Only a recipe's: a `support` key in any other layer is config like the rest.
        assert!(!toml_equivalent("devices/d.toml", claimed, validated));
        // A version that does not parse cannot be vouched for.
        assert!(!toml_equivalent("devices/x.toml", old, "device = ["));
    }

    #[test]
    fn a_solved_manifest_is_not_compared_as_toml() {
        assert!(is_toml_input("recipes/turing-rk1/forky.lock"));
        assert!(is_toml_input("devices/turing-rk1.toml"));
        assert!(!is_toml_input("recipes/turing-rk1/forky.pkgs.lock"));
        assert!(!is_toml_input("fragments/soc/rk3588.config"));
    }
}
