//! Pure disk geometry for the image node: turn the resolved boot layout and image
//! size (authored strings, [`boot2deb_core::size`]) into the exact byte and LBA
//! layout the [GPT](super::gpt) and [ext4](super::ext4) steps write against.
//!
//! Deterministic and side-effect-free, so the layout — where the rootfs
//! partition starts, how large the ext4 filesystem is, whether the boot payload
//! fits its slot — is unit-tested without touching a disk. The only
//! external contract is the sector size and the standard GPT reservation
//! (primary table at the front, backup table in the last 33 sectors); the actual
//! usable range is re-validated by the `gpt` crate when the partition is added.
//!
//! What sits ahead of the rootfs is the boot method's business, and the two shapes
//! are genuinely different: `rockchip-rkbin` writes two payloads into a *raw gap*
//! outside any partition, while `depthcharge` puts one signed payload in a *GPT
//! partition* of its own. [`BootGeometry`] carries that difference; everything after
//! it — the rootfs partition, the filesystem, the backup table — is shared.

use crate::error::EngineError;
use boot2deb_core::chromeos::{kpart_flags, SPARE_KPART_FLAGS};
use boot2deb_core::model::{Offsets, ResolvedBoot};
use boot2deb_core::size::parse_size;

/// Disk logical block (sector) size. RK images use 512-byte sectors, matching the
/// raw-gap `bs`/`seek` arithmetic and the `gpt` crate's default.
pub(crate) const SECTOR: u64 = 512;

/// ext4 block size the rootfs filesystem is formatted with (`mke2fs -b`). The
/// filesystem is a whole number of these, sized to exactly fill its partition.
pub(crate) const EXT4_BLOCK: u64 = 4096;

/// Smallest rootfs filesystem the geometry accepts: one 128 MiB ext4 block
/// group. `mke2fs` can format smaller, but a Debian rootfs cannot fit in one —
/// rejecting here fails a mis-sized image at resolution time, before any stage
/// runs, instead of at the format's ENOSPC.
const MIN_ROOTFS_BYTES: u64 = EXT4_BLOCK * 8 * EXT4_BLOCK;

/// Sectors the primary GPT reserves at the front: protective MBR (LBA 0), the
/// GPT header (LBA 1), and the 128-entry × 128-byte partition array (32 sectors,
/// LBA 2..33). The first usable LBA is therefore 34.
const GPT_FRONT_SECTORS: u64 = 34;

/// Sectors the backup GPT reserves at the end: the mirrored 32-sector entry array
/// plus the backup header. The last usable LBA is `total_lba - GPT_BACK_SECTORS - 1`.
const GPT_BACK_SECTORS: u64 = 33;

/// What the boot method puts ahead of the rootfs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum BootGeometry {
    /// `rockchip-rkbin`: two payloads written into a raw gap outside any partition.
    RawGap {
        /// `idbloader.img` byte offset.
        idbloader_off: u64,
        /// `u-boot.itb` byte offset.
        uboot_itb_off: u64,
    },
    /// `depthcharge`: a signed kernel FIT in a ChromeOS kernel partition, which the
    /// firmware finds by scanning the GPT for its type GUID.
    Kpart {
        /// The kernel slots, in on-disk order and back to back. `slots[0]` carries
        /// the signed payload; the rest ship empty at priority 0 so an on-device
        /// upgrade has a slot to write that is not the one it booted from.
        slots: Vec<KpartSlot>,
    },
}

/// One ChromeOS kernel slot's placement and attribute word.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct KpartSlot {
    /// Partition start byte offset.
    pub(crate) offset: u64,
    /// Partition first LBA.
    pub(crate) first_lba: u64,
    /// Partition length in sectors.
    pub(crate) length_lba: u64,
    /// The GPT entry's 64-bit attribute word (priority / tries / successful).
    pub(crate) flags: u64,
}

/// The resolved byte/LBA layout of one image.
///
/// All offsets are byte counts from the start of the medium. `rootfs_first_lba`
/// / `rootfs_length_lba` are the exact GPT partition bounds — the partition fills
/// the usable disk after whatever the boot method owns at the head, and the GPT
/// reservations. `rootfs_bytes` (a multiple of [`EXT4_BLOCK`], and exactly
/// `rootfs_length_lba * SECTOR`) is the size of the ext4 filesystem placed in that
/// partition; first boot grows it past the image onto the physical medium.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Geometry {
    /// Whole-disk size in bytes (the resolved `image_size`).
    pub(crate) total_size: u64,
    /// What the boot method places ahead of the rootfs.
    pub(crate) boot: BootGeometry,
    /// Rootfs partition start byte offset.
    pub(crate) rootfs_off: u64,
    /// Rootfs partition first LBA (`rootfs_off / SECTOR`).
    pub(crate) rootfs_first_lba: u64,
    /// Rootfs partition length in sectors — the partition spans the whole usable
    /// disk after the boot region and GPT reservations.
    pub(crate) rootfs_length_lba: u64,
    /// ext4 filesystem size in bytes: a multiple of [`EXT4_BLOCK`], exactly
    /// `rootfs_length_lba * SECTOR` — the filesystem fills its partition; the
    /// first-boot resize grows both onto the physical medium.
    pub(crate) rootfs_bytes: u64,
}

impl Geometry {
    /// Resolve the layout from the resolved boot configuration and image size,
    /// validating every invariant the writers rely on. Returns
    /// [`EngineError::ImageGeometry`] on any malformed value, bad ordering,
    /// misalignment, or an image too small to hold the GPT plus a rootfs partition.
    pub(crate) fn resolve(boot: &ResolvedBoot, image_size: &str) -> Result<Geometry, EngineError> {
        let total_size = parse_size(image_size)?;
        let rootfs_off = parse_size(boot.rootfs_offset())?;
        let (boot_geom, boot_end) = match boot {
            ResolvedBoot::RockchipRkbin(b) => Self::raw_gap(&b.offsets, rootfs_off)?,
            ResolvedBoot::Depthcharge(b) => Self::kpart(b)?,
        };

        // Every offset and the total must be whole sectors — partitions and the
        // GPT are sector-addressed.
        for (what, v) in [("image size", total_size), ("rootfs offset", rootfs_off)] {
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
        // Whatever the boot method owns at the head must end before the rootfs does.
        if boot_end > rootfs_off {
            return Err(geom(format!(
                "the boot region ends at {boot_end}, past the rootfs offset ({rootfs_off})"
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
        // The GPT partition fills the usable range, floored to a whole ext4 block —
        // one rootfs partition spanning the disk. The filesystem is formatted to
        // exactly the partition size (`mke2fs` takes an explicit block count).
        let partition_bytes = (available_bytes / EXT4_BLOCK) * EXT4_BLOCK;
        let rootfs_bytes = partition_bytes;
        if rootfs_bytes < MIN_ROOTFS_BYTES {
            return Err(geom(format!(
                "usable rootfs area ({available_bytes} bytes) is smaller than the {MIN_ROOTFS_BYTES}-byte minimum"
            )));
        }
        let rootfs_length_lba = partition_bytes / SECTOR;

        Ok(Geometry {
            total_size,
            boot: boot_geom,
            rootfs_off,
            rootfs_first_lba,
            rootfs_length_lba,
            rootfs_bytes,
        })
    }

    /// The raw-gap boot region: two payloads written outside any partition, ahead of
    /// the rootfs. Returns the geometry and the byte the region ends at.
    fn raw_gap(offsets: &Offsets, rootfs_off: u64) -> Result<(BootGeometry, u64), EngineError> {
        let idbloader_off = parse_size(&offsets.idbloader)?;
        let uboot_itb_off = parse_size(&offsets.uboot_itb)?;
        for (what, v) in [
            ("idbloader offset", idbloader_off),
            ("u-boot.itb offset", uboot_itb_off),
        ] {
            if !v.is_multiple_of(SECTOR) {
                return Err(geom(format!("{what} ({v}) is not a multiple of {SECTOR}")));
            }
        }
        // The payloads live outside any partition, so nothing but this check keeps
        // the first one from landing on the primary GPT table.
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
        Ok((
            BootGeometry::RawGap {
                idbloader_off,
                uboot_itb_off,
            },
            uboot_itb_off,
        ))
    }

    /// The ChromeOS kernel slots: real GPT partitions, so they must be sector-aligned
    /// and clear the primary GPT table. Returns the geometry and the byte the **last**
    /// slot ends at — which the caller checks against the rootfs offset, the same as it
    /// does for a raw gap.
    ///
    /// The slots are laid back to back from `kpart.offset`, so they cannot overlap each
    /// other by construction; the only placement question left is whether the set as a
    /// whole clears the GPT at the front and the rootfs behind it.
    fn kpart(
        boot: &boot2deb_core::model::ResolvedDepthchargeBoot,
    ) -> Result<(BootGeometry, u64), EngineError> {
        let offset = parse_size(&boot.kpart.offset)?;
        let size = parse_size(&boot.kpart.size)?;
        for (what, v) in [("kpart offset", offset), ("kpart size", size)] {
            if !v.is_multiple_of(SECTOR) {
                return Err(geom(format!("{what} ({v}) is not a multiple of {SECTOR}")));
            }
        }
        if size == 0 {
            return Err(geom("kpart size is zero — nothing could be booted".into()));
        }
        if offset < GPT_FRONT_SECTORS * SECTOR {
            return Err(geom(format!(
                "kpart offset ({offset}) overlaps the primary GPT (first {} bytes reserved)",
                GPT_FRONT_SECTORS * SECTOR
            )));
        }
        // The payload slot's attribute word is recomputed from the resolved fields
        // rather than trusted: resolution already range-checked them, so this cannot
        // fail, and keeping the packing in one place means the disk can only ever carry
        // what `kpart_flags` produces.
        let payload_flags =
            kpart_flags(boot.kpart.priority, boot.kpart.tries, boot.kpart.successful)?;

        let mut slots = Vec::with_capacity(usize::from(boot.kpart.slots));
        let mut start = offset;
        for i in 0..boot.kpart.slots {
            let end = start.checked_add(size).ok_or_else(|| {
                geom(format!(
                    "kernel slot {i} at {start} + size ({size}) overflows the offset arithmetic"
                ))
            })?;
            slots.push(KpartSlot {
                offset: start,
                first_lba: start / SECTOR,
                length_lba: size / SECTOR,
                // Only the first slot ships a payload. Every other is empty, and an
                // empty slot must never be a boot candidate — priority 0 is exactly
                // that, and it is what `SPARE_KPART_FLAGS` encodes.
                flags: if i == 0 {
                    payload_flags
                } else {
                    SPARE_KPART_FLAGS
                },
            });
            start = end;
        }
        // `start` has advanced past the last slot, which is where the rootfs may begin.
        Ok((BootGeometry::Kpart { slots }, start))
    }

    /// Verify the boot payload(s) fit the space the geometry gave them, before any
    /// of the image is written. Sizes are only known once the payloads are staged,
    /// so this is checked at write time rather than in [`resolve`](Self::resolve).
    ///
    /// `payloads` are the boot payloads in the order the method writes them: two for
    /// the raw gap (`idbloader.img`, `u-boot.itb`), one for depthcharge (the signed
    /// kernel partition image).
    pub(crate) fn check_payload_fit(&self, payloads: &[(&str, u64)]) -> Result<(), EngineError> {
        // `checked_add` so a pathological payload length near `u64::MAX` cannot wrap
        // the end offset in release and slip past the overrun guard; an
        // overflow is the same "does not fit" verdict, reported explicitly.
        let fits = |what: &str, len: u64, start: u64, limit: u64, limit_name: &str| {
            let end = start.checked_add(len).ok_or_else(|| {
                geom(format!(
                    "{what} length ({len} bytes) overflows the offset arithmetic"
                ))
            })?;
            if end > limit {
                return Err(geom(format!(
                    "{what} ({len} bytes @ {start}) overruns the {limit_name} ({limit})"
                )));
            }
            Ok(())
        };
        match self.boot {
            BootGeometry::RawGap {
                idbloader_off,
                uboot_itb_off,
            } => {
                let [(_, idb_len), (_, itb_len)] = payloads else {
                    return Err(geom(format!(
                        "the raw-gap boot region takes exactly 2 payloads, got {}",
                        payloads.len()
                    )));
                };
                fits("idbloader.img", *idb_len, idbloader_off, uboot_itb_off, "u-boot.itb offset")?;
                fits("u-boot.itb", *itb_len, uboot_itb_off, self.rootfs_off, "rootfs offset")?;
            }
            BootGeometry::Kpart { ref slots } => {
                let [(what, len)] = payloads else {
                    return Err(geom(format!(
                        "the kernel partition takes exactly 1 payload, got {}",
                        payloads.len()
                    )));
                };
                // Only the first slot is written at build time; the spares ship empty
                // and are filled by the first on-device kernel upgrade. They are the
                // same size, so a payload that fits the first fits any of them — which
                // is what makes an upgrade to a spare safe.
                let payload = slots.first().ok_or_else(|| {
                    geom("the depthcharge geometry resolved to no kernel slots".into())
                })?;
                fits(
                    what,
                    *len,
                    payload.offset,
                    payload.offset + payload.length_lba * SECTOR,
                    "kernel partition",
                )?;
            }
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
    use boot2deb_core::model::{Kpart, ResolvedDepthchargeBoot, ResolvedRkbinBoot, Rkbin};

    /// The RK1 raw-gap layout (boot-methods/rockchip-rkbin.toml).
    fn rk1_boot() -> ResolvedBoot {
        rk1_boot_with("32KiB", "8MiB", "16MiB")
    }

    fn rk1_boot_with(idbloader: &str, uboot_itb: &str, rootfs: &str) -> ResolvedBoot {
        ResolvedBoot::RockchipRkbin(ResolvedRkbinBoot {
            uboot_defconfig: "turing-rk1-rk3588_defconfig".into(),
            uboot_source: "https://example/u-boot.git".into(),
            uboot_ref: "v2026.04".into(),
            rkbin: Rkbin {
                atf: "atf.elf".into(),
                tpl: "tpl.bin".into(),
                bl32: None,
            },
            offsets: Offsets {
                idbloader: idbloader.into(),
                uboot_itb: uboot_itb.into(),
                rootfs: rootfs.into(),
            },
        })
    }

    /// The C201 kernel-slot layout (boot-methods/depthcharge.toml): two 16 MiB slots
    /// from 12 MiB, rootfs behind them at 44 MiB.
    fn c201_boot() -> ResolvedBoot {
        c201_boot_with("12MiB", "16MiB", 2, "44MiB")
    }

    fn c201_boot_with(offset: &str, size: &str, slots: u8, rootfs: &str) -> ResolvedBoot {
        ResolvedBoot::Depthcharge(ResolvedDepthchargeBoot {
            board: "speedy".into(),
            kpart: Kpart {
                offset: offset.into(),
                size: size.into(),
                slots,
                priority: 10,
                tries: 5,
                successful: true,
                flags: 0x015A_0000_0000_0000,
            },
            cmdline: "console=tty1 rootwait ro panic=30".into(),
            rootfs_offset: rootfs.into(),
        })
    }

    #[test]
    fn resolves_the_rk1_2g_layout() {
        let g = Geometry::resolve(&rk1_boot(), "2G").unwrap();
        assert_eq!(g.total_size, 2 * 1024 * 1024 * 1024);
        assert_eq!(
            g.boot,
            BootGeometry::RawGap {
                idbloader_off: 32 * 1024,
                uboot_itb_off: 8 * 1024 * 1024,
            }
        );
        assert_eq!(g.rootfs_off, 16 * 1024 * 1024);
        assert_eq!(g.rootfs_first_lba, 16 * 1024 * 1024 / SECTOR); // 32768
        // The filesystem fills the partition exactly: the usable range after the
        // 16 MiB gap and the 34-sector backup-GPT+1 tail, floored to a whole ext4
        // block. For 2 GiB that is 520187 blocks.
        assert!(g.rootfs_bytes.is_multiple_of(EXT4_BLOCK));
        assert_eq!(g.rootfs_bytes, 520_187 * EXT4_BLOCK);
        assert_eq!(g.rootfs_bytes, g.rootfs_length_lba * SECTOR);
        let end_lba = g.rootfs_first_lba + g.rootfs_length_lba;
        assert!(end_lba <= g.total_size / SECTOR - GPT_BACK_SECTORS);
    }

    #[test]
    fn resolves_the_c201_kernel_slot_layout() {
        // The exact numbers the C201 image carries: KERN-A at LBA 24576 spanning 32768
        // sectors, KERN-B abutting it at LBA 57344, and the rootfs behind both at LBA
        // 90112.
        let g = Geometry::resolve(&c201_boot(), "4G").unwrap();
        assert_eq!(
            g.boot,
            BootGeometry::Kpart {
                slots: vec![
                    KpartSlot {
                        offset: 12 * 1024 * 1024,
                        first_lba: 24_576,
                        length_lba: 32_768,
                        flags: 0x015A_0000_0000_0000,
                    },
                    KpartSlot {
                        offset: 28 * 1024 * 1024,
                        first_lba: 57_344,
                        length_lba: 32_768,
                        // The spare ships empty, and priority 0 is "never boot" — the
                        // firmware must not pick a slot with no kernel in it.
                        flags: SPARE_KPART_FLAGS,
                    },
                ],
            }
        );
        assert_eq!(g.rootfs_first_lba, 90_112);
        assert!(g.rootfs_bytes.is_multiple_of(EXT4_BLOCK));
        assert_eq!(g.rootfs_bytes, g.rootfs_length_lba * SECTOR);
        // The slots abut each other, and the rootfs starts exactly where the last one
        // ends: nothing overlaps, and nothing is wasted between them.
        let BootGeometry::Kpart { ref slots } = g.boot else {
            panic!("expected kernel slots")
        };
        for pair in slots.windows(2) {
            assert_eq!(
                pair[0].offset + pair[0].length_lba * SECTOR,
                pair[1].offset,
                "kernel slots must abut, never overlap"
            );
        }
        let last = slots.last().unwrap();
        assert_eq!(last.offset + last.length_lba * SECTOR, g.rootfs_off);
    }

    /// One slot is expressible — it is the shape with no fallback, and the geometry
    /// must not quietly invent a spare that the on-device upgrade path would then
    /// believe in.
    #[test]
    fn a_single_slot_layout_leaves_no_spare() {
        let g = Geometry::resolve(&c201_boot_with("12MiB", "16MiB", 1, "28MiB"), "4G").unwrap();
        let BootGeometry::Kpart { ref slots } = g.boot else {
            panic!("expected kernel slots")
        };
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].flags, 0x015A_0000_0000_0000);
    }

    #[test]
    fn rejects_bad_ordering_and_alignment() {
        // rootfs before u-boot.itb.
        assert!(Geometry::resolve(&rk1_boot_with("32KiB", "16MiB", "8MiB"), "2G").is_err());
        // idbloader inside the primary GPT reservation.
        assert!(Geometry::resolve(&rk1_boot_with("512", "8MiB", "16MiB"), "2G").is_err());
        // rootfs offset 512-aligned (16385 sectors) but not 4 KiB-aligned.
        assert!(Geometry::resolve(&rk1_boot_with("32KiB", "8MiB", "8389120"), "2G").is_err());
    }

    #[test]
    fn rejects_kernel_slots_that_collide() {
        // Overlapping the primary GPT: the firmware's own table would be destroyed.
        assert!(Geometry::resolve(&c201_boot_with("512", "16MiB", 2, "44MiB"), "4G").is_err());
        // The *second* slot runs into the rootfs: 12 + 2*16 = 44 MiB, past a rootfs at
        // 40 MiB. This is the collision a one-slot geometry cannot have, and the reason
        // the rootfs offset is checked against the last slot rather than the first — a
        // spare silently overlapping the rootfs would be a kernel upgrade that eats the
        // filesystem.
        assert!(Geometry::resolve(&c201_boot_with("12MiB", "16MiB", 2, "40MiB"), "4G").is_err());
        // A zero-size kernel slot could hold no kernel.
        assert!(Geometry::resolve(&c201_boot_with("12MiB", "0", 2, "44MiB"), "4G").is_err());
        // A gap between the last slot and the rootfs is allowed — only an overlap is not.
        assert!(Geometry::resolve(&c201_boot_with("12MiB", "8MiB", 2, "44MiB"), "4G").is_ok());
    }

    #[test]
    fn rejects_image_too_small_for_the_rootfs() {
        // 8 MiB total cannot hold a rootfs starting at 16 MiB.
        assert!(Geometry::resolve(&rk1_boot(), "8MiB").is_err());
    }

    #[test]
    fn rejects_rootfs_area_below_the_minimum() {
        // The rootfs clears the 16 MiB gap, but the ~84 MiB left is under the
        // 128 MiB minimum a Debian rootfs needs.
        assert!(Geometry::resolve(&rk1_boot(), "100MiB").is_err());
    }

    #[test]
    fn payload_fit_catches_overruns() {
        let g = Geometry::resolve(&rk1_boot(), "2G").unwrap();
        let gap = |idb: u64, itb: u64| g.check_payload_fit(&[("idbloader.img", idb), ("u-boot.itb", itb)]);
        // Comfortably-sized payloads fit.
        assert!(gap(400 * 1024, 2 * 1024 * 1024).is_ok());
        // An idbloader larger than the 32KiB..8MiB slot is rejected.
        assert!(gap(9 * 1024 * 1024, 1024).is_err());
        // A u-boot.itb spilling past the 16 MiB rootfs start is rejected.
        assert!(gap(1024, 9 * 1024 * 1024).is_err());
        // A payload length that would wrap the end offset is an error, not a
        // wraparound that slips past the guard.
        assert!(gap(u64::MAX, 1024).is_err());
        assert!(gap(1024, u64::MAX).is_err());
    }

    #[test]
    fn a_kernel_payload_must_fit_its_slot() {
        let g = Geometry::resolve(&c201_boot(), "4G").unwrap();
        let kpart = |len: u64| g.check_payload_fit(&[("vmlinuz.kpart", len)]);
        // The measured signed payload — 14,569,472 bytes — fits the 16 MiB slot.
        assert!(kpart(14_569_472).is_ok());
        // Exactly filling the slot is fine; one byte more is not. A kernel spilling
        // past KERN-A would be written over the head of KERN-B — corrupting the very
        // slot the upgrade path falls back to, which is the failure this bound exists
        // to prevent. The check is against the *slot*, not the whole kernel region:
        // the slots are equal-sized, so a payload that fits one fits any of them, and
        // that is exactly what makes writing a spare safe.
        assert!(kpart(16 * 1024 * 1024).is_ok());
        assert!(kpart(16 * 1024 * 1024 + 1).is_err());
        assert!(kpart(u64::MAX).is_err());
    }
}
