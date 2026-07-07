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
}
