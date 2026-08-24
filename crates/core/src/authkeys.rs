//! The shape of an `authorized_keys` entry.
//!
//! Its own module because the check has to be strict about one thing in particular:
//! the value is authored as an SSH key and lands in the file that decides who may log
//! in as the image's default account. A malformed entry is not a cosmetic problem —
//! `sshd` skips a line it cannot parse and says so only in its own log, on a board
//! that may have no console, so the failure surfaces as "my key does not work" with
//! nothing to read. Everything checkable offline is therefore checked here, at
//! resolution, where the message can name the line.
//!
//! Pure and host-independent; nothing here touches the filesystem or the network.

/// Key types `sshd` accepts and this builder will write. `ssh-dss` is deliberately
/// absent: OpenSSH removed DSA support entirely, so a DSA line in the file is one
/// nothing on the image can authenticate with.
pub const KEY_TYPES: &[&str] = &[
    "ssh-ed25519",
    "ssh-rsa",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "sk-ssh-ed25519@openssh.com",
    "sk-ecdsa-sha2-nistp256@openssh.com",
];

/// Check `entry` against the shape of an `authorized_keys` line: a key type from
/// [`KEY_TYPES`], a base64 key blob whose *own* embedded type name agrees with it, and
/// an optional trailing comment.
///
/// Returns `Err` with a terse clause naming the offending property, for the caller to
/// wrap in the typed error that suits where the value was authored — the same
/// convention as [`crate::hostname::check`].
///
/// Four rejections carry more weight than the rest:
///
///  - **Private key material.** A `-----BEGIN … PRIVATE KEY-----` block here means the
///    author reached for `id_ed25519` instead of `id_ed25519.pub`, and the consequence
///    is a private key baked into every copy of a distributable image. This is checked
///    before anything else so the message is about *that* and not about field count.
///  - **Options prefixes** (`restrict`, `command="…"`, `from="…"`). `sshd` accepts them;
///    this does not, because their syntax is quoted, comma-separated, and shell-adjacent,
///    and a builder that half-understood them would silently write a weaker restriction
///    than the author wrote. An entry here is a bare key.
///  - **A blob disagreeing with its type name.** The wire encoding of every accepted
///    key repeats the type as its first field, so a truncated or line-wrapped paste is
///    caught here rather than at first login.
///  - **Embedded newlines.** One entry is one line. A value carrying a newline would
///    become two lines in the file, the second of them unvalidated.
pub fn check_authorized_key(entry: &str) -> Result<(), &'static str> {
    // Before field-splitting: a pasted private key is the one error whose real cause is
    // invisible in any message about structure.
    if entry.contains("PRIVATE KEY") {
        return Err(
            "this is private key material — authorize the public key (the '.pub' file) instead",
        );
    }
    if entry.contains('\n') || entry.contains('\r') {
        return Err("contains a newline; one entry is one line");
    }
    if entry.trim().is_empty() {
        return Err("empty");
    }
    // Leading/trailing space would survive into the file; require the authored value to
    // be the line, so what is validated is what is written.
    if entry != entry.trim() {
        return Err("has leading or trailing whitespace");
    }

    let mut fields = entry.splitn(3, ' ');
    let key_type = fields.next().unwrap_or_default();
    let blob = fields.next().unwrap_or_default();
    let comment = fields.next();

    if blob.is_empty() {
        // An options prefix is the likely cause of a one-field line, and it is worth
        // naming: the author wrote something sshd would accept.
        if key_type.contains('=') || key_type.contains(',') {
            return Err(
                "looks like an options prefix; write a bare '<type> <base64> [comment]' entry",
            );
        }
        return Err("expected '<type> <base64-key> [comment]'");
    }
    if !KEY_TYPES.contains(&key_type) {
        // An options prefix with a key after it lands here, since the first field is
        // then the options rather than a type.
        return Err(
            "unknown key type — expected one of ssh-ed25519, ssh-rsa, ecdsa-sha2-nistp256/384/521, \
             or their sk- security-key forms (an options prefix is not accepted)",
        );
    }
    let decoded = decode_base64(blob).ok_or("key blob is not valid base64")?;
    match embedded_type(&decoded) {
        None => Err("key blob is truncated or not an SSH key encoding"),
        Some(embedded) if embedded != key_type.as_bytes() => {
            Err("key blob does not match its type name — the value looks truncated or re-wrapped")
        }
        Some(_) => match comment {
            // The comment reaches no shell (the build writes the file with a quoted
            // heredoc), so it only has to be printable and single-line.
            Some(c) if c.chars().any(|ch| ch.is_control()) => {
                Err("comment contains a control character")
            }
            _ => Ok(()),
        },
    }
}

/// The type name an SSH key blob carries as its own first field, or `None` if the
/// bytes are not a plausible SSH wire encoding.
///
/// Every accepted key's blob begins with a 32-bit big-endian length followed by that
/// many bytes of type name, which is what makes the [`check_authorized_key`]
/// type/blob cross-check possible. The length is bounded before it is used as a slice
/// index, so arbitrary bytes cannot ask this for a huge read.
fn embedded_type(decoded: &[u8]) -> Option<&[u8]> {
    let (len, rest) = decoded.split_at_checked(4)?;
    let len = u32::from_be_bytes([len[0], len[1], len[2], len[3]]) as usize;
    // The longest name in KEY_TYPES is 34 bytes; a length beyond that is not one of
    // ours regardless of what the rest of the buffer holds.
    if len == 0 || len > 64 {
        return None;
    }
    rest.get(..len)
}

/// Decode standard (`+/`) base64 with optional `=` padding, or `None` on any character
/// outside the alphabet, a misplaced pad, or a truncated group.
///
/// Hand-rolled rather than taken from a dependency because this is the only base64 in
/// `core`, and the decode exists solely to read the key blob's first field — a
/// 20-line table lookup against a crate in the pure, dependency-light config layer.
fn decode_base64(s: &str) -> Option<Vec<u8>> {
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
    if pad > 2 || !bytes.len().is_multiple_of(4) || bytes.len() < 4 {
        return None;
    }
    let body = &bytes[..bytes.len() - pad];
    let mut out = Vec::with_capacity(body.len() / 4 * 3);
    for group in body.chunks(4) {
        let mut acc = 0u32;
        for &c in group {
            acc = (acc << 6) | sextet(c)?;
        }
        // A partial final group carries 6 bits per character; `acc` is left-aligned to
        // the group's full 24 bits so the same shifts read every case.
        acc <<= 6 * (4 - group.len());
        let full = [(acc >> 16) as u8, (acc >> 8) as u8, acc as u8];
        // 2 characters encode 1 byte, 3 encode 2, 4 encode 3.
        out.extend_from_slice(&full[..group.len().saturating_sub(1)]);
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A real ed25519 public key, as `ssh-keygen` writes it.
    const ED25519: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBl5Nn9dY/aLK4WVQ5c4tYlYCkkC1J3Ry+d0nc3TgtDe operator@workstation";
    /// The same key with no comment — `authorized_keys` does not require one.
    const ED25519_NO_COMMENT: &str =
        "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBl5Nn9dY/aLK4WVQ5c4tYlYCkkC1J3Ry+d0nc3TgtDe";

    #[test]
    fn accepts_a_real_key_with_and_without_a_comment() {
        assert_eq!(check_authorized_key(ED25519), Ok(()));
        assert_eq!(check_authorized_key(ED25519_NO_COMMENT), Ok(()));
    }

    /// The decode is only trustworthy if it agrees with the encoding `ssh-keygen`
    /// produces, so assert the field it exists to read.
    #[test]
    fn reads_the_type_name_out_of_a_real_key_blob() {
        let blob = ED25519.split(' ').nth(1).unwrap();
        let decoded = decode_base64(blob).expect("a real key blob decodes");
        assert_eq!(embedded_type(&decoded), Some(&b"ssh-ed25519"[..]));
    }

    /// The catastrophic paste: a private key would otherwise be baked into every copy
    /// of a distributable image.
    #[test]
    fn rejects_private_key_material_by_name() {
        let private =
            "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaA==\n-----END OPENSSH PRIVATE KEY-----";
        let why = check_authorized_key(private).expect_err("private key material is refused");
        assert!(why.contains("private key material"), "unhelpful: {why}");
        // Even reduced to one line — the check must not depend on the newline.
        let one_line = "-----BEGIN OPENSSH PRIVATE KEY----- b3BlbnNzaA==";
        assert!(check_authorized_key(one_line).is_err());
    }

    #[test]
    fn rejects_a_blob_that_disagrees_with_its_type_name() {
        // An rsa blob under an ed25519 type name. Both halves are individually
        // well-formed — the base64 decodes, and it decodes to a real SSH encoding whose
        // own first field reads `ssh-rsa` — so only the cross-check catches it.
        let mismatched = "ssh-ed25519 AAAAB3NzaC1yc2EA";
        assert_eq!(
            embedded_type(&decode_base64("AAAAB3NzaC1yc2EA").unwrap()),
            Some(&b"ssh-rsa"[..]),
            "the fixture must really carry the other type"
        );
        let why = check_authorized_key(mismatched).expect_err("mismatch is refused");
        assert!(why.contains("does not match its type name"), "got: {why}");
    }

    #[test]
    fn rejects_a_truncated_or_rewrapped_blob() {
        // Line-wrapped by a mail client: the second half became a comment, and what is
        // left decodes to bytes that are no longer a key.
        assert!(check_authorized_key("ssh-ed25519 AAAAC3NzaC1lZDI1").is_err());
        // Not base64 at all.
        assert_eq!(
            check_authorized_key("ssh-ed25519 not-base64!!"),
            Err("key blob is not valid base64")
        );
    }

    #[test]
    fn rejects_shapes_that_are_not_a_bare_key() {
        // Missing the blob.
        assert!(check_authorized_key("ssh-ed25519").is_err());
        // DSA: sshd cannot authenticate it at all, so accepting it would ship a line
        // that silently never works.
        assert!(check_authorized_key("ssh-dss AAAAB3NzaC1kc3MAAACBAaaa").is_err());
        // An options prefix is refused, and says so.
        let why = check_authorized_key(&format!("restrict {ED25519}"))
            .expect_err("options prefixes are refused");
        assert!(why.contains("options prefix"), "got: {why}");
        // A one-field options line names the same cause.
        let why = check_authorized_key("command=\"/bin/true\"").expect_err("refused");
        assert!(why.contains("options prefix"), "got: {why}");
    }

    #[test]
    fn rejects_whitespace_and_control_characters() {
        assert!(check_authorized_key("").is_err());
        assert!(check_authorized_key("   ").is_err());
        assert!(check_authorized_key(&format!("  {ED25519}")).is_err());
        assert!(check_authorized_key(&format!("{ED25519}\n")).is_err());
        // Two keys in one entry: the second line would reach the file unvalidated.
        assert!(check_authorized_key(&format!("{ED25519}\n{ED25519}")).is_err());
        assert!(check_authorized_key(&format!("{ED25519_NO_COMMENT} tab\there")).is_err());
    }

    /// Padding, group boundaries, and the byte counts each group length produces —
    /// the cases a hand-rolled decoder gets wrong.
    #[test]
    fn base64_decode_handles_padding_and_rejects_malformed_input() {
        assert_eq!(decode_base64("AAAA"), Some(vec![0, 0, 0]));
        // "man" / "ma" / "m" — RFC 4648's own worked examples, one per pad length.
        assert_eq!(decode_base64("bWFu"), Some(b"man".to_vec()));
        assert_eq!(decode_base64("bWE="), Some(b"ma".to_vec()));
        assert_eq!(decode_base64("bQ=="), Some(b"m".to_vec()));
        // Both non-standard alphabets and stray characters are out.
        assert_eq!(decode_base64("bW-u"), None);
        assert_eq!(decode_base64("bW u"), None);
        // Unpadded remainders, over-padding, and a misplaced pad.
        assert_eq!(decode_base64("bWF"), None);
        assert_eq!(decode_base64("bQ==="), None);
        assert_eq!(decode_base64("b=Fu"), None);
        assert_eq!(decode_base64(""), None);
    }
}
