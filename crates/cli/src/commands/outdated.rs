//! `outdated`: what has moved upstream since the locks were pinned.
//!
//! The read-only sibling of `update`. `update` re-pins. This only looks, and it looks
//! across every recipe at once. That is the form the question is usually asked in
//! ("is anything behind?"), rather than one recipe at a time.
//!
//! Two neighbours answer adjacent questions about the same pins, and keeping them
//! apart is deliberate:
//!
//! - `verify-sources` asks whether a pin is still **re-fetchable** — a durable tag, an
//!   ephemeral branch tip, or orphaned. An orphaned pin fails it.
//! - `outdated` asks whether something **newer** exists. Being behind is not a
//!   failure, so this always exits zero. It is a survey, not a gate.
//!
//! Every pin a lock holds is surveyed against the listing its upstream gives
//! ([`Source`]):
//!
//! - A git source, against the refs its remote advertises.
//! - A vendored rkbin blob, against its upstream tree directory.
//! - A pool `.deb`, against its pool directory's index.
//! - A .NET SDK, against its channel's releases.
//!
//! A pin with no upstream to ask is a row too. A `.deb` vendored in the config tree is
//! one, and its row says so, so the survey states everything it does not cover.
//!
//! The network cost is one probe per distinct *listing*, not per pin. The shipped
//! recipes share a kernel repo, a patches repo and a firmware directory. A survey of
//! the whole tree is therefore a handful of round-trips. Each listing is compared by
//! [`boot2deb_core::outdated`], which is pure — all the deciding is there, and this
//! module fetches and prints.

use crate::config::source_axes;
use crate::render::{print_columns, short};
use boot2deb_core::model::Overrides;
use boot2deb_core::outdated::{
    compare, compare_deb, compare_dotnet_sdk, compare_file, dotnet_channel, dotnet_sdk_versions,
    index_names, Upgrade,
};
use boot2deb_core::{resolve_recipe, ConfigRoot};
use boot2deb_engine::sources::{advertised_refs, tree_listing, LsRef};
use serde_json::json;
use std::collections::BTreeMap;
use std::time::Duration;

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// The largest listing document a probe reads: a pool directory index or a channel's
/// release metadata. Both are well under this, and the bound is what keeps a
/// misbehaving server from handing the survey an unbounded body.
const LISTING_MAX_BYTES: u64 = 16 * 1024 * 1024;

/// How long an HTTP listing probe waits.
const LISTING_TIMEOUT: Duration = Duration::from_secs(30);

/// Where one pin's upstream lists what it holds, and so how the pin is surveyed.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Source {
    /// A git ref, compared against the refs the remote advertises.
    Git {
        /// The configured upstream URL.
        url: String,
        /// The pinned ref.
        reference: String,
        /// The exact pinned commit.
        commit: String,
    },
    /// A vendored firmware blob, compared against the files of its upstream tree
    /// directory (`[rkbin.upstream]`).
    TreeFile {
        /// The upstream repository.
        url: String,
        /// The directory in it that holds the blobs.
        dir: String,
        /// The pinned file name.
        file: String,
    },
    /// A `.deb` fetched by URL, compared against the index of the directory it is in.
    PoolDeb {
        /// The pinned URL.
        url: String,
    },
    /// A .NET SDK version, compared against the SDKs its channel has released.
    DotnetSdk {
        /// The pinned version.
        version: String,
    },
    /// A pin with no upstream to ask.
    Unsurveyed {
        /// What is pinned.
        pinned: String,
        /// Why nothing can be asked about it.
        why: String,
    },
}

impl Source {
    /// The kind token the `--json` form carries.
    fn kind(&self) -> &'static str {
        match self {
            Source::Git { .. } => "git",
            Source::TreeFile { .. } => "tree-file",
            Source::PoolDeb { .. } => "pool-deb",
            Source::DotnetSdk { .. } => "dotnet-sdk",
            Source::Unsurveyed { .. } => "unsurveyed",
        }
    }

    /// What the lock pins, as a row names it: a ref, a file name, or a version.
    fn pinned(&self) -> String {
        match self {
            Source::Git { reference, .. } => reference.clone(),
            Source::TreeFile { file, .. } => file.clone(),
            Source::PoolDeb { url } => file_of(url).to_string(),
            Source::DotnetSdk { version } => version.clone(),
            Source::Unsurveyed { pinned, .. } => pinned.clone(),
        }
    }

    /// Where the listing is read from, for the `--json` form.
    fn url(&self) -> Option<String> {
        match self {
            Source::Git { url, .. } | Source::TreeFile { url, .. } | Source::PoolDeb { url } => {
                Some(url.clone())
            }
            Source::DotnetSdk { version } => dotnet_channel(version).map(|c| releases_url(&c)),
            Source::Unsurveyed { .. } => None,
        }
    }

    /// The listing this pin is compared against, as the key the survey fetches each
    /// listing once under. `None` for a pin with nothing to fetch.
    fn listing(&self) -> Option<Listing> {
        match self {
            Source::Git { url, .. } => Some(Listing::Refs(url.clone())),
            Source::TreeFile { url, dir, .. } => Some(Listing::Tree(url.clone(), dir.clone())),
            Source::PoolDeb { url } => Some(Listing::Index(dir_of(url).to_string())),
            Source::DotnetSdk { version } => dotnet_channel(version).map(Listing::Releases),
            Source::Unsurveyed { .. } => None,
        }
    }
}

/// One upstream listing, fetched once however many pins read it.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Listing {
    /// A git remote's ref advertisement.
    Refs(String),
    /// The files of one directory of a git repository's default branch.
    Tree(String, String),
    /// An HTTP directory index, by the directory's URL.
    Index(String),
    /// A .NET channel's release metadata, by channel (`10.0`).
    Releases(String),
}

/// What a fetched [`Listing`] holds.
enum Fetched {
    /// The advertised refs.
    Refs(Vec<LsRef>),
    /// The commit a tree was listed at, and its file names.
    Tree(String, Vec<String>),
    /// File names, from an index or of SDK versions.
    Names(Vec<String>),
}

/// The URL of a .NET channel's release metadata.
fn releases_url(channel: &str) -> String {
    format!("https://builds.dotnet.microsoft.com/dotnet/release-metadata/{channel}/releases.json")
}

/// The last path segment of a URL: the file a pool URL names.
fn file_of(url: &str) -> &str {
    url.rsplit_once('/').map_or(url, |(_, f)| f)
}

/// The URL of the directory a file URL is in, with its trailing slash.
fn dir_of(url: &str) -> &str {
    url.rsplit_once('/').map_or(url, |(d, _)| d)
}

/// Fetch one listing. `Err` is the reason a probe could not answer, reported on every
/// row that reads it.
fn fetch(listing: &Listing) -> std::result::Result<Fetched, String> {
    match listing {
        Listing::Refs(url) => advertised_refs(url).map(Fetched::Refs),
        Listing::Tree(url, dir) => {
            tree_listing(url, dir).map(|(commit, names)| Fetched::Tree(commit, names))
        }
        Listing::Index(dir) => {
            let body = boot2deb_engine::netfetch::fetch_bounded(
                &format!("{dir}/"),
                LISTING_MAX_BYTES,
                LISTING_TIMEOUT,
            )
            .map_err(|e| e.0)?;
            Ok(Fetched::Names(index_names(&String::from_utf8_lossy(&body))))
        }
        Listing::Releases(channel) => {
            let body = boot2deb_engine::netfetch::fetch_bounded(
                &releases_url(channel),
                LISTING_MAX_BYTES,
                LISTING_TIMEOUT,
            )
            .map_err(|e| e.0)?;
            dotnet_sdk_versions(&String::from_utf8_lossy(&body)).map(Fetched::Names)
        }
    }
}

/// A listing's names, borrowed for the comparisons.
fn names(listed: &[String]) -> Vec<&str> {
    listed.iter().map(String::as_str).collect()
}

/// The verdict for one pin against its fetched listing.
fn judge(source: &Source, fetched: Option<&std::result::Result<Fetched, String>>) -> Upgrade {
    match (source, fetched) {
        (Source::Unsurveyed { why, .. }, _) => Upgrade::Unknown { why: why.clone() },
        // A listing that could not be read is unknown, not current: the survey says so
        // per pin rather than dropping the row.
        (_, Some(Err(why))) => Upgrade::Unknown {
            why: format!("could not read {}: {why}", source.url().unwrap_or_default()),
        },
        (
            Source::Git {
                reference, commit, ..
            },
            Some(Ok(Fetched::Refs(refs))),
        ) => {
            let view: Vec<_> = refs.iter().map(LsRef::as_remote_ref).collect();
            compare(reference, commit, &view)
        }
        (Source::TreeFile { file, .. }, Some(Ok(Fetched::Tree(at, listed)))) => {
            compare_file(file, &names(listed), Some(at))
        }
        (Source::PoolDeb { url }, Some(Ok(Fetched::Names(listed)))) => {
            compare_deb(file_of(url), &names(listed))
        }
        (Source::DotnetSdk { version }, Some(Ok(Fetched::Names(listed)))) => {
            compare_dotnet_sdk(version, &names(listed))
        }
        _ => Upgrade::Unknown {
            why: "its upstream gave no listing to compare against".into(),
        },
    }
}

/// One pin to survey: which recipe and axis it belongs to, and where its upstream
/// lists what it holds.
///
/// Owned rather than borrowed from the resolved builds, so the whole survey is
/// gathered — and its cost stated — before the first network call.
struct Pin {
    /// Recipe the pin belongs to.
    recipe: String,
    /// Axis name (`kernel`, `u-boot`, `patches`, `kmod:<name>`, `rkbin:atf`, …).
    axis: String,
    /// Where it is surveyed against.
    source: Source,
}

/// A surveyed pin: what it pins, and what upstream holds beyond it.
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
    // the real cost and each listing can be fetched once.
    let mut pins: Vec<Pin> = Vec::new();
    let mut unreadable: Vec<(String, String)> = Vec::new();
    for recipe in &targets {
        match collect(root, recipe) {
            Ok(axes) => pins.extend(axes),
            Err(e) if named => return Err(e),
            Err(e) => unreadable.push((recipe.clone(), e.to_string())),
        }
    }
    let mut listings: Vec<Listing> = pins.iter().filter_map(|p| p.source.listing()).collect();
    listings.sort_unstable();
    listings.dedup();
    if !json_out {
        println!(
            "probing {} pin(s) across {} upstream listing(s) for {} recipe(s) (read-only)\n",
            pins.len(),
            listings.len(),
            targets.len() - unreadable.len(),
        );
    }
    let fetched: BTreeMap<Listing, std::result::Result<Fetched, String>> = listings
        .into_iter()
        .map(|listing| {
            let result = fetch(&listing);
            (listing, result)
        })
        .collect();

    let rows: Vec<Row> = pins
        .into_iter()
        .map(|pin| {
            let upgrade = judge(
                &pin.source,
                pin.source.listing().and_then(|l| fetched.get(&l)),
            );
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

/// One recipe's pins, every kind.
///
/// A recipe that pins nothing from upstream — a distro kernel on a board whose firmware
/// is its own, with no extra packages — contributes none, and so is silently absent
/// from the report: there is nothing upstream that could move.
fn collect(root: &ConfigRoot, recipe: &str) -> Result<Vec<Pin>> {
    let build = resolve_recipe(root, recipe, &Overrides::default())?;
    let lock = root.lock(recipe)?;
    let pin = |axis: String, source: Source| Pin {
        recipe: recipe.to_string(),
        axis,
        source,
    };
    let mut out: Vec<Pin> = source_axes(&build, &lock)?
        .into_iter()
        .map(|axis| {
            pin(
                axis.name.into_owned(),
                Source::Git {
                    url: axis.url,
                    reference: axis.reference.to_string(),
                    commit: axis.commit.to_string(),
                },
            )
        })
        .collect();
    // The vendored firmware the bootloader is built with, surveyed against the upstream
    // directory the SoC layer names for it.
    if let (Some(boot), Some(blobs)) = (build.rkbin_boot(), &lock.blobs) {
        let named = [
            ("atf", Some(&blobs.atf)),
            ("tpl", Some(&blobs.tpl)),
            ("bl32", blobs.bl32.as_ref()),
        ];
        for (role, blob) in named {
            let Some((file, _)) = blob.and_then(|b| boot2deb_engine::blobs::parse_pin(b)) else {
                continue;
            };
            let source = match &boot.rkbin.upstream {
                Some(up) => Source::TreeFile {
                    url: up.git.clone(),
                    dir: up.dir.clone(),
                    file: file.to_string(),
                },
                None => Source::Unsurveyed {
                    pinned: file.to_string(),
                    why: "no layer declares an [rkbin.upstream] to list newer blobs from".into(),
                },
            };
            out.push(pin(format!("rkbin:{role}"), source));
        }
    }
    // Packages taken from outside the build's own mirror solve: fetched by URL, or
    // vendored in the config tree.
    for deb in &lock.extra_debs {
        let (name, source) = match (&deb.url, &deb.path) {
            (Some(url), _) => (
                file_of(url).to_string(),
                Source::PoolDeb { url: url.clone() },
            ),
            (None, Some(path)) => {
                let name = file_of(path).to_string();
                (
                    name.clone(),
                    Source::Unsurveyed {
                        pinned: name,
                        why: format!(
                            "vendored in the config tree at {path}: built outside boot2deb, \
                             so there is no upstream to ask"
                        ),
                    },
                )
            }
            (None, None) => continue,
        };
        let package = name.split_once('_').map_or(name.as_str(), |(p, _)| p);
        out.push(pin(format!("extra-deb:{package}"), source));
    }
    // The SDK an application compiles with, which the app's own config pins.
    for app in &lock.apps {
        if let Some(dotnet) = &app.dotnet {
            out.push(pin(
                format!("dotnet-sdk:{}", app.name),
                Source::DotnetSdk {
                    version: dotnet.sdk.version.clone(),
                },
            ));
        }
    }
    Ok(out)
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
        Upgrade::Current => row.pin.source.pinned(),
        Upgrade::Behind { line, latest } => {
            let plural = |n: usize| if n == 1 { "" } else { "s" };
            let pinned = row.pin.source.pinned();
            match line {
                // The conservative bump and the newest release are the same, so one
                // clause says everything.
                Some(l) if l.tag == latest.tag => format!(
                    "{} -> {} ({} newer release{})",
                    pinned,
                    latest.tag,
                    latest.count,
                    plural(latest.count)
                ),
                // They differ: lead with the in-line bump, which is the move that
                // keeps the patch series and the config in their declared envelope,
                // and name the newest release after it as the wider option.
                Some(l) => format!(
                    "{} -> {} ({} newer in this line); newest upstream {} ({} newer)",
                    pinned, l.tag, l.count, latest.tag, latest.count
                ),
                // Nothing newer shares the pin's line, so every upgrade is a line
                // change and there is only one thing to offer.
                None => format!(
                    "{} -> {} ({} newer release{}, none in this line)",
                    pinned,
                    latest.tag,
                    latest.count,
                    plural(latest.count)
                ),
            }
        }
        Upgrade::TipMoved { commit } => match &row.pin.source {
            Source::Git {
                reference,
                commit: pinned,
                ..
            } => format!(
                "branch {reference}: tip moved {} -> {}",
                short(pinned),
                short(commit)
            ),
            other => format!("{}: moved to {}", other.pinned(), short(commit)),
        },
        Upgrade::Unknown { why } => why.clone(),
    }
}

/// The closing summary: the counts, and — for each kind of pin that is behind — how
/// that kind moves. A git pin re-pins through `update`; the others move by editing the
/// config that names them, which `update` then pins.
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
        println!("no pins to survey — nothing here is taken from an upstream that could move.");
        return;
    }
    println!(
        "{behind} of {} pin(s) have moved upstream; {} unknown.",
        rows.len(),
        unknown
    );
    let behind_kind = |kind: &str| {
        rows.iter()
            .any(|r| r.upgrade.is_behind() && r.pin.source.kind() == kind)
    };
    if behind_kind("git") {
        // `update` re-pins per axis, and an omitted flag preserves the previous
        // lock's ref, so the remedy is one flag per axis actually being moved.
        println!(
            "re-pin one axis at a time, e.g. `boot2deb update <recipe> --kernel-ref <tag>`; \
             `boot2deb verify-patches` first if the move leaves a series' declared envelope."
        );
    }
    if behind_kind("tree-file") {
        println!(
            "a newer rkbin blob is taken by vendoring it under blobs/<soc>/ and naming it in \
             the layer's [rkbin]; `boot2deb update` then pins its sha256."
        );
    }
    if behind_kind("pool-deb") {
        println!(
            "a newer pool .deb is taken by changing the url and sha256 of the [[extra_debs]] \
             entry that names it; `boot2deb update` then pins it."
        );
    }
    if behind_kind("dotnet-sdk") {
        println!(
            "a newer .NET SDK is taken by changing the app's [apps.build.sdk] version and \
             sha512; `boot2deb update` then pins it."
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
        object.insert("kind".into(), json!(row.pin.source.kind()));
        object.insert("url".into(), json!(row.pin.source.url()));
        object.insert("reference".into(), json!(row.pin.source.pinned()));
        if let Source::Git { commit, .. } = &row.pin.source {
            object.insert("commit".into(), json!(commit));
        }
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
                source: Source::Git {
                    url: "https://example.invalid/linux.git".into(),
                    reference: reference.into(),
                    commit: "a".repeat(40),
                },
            },
            upgrade,
        }
    }

    fn newer(tag: &str, count: usize) -> Newer {
        Newer {
            tag: tag.into(),
            commit: Some("b".repeat(40)),
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
        for key in ["axis", "kind", "url", "reference", "commit"] {
            assert!(
                !object.contains_key(key),
                "the verdict must leave '{key}' free for the pin's identity"
            );
        }
    }

    #[test]
    fn every_kind_of_pin_a_lock_holds_is_collected() {
        // Offline: collection reads the lock and the resolution, and fetches nothing.
        let root = crate::testsupport::repo_root();
        let pins = collect(
            &root,
            "turing-rk1/forky+media-accel-rockchip+jellyfin+jellyfin-v4l2request+vulkan+avs-decode",
        )
        .unwrap();
        let by_axis = |axis: &str| {
            pins.iter()
                .find(|p| p.axis == axis)
                .unwrap_or_else(|| panic!("no {axis} pin"))
                .source
                .clone()
        };
        assert!(matches!(by_axis("kernel"), Source::Git { .. }));
        let Source::TreeFile { dir, file, .. } = by_axis("rkbin:atf") else {
            panic!("the ATF is surveyed against the SoC's rkbin upstream");
        };
        assert_eq!(dir, "bin/rk35");
        assert!(file.starts_with("rk3588_bl31_v"), "{file}");
        assert!(matches!(
            by_axis("extra-deb:libicu76"),
            Source::PoolDeb { .. }
        ));
        // A deb vendored in the tree has no upstream, and the survey says so.
        let Source::Unsurveyed { why, .. } = by_axis("extra-deb:libdavs2-16") else {
            panic!("a vendored deb is not surveyable");
        };
        assert!(why.contains("debs/"), "{why}");
        assert!(matches!(
            by_axis("dotnet-sdk:jellyfin"),
            Source::DotnetSdk { .. }
        ));
    }

    #[test]
    fn pins_sharing_an_upstream_share_one_listing() {
        let tree = |file: &str| Source::TreeFile {
            url: "https://example.invalid/rkbin.git".into(),
            dir: "bin/rk35".into(),
            file: file.into(),
        };
        assert_eq!(tree("a_v1.elf").listing(), tree("b_v2.bin").listing());
        let deb = |file: &str| Source::PoolDeb {
            url: format!("http://deb.example/pool/main/i/icu/{file}"),
        };
        assert_eq!(
            deb("libicu76_76.1-4_arm64.deb").listing(),
            Some(Listing::Index("http://deb.example/pool/main/i/icu".into()))
        );
        assert_eq!(
            Source::DotnetSdk {
                version: "10.0.401".into()
            }
            .listing(),
            Some(Listing::Releases("10.0".into()))
        );
        assert_eq!(
            Source::Unsurveyed {
                pinned: "x".into(),
                why: "y".into()
            }
            .listing(),
            None
        );
    }

    #[test]
    fn a_pin_is_judged_against_the_listing_of_its_own_kind() {
        let blob = Source::TreeFile {
            url: "u".into(),
            dir: "bin/rk35".into(),
            file: "rk3588_bl31_v1.51.elf".into(),
        };
        let listed = Ok(Fetched::Tree(
            "c".repeat(40),
            vec!["rk3588_bl31_v1.56.elf".into()],
        ));
        let Upgrade::Behind { latest, .. } = judge(&blob, Some(&listed)) else {
            panic!("expected behind");
        };
        assert_eq!(latest.tag, "rk3588_bl31_v1.56.elf");
        assert_eq!(latest.commit, Some("c".repeat(40)));

        // A listing that could not be read is unknown, naming where it was read from.
        let failed: std::result::Result<Fetched, String> = Err("timed out".into());
        let Upgrade::Unknown { why } = judge(&blob, Some(&failed)) else {
            panic!("expected unknown");
        };
        assert!(why.contains("timed out"), "{why}");

        let vendored = Source::Unsurveyed {
            pinned: "x.deb".into(),
            why: "vendored".into(),
        };
        assert_eq!(judge(&vendored, None).label(), "unknown");
    }
}
