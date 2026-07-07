//! Pure disk geometry for the image node: turn the resolved raw-gap
//! offsets and image size (authored strings, [`boot2deb_core::size`]) into the
//! exact byte and LBA layout the [GPT](super::gpt) and [ext4](super::ext4) steps
//! write against.
//!
//! Deterministic and side-effect-free, so the layout — where the rootfs
//! partition starts, how large the ext4 filesystem is, whether the bootloader
//! payloads fit the gap — is unit-tested without touching a disk. The only
//! external contract is the sector size and the standard GPT reservation
//! (primary table at the front, backup table in the last 33 sectors); the actual
//! usable range is re-validated by the `gpt` crate when the partition is added.

use crate::error::EngineError;
use boot2deb_core::model::Offsets;
use boot2deb_core::size::parse_size;

/// Disk logical block (sector) size. RK images use 512-byte sectors, matching the
/// raw-gap `bs`/`seek` arithmetic and the `gpt` crate's default.
pub(crate) const SECTOR: u64 = 512;

/// ext4 block size arcbox-ext4 formats at (its only supported value). The rootfs
/// filesystem is a whole number of these, further grouped into [`EXT4_BLOCK_GROUP`]s.
pub(crate) const EXT4_BLOCK: u64 = 4096;

/// One ext4 block group in bytes: 128 MiB. At [`EXT4_BLOCK`] the group's block
/// bitmap is a single block, mapping `EXT4_BLOCK * 8` (32768) blocks, so a group
/// spans `EXT4_BLOCK * 8 * EXT4_BLOCK` bytes.
///
/// The rootfs filesystem size is floored to a multiple of this. arcbox-ext4 rounds
/// a filesystem *up* to a whole number of block groups — growing its backing file
/// to match — so a size that is only [`EXT4_BLOCK`]-aligned would format larger
/// than the partition sized to hold it and refuse to mount ("bad geometry: block
/// count N exceeds size of device"). A group-aligned size formats to exactly the
/// partition size.
pub(crate) const EXT4_BLOCK_GROUP: u64 = EXT4_BLOCK * 8 * EXT4_BLOCK;

/// Sectors the primary GPT reserves at the front: protective MBR (LBA 0), the
/// GPT header (LBA 1), and the 128-entry × 128-byte partition array (32 sectors,
/// LBA 2..33). The first usable LBA is therefore 34.
const GPT_FRONT_SECTORS: u64 = 34;

/// Sectors the backup GPT reserves at the end: the mirrored 32-sector entry array
/// plus the backup header. The last usable LBA is `total_lba - GPT_BACK_SECTORS - 1`.
const GPT_BACK_SECTORS: u64 = 33;

/// The resolved byte/LBA layout of one image.
///
/// All offsets are byte counts from the start of the medium. `rootfs_first_lba`
/// / `rootfs_length_lba` are the exact GPT partition bounds — the partition fills
/// the usable disk after the raw gap and GPT reservations. `rootfs_bytes` (a
/// multiple of [`EXT4_BLOCK_GROUP`], `<= rootfs_length_lba * SECTOR`) is the size
/// of the ext4 filesystem placed in that partition; it is smaller than the
/// partition by up to one block group and is grown to fill it on first boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Geometry {
    /// Whole-disk size in bytes (the resolved `image_size`).
    pub(crate) total_size: u64,
    /// `idbloader.img` byte offset in the raw gap.
    pub(crate) idbloader_off: u64,
    /// `u-boot.itb` byte offset in the raw gap.
    pub(crate) uboot_itb_off: u64,
    /// Rootfs partition start byte offset.
    pub(crate) rootfs_off: u64,
    /// Rootfs partition first LBA (`rootfs_off / SECTOR`).
    pub(crate) rootfs_first_lba: u64,
    /// Rootfs partition length in sectors — the partition spans the whole usable
    /// disk after the raw gap and GPT reservations.
    pub(crate) rootfs_length_lba: u64,
    /// ext4 filesystem size in bytes: a multiple of [`EXT4_BLOCK_GROUP`], at most
    /// `rootfs_length_lba * SECTOR`. Smaller than the partition by up to one block
    /// group; a first-boot resize grows it to fill the partition.
    pub(crate) rootfs_bytes: u64,
}

impl Geometry {
    /// Resolve the layout from the authored offsets and image size, validating
    /// every invariant the writers rely on. Returns [`EngineError::ImageGeometry`]
    /// on any malformed value, bad ordering, misalignment, or an image too small
    /// to hold the GPT plus a rootfs partition.
    pub(crate) fn resolve(offsets: &Offsets, image_size: &str) -> Result<Geometry, EngineError> {
        let total_size = parse_size(image_size)?;
        let idbloader_off = parse_size(&offsets.idbloader)?;
        let uboot_itb_off = parse_size(&offsets.uboot_itb)?;
        let rootfs_off = parse_size(&offsets.rootfs)?;

        // Every offset and the total must be whole sectors — partitions and the
        // GPT are sector-addressed.
        for (what, v) in [
            ("image size", total_size),
            ("idbloader offset", idbloader_off),
            ("u-boot.itb offset", uboot_itb_off),
            ("rootfs offset", rootfs_off),
        ] {
            if !v.is_multiple_of(SECTOR) {
                return Err(geom(format!("{what} ({v}) is not a multiple of {SECTOR}")));
            }
        }
        // The rootfs partition additionally aligns to the ext4 block size.
        if !rootfs_off.is_multiple_of(EXT4_BLOCK) {
            return Err(geom(format!(
                "rootfs offset ({rootfs_off}) is not a multiple of the ext4 block size {EXT4_BLOCK}"
            )));
        }

        // Raw-gap ordering: both bootloader payloads sit ahead of the rootfs, and
        // the first one clears the primary GPT table.
        if idbloader_off < GPT_FRONT_SECTORS * SECTOR {
            return Err(geom(format!(
                "idbloader offset ({idbloader_off}) overlaps the primary GPT (first {} bytes reserved)",
                GPT_FRONT_SECTORS * SECTOR
            )));
        }
        if !(idbloader_off < uboot_itb_off && uboot_itb_off < rootfs_off) {
            return Err(geom(format!(
                "raw-gap offsets must increase: idbloader ({idbloader_off}) < u-boot.itb ({uboot_itb_off}) < rootfs ({rootfs_off})"
            )));
        }

        let total_lba = total_size / SECTOR;
        let rootfs_first_lba = rootfs_off / SECTOR;
        // The backup GPT occupies the final GPT_BACK_SECTORS; the last LBA the
        // rootfs may use is one before it.
        let last_usable_lba = total_lba
            .checked_sub(GPT_BACK_SECTORS + 1)
            .filter(|last| *last >= rootfs_first_lba)
            .ok_or_else(|| {
                geom(format!(
                    "image size ({total_size}) is too small for a rootfs partition at offset {rootfs_off}"
                ))
            })?;

        let available_bytes = (last_usable_lba - rootfs_first_lba + 1) * SECTOR;
        // The GPT partition fills the usable range (floored to a whole ext4 block) —
        // one rootfs partition spanning the disk, as the reference image lays out.
        let partition_bytes = (available_bytes / EXT4_BLOCK) * EXT4_BLOCK;
        // The ext4 filesystem is floored to a whole 128 MiB block group, not merely a
        // block: arcbox-ext4 rounds a filesystem *up* to whole groups, so only a
        // group-aligned size is guaranteed not to overflow the partition (see
        // [`EXT4_BLOCK_GROUP`]). A group multiple is necessarily <= the block-floored
        // partition, so the filesystem always fits; first-boot resize grows it to
        // fill the partition (and, past the image, the physical medium).
        let rootfs_bytes = (available_bytes / EXT4_BLOCK_GROUP) * EXT4_BLOCK_GROUP;
        if rootfs_bytes == 0 {
            return Err(geom(format!(
                "usable rootfs area ({available_bytes} bytes) is smaller than one ext4 block group ({EXT4_BLOCK_GROUP} bytes)"
            )));
        }
        let rootfs_length_lba = partition_bytes / SECTOR;

        Ok(Geometry {
            total_size,
            idbloader_off,
            uboot_itb_off,
            rootfs_off,
            rootfs_first_lba,
            rootfs_length_lba,
            rootfs_bytes,
        })
    }

    /// Verify the two bootloader payloads fit their gap slots — `idbloader.img`
    /// ends before the `u-boot.itb` offset, and `u-boot.itb` ends before the
    /// rootfs partition. Sizes are only known once the payloads are staged, so
    /// this is checked at write time rather than in [`resolve`](Self::resolve).
    pub(crate) fn check_payload_fit(
        &self,
        idbloader_len: u64,
        uboot_itb_len: u64,
    ) -> Result<(), EngineError> {
        // `checked_add` so a pathological payload length near `u64::MAX` cannot wrap the
        // end offset in release and slip past the overrun guard (GEO-1); an overflow is
        // the same "does not fit" verdict, reported explicitly.
        let idbloader_end = self.idbloader_off.checked_add(idbloader_len).ok_or_else(|| {
            geom(format!(
                "idbloader.img length ({idbloader_len} bytes) overflows the offset arithmetic"
            ))
        })?;
        if idbloader_end > self.uboot_itb_off {
            return Err(geom(format!(
                "idbloader.img ({idbloader_len} bytes @ {}) overruns the u-boot.itb offset ({})",
                self.idbloader_off, self.uboot_itb_off
            )));
        }
        let uboot_itb_end = self.uboot_itb_off.checked_add(uboot_itb_len).ok_or_else(|| {
            geom(format!(
                "u-boot.itb length ({uboot_itb_len} bytes) overflows the offset arithmetic"
            ))
        })?;
        if uboot_itb_end > self.rootfs_off {
            return Err(geom(format!(
                "u-boot.itb ({uboot_itb_len} bytes @ {}) overruns the rootfs offset ({})",
                self.uboot_itb_off, self.rootfs_off
            )));
        }
        Ok(())
    }
}

/// Build an [`EngineError::ImageGeometry`] with `detail`.
fn geom(detail: String) -> EngineError {
    EngineError::ImageGeometry { detail }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The RK1 raw-gap offsets (boot-methods/rockchip-rkbin.toml).
    fn rk1_offsets() -> Offsets {
        Offsets {
            idbloader: "32KiB".into(),
            uboot_itb: "8MiB".into(),
            rootfs: "16MiB".into(),
        }
    }

    #[test]
    fn resolves_the_rk1_2g_layout() {
        let g = Geometry::resolve(&rk1_offsets(), "2G").unwrap();
        assert_eq!(g.total_size, 2 * 1024 * 1024 * 1024);
        assert_eq!(g.idbloader_off, 32 * 1024);
        assert_eq!(g.uboot_itb_off, 8 * 1024 * 1024);
        assert_eq!(g.rootfs_off, 16 * 1024 * 1024);
        assert_eq!(g.rootfs_first_lba, 16 * 1024 * 1024 / SECTOR); // 32768
        // The ext4 filesystem is floored to a whole 128 MiB block group so
        // arcbox-ext4's round-up-to-groups cannot exceed the partition. For 2 GiB the
        // usable ~1.98 GiB floors to 15 groups = 1.875 GiB.
        assert!(g.rootfs_bytes.is_multiple_of(EXT4_BLOCK_GROUP));
        assert_eq!(g.rootfs_bytes, 15 * EXT4_BLOCK_GROUP);
        // The partition fills the usable disk, so it is at least the filesystem (a
        // first-boot resize grows the fs into it) and ends before the backup GPT.
        assert!(g.rootfs_bytes <= g.rootfs_length_lba * SECTOR);
        let end_lba = g.rootfs_first_lba + g.rootfs_length_lba;
        assert!(end_lba <= g.total_size / SECTOR - GPT_BACK_SECTORS);
    }

    #[test]
    fn rejects_bad_ordering_and_alignment() {
        // rootfs before u-boot.itb.
        let bad = Offsets {
            idbloader: "32KiB".into(),
            uboot_itb: "16MiB".into(),
            rootfs: "8MiB".into(),
        };
        assert!(Geometry::resolve(&bad, "2G").is_err());
        // idbloader inside the primary GPT reservation.
        let bad = Offsets {
            idbloader: "512".into(),
            uboot_itb: "8MiB".into(),
            rootfs: "16MiB".into(),
        };
        assert!(Geometry::resolve(&bad, "2G").is_err());
        // rootfs offset 512-aligned (16385 sectors) but not 4 KiB-aligned.
        let bad = Offsets {
            idbloader: "32KiB".into(),
            uboot_itb: "8MiB".into(),
            rootfs: "8389120".into(), // 16385 * 512; not a multiple of 4096
        };
        assert!(Geometry::resolve(&bad, "2G").is_err());
    }

    #[test]
    fn rejects_image_too_small_for_the_rootfs() {
        // 8 MiB total cannot hold a rootfs starting at 16 MiB.
        assert!(Geometry::resolve(&rk1_offsets(), "8MiB").is_err());
    }

    #[test]
    fn rejects_image_too_small_for_one_block_group() {
        // The rootfs clears the 16 MiB gap, but the ~84 MiB left is under one 128 MiB
        // ext4 block group, so no valid group-aligned filesystem fits.
        assert!(Geometry::resolve(&rk1_offsets(), "100MiB").is_err());
    }

    #[test]
    fn payload_fit_catches_overruns() {
        let g = Geometry::resolve(&rk1_offsets(), "2G").unwrap();
        // Comfortably-sized payloads fit.
        assert!(g.check_payload_fit(400 * 1024, 2 * 1024 * 1024).is_ok());
        // An idbloader larger than the 32KiB..8MiB slot is rejected.
        assert!(g.check_payload_fit(9 * 1024 * 1024, 1024).is_err());
        // A u-boot.itb spilling past the 16 MiB rootfs start is rejected.
        assert!(g.check_payload_fit(1024, 9 * 1024 * 1024).is_err());
        // A payload length that would wrap the end offset is an error, not a
        // wraparound that slips past the guard (GEO-1).
        assert!(g.check_payload_fit(u64::MAX, 1024).is_err());
        assert!(g.check_payload_fit(1024, u64::MAX).is_err());
    }
}
