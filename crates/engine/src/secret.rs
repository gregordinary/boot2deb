//! Per-image build secrets.
//!
//! The shipped image's default account gets a **unique per built image** first-boot
//! password, generated here from the kernel CSPRNG (`/dev/urandom`) so there is no
//! guessable root-capable login on the network before the forced change
//! (`passwd -e`). This is side-effecting (it reads the RNG), hence in the engine
//! rather than the pure core. A fresh secret per build deliberately places the
//! rootfs `/etc/shadow` outside the byte-reproducibility claim; the package
//! content-pin is unaffected.

use crate::error::EngineError;
use std::fs::File;
use std::io::Read;
use std::path::Path;
use std::process::Command;

/// Password alphabet: mixed case + digits with the visually ambiguous characters
/// (`0`/`O`/`o`, `1`/`l`/`I`) removed, so the one-time secret transcribes cleanly
/// at a console. All 56 symbols are shell-safe (no quoting/metacharacters), so the
/// value bakes directly into the customize-hook's `chpasswd` line. 56 symbols.
const ALPHABET: &[u8] = b"abcdefghijkmnpqrstuvwxyzABCDEFGHJKLMNPQRSTUVWXYZ23456789";
/// Generated password length. 20 symbols over the 56-symbol alphabet is ~116 bits
/// of entropy — unguessable within the first-boot window, and well beyond it.
const LEN: usize = 20;

/// Generate a fresh per-image password from `/dev/urandom`.
///
/// A 20-symbol string, uniform over the 56-symbol unambiguous alphabet by
/// rejection sampling: bytes at or above the largest multiple of the alphabet
/// length are discarded, so `byte % len` maps no symbol more often than another
/// (no modulo bias). Fails only if the CSPRNG cannot be read.
pub fn generate_password() -> Result<String, EngineError> {
    let n = ALPHABET.len();
    // Reject bytes >= this so `byte % n` is unbiased (each symbol equally likely).
    let limit = (256 / n) * n;
    let path = Path::new("/dev/urandom");
    let mut rng = File::open(path).map_err(|s| EngineError::io(path, s))?;
    let mut out = String::with_capacity(LEN);
    let mut buf = [0u8; 64];
    while out.len() < LEN {
        rng.read_exact(&mut buf).map_err(|s| EngineError::io(path, s))?;
        for &b in &buf {
            if out.len() == LEN {
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

/// Hash `pass` into a `sha512crypt` (`$6$`) entry for `/etc/shadow`, via
/// `openssl passwd -6` (a random salt per call, so the same password hashes
/// differently each time). The image stage splices the result into the default
/// account's shadow line; the plaintext is surfaced to the operator once and
/// committed nowhere.
pub(crate) fn crypt_password(pass: &str) -> Result<String, EngineError> {
    use std::io::Write;
    use std::process::Stdio;
    let mut child = Command::new("openssl")
        .args(["passwd", "-6", "-stdin"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|source| EngineError::CommandSpawn {
            command: "openssl".into(),
            context: "hash first-boot password".into(),
            source,
        })?;
    child
        .stdin
        .take()
        .expect("stdin was piped")
        .write_all(format!("{pass}\n").as_bytes())
        .map_err(|s| EngineError::io(Path::new("openssl-stdin"), s))?;
    let out = child
        .wait_with_output()
        .map_err(|source| EngineError::CommandSpawn {
            command: "openssl".into(),
            context: "hash first-boot password".into(),
            source,
        })?;
    if !out.status.success() {
        return Err(EngineError::CommandFailed {
            command: "openssl".into(),
            context: "hash first-boot password".into(),
            status: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    let hash = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !hash.starts_with("$6$") {
        return Err(EngineError::CommandFailed {
            command: "openssl".into(),
            context: "hash first-boot password".into(),
            status: None,
            stderr: format!("unexpected crypt output: {hash}"),
        });
    }
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_has_expected_shape() {
        let p = generate_password().unwrap();
        assert_eq!(p.chars().count(), LEN);
        // Every character is drawn from the unambiguous alphabet.
        for c in p.chars() {
            assert!(ALPHABET.contains(&(c as u8)), "char {c:?} not in alphabet");
        }
        // None of the excluded ambiguous characters leaked in.
        for bad in ['0', 'O', 'o', '1', 'l', 'I'] {
            assert!(!p.contains(bad), "ambiguous char {bad:?} present");
        }
    }

    #[test]
    fn passwords_are_unique() {
        // Two 116-bit draws colliding is a broken-RNG signal, not a flake.
        assert_ne!(generate_password().unwrap(), generate_password().unwrap());
    }

    #[test]
    fn crypt_password_produces_a_sha512crypt_hash() {
        // openssl is a checked host dep (doctor); skip if absent.
        if Command::new("openssl")
            .arg("version")
            .output()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            eprintln!("skipping: openssl unavailable");
            return;
        }
        let hash = crypt_password("Example116BitSecret").unwrap();
        assert!(hash.starts_with("$6$"), "sha512crypt hash, got {hash}");
        // Same password, different salt each call (openssl randomizes) — not reused.
        let again = crypt_password("Example116BitSecret").unwrap();
        assert_ne!(hash, again);
    }
}
