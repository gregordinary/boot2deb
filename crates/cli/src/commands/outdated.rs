//! `outdated`: what has moved upstream since the locks were pinned.
//!
//! The read-only sibling of `update`. `update` re-pins; this only looks, and it
//! looks across every recipe at once, which is the form the question is usually
//! asked in ("is anything behind?") rather than one recipe at a time.
//!
//! Two neighbours answer adjacent questions about the same pins, and keeping them
//! apart is deliberate:
//!
//! - `verify-sources` asks whether a pin is still **re-fetchable** — a durable tag, an
//!   ephemeral branch tip, or orphaned. An orphaned pin fails it.
//! - `outdated` asks whether something **newer** exists. Being behind is not a
//!   failure, so this always exits zero; it is a survey, not a gate.
//!
//! The network cost is one `git ls-remote` per distinct *URL*, not per pin: the
//! shipped recipes share a kernel repo and a patches repo, so a survey of the whole
//! tree is a handful of round-trips. Each remote's advertisement is compared by
//! [`boot2deb_core::outdated`], which is pure — all the deciding is there, and this
//! module fetches and prints.

use crate::config::source_axes;
use crate::render::{print_columns, short};
use boot2deb_core::model::Overrides;
use boot2deb_core::outdated::{compare, Upgrade};
use boot2deb_core::{resolve_recipe, ConfigRoot};
use boot2deb_engine::sources::{advertised_refs, LsRef};
use serde_json::json;
use std::collections::BTreeMap;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// One pin to survey: which recipe and axis it belongs to, and what it pins.
///
/// Owned rather than borrowed from the resolved builds, so the whole survey is
/// gathered — and its cost stated — before the first network call.
struct Pin {
    /// Recipe the pin belongs to.
    recipe: String,
    /// Axis name (`kernel`, `u-boot`, `patches`, `kmod:<name>`, …).
    axis: String,
    /// The configured upstream URL to probe.
    url: String,
    /// The pinned ref.
    reference: String,
    /// The exact pinned commit.
    commit: String,
}

/// A surveyed pin: what it pins, and what the remote holds beyond it.
struct Row {
    /// The pin this row reports on.
    pin: Pin,
    /// The verdict.
    upgrade: Upgrade,
}

/// Run `outdated [<recipe>...]`.
///
/// With no recipe named, every recipe in the config tree is surveyed; a recipe whose
/// lock is missing or unreadable is reported and skipped rather than ending the run,
/// since a survey of nine recipes should not be lost to one. A recipe named
/// explicitly is a hard error if it cannot be read — the caller asked about that one.
pub(crate) fn run(root: &ConfigRoot, recipes: &[String], json_out: bool) -> Result<()> {
    let (named, targets) = match recipes.is_empty() {
        false => (true, recipes.to_vec()),
        true => (false, root.list_recipes()?),
    };
    // Every pin to probe, gathered before any network call, so the header can state
    // the real cost and the remote cache can be filled once per URL.
    let mut pins: Vec<Pin> = Vec::new();
    let mut unreadable: Vec<(String, String)> = Vec::new();
    for recipe in &targets {
        match collect(root, recipe) {
            Ok(axes) => pins.extend(axes),
            Err(e) if named => return Err(e),
            Err(e) => unreadable.push((recipe.clone(), e.to_string())),
        }
    }
    // One `ls-remote` per distinct URL: the shipped recipes share a kernel repo and a
    // patches repo, so probing per pin would repeat the same round-trip many times
    // over for an identical answer.
    let mut urls: Vec<String> = pins.iter().map(|p| p.url.clone()).collect();
    urls.sort_unstable();
    urls.dedup();
    if !json_out {
        println!(
            "probing {} source pin(s) across {} remote(s) for {} recipe(s) (read-only)\n",
            pins.len(),
            urls.len(),
            targets.len() - unreadable.len(),
        );
    }
    let mut advertisements: BTreeMap<String, std::result::Result<Vec<LsRef>, String>> =
        BTreeMap::new();
    for url in urls {
        let refs = advertised_refs(&url);
        advertisements.insert(url, refs);
    }

    let rows: Vec<Row> = pins
        .into_iter()
        .map(|pin| {
            let upgrade = match &advertisements[pin.url.as_str()] {
                Ok(refs) => {
                    let view: Vec<_> = refs.iter().map(LsRef::as_remote_ref).collect();
                    compare(&pin.reference, &pin.commit, &view)
                }
                // A remote that could not be reached is unknown, not current: the
                // survey says so per pin rather than dropping the row.
                Err(why) => Upgrade::Unknown {
                    why: format!("could not read {}: {why}", pin.url),
                },
            };
            Row { pin, upgrade }
        })
        .collect();

    if json_out {
        return print_json(&rows, &unreadable);
    }
    print_table(&rows, targets.len() == 1);
    print_summary(&rows, &unreadable);
    Ok(())
}

/// One recipe's git source pins.
///
/// A recipe that fetches nothing from git — a distro kernel on a board whose firmware
/// is its own — contributes none, and so is silently absent from the report: there
/// is no upstream ref that could move.
fn collect(root: &ConfigRoot, recipe: &str) -> Result<Vec<Pin>> {
    let build = resolve_recipe(root, recipe, &Overrides::default())?;
    let lock = root.lock(recipe)?;
    Ok(source_axes(&build, &lock)?
        .into_iter()
        .map(|axis| Pin {
            recipe: recipe.to_string(),
            axis: axis.name.into_owned(),
            url: axis.url,
            reference: axis.reference.to_string(),
            commit: axis.commit.to_string(),
        })
        .collect())
}

/// The human table: one row per pin, with the recipe column dropped when the survey
/// covers a single recipe (where it would be a column of one repeated value).
fn print_table(rows: &[Row], single: bool) {
    let mut table: Vec<Vec<String>> = Vec::with_capacity(rows.len() + 1);
    let header = |mut cells: Vec<String>| {
        if !single {
            cells.insert(0, "recipe".into());
        }
        cells
    };
    table.push(header(vec![
        "axis".into(),
        "status".into(),
        "detail".into(),
    ]));
    for row in rows {
        let mut cells = vec![
            row.pin.axis.clone(),
            row.upgrade.label().into(),
            detail(row),
        ];
        if !single {
            cells.insert(0, row.pin.recipe.clone());
        }
        table.push(cells);
    }
    if table.len() > 1 {
        print_columns(&table);
    }
    println!();
}

/// The detail cell: what the pin is at, and what the remote holds beyond it.
fn detail(row: &Row) -> String {
    match &row.upgrade {
        // Naming the ref it is current *at* is the point of the row — "current" alone
        // does not say which release the build is on.
        Upgrade::Current => row.pin.reference.clone(),
        Upgrade::Behind { line, latest } => {
            let plural = |n: usize| if n == 1 { "" } else { "s" };
            match line {
                // The conservative bump and the newest release are the same, so one
                // clause says everything.
                Some(l) if l.tag == latest.tag => format!(
                    "{} -> {} ({} newer release{})",
                    row.pin.reference,
                    latest.tag,
                    latest.count,
                    plural(latest.count)
                ),
                // They differ: lead with the in-line bump, which is the move that
                // keeps the patch series and the config in their declared envelope,
                // and name the newest release after it as the wider option.
                Some(l) => format!(
                    "{} -> {} ({} newer in this line); newest upstream {} ({} newer)",
                    row.pin.reference, l.tag, l.count, latest.tag, latest.count
                ),
                // Nothing newer shares the pin's line, so every upgrade is a line
                // change and there is only one thing to offer.
                None => format!(
                    "{} -> {} ({} newer release{}, none in this line)",
                    row.pin.reference,
                    latest.tag,
                    latest.count,
                    plural(latest.count)
                ),
            }
        }
        Upgrade::TipMoved { commit } => format!(
            "branch {}: tip moved {} -> {}",
            row.pin.reference,
            short(&row.pin.commit),
            short(commit)
        ),
        Upgrade::Unknown { why } => why.clone(),
    }
}

/// The closing summary: the counts, and — when something is behind — the command
/// that acts on it.
fn print_summary(rows: &[Row], unreadable: &[(String, String)]) {
    for (recipe, why) in unreadable {
        eprintln!("  skipped {recipe}: {why}");
    }
    let behind = rows.iter().filter(|r| r.upgrade.is_behind()).count();
    let unknown = rows
        .iter()
        .filter(|r| matches!(r.upgrade, Upgrade::Unknown { .. }))
        .count();
    if rows.is_empty() {
        println!("no git source pins to survey — nothing here is fetched from a repo.");
        return;
    }
    println!(
        "{behind} of {} pin(s) have moved upstream; {} unknown.",
        rows.len(),
        unknown
    );
    if behind > 0 {
        // `update` re-pins per axis, and an omitted flag preserves the previous
        // lock's ref, so the remedy is one flag per axis actually being moved.
        println!(
            "re-pin one axis at a time, e.g. `boot2deb update <recipe> --kernel-ref <tag>`; \
             `boot2deb verify-patches` first if the move leaves a series' declared envelope."
        );
    }
}

/// The `--json` document: the rows grouped by recipe, plus the counts a caller
/// gates on.
///
/// Each pin object is the pin's identity with the verdict flattened into it, so
/// `status` is a top-level field and a consumer switches on one key.
fn print_json(rows: &[Row], unreadable: &[(String, String)]) -> Result<()> {
    let mut by_recipe: BTreeMap<&str, Vec<serde_json::Value>> = BTreeMap::new();
    for row in rows {
        let mut pin = serde_json::to_value(&row.upgrade)?;
        let object = pin.as_object_mut().expect("an Upgrade serializes as a map");
        object.insert("axis".into(), json!(row.pin.axis));
        object.insert("url".into(), json!(row.pin.url));
        object.insert("reference".into(), json!(row.pin.reference));
        object.insert("commit".into(), json!(row.pin.commit));
        by_recipe.entry(&row.pin.recipe).or_default().push(pin);
    }
    let recipes: Vec<serde_json::Value> = by_recipe
        .into_iter()
        .map(|(recipe, pins)| json!({"recipe": recipe, "pins": pins}))
        .collect();
    println!(
        "{}",
        serde_json::to_string_pretty(&json!({
            "recipes": recipes,
            "behind": rows.iter().filter(|r| r.upgrade.is_behind()).count(),
            "unknown": rows
                .iter()
                .filter(|r| matches!(r.upgrade, Upgrade::Unknown { .. }))
                .count(),
            "pins": rows.len(),
            "skipped": unreadable
                .iter()
                .map(|(recipe, why)| json!({"recipe": recipe, "error": why}))
                .collect::<Vec<_>>(),
        }))?
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use boot2deb_core::outdated::Newer;

    /// A row carrying `upgrade`, for the rendering tests.
    fn row(reference: &str, upgrade: Upgrade) -> Row {
        Row {
            pin: Pin {
                recipe: "turing-rk1/forky".into(),
                axis: "kernel".into(),
                url: "https://example.invalid/linux.git".into(),
                reference: reference.into(),
                commit: "a".repeat(40),
            },
            upgrade,
        }
    }

    fn newer(tag: &str, count: usize) -> Newer {
        Newer {
            tag: tag.into(),
            commit: "b".repeat(40),
            count,
        }
    }

    #[test]
    fn the_detail_cell_names_the_conservative_bump_before_the_widest_one() {
        // The two moves have different consequences — an in-line bump keeps the patch
        // series inside its declared envelope, a line change usually does not — so the
        // row has to distinguish them rather than printing only the newest tag.
        let both = row(
            "v7.1.6",
            Upgrade::Behind {
                line: Some(newer("v7.1.9", 2)),
                latest: newer("v7.2.1", 4),
            },
        );
        let text = detail(&both);
        assert!(text.starts_with("v7.1.6 -> v7.1.9"), "{text}");
        assert!(text.contains("newest upstream v7.2.1 (4 newer)"), "{text}");

        // When they coincide there is only one move, and the row says it once.
        let same = row(
            "v7.1.6",
            Upgrade::Behind {
                line: Some(newer("v7.1.7", 1)),
                latest: newer("v7.1.7", 1),
            },
        );
        assert_eq!(detail(&same), "v7.1.6 -> v7.1.7 (1 newer release)");

        // u-boot's shape: every release is its own line, so the row must not imply
        // an in-line move exists.
        let line_change = row(
            "v2026.04",
            Upgrade::Behind {
                line: None,
                latest: newer("v2026.10", 2),
            },
        );
        assert_eq!(
            detail(&line_change),
            "v2026.04 -> v2026.10 (2 newer releases, none in this line)"
        );
    }

    #[test]
    fn a_current_row_names_the_release_it_is_current_at() {
        // "current" alone does not say which release the build is on, which is half
        // of what the survey is read for.
        assert_eq!(detail(&row("v7.1.6", Upgrade::Current)), "v7.1.6");
    }

    #[test]
    fn a_moved_branch_tip_shows_both_commits() {
        let moved = row(
            "master",
            Upgrade::TipMoved {
                commit: "c".repeat(40),
            },
        );
        let text = detail(&moved);
        assert!(
            text.contains("aaaaaaaaaaaa") && text.contains("cccccccccccc"),
            "{text}"
        );
        assert!(text.contains("branch master"), "{text}");
    }

    #[test]
    fn the_json_pin_object_carries_the_status_at_its_top_level() {
        // A consumer switches on one key, so the verdict is flattened into the pin
        // rather than nested under it — and the identity fields must not collide with
        // the verdict's own.
        let row = row(
            "v7.1.6",
            Upgrade::Behind {
                line: None,
                latest: newer("v7.2", 1),
            },
        );
        let mut pin = serde_json::to_value(&row.upgrade).unwrap();
        let object = pin.as_object_mut().unwrap();
        assert_eq!(object["status"], "behind");
        for key in ["axis", "url", "reference", "commit"] {
            assert!(
                !object.contains_key(key),
                "the verdict must leave '{key}' free for the pin's identity"
            );
        }
    }
}
