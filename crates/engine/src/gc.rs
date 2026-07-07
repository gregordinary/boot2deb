//! Opportunistic garbage collection of stale partial-publish temporaries (ATOM-3).
//!
//! Every atomic publish in the engine stages into a uniquely-named sibling temp and
//! renames it into place, so a present entry is always complete: the
//! [artifact store](crate::artstore) and [rootfs store](crate::rootcache) use
//! `<key>.partial` dirs, the [deb store](crate::debstore) and
//! [`stage_artifact`](crate::build) use `.<name>.<pid>.partial` files, the rootfs
//! password splice uses `.<name>.<pid>.splice.partial`, and the
//! [patch fetch cache](crate::patchfetch) uses `.fetch-*` staging dirs. A hard kill
//! (SIGKILL, power loss) between stage and rename leaves that temp behind. It is
//! harmless to correctness — hit checks require the *final* entry, never a temp — but
//! it accumulates as disk clutter, since the durable stores under `<root>/cache`
//! survive `clean`.
//!
//! [`sweep_stale_temps`] removes those leftovers best-effort at store-open and
//! build-start. A temp is deleted only when it is both name-matched and older than
//! `STALE_AGE`, so an in-flight temp from a concurrent build is never disturbed.

use std::path::Path;
use std::time::{Duration, SystemTime};

/// Minimum age before a partial temp is treated as abandoned. Comfortably longer than
/// any single publish — the slowest are a full patch-repo clone and a multi-GB
/// rootfs-tar copy, both minutes, not hours — so a concurrent build's live temp is
/// never swept out from under it.
const STALE_AGE: Duration = Duration::from_secs(6 * 3600);

/// True if `name` is one of the engine's partial-publish temp names: any `.partial`
/// sibling (the stores, `stage_artifact`, and the password splice) or a `.fetch-*`
/// patch-clone staging dir. Real stored artifacts (`*.deb`, `rootfs.tar`,
/// `manifest.toml`, …) do not match, so a live entry is never a sweep candidate.
fn is_temp_name(name: &str) -> bool {
    name.contains(".partial") || name.starts_with(".fetch-")
}

/// Remove partial-publish temps under `dir` older than `STALE_AGE`, and one level
/// deeper (the artifact store keys its temps under per-node subdirs), best-effort
/// (ATOM-3).
///
/// Never fails and never logs: a sweep error (a permission issue, or a temp a
/// concurrent build is mid-rename on) is ignored — GC is opportunistic, and the
/// conservative hit checks mean any temp that survives is only clutter. Non-temp
/// subdirectories are descended one level so a stale `<node>/.sig.pid.partial` is
/// reached; their own non-temp contents are left untouched.
pub fn sweep_stale_temps(dir: &Path) {
    sweep_dir(dir, STALE_AGE, SystemTime::now(), true);
}

/// The age-parameterized core of [`sweep_stale_temps`]. `descend` sweeps one level of
/// non-temp subdirectories (the artifact store's per-node layout); recursion stops
/// there so the walk is bounded.
fn sweep_dir(dir: &Path, min_age: Duration, now: SystemTime, descend: bool) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_temp_name(name) {
            if is_stale(&entry, min_age, now) {
                let path = entry.path();
                let _ = if is_dir {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
            }
        } else if descend && is_dir {
            sweep_dir(&entry.path(), min_age, now, false);
        }
    }
}

/// Whether `entry`'s last-modified time is at least `min_age` before `now`. An
/// unreadable mtime is treated as *not* stale (leave it alone rather than risk
/// removing a live temp).
fn is_stale(entry: &std::fs::DirEntry, min_age: Duration, now: SystemTime) -> bool {
    entry
        .metadata()
        .and_then(|m| m.modified())
        .ok()
        .and_then(|mtime| now.duration_since(mtime).ok())
        .is_some_and(|age| age >= min_age)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweeps_matching_temps_and_keeps_real_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        // Temps of each shape the engine produces, plus real entries and a nested
        // per-node temp (the artifact-store layout).
        std::fs::write(dir.join(".linux-image_arm64.deb.123.partial"), b"x").unwrap();
        std::fs::write(dir.join(".rootfs.tar.9.splice.partial"), b"x").unwrap();
        std::fs::create_dir(dir.join("abc123.partial")).unwrap();
        std::fs::create_dir(dir.join(".fetch-XYZ")).unwrap();
        std::fs::write(dir.join("linux-image_arm64.deb"), b"real").unwrap();
        std::fs::create_dir(dir.join("kernel")).unwrap();
        std::fs::write(dir.join("kernel").join(".sig.5.partial"), b"x").unwrap();
        std::fs::write(dir.join("kernel").join("real.deb"), b"real").unwrap();

        // min_age 0 → everything old enough; real entries must survive.
        sweep_dir(dir, Duration::ZERO, SystemTime::now(), true);

        assert!(!dir.join(".linux-image_arm64.deb.123.partial").exists());
        assert!(!dir.join(".rootfs.tar.9.splice.partial").exists());
        assert!(!dir.join("abc123.partial").exists());
        assert!(!dir.join(".fetch-XYZ").exists());
        assert!(!dir.join("kernel").join(".sig.5.partial").exists());
        assert!(dir.join("linux-image_arm64.deb").exists(), "real deb kept");
        assert!(dir.join("kernel").join("real.deb").exists(), "nested real deb kept");
    }

    #[test]
    fn keeps_temps_younger_than_the_threshold() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::write(dir.join(".fresh.7.partial"), b"x").unwrap();
        // A just-created temp is younger than a one-day threshold → left in place, so a
        // concurrent build's in-flight temp is never swept.
        sweep_dir(dir, Duration::from_secs(86_400), SystemTime::now(), true);
        assert!(dir.join(".fresh.7.partial").exists());
    }
}
