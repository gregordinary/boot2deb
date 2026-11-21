//! The builder's own identity, and whether it still matches the checkout it is being
//! run against.
//!
//! An image's `[built_with]` provenance names the boot2deb that produced it, and the
//! commit there is stamped at *compile* time by this crate's build script. That is
//! the truthful capture point, because the binary is the builder. An installed
//! `boot2deb` has no source tree to consult. Reading whatever checkout happened to
//! be nearby at run time would record a different claim than the one the field
//! makes.
//!
//! The cost of that correctness is a development-loop hazard. Commit, forget to
//! `cargo build`, and the next image is stamped with the commit *before* yours. That
//! wrong answer first becomes visible in a provenance file written at the end of the
//! build, after the compiles are already paid for. Worse, an amended commit
//! leaves the stamp naming an object no branch reaches, so the record points at
//! nothing anyone else can resolve.
//!
//! So the mismatch is detected up front instead, from three cheap `git` reads, and
//! [`Freshness`] is what a caller gates on. Relinking the CLI is seconds.
//! Discovering the stamp is wrong afterwards costs the whole build.

use boot2deb_core::ConfigRoot;
use std::path::Path;

include!("../source_paths.rs");

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

/// How [`SOURCE_PATHS`] stood against [`commit`] when this binary was compiled, by the
/// rule [`SourceState::of`] states. [`SourceState::Unknown`] when git could not read
/// them, and when there is no commit to stand against.
pub(crate) fn source_state() -> SourceState {
    compiled_state(option_env!("BOOT2DEB_GIT_DIRTY"))
}

/// Read a `BOOT2DEB_GIT_DIRTY` value. Anything but one of the two definite stamps,
/// an absent variable included, is [`SourceState::Unknown`].
fn compiled_state(stamp: Option<&str>) -> SourceState {
    [SourceState::Clean, SourceState::Dirty]
        .into_iter()
        .find(|state| Some(state.stamp()) == stamp)
        .unwrap_or(SourceState::Unknown)
}

/// Whether [`commit`] fails to identify this binary: its sources differed from the
/// commit when it was compiled, or git could not tell. `false` without a commit, where
/// there is nothing for the flag to disclaim.
///
/// This is the value the provenance record's `dirty` takes, and that field has no third
/// value. "Could not tell" therefore reads as `true`, the answer that claims nothing
/// about the commit. [`source_state`] keeps the two apart for a caller that reports
/// which it was.
pub(crate) fn dirty() -> bool {
    commit().is_some() && source_state() != SourceState::Clean
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
    /// contains no commit stamp. An installed `boot2deb` run against a config tree is
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
    /// The binary was compiled from `HEAD`, but [`SOURCE_PATHS`] have changed since, by
    /// an edit or by a file git does not track — so it predates what is on disk now.
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
    /// Git could not read the state of [`SOURCE_PATHS`] when the binary was compiled,
    /// so its stamp says `unknown`. Reported and not gated, as
    /// [`Unverifiable`](Self::Unverifiable) is, and the provenance record says
    /// `dirty = true` for the same reason: the commit is not shown to identify the binary.
    StampUnknown {
        /// The commit the binary names, which nothing has shown identifies it.
        built: String,
    },
    /// The root is this binary's source checkout, but git cannot read its `HEAD` or the
    /// state of [`SOURCE_PATHS`] now. A failed read proves no mismatch, so this is
    /// reported and not gated. It is never [`Current`](Self::Current) either, because
    /// nothing has checked the binary against the checkout.
    CheckoutUnreadable {
        /// The commit the binary names.
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
                "this boot2deb was compiled from {head}, and its sources ({}) have changed \
                 since, by an edit or a file git does not track — so it does not contain \
                 them. Run `cargo build` to pick them up.",
                SOURCE_PATHS.join(", ")
            )),
            Self::Unverifiable { built } => Some(format!(
                "this boot2deb was compiled from a tree with uncommitted changes to its \
                 sources ({}), so {built} does not identify it and an image built now \
                 records `dirty = true`. Commit the source to make the build identifiable.",
                SOURCE_PATHS.join(", ")
            )),
            Self::StampUnknown { built } => Some(format!(
                "git could not read the state of this boot2deb's sources ({}) when it was \
                 compiled, so {built} is not shown to identify it and an image built now \
                 records `dirty = true`. Rebuild it where git can read the checkout \
                 (`touch crates/cli/build.rs && cargo build`) to re-stamp it.",
                SOURCE_PATHS.join(", ")
            )),
            Self::CheckoutUnreadable { built } => Some(format!(
                "git cannot read this checkout's commit or the state of its sources ({}), \
                 so whether this boot2deb, compiled from {built}, still matches them is \
                 unknown. An image built now records {built} as its builder unchecked.",
                SOURCE_PATHS.join(", ")
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
///
/// Once that holds, a failed `git` read is
/// [`CheckoutUnreadable`](Freshness::CheckoutUnreadable) and never
/// [`Current`](Freshness::Current): the comparison is owed, and a read that failed has
/// not made it.
pub(crate) fn freshness(root: &ConfigRoot) -> Freshness {
    freshness_of(commit(), source_state(), root.path())
}

/// [`freshness`] for a binary stamped with `built` and `compiled`, against the checkout
/// at `path`. Separate so a test can stamp a scratch checkout's commit.
fn freshness_of(built: Option<&str>, compiled: SourceState, path: &Path) -> Freshness {
    let Some(built) = built else {
        return Freshness::Unknown;
    };
    if !is_source_checkout(path) {
        return Freshness::Unknown;
    }
    let Ok(head) = boot2deb_engine::git::rev_parse_head(path) else {
        return Freshness::CheckoutUnreadable {
            built: built.to_string(),
        };
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
    let built = built.to_string();
    match compiled {
        SourceState::Dirty => return Freshness::Unverifiable { built },
        SourceState::Unknown => return Freshness::StampUnknown { built },
        SourceState::Clean => {}
    }
    match current_state(path) {
        SourceState::Clean => Freshness::Current,
        SourceState::Dirty => Freshness::SourceEdited { head: built },
        SourceState::Unknown => Freshness::CheckoutUnreadable { built },
    }
}

/// How [`SOURCE_PATHS`] stand against `HEAD` in the checkout at `path` now. The same
/// two reads and the same [`SourceState::of`] rule the build script stamps with, so a
/// binary compiled from this tree as it is now would stamp this answer.
fn current_state(path: &Path) -> SourceState {
    SourceState::of(
        boot2deb_engine::git::tracked_changes(path, &SOURCE_PATHS),
        boot2deb_engine::git::untracked_files(path, &SOURCE_PATHS).as_deref(),
    )
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
    //
    // An untracked file counts too, under the paths a build reads. The loader walks
    // those directories, so a new overlay file ships in the image with no tracked change
    // to show for it. A scratch file anywhere else is not input and does not count.
    Some((
        head.chars().take(STAMP_WIDTH).collect(),
        boot2deb_engine::git::has_tracked_changes(path, &[])
            || boot2deb_engine::git::has_untracked(path, boot2deb_core::loader::BUILD_INPUT_PATHS),
    ))
}

/// Whether `path` is a boot2deb *source* checkout rather than only a config tree.
///
/// Both conditions are required. The crate manifest is what says this tree could
/// have produced the running binary; the `.git` directory is what gives it a commit to
/// be compared against. A source tree exported without its history has neither an
/// answer nor a problem.
fn is_source_checkout(path: &Path) -> bool {
    path.join("crates").join("cli").join("Cargo.toml").is_file() && path.join(".git").exists()
}

/// The identity line shared by `doctor` and the build gate: what this binary is, and
/// what that means for the images it stamps.
pub(crate) fn identity() -> String {
    match (commit(), source_state()) {
        (Some(c), SourceState::Clean) => format!("{} ({c})", version()),
        (Some(c), SourceState::Dirty) => format!("{} ({c}, dirty)", version()),
        (Some(c), SourceState::Unknown) => {
            format!("{} ({c}, source state unknown)", version())
        }
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
        // A git read that failed proves no mismatch, at compile time or now.
        assert!(!Freshness::StampUnknown {
            built: "aaaaaaaaaaaa".into(),
        }
        .is_stale());
        assert!(!Freshness::CheckoutUnreadable {
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
        // It names every path the flag covers, so an edit to Cargo.lock alone is not
        // reported as an edit under crates/.
        for path in SOURCE_PATHS {
            assert!(note.contains(path), "{note}");
        }
    }

    #[test]
    fn the_unknown_variants_say_so_without_claiming_a_mismatch() {
        let stamp = Freshness::StampUnknown {
            built: "7e6e2f02674c".into(),
        }
        .note()
        .expect("an unknown stamp is worth saying");
        assert!(stamp.contains("dirty = true"), "{stamp}");
        let now = Freshness::CheckoutUnreadable {
            built: "7e6e2f02674c".into(),
        }
        .note()
        .expect("an unread checkout is worth saying");
        assert!(now.contains("unknown"), "{now}");
        for note in [stamp, now] {
            assert!(!note.contains("checkout is at"), "{note}");
            assert!(note.contains("7e6e2f02674c"), "{note}");
        }
    }

    /// The compiled stamp asks the run-time check's question, by the one rule both
    /// include, of [`SOURCE_PATHS`] and not of the whole repo. This test binary was
    /// compiled from this checkout, so the two answers agree unless the build script got
    /// it wrong. That happens in two ways. It reads a path outside the source paths, such
    /// as a recipe or a doc page. Or a rebuild compiles a change under crates/ without
    /// re-running the script.
    #[test]
    fn the_compiled_stamp_agrees_with_the_run_time_read() {
        let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
        if commit().is_none() || !is_source_checkout(&root) {
            // Compiled outside a checkout: there is no stamp to compare.
            return;
        }
        assert_eq!(
            source_state(),
            current_state(&root),
            "BOOT2DEB_GIT_DIRTY disagrees with a run-time read of {}",
            SOURCE_PATHS.join(" ")
        );
    }

    #[test]
    fn a_stamp_reads_back_as_the_state_it_was_written_from() {
        for state in [SourceState::Clean, SourceState::Dirty, SourceState::Unknown] {
            assert_eq!(compiled_state(Some(state.stamp())), state);
        }
        // A missing or unrecognized stamp claims nothing.
        assert_eq!(compiled_state(None), SourceState::Unknown);
        assert_eq!(compiled_state(Some("")), SourceState::Unknown);
    }

    /// A read that proves the tree dirty settles it. Otherwise a failed read is unknown,
    /// never clean.
    #[test]
    fn a_failed_git_read_is_unknown_and_never_clean() {
        let new_module = ["crates/cli/src/new.rs".to_string()];
        assert_eq!(SourceState::of(Some(false), Some(&[])), SourceState::Clean);
        assert_eq!(SourceState::of(None, Some(&[])), SourceState::Unknown);
        assert_eq!(SourceState::of(Some(false), None), SourceState::Unknown);
        assert_eq!(SourceState::of(None, None), SourceState::Unknown);
        assert_eq!(SourceState::of(Some(true), None), SourceState::Dirty);
        assert_eq!(SourceState::of(None, Some(&new_module)), SourceState::Dirty);
        // A rustfmt backup is the one untracked file that is never compiled.
        let backup = ["crates/cli/src/main.rs.bk".to_string()];
        assert_eq!(
            SourceState::of(Some(false), Some(&backup)),
            SourceState::Clean
        );
    }

    /// Run git in `dir`, asserting success, and return trimmed stdout.
    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("spawn git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// A scratch boot2deb source checkout at one commit, and that commit at the stamp's
    /// width. Its `.gitignore` ignores `target/` at the root, as the real one does, and
    /// `*.gen.rs` anywhere, to stand for an ignored file under the source paths.
    fn source_checkout() -> (tempfile::TempDir, String) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path();
        std::fs::create_dir_all(repo.join("crates/cli/src")).unwrap();
        std::fs::write(repo.join("crates/cli/Cargo.toml"), "[package]\n").unwrap();
        std::fs::write(repo.join("crates/cli/src/main.rs"), "mod builder;\n").unwrap();
        std::fs::write(repo.join("Cargo.toml"), "[workspace]\n").unwrap();
        std::fs::write(repo.join(".gitignore"), "/target\n*.gen.rs\n**/*.rs.bk\n").unwrap();
        git(repo, &["init", "-q"]);
        git(repo, &["config", "user.email", "t@boot2deb"]);
        git(repo, &["config", "user.name", "t"]);
        git(repo, &["add", "."]);
        git(repo, &["commit", "-qm", "one"]);
        let head = git(
            repo,
            &["rev-parse", &format!("--short={STAMP_WIDTH}"), "HEAD"],
        );
        (tmp, head)
    }

    /// A module a tracked `mod` declares compiles from the filesystem before anyone
    /// stages it, and so does an ignored file under the source paths. Neither is in the
    /// commit, so either one makes the tree dirty. Files outside the source paths,
    /// ignored build output included, do not.
    #[test]
    fn an_untracked_or_ignored_source_file_makes_the_tree_dirty_and_others_do_not() {
        let (tmp, head) = source_checkout();
        let repo = tmp.path();
        assert_eq!(current_state(repo), SourceState::Clean);
        assert_eq!(
            freshness_of(Some(&head), SourceState::Clean, repo),
            Freshness::Current
        );

        // A scratch file elsewhere, an ignored build directory at the root, and a
        // rustfmt backup beside a module.
        std::fs::write(repo.join("notes.txt"), "x").unwrap();
        std::fs::create_dir_all(repo.join("target/debug")).unwrap();
        std::fs::write(repo.join("target/debug/boot2deb"), "x").unwrap();
        std::fs::write(repo.join("crates/cli/src/main.rs.bk"), "x").unwrap();
        assert_eq!(current_state(repo), SourceState::Clean);

        // An ignored file under the source paths: an ignore rule is no promise that
        // nothing compiles it.
        std::fs::write(repo.join("crates/cli/src/table.gen.rs"), "x").unwrap();
        assert_eq!(current_state(repo), SourceState::Dirty);
        std::fs::remove_file(repo.join("crates/cli/src/table.gen.rs")).unwrap();
        assert_eq!(current_state(repo), SourceState::Clean);

        // The module `main.rs` declares, not yet staged.
        std::fs::write(repo.join("crates/cli/src/builder.rs"), "x").unwrap();
        assert_eq!(current_state(repo), SourceState::Dirty);
        assert_eq!(
            freshness_of(Some(&head), SourceState::Clean, repo),
            Freshness::SourceEdited { head: head.clone() }
        );
    }

    #[test]
    fn the_compiled_state_decides_before_the_checkout_is_read() {
        let (tmp, head) = source_checkout();
        let repo = tmp.path();
        assert_eq!(
            freshness_of(Some(&head), SourceState::Dirty, repo),
            Freshness::Unverifiable {
                built: head.clone()
            }
        );
        assert_eq!(
            freshness_of(Some(&head), SourceState::Unknown, repo),
            Freshness::StampUnknown {
                built: head.clone()
            }
        );
        // A binary with no commit is not compared at all.
        assert_eq!(
            freshness_of(None, SourceState::Unknown, repo),
            Freshness::Unknown
        );
    }

    /// A source checkout whose `.git` git cannot read is owed a comparison it cannot
    /// have. That is reported, and never passes as current.
    #[test]
    fn a_source_checkout_git_cannot_read_is_unreadable_and_never_current() {
        let tmp = tempfile::tempdir().expect("tempdir");
        let repo = tmp.path();
        std::fs::create_dir_all(repo.join("crates/cli")).unwrap();
        std::fs::write(repo.join("crates/cli/Cargo.toml"), "[package]\n").unwrap();
        std::fs::write(repo.join(".git"), "").unwrap();
        assert!(is_source_checkout(repo));
        assert_eq!(current_state(repo), SourceState::Unknown);
        assert_eq!(
            freshness_of(Some("7e6e2f02674c"), SourceState::Clean, repo),
            Freshness::CheckoutUnreadable {
                built: "7e6e2f02674c".into()
            }
        );
    }

    #[test]
    fn a_tree_without_the_crate_manifest_is_not_a_source_checkout() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert!(!is_source_checkout(dir.path()));
    }
}
