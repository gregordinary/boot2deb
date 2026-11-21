//! A strict, bounded reader for the GUID partition tables the image node writes.
//!
//! The writer is the `gpt` crate ([`super::gpt`]). Reading back with it is not enough for
//! a check:
//!
//! - It does not verify the partition-entry array's CRC.
//! - It asserts (panics) on an entry size other than 128.
//! - It accepts either header when the other is damaged.
//!
//! This reader holds a table to everything the specification lets a reader check, and
//! turns every failure into a sentence:
//!
//! - Each header has the signature, a header size between 92 and 512, and a matching
//!   header CRC32. Its entries are 128 bytes, their count is bounded, and it names the LBA
//!   it was read from as its own.
//! - The entry array lies inside the image and matches the header's array CRC32.
//! - Each used entry starts no later than it ends, and lies inside the usable range.
//! - The backup header sits on the image's last LBA, points back at the primary, and
//!   describes the same disk and the same entries.
//!
//! Every offset is computed with checked arithmetic and held to the image's length before
//! anything is read at it. The largest read is one entry array, of at most
//! [`MAX_ENTRIES`] entries. A header that lies about its array cannot make the reader
//! allocate more than that.

use crate::press::verify::TableEntry;
use std::io::{Read, Seek, SeekFrom};
use uuid::Uuid;

/// The logical sector size every image this crate writes uses.
pub(crate) const SECTOR: u64 = 512;

/// The header signature.
const SIGNATURE: &[u8; 8] = b"EFI PART";

/// The only entry size the specification's readers must accept, and the one the writer
/// uses.
const ENTRY_SIZE: u64 = 128;

/// The most entries a table may declare. The writer declares 128; the bound only keeps a
/// corrupt header from naming an array larger than any real one.
pub(crate) const MAX_ENTRIES: u32 = 1024;

/// One validated GPT header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Header {
    /// The LBA the header names as its own, which is the LBA it was read from.
    pub(crate) current_lba: u64,
    /// The LBA of the other copy.
    pub(crate) other_lba: u64,
    /// The first LBA a partition may use.
    pub(crate) first_usable: u64,
    /// The last LBA a partition may use.
    pub(crate) last_usable: u64,
    /// The disk GUID.
    pub(crate) disk_guid: Uuid,
    /// The LBA the entry array starts at.
    pub(crate) entries_lba: u64,
    /// How many entries the array holds, used or not.
    pub(crate) entry_count: u32,
    /// The CRC32 of the entry array.
    pub(crate) entries_crc: u32,
}

/// A table read off an image: the primary header, its used entries, and what the
/// backup copy came to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Table {
    /// The primary header.
    pub(crate) header: Header,
    /// The used entries, in index order.
    pub(crate) entries: Vec<TableEntry>,
}

/// Read and validate the primary table of a device `len` bytes long.
///
/// # Errors
///
/// A sentence naming what does not hold.
pub(crate) fn read_primary<R: Read + Seek>(dev: &mut R, len: u64) -> Result<Table, String> {
    let header = read_header(dev, 1, len).map_err(|e| format!("the primary GPT header {e}"))?;
    let entries = read_entries(dev, &header, len).map_err(|e| format!("the primary GPT {e}"))?;
    Ok(Table { header, entries })
}

/// Validate the backup copy of `primary` on a device `len` bytes long: on the last LBA,
/// pointing back at the primary, and describing the same disk and the same entries.
///
/// # Errors
///
/// A sentence naming what does not hold.
pub(crate) fn check_backup<R: Read + Seek>(
    dev: &mut R,
    len: u64,
    primary: &Table,
) -> Result<(), String> {
    let last = (len / SECTOR)
        .checked_sub(1)
        .ok_or("the image is shorter than one sector")?;
    if primary.header.other_lba != last {
        return Err(format!(
            "the primary GPT header places its backup at LBA {}, but the image's last LBA \
             is {last}",
            primary.header.other_lba
        ));
    }
    let backup = read_header(dev, last, len).map_err(|e| format!("the backup GPT header {e}"))?;
    if backup.other_lba != 1 {
        return Err(format!(
            "the backup GPT header points at a primary on LBA {}, not 1",
            backup.other_lba
        ));
    }
    let same = backup.disk_guid == primary.header.disk_guid
        && backup.first_usable == primary.header.first_usable
        && backup.last_usable == primary.header.last_usable
        && backup.entry_count == primary.header.entry_count
        && backup.entries_crc == primary.header.entries_crc;
    if !same {
        return Err(
            "the backup GPT header describes a different disk or entry array from the \
             primary"
                .into(),
        );
    }
    let array_sectors = array_bytes(&backup)?.div_ceil(SECTOR);
    if backup.entries_lba.checked_add(array_sectors) != Some(last) {
        return Err(format!(
            "the backup GPT entry array starts at LBA {}, not directly ahead of the backup \
             header",
            backup.entries_lba
        ));
    }
    let entries = read_entries(dev, &backup, len).map_err(|e| format!("the backup GPT {e}"))?;
    if entries != primary.entries {
        return Err("the backup GPT entries differ from the primary's".into());
    }
    Ok(())
}

/// Read and validate the header at `lba`.
fn read_header<R: Read + Seek>(dev: &mut R, lba: u64, len: u64) -> Result<Header, String> {
    let sector = read_at(dev, lba, SECTOR, len)?;
    if &sector[..8] != SIGNATURE {
        return Err(format!("at LBA {lba} has no `EFI PART` signature"));
    }
    let header_size = u64::from(le32(&sector, 12));
    if !(92..=SECTOR).contains(&header_size) {
        return Err(format!(
            "at LBA {lba} declares a size of {header_size} bytes"
        ));
    }
    let mut crc_input = sector[..header_size as usize].to_vec();
    crc_input[16..20].fill(0);
    if crc32fast::hash(&crc_input) != le32(&sector, 16) {
        return Err(format!("at LBA {lba} fails its CRC32"));
    }
    let header = Header {
        current_lba: le64(&sector, 24),
        other_lba: le64(&sector, 32),
        first_usable: le64(&sector, 40),
        last_usable: le64(&sector, 48),
        disk_guid: guid(&sector[56..72]),
        entries_lba: le64(&sector, 72),
        entry_count: le32(&sector, 80),
        entries_crc: le32(&sector, 88),
    };
    let entry_size = u64::from(le32(&sector, 84));
    if header.current_lba != lba {
        return Err(format!(
            "at LBA {lba} names LBA {} as its own",
            header.current_lba
        ));
    }
    if entry_size != ENTRY_SIZE {
        return Err(format!(
            "at LBA {lba} declares {entry_size}-byte entries, not {ENTRY_SIZE}"
        ));
    }
    if header.entry_count == 0 || header.entry_count > MAX_ENTRIES {
        return Err(format!(
            "at LBA {lba} declares {} entries, outside 1..={MAX_ENTRIES}",
            header.entry_count
        ));
    }
    if header.first_usable > header.last_usable {
        return Err(format!(
            "at LBA {lba} has a usable range that ends before it starts ({}..{})",
            header.first_usable, header.last_usable
        ));
    }
    Ok(header)
}

/// The entry array's length in bytes.
fn array_bytes(header: &Header) -> Result<u64, String> {
    u64::from(header.entry_count)
        .checked_mul(ENTRY_SIZE)
        .ok_or_else(|| "entry array length overflows".to_string())
}

/// Read and validate the entry array `header` describes, returning its used entries.
fn read_entries<R: Read + Seek>(
    dev: &mut R,
    header: &Header,
    len: u64,
) -> Result<Vec<TableEntry>, String> {
    let bytes = read_at(dev, header.entries_lba, array_bytes(header)?, len)
        .map_err(|e| format!("entry array {e}"))?;
    if crc32fast::hash(&bytes) != header.entries_crc {
        return Err("entry array fails its CRC32".into());
    }
    let mut entries = Vec::new();
    for (i, raw) in bytes.chunks_exact(ENTRY_SIZE as usize).enumerate() {
        if raw.iter().all(|&b| b == 0) {
            continue;
        }
        let index = u32::try_from(i + 1).expect("bounded by MAX_ENTRIES");
        let entry = TableEntry {
            index,
            type_guid: guid(&raw[0..16]).hyphenated().to_string(),
            part_guid: guid(&raw[16..32]).hyphenated().to_string(),
            first_lba: le64(raw, 32),
            last_lba: le64(raw, 40),
            flags: le64(raw, 48),
            name: name(&raw[56..128]),
        };
        if entry.first_lba > entry.last_lba {
            return Err(format!(
                "entry {index} ends (LBA {}) before it starts (LBA {})",
                entry.last_lba, entry.first_lba
            ));
        }
        if entry.first_lba < header.first_usable || entry.last_lba > header.last_usable {
            return Err(format!(
                "entry {index} (LBA {}..{}) lies outside the usable range {}..{}",
                entry.first_lba, entry.last_lba, header.first_usable, header.last_usable
            ));
        }
        entries.push(entry);
    }
    Ok(entries)
}

/// Read `bytes` bytes at `lba`, holding the whole extent to the device's `len`.
fn read_at<R: Read + Seek>(dev: &mut R, lba: u64, bytes: u64, len: u64) -> Result<Vec<u8>, String> {
    let start = lba
        .checked_mul(SECTOR)
        .ok_or_else(|| format!("at LBA {lba} is beyond any addressable offset"))?;
    let end = start
        .checked_add(bytes)
        .filter(|&end| end <= len)
        .ok_or_else(|| format!("at LBA {lba} runs past the image's end ({len} bytes)"))?;
    dev.seek(SeekFrom::Start(start))
        .map_err(|e| format!("at LBA {lba} cannot be reached: {e}"))?;
    let mut buf = vec![0u8; (end - start) as usize];
    dev.read_exact(&mut buf)
        .map_err(|e| format!("at LBA {lba} cannot be read: {e}"))?;
    Ok(buf)
}

/// A little-endian `u32` at `off`.
fn le32(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes(b[off..off + 4].try_into().expect("4 bytes"))
}

/// A little-endian `u64` at `off`.
fn le64(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(b[off..off + 8].try_into().expect("8 bytes"))
}

/// A GUID in the table's mixed-endian encoding.
fn guid(b: &[u8]) -> Uuid {
    Uuid::from_bytes_le(b.try_into().expect("16 bytes"))
}

/// An entry name: UTF-16LE, NUL-terminated within its 72 bytes.
fn name(b: &[u8]) -> String {
    let units: Vec<u16> = b
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .take_while(|&u| u != 0)
        .collect();
    String::from_utf16_lossy(&units)
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpt::disk::LogicalBlockSize;
    use gpt::GptConfig;
    use std::io::Cursor;

    /// A 4 MiB disk with two partitions, written by the same `gpt` crate the image node
    /// writes with.
    fn disk() -> Vec<u8> {
        let len = 4 << 20;
        let mut dev = Cursor::new(vec![0u8; len]);
        let mut gdisk = GptConfig::new()
            .writable(true)
            .logical_block_size(LogicalBlockSize::Lb512)
            .create_from_device(&mut dev, Some(Uuid::from_bytes([0xa1; 16])))
            .unwrap();
        gdisk
            .add_partition("b2d-seed", 1 << 20, gpt::partition_types::BASIC, 0, None)
            .unwrap();
        gdisk
            .add_partition(
                "rootfs",
                2 << 20,
                gpt::partition_types::LINUX_FS,
                1 << 2,
                None,
            )
            .unwrap();
        gdisk.write_inplace().unwrap();
        dev.into_inner()
    }

    #[test]
    fn a_written_table_reads_back_whole_with_its_backup() {
        let bytes = disk();
        let len = bytes.len() as u64;
        let mut dev = Cursor::new(bytes);
        let table = read_primary(&mut dev, len).unwrap();
        assert_eq!(table.entries.len(), 2);
        assert_eq!(table.entries[1].name, "rootfs");
        assert_eq!(table.entries[1].flags, 1 << 2);
        assert_eq!(
            table.entries[1].type_guid,
            "0fc63daf-8483-4772-8e79-3d69d8477de4"
        );
        check_backup(&mut dev, len, &table).unwrap();
    }

    /// A flipped byte in the entry array is a CRC failure, which the `gpt` crate does not
    /// check.
    #[test]
    fn a_damaged_entry_array_fails_its_crc() {
        let mut bytes = disk();
        bytes[2 * 512 + 32] ^= 0xff; // entry 1's first LBA
        let len = bytes.len() as u64;
        let err = read_primary(&mut Cursor::new(bytes), len).unwrap_err();
        assert!(err.contains("CRC32"), "{err}");
    }

    /// A header whose array lies past the image's end is refused before anything is read
    /// there, however its CRC was made to agree.
    #[test]
    fn a_header_naming_an_array_past_the_end_is_refused_without_reading_it() {
        let mut bytes = disk();
        let mut header = bytes[512..1024].to_vec();
        header[72..80].copy_from_slice(&u64::MAX.to_le_bytes()); // entries LBA
        header[16..20].fill(0);
        let crc = crc32fast::hash(&header[..92]);
        header[16..20].copy_from_slice(&crc.to_le_bytes());
        bytes[512..1024].copy_from_slice(&header);
        let len = bytes.len() as u64;
        let err = read_primary(&mut Cursor::new(bytes), len).unwrap_err();
        assert!(
            err.contains("beyond any addressable offset") || err.contains("past"),
            "{err}"
        );
    }

    /// An entry size other than 128 is an error, not a panic.
    #[test]
    fn an_odd_entry_size_is_an_error() {
        let mut bytes = disk();
        let mut header = bytes[512..1024].to_vec();
        header[84..88].copy_from_slice(&256u32.to_le_bytes());
        header[16..20].fill(0);
        let crc = crc32fast::hash(&header[..92]);
        header[16..20].copy_from_slice(&crc.to_le_bytes());
        bytes[512..1024].copy_from_slice(&header);
        let len = bytes.len() as u64;
        let err = read_primary(&mut Cursor::new(bytes), len).unwrap_err();
        assert!(err.contains("256-byte entries"), "{err}");
    }

    /// An image cut short loses its backup, and an image grown past its table no longer
    /// ends at it; the primary still reads in both.
    #[test]
    fn the_backup_must_sit_on_the_images_last_lba() {
        let bytes = disk();
        let cut = bytes[..bytes.len() - 512].to_vec();
        let len = cut.len() as u64;
        let mut dev = Cursor::new(cut);
        let table = read_primary(&mut dev, len).unwrap();
        assert!(check_backup(&mut dev, len, &table)
            .unwrap_err()
            .contains("last LBA"));

        let mut grown = bytes.clone();
        grown.extend_from_slice(&[0u8; 4096]);
        let len = grown.len() as u64;
        let mut dev = Cursor::new(grown);
        let table = read_primary(&mut dev, len).unwrap();
        assert!(check_backup(&mut dev, len, &table).is_err());
    }

    /// A damaged backup array is caught even with the primary intact.
    #[test]
    fn a_damaged_backup_array_is_caught() {
        let mut bytes = disk();
        let len = bytes.len();
        // The backup array is the 32 sectors ahead of the last one.
        bytes[len - 33 * 512 + 40] ^= 0x01;
        let len = len as u64;
        let mut dev = Cursor::new(bytes);
        let table = read_primary(&mut dev, len).unwrap();
        assert!(check_backup(&mut dev, len, &table).is_err());
    }
}
