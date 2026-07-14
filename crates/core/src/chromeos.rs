//! ChromeOS GPT kernel-partition attributes — the bits that decide which kernel a
//! Chromebook's firmware boots.
//!
//! Pure and deterministic: this is bit packing, and the one place the layout is
//! written down.
//!
//! A ChromeOS-firmware board does not read a bootloader from a fixed offset. It
//! scans every boot medium's GPT for partitions of the ChromeOS kernel type, and
//! chooses among them using three fields packed into the **top 16 bits (48-63) of
//! the entry's 64-bit attribute word**:
//!
//! | field        | bits  | meaning                                                    |
//! |--------------|-------|------------------------------------------------------------|
//! | `priority`   | 51:48 | boot order; 15 is highest, **0 means never boot**           |
//! | `tries`      | 55:52 | attempts remaining; decremented on each failed boot         |
//! | `successful` | 56    | known-good; the firmware stops decrementing `tries`         |
//!
//! The conventional writer is ChromeOS's `cgpt`, but these are plain bits in a
//! standard GPT entry, so the `gpt` crate's raw `flags: u64` writes them and the
//! build needs no ChromeOS host tooling at all. §3.6.

use crate::error::ConfigError;

/// Bit position of the `successful` flag.
const SUCCESSFUL_BIT: u32 = 56;
/// Least-significant bit of the 4-bit `tries` field.
const TRIES_SHIFT: u32 = 52;
/// Least-significant bit of the 4-bit `priority` field.
const PRIORITY_SHIFT: u32 = 48;
/// `priority` and `tries` are 4 bits each.
const NIBBLE_MAX: u8 = 0xF;

/// Pack the ChromeOS kernel-partition attributes into a GPT entry's attribute word.
///
/// `priority` and `tries` are 4-bit fields, so a value above 15 cannot be
/// represented and is a [`ConfigError::InvalidKpartAttr`] rather than a silent
/// truncation into a neighbouring field — a truncated `priority` of 0 would mean
/// "never boot", which is precisely the failure that must not happen quietly.
pub fn kpart_flags(priority: u8, tries: u8, successful: bool) -> Result<u64, ConfigError> {
    let nibble = |value: u8, field: &'static str| {
        if value <= NIBBLE_MAX {
            Ok(u64::from(value))
        } else {
            Err(ConfigError::InvalidKpartAttr { field, value })
        }
    };
    let priority = nibble(priority, "kpart_priority")?;
    let tries = nibble(tries, "kpart_tries")?;
    Ok((priority << PRIORITY_SHIFT)
        | (tries << TRIES_SHIFT)
        | (u64::from(successful) << SUCCESSFUL_BIT))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_shipped_attributes_pack_to_the_value_a_booting_board_carries() {
        // priority=10, tries=5, successful=1 — read back off the working image and
        // off the postmarketOS install on the target unit. If this constant ever
        // changes, an image stops booting, so it is asserted literally.
        assert_eq!(kpart_flags(10, 5, true).unwrap(), 0x015A_0000_0000_0000);
    }

    #[test]
    fn each_field_lands_in_its_own_bits() {
        // Fields are independent: setting one must not disturb another.
        assert_eq!(kpart_flags(15, 0, false).unwrap(), 0x000F_0000_0000_0000);
        assert_eq!(kpart_flags(0, 15, false).unwrap(), 0x00F0_0000_0000_0000);
        assert_eq!(kpart_flags(0, 0, true).unwrap(), 0x0100_0000_0000_0000);
        // Nothing at all: a partition the firmware will never boot.
        assert_eq!(kpart_flags(0, 0, false).unwrap(), 0);
    }

    #[test]
    fn an_out_of_range_field_is_a_typed_error_not_a_truncation() {
        // 16 does not fit 4 bits. Truncating would write priority=0 — "never boot" —
        // and the board would silently refuse the image it was just given.
        for (priority, tries, field) in [(16, 5, "kpart_priority"), (10, 16, "kpart_tries")] {
            match kpart_flags(priority, tries, true) {
                Err(ConfigError::InvalidKpartAttr { field: f, .. }) => assert_eq!(f, field),
                other => panic!("expected InvalidKpartAttr for {field}, got {other:?}"),
            }
        }
        // The boundary value itself is fine.
        assert!(kpart_flags(15, 15, true).is_ok());
    }
}
