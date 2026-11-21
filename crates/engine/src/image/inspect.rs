//! Read back what a *finished* image artifact says about itself. That is both copies of
//! its partition table, its rootfs superblock, its EFI system partition, and the bytes of
//! its boot region.
//!
//! The counterpart of this module's `ext4`, `esp` and `gpt` siblings, which write those
//! structures. This module exists so the acceptance gate asks the questions in the same
//! language the image was written in. The alternative is a second implementation of every
//! parser, in a second language, that nothing tests.
//!
//! **Every read is bounded by the image.** An artifact is opened as a [`RawImage`]. A raw
//! `.img` is read in place, and a compressed one is decompressed first into a sparse
//! temporary file. Every structure is then reached by seeking, not by decompressing
//! everything ahead of it.
//!
//! The table is read by this crate's strict GPT reader (the `gptread` module). Every
//! partition extent is held to the image's length with checked arithmetic before anything
//! is read inside it. Each read is of a fixed size (a superblock), or of an extent the
//! table and the image both bound. A corrupt or truncated
//! artifact is an error that says what is wrong with it. It is never an allocation the
//! size of a field's claim.
//!
//! Partitions are found by their partition GUID, which the image's identity fixes, and
//! their type is a separate fact checked against it. Finding them by name would tie the
//! reader to a label the recipe can choose (`--rootfs-label`). It would also accept an
//! entry of the wrong type under the right name.
//!
//! Pure except for reading the artifact and writing the sparse temporary file: no mount,
//! no loop device, no root.

use super::gptread::{self, SECTOR};
use crate::error::EngineError;
use crate::press::verify::TableEntry;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// The GPT type GUID of a Linux filesystem partition, which the image node gives the
/// rootfs.
pub const LINUX_FS_TYPE: Uuid = Uuid::from_u128(0x0fc63daf_8483_4772_8e79_3d69d8477de4);

/// The GPT type GUID of an EFI system partition, which firmware selects the partition by.
pub const EFI_SYSTEM_TYPE: Uuid = Uuid::from_u128(0xc12a7328_f81f_11d2_ba4b_00a0c93ec93b);

/// Offset of the ext4 superblock within its partition. Fixed by the format: the first
/// 1024 bytes are reserved for a boot block.
const SUPERBLOCK_OFFSET: u64 = 1024;

/// Bytes of the superblock this reads: the whole structure, which reaches every field
/// below.
const SUPERBLOCK_BYTES: usize = 1024;

/// `s_blocks_count_lo`, at offset 0x04.
const BLOCKS_LO_OFFSET: usize = 0x04;

/// `s_free_blocks_count_lo`, at offset 0x0C.
const FREE_BLOCKS_LO_OFFSET: usize = 0x0C;

/// `s_log_block_size`, at offset 0x18: the block size is `1024 << s_log_block_size`.
const LOG_BLOCK_SIZE_OFFSET: usize = 0x18;

/// `s_magic`, at offset 0x38.
const MAGIC_OFFSET: usize = 0x38;

/// `s_feature_incompat`, at offset 0x60.
const INCOMPAT_OFFSET: usize = 0x60;

/// `INCOMPAT_64BIT`: the block counts have high halves.
const INCOMPAT_64BIT: u32 = 0x80;

/// `s_blocks_count_hi`, at offset 0x150.
const BLOCKS_HI_OFFSET: usize = 0x150;

/// `s_free_blocks_count_hi`, at offset 0x158.
const FREE_BLOCKS_HI_OFFSET: usize = 0x158;

/// The value `s_magic` must hold for the bytes to be an ext2/3/4 superblock.
const EXT_MAGIC: u16 = 0xEF53;

/// The largest `s_log_block_size` ext4 defines (64 KiB blocks).
const MAX_LOG_BLOCK_SIZE: u32 = 6;

/// GPT attribute bit 2, "legacy BIOS bootable", read back here as the writer's
/// [`gpt`](super::gpt) side sets it.
const LEGACY_BIOS_BOOTABLE: u64 = 1 << 2;

/// A finished image, readable by offset.
///
/// A raw `.img` is read where it is. A `.img.xz` or `.img.gz` cannot seek, so it is
/// decompressed into a sparse temporary file in the scratch directory the caller names.
/// The file is removed when this is dropped, and stays proportional to the image's data
/// rather than its length.
pub struct RawImage {
    /// The open image bytes: the artifact itself, or the decompressed temporary file.
    file: File,
    /// The image's length in bytes, which every read is held to.
    len: u64,
    /// The artifact this was opened from, for messages.
    artifact: PathBuf,
    /// The decompressed temporary file, kept alive for as long as `file` is read.
    _temp: Option<tempfile::NamedTempFile>,
}

impl RawImage {
    /// Open `artifact` for reading by offset, decompressing it into `scratch` first when
    /// it is a container.
    ///
    /// # Errors
    ///
    /// [`EngineError`] when the artifact cannot be read or decompressed, or is not an
    /// image artifact by name.
    pub fn open(artifact: &Path, scratch: &Path) -> Result<Self, EngineError> {
        use crate::press::write::Container;
        let (file, temp) = if Container::of(artifact)? == Container::Raw {
            (
                File::open(artifact).map_err(|s| EngineError::io(artifact, s))?,
                None,
            )
        } else {
            let mut temp = tempfile::Builder::new()
                .prefix(".boot2deb-verify-")
                .tempfile_in(scratch)
                .map_err(|s| EngineError::io(scratch, s))?;
            decompress_sparse(artifact, temp.as_file_mut())?;
            let file = temp.reopen().map_err(|s| EngineError::io(temp.path(), s))?;
            (file, Some(temp))
        };
        let len = file
            .metadata()
            .map_err(|s| EngineError::io(artifact, s))?
            .len();
        Ok(Self {
            file,
            len,
            artifact: artifact.to_path_buf(),
            _temp: temp,
        })
    }

    /// The image's length in bytes: the decompressed disk, for a container.
    #[must_use]
    pub fn len(&self) -> u64 {
        self.len
    }

    /// Whether the image is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// The artifact this image was opened from.
    #[must_use]
    pub fn artifact(&self) -> &Path {
        &self.artifact
    }

    /// A second handle on the image bytes, for a reader that takes ownership of one.
    ///
    /// # Errors
    ///
    /// [`EngineError::Io`] when the handle cannot be duplicated.
    pub fn handle(&self) -> Result<File, EngineError> {
        self.file
            .try_clone()
            .map_err(|s| EngineError::io(&self.artifact, s))
    }

    /// Read `len` bytes at `offset`, held to the image's end.
    fn read_at(&self, offset: u64, len: usize) -> Result<Vec<u8>, EngineError> {
        self.check_extent(offset, len as u64, "the read")?;
        let mut buf = vec![0u8; len];
        self.file
            .read_exact_at(&mut buf, offset)
            .map_err(|s| EngineError::io(&self.artifact, s))?;
        Ok(buf)
    }

    /// The sha256 of `len` bytes at `offset`, read a chunk at a time.
    ///
    /// # Errors
    ///
    /// [`EngineError::ImageFileInvalid`] when the extent runs past the image's end, and
    /// [`EngineError::Io`] when it cannot be read.
    pub fn sha256_at(&self, offset: u64, len: u64) -> Result<String, EngineError> {
        self.check_extent(offset, len, "the region")?;
        let mut hasher = Sha256::new();
        let mut buf = vec![0u8; 1 << 20];
        let mut done = 0u64;
        while done < len {
            let n = buf
                .len()
                .min(usize::try_from(len - done).unwrap_or(buf.len()));
            self.file
                .read_exact_at(&mut buf[..n], offset + done)
                .map_err(|s| EngineError::io(&self.artifact, s))?;
            hasher.update(&buf[..n]);
            done += n as u64;
        }
        Ok(crate::blobs::hex(&hasher.finalize()))
    }

    /// Fail unless `len` bytes at `offset` lie inside the image.
    fn check_extent(&self, offset: u64, len: u64, what: &str) -> Result<(), EngineError> {
        match offset.checked_add(len) {
            Some(end) if end <= self.len => Ok(()),
            _ => Err(self.invalid(format!(
                "{what} at byte {offset} ({len} bytes) runs past the image's end ({} bytes)",
                self.len
            ))),
        }
    }

    /// An [`EngineError::ImageFileInvalid`] naming this image.
    fn invalid(&self, detail: String) -> EngineError {
        EngineError::ImageFileInvalid {
            target: self.artifact.display().to_string(),
            detail,
        }
    }

    /// A `Read + Seek` view of the image bytes, for the table reader.
    fn device(&self) -> Result<File, EngineError> {
        let mut f = self.handle()?;
        f.seek(SeekFrom::Start(0))
            .map_err(|s| EngineError::io(&self.artifact, s))?;
        Ok(f)
    }
}

/// An image's partition table as the strict reader found it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiskTable {
    /// The used entries of the primary table, in index order.
    pub entries: Vec<TableEntry>,
    /// What the backup copy came to: `Ok` when it sits on the image's last LBA and
    /// agrees with the primary, otherwise why not. An image file is exactly its disk, so
    /// anything but `Ok` is a defect in the file.
    pub backup: Result<(), String>,
}

/// One partition, its extent resolved and held to the image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Partition {
    /// The entry's index (1-based).
    pub index: u32,
    /// The entry's label.
    pub name: String,
    /// Byte offset of the partition's first sector.
    pub start: u64,
    /// The partition's length in bytes.
    pub bytes: u64,
    /// The entry's type GUID.
    pub type_guid: Uuid,
    /// Whether the entry has the legacy-BIOS-bootable attribute.
    ///
    /// U-Boot's `bootflow scan` narrows to the partitions marked with it as soon as any
    /// partition on the medium is. It scans partition 1 alone when none is, and
    /// partition 1 is the seed. An unmarked boot partition is therefore a board that
    /// reaches its prompt and finds nothing to boot, which no filesystem-level check can
    /// see.
    pub bootable: bool,
}

/// Read `image`'s partition table: the primary, strictly, and the backup against it.
///
/// # Errors
///
/// [`EngineError::ImageVerifyGpt`] when the primary table does not validate.
pub fn read_table(image: &RawImage) -> Result<DiskTable, EngineError> {
    let mut dev = image.device()?;
    let primary = gptread::read_primary(&mut dev, image.len).map_err(|detail| {
        EngineError::ImageVerifyGpt {
            target: image.artifact.display().to_string(),
            detail,
        }
    })?;
    let backup = gptread::check_backup(&mut dev, image.len, &primary);
    Ok(DiskTable {
        entries: primary.entries,
        backup,
    })
}

impl DiskTable {
    /// The partition whose GUID is `guid`, its extent held to `image`.
    ///
    /// # Errors
    ///
    /// [`EngineError::ImageVerifyGpt`] when no entry has the GUID, and
    /// [`EngineError::ImageFileInvalid`] when its extent runs past the image's end.
    pub fn by_guid(&self, image: &RawImage, guid: Uuid) -> Result<Partition, EngineError> {
        let want = guid.hyphenated().to_string();
        let entry = self
            .entries
            .iter()
            .find(|e| e.part_guid == want)
            .ok_or_else(|| EngineError::ImageVerifyGpt {
                target: image.artifact.display().to_string(),
                detail: format!("no partition with GUID {want} in the primary GPT"),
            })?;
        // The table reader already held first <= last; the byte extent is what the image
        // must contain.
        let start = entry.first_lba.checked_mul(SECTOR);
        let bytes = (entry.last_lba - entry.first_lba)
            .checked_add(1)
            .and_then(|n| n.checked_mul(SECTOR));
        let (Some(start), Some(bytes)) = (start, bytes) else {
            return Err(image.invalid(format!(
                "partition {} (LBA {}..{}) is beyond any addressable offset",
                entry.index, entry.first_lba, entry.last_lba
            )));
        };
        image.check_extent(start, bytes, &format!("partition {}", entry.index))?;
        Ok(Partition {
            index: entry.index,
            name: entry.name.clone(),
            start,
            bytes,
            type_guid: Uuid::parse_str(&entry.type_guid).unwrap_or(Uuid::nil()),
            bootable: entry.flags & LEGACY_BIOS_BOOTABLE != 0,
        })
    }
}

/// What an ext4 superblock says about the filesystem's size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Superblock {
    /// The block size in bytes.
    pub block_size: u64,
    /// The filesystem's length in blocks.
    pub blocks: u64,
    /// The free-block count.
    ///
    /// This is the number a fitted `image_size` was measured against. The formatter
    /// decides how large the filesystem is from what it wrote. Nothing outside the
    /// superblock says whether the slack a recipe asked for survived.
    pub free_blocks: u64,
}

impl Superblock {
    /// The filesystem's length in bytes, `None` when it overflows (which no partition
    /// can contain).
    #[must_use]
    pub fn bytes(&self) -> Option<u64> {
        self.blocks.checked_mul(self.block_size)
    }
}

/// Read the ext4 superblock at the start of `part`.
///
/// Both halves of each 64-bit count are read when the filesystem has `64bit` set, which
/// every image this writes has. Taking the low half alone would silently truncate a
/// filesystem above 16 TiB.
///
/// # Errors
///
/// [`EngineError::ImageFileInvalid`] when the partition is too short to hold a
/// superblock, or the bytes there are not one.
pub fn superblock(image: &RawImage, part: &Partition) -> Result<Superblock, EngineError> {
    if part.bytes < SUPERBLOCK_OFFSET + SUPERBLOCK_BYTES as u64 {
        return Err(image.invalid(format!(
            "partition {} ({} bytes) is too short to hold an ext4 superblock",
            part.index, part.bytes
        )));
    }
    let sb = image.read_at(part.start + SUPERBLOCK_OFFSET, SUPERBLOCK_BYTES)?;
    let u16_at = |off: usize| u16::from_le_bytes([sb[off], sb[off + 1]]);
    let u32_at = |off: usize| u32::from_le_bytes([sb[off], sb[off + 1], sb[off + 2], sb[off + 3]]);
    let magic = u16_at(MAGIC_OFFSET);
    if magic != EXT_MAGIC {
        return Err(image.invalid(format!(
            "partition {} holds no ext4 superblock (magic {magic:#06x}, want {EXT_MAGIC:#06x})",
            part.index
        )));
    }
    let log = u32_at(LOG_BLOCK_SIZE_OFFSET);
    if log > MAX_LOG_BLOCK_SIZE {
        return Err(image.invalid(format!(
            "partition {}'s superblock declares a block size of 1024 << {log}",
            part.index
        )));
    }
    let wide = u32_at(INCOMPAT_OFFSET) & INCOMPAT_64BIT != 0;
    let count = |lo: usize, hi: usize| {
        u64::from(u32_at(lo)) | if wide { u64::from(u32_at(hi)) << 32 } else { 0 }
    };
    Ok(Superblock {
        block_size: 1024 << log,
        blocks: count(BLOCKS_LO_OFFSET, BLOCKS_HI_OFFSET),
        free_blocks: count(FREE_BLOCKS_LO_OFFSET, FREE_BLOCKS_HI_OFFSET),
    })
}

/// What an image's EFI system partition holds, read back from the artifact.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EspReport {
    /// Whether the entry's type is the EFI system partition's, which is what firmware
    /// selects the partition by.
    pub efi_type: bool,
    /// Whether the entry has the legacy-BIOS-bootable attribute. Under the `efi` loader
    /// it is the partition u-boot's scan has to open.
    pub bootable: bool,
    /// Whether the volume reads as FAT32, which the UEFI specification requires of a
    /// system partition on fixed media.
    pub fat32: bool,
    /// The reader's scan of the whole volume, rendered, or `None` for a clean one.
    pub findings: Option<String>,
    /// The paths asked for that the volume does not hold as files.
    pub missing: Vec<String>,
}

/// Read the EFI system partition `part` back, and look for each of `required` (paths from
/// the volume's root) as a file. The volume is read where it sits, bounded to the
/// partition.
///
/// # Errors
///
/// [`EngineError::ImageFileInvalid`] when its bytes do not open as a FAT volume.
pub fn esp(
    image: &RawImage,
    part: &Partition,
    required: &[String],
) -> Result<EspReport, EngineError> {
    use ferrosys::fat::{FatType, OpenOptions, Reader};
    let options = OpenOptions::new().base(part.start).len(part.bytes);
    let mut reader = Reader::open_with(image.handle()?, &options).map_err(|e| {
        image.invalid(format!(
            "the EFI system partition does not open as FAT: {e}"
        ))
    })?;
    let fat32 = reader.layout().fat_type == FatType::Fat32;
    let scan = reader.scan();
    let findings = (!scan.is_clean()).then(|| scan.to_report().to_table());
    let missing = required
        .iter()
        .filter(|path| {
            reader
                .lookup(path.as_bytes())
                .map_or(true, |node| node.is_dir())
        })
        .cloned()
        .collect();
    Ok(EspReport {
        efi_type: part.type_guid == EFI_SYSTEM_TYPE,
        bootable: part.bootable,
        fat32,
        findings,
        missing,
    })
}

/// Decompress `artifact` into `out`, seeking past every all-zero chunk rather than
/// writing it. An image is mostly unallocated filesystem, so the file stays sparse and
/// the write stays proportional to the data.
fn decompress_sparse(artifact: &Path, out: &mut File) -> Result<(), EngineError> {
    use std::io::Write;
    let mut decoder = crate::press::write::open_decoded(artifact)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut len: u64 = 0;
    loop {
        let n = decoder
            .read(&mut buf)
            .map_err(|s| EngineError::io(artifact, s))?;
        if n == 0 {
            break;
        }
        if buf[..n].iter().all(|&b| b == 0) {
            out.seek(SeekFrom::Current(n as i64))
                .map_err(|s| EngineError::io(artifact, s))?;
        } else {
            out.write_all(&buf[..n])
                .map_err(|s| EngineError::io(artifact, s))?;
        }
        len += n as u64;
    }
    // A trailing run of zeros was seeked over, not written, so the length is set.
    out.set_len(len).map_err(|s| EngineError::io(artifact, s))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reader, against a real shipped artifact. The offsets above are a claim
    /// about the on-disk format; this is the claim exercised: the filesystem fills its
    /// partition, and the free-block count is a plausible fraction of it.
    ///
    /// `#[ignore]` because it needs a built image, which a plain `cargo test` has no
    /// business producing. Run it with an artifact and the build point's root PARTUUID:
    /// `IMAGE=build/<recipe>/artifacts/<stem>.img.xz PARTUUID=<uuid> cargo test -- --ignored`.
    #[test]
    #[ignore = "needs a built image artifact; set IMAGE=<path> PARTUUID=<uuid>"]
    fn the_superblock_reader_agrees_with_a_shipped_artifact() {
        let (Some(image), Some(guid)) = (std::env::var_os("IMAGE"), std::env::var("PARTUUID").ok())
        else {
            panic!("set IMAGE=<path to a built .img/.img.xz> and PARTUUID=<root partuuid>");
        };
        let scratch = tempfile::tempdir().unwrap();
        let image = RawImage::open(Path::new(&image), scratch.path()).unwrap();
        let table = read_table(&image).unwrap();
        assert_eq!(table.backup, Ok(()));
        let part = table
            .by_guid(&image, Uuid::parse_str(&guid).unwrap())
            .unwrap();
        assert_eq!(part.type_guid, LINUX_FS_TYPE);
        let sb = superblock(&image, &part).unwrap();
        assert_eq!(sb.bytes(), Some(part.bytes));
        assert!(sb.free_blocks > 0 && sb.free_blocks < sb.blocks);
    }

    #[test]
    fn the_superblock_offsets_are_the_ones_the_format_fixes() {
        // Stated as a test because they are the whole of this module's contract with
        // the on-disk format: a wrong offset reads a different field and reports a
        // plausible number. These are `ext2_fs.h`'s, and they do not move.
        assert_eq!(SUPERBLOCK_OFFSET, 1024);
        assert_eq!(BLOCKS_LO_OFFSET, 0x04);
        assert_eq!(FREE_BLOCKS_LO_OFFSET, 0x0C);
        assert_eq!(LOG_BLOCK_SIZE_OFFSET, 0x18);
        assert_eq!(MAGIC_OFFSET, 0x38);
        assert_eq!(INCOMPAT_OFFSET, 0x60);
        assert_eq!(BLOCKS_HI_OFFSET, 0x150);
        assert_eq!(FREE_BLOCKS_HI_OFFSET, 0x158);
        assert_eq!(EXT_MAGIC, 0xEF53);
        const _: () = assert!(SUPERBLOCK_BYTES >= FREE_BLOCKS_HI_OFFSET + 4);
    }

    /// The type GUIDs are the specification's, spelled the way the table reader
    /// renders them.
    #[test]
    fn the_type_guids_are_the_specifications() {
        assert_eq!(
            LINUX_FS_TYPE,
            Uuid::parse_str("0fc63daf-8483-4772-8e79-3d69d8477de4").unwrap()
        );
        assert_eq!(
            EFI_SYSTEM_TYPE,
            Uuid::parse_str("c12a7328-f81f-11d2-ba4b-00a0c93ec93b").unwrap()
        );
        assert_eq!(gpt::partition_types::EFI.guid, EFI_SYSTEM_TYPE);
    }

    /// A raw image too short for the region asked of it is an error before any read, and
    /// a partition whose entry runs past the image is refused, not read.
    #[test]
    fn every_read_is_held_to_the_images_end() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("small.img");
        std::fs::write(&path, vec![0u8; 4096]).unwrap();
        let image = RawImage::open(&path, dir.path()).unwrap();
        assert!(image.sha256_at(0, 4096).is_ok());
        assert!(image.sha256_at(4000, 97).is_err());
        assert!(image.sha256_at(u64::MAX, 2).is_err());
        let table = DiskTable {
            entries: vec![TableEntry {
                index: 1,
                name: "rootfs".into(),
                first_lba: 4,
                last_lba: 1 << 40,
                type_guid: LINUX_FS_TYPE.hyphenated().to_string(),
                part_guid: Uuid::from_bytes([7; 16]).hyphenated().to_string(),
                flags: 0,
            }],
            backup: Ok(()),
        };
        let err = table
            .by_guid(&image, Uuid::from_bytes([7; 16]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("past the image's end"), "{err}");
        assert!(table.by_guid(&image, Uuid::from_bytes([8; 16])).is_err());
    }

    /// A partition too short for a superblock, and one whose superblock is not ext4, are
    /// errors with no read beyond the fixed superblock window.
    #[test]
    fn a_superblock_is_read_only_where_one_can_be() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("p.img");
        std::fs::write(&path, vec![0u8; 8192]).unwrap();
        let image = RawImage::open(&path, dir.path()).unwrap();
        let part = |bytes| Partition {
            index: 1,
            name: "x".into(),
            start: 0,
            bytes,
            type_guid: LINUX_FS_TYPE,
            bootable: true,
        };
        assert!(superblock(&image, &part(1024))
            .unwrap_err()
            .to_string()
            .contains("too short"));
        assert!(superblock(&image, &part(8192))
            .unwrap_err()
            .to_string()
            .contains("no ext4 superblock"));
    }
}
