// The binary's source paths and the rule that reads their state, `include!`d by both the
// build script and `src/builder.rs`. One list and one rule serve the compile-time dirty
// stamp and the run-time freshness check, so the two cannot disagree about which changes
// alter what the binary does. Each side runs its own two `git` reads, since a build
// script cannot link the engine, and hands the answers to [`SourceState::of`].

/// Paths whose content decides what the compiled binary does, relative to the workspace
/// root. A change anywhere else in the repo — a device `.toml`, a `.dts`, a doc page — is
/// build *input*, recorded by the lock and the config stamp, and leaves the binary's
/// identity intact.
///
/// Every compile-time input of the workspace lies under these: the crates' sources and
/// every file they `include_str!`, `include_bytes!` or `include!`, and the manifests and
/// lock that choose the dependencies.
pub(crate) const SOURCE_PATHS: [&str; 3] = ["crates", "Cargo.toml", "Cargo.lock"];

/// Suffixes of untracked files under [`SOURCE_PATHS`] that are not compile input:
/// `rustfmt --backup` copies (`*.rs.bk`). Nothing compiles a file by that name, and the
/// repository ignores it.
const NOT_COMPILED: [&str; 1] = [".rs.bk"];

/// How [`SOURCE_PATHS`] stand against `HEAD`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SourceState {
    /// Nothing under the source paths differs from `HEAD`, so the commit identifies
    /// the source.
    Clean,
    /// Something under them does. A tracked file differs from `HEAD`, staged or not, or
    /// a file git does not track is present.
    Dirty,
    /// Git could not answer: it is absent, failed, or found no repository where the
    /// checkout's `.git` is. Never read as clean, since a read that failed proves
    /// nothing about the tree.
    Unknown,
}

impl SourceState {
    /// Combine the two `git` reads into one state.
    ///
    /// `tracked` is the answer of `git diff --quiet HEAD -- <SOURCE_PATHS>`: whether
    /// tracked content differs, or `None` when git failed. `untracked` is the listing of
    /// `git ls-files --others -- <SOURCE_PATHS>`, or `None` when git failed.
    ///
    /// The listing deliberately includes ignored files. A module or an included script
    /// compiles from the filesystem whether or not it is staged, and an ignore rule says
    /// what is not committed, never what is not compiled. So every file git does not track
    /// under the source paths counts, except the [`NOT_COMPILED`] suffixes. Build output
    /// does not reach the rule, because `target/` sits at the workspace root, outside the
    /// source paths.
    ///
    /// A read that proves the tree dirty settles the answer even when the other read
    /// failed. Otherwise any failed read leaves the state `Unknown`.
    pub(crate) fn of(tracked: Option<bool>, untracked: Option<&[String]>) -> Self {
        let untracked = untracked.map(|files| {
            files
                .iter()
                .any(|f| !NOT_COMPILED.iter().any(|suffix| f.ends_with(suffix)))
        });
        match (tracked, untracked) {
            (Some(true), _) | (_, Some(true)) => Self::Dirty,
            (Some(false), Some(false)) => Self::Clean,
            _ => Self::Unknown,
        }
    }

    /// The value `BOOT2DEB_GIT_DIRTY` holds for this state: `false`, `true` or `unknown`.
    pub(crate) fn stamp(self) -> &'static str {
        match self {
            Self::Clean => "false",
            Self::Dirty => "true",
            Self::Unknown => "unknown",
        }
    }
}
