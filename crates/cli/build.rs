//! Build script: stamp the boot2deb git commit and the state of its sources into the
//! binary as compile-time env vars (`BOOT2DEB_GIT_COMMIT`, `BOOT2DEB_GIT_DIRTY`), so a
//! built image's provenance manifest records which boot2deb checkout produced it. Absent
//! a git checkout of its own (a source tarball, even one unpacked inside another
//! repository) the commit is emitted empty and the crate version alone identifies the
//! builder.

use std::path::{Path, PathBuf};
use std::process::Command;

include!("source_paths.rs");

fn main() {
    // Re-stamp whenever HEAD moves, so an incremental rebuild reflects the current
    // checkout. The crate dir is crates/cli; the repo's .git sits two levels up.
    //
    // Cargo reads a watched path that does not exist as changed, so where these are
    // absent it re-runs this script, and recompiles the crate, on every build. That
    // happens in a source tarball with no .git, and in a linked worktree, whose .git is
    // a file. The stamp stays correct in both, which is what matters; watching only the
    // markers that exist would leave a worktree's stamp at whatever commit it first saw.
    //
    // The reflog is the essential one: `.git/HEAD`'s *contents* change only on a
    // branch switch — an ordinary commit moves `refs/heads/<branch>` and leaves HEAD
    // naming the same ref — while `.git/logs/HEAD` gains a line on every commit, amend,
    // checkout and reset. The index is watched too, since staging alone changes what
    // `git diff HEAD` reports and so flips the dirty flag below.
    for marker in [
        "../../.git/logs/HEAD",
        "../../.git/HEAD",
        "../../.git/index",
    ] {
        println!("cargo:rerun-if-changed={marker}");
    }
    // Re-stamp on every edit to the sources as well. Cargo scans a directory
    // recursively, so a change anywhere under crates/, a new file included, re-runs
    // this script. Without it the rebuild that compiles an edit keeps the dirty answer
    // from the last HEAD move, and the binary claims a clean tree it was not built from.
    for path in SOURCE_PATHS {
        println!("cargo:rerun-if-changed=../../{path}");
    }

    let root = workspace_root();
    // Only this checkout's own repository names this source. `git` walks upward to the
    // nearest `.git`, so a source tree unpacked inside some other checkout would
    // otherwise be stamped with that checkout's commit and dirty state. `.git` is a
    // directory in a clone and a file in a linked worktree, and both are this tree's own.
    let commit = if root.join(".git").exists() {
        git(&root, &["rev-parse", "--short=12", "HEAD"]).unwrap_or_default()
    } else {
        String::new()
    };
    // The state of SOURCE_PATHS against that commit, by the rule `SourceState::of`
    // states: a tracked change, or any file git does not track, ignored or not, makes the
    // tree dirty. A git read that fails stamps `unknown`, never `false`. Unknown without
    // a commit, where there is nothing to stand against. An edit outside SOURCE_PATHS,
    // such as a recipe or a doc page, is build input, which the config stamp records.
    let state = if commit.is_empty() {
        SourceState::Unknown
    } else {
        SourceState::of(tracked_changes(&root), untracked_files(&root).as_deref())
    };

    println!("cargo:rustc-env=BOOT2DEB_GIT_COMMIT={commit}");
    println!("cargo:rustc-env=BOOT2DEB_GIT_DIRTY={}", state.stamp());
}

/// The workspace root, two levels above this crate. `SOURCE_PATHS` are relative to it,
/// and `git diff` resolves a pathspec against its working directory, so the diff runs
/// from here rather than from the crate directory Cargo starts the script in.
fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// Run `git <args>` in `dir` and return trimmed stdout, or `None` if git is absent,
/// errors, or prints nothing (e.g. the build tree is not a git checkout).
fn git(dir: &Path, args: &[&str]) -> Option<String> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}

/// Whether tracked content under SOURCE_PATHS differs from HEAD, staged or not.
/// `git diff --quiet` exits 1 for differences and 0 for none. Anything else, such as
/// 128 for "not a repository" or a git that will not spawn, is `None`.
fn tracked_changes(root: &Path) -> Option<bool> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["diff", "--quiet", "HEAD", "--"])
        .args(SOURCE_PATHS)
        .output()
        .ok()?;
    match out.status.code() {
        Some(0) => Some(false),
        Some(1) => Some(true),
        _ => None,
    }
}

/// Every file under SOURCE_PATHS that git does not track, ignored ones included, or
/// `None` when git fails.
fn untracked_files(root: &Path) -> Option<Vec<String>> {
    let out = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["ls-files", "--others", "-z", "--"])
        .args(SOURCE_PATHS)
        .output()
        .ok()?;
    out.status.success().then(|| {
        out.stdout
            .split(|&b| b == 0)
            .filter(|f| !f.is_empty())
            .map(|f| String::from_utf8_lossy(f).into_owned())
            .collect()
    })
}
