//! Per-image build secrets.
//!
//! The shipped image's default account gets a **unique per built image** first-boot
//! password, generated here from the kernel CSPRNG (`/dev/urandom`) so there is no
//! guessable root-capable login on the network before the forced change
//! (`passwd -e`). This is side-effecting (it reads the RNG), hence in the engine
//! rather than the pure core. A fresh secret per build deliberately places the
//! rootfs `/etc/shadow` outside the byte-reproducibility claim; the package
//! content-pin is unaffected.
//!
//! Expiry forces the *operator* to replace the password; it does not hold off anyone
//! else, since a login against an expired account is allowed to set the new password.
//! That is what the length is for, and why the length is a validated config value
//! rather than a preference: an unguessable secret is the only thing standing between
//! the board and whoever else reaches it first.
//!
//! Both the draw and the hash are in-process: nothing on the credential path shells
//! out to a host binary, so what lands in the image's `/etc/shadow` does not depend
//! on which host built it.

use crate::error::EngineError;
use sha_crypt::{PasswordHasher, ShaCrypt};
use std::fs::File;
use std::io::Read;
use std::path::Path;

/// Password alphabet: mixed case + digits with the visually ambiguous characters
/// (`0`/`O`/`o`, `1`/`l`/`I`) removed, so the one-time secret transcribes cleanly
/// at a console. All 56 symbols are shell-safe (no quoting/metacharacters), so the
/// value bakes directly into the customize-hook's `chpasswd` line. 56 symbols.
const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
/// Raw salt bytes drawn per hash. `sha512crypt` encodes the salt in its 6-bit crypt
/// alphabet, so 12 bytes become the 16 salt characters the format allows at most.
const SALT_BYTES: usize = 12;

/// Fill `buf` from the kernel CSPRNG. Fails only if `/dev/urandom` cannot be read,
/// which on Linux means something is wrong that no fallback should paper over.
fn fill_random(buf: &mut [u8]) -> Result<(), EngineError> {
    let path = Path::new("/dev/urandom");
    File::open(path)
        .and_then(|mut f| f.read_exact(buf))
        .map_err(|s| EngineError::io(path, s))
}

/// Generate a fresh per-image password of `len` symbols from `/dev/urandom`.
///
/// Uniform over the 56-symbol unambiguous alphabet by rejection sampling: bytes at or
/// above the largest multiple of the alphabet length are discarded, so `byte % len`
/// maps no symbol more often than another (no modulo bias). Each symbol carries
/// log2(56) ≈ 5.81 bits, so the length the config resolved to *is* the entropy
/// statement. Fails only if the CSPRNG cannot be read.
///
/// `len` comes from
/// [`ResolvedBuild::first_boot_password_length`](boot2deb_core::model::ResolvedBuild::first_boot_password_length),
/// which resolution has already bounded — so this imposes no floor of its own. It has
/// no business second-guessing a validated config value, and a second, quieter bound
/// here would be a place for the two to disagree.
pub fn generate_password(len: usize) -> Result<String, EngineError> {
    let n = ALPHABET.len();
    // Reject bytes >= this so `byte % n` is unbiased (each symbol equally likely).
    let limit = (256 / n) * n;
    let mut out = String::with_capacity(len);
    let mut buf = [0u8; 64];
    while out.len() < len {
        fill_random(&mut buf)?;
        for &b in &buf {
            if out.len() == len {
                break;
            }
            let b = b as usize;
            if b < limit {
                out.push(ALPHABET[b % n] as char);
            }
        }
    }
    Ok(out)
}

/// Hash `pass` into a `sha512crypt` (`$6$`) entry for `/etc/shadow`, over a fresh
/// random salt — so the same password hashes differently each time.
///
/// Computed in-process by the pure-Rust `sha-crypt` implementation at the format's
/// standard 5000 rounds. The alternative, `openssl passwd -6`, would put a host
/// binary on the one security-relevant path in an otherwise in-process pipeline, and
/// it is not portable: LibreSSL's `openssl passwd` has no `-6`, so a macOS host would
/// fail here at image assembly with the whole build already done.
///
/// The emitted string states its round count (`$6$rounds=5000$…`) rather than leaving
/// it implied. glibc, musl, and `libxcrypt` all read that form and it is what the
/// account's own hash records — an explicit parameter beats a defaulted one in a file
/// that outlives the tool that wrote it.
///
/// The image stage splices the result into the default account's shadow line; the
/// plaintext is surfaced to the operator once and committed nowhere.
pub(crate) fn crypt_password(pass: &str) -> Result<String, EngineError> {
    let mut salt = [0u8; SALT_BYTES];
    fill_random(&mut salt)?;
    let hash = ShaCrypt::SHA512
        .hash_password_with_salt(pass.as_bytes(), &salt)
        .map_err(|e| EngineError::Secret {
            context: "hash the first-boot password".into(),
            message: e.to_string(),
        })?;
    let hash = hash.as_str().to_string();
    // The prefix is the one thing `/etc/shadow` reads to pick the algorithm, so a
    // string that is not `$6$` would silently install as something else entirely.
    if !hash.starts_with("$6$") {
        return Err(EngineError::Secret {
            context: "hash the first-boot password".into(),
            message: format!("expected a sha512crypt ($6$) hash, got {hash}"),
        });
    }
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The length the config asked for is the length that comes out, across the range
    /// resolution admits and at both of its ends — the generator refills its random
    /// buffer in 64-byte blocks, so a length near or above one block is a distinct case
    /// from a short one.
    #[test]
    fn password_has_the_requested_length_and_alphabet() {
        use boot2deb_core::model::{
            DEFAULT_PASSWORD_LENGTH, MAX_PASSWORD_LENGTH, MIN_PASSWORD_LENGTH,
        };
        for len in [
            MIN_PASSWORD_LENGTH,
            DEFAULT_PASSWORD_LENGTH,
            32,
            MAX_PASSWORD_LENGTH,
        ] {
            let p = generate_password(len as usize).unwrap();
            assert_eq!(p.chars().count(), len as usize, "length {len}");
            // Every character is drawn from the unambiguous alphabet.
            for c in p.chars() {
                assert!(ALPHABET.contains(&(c as u8)), "char {c:?} not in alphabet");
            }
            // None of the excluded ambiguous characters leaked in.
            for bad in ['0', 'O', 'o', '1', 'l', 'I'] {
                assert!(!p.contains(bad), "ambiguous char {bad:?} present");
            }
        }
    }

    #[test]
    fn passwords_are_unique() {
        // Two 70-bit draws colliding is a broken-RNG signal, not a flake.
        let len = boot2deb_core::model::DEFAULT_PASSWORD_LENGTH as usize;
        assert_ne!(
            generate_password(len).unwrap(),
            generate_password(len).unwrap()
        );
    }

    /// The hash the image's `/etc/shadow` carries: `sha512crypt`, salted per call,
    /// and computed with no host tool involved (so this test cannot skip).
    #[test]
    fn crypt_password_produces_a_sha512crypt_hash() {
        let hash = crypt_password("Example116BitSecret").unwrap();
        assert!(hash.starts_with("$6$"), "sha512crypt hash, got {hash}");
        // Same password, fresh salt each call — two accounts never share a hash.
        let again = crypt_password("Example116BitSecret").unwrap();
        assert_ne!(hash, again);
        // The shape `/etc/shadow` and every crypt(3) reader parse:
        // `$6$rounds=5000$<salt>$<checksum>`, salt and checksum in the crypt alphabet.
        let fields: Vec<&str> = hash.split('$').collect();
        assert_eq!(fields.len(), 5, "five `$`-separated fields, got {hash}");
        assert_eq!(fields[1], "6");
        assert_eq!(fields[2], "rounds=5000");
        assert_eq!(fields[3].len(), 16, "the format's maximum salt length");
        assert_eq!(fields[4].len(), 86, "a base64 sha512 checksum");
        for field in [fields[3], fields[4]] {
            assert!(
                field
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'/'),
                "outside the crypt(3) alphabet: {field}"
            );
        }
    }

    /// The hash a board's `login` recomputes must be the one this build wrote.
    ///
    /// Shape alone proves nothing: an implementation that emitted *a* well-formed
    /// `$6$` string over the wrong digest would lock every image out of its own
    /// console, with a build that reported success. Two properties are asserted:
    ///
    ///  1. **Known answer.** The reference vector from Ulrich Drepper's sha512crypt
    ///     specification — the algorithm glibc, musl, and `libxcrypt` implement —
    ///     verifies. That fixes the digest, the round count, and the base64 ordering
    ///     against something outside this codebase.
    ///  2. **Round trip.** A hash this module produces verifies for its own password
    ///     and not for another, which is exactly what `login` does on the board.
    #[test]
    fn the_hash_is_the_one_a_board_will_check_against() {
        use sha_crypt::{PasswordHashRef, PasswordVerifier, ShaCrypt};

        let reference = "$6$saltstring$svn8UoSVapNtMuq1ukKS4tPQd8iKwSMHWjl/O817G3uBnIF\
                         NjnQJuesI68u4OTLiBFdcbYEdFCoEOfaS35inz1";
        let parsed = PasswordHashRef::new(reference).expect("the spec vector parses");
        ShaCrypt::SHA512
            .verify_password(b"Hello world!", parsed)
            .expect("the spec vector's own password verifies");
        assert!(
            ShaCrypt::SHA512
                .verify_password(b"Hello world", parsed)
                .is_err(),
            "a different password must not verify"
        );

        let mine = crypt_password("Example116BitSecret").unwrap();
        let mine = PasswordHashRef::new(&mine).expect("what we write parses as crypt(3)");
        ShaCrypt::SHA512
            .verify_password(b"Example116BitSecret", mine)
            .expect("the account's own password opens the account");
        assert!(
            ShaCrypt::SHA512.verify_password(b"wrong", mine).is_err(),
            "another password must not open it"
        );
    }
}
