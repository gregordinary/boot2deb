//! The ChromeOS verified-boot (vboot) kernel partition, read, verified, and re-signed
//! under a new kernel command line, in process.
//!
//! A press that gives its image a fresh identity changes the `root=` the kernel boots
//! with ([`press::identity`](crate::press::identity)). On a depthcharge board that value
//! sits inside the signed partition, so changing it is a re-sign. This module does that
//! with no sandbox, no host `futility`, and no rootfs to run one in. It is pure: bytes in,
//! bytes out.
//!
//! A partition is three regions laid end to end:
//!
//! - The **keyblock** carries the *data key*, the public half of the key that signs
//!   everything after it. The firmware checks the keyblock against its own kernel subkey,
//!   and a re-sign carries it unchanged.
//! - The **preamble** records where the body loads and where its bootloader stub sits,
//!   and carries the body's signature. Its own signature covers its header and that body
//!   signature.
//! - The **body** is the kernel blob (a FIT on these boards), a 4 KiB area holding the
//!   NUL-terminated command line, a 4 KiB parameters area, and the bootloader stub.
//!
//! The command line lives in the signed body. Changing it means re-signing the body, then
//! the preamble whose signature covers the body's. Nothing else moves: every size, every
//! offset, the keyblock, and every other byte of the body stay as they were.
//!
//! Signatures are RSASSA-PKCS1-v1_5 over SHA-256, which is deterministic. The same bytes
//! signed with the same key give the same signature. The tests therefore hold this module
//! to partitions `futility` packed and repacked, byte for byte.

use crate::error::EngineError;
use rsa::pkcs1::DecodeRsaPrivateKey;
use rsa::traits::PublicKeyParts;
use rsa::{BigUint, Pkcs1v15Sign, RsaPrivateKey, RsaPublicKey};
use sha2::{Digest, Sha256};
use std::ops::Range;

/// The keyblock magic, the first eight bytes of every signed kernel partition.
const MAGIC: &[u8] = b"CHROMEOS";

/// The command-line area's size, `CROS_CONFIG_SIZE` in `futility`. The line is
/// NUL-terminated inside it, so it holds one byte less than this.
const CONFIG_BYTES: usize = 4096;

/// The parameters area between the command line and the bootloader stub,
/// `CROS_PARAMS_SIZE` in `futility`. It holds x86 boot parameters and is zero on ARM.
const PARAMS_BYTES: usize = 4096;

/// The public exponent of every vboot key. A packed key stores only the modulus and two
/// Montgomery constants, because the firmware's verifier fixes the exponent at F4.
const PUBLIC_EXPONENT: u32 = 65_537;

/// The largest modulus a vboot algorithm uses, in bits.
const MAX_MODULUS_BITS: usize = 8192;

/// The vboot structure version this module reads, for both the keyblock and the
/// preamble. Every depthcharge firmware boots version 2.
const STRUCT_VERSION_MAJOR: u32 = 2;

// Field offsets, from vboot's own structure definitions. Every field is little-endian.
// Each size, offset, and address is a 32-bit value followed by 32 reserved bits, or a
// 64-bit value, so each is read here as a u64. The two version fields are plain u32s.

/// Keyblock: the header version's major number.
const KB_VERSION_MAJOR: usize = 8;
/// Keyblock: its total size, padding included.
const KB_SIZE: usize = 16;
/// Keyblock: the packed data key's header.
const KB_DATA_KEY: usize = 80;
/// Packed key header: the key data's offset from the header.
const KEY_OFFSET: usize = 0;
/// Packed key header: the key data's size.
const KEY_SIZE: usize = 8;
/// Packed key header: the vboot algorithm id.
const KEY_ALGORITHM: usize = 16;
/// Packed key header: its size.
const KEY_HEADER: usize = 32;
/// Preamble: its total size, padding included.
const PRE_SIZE: usize = 0;
/// Preamble: the signature over the preamble.
const PRE_SIGNATURE: usize = 8;
/// Preamble: the header version's major number.
const PRE_VERSION_MAJOR: usize = 32;
/// Preamble: the address the body is loaded at.
const PRE_BODY_LOAD: usize = 48;
/// Preamble: the bootloader stub's address, in the same address space as the body's.
const PRE_BOOTLOADER: usize = 56;
/// Preamble: the signature over the body.
const PRE_BODY_SIGNATURE: usize = 72;
/// A signature header's size: the signature's offset from the header, its size, and the
/// length of the data it signs.
const SIG_HEADER: usize = 24;
/// The preamble header bytes this module reads, which end with the body signature's
/// header.
const PRE_HEADER: usize = PRE_BODY_SIGNATURE + SIG_HEADER;

/// The modulus size, in bits, that a vboot algorithm id names, for the ids that hash with
/// SHA-256. vboot numbers its algorithms by modulus and then by hash: SHA-1, SHA-256,
/// SHA-512. SHA-256 is the hash every depthcharge kernel key uses, so it is the one this
/// module signs with, and the others are refused by id.
fn sha256_modulus_bits(algorithm: u64) -> Option<usize> {
    match algorithm {
        1 => Some(1024),
        4 => Some(2048),
        7 => Some(4096),
        10 => Some(8192),
        _ => None,
    }
}

/// A signed kernel partition, parsed and with both of its signatures verified.
///
/// Every range is an absolute byte range of the partition, checked to lie inside it, so
/// nothing after [`parse`](Self::parse) indexes out of bounds.
#[derive(Debug, Clone)]
pub(crate) struct KernelPartition {
    /// The whole partition image, as written to a kernel slot.
    bytes: Vec<u8>,
    /// The vboot algorithm the keyblock's data key declares.
    algorithm: u64,
    /// The keyblock's data key, which verifies both signatures.
    data_key: RsaPublicKey,
    /// The body: the kernel blob, the command line, the parameters, the stub.
    body: Range<usize>,
    /// The command-line area inside the body.
    config: Range<usize>,
    /// Where the body's signature is stored, inside the preamble.
    body_signature: Range<usize>,
    /// The part of the preamble its own signature covers, the body signature included.
    preamble_signed: Range<usize>,
    /// Where the preamble's signature is stored, outside the part it covers.
    preamble_signature: Range<usize>,
}

impl KernelPartition {
    /// Parse a signed kernel partition and verify both of its signatures against the
    /// data key its keyblock carries.
    ///
    /// Verification comes first because it proves the layout was read correctly. A
    /// partition whose fields were misread fails here instead of being re-signed into
    /// something the firmware rejects.
    ///
    /// # Errors
    ///
    /// [`EngineError::KpartResign`] when the bytes are not a vboot kernel partition of
    /// structure version 2, are truncated, name an algorithm that does not hash with
    /// SHA-256, or carry a signature that does not verify.
    pub(crate) fn parse(bytes: Vec<u8>) -> Result<Self, EngineError> {
        if !bytes.starts_with(MAGIC) {
            return Err(resign_err(
                "it does not start with the vboot keyblock magic, so it is not a signed \
                 kernel partition",
            ));
        }
        let keyblock_version = u32_at(&bytes, KB_VERSION_MAJOR)?;
        if keyblock_version != STRUCT_VERSION_MAJOR {
            return Err(resign_err(format!(
                "its keyblock is structure version {keyblock_version}, and only version \
                 {STRUCT_VERSION_MAJOR} is read"
            )));
        }
        let keyblock_len = usize_at(&bytes, KB_SIZE)?;
        let keyblock = within(&bytes, 0..keyblock_len, "the keyblock")?;

        let algorithm = u64_at(keyblock, KB_DATA_KEY + KEY_ALGORITHM)?;
        let bits = sha256_modulus_bits(algorithm).ok_or_else(|| {
            resign_err(format!(
                "its data key is vboot algorithm {algorithm}, which does not hash with \
                 SHA-256"
            ))
        })?;
        let key_start = KB_DATA_KEY
            .checked_add(usize_at(keyblock, KB_DATA_KEY + KEY_OFFSET)?)
            .ok_or_else(|| resign_err("its data key's offset overflows"))?;
        let key_len = usize_at(keyblock, KB_DATA_KEY + KEY_SIZE)?;
        let key_data = within(keyblock, span(key_start, key_len)?, "the data key")?;
        if key_start < KB_DATA_KEY + KEY_HEADER {
            return Err(resign_err("its data key overlaps the key header"));
        }
        let data_key = packed_public_key(key_data, bits)?;
        let signature_len = bits / 8;

        let preamble_start = keyblock_len;
        let preamble_len = usize_at(&bytes, preamble_start + PRE_SIZE)?;
        let preamble = span(preamble_start, preamble_len)?;
        within(&bytes, preamble.clone(), "the preamble")?;
        if preamble_len < PRE_HEADER {
            return Err(resign_err(format!(
                "its preamble is {preamble_len} bytes, shorter than the {PRE_HEADER}-byte \
                 header"
            )));
        }
        let preamble_version = u32_at(&bytes, preamble_start + PRE_VERSION_MAJOR)?;
        if preamble_version != STRUCT_VERSION_MAJOR {
            return Err(resign_err(format!(
                "its preamble is structure version {preamble_version}, and only version \
                 {STRUCT_VERSION_MAJOR} is read"
            )));
        }
        let (preamble_signature, preamble_signed_len) = signature_at(
            &bytes,
            preamble_start + PRE_SIGNATURE,
            &preamble,
            signature_len,
        )?;
        let preamble_signed = span(preamble_start, preamble_signed_len)?;
        if preamble_signed.end > preamble.end {
            return Err(resign_err(
                "its preamble signature covers more than the preamble",
            ));
        }
        if overlaps(&preamble_signature, &preamble_signed) {
            return Err(resign_err(
                "its preamble signature lies inside the bytes it signs",
            ));
        }
        let (body_signature, body_len) = signature_at(
            &bytes,
            preamble_start + PRE_BODY_SIGNATURE,
            &preamble,
            signature_len,
        )?;
        let body = span(preamble.end, body_len)?;
        within(&bytes, body.clone(), "the body")?;

        // The command line sits just below the parameters area, which sits just below
        // the stub. The preamble states the stub's address and the body's, so the
        // distance between the two places the command line within the body.
        let load = u64_at(&bytes, preamble_start + PRE_BODY_LOAD)?;
        let stub = u64_at(&bytes, preamble_start + PRE_BOOTLOADER)?;
        let config = stub
            .checked_sub(load)
            .and_then(|stub| usize::try_from(stub).ok())
            .and_then(|stub| stub.checked_sub(PARAMS_BYTES + CONFIG_BYTES))
            .map(|start| body.start + start..body.start + start + CONFIG_BYTES)
            .filter(|config| config.end <= body.end)
            .ok_or_else(|| {
                resign_err(format!(
                    "its bootloader stub at {stub:#x} leaves no command-line area in a body \
                     of {body_len} bytes loaded at {load:#x}"
                ))
            })?;

        let partition = KernelPartition {
            bytes,
            algorithm,
            data_key,
            body,
            config,
            body_signature,
            preamble_signed,
            preamble_signature,
        };
        partition.verify()?;
        Ok(partition)
    }

    /// The kernel command line, read up to its terminating NUL.
    ///
    /// # Errors
    ///
    /// [`EngineError::KpartResign`] when the command-line area has no terminator or is not
    /// UTF-8.
    pub(crate) fn cmdline(&self) -> Result<&str, EngineError> {
        let area = &self.bytes[self.config.clone()];
        let end = area
            .iter()
            .position(|&b| b == 0)
            .ok_or_else(|| resign_err("its command line is not NUL-terminated"))?;
        std::str::from_utf8(&area[..end]).map_err(|_| resign_err("its command line is not UTF-8"))
    }

    /// Replace the command line with `cmdline`, then re-sign the body and the preamble
    /// with `key` and verify both again.
    ///
    /// The re-verification uses the keyblock's data key, not `key`, so it proves the
    /// firmware's own check passes. The keyblock and every byte outside the command-line
    /// area and the two signatures are left as they were. A refused re-sign leaves the
    /// partition unchanged.
    ///
    /// # Errors
    ///
    /// [`EngineError::KpartResign`] when `key` is not the key the partition was signed
    /// with, or when `cmdline` holds a NUL or does not fit the command-line area with its
    /// terminator.
    pub(crate) fn resign(&mut self, cmdline: &str, key: &SigningKey) -> Result<(), EngineError> {
        if key.algorithm != self.algorithm {
            return Err(resign_err(format!(
                "the image's signing key is vboot algorithm {}, and the kernel was signed \
                 with algorithm {}",
                key.algorithm, self.algorithm
            )));
        }
        if key.key.n() != self.data_key.n() {
            return Err(resign_err(
                "the image's signing key is not the key the kernel was signed with",
            ));
        }
        if cmdline.contains('\0') {
            return Err(resign_err("the new command line holds a NUL byte"));
        }
        if cmdline.len() >= CONFIG_BYTES {
            return Err(resign_err(format!(
                "the new command line is {} bytes, and the command-line area holds {} with \
                 its terminator",
                cmdline.len(),
                CONFIG_BYTES - 1
            )));
        }

        let mut bytes = self.bytes.clone();
        let area = &mut bytes[self.config.clone()];
        area.fill(0);
        area[..cmdline.len()].copy_from_slice(cmdline.as_bytes());
        let body_signature = key.sign(&bytes[self.body.clone()])?;
        bytes[self.body_signature.clone()].copy_from_slice(&body_signature);
        // Signed second, because the bytes it covers include the body signature.
        let preamble_signature = key.sign(&bytes[self.preamble_signed.clone()])?;
        bytes[self.preamble_signature.clone()].copy_from_slice(&preamble_signature);

        let resigned = KernelPartition {
            bytes,
            ..self.clone()
        };
        resigned.verify()?;
        *self = resigned;
        Ok(())
    }

    /// The partition image, as written to a kernel slot.
    pub(crate) fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    /// Verify the body signature, then the preamble signature, against the data key.
    fn verify(&self) -> Result<(), EngineError> {
        let check = |data: &Range<usize>, signature: &Range<usize>, what: &str| {
            self.data_key
                .verify(
                    Pkcs1v15Sign::new::<Sha256>(),
                    &Sha256::digest(&self.bytes[data.clone()]),
                    &self.bytes[signature.clone()],
                )
                .map_err(|_| {
                    resign_err(format!(
                        "its {what} signature does not verify against its own data key"
                    ))
                })
        };
        check(&self.body, &self.body_signature, "body")?;
        check(&self.preamble_signed, &self.preamble_signature, "preamble")
    }
}

/// A kernel data key's private half, read from a `.vbprivk` file.
pub(crate) struct SigningKey {
    /// The vboot algorithm id the file declares.
    algorithm: u64,
    /// The RSA key itself.
    key: RsaPrivateKey,
}

impl SigningKey {
    /// Read a `.vbprivk` file: the vboot algorithm id as a little-endian u64, then the
    /// RSA private key in PKCS#1 DER.
    ///
    /// # Errors
    ///
    /// [`EngineError::KpartResign`] when the file is shorter than its header or its key
    /// does not decode.
    pub(crate) fn from_vbprivk(bytes: &[u8]) -> Result<Self, EngineError> {
        let algorithm = u64_at(bytes, 0)?;
        let key = RsaPrivateKey::from_pkcs1_der(&bytes[8..]).map_err(|e| {
            resign_err(format!(
                "the signing key does not decode as an RSA key: {e}"
            ))
        })?;
        Ok(SigningKey { algorithm, key })
    }

    /// Sign `data`: RSASSA-PKCS1-v1_5 over its SHA-256 digest, as long as the modulus.
    fn sign(&self, data: &[u8]) -> Result<Vec<u8>, EngineError> {
        self.key
            .sign(Pkcs1v15Sign::new::<Sha256>(), &Sha256::digest(data))
            .map_err(|e| resign_err(format!("signing failed: {e}")))
    }
}

/// Decode a vboot packed RSA public key, `bits` long.
///
/// The key data is a word count, the Montgomery constant `-1/n[0] mod 2^32`, the modulus
/// as that many little-endian 32-bit words, then `R^2 mod n` in the same form. Only the
/// modulus is needed here, because the exponent is fixed.
fn packed_public_key(data: &[u8], bits: usize) -> Result<RsaPublicKey, EngineError> {
    let words = usize::try_from(u32_at(data, 0)?).unwrap_or(usize::MAX);
    if words.checked_mul(32) != Some(bits) {
        return Err(resign_err(format!(
            "its data key holds {words} modulus words, and its algorithm names a {bits}-bit \
             modulus"
        )));
    }
    let modulus = within(data, span(8, words * 4)?, "the data key's modulus")?;
    RsaPublicKey::new_with_max_size(
        BigUint::from_bytes_le(modulus),
        BigUint::from(PUBLIC_EXPONENT),
        MAX_MODULUS_BITS,
    )
    .map_err(|e| resign_err(format!("its data key is not a usable RSA key: {e}")))
}

/// Read a signature header at `at`: the signature's own range and the length of the data
/// it signs. The signature must lie inside `container` and be `len` bytes, the modulus
/// size.
fn signature_at(
    bytes: &[u8],
    at: usize,
    container: &Range<usize>,
    len: usize,
) -> Result<(Range<usize>, usize), EngineError> {
    let start = at
        .checked_add(usize_at(bytes, at)?)
        .ok_or_else(|| resign_err("a signature offset overflows"))?;
    let size = usize_at(bytes, at + 8)?;
    let signed = usize_at(bytes, at + 16)?;
    if size != len {
        return Err(resign_err(format!(
            "a signature is {size} bytes, and its key's signatures are {len}"
        )));
    }
    let range = span(start, size)?;
    if range.start < container.start || range.end > container.end {
        return Err(resign_err("a signature lies outside the preamble"));
    }
    Ok((range, signed))
}

/// `start..start + len`, refusing an overflow.
fn span(start: usize, len: usize) -> Result<Range<usize>, EngineError> {
    start
        .checked_add(len)
        .map(|end| start..end)
        .ok_or_else(|| resign_err("a field's length overflows"))
}

/// `bytes[range]`, or an error naming `what` when the range runs past the end.
fn within<'a>(bytes: &'a [u8], range: Range<usize>, what: &str) -> Result<&'a [u8], EngineError> {
    bytes.get(range).ok_or_else(|| {
        resign_err(format!(
            "{what} runs past the end of the {}-byte partition",
            bytes.len()
        ))
    })
}

/// Whether two ranges share a byte.
fn overlaps(a: &Range<usize>, b: &Range<usize>) -> bool {
    a.start < b.end && b.start < a.end
}

/// The little-endian u64 at `at`.
fn u64_at(bytes: &[u8], at: usize) -> Result<u64, EngineError> {
    let field = within(bytes, span(at, 8)?, "a header field")?;
    Ok(u64::from_le_bytes(field.try_into().expect("eight bytes")))
}

/// The little-endian u32 at `at`.
fn u32_at(bytes: &[u8], at: usize) -> Result<u32, EngineError> {
    let field = within(bytes, span(at, 4)?, "a header field")?;
    Ok(u32::from_le_bytes(field.try_into().expect("four bytes")))
}

/// The little-endian u64 at `at`, as a size or an offset in this address space.
fn usize_at(bytes: &[u8], at: usize) -> Result<usize, EngineError> {
    usize::try_from(u64_at(bytes, at)?).map_err(|_| resign_err("a size field overflows"))
}

/// A [`KpartResign`](EngineError::KpartResign) error.
fn resign_err(detail: impl Into<String>) -> EngineError {
    EngineError::KpartResign {
        detail: detail.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in kernel packed and signed by `futility` with the developer data key.
    const SIGNED: &[u8] = include_bytes!("testdata/vboot/signed.kpart");
    /// [`SIGNED`] repacked by `futility` with only its command line changed.
    const RESIGNED: &[u8] = include_bytes!("testdata/vboot/resigned.kpart");
    /// The vboot developer kernel data key, which signed both partitions.
    const DATA_KEY: &[u8] = include_bytes!("testdata/vboot/kernel_data_key.vbprivk");
    /// A different published developer key, which signed neither.
    const OTHER_KEY: &[u8] = include_bytes!("testdata/vboot/kernel_subkey.vbprivk");

    const SIGNED_CMDLINE: &str = "kern_guid=%U console=tty1 rootwait ro loglevel=4 \
                                  root=PARTUUID=0a6f3e5c-2b1d-4c8e-9f70-1a2b3c4d5e6f";
    const RESIGNED_CMDLINE: &str = "kern_guid=%U console=tty1 rootwait ro loglevel=4 \
                                    root=PARTUUID=7d2c9b4e-5f61-4a83-b0c2-d3e4f5a6b7c8";

    fn key() -> SigningKey {
        SigningKey::from_vbprivk(DATA_KEY).unwrap()
    }

    #[test]
    fn a_partition_futility_signed_parses_and_verifies() {
        let part = KernelPartition::parse(SIGNED.to_vec()).unwrap();
        assert_eq!(part.cmdline().unwrap(), SIGNED_CMDLINE);
        // The layout futility writes at its default padding: 64 KiB of header, then a
        // body whose command line sits 8 KiB below the end of the stub's page.
        assert_eq!(part.body.start, 0x10000);
        assert_eq!(part.config.start - part.body.start, 0x2000);
    }

    /// Signatures are deterministic, so signing the same contents with the same key must
    /// reproduce futility's bytes exactly. That pins the digest, the padding, and where
    /// each signature lands.
    #[test]
    fn re_signing_an_unchanged_command_line_reproduces_futilitys_bytes() {
        let mut part = KernelPartition::parse(SIGNED.to_vec()).unwrap();
        part.resign(SIGNED_CMDLINE, &key()).unwrap();
        assert!(part.into_bytes() == SIGNED);
    }

    #[test]
    fn re_signing_a_new_command_line_matches_futilitys_repack() {
        let mut part = KernelPartition::parse(SIGNED.to_vec()).unwrap();
        part.resign(RESIGNED_CMDLINE, &key()).unwrap();
        assert_eq!(part.cmdline().unwrap(), RESIGNED_CMDLINE);
        let bytes = part.into_bytes();
        assert!(
            bytes == RESIGNED,
            "the re-signed partition differs from futility's"
        );
        // And the result reads back as a partition in its own right.
        KernelPartition::parse(bytes).unwrap();
    }

    #[test]
    fn a_partition_changed_after_signing_does_not_parse() {
        // One byte of the kernel blob, which the body signature covers.
        let mut body = SIGNED.to_vec();
        body[0x10000 + 100] ^= 1;
        assert!(matches!(
            KernelPartition::parse(body),
            Err(EngineError::KpartResign { detail }) if detail.contains("body signature")
        ));
        // One byte of the preamble header, which the preamble signature covers.
        let mut header = SIGNED.to_vec();
        header[0x4b8 + 40] ^= 1;
        assert!(matches!(
            KernelPartition::parse(header),
            Err(EngineError::KpartResign { detail }) if detail.contains("preamble signature")
        ));
    }

    #[test]
    fn a_key_that_did_not_sign_the_kernel_is_refused_and_changes_nothing() {
        let mut part = KernelPartition::parse(SIGNED.to_vec()).unwrap();
        let other = SigningKey::from_vbprivk(OTHER_KEY).unwrap();
        assert!(matches!(
            part.resign(RESIGNED_CMDLINE, &other),
            Err(EngineError::KpartResign { .. })
        ));
        assert!(part.into_bytes() == SIGNED);
    }

    #[test]
    fn a_command_line_that_does_not_fit_or_holds_a_nul_is_refused() {
        let mut part = KernelPartition::parse(SIGNED.to_vec()).unwrap();
        assert!(part.resign(&"x".repeat(CONFIG_BYTES), &key()).is_err());
        assert!(part.resign("root=/dev/sda\0x", &key()).is_err());
        // The longest line that fits leaves room for its terminator.
        part.resign(&"x".repeat(CONFIG_BYTES - 1), &key()).unwrap();
        assert_eq!(part.cmdline().unwrap().len(), CONFIG_BYTES - 1);
    }

    #[test]
    fn input_that_is_not_a_whole_signed_partition_is_refused() {
        assert!(KernelPartition::parse(b"\x1f\x8bnot a keyblock".to_vec()).is_err());
        assert!(KernelPartition::parse(SIGNED[..100].to_vec()).is_err());
        assert!(KernelPartition::parse(SIGNED[..0x10000].to_vec()).is_err());
        assert!(SigningKey::from_vbprivk(&DATA_KEY[..4]).is_err());
        assert!(SigningKey::from_vbprivk(&DATA_KEY[..200]).is_err());
    }
}
