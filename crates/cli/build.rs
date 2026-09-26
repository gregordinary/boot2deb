//! Build script: stamp the boot2deb git commit and dirty flag into the binary as
//! compile-time env vars (`BOOT2DEB_GIT_COMMIT`, `BOOT2DEB_GIT_DIRTY`), so a built
//! image's provenance manifest records which boot2deb checkout produced it. Absent a
//! git checkout (e.g. a source tarball) the commit is emitted empty and the crate
//! version alone identifies the builder.

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
    // The reflog is the load-bearing one: `.git/HEAD`'s *contents* change only on a
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
    // recursively, so a change anywhere under crates/ re-runs this script. Without it
    // the rebuild that compiles an edit keeps the dirty answer from the last HEAD move,
    // and the binary claims a clean tree it was not built from.
    for path in SOURCE_PATHS {
        println!("cargo:rerun-if-changed=../../{path}");
    }

    let root = workspace_root();
    let commit = git(&root, &["rev-parse", "--short=12", "HEAD"]).unwrap_or_default();
    // "Dirty" is tracked content under SOURCE_PATHS differing from HEAD (`git diff`),
    // staged or not. Untracked files do not change the build output, and neither does
    // an edit outside SOURCE_PATHS: a recipe or a doc page is build input, which the
    // config stamp records. Unknown without a commit.
    let dirty = !commit.is_empty()
        && Command::new("git")
            .arg("-C")
            .arg(&root)
            .args(["diff", "--quiet", "HEAD", "--"])
            .args(SOURCE_PATHS)
            .output()
            .is_ok_and(|o| o.status.code() == Some(1));

    println!("cargo:rustc-env=BOOT2DEB_GIT_COMMIT={commit}");
    println!("cargo:rustc-env=BOOT2DEB_GIT_DIRTY={dirty}");
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
