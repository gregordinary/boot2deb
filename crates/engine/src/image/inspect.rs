//! Read back what a *finished* image artifact says about itself: its partition table
//! and its rootfs filesystem's superblock.
//!
//! The counterpart of this module's `ext4` and `gpt` siblings, which write those
//! structures. This module exists so the acceptance gate asks the questions in the same
//! language the image was written in — the alternative is a second implementation of
//! both parsers, in a second language, that nothing tests.
//!
//! **Only the head of the artifact is read.** The GPT is the first 34 sectors and the
//! superblock is 1024 bytes into the rootfs partition, so a compressed multi-gigabyte
//! image costs a few hundred kilobytes of decompression rather than a full pass.
//!
//! Pure except for reading the artifact: no mount, no loop device, no root.

use crate::error::EngineError;
use std::path::Path;

/// The GPT partition label the image node gives the rootfs. The gate reads the
/// partition back by this name, so it is the same constant the writer uses.
pub const ROOTFS_PARTLABEL: &str = "rootfs";

/// Offset of the ext4 superblock within its partition. Fixed by the format: the first
/// 1024 bytes are reserved for a boot block.
const SUPERBLOCK_OFFSET: u64 = 1024;

/// `s_magic`, at offset 0x38 of the superblock.
const MAGIC_OFFSET: usize = 0x38;

/// The value `s_magic` must hold for the bytes to be an ext2/3/4 superblock.
const EXT_MAGIC: u16 = 0xEF53;

/// `s_free_blocks_count_lo`, at offset 0x0C.
const FREE_BLOCKS_LO_OFFSET: usize = 0x0C;

/// `s_free_blocks_count_hi`, at offset 0x158 — the high half of the 64-bit count on a
/// filesystem with `64bit` set, which every image this writes has.
const FREE_BLOCKS_HI_OFFSET: usize = 0x158;

/// Bytes to read past the superblock's start: enough to reach the high half of the
/// free-block count with room to spare.
const SUPERBLOCK_BYTES: usize = 0x400;

/// What the rootfs partition's own GPT entry says: where it starts and how large it is.
///
/// # Errors
///
/// [`EngineError::ImageVerifyGpt`] when the artifact carries no readable primary table,
/// or no partition labelled [`ROOTFS_PARTLABEL`] — a `-boot.img` legitimately has
/// neither, and the caller reports that rather than treating it as a zero-length one.
pub fn rootfs_partition(artifact: &Path) -> Result<RootfsPartition, EngineError> {
    let entry = crate::press::verify::planned_table(artifact)?
        .into_iter()
        .find(|e| e.name == ROOTFS_PARTLABEL)
        .ok_or_else(|| EngineError::ImageVerifyGpt {
            target: artifact.display().to_string(),
            detail: format!("no partition labelled '{ROOTFS_PARTLABEL}' in the primary GPT"),
        })?;
    Ok(RootfsPartition {
        start: entry.first_lba * 512,
        // Inclusive last LBA, so the length is one sector more than the difference.
        bytes: (entry.last_lba - entry.first_lba + 1) * 512,
    })
}

/// Where an image's rootfs partition sits and how large it is, in bytes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootfsPartition {
    /// Byte offset of the partition's first sector.
    pub start: u64,
    /// The partition's length. The filesystem inside must be exactly this: larger and
    /// it will not mount, smaller and the difference is wasted.
    pub bytes: u64,
}

/// The free-block count in the rootfs filesystem's superblock.
///
/// This is the number a fitted `image_size` was measured against — the formatter
/// decides how large the filesystem is from what it wrote, so nothing outside the
/// superblock says whether the slack a recipe asked for actually survived.
///
/// Both halves of the 64-bit count are read: every image this writes has `64bit` set,
/// and taking the low half alone would silently truncate a filesystem above 16 TiB.
///
/// # Errors
///
/// [`EngineError::ImageVerifyGpt`] when the artifact has no rootfs partition, and
/// [`EngineError::ImageFileInvalid`] when the bytes at its start are not a superblock.
pub fn rootfs_free_blocks(artifact: &Path) -> Result<u64, EngineError> {
    let part = rootfs_partition(artifact)?;
    let want = (part.start + SUPERBLOCK_OFFSET) as usize + SUPERBLOCK_BYTES;
    let prefix = crate::press::write::decompressed_prefix(artifact, want)?;
    let sb = (part.start + SUPERBLOCK_OFFSET) as usize;
    let field = |off: usize, len: usize| -> Option<&[u8]> { prefix.get(sb + off..sb + off + len) };
    let magic = field(MAGIC_OFFSET, 2)
        .map(|b| u16::from_le_bytes([b[0], b[1]]))
        .ok_or_else(|| EngineError::ImageFileInvalid {
            target: artifact.display().to_string(),
            detail: format!("the artifact ends before the superblock at offset {sb}"),
        })?;
    if magic != EXT_MAGIC {
        return Err(EngineError::ImageFileInvalid {
            target: artifact.display().to_string(),
            detail: format!(
                "the rootfs partition holds no ext4 superblock (magic {magic:#06x}, want \
                 {EXT_MAGIC:#06x})"
            ),
        });
    }
    let read32 = |off: usize| -> u64 {
        field(off, 4)
            .map(|b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as u64)
            .unwrap_or(0)
    };
    Ok(read32(FREE_BLOCKS_LO_OFFSET) | (read32(FREE_BLOCKS_HI_OFFSET) << 32))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The reader, against a real shipped artifact. The offsets above are a claim
    /// about the on-disk format; this is the claim exercised — the free-block count
    /// has to be a plausible fraction of a filesystem that fills its partition.
    ///
    /// `#[ignore]` because it needs a built image, which a plain `cargo test` has no
    /// business producing. Run it with an artifact:
    /// `IMAGE=build/<recipe>/artifacts/<stem>.img.xz cargo test -- --ignored`.
    #[test]
    #[ignore = "needs a built image artifact; set IMAGE=<path>"]
    fn the_superblock_reader_agrees_with_a_shipped_artifact() {
        let Some(image) = std::env::var_os("IMAGE") else {
            panic!("set IMAGE=<path to a built .img/.img.xz>");
        };
        let image = std::path::Path::new(&image);
        let part = rootfs_partition(image).expect("the artifact carries a rootfs partition");
        let free = rootfs_free_blocks(image).expect("its superblock reads");
        assert!(part.bytes > 0, "the partition has a length");
        // A rootfs the build just filled has free blocks, and fewer than the whole
        // partition holds — either extreme means a field was read at the wrong offset.
        assert!(free > 0, "a freshly formatted rootfs has free blocks");
        assert!(
            free * 4096 < part.bytes,
            "free space {free} blocks cannot exceed the partition"
        );
    }

    #[test]
    fn the_superblock_offsets_are_the_ones_the_format_fixes() {
        // Stated as a test because they are the whole of this module's contract with
        // the on-disk format: a wrong offset reads a different field and reports a
        // plausible number. These are `ext2_fs.h`'s, and they do not move.
        assert_eq!(SUPERBLOCK_OFFSET, 1024);
        assert_eq!(MAGIC_OFFSET, 0x38);
        assert_eq!(FREE_BLOCKS_LO_OFFSET, 0x0C);
        assert_eq!(FREE_BLOCKS_HI_OFFSET, 0x158);
        assert_eq!(EXT_MAGIC, 0xEF53);
        // The read has to reach the high half of the count. A `const` assertion, so it
        // is a compile error rather than a test failure if the window ever shrinks past
        // the last field this module reads.
        const _: () = assert!(SUPERBLOCK_BYTES >= FREE_BLOCKS_HI_OFFSET + 4);
    }
}
