//! Build script: stamp the boot2deb git commit and dirty flag into the binary as
//! compile-time env vars (`BOOT2DEB_GIT_COMMIT`, `BOOT2DEB_GIT_DIRTY`), so a built
//! image's provenance manifest records which boot2deb checkout produced it. Absent a
//! git checkout (e.g. a source tarball) the commit is emitted empty and the crate
//! version alone identifies the builder.

use std::process::Command;

fn main() {
    // Re-stamp when HEAD moves (a new commit/checkout) or the index changes (staging),
    // so an incremental rebuild reflects the current checkout. The crate dir is
    // crates/cli; the repo's .git sits two levels up. A path that does not exist (a
    // tarball with no .git) is simply not watched.
    for marker in ["../../.git/HEAD", "../../.git/index"] {
        println!("cargo:rerun-if-changed={marker}");
    }

    let commit = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_default();
    // Untracked files do not change the build output, so "dirty" is tracked content
    // differing from HEAD (`git diff`), not `git status`. Unknown without a commit.
    let dirty = !commit.is_empty()
        && !Command::new("git")
            .args(["diff", "--quiet", "HEAD"])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(true);

    println!("cargo:rustc-env=BOOT2DEB_GIT_COMMIT={commit}");
    println!("cargo:rustc-env=BOOT2DEB_GIT_DIRTY={dirty}");
}

/// Run `git <args>` and return trimmed stdout, or `None` if git is absent, errors, or
/// prints nothing (e.g. the build tree is not a git checkout).
fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    (!s.is_empty()).then_some(s)
}
