//! What an image's package set weighs, rolled up — the pure half of `size`.
//!
//! Pure and deterministic: given the per-package rows a published plan document
//! carries, group them and order them. Reading that document is the engine's job
//! (`boot2deb_engine::rootfs::read_plan_weights`); everything here is arithmetic over
//! the rows it hands back, so the whole policy is unit-testable without a build.
//!
//! **These are the archive's own estimates, not measurements.** `Installed-Size` is a
//! figure the package's builder computed over a staged tree and Debian Policy states in
//! kibibytes; it counts no filesystem overhead, no shared inode saved by a hard link,
//! and nothing the image gains after `dpkg` — the kernel `/boot` artifacts an install
//! hook produces, the initramfs, the ext4 metadata. So a report here answers "what did
//! the package set contribute", never "how large is the image", and everything it
//! renders says so.
//!
//! Policy also *permits* a stanza to omit the field. An omission is carried as
//! [`None`] and counted separately rather than folded in as zero, because a total that
//! silently absorbed unknowns would read as complete when it is not.

use serde::Serialize;
use std::collections::BTreeMap;

/// One package as a published plan document weighs it.
///
/// Projected from the plan by the engine rather than parsed here, so this module stays
/// free of the provisioner library — the same split [`crate::provenance`] makes for the
/// archive rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedWeight {
    /// Binary package name, as the plan names it.
    pub name: String,
    /// The exact version installed.
    pub version: String,
    /// The source package this was built from, where the archive states one that
    /// differs from [`name`](Self::name). `None` means the source carries the same
    /// name, which is how Debian encodes the common case — so a rollup by source keys
    /// on `source.unwrap_or(name)` rather than on this field alone.
    pub source: Option<String>,
    /// The archive's `Installed-Size` in **kibibytes**, kept in Policy's own unit so it
    /// compares against a mirror's value directly. `None` where the stanza carried
    /// none.
    pub installed_kib: Option<u64>,
    /// Index of the repository the package was retained from, which is the index the
    /// plan's archive stanzas and the provenance manifest's `[[archives]]` share.
    pub archive: usize,
}

/// Which axis a report rolls up on.
///
/// Three, because three are answerable from a plan document. A fourth — which config
/// layer asked for a package — is not: the plan records the *repository* a package was
/// fetched from, and most of an image is transitive dependencies no layer named, so any
/// per-layer figure would be a confident answer to a question the data cannot support.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Grouping {
    /// One row per binary package — the "what is biggest" view.
    Package,
    /// One row per source package, which attributes the several binary packages of one
    /// build to the thing that was built.
    Source,
    /// One row per repository, which separates what Debian shipped from what this build
    /// compiled into its own pool.
    Archive,
}

impl Grouping {
    /// What one row of this grouping is, for the report's header line.
    pub fn noun(self) -> &'static str {
        match self {
            Grouping::Package => "package",
            Grouping::Source => "source package",
            Grouping::Archive => "repository",
        }
    }

    /// [`noun`](Self::noun) in the plural, spelled out rather than suffixed — one of
    /// the three does not take a bare `s`.
    pub fn plural(self) -> &'static str {
        match self {
            Grouping::Package => "packages",
            Grouping::Source => "source packages",
            Grouping::Archive => "repositories",
        }
    }
}

/// One row of a report: a group and what it weighs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WeightRow {
    /// The group's name — a package name, a source package name, or a repository
    /// label.
    pub key: String,
    /// The sum of the group's stated `Installed-Size` values, in kibibytes.
    pub installed_kib: u64,
    /// How many packages fell in this group.
    pub packages: usize,
    /// How many of those stated no size, and so contributed nothing to
    /// [`installed_kib`](Self::installed_kib).
    pub unsized_packages: usize,
}

/// A finished rollup: the rows, and the totals that say how complete they are.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WeightReport {
    /// The axis the rows are grouped on.
    pub grouping: Grouping,
    /// Rows, heaviest first; ties broken by key so the output is stable.
    pub rows: Vec<WeightRow>,
    /// Every stated size in the plan, summed — the figure the rows partition,
    /// unchanged by any `top` truncation the caller applies afterwards.
    pub total_kib: u64,
    /// How many packages the plan carries.
    pub packages: usize,
    /// How many of those stated no size. Reported rather than hidden: a total is only
    /// as complete as this is small, and Policy permits the omission.
    pub unsized_packages: usize,
}

impl WeightReport {
    /// Roll `weights` up on `grouping`.
    ///
    /// `archive_labels` names the repositories by index, for [`Grouping::Archive`]; an
    /// index it does not cover falls back to `archive <n>`, so a plan carrying more
    /// stanzas than the caller resolved still produces a complete report rather than
    /// dropping rows.
    pub fn build(
        weights: &[PlannedWeight],
        grouping: Grouping,
        archive_labels: &[String],
    ) -> WeightReport {
        // Keyed through a BTreeMap so the pre-sort order is the key order, which is
        // what makes the tie-break below total rather than arbitrary.
        let mut groups: BTreeMap<String, WeightRow> = BTreeMap::new();
        for weight in weights {
            let key = match grouping {
                Grouping::Package => weight.name.clone(),
                // `None` means "same name as the binary package", so it is the value
                // and not an absence — folding it to the name is what makes a source's
                // several outputs land in one row.
                Grouping::Source => weight.source.clone().unwrap_or_else(|| weight.name.clone()),
                Grouping::Archive => archive_labels
                    .get(weight.archive)
                    .cloned()
                    .unwrap_or_else(|| format!("archive {}", weight.archive)),
            };
            let row = groups.entry(key.clone()).or_insert_with(|| WeightRow {
                key,
                installed_kib: 0,
                packages: 0,
                unsized_packages: 0,
            });
            row.packages += 1;
            match weight.installed_kib {
                Some(kib) => row.installed_kib += kib,
                None => row.unsized_packages += 1,
            }
        }
        let mut rows: Vec<WeightRow> = groups.into_values().collect();
        // Heaviest first, and by key within a weight: a report of an image is read
        // twice and compared, so equal rows must not swap places between runs.
        rows.sort_by(|a, b| {
            b.installed_kib
                .cmp(&a.installed_kib)
                .then_with(|| a.key.cmp(&b.key))
        });
        WeightReport {
            grouping,
            total_kib: rows.iter().map(|r| r.installed_kib).sum(),
            packages: weights.len(),
            unsized_packages: weights.iter().filter(|w| w.installed_kib.is_none()).count(),
            rows,
        }
    }

    /// The first `n` rows, or all of them when `n` is `None`.
    ///
    /// Truncation is applied after the totals are computed, so a `--top 20` report still
    /// states the whole set's weight and can say what share the rows shown account for.
    pub fn top(&self, n: Option<usize>) -> &[WeightRow] {
        match n {
            Some(n) => &self.rows[..n.min(self.rows.len())],
            None => &self.rows,
        }
    }
}

/// Index the binary packages that name a source package other than themselves.
///
/// Only the differing ones, because that is what the plan records and what a consumer
/// needs: a package whose source shares its name is the common case, and an entry
/// mapping a name to itself would say nothing while inviting a reader to treat its
/// absence elsewhere as missing data.
///
/// The join key is the binary package name, which is unique within one plan — a plan
/// installs one version of one package per name, by construction.
pub fn source_index(weights: &[PlannedWeight]) -> BTreeMap<String, String> {
    weights
        .iter()
        .filter_map(|w| {
            w.source
                .as_ref()
                .filter(|source| *source != &w.name)
                .map(|source| (w.name.clone(), source.clone()))
        })
        .collect()
}

/// Render a kibibyte count the way a size report reads it: one decimal place in the
/// largest binary unit at or below the value.
///
/// Rounded, unlike [`format_size`](crate::size::format_size), which divides exactly
/// because it renders *offsets* and a rounded offset is the wrong offset. Nothing here
/// is an offset — these are estimates being compared against each other, where `412.3
/// MiB` is the useful answer and `422195 KiB` is not.
///
/// ```
/// use boot2deb_core::weight::format_kib;
/// assert_eq!(format_kib(0), "0 B");
/// assert_eq!(format_kib(1), "1.0 KiB");
/// assert_eq!(format_kib(1536), "1.5 MiB");
/// assert_eq!(format_kib(2 * 1024 * 1024), "2.0 GiB");
/// ```
pub fn format_kib(kib: u64) -> String {
    if kib == 0 {
        // Not "0.0 KiB": a package that states a size of zero and one that states none
        // are different facts, and the report distinguishes them elsewhere. This is
        // simply the honest rendering of a zero.
        return "0 B".to_string();
    }
    const UNITS: [&str; 4] = ["KiB", "MiB", "GiB", "TiB"];
    let mut value = kib as f64;
    let mut unit = 0;
    while value >= 1024.0 && unit + 1 < UNITS.len() {
        value /= 1024.0;
        unit += 1;
    }
    format!("{value:.1} {}", UNITS[unit])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plan standing in for a real one: two binary packages from one source, a third
    /// from its own, and a fourth stating no size at all — which is the shape every
    /// property below turns on.
    fn sample() -> Vec<PlannedWeight> {
        let w =
            |name: &str, source: Option<&str>, kib: Option<u64>, archive: usize| PlannedWeight {
                name: name.to_string(),
                version: "1".to_string(),
                source: source.map(str::to_string),
                installed_kib: kib,
                archive,
            };
        vec![
            w("libsystemd0", Some("systemd"), Some(600), 0),
            w("systemd", None, Some(1400), 0),
            w("base-files", None, Some(400), 0),
            w("linux-image-boot2deb", None, None, 1),
        ]
    }

    #[test]
    fn a_source_rollup_folds_a_binary_package_onto_the_thing_that_was_built() {
        let report = WeightReport::build(&sample(), Grouping::Source, &[]);
        // `systemd` states no `Source` of its own, which Debian's encoding of "the
        // source has this name" — so it must land in the same row as `libsystemd0`,
        // which names it explicitly.
        let systemd = report
            .rows
            .iter()
            .find(|r| r.key == "systemd")
            .expect("a systemd row");
        assert_eq!(systemd.installed_kib, 2000);
        assert_eq!(systemd.packages, 2);
        assert!(
            !report.rows.iter().any(|r| r.key == "libsystemd0"),
            "a binary package must not also appear as its own source: {:?}",
            report.rows
        );
    }

    #[test]
    fn rows_are_heaviest_first_and_ties_break_on_the_key() {
        let tied = vec![
            PlannedWeight {
                name: "zzz".into(),
                version: "1".into(),
                source: None,
                installed_kib: Some(10),
                archive: 0,
            },
            PlannedWeight {
                name: "aaa".into(),
                version: "1".into(),
                source: None,
                installed_kib: Some(10),
                archive: 0,
            },
        ];
        let report = WeightReport::build(&tied, Grouping::Package, &[]);
        let keys: Vec<&str> = report.rows.iter().map(|r| r.key.as_str()).collect();
        assert_eq!(
            keys,
            ["aaa", "zzz"],
            "equal rows must not swap places between runs"
        );
    }

    #[test]
    fn an_unstated_size_is_counted_apart_rather_than_folded_in_as_zero() {
        let report = WeightReport::build(&sample(), Grouping::Package, &[]);
        assert_eq!(report.packages, 4);
        assert_eq!(report.unsized_packages, 1);
        assert_eq!(report.total_kib, 2400, "only the stated sizes are summed");
        let kernel = report
            .rows
            .iter()
            .find(|r| r.key == "linux-image-boot2deb")
            .expect("a row for the package with no stated size");
        assert_eq!(kernel.installed_kib, 0);
        assert_eq!(
            kernel.unsized_packages, 1,
            "the row says why its weight is zero"
        );
    }

    #[test]
    fn an_archive_rollup_labels_by_index_and_survives_a_label_it_was_not_given() {
        let labels = vec!["deb.debian.org/debian".to_string()];
        let report = WeightReport::build(&sample(), Grouping::Archive, &labels);
        let keys: Vec<&str> = report.rows.iter().map(|r| r.key.as_str()).collect();
        // Archive 1 has no label here — the plan carries a stanza the caller did not
        // name — and the row is still reported rather than dropped.
        assert_eq!(keys, ["deb.debian.org/debian", "archive 1"]);
        assert_eq!(report.rows[0].packages, 3);
    }

    #[test]
    fn truncation_leaves_the_totals_describing_the_whole_set() {
        let report = WeightReport::build(&sample(), Grouping::Package, &[]);
        assert_eq!(report.top(Some(2)).len(), 2);
        assert_eq!(report.top(Some(99)).len(), report.rows.len());
        assert_eq!(report.top(None).len(), report.rows.len());
        assert_eq!(
            report.total_kib, 2400,
            "a truncated view still states what the whole set weighs"
        );
    }

    #[test]
    fn the_source_index_holds_only_the_packages_whose_source_is_named_separately() {
        let index = source_index(&sample());
        assert_eq!(
            index.get("libsystemd0").map(String::as_str),
            Some("systemd")
        );
        assert_eq!(index.get("locales"), None, "no such package in the sample");
        // `systemd` and `base-files` state no `Source`, which means "the same name" —
        // an entry mapping a name to itself would say nothing, and its absence here is
        // what lets a consumer read a missing key as "same" rather than as unknown.
        assert!(!index.contains_key("systemd"));
        assert!(!index.contains_key("base-files"));
        // A stanza may also state a source *equal* to the package's own name, which the
        // plan carries verbatim; it means the same thing and is indexed the same way.
        let explicit = vec![PlannedWeight {
            name: "systemd".into(),
            version: "1".into(),
            source: Some("systemd".into()),
            installed_kib: Some(1),
            archive: 0,
        }];
        assert!(source_index(&explicit).is_empty());
    }

    #[test]
    fn sizes_render_in_the_largest_unit_that_fits() {
        assert_eq!(format_kib(0), "0 B");
        assert_eq!(format_kib(512), "512.0 KiB");
        assert_eq!(format_kib(1024), "1.0 MiB");
        assert_eq!(format_kib(422_195), "412.3 MiB");
        assert_eq!(format_kib(3 * 1024 * 1024), "3.0 GiB");
        // The unit list stops at TiB, so a larger value stays in TiB rather than
        // reaching past the table.
        assert_eq!(format_kib(5 * 1024u64.pow(4)), "5120.0 TiB");
    }
}
