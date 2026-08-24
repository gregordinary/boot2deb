//! `size`: what an image's package set weighs, and what the weight is made of.
//!
//! Reads the plan document a build published — the one file that carries the archive's
//! own `Installed-Size` and `Source` per package — and rolls it up. Offline, and it
//! builds nothing: the input is a *published build*, because a lock says what an image
//! would be made of and only a build says what one is.
//!
//! All the arithmetic is [`boot2deb_core::weight`]; this module finds the file, reads
//! it, and prints the table.

use boot2deb_core::weight::{format_kib, Grouping, WeightReport};
use boot2deb_core::ConfigRoot;
use clap::ValueEnum;
use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Which axis to roll up on.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum ByArg {
    /// One row per binary package.
    Package,
    /// One row per source package.
    Source,
    /// One row per repository.
    Archive,
}

impl From<ByArg> for Grouping {
    fn from(by: ByArg) -> Grouping {
        match by {
            ByArg::Package => Grouping::Package,
            ByArg::Source => Grouping::Source,
            ByArg::Archive => Grouping::Archive,
        }
    }
}

/// Run `size <recipe|plan>`.
///
/// `target` is either a recipe — whose published plan is looked for in the same output
/// directory `build` writes to — or a path to a `.plan` from anywhere, which is the
/// form someone handed an image uses.
pub(crate) fn run(
    root: &ConfigRoot,
    target: &str,
    by: ByArg,
    top: Option<usize>,
    features: Vec<String>,
    json: bool,
) -> Result<()> {
    let path = locate(root, target, features)?;
    let weights = boot2deb_engine::rootfs::read_plan_weights(&path)?;
    // A repository is named by the mirror that served it. The build's own pool has no
    // URL in the record — it is a per-run path on the build host — so it is named by
    // what it *is*, which is also what a reader wants to see against a row: the
    // difference that matters is Debian's packages against this build's own.
    let labels: Vec<String> = weights
        .archives
        .iter()
        .map(|a| match (&a.mirror, a.local) {
            (_, true) => "this build's own package pool".to_string(),
            (Some(mirror), _) => mirror.clone(),
            (None, _) => format!("archive {}", a.index),
        })
        .collect();
    let report = WeightReport::build(&weights.packages, by.into(), &labels);

    if json {
        // The whole report, not the truncated view: `--top` is a reading aid for a
        // terminal, and a consumer that asked for structure can slice it itself.
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    print_report(&report, top, &path);
    Ok(())
}

/// Print the table, with the caveats that keep its figures honest attached to the
/// figures rather than left to a manual page.
fn print_report(report: &WeightReport, top: Option<usize>, path: &Path) {
    let rows = report.top(top);
    println!("size {} — by {}", path.display(), report.grouping.noun());
    println!();
    let width = rows.iter().map(|r| r.key.len()).max().unwrap_or(0).max(8);
    for (rank, row) in rows.iter().enumerate() {
        let share = if report.total_kib == 0 {
            0.0
        } else {
            row.installed_kib as f64 * 100.0 / report.total_kib as f64
        };
        // The package count earns its column only where a row can hold more than one.
        let count = match report.grouping {
            Grouping::Package => String::new(),
            _ => format!("  {:>4} pkg", row.packages),
        };
        println!(
            "{:>3}. {:<width$}  {:>10}  {:>5.1}%{count}",
            rank + 1,
            row.key,
            format_kib(row.installed_kib),
            share,
        );
    }
    println!();
    match report.grouping {
        // One row per package, so a row count would restate the package count.
        Grouping::Package => println!(
            "total {} across {} packages",
            format_kib(report.total_kib),
            report.packages,
        ),
        _ => println!(
            "total {} across {} packages in {} {}",
            format_kib(report.total_kib),
            report.packages,
            report.rows.len(),
            report.grouping.plural(),
        ),
    }
    if rows.len() < report.rows.len() {
        let shown: u64 = rows.iter().map(|r| r.installed_kib).sum();
        let share = if report.total_kib == 0 {
            0.0
        } else {
            shown as f64 * 100.0 / report.total_kib as f64
        };
        println!(
            "showing the top {} of {} rows ({:.1}% of the total) — pass --top 0 for all",
            rows.len(),
            report.rows.len(),
            share,
        );
    }
    if report.unsized_packages > 0 {
        println!(
            "note: {} package(s) state no Installed-Size and contribute nothing to the total. \
             Debian Policy permits the omission; a plan written before boot2deb recorded the \
             field states none at all, in which case rebuild the image to populate it.",
            report.unsized_packages
        );
    }
    println!(
        "note: these are the archives' own Installed-Size estimates, in the kibibytes Debian \
         Policy defines them in — what each package's builder measured over a staged tree, not \
         a measurement of this image. They exclude filesystem overhead and everything the image \
         gains after dpkg (the initramfs, the /boot artifacts an install hook produces), so the \
         total is smaller than the image and is for comparing rows against each other."
    );
}

/// Resolve `target` to a published plan document.
///
/// A path is taken as given. A recipe resolves to the plan in its own output directory —
/// where a build on this machine published it — and a missing one is an error naming the
/// build that would write it, since there is nothing to weigh yet.
fn locate(root: &ConfigRoot, target: &str, features: Vec<String>) -> Result<PathBuf> {
    let as_path = Path::new(target);
    if target.ends_with(".plan") || as_path.is_file() {
        if !as_path.is_file() {
            return Err(format!("no plan document at {target}").into());
        }
        return Ok(as_path.to_path_buf());
    }
    // The same point resolution `build`, `reproduce` and `sbom` perform: a feature
    // variant's artifacts are named for the variant, so its weight must not be read
    // from the base recipe's document.
    let point = crate::config::build_point(target, features)?;
    let stem = point.artifact_stem();
    let path = crate::fsutil::absolutize(
        crate::workdir::work_dir_for(root, point.reference().as_str(), None)
            .join("artifacts")
            .join(format!("{stem}.plan")),
    );
    if !path.is_file() {
        return Err(format!(
            "no plan document at {} — a size report describes an image that exists, so build \
             one first (`boot2deb build {target}`) or pass the path to a `.plan` shipped with \
             an image.",
            path.display()
        )
        .into());
    }
    Ok(path)
}
