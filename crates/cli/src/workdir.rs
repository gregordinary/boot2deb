//! The build scratch dir's ownership stamp.
//!
//! `clean --work-dir <path>` removes a tree recursively, so it must be able to
//! prove the target is boot2deb's own scratch and not, say, a mistyped path into
//! the user's home. `build` stamps every work dir it creates with a marker file;
//! `clean` refuses to remove an unstamped one unless forced.

use std::path::Path;

/// The marker file [`mark_work_dir`] stamps into every work dir `build` creates
/// and [`check_work_dir_removable`] requires before `clean` removes one.
/// Its presence means "boot2deb created this scratch tree," so `clean` can prove
/// a removal target is its own rather than an arbitrary `--work-dir` typo.
pub(crate) const WORK_DIR_MARKER: &str = ".boot2deb-work";

/// Create `work_dir` (if needed) and stamp it with the [`WORK_DIR_MARKER`], so a
/// later `clean` recognizes it as boot2deb-owned.
pub(crate) fn mark_work_dir(work_dir: &Path) -> Result<(), Box<dyn std::error::Error>> {
    std::fs::create_dir_all(work_dir)
        .map_err(|e| format!("failed to create {}: {e}", work_dir.display()))?;
    let marker = work_dir.join(WORK_DIR_MARKER);
    if !marker.exists() {
        std::fs::write(&marker, "boot2deb work dir; `boot2deb clean` may remove this tree\n")
            .map_err(|e| format!("failed to write {}: {e}", marker.display()))?;
    }
    Ok(())
}

/// The removal guard: `Ok` when `clean` may remove (within) `work_dir` —
/// it is stamped with the [`WORK_DIR_MARKER`], the caller forced it, or it does
/// not exist (the removal loop then just reports it absent). `Err` carries the
/// refusal message.
pub(crate) fn check_work_dir_removable(work_dir: &Path, force: bool) -> Result<(), String> {
    if force || !work_dir.exists() || work_dir.join(WORK_DIR_MARKER).exists() {
        return Ok(());
    }
    Err(format!(
        "refusing to remove {}: not stamped as a boot2deb work dir (no {WORK_DIR_MARKER} marker); \
         re-check --work-dir, or pass --force to remove it anyway",
        work_dir.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_guard_requires_the_ownership_stamp() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("not-a-work-dir");
        std::fs::create_dir_all(&dir).unwrap();
        // An unmarked existing directory is refused: a mistyped
        // --work-dir must not become a recursive delete.
        let err = check_work_dir_removable(&dir, false).unwrap_err();
        assert!(err.contains("refusing to remove"), "{err}");
        assert!(err.contains(WORK_DIR_MARKER), "{err}");
        // --force overrides.
        assert!(check_work_dir_removable(&dir, true).is_ok());
        // A dir `build` stamped is removable without force.
        mark_work_dir(&dir).unwrap();
        assert!(check_work_dir_removable(&dir, false).is_ok());
        // An absent dir passes; the removal loop reports it absent and skips.
        assert!(check_work_dir_removable(&tmp.path().join("missing"), false).is_ok());
    }

    #[test]
    fn mark_work_dir_creates_and_stamps_idempotently() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("work");
        // Creates the directory and the marker in one step...
        mark_work_dir(&dir).unwrap();
        assert!(dir.join(WORK_DIR_MARKER).is_file());
        // ...and re-stamping an already-marked dir is a no-op, not an error.
        mark_work_dir(&dir).unwrap();
        assert!(dir.join(WORK_DIR_MARKER).is_file());
    }
}
