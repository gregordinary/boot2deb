//! `diff`: what moved between two build points.
//!
//! Reads the documents a build already leaves behind — a recipe's `.lock`, a
//! published `.provenance.toml`, the solved package manifest each names, and the
//! kernel fragments the config tree holds — normalizes each side into a
//! [`boot2deb_core::diff::Side`], and renders the comparison. All the deciding
//! happens in [`boot2deb_core::diff`]; this module reads files and prints.
//!
//! The one section that reaches outside those documents is the per-patch file delta,
//! which needs the `patches` repo to resolve a moved commit into named files. It
//! degrades to a note rather than failing, since a comparison missing one section is
//! worth more than no comparison.

use crate::config::{default_patches_checkout, fragment_paths};
use crate::render::{print_columns, short};
use boot2deb_core::diff::{
    compare, ArchiveChange, BuilderChanges, Change, KernelChanges, PackageChanges, PatchAxisChange,
    Report, Section, Side, SourceChange,
};
use boot2deb_core::kconfig::FragmentSet;
use boot2deb_core::model::Overrides;
use boot2deb_core::provenance::ProvenanceManifest;
use boot2deb_core::{resolve_recipe, ConfigRoot};
use boot2deb_engine::patchdelta::{series_delta, SeriesDelta};
use clap::ValueEnum;
use std::path::Path;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Which sections `--section` can select.
///
/// The command's default is all of them; naming one narrows the report to it, for
/// the reader who already knows which question they are asking.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum SectionArg {
    /// The solved package set: added, removed, re-versioned, rebuilt.
    Packages,
    /// The kernel pin and the requested kernel configuration.
    Kernel,
    /// Patch series membership, their pins, and the patch files behind a moved pin.
    Patches,
    /// Every other pinned source tree.
    Sources,
    /// rkbin blob pins.
    Blobs,
    /// What built each side, and the archive state it resolved against.
    Builder,
}

/// Run `diff <a> <b>`.
///
/// Each side is a recipe name, a path to a `.lock`, or a path to a
/// `.provenance.toml`. A recipe name resolves to both its lock *and* its resolved
/// kernel fragments, which is why it answers the kconfig delta and a bare lock path
/// does not.
pub(crate) fn run(
    root: &ConfigRoot,
    left: &str,
    right: &str,
    sections: &[SectionArg],
    patches_path: Option<&Path>,
    json_out: bool,
) -> Result<()> {
    let left = read_side(root, left)?;
    let right = read_side(root, right)?;
    let report = compare(&left, &right);
    let wanted = |s: SectionArg| sections.is_empty() || sections.contains(&s);

    // The per-patch file delta, for every axis whose pin moved. Computed here rather
    // than in the pure comparison because it reads a git repository.
    let deltas = if wanted(SectionArg::Patches) {
        patch_file_deltas(root, patches_path, &report, &left, &right)
    } else {
        Vec::new()
    };

    if json_out {
        let mut doc = serde_json::to_value(&report)?;
        if wanted(SectionArg::Patches) {
            doc["patch_files"] =
                serde_json::to_value(deltas.iter().map(delta_json).collect::<Vec<_>>())?;
        }
        println!("{}", serde_json::to_string_pretty(&doc)?);
        return Ok(());
    }

    println!("{}  ->  {}\n", report.left, report.right);
    let mut quiet = true;
    if wanted(SectionArg::Packages) {
        quiet &= section("packages", &report.packages, print_packages);
    }
    if wanted(SectionArg::Kernel) {
        quiet &= section("kernel", &report.kernel, print_kernel);
    }
    if wanted(SectionArg::Patches) {
        quiet &= section("patches", &report.patches, |changes| {
            print_patches(changes, &deltas)
        });
    }
    if wanted(SectionArg::Sources) {
        quiet &= section("sources", &report.sources, |c| print_sources(c));
    }
    if wanted(SectionArg::Blobs) {
        quiet &= section("blobs", &report.blobs, |c| print_blobs(c));
    }
    if wanted(SectionArg::Builder) {
        quiet &= section("builder", &report.builder, print_builder);
    }
    // Over the sections that were *asked for*, not the whole report: under
    // `--section kernel` the claim has to be about the kernel alone, or it would
    // vouch for sections this run never printed.
    if quiet {
        println!("the two build points state the same thing in every section compared");
    }
    Ok(())
}

/// Read one side of the comparison.
///
/// A path is taken as a document and read by its shape; anything else is a recipe
/// name. Naming a recipe is the fuller of the two: it yields the lock, the solved
/// manifest committed beside it, *and* the resolved kernel fragments, which no
/// single document on disk carries.
fn read_side(root: &ConfigRoot, spec: &str) -> Result<Side> {
    let path = Path::new(spec);
    if path.is_file() {
        read_document(path)
    } else {
        read_recipe(root, spec)
    }
}

/// Read a side from a `.lock` or a `.provenance.toml` on disk.
///
/// The solved manifest is looked for beside the named file, since that is where both
/// documents' `manifest` key points: a lock's manifest is committed beside it in
/// `recipes/`, and a published manifest sits beside the provenance in the artifact
/// directory. A manifest that is not there leaves the packages section unavailable
/// rather than failing the whole comparison.
///
/// A document names no fragments, so a side read this way never answers the kconfig
/// delta. That is what the recipe form is for: a fragment set is a property of the
/// config tree, not of anything a build writes out.
fn read_document(path: &Path) -> Result<Side> {
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    let label = path.display().to_string();
    let dir = path.parent().unwrap_or(Path::new("."));
    if path
        .file_name()
        .is_some_and(|n| n.to_string_lossy().ends_with(".provenance.toml"))
    {
        let prov = ProvenanceManifest::from_toml_str(&text, &label)?;
        let mut side = Side::from_provenance(&label, &prov);
        side.packages = read_manifest(&dir.join(&prov.rootfs.manifest));
        return Ok(side);
    }
    let lock = boot2deb_core::lock::Lock::from_toml_str(&text, &label)?;
    let mut side = Side::from_lock(&label, &lock);
    side.packages = lock
        .rootfs
        .as_ref()
        .and_then(|r| read_manifest(&dir.join(&r.manifest)));
    Ok(side)
}

/// Read a side from a recipe name: its lock, the manifest committed beside it, and
/// the fragments its resolved kernel merges.
fn read_recipe(root: &ConfigRoot, recipe: &str) -> Result<Side> {
    let lock = root.lock(recipe)?;
    let build = resolve_recipe(root, recipe, &Overrides::default())?;
    // The resolve supplies the kernel's identity, which a lock cannot state for a
    // distro-package kernel: it pins no commit, so it writes no `[kernel]` table, and
    // two boards' kernels would otherwise compare as absent on both sides.
    let mut side = Side::from_lock(recipe, &lock).merge(Side::from_resolved(recipe, &build));
    if let Some(rootfs) = &lock.rootfs {
        side.packages = read_manifest(&root.recipe_sibling(recipe, &rootfs.manifest)?);
    }
    side.kconfig = read_fragments(root, &build)?;
    Ok(side)
}

/// Merge a resolved build's kernel fragments, naming each by its config-root-relative
/// path so the delta can attribute a symbol to a file a reader can open.
///
/// `None` for a distro-package kernel, which merges no fragments because Debian owns
/// its configuration. That is an absence and not an empty set: comparing it as empty
/// would report every symbol the other side's fragments name as newly enabled, which
/// says nothing about how the two kernels differ.
fn read_fragments(
    root: &ConfigRoot,
    build: &boot2deb_core::model::ResolvedBuild,
) -> Result<Option<FragmentSet>> {
    if build
        .image
        .as_ref()
        .and_then(|i| i.kernel.compiled())
        .is_none()
    {
        return Ok(None);
    }
    let paths = fragment_paths(root, build)?;
    let mut texts = Vec::new();
    for path in paths {
        let text = std::fs::read_to_string(&path)
            .map_err(|e| format!("read fragment {}: {e}", path.display()))?;
        let name = path
            .strip_prefix(root.path())
            .unwrap_or(&path)
            .display()
            .to_string();
        texts.push((name, text));
    }
    Ok(Some(FragmentSet::merge(
        texts.iter().map(|(n, t)| (n.as_str(), t.as_str())),
    )))
}

/// Read a solved package manifest, or `None` when it is not there.
///
/// A missing manifest is an absence, not an error: an old artifact directory may
/// have been swept, and every other section still compares. A manifest that *is*
/// there and does not parse is also `None` — the file is a content pin, so a
/// partially-read one would understate the set and silently misreport the diff.
fn read_manifest(path: &Path) -> Option<Vec<boot2deb_core::manifest::Package>> {
    let text = std::fs::read_to_string(path).ok()?;
    boot2deb_core::manifest::parse(&text, &path.display().to_string()).ok()
}

/// The per-patch file delta for every axis whose patches-repo commit moved, keyed by
/// axis.
///
/// Skipped entirely where no axis's commit moved, so a comparison of two build points
/// on the same patches commit never touches the repository.
///
/// The series to describe come from the two sides rather than from the report: a
/// commit can move under an *unchanged* series list, which is the ordinary case — a
/// re-pin onto a newer `patches` HEAD — and it is exactly the case where naming the
/// files that moved is worth the most.
fn patch_file_deltas(
    root: &ConfigRoot,
    patches_path: Option<&Path>,
    report: &Report,
    left: &Side,
    right: &Side,
) -> Vec<(String, SeriesDelta)> {
    let Section::Compared { changes } = &report.patches else {
        return Vec::new();
    };
    let repo = patches_path
        .map(Path::to_path_buf)
        .unwrap_or_else(|| default_patches_checkout(root));
    changes
        .iter()
        .filter_map(|axis| {
            let commit = axis.commit.as_ref()?;
            let (from, to) = (commit.from.as_deref()?, commit.to.as_deref()?);
            // Every series either side applies on this axis: one that appeared has a
            // file list worth showing, one that vanished had one, and one that stayed
            // is where a moved commit hides.
            let mut series: Vec<&String> = [left, right]
                .iter()
                .flat_map(|side| &side.patches)
                .filter(|p| p.axis == axis.axis)
                .flat_map(|p| &p.series)
                .collect();
            series.sort();
            series.dedup();
            Some(
                series
                    .into_iter()
                    .map(|s| (axis.axis.clone(), series_delta(&repo, from, to, s)))
                    .collect::<Vec<_>>(),
            )
        })
        .flatten()
        .collect()
}

/// Print one section under its heading, or the reason it could not be compared.
/// Returns whether it had nothing to report.
///
/// A section that compared clean prints its heading and `unchanged`, so a reader can
/// see it was asked — the difference between an answer and a silence.
fn section<T: boot2deb_core::diff::IsEmpty>(
    name: &str,
    section: &Section<T>,
    print: impl FnOnce(&T),
) -> bool {
    match section {
        Section::Unavailable { why } => println!("{name}: not compared — {why}\n"),
        Section::Compared { changes } if changes.is_empty() => println!("{name}: unchanged\n"),
        Section::Compared { changes } => {
            println!("{name}:");
            print(changes);
            println!();
        }
    }
    section.is_quiet()
}

/// A [`Change`]'s two sides, with `-` for a side that does not state the value.
fn arrow(change: &Change) -> String {
    let side = |v: &Option<String>| v.as_deref().unwrap_or("-").to_string();
    format!("{} -> {}", side(&change.from), side(&change.to))
}

/// A [`Change`] over commit ids, shortened for display.
fn commit_arrow(change: &Change) -> String {
    let side = |v: &Option<String>| v.as_deref().map(short).unwrap_or("-").to_string();
    format!("{} -> {}", side(&change.from), side(&change.to))
}

fn print_packages(c: &PackageChanges) {
    let mut rows = Vec::new();
    for p in &c.changed {
        rows.push(vec![
            "changed".into(),
            p.name.clone(),
            format!("{} -> {}", p.from, p.to),
        ]);
    }
    for p in &c.added {
        rows.push(vec![
            "added".into(),
            p.name.clone(),
            format!("{} [{}]", p.version, p.architecture),
        ]);
    }
    for p in &c.removed {
        rows.push(vec![
            "removed".into(),
            p.name.clone(),
            format!("{} [{}]", p.version, p.architecture),
        ]);
    }
    for p in &c.rebuilt {
        rows.push(vec![
            "rebuilt".into(),
            p.name.clone(),
            format!("{} (same version, different .deb)", p.version),
        ]);
    }
    print_columns(&rows);
    println!(
        "  {} changed, {} added, {} removed, {} rebuilt",
        c.changed.len(),
        c.added.len(),
        c.removed.len(),
        c.rebuilt.len()
    );
}

fn print_kernel(c: &KernelChanges) {
    let mut rows = Vec::new();
    for (label, change, is_commit) in [
        ("id", &c.id, false),
        ("flavor", &c.flavor, false),
        ("source", &c.source, false),
        ("ref", &c.reference, false),
        ("commit", &c.commit, true),
        ("package", &c.package, false),
    ] {
        if let Some(change) = change {
            rows.push(vec![
                label.to_string(),
                if is_commit {
                    commit_arrow(change)
                } else {
                    arrow(change)
                },
            ]);
        }
    }
    print_columns(&rows);
    match &c.kconfig {
        Section::Unavailable { why } => println!("  kconfig: not compared — {why}"),
        Section::Compared { changes } if changes.is_empty() => {
            println!("  kconfig: unchanged")
        }
        Section::Compared { changes } => {
            // The count first: two kernel definitions with different fragment sets
            // differ in thousands of symbols, and a reader wants the scale before
            // the list.
            println!("  kconfig: {} symbols differ", changes.len());
            let rows: Vec<Vec<String>> = changes
                .iter()
                .map(|s| {
                    vec![
                        format!("  {}", s.symbol),
                        format!("{} -> {}", s.from, s.to),
                        // The fragment behind the value on the side that has one —
                        // the whole reason to compare fragment sets rather than the
                        // `.config` files they generate.
                        s.to_fragment
                            .as_deref()
                            .or(s.from_fragment.as_deref())
                            .unwrap_or("-")
                            .to_string(),
                    ]
                })
                .collect();
            print_columns(&rows);
        }
    }
}

fn print_patches(changes: &[PatchAxisChange], deltas: &[(String, SeriesDelta)]) {
    for axis in changes {
        println!("  {}:", axis.axis);
        let mut rows = Vec::new();
        if !axis.series_added.is_empty() {
            rows.push(vec!["    series +".into(), axis.series_added.join(", ")]);
        }
        if !axis.series_removed.is_empty() {
            rows.push(vec!["    series -".into(), axis.series_removed.join(", ")]);
        }
        if let Some(r) = &axis.reference {
            rows.push(vec!["    ref".into(), arrow(r)]);
        }
        if let Some(c) = &axis.commit {
            rows.push(vec!["    commit".into(), commit_arrow(c)]);
        }
        print_columns(&rows);
        for (_, delta) in deltas.iter().filter(|(a, _)| *a == axis.axis) {
            print_series_delta(delta);
        }
    }
}

/// The named patch files behind a moved commit, or the reason they could not be
/// resolved.
fn print_series_delta(delta: &SeriesDelta) {
    match delta {
        SeriesDelta::Unavailable { series, why } => {
            println!("    {series}: patch files not compared — {why}")
        }
        SeriesDelta::Compared {
            series,
            added,
            removed,
            modified,
        } => {
            if delta.is_quiet() {
                println!("    {series}: the pin moved, but no patch file in it did");
                return;
            }
            println!("    {series}:");
            let mut rows = Vec::new();
            for (mark, files) in [("+", added), ("-", removed), ("~", modified)] {
                for f in files {
                    rows.push(vec![format!("      {mark}"), f.clone()]);
                }
            }
            print_columns(&rows);
        }
    }
}

fn print_sources(changes: &[SourceChange]) {
    // Headed, unlike the sections whose first column is a field name: a source row is
    // three arrows, and which of them is the ref and which the commit is not evident
    // from an axis that pins a bare sha as its ref.
    let mut rows = vec![vec![
        "axis".to_string(),
        "ref".to_string(),
        "commit".to_string(),
    ]];
    rows.extend(changes.iter().map(|c| {
        vec![
            c.axis.clone(),
            c.reference.as_ref().map(arrow).unwrap_or_default(),
            c.commit.as_ref().map(commit_arrow).unwrap_or_default(),
        ]
    }));
    print_columns(&rows);
}

fn print_blobs(changes: &[Change]) {
    let rows: Vec<Vec<String>> = changes.iter().map(|c| vec![arrow(c)]).collect();
    print_columns(&rows);
}

fn print_builder(c: &BuilderChanges) {
    let mut rows = Vec::new();
    for (label, change) in [
        ("boot2deb", &c.version),
        ("commit", &c.commit),
        ("dirty", &c.dirty),
        ("config commit", &c.config_commit),
        ("config dirty", &c.config_dirty),
        ("host arch", &c.host_arch),
        ("target arch", &c.target_arch),
        ("CROSS_COMPILE", &c.cross_compile),
    ] {
        if let Some(change) = change {
            rows.push(vec![label.to_string(), arrow(change)]);
        }
    }
    for ArchiveChange {
        mirror,
        release_sha256,
    } in &c.archives
    {
        rows.push(vec![
            format!("archive {mirror}"),
            commit_arrow(release_sha256),
        ]);
    }
    print_columns(&rows);
}

/// One series' file delta as a JSON object, for `--json`.
fn delta_json((axis, delta): &(String, SeriesDelta)) -> serde_json::Value {
    match delta {
        SeriesDelta::Compared {
            series,
            added,
            removed,
            modified,
        } => serde_json::json!({
            "axis": axis, "series": series, "status": "compared",
            "added": added, "removed": removed, "modified": modified,
        }),
        SeriesDelta::Unavailable { series, why } => serde_json::json!({
            "axis": axis, "series": series, "status": "unavailable", "why": why,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boot2deb_core::diff::Change;

    #[test]
    fn an_arrow_marks_a_side_that_does_not_state_the_value() {
        assert_eq!(
            arrow(&Change {
                from: Some("v7.1.5".into()),
                to: Some("v7.1.6".into())
            }),
            "v7.1.5 -> v7.1.6"
        );
        // An axis that appeared: the left side has nothing to say, which reads as a
        // dash rather than as an empty string a reader would take for a bug.
        assert_eq!(
            arrow(&Change {
                from: None,
                to: Some("v1.3.0".into())
            }),
            "- -> v1.3.0"
        );
    }

    #[test]
    fn commit_ids_are_shortened_on_both_sides() {
        let long = |c: char| c.to_string().repeat(40);
        assert_eq!(
            commit_arrow(&Change {
                from: Some(long('a')),
                to: Some(long('b'))
            }),
            "aaaaaaaaaaaa -> bbbbbbbbbbbb"
        );
    }
}
