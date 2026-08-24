//! Parse human-authored size / offset strings (`32KiB`, `8MiB`, `2G`) to bytes.
//!
//! Config carries the raw-gap offsets and the image size as authored strings
//! ([`Offsets`](crate::model::Offsets), [`ResolvedBuild::image_size`](crate::model::ResolvedBuild));
//! they are parsed to exact byte counts only when an artifact is written — the
//! u-boot deb's documented `dd` offsets and the image node's partition
//! geometry. This is that parse: pure and deterministic, so the geometry
//! is unit-testable without touching a disk.
//!
//! Units are binary (powers of 1024), matching the authored values and the
//! builder's `m = 1024²` / `g = 1024³` convention — `K`/`KB`/`KiB` are all 1024,
//! and so on up through `T`. Parsing is case-insensitive and tolerates
//! whitespace around the number and unit.

use crate::error::ConfigError;

/// Parse a size / offset string to a byte count.
///
/// Accepts a bare integer (bytes) or an integer with a binary unit suffix —
/// `K`/`KB`/`KiB` (×1024), `M`/`MB`/`MiB` (×1024²), `G`/`GB`/`GiB` (×1024³),
/// `T`/`TB`/`TiB` (×1024⁴) — case-insensitively, with optional whitespace
/// around the unit. A malformed string, a missing or unknown unit, or a value
/// that overflows [`u64`] is a [`ConfigError::InvalidSize`].
///
/// ```
/// use boot2deb_core::size::parse_size;
/// assert_eq!(parse_size("32KiB").unwrap(), 32 * 1024);
/// assert_eq!(parse_size("8MiB").unwrap(), 8 * 1024 * 1024);
/// assert_eq!(parse_size("2G").unwrap(), 2 * 1024 * 1024 * 1024);
/// assert_eq!(parse_size("512").unwrap(), 512);
/// ```
pub fn parse_size(input: &str) -> Result<u64, ConfigError> {
    let s = input.trim();
    // Split the leading run of ASCII digits from the (optional) unit suffix.
    let digits_end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    let (num, unit) = s.split_at(digits_end);
    // An empty digit run fails to parse — the same InvalidSize as a bad number.
    let value: u64 = num.parse().map_err(|_| invalid(input))?;
    let multiplier = unit_multiplier(unit.trim()).ok_or_else(|| invalid(input))?;
    value.checked_mul(multiplier).ok_or_else(|| invalid(input))
}

/// Render a byte count in the largest binary unit that divides it **exactly**,
/// producing a string [`parse_size`] round-trips.
///
/// For a value resolution *derived* rather than read from config — the depthcharge
/// rootfs offset, computed from the slot geometry. Everything else on a resolved
/// build carries the author's own string, so a derived value should read like one
/// instead of appearing as the lone bare byte count in the output.
///
/// Exact division only: `76MiB` for 79691776, but a value no unit divides stays
/// bytes rather than being rounded, since these are offsets and a rounded offset is
/// the wrong offset.
///
/// ```
/// use boot2deb_core::size::{format_size, parse_size};
/// assert_eq!(format_size(76 * 1024 * 1024), "76MiB");
/// assert_eq!(format_size(2 * 1024 * 1024 * 1024), "2GiB");
/// assert_eq!(format_size(1536), "1536");        // 1.5KiB is not exact
/// assert_eq!(format_size(0), "0");
/// assert_eq!(parse_size(&format_size(44 * 1024 * 1024)).unwrap(), 44 * 1024 * 1024);
/// ```
pub fn format_size(bytes: u64) -> String {
    const K: u64 = 1024;
    for (unit, mult) in [
        ("TiB", K * K * K * K),
        ("GiB", K * K * K),
        ("MiB", K * K),
        ("KiB", K),
    ] {
        if bytes >= mult && bytes.is_multiple_of(mult) {
            return format!("{}{unit}", bytes / mult);
        }
    }
    bytes.to_string()
}

/// Byte multiplier for a trimmed unit suffix; `None` for an unrecognized unit.
/// An empty suffix (or `b`) means raw bytes (×1).
fn unit_multiplier(unit: &str) -> Option<u64> {
    const K: u64 = 1024;
    Some(match unit.to_ascii_lowercase().as_str() {
        "" | "b" => 1,
        "k" | "kb" | "kib" => K,
        "m" | "mb" | "mib" => K * K,
        "g" | "gb" | "gib" => K * K * K,
        "t" | "tb" | "tib" => K * K * K * K,
        _ => return None,
    })
}

/// Build the [`ConfigError::InvalidSize`] for `input`, echoing the original
/// string so the message points at what the author wrote.
fn invalid(input: &str) -> ConfigError {
    ConfigError::InvalidSize {
        value: input.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_binary_units_case_insensitively() {
        assert_eq!(parse_size("32KiB").unwrap(), 32 * 1024);
        assert_eq!(parse_size("8MiB").unwrap(), 8 * 1024 * 1024);
        assert_eq!(parse_size("16MiB").unwrap(), 16 * 1024 * 1024);
        // The IEC and short forms agree, and case does not matter.
        assert_eq!(parse_size("2G").unwrap(), 2 * 1024 * 1024 * 1024);
        assert_eq!(parse_size("2g").unwrap(), parse_size("2G").unwrap());
        assert_eq!(parse_size("2GiB").unwrap(), parse_size("2G").unwrap());
        assert_eq!(parse_size("1tb").unwrap(), 1024u64.pow(4));
    }

    #[test]
    fn bare_number_is_bytes_and_whitespace_tolerated() {
        assert_eq!(parse_size("512").unwrap(), 512);
        assert_eq!(parse_size("  4096  ").unwrap(), 4096);
        assert_eq!(parse_size("16 MiB").unwrap(), 16 * 1024 * 1024);
        assert_eq!(parse_size("0").unwrap(), 0);
    }

    #[test]
    fn rejects_malformed_and_overflow() {
        assert!(parse_size("").is_err());
        assert!(parse_size("abc").is_err());
        assert!(parse_size("12qb").is_err()); // unknown unit
        assert!(parse_size("MiB").is_err()); // no number

        // Parses as a number but overflows u64 on the unit multiply.
        assert!(parse_size("17000000000000G").is_err());
    }

    #[test]
    fn format_size_picks_the_largest_exact_unit_and_round_trips() {
        assert_eq!(format_size(44 * 1024 * 1024), "44MiB");
        assert_eq!(format_size(76 * 1024 * 1024), "76MiB");
        assert_eq!(format_size(32 * 1024), "32KiB");
        assert_eq!(format_size(1024u64.pow(4)), "1TiB");
        // Inexact values stay bytes: these are offsets, and a rounded offset is a
        // different offset.
        assert_eq!(format_size(1536), "1536");
        assert_eq!(format_size(1), "1");
        assert_eq!(format_size(0), "0");
        for v in [0, 1, 512, 1536, 12 * 1024 * 1024, 76 * 1024 * 1024] {
            assert_eq!(parse_size(&format_size(v)).unwrap(), v);
        }
    }
}
