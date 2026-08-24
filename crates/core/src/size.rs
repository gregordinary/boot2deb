//! Parse human-authored size / offset strings (`32KiB`, `8MiB`, `2G`) to bytes.
//!
//! Config carries the raw-gap offsets and the image size as authored strings
//! ([`Offsets`](crate::model::Offsets), [`ResolvedImage::image_size`](crate::model::ResolvedBuild));
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

/// What an authored `image_size` asks for: a stated whole-disk size, or one to be
/// measured from the rootfs itself.
///
/// The two run the image node in opposite orders. A stated size lays out the disk first
/// and formats the rootfs into the partition that leaves; a fitted one formats first —
/// searching for the smallest filesystem that holds the rootfs with the stated room to
/// spare — and lays out the disk around the answer. Only the grammar is decided here;
/// what a fit costs and what limits it are the formatter's, and the image node applies
/// them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageSize {
    /// An authored whole-disk size in bytes (`4G`).
    Fixed(u64),
    /// Size the image to its contents, leaving this much of the rootfs free (`fit+20%`).
    Fit(Slack),
}

/// How much of a fitted rootfs must remain free once it holds the source.
///
/// A restatement of the formatter's own slack in terms this crate can hold — the image
/// node translates it — because the *grammar* is config's business and this crate is
/// where an authored string is validated, while the search that consumes it is not.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Slack {
    /// At least this many bytes free.
    Bytes(u64),
    /// At least this share of the filesystem free, in hundredths of one percent:
    /// `2000` for a fifth, `150` for 1.5%.
    Share(u16),
}

/// Parse an authored `image_size`.
///
/// The grammar is a plain size ([`parse_size`]), or `fit+<slack>` where the slack is a
/// share (`fit+20%`, `fit+1.5%`) or a byte count (`fit+512M`).
///
/// A bare `fit` is refused: the smallest filesystem holding a rootfs is one with nothing
/// left in it, which boots into a full disk, so the room to leave is stated rather than
/// defaulted. `fit+0%` is accepted — an explicit zero is a decision, an unstated one is
/// an omission.
///
/// This validates the *form* only. Whether a share or a byte slack is within the limits
/// the search will accept is checked where the search lives, together with the rest of
/// the image geometry.
///
/// ```
/// use boot2deb_core::size::{parse_image_size, ImageSize, Slack};
/// assert_eq!(parse_image_size("4G").unwrap(), ImageSize::Fixed(4 << 30));
/// assert_eq!(parse_image_size("fit+20%").unwrap(), ImageSize::Fit(Slack::Share(2000)));
/// assert_eq!(parse_image_size("fit+512M").unwrap(), ImageSize::Fit(Slack::Bytes(512 << 20)));
/// assert!(parse_image_size("fit").is_err());
/// ```
pub fn parse_image_size(input: &str) -> Result<ImageSize, ConfigError> {
    let spec = input.trim();
    let Some(slack) = spec.strip_prefix("fit") else {
        // Not a measured size, so it is a stated one — and a malformed stated size is
        // reported as itself, since naming the `fit` forms would only misdirect an author
        // who typed `2GB!`.
        return parse_size(spec).map(ImageSize::Fixed);
    };
    let slack = slack
        .strip_prefix('+')
        .ok_or_else(|| invalid_image(input))?;
    match slack.strip_suffix('%') {
        Some(share) => Ok(ImageSize::Fit(Slack::Share(parse_hundredths(
            share, input,
        )?))),
        None => Ok(ImageSize::Fit(Slack::Bytes(
            parse_size(slack).map_err(|_| invalid_image(input))?,
        ))),
    }
}

/// Parse a percentage into hundredths of one percent — the unit the formatter's share
/// slack takes: `20` → `2000`, `1.5` → `150`, `0.01` → `1`.
///
/// Two fraction digits, because that is the unit's whole resolution. A third is refused
/// rather than rounded away: a size grammar that quietly discards part of what it was
/// given is worse than one that says it cannot carry it.
fn parse_hundredths(share: &str, input: &str) -> Result<u16, ConfigError> {
    let (whole, frac) = match share.split_once('.') {
        Some((whole, frac)) if frac.len() <= 2 => (whole, frac),
        Some(_) => return Err(invalid_image(input)),
        None => (share, ""),
    };
    if whole.is_empty() && frac.is_empty() {
        return Err(invalid_image(input));
    }
    let digits = |s: &str| s.chars().all(|c| c.is_ascii_digit());
    if !digits(whole) || !digits(frac) {
        return Err(invalid_image(input));
    }
    let whole: u32 = if whole.is_empty() {
        0
    } else {
        whole.parse().map_err(|_| invalid_image(input))?
    };
    // Right-pad so `.5` is five tenths rather than five hundredths.
    let frac: u32 = format!("{frac:0<2}")
        .parse()
        .map_err(|_| invalid_image(input))?;
    whole
        .checked_mul(100)
        .and_then(|w| w.checked_add(frac))
        .and_then(|h| u16::try_from(h).ok())
        .ok_or_else(|| invalid_image(input))
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

/// Build the [`ConfigError::InvalidImageSize`] for `input`, likewise — the variant that
/// names the measured forms, for the one field that accepts them.
fn invalid_image(input: &str) -> ConfigError {
    ConfigError::InvalidImageSize {
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

    /// The `image_size` grammar in both directions. The share form is the one with
    /// arithmetic in it: the unit is hundredths of one percent, so a decimal point has to
    /// land in the right column or a recipe asking for a fifth gets a five-hundredth.
    #[test]
    fn an_image_size_is_a_stated_size_or_a_fit_with_slack() {
        assert_eq!(parse_image_size("4G").unwrap(), ImageSize::Fixed(4 << 30));
        assert_eq!(parse_image_size("512").unwrap(), ImageSize::Fixed(512));
        assert_eq!(
            parse_image_size("fit+20%").unwrap(),
            ImageSize::Fit(Slack::Share(2000))
        );
        assert_eq!(
            parse_image_size("fit+1.5%").unwrap(),
            ImageSize::Fit(Slack::Share(150))
        );
        assert_eq!(
            parse_image_size("fit+0.01%").unwrap(),
            ImageSize::Fit(Slack::Share(1))
        );
        // An explicit zero is a decision and is honoured; an unstated one is not.
        assert_eq!(
            parse_image_size("fit+0%").unwrap(),
            ImageSize::Fit(Slack::Share(0))
        );
        assert_eq!(
            parse_image_size("fit+512M").unwrap(),
            ImageSize::Fit(Slack::Bytes(512 << 20))
        );

        // A bare `fit` names no slack, which would build an image with nothing free.
        assert!(parse_image_size("fit").is_err());
        // A third fraction digit is refused rather than rounded away.
        assert!(parse_image_size("fit+1.234%").is_err());
        for bad in [
            "fit+", "fit+%", "fit+x%", "fit+.%", "fit-20%", "fit20%", "fitful",
        ] {
            assert!(parse_image_size(bad).is_err(), "{bad} must be refused");
        }

        // A share past what any search accepts still *parses*: the magnitude limits are
        // the image node's, and refusing them here would put half a rule in each crate.
        assert_eq!(
            parse_image_size("fit+95%").unwrap(),
            ImageSize::Fit(Slack::Share(9500))
        );
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
