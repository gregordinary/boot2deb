//! Snapshot activation + capture: the ordered mirror list a rootfs
//! bootstrap fetches from, and the `snapshot.debian.org` timestamp
//! `--save-snapshot` stamps into the lock.
//!
//! The solved-manifest content pin already fixes *which bytes* a build
//! installs; the snapshot is purely an availability backstop for when those exact
//! versions rotate off the live mirror. So it is dormant by default (`mode = off`)
//! and never on the hot path — [`resolve_mirrors`] returns just the live mirror
//! until a mode explicitly activates it.
//!
//! Pure: the URL and timestamp formatting and the mode-to-mirror-list mapping do no
//! I/O and read no clock (the caller passes `SystemTime::now()` into
//! [`format_timestamp`]), so all of it is unit-testable.

use crate::error::EngineError;
use boot2deb_core::lock::{SnapshotMode, SnapshotPin};

/// Base of the `snapshot.debian.org` Debian archive; a captured timestamp is
/// appended to select the point-in-time mirror.
const SNAPSHOT_ARCHIVE: &str = "https://snapshot.debian.org/archive/debian";

/// The point-in-time mirror URL for a captured snapshot timestamp.
pub fn snapshot_mirror(timestamp: &str) -> String {
    format!("{SNAPSHOT_ARCHIVE}/{timestamp}/")
}

/// Whether a resolved mirror list holds a point-in-time archive — one whose `Release`
/// is past its `Valid-Until` by design.
///
/// This is the question every provisioner configuration site asks before relaxing the
/// release freshness check, and it is answered here because this module is the one that
/// knows which URLs are snapshots. Asking it of the list *contents* rather than of the
/// list *shape* is what makes [`Pin`](SnapshotMode::Pin) work: a pin has exactly one
/// mirror and it is the snapshot, so a site keying on "are there fallbacks?" would
/// refuse the stale release in the one mode where every fetch comes from a deliberately
/// expired archive.
///
/// The posture the caller then takes is repository-wide — it relaxes freshness for every
/// mirror in the list, not just the snapshot — which is why it is taken only when a
/// snapshot is actually present. A [`Fallback`](SnapshotMode::Fallback) list pays that
/// price knowingly: the live mirror loses its freshness check so the backstop can be
/// read at all.
pub fn has_snapshot(mirrors: &[String]) -> bool {
    mirrors.iter().any(|m| m.starts_with(SNAPSHOT_ARCHIVE))
}

/// Resolve the ordered mirror list the rootfs bootstrap fetches from, honoring the
/// active snapshot mode. `base_mirror` is the live Debian mirror ([`crate::DEFAULT_MIRROR`]);
/// `mode` is the effective mode (a `--snapshot` override, else the lock's captured
/// mode); `snapshot` is the lock's captured pin, if any.
///
/// - [`Off`](SnapshotMode::Off) (or no snapshot): the live mirror only — the
///   snapshot stays provenance and never touches the build.
/// - [`Fallback`](SnapshotMode::Fallback): live mirror first, snapshot second, so
///   apt fills a 404 (a version that rotated off the live mirror) from the snapshot.
/// - [`Pin`](SnapshotMode::Pin): the snapshot only — a fully deterministic userland.
///
/// A `Fallback`/`Pin` mode with no captured timestamp is
/// [`SnapshotUnavailable`](EngineError::SnapshotUnavailable): there is nothing to
/// fetch from, so the request is refused rather than silently downgraded to live.
pub fn resolve_mirrors(
    base_mirror: &str,
    snapshot: Option<&SnapshotPin>,
    mode: SnapshotMode,
) -> Result<Vec<String>, EngineError> {
    match mode {
        SnapshotMode::Off => Ok(vec![base_mirror.to_string()]),
        SnapshotMode::Fallback | SnapshotMode::Pin => {
            let ts =
                snapshot
                    .map(|s| s.timestamp.as_str())
                    .ok_or(EngineError::SnapshotUnavailable {
                        mode: mode.as_str(),
                    })?;
            let snap = snapshot_mirror(ts);
            Ok(match mode {
                SnapshotMode::Pin => vec![snap],
                // Fallback: live first, snapshot backfills 404s.
                _ => vec![base_mirror.to_string(), snap],
            })
        }
    }
}

/// Format a Unix timestamp (whole seconds) as a `snapshot.debian.org` timestamp
/// `YYYYMMDDTHHMMSSZ` in UTC. `--save-snapshot` passes `SystemTime::now()` through
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

    #[test]
    fn off_or_absent_uses_live_mirror_only() {
        let live = "http://deb.debian.org/debian";
        // No captured snapshot, mode off: live only.
        assert_eq!(
            resolve_mirrors(live, None, SnapshotMode::Off).unwrap(),
            vec![live.to_string()]
        );
        // A captured snapshot in off mode stays provenance — still live only.
        let p = pin("20260628T083000Z", SnapshotMode::Off);
        assert_eq!(
            resolve_mirrors(live, Some(&p), SnapshotMode::Off).unwrap(),
            vec![live.to_string()]
        );
    }

    #[test]
    fn fallback_is_live_then_snapshot() {
        let live = "http://deb.debian.org/debian";
        let p = pin("20260628T083000Z", SnapshotMode::Fallback);
        let mirrors = resolve_mirrors(live, Some(&p), SnapshotMode::Fallback).unwrap();
        assert_eq!(
            mirrors,
            vec![
                live.to_string(),
                "https://snapshot.debian.org/archive/debian/20260628T083000Z/".to_string(),
            ]
        );
    }

    #[test]
    fn pin_is_snapshot_only() {
        let live = "http://deb.debian.org/debian";
        let p = pin("20260628T083000Z", SnapshotMode::Pin);
        let mirrors = resolve_mirrors(live, Some(&p), SnapshotMode::Pin).unwrap();
        assert_eq!(
            mirrors,
            vec!["https://snapshot.debian.org/archive/debian/20260628T083000Z/".to_string()]
        );
    }

    #[test]
    fn snapshot_mode_without_capture_is_an_error() {
        let live = "http://deb.debian.org/debian";
        for mode in [SnapshotMode::Fallback, SnapshotMode::Pin] {
            match resolve_mirrors(live, None, mode) {
                Err(EngineError::SnapshotUnavailable { mode: m }) => {
                    assert_eq!(m, mode.as_str())
                }
                other => panic!("expected SnapshotUnavailable, got {other:?}"),
            }
        }
    }

    #[test]
    fn the_stale_release_posture_follows_the_mode_not_the_list_length() {
        // The posture a provisioner takes is decided by whether a point-in-time archive
        // is in the list. `pin` has one mirror and it is the snapshot, so a rule keyed
        // on "has fallbacks" would get this mode — the only one where *every* fetch is
        // from an expired archive — exactly backwards.
        let live = "http://deb.debian.org/debian";
        for (mode, expected) in [
            (SnapshotMode::Off, false),
            (SnapshotMode::Fallback, true),
            (SnapshotMode::Pin, true),
        ] {
            let p = pin("20260628T083000Z", mode);
            let mirrors = resolve_mirrors(live, Some(&p), mode).unwrap();
            assert_eq!(
                has_snapshot(&mirrors),
                expected,
                "{} mirrors {mirrors:?}",
                mode.as_str()
            );
        }
    }

    #[test]
    fn a_live_only_mirror_list_is_not_a_snapshot() {
        assert!(!has_snapshot(&[]));
        assert!(!has_snapshot(&["http://deb.debian.org/debian".to_string()]));
        // A mirror that merely mentions the host in a path is not the archive.
        assert!(!has_snapshot(&[
            "http://example.invalid/snapshot.debian.org/archive/debian/".to_string()
        ]));
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
