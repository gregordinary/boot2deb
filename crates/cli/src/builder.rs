//! The builder's own identity, and whether it still matches the checkout it is being
//! run against.
//!
//! An image's `[built_with]` provenance names the boot2deb that produced it, and the
//! commit there is stamped at *compile* time by this crate's build script. That is the
//! truthful capture point, because the binary is the builder: an installed `boot2deb`
//! has no source tree to consult, and reading whatever checkout happened to be nearby
//! at run time would record a different claim than the one the field makes.
//!
//! The cost of that correctness is a development-loop hazard. Commit, forget to
//! `cargo build`, and the next image is stamped with the commit *before* yours — a
//! wrong answer that first becomes visible in a provenance file written at the end of
//! the build, after the compiles have already been paid for. Worse, an amended commit
//! leaves the stamp naming an object no branch reaches, so the record points at
//! nothing anyone else can resolve.
//!
//! So the mismatch is detected up front instead, from two cheap `git` reads, and
//! [`Freshness`] is what a caller gates on. Relinking the CLI is seconds; discovering
//! the stamp is wrong afterwards costs the whole build.

use boot2deb_core::ConfigRoot;
use std::path::Path;

/// Tracked paths whose content decides what the compiled binary does. A change
/// anywhere else in the repo — a device `.toml`, a `.dts`, a doc page — is build
/// *input*, recorded by the lock and the config stamp, and leaves the binary's
/// identity intact.
const SOURCE_PATHS: [&str; 3] = ["crates", "Cargo.toml", "Cargo.lock"];

/// Width the build script abbreviates the binary's commit to (`rev-parse --short=12`).
/// The config tree's commit is cut to the same width so both coordinates in one
/// `[built_with]` block read alike.
const STAMP_WIDTH: usize = 12;

/// This binary's crate version. Always known; the fallback identity when no commit is.
pub(crate) fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// The commit this binary was compiled from, or `None` when it was compiled outside a
/// git checkout (a source tarball), where [`version`] alone identifies it.
pub(crate) fn commit() -> Option<&'static str> {
    option_env!("BOOT2DEB_GIT_COMMIT").filter(|s| !s.is_empty())
}

/// Whether the checkout carried uncommitted changes to [`SOURCE_PATHS`] when this
/// binary was compiled. `true` means [`commit`] does not identify it.
pub(crate) fn dirty() -> bool {
    matches!(option_env!("BOOT2DEB_GIT_DIRTY"), Some("true"))
}

/// How this binary stands relative to the boot2deb source checkout it is being run
/// against.
///
/// Only [`Behind`](Freshness::Behind) and [`SourceEdited`](Freshness::SourceEdited)
/// are certain defects, and they are the two a build gates on: in both, the binary
/// provably does not contain the checkout's current source, so the commit it will
/// stamp names something other than what ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Freshness {
    /// No comparison is possible, and none is owed: the root is not a boot2deb source
    /// checkout (an out-of-tree config tree, or an unpacked tarball), or this binary
    /// carries no commit stamp. An installed `boot2deb` run against a config tree is
    /// the ordinary case here, not a degraded one.
    Unknown,
    /// The binary was compiled from this checkout's `HEAD`, and nothing it is compiled
    /// from has changed since.
    Current,
    /// The binary was compiled from a different commit than `HEAD`. The usual cause is
    /// a commit or amend with no rebuild after it.
    Behind {
        /// The commit stamped into this binary, as it will appear in provenance.
        built: String,
        /// The checkout's current `HEAD`, abbreviated to the stamp's width.
        head: String,
    },
    /// The binary was compiled from `HEAD`, but [`SOURCE_PATHS`] have been edited
    /// since — so it predates changes that are on disk now.
    SourceEdited {
        /// The commit both the binary and the checkout name, whose content the working
        /// tree no longer matches.
        head: String,
    },
    /// The binary was compiled from a tree that already had uncommitted source changes.
    /// Whether it matches what is on disk now cannot be established either way, so this
    /// is reported and not gated — the provenance record says `dirty = true`, which is
    /// the honest answer rather than a false one.
    Unverifiable {
        /// The commit the binary names, which its dirty flag disclaims.
        built: String,
    },
}

impl Freshness {
    /// Whether this verdict is a certain mismatch between the running binary and the
    /// checkout's source, and so something a build should refuse to stamp an image
    /// with.
    pub(crate) fn is_stale(&self) -> bool {
        matches!(self, Self::Behind { .. } | Self::SourceEdited { .. })
    }

    /// One line for a human, or `None` where there is nothing worth saying — a
    /// [`Current`](Self::Current) or [`Unknown`](Self::Unknown) builder is the case
    /// that needs no action, and announcing it on every command would be noise.
    pub(crate) fn note(&self) -> Option<String> {
        match self {
            Self::Unknown | Self::Current => None,
            Self::Behind { built, head } => Some(format!(
                "this boot2deb was compiled from {built}, but the checkout is at {head}. \
                 An image built now records {built} as its builder — a commit that is not \
                 what is on disk, and that nobody else can resolve if it was amended away. \
                 Run `cargo build` (seconds) to re-stamp it."
            )),
            Self::SourceEdited { head } => Some(format!(
                "this boot2deb was compiled from {head}, and the sources under \
                 crates/ have been edited since — so it does not contain them. \
                 Run `cargo build` to pick them up."
            )),
            Self::Unverifiable { built } => Some(format!(
                "this boot2deb was compiled from a tree with uncommitted changes to \
                 crates/, so {built} does not identify it and an image built now records \
                 `dirty = true`. Commit the source to make the build identifiable."
            )),
        }
    }
}

/// Compare this binary against the boot2deb source checkout at `root`, if that is what
/// `root` is.
///
/// The applicability test is deliberately narrow: a config root only raises the
/// question when it is *also* the source tree this binary could have been built from,
/// which is the co-located layout boot2deb is developed in. An out-of-tree config tree
/// or an installed binary answers [`Freshness::Unknown`] and is left alone, because
/// there is no expectation there for a stale binary to violate.
pub(crate) fn freshness(root: &ConfigRoot) -> Freshness {
    let Some(built) = commit() else {
        return Freshness::Unknown;
    };
    let path = root.path();
    if !is_source_checkout(path) {
        return Freshness::Unknown;
    }
    let Ok(head) = boot2deb_engine::git::rev_parse_head(path) else {
        return Freshness::Unknown;
    };
    // The stamp is abbreviated and `rev-parse` is not, so the stamp is the prefix. A
    // stamp that is not a prefix of HEAD is a different commit, which is the whole
    // question — no width normalization can turn one into the other.
    if !head.starts_with(built) {
        return Freshness::Behind {
            built: built.to_string(),
            head: head.chars().take(built.len()).collect(),
        };
    }
    if dirty() {
        return Freshness::Unverifiable {
            built: built.to_string(),
        };
    }
    if boot2deb_engine::git::has_tracked_changes(path, &SOURCE_PATHS) {
        return Freshness::SourceEdited {
            head: built.to_string(),
        };
    }
    Freshness::Current
}

/// The config tree's own identity: its `HEAD`, abbreviated, and whether its tracked
/// content still matches that commit.
///
/// `None` when the root is not a git checkout. A generated or unpacked config tree has
/// no commit, and there is nothing to record — inventing a coordinate would be worse
/// than admitting there is none.
///
/// Probed rather than stamped, which is the whole difference from the binary's commit:
/// one boot2deb resolves whatever `--root` names, so which config tree ran is not
/// knowable until it is named.
pub(crate) fn config_stamp(root: &ConfigRoot) -> Option<(String, bool)> {
    let path = root.path();
    if !path.join(".git").exists() {
        return None;
    }
    let head = boot2deb_engine::git::rev_parse_head(path).ok()?;
    // The dirty question is asked of the *whole* tree here, not of `SOURCE_PATHS`:
    // every file under a config root is potential build input, so an edited `.dts` or
    // device layer is precisely what this flag exists to disclaim. That is the mirror
    // image of the binary's flag, which narrows to what compiles.
    Some((
        head.chars().take(STAMP_WIDTH).collect(),
        boot2deb_engine::git::has_tracked_changes(path, &[]),
    ))
}

/// Whether `path` is a boot2deb *source* checkout rather than only a config tree.
///
/// Both conditions are load-bearing. The crate manifest is what says this tree could
/// have produced the running binary; the `.git` directory is what gives it a commit to
/// be compared against. A source tree exported without its history has neither an
/// answer nor a problem.
fn is_source_checkout(path: &Path) -> bool {
    path.join("crates").join("cli").join("Cargo.toml").is_file() && path.join(".git").exists()
}

/// The identity line shared by `doctor` and the build gate: what this binary is, and
/// what that means for the images it stamps.
pub(crate) fn identity() -> String {
    match (commit(), dirty()) {
        (Some(c), true) => format!("{} ({c}, dirty)", version()),
        (Some(c), false) => format!("{} ({c})", version()),
        (None, _) => format!("{} (no commit — built outside a git checkout)", version()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_certain_mismatches_are_stale() {
        assert!(Freshness::Behind {
            built: "aaaaaaaaaaaa".into(),
            head: "bbbbbbbbbbbb".into(),
        }
        .is_stale());
        assert!(Freshness::SourceEdited {
            head: "aaaaaaaaaaaa".into(),
        }
        .is_stale());
        // A build compiled from an already-dirty tree is reported, never gated: the
        // provenance `dirty` flag is the honest record, and refusing the build would
        // block the ordinary edit-and-run loop it describes.
        assert!(!Freshness::Unverifiable {
            built: "aaaaaaaaaaaa".into(),
        }
        .is_stale());
        assert!(!Freshness::Current.is_stale());
        assert!(!Freshness::Unknown.is_stale());
    }

    #[test]
    fn the_cases_needing_no_action_say_nothing() {
        assert!(Freshness::Current.note().is_none());
        assert!(Freshness::Unknown.note().is_none());
    }

    #[test]
    fn a_behind_builder_names_both_commits_and_the_fix() {
        let note = Freshness::Behind {
            built: "7e6e2f02674c".into(),
            head: "90ab9c660bc1".into(),
        }
        .note()
        .expect("a mismatch has something to say");
        assert!(note.contains("7e6e2f02674c"), "{note}");
        assert!(note.contains("90ab9c660bc1"), "{note}");
        assert!(note.contains("cargo build"), "{note}");
    }

    #[test]
    fn an_unverifiable_builder_is_reported_without_claiming_a_mismatch() {
        let note = Freshness::Unverifiable {
            built: "7e6e2f02674c".into(),
        }
        .note()
        .expect("a dirty stamp is worth saying");
        assert!(note.contains("dirty"), "{note}");
        // It must not tell the operator their binary is behind: that is unknown here,
        // and naming it would send them to rebuild something that may be current.
        assert!(!note.contains("checkout is at"), "{note}");
    }

    #[test]
    fn a_tree_without_the_crate_manifest_is_not_a_source_checkout() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!is_source_checkout(dir.path()));
    }
}
