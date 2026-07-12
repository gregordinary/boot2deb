//! Filesystem helpers shared by the command handlers: path absolutization, the
//! guarded scaffold write, and directory-size accounting for `clean`.

use std::path::{Path, PathBuf};

/// Make `path` absolute (against the current dir) if it is relative, so it is
/// safe to hand to `bwrap --bind`/`--chdir` inside the sandbox namespace. Falls
/// back to the input if the current dir is unreadable.
pub(crate) fn absolutize(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        path
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(&path))
            .unwrap_or(path)
    }
}

/// Write a scaffolded file, creating its parent directory. Refuses to clobber an
/// existing file unless `force`, so a re-run never silently overwrites hand-edits.
pub(crate) fn write_scaffold_file(
    path: &Path,
    contents: &str,
    force: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if path.exists() && !force {
        return Err(format!("{} already exists — pass --force to overwrite", path.display()).into());
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
        let err = write_scaffold_file(&path, "second\n", false).unwrap_err().to_string();
        assert!(err.contains("--force"), "{err}");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "first\n");
        // With --force it is overwritten.
        write_scaffold_file(&path, "second\n", true).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "second\n");
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
