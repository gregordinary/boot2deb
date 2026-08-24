//! The shape of the Wi-Fi seed keys.
//!
//! Its own module for the same reason [`hostname`](crate::hostname) is: the values
//! cross a trust boundary. A `wifi_ssid=`/`wifi_psk=` pair is typed at press time,
//! travels as a line of `seed.txt`, and is written by the device's first-boot hook
//! into a NetworkManager keyfile — a line-based format of its own. Both formats
//! take one value per line, so the one property this module must hold is that a
//! value cannot smuggle a line (or a control character) into a file another
//! parser reads. The checks run where the value is authored, so a bad value is a
//! press-time error naming the flag, not a board that silently never joins.
//!
//! Pure and host-independent; nothing here touches the filesystem.

/// Longest SSID 802.11 allows, in bytes.
pub const MAX_SSID_LEN: usize = 32;

/// WPA passphrase bounds (IEEE 802.11i Annex H): 8 to 63 printable ASCII
/// characters, or exactly 64 hexadecimal digits naming the raw 256-bit key.
pub const MIN_PSK_LEN: usize = 8;

/// See [`MIN_PSK_LEN`].
pub const MAX_PSK_LEN: usize = 63;

/// Check an SSID: 1 to [`MAX_SSID_LEN`] bytes with no control characters.
///
/// 802.11 itself allows arbitrary octets, but this SSID rides two line-based
/// text files (`seed.txt`, the NetworkManager keyfile), so a value a line-based
/// parser cannot carry — a newline, any other control character — is refused
/// here rather than mangled there. Spaces and non-ASCII UTF-8 are fine.
///
/// # Errors
///
/// A terse clause naming the offending property, for the caller to wrap with the
/// flag or key the value was authored under.
pub fn check_ssid(ssid: &str) -> Result<(), &'static str> {
    if ssid.is_empty() {
        return Err("empty");
    }
    if ssid.len() > MAX_SSID_LEN {
        return Err("longer than 32 bytes, which 802.11 cannot carry");
    }
    if ssid.chars().any(char::is_control) {
        return Err("contains a control character, which a seed line cannot carry");
    }
    if ssid != ssid.trim() {
        return Err("starts or ends with whitespace, which the seed parser trims away");
    }
    Ok(())
}

/// Check a WPA passphrase: [`MIN_PSK_LEN`] to [`MAX_PSK_LEN`] printable ASCII
/// characters, or exactly 64 hex digits (the raw pre-shared key).
///
/// The bound is the standard's, not this tool's: `wpa_passphrase` and
/// NetworkManager both refuse values outside it, so accepting one here would
/// press a card whose network the board can never join.
///
/// # Errors
///
/// A terse clause naming the offending property, as [`check_ssid`] returns.
pub fn check_psk(psk: &str) -> Result<(), &'static str> {
    if psk.len() == 64 {
        return if psk.chars().all(|c| c.is_ascii_hexdigit()) {
            Ok(())
        } else {
            Err("64 characters is the raw-key form, which must be all hex digits")
        };
    }
    if psk.len() < MIN_PSK_LEN {
        return Err("shorter than 8 characters, which WPA refuses");
    }
    if psk.len() > MAX_PSK_LEN {
        return Err("longer than 63 characters, which WPA refuses (64 hex digits names a raw key)");
    }
    if !psk.chars().all(|c| c.is_ascii_graphic() || c == ' ') {
        return Err("contains a character outside printable ASCII, which WPA refuses");
    }
    // Interior spaces are legal WPA; edge whitespace is not carried by the seed
    // file's line parser, so a value that depends on it would silently change.
    if psk != psk.trim() {
        return Err("starts or ends with whitespace, which the seed parser trims away");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ssids_hold_the_line_based_contract() {
        check_ssid("lab net").unwrap();
        check_ssid("Überfunk").unwrap();
        check_ssid(&"x".repeat(32)).unwrap();

        assert!(check_ssid("").is_err());
        assert!(check_ssid(&"x".repeat(33)).is_err());
        assert!(check_ssid("two\nlines").is_err());
        assert!(check_ssid("tab\there").is_err());
        assert!(check_ssid(" padded ").is_err());
    }

    #[test]
    fn psks_hold_the_wpa_bounds() {
        check_psk("hunter22").unwrap();
        check_psk("pass phrase with spaces").unwrap();
        check_psk(&"x".repeat(63)).unwrap();
        check_psk(&"a1".repeat(32)).unwrap();

        assert!(check_psk("short").is_err());
        assert!(
            check_psk(&"x".repeat(64)).is_err(),
            "64 non-hex is not a raw key"
        );
        assert!(check_psk(&"x".repeat(65)).is_err());
        assert!(check_psk("emoji-\u{1F512}pass").is_err());
        assert!(check_psk("two\nlines-long").is_err());
    }
}
