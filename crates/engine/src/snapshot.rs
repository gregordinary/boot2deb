//! Snapshot activation and capture: the ordered mirror list each pocket of a
//! bootstrap fetches from, and the timestamp `--save-snapshot` stamps into the lock.
//!
//! The solved-manifest content pin already fixes *which bytes* a build installs. The
//! snapshot is purely an availability backstop for when those exact versions rotate
//! off the live mirror. So it is dormant by default (`mode = off`) and never on the
//! hot path. [`pocket_mirrors`] returns just the live mirror until a mode explicitly
//! activates it.
//!
//! Where the point-in-time archive lives is a fact of the distribution, stated by its
//! [distro layer](boot2deb_core::distro::DistroLayer). One timestamp selects the
//! same moment in every pocket's archive. A pinned build therefore cannot mix a
//! security pocket from one day with a release pocket from another.
//!
//! Pure: the URL and timestamp formatting and the mode-to-mirror-list mapping do no
//! I/O and read no clock (the caller passes `SystemTime::now()` into
//! [`format_timestamp`]), so all of it is unit-testable.

use crate::error::EngineError;
use boot2deb_core::distro::Pocket;
use boot2deb_core::lock::{SnapshotMode, SnapshotPin};

/// The point-in-time mirror URL for a captured `timestamp` under a distribution's
/// snapshot archive `base`.
pub fn snapshot_mirror(base: &str, timestamp: &str) -> String {
    format!("{base}/{timestamp}/")
}

/// The ordered mirror list `pocket` is fetched from, honoring the active snapshot
/// mode.
///
/// The parameters:
///
/// - `pocket` contains the live mirror and the base of its point-in-time archive.
/// - `snapshot` is the lock's captured pin, if any.
/// - `mode` is the effective mode: a `--snapshot` override, else the lock's captured
///   mode.
///
/// The modes:
///
/// - [`Off`](SnapshotMode::Off) (or no snapshot): the live mirror only — the
///   snapshot stays provenance and never touches the build.
/// - [`Fallback`](SnapshotMode::Fallback): live mirror first, snapshot second, so
///   apt fills a 404 (a version that rotated off the live mirror) from the snapshot.
/// - [`Pin`](SnapshotMode::Pin): the snapshot only — a fully deterministic userland.
///
/// A `Fallback`/`Pin` mode with no captured timestamp is
/// [`SnapshotUnavailable`](EngineError::SnapshotUnavailable), and one for a pocket
/// whose distribution has no snapshot archive is
/// [`NoSnapshotArchive`](EngineError::NoSnapshotArchive). There is nothing to fetch
/// from in either case, so the request is refused rather than silently downgraded to
/// live.
pub fn pocket_mirrors(
    pocket: &Pocket,
    snapshot: Option<&SnapshotPin>,
    mode: SnapshotMode,
) -> Result<Vec<String>, EngineError> {
    if mode == SnapshotMode::Off {
        return Ok(vec![pocket.mirror.clone()]);
    }
    let ts = snapshot
        .map(|s| s.timestamp.as_str())
        .ok_or(EngineError::SnapshotUnavailable {
            mode: mode.as_str(),
        })?;
    let base = pocket
        .snapshot
        .as_deref()
        .ok_or_else(|| EngineError::NoSnapshotArchive {
            suite: pocket.suite.clone(),
        })?;
    let snap = snapshot_mirror(base, ts);
    Ok(match mode {
        SnapshotMode::Pin => vec![snap],
        // Fallback: live first, snapshot backfills 404s.
        _ => vec![pocket.mirror.clone(), snap],
    })
}

/// Format a Unix timestamp (whole seconds) as a snapshot timestamp
/// `YYYYMMDDTHHMMSSZ` in UTC, the form `snapshot.debian.org` and `snapshot.ubuntu.com`
/// both take. `--save-snapshot` passes `SystemTime::now()` through
/// here, so the captured timestamp names a snapshot that contains the versions the
/// build just solved.
///
/// The calendar conversion is
/// [`boot2deb_core::datetime`](boot2deb_core::datetime::format_compact) — shared with
/// the SBOM's RFC 3339 stamp, so the two cannot disagree about what a Unix second is.
pub fn format_timestamp(unix_secs: u64) -> String {
    boot2deb_core::datetime::format_compact(unix_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin(ts: &str, mode: SnapshotMode) -> SnapshotPin {
        SnapshotPin {
            timestamp: ts.to_string(),
            mode,
        }
    }

    fn pocket(snapshot: Option<&str>) -> Pocket {
        Pocket {
            suite: "forky".into(),
            mirror: "http://deb.debian.org/debian".into(),
            snapshot: snapshot.map(str::to_string),
        }
    }

    const SNAPSHOT: &str = "https://snapshot.debian.org/archive/debian";

    #[test]
    fn off_or_absent_uses_live_mirror_only() {
        let live = vec!["http://deb.debian.org/debian".to_string()];
        let p = pocket(Some(SNAPSHOT));
        // No captured snapshot, mode off: live only.
        assert_eq!(pocket_mirrors(&p, None, SnapshotMode::Off).unwrap(), live);
        // A captured snapshot in off mode stays provenance — still live only.
        let captured = pin("20260628T083000Z", SnapshotMode::Off);
        assert_eq!(
            pocket_mirrors(&p, Some(&captured), SnapshotMode::Off).unwrap(),
            live
        );
        // A distribution with no snapshot archive is fine as long as none is asked for.
        assert_eq!(
            pocket_mirrors(&pocket(None), None, SnapshotMode::Off).unwrap(),
            live
        );
    }

    #[test]
    fn fallback_is_live_then_snapshot() {
        let captured = pin("20260628T083000Z", SnapshotMode::Fallback);
        let mirrors = pocket_mirrors(
            &pocket(Some(SNAPSHOT)),
            Some(&captured),
            SnapshotMode::Fallback,
        )
        .unwrap();
        assert_eq!(
            mirrors,
            vec![
                "http://deb.debian.org/debian".to_string(),
                "https://snapshot.debian.org/archive/debian/20260628T083000Z/".to_string(),
            ]
        );
    }

    #[test]
    fn pin_is_snapshot_only() {
        let captured = pin("20260628T083000Z", SnapshotMode::Pin);
        let mirrors =
            pocket_mirrors(&pocket(Some(SNAPSHOT)), Some(&captured), SnapshotMode::Pin).unwrap();
        assert_eq!(
            mirrors,
            vec!["https://snapshot.debian.org/archive/debian/20260628T083000Z/".to_string()]
        );
    }

    #[test]
    fn snapshot_mode_without_capture_is_an_error() {
        for mode in [SnapshotMode::Fallback, SnapshotMode::Pin] {
            match pocket_mirrors(&pocket(Some(SNAPSHOT)), None, mode) {
                Err(EngineError::SnapshotUnavailable { mode: m }) => {
                    assert_eq!(m, mode.as_str())
                }
                other => panic!("expected SnapshotUnavailable, got {other:?}"),
            }
        }
    }

    #[test]
    fn snapshot_mode_without_an_archive_to_take_it_from_is_an_error() {
        let captured = pin("20260628T083000Z", SnapshotMode::Pin);
        for mode in [SnapshotMode::Fallback, SnapshotMode::Pin] {
            match pocket_mirrors(&pocket(None), Some(&captured), mode) {
                Err(EngineError::NoSnapshotArchive { suite }) => assert_eq!(suite, "forky"),
                other => panic!("expected NoSnapshotArchive, got {other:?}"),
            }
        }
    }

    #[test]
    fn timestamp_formats_utc_civil_date() {
        // Unix epoch.
        assert_eq!(format_timestamp(0), "19700101T000000Z");
        // 2026-07-03T12:34:56Z — a known instant (verified against date -u).
        assert_eq!(format_timestamp(1_783_082_096), "20260703T123456Z");
        // Leap-day boundary: 2024-02-29T00:00:00Z.
        assert_eq!(format_timestamp(1_709_164_800), "20240229T000000Z");
    }
}
