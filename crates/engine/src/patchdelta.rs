//! Which patch files a series gained, lost, and rewrote between two commits of the
//! `patches` repo.
//!
//! The resolution of a bare "patches commit moved" into named files. A lock records
//! only the commit, so two builds pinning two commits say nothing about *what* moved
//! between them — the difference between deciding a `validated` claim survives a bump
//! and skipping the question.
//!
//! Read entirely out of the object store ([`git::show_file`], [`git::blob_id`]), so it
//! answers about historical pins without a checkout at either of them and without
//! disturbing a checkout at one. It needs the repo to be present and to carry both
//! commits, which a co-developed or freshly-fetched checkout may not — so every gap is
//! an [`Unavailable`](SeriesDelta::Unavailable) answer rather than a failure, on the
//! reasoning that a comparison missing one section is worth more than no comparison.

use crate::git;
use boot2deb_core::series::{PatchSeries, Scope};
use std::path::Path;

/// What happened to one series' patch files between two commits.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SeriesDelta {
    /// Both commits' series documents were read and compared.
    Compared {
        /// The series name.
        series: String,
        /// Patch files the series lists at `to` and not at `from`.
        added: Vec<String>,
        /// Patch files it lists at `from` and not at `to`.
        removed: Vec<String>,
        /// Patch files it lists at both, whose contents differ.
        modified: Vec<String>,
    },
    /// The delta could not be computed, and why — the commit moved, and this is what
    /// stands in for the file list.
    Unavailable {
        /// The series name.
        series: String,
        /// What was missing, for the report to print in place of the file list.
        why: String,
    },
}

impl SeriesDelta {
    /// The series this delta describes, however it turned out.
    pub fn series(&self) -> &str {
        match self {
            SeriesDelta::Compared { series, .. } | SeriesDelta::Unavailable { series, .. } => {
                series
            }
        }
    }

    /// Whether a compared delta found no file change at all. An unavailable one is
    /// not quiet: it has nothing to say, which is different from having nothing to
    /// report.
    pub fn is_quiet(&self) -> bool {
        match self {
            SeriesDelta::Compared {
                added,
                removed,
                modified,
                ..
            } => added.is_empty() && removed.is_empty() && modified.is_empty(),
            SeriesDelta::Unavailable { .. } => false,
        }
    }
}

/// The patch files of `series` that differ between commits `from` and `to` of the
/// `patches` checkout at `repo`.
///
/// A series absent at one end is not a gap: every file it lists at the other end is
/// reported added or removed, since that is exactly what happened. Absent at both
/// ends is unavailable — there is no series to describe.
///
/// The file lists span every [`Scope`], because a series' patches are one set of
/// files regardless of which tree each is applied to, and a reader asking what moved
/// is asking about the files.
pub fn series_delta(repo: &Path, from: &str, to: &str, series: &str) -> SeriesDelta {
    let unavailable = |why: String| SeriesDelta::Unavailable {
        series: series.to_string(),
        why,
    };
    if !repo.is_dir() {
        return unavailable(format!("no patches checkout at {}", repo.display()));
    }
    for commit in [from, to] {
        if !git::has_commit(repo, commit) {
            return unavailable(format!(
                "{} is not in the patches checkout at {} — fetch it to see which \
                 patch files moved",
                &commit[..commit.len().min(12)],
                repo.display()
            ));
        }
    }
    let path = format!("series/{series}.toml");
    let paths_at = |commit: &str| -> Option<Result<Vec<String>, String>> {
        let text = git::show_file(repo, commit, &path)?;
        Some(match toml::from_str::<PatchSeries>(&text) {
            Ok(doc) => Ok(patch_paths(&doc)),
            Err(e) => Err(format!(
                "{path} at {} does not parse: {e}",
                &commit[..commit.len().min(12)]
            )),
        })
    };
    let (before, after) = match (paths_at(from), paths_at(to)) {
        (None, None) => return unavailable(format!("{path} exists at neither commit")),
        (b, a) => (b.unwrap_or(Ok(Vec::new())), a.unwrap_or(Ok(Vec::new()))),
    };
    let (before, after) = match (before, after) {
        (Err(why), _) | (_, Err(why)) => return unavailable(why),
        (Ok(b), Ok(a)) => (b, a),
    };
    SeriesDelta::Compared {
        series: series.to_string(),
        added: after
            .iter()
            .filter(|p| !before.contains(p))
            .cloned()
            .collect(),
        removed: before
            .iter()
            .filter(|p| !after.contains(p))
            .cloned()
            .collect(),
        // Listed at both ends: equal object ids are equal bytes, so an id that moved
        // is a patch that was rewritten under an unchanged name — the case a
        // membership comparison alone would call unchanged.
        modified: after
            .iter()
            .filter(|p| before.contains(p))
            .filter(|p| git::blob_id(repo, from, p) != git::blob_id(repo, to, p))
            .cloned()
            .collect(),
    }
}

/// Every patch path a series document lists, across all scopes, in scope order and
/// deduplicated — one tree's list may name a file another's does too.
fn patch_paths(doc: &PatchSeries) -> Vec<String> {
    let mut paths: Vec<String> = Scope::ALL
        .iter()
        .flat_map(|scope| doc.scope(*scope))
        .map(|entry| entry.path().to_string())
        .collect();
    paths.dedup();
    paths
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::process::Command;

    /// A throwaway `patches` repo with two commits, so the delta is exercised against
    /// real object-store reads rather than a stub of them.
    struct Fixture {
        dir: tempfile::TempDir,
        first: String,
        second: String,
    }

    impl Fixture {
        fn repo(&self) -> PathBuf {
            self.dir.path().to_path_buf()
        }
    }

    fn git(repo: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(repo)
            .args(args)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.invalid")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.invalid")
            .output()
            .expect("git runs");
        assert!(out.status.success(), "git {args:?}: {out:?}");
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn write(repo: &Path, rel: &str, body: &str) {
        let path = repo.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, body).unwrap();
    }

    /// Commit one: two patches. Commit two: the first rewritten, the second dropped,
    /// a third added — every category at once.
    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        git(repo, &["init", "-q", "-b", "main"]);
        write(repo, "media-accel/kernel/050-vdec.patch", "one\n");
        write(repo, "media-accel/kernel/060-venc.patch", "two\n");
        write(
            repo,
            "series/rk3588-accel.toml",
            "kernel = [\n  \"media-accel/kernel/050-vdec.patch\",\n  \
             \"media-accel/kernel/060-venc.patch\",\n]\n",
        );
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "-qm", "first"]);
        let first = git(repo, &["rev-parse", "HEAD"]);

        write(
            repo,
            "media-accel/kernel/050-vdec.patch",
            "one, rewritten\n",
        );
        std::fs::remove_file(repo.join("media-accel/kernel/060-venc.patch")).unwrap();
        write(repo, "rocket/087-task-array.patch", "three\n");
        write(
            repo,
            "series/rk3588-accel.toml",
            "kernel = [\n  \"media-accel/kernel/050-vdec.patch\",\n  \
             \"rocket/087-task-array.patch\",\n]\n",
        );
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "-qm", "second"]);
        let second = git(repo, &["rev-parse", "HEAD"]);
        Fixture { dir, first, second }
    }

    #[test]
    fn a_moved_commit_resolves_into_added_removed_and_rewritten_files() {
        let f = fixture();
        let delta = series_delta(&f.repo(), &f.first, &f.second, "rk3588-accel");
        let SeriesDelta::Compared {
            added,
            removed,
            modified,
            ..
        } = &delta
        else {
            panic!("both commits are present: {delta:?}");
        };
        assert_eq!(added, &["rocket/087-task-array.patch"]);
        assert_eq!(removed, &["media-accel/kernel/060-venc.patch"]);
        // Listed at both ends under one name, different bytes — the case a membership
        // comparison calls unchanged and this exists to catch.
        assert_eq!(modified, &["media-accel/kernel/050-vdec.patch"]);
        assert!(!delta.is_quiet());
    }

    #[test]
    fn comparing_a_commit_with_itself_is_quiet() {
        let f = fixture();
        let delta = series_delta(&f.repo(), &f.second, &f.second, "rk3588-accel");
        assert!(delta.is_quiet(), "{delta:?}");
    }

    /// The degradation the plan asks for: a commit the checkout does not carry
    /// reports itself unavailable, naming the commit, rather than failing the whole
    /// comparison the section belongs to.
    #[test]
    fn a_commit_the_checkout_lacks_degrades_rather_than_failing() {
        let f = fixture();
        let absent = "0".repeat(40);
        match series_delta(&f.repo(), &f.first, &absent, "rk3588-accel") {
            SeriesDelta::Unavailable { series, why } => {
                assert_eq!(series, "rk3588-accel");
                assert!(why.contains("000000000000"), "{why}");
            }
            other => panic!("expected unavailable, got {other:?}"),
        }
        // A path that is not a checkout at all is the same kind of answer.
        match series_delta(Path::new("/nonexistent"), &f.first, &f.second, "x") {
            SeriesDelta::Unavailable { why, .. } => assert!(why.contains("/nonexistent"), "{why}"),
            other => panic!("expected unavailable, got {other:?}"),
        }
    }

    /// A series that did not exist yet is not a gap: everything it lists now arrived,
    /// which is what a reader wants to be told.
    #[test]
    fn a_series_absent_at_one_end_reports_its_whole_list() {
        let f = fixture();
        let repo = f.repo();
        write(
            &repo,
            "series/rocket.toml",
            "kernel = [\"rocket/087-task-array.patch\"]\n",
        );
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-qm", "third"]);
        let third = git(&repo, &["rev-parse", "HEAD"]);

        let SeriesDelta::Compared {
            added,
            removed,
            modified,
            ..
        } = series_delta(&repo, &f.first, &third, "rocket")
        else {
            panic!("both commits are present");
        };
        assert_eq!(added, &["rocket/087-task-array.patch"]);
        assert!(removed.is_empty());
        assert!(modified.is_empty());

        // Absent at both ends is the one case with nothing to describe.
        match series_delta(&repo, &f.first, &f.second, "rocket") {
            SeriesDelta::Unavailable { why, .. } => {
                assert!(why.contains("neither commit"), "{why}")
            }
            other => panic!("expected unavailable, got {other:?}"),
        }
    }
}
