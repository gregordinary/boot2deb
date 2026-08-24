//! Filesystem helpers shared by the command handlers: path absolutization and
//! lexical normalization, the guarded scaffold write, and directory-size accounting
//! for `clean`.

use std::path::{Component, Path, PathBuf};

/// Make `path` absolute (against the current dir) if it is relative, so it is
/// safe to use as a sandbox bind source and working directory (the cage exposes
/// each bind at its host path). Falls back to the input if the current dir is
/// unreadable.
pub(crate) fn absolutize(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&path))
            .unwrap_or(path)
    }
}

/// Lexically normalize `path`: drop `.` components and cancel each `..` against the
/// component before it, without touching the filesystem.
///
/// For paths built by joining onto `--root` (whose default is the literal `.`), the
/// unnormalized form is correct but unreadable — `<cwd>/./../patches` names the right
/// directory and tells an operator nothing. Every such path is shown in an error
/// message or a next-step hint, so it is normalized before it is used.
///
/// Lexical, so a `..` that crosses a symlinked component resolves to the parent of
/// the *link*, not of its target. That is the intended meaning here ("the sibling of
/// the config root"), and it needs no existing path to compute.
pub(crate) fn normalize(path: PathBuf) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => match out.components().next_back() {
                Some(Component::Normal(_)) => {
                    out.pop();
                }
                // A leading `..`, or one following another, has nothing to cancel
                // against and is kept — dropping it would name a different directory.
                Some(Component::ParentDir) | None => out.push(component),
                // The root's parent is the root, so `/..` collapses to `/`.
                Some(_) => {}
            },
            other => out.push(other),
        }
    }
    if out.as_os_str().is_empty() {
        out.push(".");
    }
    out
}

/// Write a scaffolded file, creating its parent directory. Refuses to clobber an
/// existing file unless `force`, so a re-run never silently overwrites hand-edits.
pub(crate) fn write_scaffold_file(
    path: &Path,
    contents: &str,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if path.exists() && !force {
        return Err(format!(
            "{} already exists — pass --force to overwrite",
            path.display()
        )
        .into());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, contents)?;
    Ok(())
}

/// Total size in bytes of a directory tree, following no symlinks (counts the link,
/// not its target). Best-effort: an unreadable entry contributes nothing rather
/// than failing the whole size estimate.
pub(crate) fn dir_size(path: &Path) -> u64 {
    let meta = match std::fs::symlink_metadata(path) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    if meta.is_dir() {
        match std::fs::read_dir(path) {
            Ok(entries) => entries.flatten().map(|e| dir_size(&e.path())).sum(),
            Err(_) => 0,
        }
    } else {
        meta.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn write_scaffold_file_refuses_to_clobber_without_force() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("devices/board.toml");
        // Creates the parent directory on the way.
        write_scaffold_file(&path, "first\n", false).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\n");
        // A second write without --force leaves the hand-edited file intact.
        let err = write_scaffold_file(&path, "second\n", false)
            .unwrap_err()
            .to_string();
        assert!(err.contains("--force"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\n");
        // With --force it is overwritten.
        write_scaffold_file(&path, "second\n", true).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second\n");
    }

    #[test]
    fn normalize_cancels_dot_and_parent_components() {
        let n = |s: &str| normalize(PathBuf::from(s)).display().to_string();
        // The shape this exists for: `--root .` joined with `../patches`.
        assert_eq!(n("/home/dev/boot2deb/./../patches"), "/home/dev/patches");
        assert_eq!(n("/a/b/../c"), "/a/c");
        assert_eq!(n("./a/b"), "a/b");
        // A `..` with nothing to cancel against is kept: dropping it would name a
        // different directory.
        assert_eq!(n("../../a"), "../../a");
        assert_eq!(n("a/../../b"), "../b");
        // The root has no parent.
        assert_eq!(n("/../a"), "/a");
        // A path that cancels to nothing is the current directory, not the empty path.
        assert_eq!(n("a/.."), ".");
    }

    #[test]
    fn dir_size_sums_a_tree_and_tolerates_an_absent_path() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(tmp.path().join("sub")).unwrap();
        std::fs::write(tmp.path().join("a"), vec![0u8; 100]).unwrap();
        std::fs::write(tmp.path().join("sub/b"), vec![0u8; 23]).unwrap();
        // Directory entries themselves have a size, so the tree is at least the files.
        assert!(dir_size(tmp.path()) >= 123);
        assert_eq!(dir_size(&tmp.path().join("missing")), 0);
    }
}
