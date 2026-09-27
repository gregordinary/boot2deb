//! Standard base64 (RFC 4648 section 4, the `+/` alphabet with `=` padding), both ways.
//!
//! Pure. Two readers of one alphabet share it. [`authkeys`](crate::authkeys) decodes an
//! SSH key blob to read its type name. [`files`](crate::files) encodes a symlink target
//! and an extended attribute's value into a manifest. A table lookup in the
//! dependency-light config layer, rather than a crate, because that is all either needs.

/// The standard alphabet, indexed by sextet value.
const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Encode `bytes` as padded standard base64. The empty input encodes to the empty
/// string.
///
/// ```
/// assert_eq!(boot2deb_core::base64::encode(b"man"), "bWFu");
/// assert_eq!(boot2deb_core::base64::encode(b"m"), "bQ==");
/// ```
pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for group in bytes.chunks(3) {
        let acc = group
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, &b)| acc | (b as u32) << (16 - 8 * i));
        // A group of n bytes yields n + 1 characters, and padding fills it to four.
        for i in 0..=group.len() {
            out.push(ALPHABET[((acc >> (18 - 6 * i)) & 0x3f) as usize] as char);
        }
        for _ in group.len()..3 {
            out.push('=');
        }
    }
    out
}

/// Decode padded standard base64, or `None` on any character outside the alphabet, a
/// misplaced pad, or a truncated group. The empty string decodes to no bytes.
pub fn decode(s: &str) -> Option<Vec<u8>> {
    /// Sextet value of a base64 character, or `None` for anything else.
    fn sextet(c: u8) -> Option<u32> {
        match c {
            b'A'..=b'Z' => Some((c - b'A') as u32),
            b'a'..=b'z' => Some((c - b'a') as u32 + 26),
            b'0'..=b'9' => Some((c - b'0') as u32 + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let bytes = s.as_bytes();
    // Padding is only ever the last one or two characters, and the body must be whole
    // 4-character groups once it is accounted for.
    let pad = bytes.iter().rev().take_while(|&&c| c == b'=').count();
    if pad > 2 || !bytes.len().is_multiple_of(4) {
        return None;
    }
    let body = &bytes[..bytes.len() - pad];
    let mut out = Vec::with_capacity(body.len() / 4 * 3);
    for group in body.chunks(4) {
        // A lone character carries 6 bits, which is less than one byte.
        if group.len() == 1 {
            return None;
        }
        let mut acc = 0u32;
        for &c in group {
            acc = (acc << 6) | sextet(c)?;
        }
        // A partial final group carries 6 bits per character; `acc` is left-aligned to
        // the group's full 24 bits so the same shifts read every case.
        acc <<= 6 * (4 - group.len());
        let full = [(acc >> 16) as u8, (acc >> 8) as u8, acc as u8];
        // 2 characters encode 1 byte, 3 encode 2, 4 encode 3.
        out.extend_from_slice(&full[..group.len() - 1]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Padding, group boundaries, and the byte counts each group length produces —
    /// the cases a hand-rolled decoder gets wrong.
    #[test]
    fn decode_handles_padding_and_rejects_malformed_input() {
        assert_eq!(decode("AAAA"), Some(vec![0, 0, 0]));
        // "man" / "ma" / "m" — RFC 4648's own worked examples, one per pad length.
        assert_eq!(decode("bWFu"), Some(b"man".to_vec()));
        assert_eq!(decode("bWE="), Some(b"ma".to_vec()));
        assert_eq!(decode("bQ=="), Some(b"m".to_vec()));
        assert_eq!(decode(""), Some(Vec::new()));
        // Both non-standard alphabets and stray characters are out.
        assert_eq!(decode("bW-u"), None);
        assert_eq!(decode("bW u"), None);
        // Unpadded remainders, over-padding, and a misplaced pad.
        assert_eq!(decode("bWF"), None);
        assert_eq!(decode("bQ==="), None);
        assert_eq!(decode("b=Fu"), None);
    }

    /// Every byte value and every remainder length survives a round trip, and the
    /// encoding is RFC 4648's own for its worked examples.
    #[test]
    fn encode_round_trips_every_remainder() {
        for (plain, coded) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(encode(plain.as_bytes()), coded);
            assert_eq!(decode(coded).unwrap(), plain.as_bytes());
        }
        let every: Vec<u8> = (0..=255).collect();
        assert_eq!(decode(&encode(&every)).unwrap(), every);
    }
}
