//! The three UTC timestamp spellings this project writes, and the calendar
//! conversion behind all of them.
//!
//! Pure: the caller reads the clock and passes whole Unix seconds, so the civil-date
//! conversion is unit-testable and nothing here is a source of nondeterminism.
//!
//! There are three spellings because three external formats demand different ones.
//! `snapshot.debian.org` wants `YYYYMMDDTHHMMSSZ`, SPDX and CycloneDX want RFC 3339,
//! and kbuild's `KBUILD_BUILD_TIMESTAMP` wants what `date` prints. One calendar
//! conversion, because a second copy of it is the kind of code that is wrong for four
//! years without anyone noticing.

/// Format whole Unix seconds as a `snapshot.debian.org` timestamp,
/// `YYYYMMDDTHHMMSSZ` in UTC — the spelling a snapshot mirror URL takes.
///
/// ```
/// use boot2deb_core::datetime::format_compact;
/// assert_eq!(format_compact(1_767_225_600), "20260101T000000Z");
/// ```
pub fn format_compact(unix_secs: u64) -> String {
    let (y, mo, d, h, m, s) = parts(unix_secs);
    format!("{y:04}{mo:02}{d:02}T{h:02}{m:02}{s:02}Z")
}

/// Format whole Unix seconds as RFC 3339 UTC, `YYYY-MM-DDTHH:MM:SSZ` — the spelling
/// SPDX's `creationInfo.created` and CycloneDX's `metadata.timestamp` require.
///
/// ```
/// use boot2deb_core::datetime::format_rfc3339;
/// assert_eq!(format_rfc3339(1_767_225_600), "2026-01-01T00:00:00Z");
/// ```
pub fn format_rfc3339(unix_secs: u64) -> String {
    let (y, mo, d, h, m, s) = parts(unix_secs);
    format!("{y:04}-{mo:02}-{d:02}T{h:02}:{m:02}:{s:02}Z")
}

/// Format whole Unix seconds the way `date` prints them in the C locale and UTC,
/// `Www Mmm dd HH:MM:SS UTC YYYY` with a space-padded day.
///
/// It is the spelling kbuild's `KBUILD_BUILD_TIMESTAMP` takes. Kbuild parses the value
/// with `date -d`, and also copies it verbatim into the kernel's version string. So it is
/// what `uname -v` shows on the booted board.
///
/// ```
/// use boot2deb_core::datetime::format_date_c;
/// assert_eq!(format_date_c(0), "Thu Jan  1 00:00:00 UTC 1970");
/// ```
pub fn format_date_c(unix_secs: u64) -> String {
    const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let (y, mo, d, h, m, s) = parts(unix_secs);
    // 1970-01-01 was a Thursday, which is why the table starts there.
    let weekday = WEEKDAYS[((unix_secs / 86_400) % 7) as usize];
    let month = MONTHS[(mo - 1) as usize];
    format!("{weekday} {month} {d:>2} {h:02}:{m:02}:{s:02} UTC {y}")
}

/// Split whole Unix seconds into UTC `(year, month, day, hour, minute, second)`.
fn parts(unix_secs: u64) -> (i64, u32, u32, u64, u64, u64) {
    let days = (unix_secs / 86_400) as i64;
    let sod = unix_secs % 86_400;
    let (y, mo, d) = civil_from_days(days);
    (y, mo, d, sod / 3600, (sod % 3600) / 60, sod % 60)
}

/// Days-since-1970-01-01 → `(year, month, day)` in the proleptic Gregorian
/// calendar (Howard Hinnant's `civil_from_days`). Avoids a date-library dependency
/// for the handful of timestamps this project writes.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097; // [0, 146096]
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32; // [1, 31]
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32; // [1, 12]
    (if m <= 2 { y + 1 } else { y }, m, d)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_spellings_agree_on_the_same_instant() {
        // The two formats differ only in punctuation, so a divergence would mean the
        // calendar conversion had been forked — which is the thing this module exists
        // to prevent.
        for secs in [0, 1_767_225_600, 1_754_006_400, 951_782_400] {
            let compact = format_compact(secs);
            let rfc = format_rfc3339(secs);
            assert_eq!(compact.replace(['-', ':'], ""), rfc.replace(['-', ':'], ""));
        }
    }

    /// Reference strings from GNU `date -d @<secs>` under `LC_ALL=C TZ=UTC`, which is
    /// the parser kbuild hands the value to. A single-digit day is space-padded.
    #[test]
    fn the_date_spelling_matches_gnu_date() {
        for (secs, want) in [
            (0, "Thu Jan  1 00:00:00 UTC 1970"),
            (1_790_347_034, "Fri Sep 25 14:37:14 UTC 2026"),
            (951_782_400, "Tue Feb 29 00:00:00 UTC 2000"),
            (1_767_225_600, "Thu Jan  1 00:00:00 UTC 2026"),
        ] {
            assert_eq!(format_date_c(secs), want);
        }
    }

    #[test]
    fn leap_years_and_the_epoch_land_on_the_right_day() {
        assert_eq!(format_rfc3339(0), "1970-01-01T00:00:00Z");
        // 2000-02-29: a leap year by the 400-rule, the case a naive conversion gets
        // wrong and a test that only checks a recent date never reaches.
        assert_eq!(format_rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        // 2100 is *not* a leap year by the 100-rule.
        assert_eq!(format_rfc3339(4_107_542_400), "2100-03-01T00:00:00Z");
        // Time of day, carried through both fields.
        assert_eq!(format_rfc3339(1_767_225_600 + 3661), "2026-01-01T01:01:01Z");
    }
}
