//! Vendored apt keyring auditing: enumerate an OpenPGP keyring's certificates and
//! hold them to a checked-in manifest.
//!
//! A keyring under `blobs/keyrings/` is a *trust anchor*. It decides whose `Release`
//! signatures the bootstrap accepts. As a binary blob it is also the one vendored
//! file a human cannot review. A diff reports `Bin 55918 -> 55934 bytes` and nothing
//! about which keys changed.
//!
//! So every vendored keyring ships a sibling `<name>.fingerprints` manifest naming
//! the primary keys it is allowed to contain. [`verify`] fails closed unless the
//! keyring holds exactly that set. Swapping a key now shows up in review as a line
//! of text. The trusted set is a list anyone can check against `debian.org` without
//! trusting this repo.
//!
//! Only **primary** keys are listed. A subkey cannot be added without a binding
//! signature from its primary key. The bootstrap's verifier checks that signature,
//! and an attacker cannot forge it without the primary secret. Pinning the primaries
//! therefore pins the whole certificate.
//!
//! The keyring is read by [`irongall`], the OpenPGP implementation the bootstrap's
//! own signature verifier is built on. It reads as a sequence of certificates, each
//! named by its primary key's fingerprint. A certificate irongall refuses, or packet
//! framing it cannot follow, is an error rather than a skipped certificate. A keyring
//! this module cannot fully account for is therefore never declared verified.
//!
//! A vetted certificate must also hold a key the verifier can use, judged by the
//! same policy the bootstrap judges a signature under. One that holds none is a
//! trust anchor that can never vouch for a release. [`verify`] refuses it, rather
//! than leaving it to fail the first bootstrap that needed it.

use crate::error::EngineError;
use irongall::cert;
use irongall::policy::Policy;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Extension of the fingerprint manifest that sits beside each vendored keyring.
const MANIFEST_EXT: &str = "fingerprints";

/// The manifest path for a keyring: `<keyring>.fingerprints` (replacing `.gpg`).
pub fn manifest_path(keyring: &Path) -> PathBuf {
    keyring.with_extension(MANIFEST_EXT)
}

/// Verify a vendored keyring against its sibling fingerprint manifest.
///
/// The manifest is **mandatory**. A vendored keyring with no manifest is
/// [`EngineError::KeyringManifestMissing`] rather than an unchecked pass, so
/// deleting the manifest cannot silently disable the check.
///
/// The keyring's primary-key set must equal the manifest's exactly. An extra key is
/// an injected trust anchor, a missing one is a stale manifest, and both are
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
    let expected =
        parse_manifest(&manifest_text).map_err(|reason| EngineError::KeyringManifestMalformed {
            manifest: manifest.display().to_string(),
            reason,
        })?;

    let bytes = std::fs::read(keyring).map_err(|source| EngineError::io(keyring, source))?;
    let actual = certificates(&bytes).map_err(|reason| EngineError::KeyringMalformed {
        keyring: keyring.display().to_string(),
        reason,
    })?;

    let expected_set: BTreeSet<&str> = expected.iter().map(|k| k.fingerprint.as_str()).collect();
    let actual_set: BTreeSet<&str> = actual.iter().map(|c| c.fingerprint.as_str()).collect();
    if expected_set != actual_set {
        let unexpected: Vec<String> = actual_set
            .difference(&expected_set)
            .map(|f| (*f).to_string())
            .collect();
        // Report the manifest's label alongside a key the keyring no longer contains:
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
    // Asked only of a keyring that matched, so the certificates named here are the
    // vetted ones and each has its manifest label.
    let unusable: Vec<String> = expected
        .iter()
        .filter(|k| {
            actual
                .iter()
                .any(|c| c.fingerprint == k.fingerprint && !c.usable)
        })
        .map(Key::to_string)
        .collect();
    if !unusable.is_empty() {
        return Err(EngineError::KeyringUnusable {
            keyring: keyring.display().to_string(),
            unusable,
        });
    }
    Ok(expected)
}

/// One trusted primary key: its fingerprint and the human label the manifest
/// contains beside it (the key's UID, so a reviewer reading the diff sees *whose*
/// key is being added, not just 40 hex digits).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Key {
    /// Uppercase hex fingerprint, 40 digits for a version 4 key and 64 for a version 6
    /// one — the identity actually enforced.
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

/// Parse a fingerprint manifest: one key per line as `<hex fingerprint>[ label]`, with
/// `#` comments and blank lines ignored. A fingerprint is 40 hex digits for a version 4
/// key and 64 for a version 6 one.
///
/// Pure, so the format is testable without a keyring. A duplicate fingerprint is an
/// error rather than a silently collapsed set. It means the file was edited wrong,
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
        if !matches!(fingerprint.len(), 40 | 64)
            || !fingerprint.bytes().all(|b| b.is_ascii_hexdigit())
        {
            return Err(format!(
                "line {lineno}: expected a fingerprint of 40 hex digits (a version 4 key) \
                 or 64 (version 6), found {fingerprint:?}"
            ));
        }
        let fingerprint = fingerprint.to_ascii_uppercase();
        if keys.iter().any(|k| k.fingerprint == fingerprint) {
            return Err(format!(
                "line {lineno}: duplicate fingerprint {fingerprint}"
            ));
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

/// Uppercase-hex fingerprints of every certificate's **primary** key in an OpenPGP
/// keyring, in keyring order.
///
/// Pure: the whole keyring is one byte slice in, fingerprints out. `Err` contains a
/// human-readable reason. The caller wraps it with the keyring's path.
pub fn fingerprints(bytes: &[u8]) -> Result<Vec<String>, String> {
    certificates(bytes).map(|read| read.into_iter().map(|c| c.fingerprint).collect())
}

/// One certificate of a keyring, as the audit reads it.
struct Certificate {
    /// Its primary key's fingerprint, uppercase hex.
    fingerprint: String,
    /// Whether it holds a key, primary or subkey, the bootstrap's verifier can use.
    usable: bool,
}

/// Read every certificate in a keyring, failing on the first one irongall refuses.
///
/// Usability is judged by irongall's default [`Policy`], which is the one the
/// bootstrap's verifier judges a signature under: RSA from 2048 bits, and Ed25519. It
/// is a property of the keys alone, not of the time, so a certificate that has since
/// expired is still usable here; whether it is still in force is the verifier's
/// question at the moment it verifies.
fn certificates(bytes: &[u8]) -> Result<Vec<Certificate>, String> {
    let policy = Policy::new();
    let mut read = Vec::new();
    for certificate in cert::certificates(bytes) {
        let certificate = certificate.map_err(|err| err.to_string())?;
        let usable = std::iter::once(certificate.primary())
            .chain(certificate.subkeys().iter().map(|subkey| subkey.key()))
            .any(|key| policy.accepts_key(key));
        read.push(Certificate {
            fingerprint: certificate.primary().fingerprint().to_string(),
            usable,
        });
    }
    if read.is_empty() {
        return Err("no certificates found — not an OpenPGP keyring?".into());
    }
    Ok(read)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// OpenPGP packet tags the fixtures are built from.
    const TAG_PUBLIC_KEY: u8 = 6;
    const TAG_USER_ID: u8 = 13;
    const TAG_PUBLIC_SUBKEY: u8 = 14;

    /// The public keys of RFC 8032's first two Ed25519 test vectors: published, valid
    /// points, so a key built on one is a key the verifier can use.
    const ED25519_A: [u8; 32] = [
        0xd7, 0x5a, 0x98, 0x01, 0x82, 0xb1, 0x0a, 0xb7, 0xd5, 0x4b, 0xfe, 0xd3, 0xc9, 0x64, 0x07,
        0x3a, 0x0e, 0xe1, 0x72, 0xf3, 0xda, 0xa6, 0x23, 0x25, 0xaf, 0x02, 0x1a, 0x68, 0xf7, 0x07,
        0x51, 0x1a,
    ];
    const ED25519_B: [u8; 32] = [
        0x3d, 0x40, 0x17, 0xc3, 0xe8, 0x43, 0x89, 0x5a, 0x92, 0xb7, 0x0a, 0xa7, 0x4d, 0x1b, 0x7e,
        0xbc, 0x9c, 0x98, 0x2c, 0xcf, 0x2e, 0xc4, 0x96, 0x8c, 0xc0, 0xcd, 0x55, 0xf1, 0x2a, 0xf4,
        0x66, 0x0c,
    ];

    /// A version 4 Ed25519 public-key packet body (RFC 9580 §5.5.2, §5.5.5.9). Built
    /// here rather than read from `blobs/` so the audit is tested against fixed vectors
    /// that cannot drift when a keyring is refreshed.
    fn v4_key_body(point: &[u8; 32]) -> Vec<u8> {
        let mut body = vec![4]; // version
        body.extend_from_slice(&[0x60, 0x00, 0x00, 0x00]); // creation time
        body.push(27); // algorithm: Ed25519
        body.extend_from_slice(point);
        body
    }

    /// A version 4 RSA public-key packet body with a 16-bit modulus: a key irongall
    /// reads and the verifier's policy refuses, being far under 2048 bits.
    fn v4_small_rsa_body() -> Vec<u8> {
        let mut body = vec![4, 0x60, 0x00, 0x00, 0x00, 1]; // version, time, RSA
        body.extend_from_slice(&[0x00, 0x10, 0xC0, 0xFF]); // n: a 16-bit MPI
        body.extend_from_slice(&[0x00, 0x11, 0x01, 0x00, 0x01]); // e: 65537
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

    /// The version 4 fingerprint, computed here from its definition rather than by the
    /// code under test: SHA-1 over `0x99`, the body's length in two octets, and the
    /// body (RFC 9580 §5.5.4.2).
    fn v4_fingerprint(body: &[u8]) -> String {
        use irongall::hash::{HashFunction as _, Sha1};
        let mut hash = Sha1::new();
        hash.update(&[0x99]);
        hash.update(&u16::try_from(body.len()).unwrap().to_be_bytes());
        hash.update(body);
        hash.finalize()
            .iter()
            .map(|byte| format!("{byte:02X}"))
            .collect()
    }

    #[test]
    fn fingerprint_is_sha1_over_the_prefixed_body() {
        let body = v4_key_body(&ED25519_A);
        assert_eq!(
            fingerprints(&old_packet(TAG_PUBLIC_KEY, &body)).unwrap(),
            vec![v4_fingerprint(&body)]
        );
    }

    #[test]
    fn both_header_formats_parse_to_the_same_key() {
        let body = v4_key_body(&ED25519_A);
        assert_eq!(
            fingerprints(&old_packet(TAG_PUBLIC_KEY, &body)).unwrap(),
            fingerprints(&new_packet(TAG_PUBLIC_KEY, &body)).unwrap()
        );
    }

    #[test]
    fn subkeys_and_user_ids_are_not_listed() {
        let mut keyring = old_packet(TAG_PUBLIC_KEY, &v4_key_body(&ED25519_A));
        keyring.extend(old_packet(TAG_USER_ID, b"Test Key"));
        // Bound by the primary, so pinned through it.
        keyring.extend(old_packet(TAG_PUBLIC_SUBKEY, &v4_key_body(&ED25519_B)));
        let fprs = fingerprints(&keyring).unwrap();
        assert_eq!(fprs.len(), 1, "only the primary key is pinned");
    }

    #[test]
    fn multiple_primaries_are_listed_in_keyring_order() {
        let a = v4_key_body(&ED25519_A);
        let b = v4_key_body(&ED25519_B);
        let mut keyring = old_packet(TAG_PUBLIC_KEY, &a);
        keyring.extend(old_packet(TAG_PUBLIC_KEY, &b));
        assert_eq!(
            fingerprints(&keyring).unwrap(),
            vec![v4_fingerprint(&a), v4_fingerprint(&b)]
        );
    }

    #[test]
    fn unparseable_input_errors_rather_than_yielding_nothing() {
        // A truncated packet must not silently parse as "a keyring with no keys",
        // which would otherwise sail through as a vacuous match.
        assert!(fingerprints(b"\x99\x00\x40truncated").is_err());
        assert!(fingerprints(b"not a keyring").is_err());
        assert!(fingerprints(b"").is_err());
        // A v3 key is refused, not skipped.
        let mut v3 = v4_key_body(&ED25519_A);
        v3[0] = 3;
        assert!(fingerprints(&old_packet(TAG_PUBLIC_KEY, &v3)).is_err());
        // A partial body length is refused.
        assert!(fingerprints(&[0xc0 | TAG_PUBLIC_KEY, 224, 0, 0]).is_err());
        // A refused certificate fails the whole read, though another one reads.
        let mut keyring = old_packet(TAG_PUBLIC_KEY, &v4_key_body(&ED25519_A));
        keyring.extend(old_packet(TAG_PUBLIC_KEY, &v3));
        assert!(fingerprints(&keyring).is_err());
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
        assert_eq!(
            keys[0].fingerprint,
            "1F89983E0081FDE018F3CC9673A4F27B8DD47936"
        );
        assert_eq!(
            keys[0].label,
            "Debian Archive Automatic Signing Key (11/bullseye)"
        );
        // Case is normalized, so a lowercase manifest still matches the keyring.
        assert_eq!(
            keys[1].fingerprint,
            "AC530D520F2F3269F5E98313A48449044AAD5C5D"
        );
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
        let body = v4_key_body(&ED25519_A);
        let bytes = old_packet(TAG_PUBLIC_KEY, &body);
        let fpr = v4_fingerprint(&body);
        let (_tmp, keyring) = fixture(&bytes, &format!("{fpr}  Test Key\n"));
        let keys = verify(&keyring).unwrap();
        assert_eq!(keys.len(), 1);
        assert_eq!(keys[0].label, "Test Key");
    }

    #[test]
    fn verify_rejects_an_injected_key() {
        let body = v4_key_body(&ED25519_A);
        let rogue = v4_key_body(&ED25519_B);
        let fpr = v4_fingerprint(&body);
        let mut bytes = old_packet(TAG_PUBLIC_KEY, &body);
        bytes.extend(old_packet(TAG_PUBLIC_KEY, &rogue));
        let (_tmp, keyring) = fixture(&bytes, &format!("{fpr}  Test Key\n"));
        let err = verify(&keyring).unwrap_err();
        let EngineError::KeyringFingerprintMismatch {
            unexpected,
            missing,
            ..
        } = err
        else {
            panic!("expected a fingerprint mismatch, got {err:?}");
        };
        assert_eq!(unexpected.len(), 1, "the extra key is named");
        assert!(missing.is_empty());
    }

    #[test]
    fn verify_rejects_a_swapped_key() {
        let body = v4_key_body(&ED25519_A);
        let swapped = v4_key_body(&ED25519_B);
        let expected = v4_fingerprint(&body);
        // The keyring holds a *different* key than the manifest names.
        let (_tmp, keyring) = fixture(
            &old_packet(TAG_PUBLIC_KEY, &swapped),
            &format!("{expected}  Test Key\n"),
        );
        let err = verify(&keyring).unwrap_err();
        let EngineError::KeyringFingerprintMismatch {
            unexpected,
            missing,
            ..
        } = err
        else {
            panic!("expected a fingerprint mismatch, got {err:?}");
        };
        assert_eq!(unexpected.len(), 1);
        assert_eq!(missing.len(), 1, "the vetted key is reported as gone");
        assert!(
            missing[0].contains("Test Key"),
            "the label is included: {missing:?}"
        );
    }

    /// A vetted certificate the verifier can do nothing with is refused even though the
    /// keyring matches its manifest exactly: pinned, it is a trust anchor that would fail
    /// the first bootstrap it was meant to verify.
    #[test]
    fn verify_refuses_a_certificate_no_signature_can_be_verified_with() {
        let usable = v4_key_body(&ED25519_A);
        let small = v4_small_rsa_body();
        let mut bytes = old_packet(TAG_PUBLIC_KEY, &usable);
        bytes.extend(old_packet(TAG_PUBLIC_KEY, &small));
        let (_tmp, keyring) = fixture(
            &bytes,
            &format!(
                "{}  Usable Key\n{}  Small Key\n",
                v4_fingerprint(&usable),
                v4_fingerprint(&small)
            ),
        );
        let err = verify(&keyring).unwrap_err();
        let EngineError::KeyringUnusable { unusable, .. } = err else {
            panic!("expected an unusable keyring, got {err:?}");
        };
        assert_eq!(
            unusable.len(),
            1,
            "only the small key is named: {unusable:?}"
        );
        assert!(unusable[0].contains("Small Key"), "{unusable:?}");
    }

    /// A certificate whose primary only certifies is usable through its subkey, which
    /// is what an archive key that signs with a subkey looks like.
    #[test]
    fn a_usable_subkey_makes_its_certificate_usable() {
        let primary = v4_small_rsa_body();
        let mut bytes = old_packet(TAG_PUBLIC_KEY, &primary);
        bytes.extend(old_packet(TAG_PUBLIC_SUBKEY, &v4_key_body(&ED25519_A)));
        let (_tmp, keyring) = fixture(&bytes, &format!("{}  Key\n", v4_fingerprint(&primary)));
        assert_eq!(verify(&keyring).unwrap().len(), 1);
    }

    #[test]
    fn manifest_accepts_a_version_6_fingerprint() {
        let v6 = "CB186C4F0609A697E4D52DFA6C722B0C1F1E27C18A56708F6525EC27BAD9ACC9";
        let keys = parse_manifest(&format!("{v6}  A v6 Key\n")).unwrap();
        assert_eq!(keys[0].fingerprint, v6);
    }

    #[test]
    fn verify_requires_a_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let keyring = tmp.path().join("test.gpg");
        std::fs::write(
            &keyring,
            old_packet(TAG_PUBLIC_KEY, &v4_key_body(&ED25519_A)),
        )
        .unwrap();
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
        assert!(
            checked > 0,
            "no keyrings found under {}",
            keyrings.display()
        );
    }
}
