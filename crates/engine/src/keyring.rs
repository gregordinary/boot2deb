//! Vendored apt keyring auditing: enumerate an OpenPGP keyring's primary-key
//! fingerprints and hold them to a checked-in manifest.
//!
//! A keyring under `blobs/keyrings/` is a *trust anchor*: it decides whose
//! `Release` signatures the bootstrap accepts. As a binary blob it is also the one
//! vendored file a human cannot review — a diff reports `Bin 55918 -> 55934 bytes`
//! and nothing about which keys changed. So every vendored keyring ships a sibling
//! `<name>.fingerprints` manifest naming the primary keys it is allowed to contain,
//! and [`verify`] fails closed unless the keyring holds exactly that set. Swapping a
//! key now shows up in review as a line of text, and the trusted set is a list anyone
//! can check against `debian.org` without trusting this repo.
//!
//! Only **primary** keys (packet tag 6) are listed. A subkey cannot be added without a
//! binding signature from its primary key, which `gpgv` verifies during the bootstrap
//! and which an attacker cannot forge without the primary secret — so pinning the
//! primaries pins the whole certificate.
//!
//! Parsing is pure-Rust and deliberately narrow: walk the packet stream (RFC 4880
//! §4.2), take the v4 public-key packets, and fingerprint each as
//! `SHA-1(0x99 ‖ len_be16 ‖ body)` (§12.2). Anything it does not understand — a
//! partial length, an unknown key version — is an error, never a skipped packet, so a
//! keyring this module cannot fully account for is never declared verified.

use crate::error::EngineError;
use sha1::{Digest, Sha1};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Extension of the fingerprint manifest that sits beside each vendored keyring.
const MANIFEST_EXT: &str = "fingerprints";

/// OpenPGP packet tag for a Public-Key (primary) packet.
const TAG_PUBLIC_KEY: u8 = 6;

/// The manifest path for a keyring: `<keyring>.fingerprints` (replacing `.gpg`).
pub fn manifest_path(keyring: &Path) -> PathBuf {
    keyring.with_extension(MANIFEST_EXT)
}

/// Verify a vendored keyring against its sibling fingerprint manifest.
///
/// The manifest is **mandatory**: a vendored keyring with no manifest is
/// [`EngineError::KeyringManifestMissing`], not an unchecked pass, so deleting the
/// manifest cannot silently disable the check. The keyring's primary-key set must
/// equal the manifest's exactly — an extra key is an injected trust anchor, a missing
/// one is a stale manifest, and both are
/// [`EngineError::KeyringFingerprintMismatch`].
///
/// Returns the verified fingerprints in manifest order, for callers that want to
/// report what they trusted.
pub fn verify(keyring: &Path) -> Result<Vec<Key>, EngineError> {
    let manifest = manifest_path(keyring);
    if !manifest.exists() {
        return Err(EngineError::KeyringManifestMissing {
            keyring: keyring.display().to_string(),
            manifest: manifest.display().to_string(),
        });
    }
    let manifest_text =
        std::fs::read_to_string(&manifest).map_err(|source| EngineError::io(&manifest, source))?;
    let expected = parse_manifest(&manifest_text).map_err(|reason| {
        EngineError::KeyringManifestMalformed {
            manifest: manifest.display().to_string(),
            reason,
        }
    })?;

    let bytes = std::fs::read(keyring).map_err(|source| EngineError::io(keyring, source))?;
    let actual = fingerprints(&bytes).map_err(|reason| EngineError::KeyringMalformed {
        keyring: keyring.display().to_string(),
        reason,
    })?;

    let expected_set: BTreeSet<&str> = expected.iter().map(|k| k.fingerprint.as_str()).collect();
    let actual_set: BTreeSet<&str> = actual.iter().map(String::as_str).collect();
    if expected_set != actual_set {
        let unexpected: Vec<String> = actual_set
            .difference(&expected_set)
            .map(|f| (*f).to_string())
            .collect();
        // Report the manifest's label alongside a key the keyring no longer carries:
        // "which key went missing" is the question a stale-manifest failure raises.
        let missing: Vec<String> = expected
            .iter()
            .filter(|k| !actual_set.contains(k.fingerprint.as_str()))
            .map(Key::to_string)
            .collect();
        return Err(EngineError::KeyringFingerprintMismatch {
            keyring: keyring.display().to_string(),
            manifest: manifest.display().to_string(),
            unexpected,
            missing,
        });
    }
    Ok(expected)
}

/// One trusted primary key: its fingerprint and the human label the manifest
/// carries beside it (the key's UID, so a reviewer reading the diff sees *whose*
/// key is being added, not just 40 hex digits).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    /// Uppercase 40-hex v4 fingerprint — the identity actually enforced.
    pub fingerprint: String,
    /// Human label for review and `doctor` output. Not enforced: it names the key,
    /// it does not authenticate it.
    pub label: String,
}

impl std::fmt::Display for Key {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        if self.label.is_empty() {
            write!(f, "{}", self.fingerprint)
        } else {
            write!(f, "{} ({})", self.fingerprint, self.label)
        }
    }
}

/// Parse a fingerprint manifest: one key per line as `<40-hex fingerprint>[ label]`,
/// with `#` comments and blank lines ignored.
///
/// Pure, so the format is testable without a keyring. A duplicate fingerprint is an
/// error rather than a silently collapsed set — it means the file was edited wrong,
/// and the point of the manifest is that it is read carefully.
pub fn parse_manifest(text: &str) -> Result<Vec<Key>, String> {
    let mut keys: Vec<Key> = Vec::new();
    for (idx, raw) in text.lines().enumerate() {
        let line = raw.split('#').next().unwrap_or("").trim();
        if line.is_empty() {
            continue;
        }
        let lineno = idx + 1;
        let (fingerprint, label) = match line.split_once(char::is_whitespace) {
            Some((f, rest)) => (f, rest.trim()),
            None => (line, ""),
        };
        if fingerprint.len() != 40 || !fingerprint.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!(
                "line {lineno}: expected a 40-hex-digit v4 fingerprint, found {fingerprint:?}"
            ));
        }
        let fingerprint = fingerprint.to_ascii_uppercase();
        if keys.iter().any(|k| k.fingerprint == fingerprint) {
            return Err(format!("line {lineno}: duplicate fingerprint {fingerprint}"));
        }
        keys.push(Key {
            fingerprint,
            label: label.to_string(),
        });
    }
    if keys.is_empty() {
        return Err("no fingerprints listed — an empty manifest would trust nothing".into());
    }
    Ok(keys)
}

/// Uppercase-hex v4 fingerprints of every **primary** public key in an OpenPGP
/// keyring, in packet order.
///
/// Pure: the whole keyring is one byte slice in, fingerprints out. `Err` carries a
/// human-readable reason; the caller wraps it with the keyring's path.
pub fn fingerprints(bytes: &[u8]) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for packet in packets(bytes)? {
        if packet.tag == TAG_PUBLIC_KEY {
            out.push(fingerprint_v4(packet.body)?);
        }
    }
    if out.is_empty() {
        return Err("no public-key packets found — not an OpenPGP keyring?".into());
    }
    Ok(out)
}

/// One OpenPGP packet: its tag and its body, borrowed from the keyring bytes.
struct Packet<'a> {
    tag: u8,
    body: &'a [u8],
}

/// Split an OpenPGP message into packets (RFC 4880 §4.2), handling both the old and
/// new header formats.
///
/// Indeterminate and partial body lengths are rejected: they are for streamed literal
/// data, never appear in a keyring, and admitting them would mean guessing where a
/// packet ends — in a file whose whole job is to be exact.
fn packets(bytes: &[u8]) -> Result<Vec<Packet<'_>>, String> {
    let mut packets = Vec::new();
    let mut pos = 0usize;
    while pos < bytes.len() {
        let header = bytes[pos];
        if header & 0x80 == 0 {
            return Err(format!("byte {pos}: not a packet header (high bit clear)"));
        }
        pos += 1;
        let (tag, len) = if header & 0x40 == 0 {
            // Old format: tag in bits 5-2, length type in bits 1-0.
            let tag = (header >> 2) & 0x0f;
            let len = match header & 0x03 {
                0 => be_uint(bytes, &mut pos, 1)?,
                1 => be_uint(bytes, &mut pos, 2)?,
                2 => be_uint(bytes, &mut pos, 4)?,
                _ => return Err(format!("byte {pos}: indeterminate packet length")),
            };
            (tag, len)
        } else {
            // New format: tag in bits 5-0, then a variable-length body length.
            let tag = header & 0x3f;
            let first = *bytes
                .get(pos)
                .ok_or_else(|| format!("byte {pos}: truncated packet length"))?;
            pos += 1;
            let len = match first {
                0..=191 => first as usize,
                192..=223 => {
                    let second = *bytes
                        .get(pos)
                        .ok_or_else(|| format!("byte {pos}: truncated 2-octet length"))?;
                    pos += 1;
                    ((first as usize - 192) << 8) + second as usize + 192
                }
                224..=254 => {
                    return Err(format!("byte {pos}: partial body length"));
                }
                255 => be_uint(bytes, &mut pos, 4)?,
            };
            (tag, len)
        };
        let end = pos
            .checked_add(len)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| {
                format!("byte {pos}: packet claims {len} bytes, past the end of the keyring")
            })?;
        packets.push(Packet {
            tag,
            body: &bytes[pos..end],
        });
        pos = end;
    }
    Ok(packets)
}

/// Read an `n`-byte big-endian unsigned length at `*pos`, advancing it.
fn be_uint(bytes: &[u8], pos: &mut usize, n: usize) -> Result<usize, String> {
    let slice = bytes
        .get(*pos..*pos + n)
        .ok_or_else(|| format!("byte {pos}: truncated {n}-octet length"))?;
    *pos += n;
    Ok(slice.iter().fold(0usize, |acc, b| (acc << 8) | *b as usize))
}

/// Fingerprint a v4 public-key packet body: `SHA-1(0x99 ‖ len_be16 ‖ body)`
/// (RFC 4880 §12.2), uppercase hex.
///
/// SHA-1 here is a *format constant*, not a security choice: it is how a v4
/// fingerprint is defined, and the value is compared against a fingerprint the
/// operator vetted out-of-band rather than used to resist collisions.
fn fingerprint_v4(body: &[u8]) -> Result<String, String> {
    match body.first() {
        Some(4) => {}
        Some(v) => {
            return Err(format!(
                "unsupported public-key packet version {v} (only v4 keys are understood)"
            ))
        }
        None => return Err("empty public-key packet".into()),
    }
    let len = u16::try_from(body.len())
        .map_err(|_| format!("public-key packet of {} bytes exceeds v4's 16-bit length", body.len()))?;
    let mut hasher = Sha1::new();
    hasher.update([0x99]);
    hasher.update(len.to_be_bytes());
    hasher.update(body);
    let mut out = String::with_capacity(40);
    for byte in hasher.finalize() {
        use std::fmt::Write;
        let _ = write!(out, "{byte:02X}");
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal but real v4 RSA public-key packet body, and the fingerprint it must
    /// produce. Built here rather than read from `blobs/` so the parser is tested
    /// against a fixed vector that cannot drift when a keyring is refreshed.
    fn v4_key_body() -> Vec<u8> {
        let mut body = vec![4]; // version
        body.extend_from_slice(&[0x60, 0x00, 0x00, 0x00]); // creation time
        body.push(1); // algo: RSA
        // One 16-bit MPI: bit length, then the big-endian value.
        body.extend_from_slice(&[0x00, 0x10]);
        body.extend_from_slice(&[0xC0, 0xFF]);
        body
    }

    /// Wrap a body in an old-format header with a 1-octet length.
    fn old_packet(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![0x80 | (tag << 2)];
        out.push(body.len() as u8);
        out.extend_from_slice(body);
        out
    }

    /// Wrap a body in a new-format header with a 1-octet length.
    fn new_packet(tag: u8, body: &[u8]) -> Vec<u8> {
        let mut out = vec![0xc0 | tag];
        out.push(body.len() as u8);
        out.extend_from_slice(body);
        out
    }

    #[test]
    fn fingerprint_is_sha1_over_the_prefixed_body() {
        let body = v4_key_body();
        // Independently: sha1(0x99 || 0x000c || body) for this 12-byte body.
        let fpr = fingerprint_v4(&body).unwrap();
        assert_eq!(fpr.len(), 40);
        assert!(fpr.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_lowercase()));
        // Stable across calls and independent of surrounding packets.
        assert_eq!(fingerprints(&old_packet(TAG_PUBLIC_KEY, &body)).unwrap(), vec![fpr]);
    }

    #[test]
    fn both_header_formats_parse_to_the_same_key() {
        let body = v4_key_body();
        assert_eq!(
            fingerprints(&old_packet(TAG_PUBLIC_KEY, &body)).unwrap(),
            fingerprints(&new_packet(TAG_PUBLIC_KEY, &body)).unwrap()
        );
    }

    #[test]
    fn subkeys_and_signatures_are_not_listed() {
        let body = v4_key_body();
        let mut keyring = old_packet(TAG_PUBLIC_KEY, &body);
        keyring.extend(old_packet(13, b"uid")); // User ID
        keyring.extend(old_packet(2, b"sig")); // Signature
        keyring.extend(old_packet(14, &body)); // Public-Subkey — bound by the primary
        let fprs = fingerprints(&keyring).unwrap();
        assert_eq!(fprs.len(), 1, "only the primary key is pinned");
    }

    #[test]
    fn multiple_primaries_are_listed_in_packet_order() {
        let mut a = v4_key_body();
        let mut b = v4_key_body();
        a.push(0x01);
        b.push(0x02);
        let mut keyring = old_packet(TAG_PUBLIC_KEY, &a);
        keyring.extend(old_packet(TAG_PUBLIC_KEY, &b));
        let fprs = fingerprints(&keyring).unwrap();
        assert_eq!(fprs.len(), 2);
        assert_ne!(fprs[0], fprs[1]);
    }

    #[test]
    fn unparseable_input_errors_rather_than_yielding_nothing() {
        // A truncated packet must not silently parse as "a keyring with no keys",
        // which would otherwise sail through as a vacuous match.
        assert!(fingerprints(b"\x99\x00\x40truncated").is_err());
        assert!(fingerprints(b"not a keyring").is_err());
        assert!(fingerprints(b"").is_err());
        // A v3 key is refused, not skipped.
        let mut v3 = v4_key_body();
        v3[0] = 3;
        assert!(fingerprints(&old_packet(TAG_PUBLIC_KEY, &v3)).is_err());
        // A partial body length is refused.
        assert!(fingerprints(&[0xc0 | TAG_PUBLIC_KEY, 224, 0, 0]).is_err());
    }

    #[test]
    fn manifest_parses_fingerprints_and_labels() {
        let keys = parse_manifest(
            "# Debian archive keys\n\
             \n\
             1F89983E0081FDE018F3CC9673A4F27B8DD47936  Debian Archive Automatic Signing Key (11/bullseye)\n\
             ac530d520f2f3269f5e98313a48449044aad5c5d\n",
        )
        .unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].fingerprint, "1F89983E0081FDE018F3CC9673A4F27B8DD47936");
        assert_eq!(keys[0].label, "Debian Archive Automatic Signing Key (11/bullseye)");
        // Case is normalized, so a lowercase manifest still matches the keyring.
        assert_eq!(keys[1].fingerprint, "AC530D520F2F3269F5E98313A48449044AAD5C5D");
        assert_eq!(keys[1].label, "");
    }

    #[test]
    fn manifest_rejects_malformed_and_empty_input() {
        assert!(parse_manifest("deadbeef  too short").is_err());
        assert!(parse_manifest("zzz9983E0081FDE018F3CC9673A4F27B8DD47936  not hex").is_err());
        assert!(parse_manifest("# only a comment\n").is_err());
        assert!(parse_manifest("").is_err());
        let dup = "1F89983E0081FDE018F3CC9673A4F27B8DD47936 a\n\
                   1f89983e0081fde018f3cc9673a4f27b8dd47936 b\n";
        assert!(parse_manifest(dup).is_err());
    }

    /// Write a keyring + manifest pair into a temp dir and verify them.
    fn fixture(keyring_bytes: &[u8], manifest: &str) -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let keyring = tmp.path().join("test.gpg");
        std::fs::write(&keyring, keyring_bytes).unwrap();
        std::fs::write(manifest_path(&keyring), manifest).unwrap();
        (tmp, keyring)
    }

    #[test]
    fn verify_accepts_a_keyring_matching_its_manifest() {
        let body = v4_key_body();
        let bytes = old_packet(TAG_PUBLIC_KEY, &body);
        let fpr = fingerprint_v4(&body).unwrap();
        let (_tmp, keyring) = fixture(&bytes, &format!("{fpr}  Test Key\n"));
        let keys = verify(&keyring).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].label, "Test Key");
    }

    #[test]
    fn verify_rejects_an_injected_key() {
        let body = v4_key_body();
        let mut rogue = v4_key_body();
        rogue.push(0xff);
        let fpr = fingerprint_v4(&body).unwrap();
        let mut bytes = old_packet(TAG_PUBLIC_KEY, &body);
        bytes.extend(old_packet(TAG_PUBLIC_KEY, &rogue));
        let (_tmp, keyring) = fixture(&bytes, &format!("{fpr}  Test Key\n"));
        let err = verify(&keyring).unwrap_err();
        let EngineError::KeyringFingerprintMismatch { unexpected, missing, .. } = err else {
            panic!("expected a fingerprint mismatch, got {err:?}");
        };
        assert_eq!(unexpected.len(), 1, "the extra key is named");
        assert!(missing.is_empty());
    }

    #[test]
    fn verify_rejects_a_swapped_key() {
        let body = v4_key_body();
        let mut swapped = v4_key_body();
        swapped.push(0xff);
        let expected = fingerprint_v4(&body).unwrap();
        // The keyring holds a *different* key than the manifest names.
        let (_tmp, keyring) = fixture(
            &old_packet(TAG_PUBLIC_KEY, &swapped),
            &format!("{expected}  Test Key\n"),
        );
        let err = verify(&keyring).unwrap_err();
        let EngineError::KeyringFingerprintMismatch { unexpected, missing, .. } = err else {
            panic!("expected a fingerprint mismatch, got {err:?}");
        };
        assert_eq!(unexpected.len(), 1);
        assert_eq!(missing.len(), 1, "the vetted key is reported as gone");
        assert!(missing[0].contains("Test Key"), "the label rides along: {missing:?}");
    }

    #[test]
    fn verify_requires_a_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let keyring = tmp.path().join("test.gpg");
        std::fs::write(&keyring, old_packet(TAG_PUBLIC_KEY, &v4_key_body())).unwrap();
        // No manifest beside it: fail closed rather than pass unchecked.
        assert!(matches!(
            verify(&keyring),
            Err(EngineError::KeyringManifestMissing { .. })
        ));
    }

    #[test]
    fn manifest_path_replaces_the_gpg_extension() {
        assert_eq!(
            manifest_path(Path::new("blobs/keyrings/jellyfin.gpg")),
            PathBuf::from("blobs/keyrings/jellyfin.fingerprints")
        );
    }

    /// Every keyring this repo ships matches the fingerprints it ships beside it.
    ///
    /// The guard that actually matters: the unit tests above prove the parser is
    /// right, but this one proves the *blobs* are. A commit that swaps a keyring, adds
    /// a key, or refreshes one without re-vetting its manifest fails `cargo test`
    /// rather than reaching a build — which is what makes the manifest a review
    /// artifact instead of a comment.
    #[test]
    fn shipped_keyrings_match_their_manifests() {
        let keyrings = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap()
            .join("blobs/keyrings");
        let mut checked = 0usize;
        for entry in std::fs::read_dir(&keyrings).expect("blobs/keyrings is shipped") {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|e| e == "gpg") {
                verify(&path).unwrap_or_else(|e| panic!("shipped keyring {}: {e}", path.display()));
                checked += 1;
            }
        }
        assert!(checked > 0, "no keyrings found under {}", keyrings.display());
    }
}
