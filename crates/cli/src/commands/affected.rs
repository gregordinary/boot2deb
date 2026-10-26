//! `affected`: which build points a change to the tree reaches.
//!
//! Two questions, one mechanism:
//!
//! - `affected <rev>`: which build points read something that differs between `rev`
//!   and the working tree. The question a CI job asks of a branch (`origin/main`), so
//!   it builds and verifies only what the branch changed.
//! - `affected --claims`: which `validated` support claims rest on inputs that have
//!   changed since the commit the claim names. A lock re-pin is not the only way to
//!   change an image. An edited fragment, overlay file or package list changes it too,
//!   and no lock records that. This is the check that sees it.
//!
//! A point's inputs are the paths its build reads from the config tree, recorded while
//! the same lookups `build` makes run ([`build_inputs`]). The deciding is
//! [`boot2deb_core::affected`], which is pure, and this module asks git what changed
//! and prints.
//!
//! The builder's own source is reported apart from the inputs. A change under
//! `crates/` alters what every build runs. Which images it moves is not decidable from
//! the files, so it is named once rather than charged to every point.
//!
//! Both forms are surveys: they exit zero whatever they find, and a caller gates on
//! the `--json` document.

use crate::builder::SOURCE_PATHS;
use crate::config::{build_inputs, lock_files};
use crate::render::{print_columns, short};
use boot2deb_core::affected::{is_toml_input, toml_equivalent, touched, under_any};
use boot2deb_core::model::SupportStatus;
use boot2deb_core::ConfigRoot;
use serde_json::json;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// One build point's inputs, relative to the repository root.
struct PointInputs {
    /// The paths its build reads, and the directories it walks, under the repository.
    inputs: BTreeSet<String>,
    /// How many of its inputs lie outside the repository (an `--overlay` elsewhere),
    /// which git cannot say anything about.
    outside: usize,
}

/// Run `affected <rev>` or `affected --claims`.
pub(crate) fn run(
    root: &ConfigRoot,
    since: Option<&str>,
    claims: bool,
    json_out: bool,
) -> Result<()> {
    let repo = boot2deb_engine::git::toplevel(root.path()).ok_or_else(|| {
        format!(
            "{} is not in a git work tree, so there is no history to compare against",
            root.path().display()
        )
    })?;
    match (since, claims) {
        (Some(rev), false) => run_since(root, &repo, rev, json_out),
        (None, true) => run_claims(root, &repo, json_out),
        _ => Err("give a revision to compare against, or --claims".into()),
    }
}

/// `affected <rev>`: every build point with a lock, against one revision.
fn run_since(root: &ConfigRoot, repo: &Path, rev: &str, json_out: bool) -> Result<()> {
    let changed = changes_since(repo, rev)?;
    let builder: Vec<&str> = changed
        .iter()
        .map(String::as_str)
        .filter(|p| under_any(p, &SOURCE_PATHS))
        .collect();
    let mut references: Vec<String> = lock_files(root)?.into_iter().map(|(r, _)| r).collect();
    references.dedup();

    let mut reached: Vec<(String, Vec<String>)> = Vec::new();
    let mut unaffected: Vec<String> = Vec::new();
    let mut skipped: Vec<(String, String)> = Vec::new();
    let mut outside = 0usize;
    for reference in references {
        match point_inputs(root, repo, &reference) {
            Ok(p) => {
                outside += p.outside;
                let hit: Vec<String> = touched(&p.inputs, &changed)
                    .into_iter()
                    .map(str::to_owned)
                    .collect();
                if hit.is_empty() {
                    unaffected.push(reference);
                } else {
                    reached.push((reference, hit));
                }
            }
            // A survey of every point is not lost to one that does not resolve; the
            // point is named instead, since its inputs are unknown rather than empty.
            Err(e) => skipped.push((reference, e.to_string())),
        }
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "since": rev,
                "changed": changed,
                "builder": builder,
                "affected": reached.iter().map(|(p, hit)| json!({
                    "point": p, "inputs": hit,
                })).collect::<Vec<_>>(),
                "unaffected": unaffected,
                "skipped": skipped.iter().map(|(p, why)| json!({
                    "point": p, "error": why,
                })).collect::<Vec<_>>(),
                "outside_repository": outside,
            }))?
        );
        return Ok(());
    }
    println!(
        "{} path(s) differ between {rev} and the working tree\n",
        changed.len()
    );
    if !reached.is_empty() {
        let mut table = vec![vec!["point".to_string(), "changed inputs".to_string()]];
        table.extend(
            reached
                .iter()
                .map(|(p, hit)| vec![p.clone(), summarize(hit)]),
        );
        print_columns(&table);
        println!();
    }
    print_builder_line(&builder);
    for (point, why) in &skipped {
        eprintln!("  skipped {point}: {why}");
    }
    if outside > 0 {
        println!(
            "{outside} input(s) lie outside this repository (an --overlay elsewhere) and \
             were not compared."
        );
    }
    println!(
        "{} of {} build point(s) reached; {} unaffected.",
        reached.len(),
        reached.len() + unaffected.len() + skipped.len(),
        unaffected.len()
    );
    Ok(())
}

/// `affected --claims`: each `validated` claim against the commit it names.
fn run_claims(root: &ConfigRoot, repo: &Path, json_out: bool) -> Result<()> {
    struct ClaimRow {
        recipe: String,
        date: String,
        commit: String,
        /// The changed inputs, or why the comparison could not be made.
        verdict: std::result::Result<Vec<String>, String>,
        builder: usize,
    }
    let mut rows = Vec::new();
    for recipe in root.list_recipes()? {
        let Some(claim) = root.recipe(&recipe)?.support else {
            continue;
        };
        let (SupportStatus::Validated, Some(commit)) = (claim.status, claim.commit) else {
            continue;
        };
        let compared = if !boot2deb_engine::git::has_commit(repo, &commit) {
            Err(format!(
                "commit {} is not in this clone; fetch its history to compare",
                short(&commit)
            ))
        } else {
            changes_since(repo, &commit)
                .and_then(|changed| {
                    let p = point_inputs(root, repo, &recipe)?;
                    let builder = changed
                        .iter()
                        .filter(|c| under_any(c, &SOURCE_PATHS))
                        .count();
                    Ok((
                        touched(&p.inputs, &changed)
                            .into_iter()
                            .map(str::to_owned)
                            .collect(),
                        builder,
                    ))
                })
                .map_err(|e| e.to_string())
        };
        let (verdict, builder) = match compared {
            Ok((hit, builder)) => (Ok(hit), builder),
            Err(why) => (Err(why), 0),
        };
        rows.push(ClaimRow {
            recipe,
            date: claim.date,
            commit,
            verdict,
            builder,
        });
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "claims": rows.iter().map(|r| {
                    let mut row = json!({
                        "recipe": r.recipe,
                        "date": r.date,
                        "commit": r.commit,
                        "builder_changes": r.builder,
                    });
                    let object = row.as_object_mut().expect("a claim row is a map");
                    match &r.verdict {
                        Ok(hit) => {
                            object.insert("status".into(), json!(if hit.is_empty() { "holds" } else { "moved" }));
                            object.insert("inputs".into(), json!(hit));
                        }
                        Err(why) => {
                            object.insert("status".into(), json!("unknown"));
                            object.insert("error".into(), json!(why));
                        }
                    }
                    row
                }).collect::<Vec<_>>(),
            }))?
        );
        return Ok(());
    }
    if rows.is_empty() {
        println!("no recipe carries a validated claim, so there is nothing to check.");
        return Ok(());
    }
    let mut table = vec![vec![
        "recipe".to_string(),
        "validated".to_string(),
        "status".to_string(),
        "detail".to_string(),
    ]];
    for r in &rows {
        let (status, detail) = match &r.verdict {
            Ok(hit) if hit.is_empty() => ("holds", "no input changed".to_string()),
            Ok(hit) => ("moved", summarize(hit)),
            Err(why) => ("unknown", why.clone()),
        };
        let builder = match (r.builder, &r.verdict) {
            (0, _) | (_, Err(_)) => String::new(),
            (n, _) => format!("; the builder changed in {n} file(s)"),
        };
        table.push(vec![
            r.recipe.clone(),
            format!("{} at {}", r.date, short(&r.commit)),
            status.to_string(),
            format!("{detail}{builder}"),
        ]);
    }
    print_columns(&table);
    let moved = rows
        .iter()
        .filter(|r| r.verdict.as_ref().is_ok_and(|hit| !hit.is_empty()))
        .count();
    if moved > 0 {
        println!(
            "\n{moved} validated claim(s) rest on inputs that have changed since their \
             image was built. Boot an image of the current tree and record its commit, \
             or set the claim back to `expected`."
        );
    }
    Ok(())
}

/// The paths that differ between `rev` and the working tree, less the TOML files whose
/// two versions say the same thing to a build ([`toml_equivalent`]).
fn changes_since(repo: &Path, rev: &str) -> Result<Vec<String>> {
    let mut changed = boot2deb_engine::git::changed_paths(repo, rev)?;
    changed.retain(|path| {
        if !is_toml_input(path) {
            return true;
        }
        let old = boot2deb_engine::git::show_file(repo, rev, path);
        let new = std::fs::read_to_string(repo.join(path)).ok();
        !matches!((old, new), (Some(old), Some(new)) if toml_equivalent(path, &old, &new))
    });
    Ok(changed)
}

/// Record the inputs of one build point, as paths relative to the repository root.
///
/// The probes are made under the search roots as given, which may be relative or reach
/// the repository through a symlink, so each is re-rooted on its search root's
/// canonical path before it is compared with git's.
fn point_inputs(root: &ConfigRoot, repo: &Path, reference: &str) -> Result<PointInputs> {
    root.record_probes();
    let made = build_inputs(root, reference);
    let probes = root.take_probes();
    made?;
    let repo = repo.canonicalize()?;
    let canonical: Vec<(PathBuf, PathBuf)> = root
        .search_paths()
        .iter()
        .map(|r| Ok((r.clone(), r.canonicalize()?)))
        .collect::<std::io::Result<_>>()?;
    let mut inputs = BTreeSet::new();
    let mut outside = 0usize;
    for probe in probes {
        let Some((given, canon)) = canonical.iter().find(|(given, _)| probe.starts_with(given))
        else {
            outside += 1;
            continue;
        };
        let full = crate::fsutil::normalize(canon.join(probe.strip_prefix(given)?));
        match full.strip_prefix(&repo) {
            Ok(rel) => {
                inputs.insert(rel.to_string_lossy().into_owned());
            }
            Err(_) => outside += 1,
        }
    }
    Ok(PointInputs { inputs, outside })
}

/// A list of changed paths as one cell: the first, and how many more.
fn summarize(paths: &[String]) -> String {
    match paths {
        [] => String::new(),
        [one] => one.clone(),
        [first, rest @ ..] => format!("{first} (+{} more)", rest.len()),
    }
}

/// The line naming changes to the builder's own source, which reach every build.
fn print_builder_line(builder: &[&str]) {
    if !builder.is_empty() {
        println!(
            "the builder changed in {} file(s) under {}: every build runs different code, \
             and which images that moves is not decidable from the files.",
            builder.len(),
            SOURCE_PATHS.join(", ")
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::repo_root;

    #[test]
    fn a_points_inputs_are_the_files_and_directories_its_build_reads() {
        let root = repo_root();
        let repo = boot2deb_engine::git::toplevel(root.path());
        let Some(repo) = repo else {
            eprintln!("skipping: the source tree is not a git work tree");
            return;
        };
        let p = point_inputs(&root, &repo, "turing-rk1/forky").unwrap();
        let prefix = root
            .path()
            .canonicalize()
            .unwrap()
            .strip_prefix(repo.canonicalize().unwrap())
            .unwrap()
            .to_path_buf();
        let rel = |p: &str| prefix.join(p).to_string_lossy().into_owned();
        for wanted in [
            "recipes/turing-rk1/forky.toml",
            "recipes/turing-rk1/forky.lock",
            "devices/turing-rk1.toml",
            "socs/rk3588.toml",
            "boot-methods/rockchip-rkbin.toml",
            "base.toml",
            "blobs/rk3588",
            "blobs/keyrings/debian-archive-keyring.gpg",
            "base/overlay",
        ] {
            assert!(p.inputs.contains(&rel(wanted)), "{wanted} is an input");
        }
        // A kernel fragment the kernel definition names is read, and another board's
        // device layer is not.
        assert!(
            p.inputs.iter().any(|i| i.contains("fragments/")),
            "{:?}",
            p.inputs
        );
        assert!(!p.inputs.contains(&rel("devices/h96-max-m9.toml")));
        assert_eq!(p.outside, 0);
    }

    #[test]
    fn a_list_of_paths_is_one_cell() {
        assert_eq!(summarize(&["a".into()]), "a");
        assert_eq!(
            summarize(&["a".into(), "b".into(), "c".into()]),
            "a (+2 more)"
        );
    }
}
