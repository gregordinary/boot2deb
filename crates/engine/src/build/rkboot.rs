//! Pure-Rust writer for the Rockchip "RKBOOT" maskrom loader container.
//!
//! `rkdeveloptool db <file>` (and other rockusb hosts) stream a single
//! RKBOOT-format file to the BootROM to run a u-boot from RAM. The engine's
//! u-boot stage already emits the two raw download payloads
//! ([`MaskromImages`](super::uboot::MaskromImages)) — `usb471` (the DDR TPL,
//! CODE471) and `usb472` (SPL + FIT, CODE472). This module packs them into the
//! container `db` consumes, so the build hands out a directly-flashable loader
//! instead of leaving a `boot_merger` step to run by hand.
//!
//! The layout is reproduced from a hardware-proven RK3576 loader, not guessed:
//! the 102-byte header and 57-byte entry descriptors match rkdeveloptool's
//! `rk_boot_header`/`rk_boot_entry`, and the CODE471 group carries the generated
//! `UsbHead` RKNS block that the closed rkbin `boot_merger` writes
//! under `CREATE_IDB`/`NEWIDB`. `rkdeveloptool db` downloads only the CODE471 and
//! CODE472 entries, so the container omits the LOADER section (the storage-write
//! blobs, which `db` never touches) — that section is also the only part carrying
//! an encrypted/signed header, which is why omitting it keeps the writer pure and
//! deterministic.
//!
//! Blobs are stored plaintext (RC4 off; `rc4Flag = 1` signals "not encrypted"),
//! padded up to the 4096-byte RKNS page, and the file ends with the Rockchip
//! CRC32 ([`rk_crc32`]).

use sha2::{Digest, Sha256};

/// The RKBOOT file header length in bytes (`rk_boot_header`, byte-packed).
const HEADER_LEN: usize = 102;
/// One entry descriptor's length (`rk_boot_entry`): the `type` field is a C
/// `int`, so the packed struct is 57 bytes, not 54.
const ENTRY_LEN: usize = 57;
/// The RKNS page size blobs are padded up to, and the `UsbHead` block size.
const PAGE: usize = 4096;
/// Sector size the RKNS `UsbHead` counts blobs in.
const SECTOR: usize = 512;

/// Entry `type` for a CODE471 download stage (DDR TPL group).
const TYPE_471: u32 = 1;
/// Entry `type` for a CODE472 download stage (SPL + FIT).
const TYPE_472: u32 = 2;

/// The container `version`/`mergerVersion` header words, copied verbatim from the
/// reference loader (boot_merger tool version constants; `db` ignores them).
const VERSION: u32 = 0x0000_0164;
/// See [`VERSION`].
const MERGER_VERSION: u32 = 0x0100_0000;

/// Rockchip CRC32: polynomial `0x04C10DB7` (one bit off the standard CRC32),
/// MSB-first, init 0, no input/output reflection, no final xor. This is the
/// trailing checksum `boot_merger` appends and is not the zlib/ISO CRC32.
pub fn rk_crc32(buf: &[u8]) -> u32 {
    let mut crc: u32 = 0;
    for &b in buf {
        crc ^= (b as u32) << 24;
        for _ in 0..8 {
            crc = if crc & 0x8000_0000 != 0 {
                (crc << 1) ^ 0x04C1_0DB7
            } else {
                crc << 1
            };
        }
    }
    crc
}

/// Round `len` up to the next [`PAGE`] boundary.
fn padded_len(len: usize) -> usize {
    len.div_ceil(PAGE) * PAGE
}

/// The `UsbHead` RKNS block: the sector-0 IDB header for the CODE471 download
/// group. It describes the two contiguous sections that follow it (the padded
/// `usb471` then `usb472`) by start-sector and sector-count, carries a SHA-256 of
/// each padded section, and a SHA-256 of its own first 1536 bytes. Every field is
/// deterministic in the two payloads; the byte offsets are those the reference
/// RK3576 loader uses.
fn usb_head(p471: &[u8], p472: &[u8]) -> [u8; PAGE] {
    debug_assert_eq!(p471.len() % PAGE, 0);
    debug_assert_eq!(p472.len() % PAGE, 0);
    let mut h = [0u8; PAGE];
    h[0..4].copy_from_slice(b"RKNS");
    h[8..12].copy_from_slice(&0x0002_0180u32.to_le_bytes());
    h[12] = 1;
    h[97] = 1;

    // Section layout, in RKNS sectors relative to this block's start. The two
    // payloads follow `UsbHead` back to back, so start_472 = start_471 + count_471.
    let start_471 = (PAGE / SECTOR) as u16; // 8
    let count_471 = (p471.len() / SECTOR) as u16;
    let start_472 = start_471 + count_471;
    let count_472 = (p472.len() / SECTOR) as u16;

    // Section descriptor for the CODE471 payload.
    h[120..122].copy_from_slice(&start_471.to_le_bytes());
    h[122..124].copy_from_slice(&count_471.to_le_bytes());
    h[124..128].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    h[132] = 1;
    h[144..176].copy_from_slice(&Sha256::digest(p471));

    // Section descriptor for the CODE472 payload.
    h[208..210].copy_from_slice(&start_472.to_le_bytes());
    h[210..212].copy_from_slice(&count_472.to_le_bytes());
    h[212..216].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    h[220] = 2;
    h[232..264].copy_from_slice(&Sha256::digest(p472));

    // The header self-hash covers everything before it.
    let self_hash = Sha256::digest(&h[..1536]);
    h[1536..1568].copy_from_slice(&self_hash);
    h
}

/// Pack a UTF-16LE entry name into the 40-byte (`[u16; 20]`) name field,
/// truncated to 20 code units and null-padded, as `boot_merger` does.
fn name_field(name: &str) -> [u8; 40] {
    let mut out = [0u8; 40];
    for (i, u) in name.encode_utf16().take(20).enumerate() {
        out[i * 2..i * 2 + 2].copy_from_slice(&u.to_le_bytes());
    }
    out
}

/// Write one 57-byte entry descriptor into `buf` at `at`.
fn write_entry(
    buf: &mut [u8],
    at: usize,
    ty: u32,
    name: &str,
    data_off: u32,
    data_size: u32,
    delay: u32,
) {
    let e = &mut buf[at..at + ENTRY_LEN];
    e[0] = ENTRY_LEN as u8;
    e[1..5].copy_from_slice(&ty.to_le_bytes());
    e[5..45].copy_from_slice(&name_field(name));
    e[45..49].copy_from_slice(&data_off.to_le_bytes());
    e[49..53].copy_from_slice(&data_size.to_le_bytes());
    e[53..57].copy_from_slice(&delay.to_le_bytes());
}

/// Build the `db`-loadable RKBOOT container from the CODE471/CODE472 payloads.
///
/// `chip` is the four-character SoC code (e.g. `b"3576"`), packed big-endian into
/// the header `chipType` word the way `boot_merger`'s `convertChipType` does. The
/// `releaseTime` is fixed (not the wall clock) so the output is reproducible.
pub fn write_maskrom_loader(chip: [u8; 4], usb471: &[u8], usb472: &[u8]) -> Vec<u8> {
    let mut p471 = usb471.to_vec();
    p471.resize(padded_len(p471.len()), 0);
    let mut p472 = usb472.to_vec();
    p472.resize(padded_len(p472.len()), 0);

    // Entry tables: 2 CODE471 entries (UsbHead + payload) then 1 CODE472 entry.
    // No LOADER section (`db` never reads it).
    let off_471 = HEADER_LEN as u32;
    let off_472 = off_471 + 2 * ENTRY_LEN as u32;
    let data_start = off_472 as usize + ENTRY_LEN;

    let head_off = data_start;
    let b471_off = head_off + PAGE;
    let b472_off = b471_off + p471.len();
    let body_len = b472_off + p472.len();

    let mut buf = vec![0u8; body_len];

    // Header.
    let chip_type = u32::from_be_bytes(chip);
    buf[0..4].copy_from_slice(b"LDR ");
    buf[4..6].copy_from_slice(&(HEADER_LEN as u16).to_le_bytes());
    buf[6..10].copy_from_slice(&VERSION.to_le_bytes());
    buf[10..14].copy_from_slice(&MERGER_VERSION.to_le_bytes());
    // releaseTime (year u16, month/day/hour/min/sec u8) — fixed for reproducibility.
    buf[14..16].copy_from_slice(&2026u16.to_le_bytes());
    buf[16] = 1; // month
    buf[17] = 1; // day
    buf[21..25].copy_from_slice(&chip_type.to_le_bytes());
    buf[25] = 2; // code471Num (UsbHead + payload)
    buf[26..30].copy_from_slice(&off_471.to_le_bytes());
    buf[30] = ENTRY_LEN as u8;
    buf[31] = 1; // code472Num
    buf[32..36].copy_from_slice(&off_472.to_le_bytes());
    buf[36] = ENTRY_LEN as u8;
    buf[37] = 0; // loaderNum — none
    buf[38..42].copy_from_slice(&0u32.to_le_bytes());
    buf[42] = ENTRY_LEN as u8;
    buf[43] = 0; // signFlag — unsigned
    buf[44] = 1; // rc4Flag — 1 means RC4 disabled (blobs plaintext)

    // Entry descriptors.
    write_entry(
        &mut buf,
        off_471 as usize,
        TYPE_471,
        "UsbHead",
        head_off as u32,
        PAGE as u32,
        1,
    );
    write_entry(
        &mut buf,
        off_471 as usize + ENTRY_LEN,
        TYPE_471,
        "usb471",
        b471_off as u32,
        p471.len() as u32,
        1,
    );
    write_entry(
        &mut buf,
        off_472 as usize,
        TYPE_472,
        "usb472",
        b472_off as u32,
        p472.len() as u32,
        0,
    );

    // Data region: UsbHead, then the two padded payloads, contiguous.
    buf[head_off..head_off + PAGE].copy_from_slice(&usb_head(&p471, &p472));
    buf[b471_off..b471_off + p471.len()].copy_from_slice(&p471);
    buf[b472_off..b472_off + p472.len()].copy_from_slice(&p472);

    // Trailing Rockchip CRC32 over the whole body, little-endian.
    let crc = rk_crc32(&buf);
    buf.extend_from_slice(&crc.to_le_bytes());
    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rk_crc32_known_answer() {
        // Rockchip CRC32 (poly 0x04C10DB7), verified against the reference loader.
        assert_eq!(rk_crc32(b"123456789"), 0x889a_9615);
        assert_eq!(rk_crc32(b""), 0);
        assert_eq!(rk_crc32(b"boot2deb"), 0x107b_db31);
    }

    #[test]
    fn header_and_entries_well_formed() {
        let b = write_maskrom_loader(*b"3576", &vec![0xAAu8; 78448], &vec![0xBBu8; 3128832]);
        // tag "LDR ", header size, chipType.
        assert_eq!(&b[0..4], b"LDR ");
        assert_eq!(u16::from_le_bytes([b[4], b[5]]), HEADER_LEN as u16);
        assert_eq!(
            u32::from_le_bytes([b[21], b[22], b[23], b[24]]),
            0x3335_3736
        );
        // Entry counts / offsets.
        assert_eq!(b[25], 2); // 471 entries
        assert_eq!(b[31], 1); // 472 entries
        assert_eq!(b[37], 0); // no loader entries
        assert_eq!(b[44], 1); // rc4Flag = disabled

        // 472 entry offset = header + 2 * entry.
        assert_eq!(
            u32::from_le_bytes([b[32], b[33], b[34], b[35]]),
            (HEADER_LEN + 2 * ENTRY_LEN) as u32
        );
        // Blobs padded up to the page.
        let usb472_entry = HEADER_LEN + 2 * ENTRY_LEN;
        let d472_size = u32::from_le_bytes([
            b[usb472_entry + 49],
            b[usb472_entry + 50],
            b[usb472_entry + 51],
            b[usb472_entry + 52],
        ]) as usize;
        assert_eq!(d472_size, padded_len(3128832));
        // Trailing CRC is self-consistent.
        let (body, tail) = b.split_at(b.len() - 4);
        assert_eq!(u32::from_le_bytes(tail.try_into().unwrap()), rk_crc32(body));
    }

    #[test]
    fn usb_head_hashes_the_padded_payloads() {
        let p471 = vec![0x11u8; PAGE];
        let p472 = vec![0x22u8; PAGE * 3];
        let h = usb_head(&p471, &p472);
        assert_eq!(&h[0..4], b"RKNS");
        assert_eq!(&h[144..176], Sha256::digest(&p471).as_slice());
        assert_eq!(&h[232..264], Sha256::digest(&p472).as_slice());
        assert_eq!(&h[1536..1568], Sha256::digest(&h[..1536]).as_slice());
        // Section counts in 512-byte sectors, contiguous after the 4096 header.
        assert_eq!(u16::from_le_bytes([h[120], h[121]]), 8);
        assert_eq!(u16::from_le_bytes([h[122], h[123]]), (PAGE / SECTOR) as u16);
        assert_eq!(
            u16::from_le_bytes([h[208], h[209]]),
            8 + (PAGE / SECTOR) as u16
        );
    }
}
