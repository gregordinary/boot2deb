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
use boot2deb_core::press::SEED_PARTITION_BYTES;
use boot2deb_core::size::{parse_size, Slack as CoreSlack};
use ferrosys::Slack;

/// Disk logical block (sector) size. RK images use 512-byte sectors, matching the
/// raw-gap `bs`/`seek` arithmetic and the `gpt` crate's default.
pub(crate) const SECTOR: u64 = 512;

/// ext4 block size the rootfs filesystem is formatted with. The filesystem is a
/// whole number of these, sized to exactly fill its partition.
pub(crate) const EXT4_BLOCK: u64 = 4096;

/// Smallest rootfs filesystem the geometry accepts: one 128 MiB ext4 block
/// group. A smaller ext4 is legal, but a Debian rootfs cannot fit in one —
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
    /// Seed partition start byte offset (see [`BootRegion::seed_off`]). The
    /// partition is exactly [`SEED_PARTITION_BYTES`] long.
    pub(crate) seed_off: u64,
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

/// The head of the disk: what the boot method owns there, and where the rootfs
/// partition begins.
///
/// Resolved on its own because it is the half of the layout that does **not** answer to
/// the image's size. That is what lets a fitted image check its boot payload and plan its
/// filesystem before the disk size exists — under
/// [`ImageSize::Fit`](boot2deb_core::size::ImageSize::Fit) the size is decided *by* the
/// filesystem, so the two halves cannot be resolved in one pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct BootRegion {
    /// What the boot method places ahead of the rootfs.
    pub(crate) boot: BootGeometry,
    /// Seed partition start byte offset: the 1 MiB per-unit personalization
    /// partition ([`SEED_PARTITION_BYTES`]), placed in space the boot method
    /// already leaves free so no authored offset moves. Under `rockchip-rkbin`
    /// it is the last MiB before the rootfs — carved out of the `u-boot.itb`
    /// slot's tail, which [`check_payload_fit`](Self::check_payload_fit) then
    /// bounds at this offset. Under `depthcharge` it sits directly below the
    /// kernel slots, whose 12 MiB offset leaves the room; the slots' own end
    /// still abuts the rootfs exactly.
    pub(crate) seed_off: u64,
    /// Rootfs partition start byte offset — a multiple of both [`SECTOR`] and
    /// [`EXT4_BLOCK`], and at or past the byte the boot region ends at.
    pub(crate) rootfs_off: u64,
}

impl BootRegion {
    /// Resolve the head of the disk from the resolved boot configuration, validating
    /// every invariant that does not answer to the image's size: the offsets parse, they
    /// are aligned, they increase, they clear the primary GPT, and whatever the boot
    /// method owns ends at or before the rootfs begins.
    ///
    /// # Errors
    ///
    /// [`EngineError::ImageGeometry`] on any malformed value, bad ordering, or
    /// misalignment.
    pub(crate) fn resolve(boot: &ResolvedBoot) -> Result<BootRegion, EngineError> {
        let rootfs_off = parse_size(boot.rootfs_offset())?;
        let (boot_geom, boot_end) = match boot {
            ResolvedBoot::RockchipRkbin(b) => Self::raw_gap(&b.offsets, rootfs_off)?,
            ResolvedBoot::Depthcharge(b) => Self::kpart(b)?,
        };

        // Partitions and the GPT are sector-addressed.
        if !rootfs_off.is_multiple_of(SECTOR) {
            return Err(geom(format!(
                "rootfs offset ({rootfs_off}) is not a multiple of {SECTOR}"
            )));
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

        // Place the seed partition in the space the boot method leaves free (see
        // the field's docs). Alignment is inherited: the anchor is sector-aligned
        // (and, before the rootfs, 4 KiB-aligned) and the seed size is a multiple
        // of both, so subtraction preserves it.
        let seed_off = match &boot_geom {
            BootGeometry::RawGap { uboot_itb_off, .. } => {
                let seed_off = rootfs_off
                    .checked_sub(SEED_PARTITION_BYTES)
                    .ok_or_else(|| {
                        geom(format!(
                            "rootfs offset ({rootfs_off}) leaves no room for the \
                         {SEED_PARTITION_BYTES}-byte seed partition ahead of it"
                        ))
                    })?;
                // The seed comes out of the u-boot.itb slot's tail, which must
                // still start below it — a slot the seed swallowed whole is a
                // layout with nowhere to put the bootloader.
                if *uboot_itb_off >= seed_off {
                    return Err(geom(format!(
                        "the u-boot.itb offset ({uboot_itb_off}) leaves no room for the \
                         {SEED_PARTITION_BYTES}-byte seed partition before the rootfs \
                         ({rootfs_off}) — move the rootfs offset up"
                    )));
                }
                seed_off
            }
            BootGeometry::Kpart { slots } => {
                let first_slot = slots.first().ok_or_else(|| {
                    geom("the depthcharge geometry resolved to no kernel slots".into())
                })?;
                let seed_off = first_slot
                    .offset
                    .checked_sub(SEED_PARTITION_BYTES)
                    .filter(|off| *off >= GPT_FRONT_SECTORS * SECTOR)
                    .ok_or_else(|| {
                        geom(format!(
                            "the kernel slot offset ({}) leaves no room for the \
                             {SEED_PARTITION_BYTES}-byte seed partition between the primary \
                             GPT and the slots",
                            first_slot.offset
                        ))
                    })?;
                seed_off
            }
        };

        Ok(BootRegion {
            boot: boot_geom,
            seed_off,
            rootfs_off,
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
                fits(
                    "idbloader.img",
                    *idb_len,
                    idbloader_off,
                    uboot_itb_off,
                    "u-boot.itb offset",
                )?;
                // The seed partition owns the last MiB before the rootfs, so the
                // itb's budget ends where the seed begins, not where the rootfs
                // does — an itb that spilled into the seed would be corrupted by
                // the very first `flash --hostname`.
                fits(
                    "u-boot.itb",
                    *itb_len,
                    uboot_itb_off,
                    self.seed_off,
                    "seed partition offset",
                )?;
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

impl Geometry {
    /// Resolve the whole layout from the resolved boot configuration and an authored
    /// whole-disk size: the head of the disk, then the rootfs partition filling what is
    /// left of it.
    ///
    /// `total_size` is the already-parsed whole-disk size — the caller has read the
    /// authored `image_size` to learn whether it is fixed at all, so re-parsing the
    /// string here would be the same work twice and a second place for it to disagree.
    ///
    /// # Errors
    ///
    /// [`EngineError::ImageGeometry`] on any malformed value, bad ordering,
    /// misalignment, or an image too small to hold the GPT plus a rootfs partition.
    pub(crate) fn resolve(boot: &ResolvedBoot, total_size: u64) -> Result<Geometry, EngineError> {
        if !total_size.is_multiple_of(SECTOR) {
            return Err(geom(format!(
                "image size ({total_size}) is not a multiple of {SECTOR}"
            )));
        }
        let region = BootRegion::resolve(boot)?;
        let rootfs_off = region.rootfs_off;

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
        // exactly the partition size (the formatter takes an explicit block count).
        let partition_bytes = (available_bytes / EXT4_BLOCK) * EXT4_BLOCK;
        // The floor belongs to this direction only: it catches a *mis-authored* number
        // before any work happens. See [`around_rootfs`](Self::around_rootfs) for why a
        // measured size is not held to it.
        if partition_bytes < MIN_ROOTFS_BYTES {
            return Err(geom(format!(
                "usable rootfs area ({available_bytes} bytes) is smaller than the {MIN_ROOTFS_BYTES}-byte minimum"
            )));
        }

        Self::assemble(region, total_size, partition_bytes)
    }

    /// Lay the disk out around a filesystem whose size the format already decided.
    ///
    /// The inverse of [`resolve`](Self::resolve): there the authored size fixes the disk
    /// and the rootfs fills what is left, here the fitted filesystem fixes the partition
    /// and the disk is sized to carry it plus the backup GPT. `rootfs_bytes` is what the
    /// format reported, so the partition is exactly the filesystem — there is no slack
    /// between them to floor away.
    ///
    /// [`MIN_ROOTFS_BYTES`] deliberately does **not** apply here. That floor exists to
    /// catch a *mis-authored* number before work happens; a fitted size is a measurement
    /// of a rootfs that provably fits. Honouring it would mean either re-formatting at
    /// the floor — a wasted pass — or laying out a partition larger than the filesystem
    /// written into it, which does not mount.
    ///
    /// # Errors
    ///
    /// [`EngineError::ImageGeometry`] for a boot region that does not resolve, a
    /// filesystem that is not whole ext4 blocks, or a total that overflows the offset
    /// arithmetic.
    pub(crate) fn around_rootfs(
        boot: &ResolvedBoot,
        rootfs_bytes: u64,
    ) -> Result<Geometry, EngineError> {
        let region = BootRegion::resolve(boot)?;
        // The disk is the head, the partition, and the backup GPT behind it — the
        // smallest medium that carries this filesystem at this offset.
        let total_size = region
            .rootfs_off
            .checked_add(rootfs_bytes)
            .and_then(|end| end.checked_add(GPT_BACK_SECTORS * SECTOR))
            .ok_or_else(|| {
                geom(format!(
                    "a rootfs of {rootfs_bytes} bytes at offset {} overflows the offset arithmetic",
                    region.rootfs_off
                ))
            })?;

        Self::assemble(region, total_size, rootfs_bytes)
    }

    /// The one place a [`Geometry`] is built, whichever direction it was resolved from,
    /// and where the invariants both directions owe the writers are checked.
    ///
    /// Each direction reaches this having decided a different half — [`resolve`](Self::resolve)
    /// the disk, [`around_rootfs`](Self::around_rootfs) the filesystem — so neither is in a
    /// position to check the *relationship* between them against the other's arithmetic.
    /// Here both are in hand, so the GPT consistency the writers rely on is stated once
    /// rather than argued twice: the partition is whole ext4 blocks, it is a whole number
    /// of sectors, and it ends at or before the last LBA the backup table leaves usable.
    ///
    /// That last one is the load-bearing check. A filesystem laid past the usable range is
    /// a partition the GPT writer refuses; a partition laid *larger* than its filesystem is
    /// worse, because the table is written happily and the image only fails when a board
    /// tries to mount it.
    ///
    /// # Errors
    ///
    /// [`EngineError::ImageGeometry`] naming whichever invariant does not hold.
    fn assemble(
        region: BootRegion,
        total_size: u64,
        rootfs_bytes: u64,
    ) -> Result<Geometry, EngineError> {
        let rootfs_off = region.rootfs_off;
        if !rootfs_bytes.is_multiple_of(EXT4_BLOCK) {
            return Err(geom(format!(
                "the rootfs filesystem ({rootfs_bytes} bytes) is not a multiple of the ext4 block size {EXT4_BLOCK}"
            )));
        }
        if !total_size.is_multiple_of(SECTOR) {
            return Err(geom(format!(
                "image size ({total_size}) is not a multiple of {SECTOR}"
            )));
        }
        let rootfs_first_lba = rootfs_off / SECTOR;
        let rootfs_length_lba = rootfs_bytes / SECTOR;
        // The rootfs must end at or before the last usable LBA: the backup table owns
        // the final GPT_BACK_SECTORS, and the last LBA a partition may use is one before
        // it. A fitted disk lands exactly on that bound, so there is no slack here to
        // absorb an off-by-one in either direction's arithmetic.
        let end_lba = rootfs_first_lba
            .checked_add(rootfs_length_lba)
            .ok_or_else(|| geom("the rootfs partition overflows the LBA arithmetic".into()))?;
        let usable_end_lba = (total_size / SECTOR)
            .checked_sub(GPT_BACK_SECTORS)
            .filter(|usable| end_lba <= *usable)
            .ok_or_else(|| {
                geom(format!(
                    "the rootfs partition ends at LBA {end_lba}, past what the {total_size}-byte \
                     image leaves usable before the backup GPT"
                ))
            })?;
        debug_assert!(end_lba <= usable_end_lba);

        Ok(Geometry {
            total_size,
            boot: region.boot,
            seed_off: region.seed_off,
            rootfs_off,
            rootfs_first_lba,
            rootfs_length_lba,
            rootfs_bytes,
        })
    }
}

/// The largest byte slack a fit search will look for — 64 GiB.
///
/// The share ceiling is the formatter's own ([`Slack::MAX_SHARE`]); this one is not,
/// because a byte slack is an absolute quantity the search has no opinion about. Past
/// this the caller has effectively named a size, and naming it outright is both faster
/// and what they meant: the search would otherwise place the rootfs into candidate
/// geometries tens of gibibytes wide to arrive at a number the recipe could have stated.
const MAX_SLACK_BYTES: u64 = 64 << 30;

/// Translate an authored fit slack into the formatter's own, refusing one the search
/// will not accept.
///
/// The two types say the same thing in different vocabularies — [`parse_image_size`]
/// validates the *grammar* and stops there, deliberately, because the limits belong to
/// whoever runs the search. This is where they are applied, and it runs at resolution
/// time as well as at build time: a `fit+95%` that the formatter would refuse must fail
/// before a rootfs is built, not after.
///
/// `image_size` is the authored string, carried only so the message names what the
/// recipe wrote rather than a number the reader has to map back.
///
/// # Errors
///
/// [`EngineError::ImageGeometry`] for a share past [`Slack::MAX_SHARE`] or a byte slack
/// past [`MAX_SLACK_BYTES`].
pub(crate) fn fit_slack(image_size: &str, slack: CoreSlack) -> Result<Slack, EngineError> {
    match slack {
        CoreSlack::Bytes(bytes) if bytes > MAX_SLACK_BYTES => Err(geom(format!(
            "image_size {image_size:?} asks for {bytes} bytes free, past the {} GiB the fit \
             search accepts — that much room is a size to name outright",
            MAX_SLACK_BYTES >> 30,
        ))),
        CoreSlack::Bytes(bytes) => Ok(Slack::Bytes(bytes)),
        CoreSlack::Share(hundredths) if hundredths > Slack::MAX_SHARE => Err(geom(format!(
            "image_size {image_size:?} asks for {}.{:02}% free, past the {}% the fit search accepts \
             — a filesystem that far from what its contents need is a size to name outright",
            hundredths / 100,
            hundredths % 100,
            Slack::MAX_SHARE / 100,
        ))),
        CoreSlack::Share(hundredths) => Ok(Slack::Share(hundredths)),
    }
}

/// Build an [`EngineError::ImageGeometry`] with `detail`.
fn geom(detail: String) -> EngineError {
    EngineError::ImageGeometry { detail }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boot2deb_core::model::{
        InitramfsCompress, Kpart, ResolvedDepthchargeBoot, ResolvedRkbinBoot, Rkbin,
    };

    /// The RK1 raw-gap layout (boot-methods/rockchip-rkbin.toml).
    fn rk1_boot() -> ResolvedBoot {
        rk1_boot_with("32KiB", "8MiB", "16MiB")
    }

    fn rk1_boot_with(idbloader: &str, uboot_itb: &str, rootfs: &str) -> ResolvedBoot {
        ResolvedBoot::RockchipRkbin(ResolvedRkbinBoot {
            uboot_defconfig: "turing-rk1-rk3588_defconfig".into(),
            uboot_source: "https://example/u-boot.git".into(),
            uboot_ref: "v2026.04".into(),
            uboot_series: None,
            uboot_patches_url: None,
            uboot_patches_ref: None,
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
            initramfs_compress: InitramfsCompress::Xz,
        })
    }

    #[test]
    fn resolves_the_rk1_2g_layout() {
        let g = Geometry::resolve(&rk1_boot(), 2 << 30).unwrap();
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
        let g = Geometry::resolve(&c201_boot(), 4 << 30).unwrap();
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
        let g = Geometry::resolve(&c201_boot_with("12MiB", "16MiB", 1, "28MiB"), 4 << 30).unwrap();
        let BootGeometry::Kpart { ref slots } = g.boot else {
            panic!("expected kernel slots")
        };
        assert_eq!(slots.len(), 1);
        assert_eq!(slots[0].flags, 0x015A_0000_0000_0000);
    }

    /// The seed partition's placement, both shapes: the MiB directly below the
    /// rootfs on a raw-gap layout, the MiB directly below the kernel slots on a
    /// depthcharge one — and the refusals when the space is not there.
    #[test]
    fn the_seed_partition_takes_the_free_mib_each_method_leaves() {
        let g = Geometry::resolve(&rk1_boot(), 2 << 30).unwrap();
        assert_eq!(g.seed_off, 15 * 1024 * 1024, "rootfs 16 MiB − 1 MiB");

        let g = Geometry::resolve(&c201_boot(), 4 << 30).unwrap();
        assert_eq!(g.seed_off, 11 * 1024 * 1024, "kpart 12 MiB − 1 MiB");
        // The slots themselves still abut the rootfs exactly — the seed took
        // nothing from them.
        assert_eq!(g.rootfs_off, 44 * 1024 * 1024);

        // A u-boot.itb slot the seed would swallow whole is refused: rootfs at
        // 8.5 MiB puts the seed at 7.5 MiB, below the itb at 8 MiB. (8.5 MiB is
        // 4 KiB-aligned, so the alignment gate does not mask this one.)
        assert!(Geometry::resolve(&rk1_boot_with("32KiB", "8MiB", "8912896"), 2 << 30).is_err());
        // Kernel slots at 1 MiB leave the seed nowhere above the primary GPT.
        assert!(Geometry::resolve(&c201_boot_with("1MiB", "16MiB", 2, "36MiB"), 4 << 30).is_err());

        // The itb budget ends at the seed, not at the rootfs: a payload that fits the
        // raw gap but spills into the seed's MiB is refused.
        let region = BootRegion::resolve(&rk1_boot()).unwrap();
        assert!(region
            .check_payload_fit(&[("idbloader.img", 1024), ("u-boot.itb", 7 << 20)])
            .is_ok());
        assert!(region
            .check_payload_fit(&[("idbloader.img", 1024), ("u-boot.itb", (7 << 20) + 1)])
            .is_err());
    }

    #[test]
    fn rejects_bad_ordering_and_alignment() {
        // rootfs before u-boot.itb.
        assert!(Geometry::resolve(&rk1_boot_with("32KiB", "16MiB", "8MiB"), 2 << 30).is_err());
        // idbloader inside the primary GPT reservation.
        assert!(Geometry::resolve(&rk1_boot_with("512", "8MiB", "16MiB"), 2 << 30).is_err());
        // rootfs offset 512-aligned (16385 sectors) but not 4 KiB-aligned.
        assert!(Geometry::resolve(&rk1_boot_with("32KiB", "8MiB", "8389120"), 2 << 30).is_err());
    }

    #[test]
    fn rejects_kernel_slots_that_collide() {
        // Overlapping the primary GPT: the firmware's own table would be destroyed.
        assert!(Geometry::resolve(&c201_boot_with("512", "16MiB", 2, "44MiB"), 4 << 30).is_err());
        // The *second* slot runs into the rootfs: 12 + 2*16 = 44 MiB, past a rootfs at
        // 40 MiB. This is the collision a one-slot geometry cannot have, and the reason
        // the rootfs offset is checked against the last slot rather than the first — a
        // spare silently overlapping the rootfs would be a kernel upgrade that eats the
        // filesystem.
        assert!(Geometry::resolve(&c201_boot_with("12MiB", "16MiB", 2, "40MiB"), 4 << 30).is_err());
        // A zero-size kernel slot could hold no kernel.
        assert!(Geometry::resolve(&c201_boot_with("12MiB", "0", 2, "44MiB"), 4 << 30).is_err());
        // A gap between the last slot and the rootfs is allowed — only an overlap is not.
        assert!(Geometry::resolve(&c201_boot_with("12MiB", "8MiB", 2, "44MiB"), 4 << 30).is_ok());
    }

    /// A fitted image inverts the layout: the filesystem is decided first and the disk
    /// is sized around it. The properties that must hold are that the head of the disk
    /// is untouched by the inversion, that the partition is exactly the filesystem the
    /// format reported, and that the backup GPT still fits behind it.
    #[test]
    fn a_fitted_rootfs_sizes_the_disk_around_itself() {
        let rootfs_bytes = 700 * 1024 * 1024;
        let g = Geometry::around_rootfs(&rk1_boot(), rootfs_bytes).unwrap();

        // The head of the disk does not answer to the size, so it is what the authored
        // layout says regardless of which direction the geometry was resolved in.
        let fixed = Geometry::resolve(&rk1_boot(), 2 << 30).unwrap();
        assert_eq!(g.boot, fixed.boot);
        assert_eq!(g.rootfs_off, fixed.rootfs_off);
        assert_eq!(g.rootfs_first_lba, fixed.rootfs_first_lba);

        // The partition is the filesystem exactly — under a fit there is no slack
        // between them to floor away.
        assert_eq!(g.rootfs_bytes, rootfs_bytes);
        assert_eq!(g.rootfs_bytes, g.rootfs_length_lba * SECTOR);

        // And the disk carries the head, the partition, and the backup table.
        assert_eq!(
            g.total_size,
            g.rootfs_off + rootfs_bytes + GPT_BACK_SECTORS * SECTOR
        );
        let end_lba = g.rootfs_first_lba + g.rootfs_length_lba;
        assert!(end_lba <= g.total_size / SECTOR - GPT_BACK_SECTORS);

        // A filesystem that is not whole ext4 blocks is refused rather than laid out
        // into a partition that would describe it wrongly.
        assert!(Geometry::around_rootfs(&rk1_boot(), rootfs_bytes + 1).is_err());

        // But the authored-size floor does *not* apply to a fitted one. A format that
        // returned a small filesystem returned a working one, and rejecting it here
        // would fail a build after the whole rootfs had been written — which is exactly
        // what an 8 MiB fitted rootfs did before this was separated out.
        let small = Geometry::around_rootfs(&rk1_boot(), 8 * 1024 * 1024)
            .expect("a fitted size is reported by a completed format, not authored");
        assert!(small.rootfs_bytes < MIN_ROOTFS_BYTES);
        assert_eq!(
            small.total_size,
            small.rootfs_off + small.rootfs_bytes + GPT_BACK_SECTORS * SECTOR
        );
    }

    /// The exact disk a hardware-gated fitted build came out to, pinned as arithmetic.
    ///
    /// `asus-c201-libreboot/mainline-forky --image-size fit+20%` passed the build gate on
    /// 2026-08-03 with these numbers measured off the artifact: 32 MiB kernel slots at 12
    /// and 44 MiB, the rootfs at 76 MiB, a filesystem of 837,824,512 bytes, and a whole
    /// image of 917,533,184. This asserts the layout still derives that disk from that
    /// filesystem, which is the half of the gate that does not need a board — so a change
    /// to the offset arithmetic fails here rather than on the next flash.
    #[test]
    fn the_gated_c201_libreboot_fit_lays_out_the_disk_it_was_measured_at() {
        const ROOTFS_BYTES: u64 = 837_824_512;
        const IMAGE_BYTES: u64 = 917_533_184;

        let boot = c201_boot_with("12MiB", "32MiB", 2, "76MiB");
        let g = Geometry::around_rootfs(&boot, ROOTFS_BYTES).unwrap();

        assert_eq!(g.rootfs_off, 76 * 1024 * 1024, "the rootfs sits at 76 MiB");
        assert_eq!(
            g.total_size, IMAGE_BYTES,
            "76 MiB + {ROOTFS_BYTES} + 33 sectors is the disk the gate measured"
        );
        // The filesystem fills its partition to the byte — the property the gate checked
        // as "ext4 = its GPT partition".
        assert_eq!(g.rootfs_bytes, ROOTFS_BYTES);
        assert_eq!(g.rootfs_length_lba * SECTOR, ROOTFS_BYTES);
        // And the slots the pairing resolved to, which is what puts the rootfs at 76 MiB
        // rather than the stock 44.
        let BootGeometry::Kpart { ref slots } = g.boot else {
            panic!("expected kernel slots")
        };
        assert_eq!(slots.len(), 2);
        assert_eq!(slots[0].offset, 12 * 1024 * 1024);
        assert_eq!(slots[1].offset, 44 * 1024 * 1024);
    }

    /// `parse_image_size` validates the grammar and stops; the limit is the search's.
    /// A share the formatter would refuse has to fail at resolution time, because the
    /// alternative is discovering it after the whole rootfs has been built.
    #[test]
    fn a_fit_slack_past_what_the_search_accepts_is_refused() {
        // Inside the limit, both spellings pass straight through.
        assert_eq!(
            fit_slack("fit+20%", CoreSlack::Share(2000)).unwrap(),
            Slack::Share(2000)
        );
        assert_eq!(
            fit_slack("fit+512M", CoreSlack::Bytes(512 << 20)).unwrap(),
            Slack::Bytes(512 << 20)
        );
        // Each boundary itself is accepted; one step past it is not.
        assert!(fit_slack("fit+90%", CoreSlack::Share(Slack::MAX_SHARE)).is_ok());
        assert!(fit_slack("fit+64G", CoreSlack::Bytes(MAX_SLACK_BYTES)).is_ok());

        let err = fit_slack("fit+95%", CoreSlack::Share(9500)).unwrap_err();
        let EngineError::ImageGeometry { detail } = err else {
            panic!("expected an image-geometry error");
        };
        // The message names what the recipe wrote and what the ceiling is, so the fix
        // does not require mapping a number back to the authored string.
        assert!(detail.contains("fit+95%"), "{detail}");
        assert!(detail.contains("90%"), "{detail}");

        // The byte form has a ceiling of its own. It is boot2deb's, not the formatter's
        // — ferrosys refuses an oversized *share* and has no opinion about bytes — so
        // without this a `fit+1T` would search geometries a terabyte wide before
        // failing, long after the rootfs had been built.
        let err = fit_slack("fit+1T", CoreSlack::Bytes(1 << 40)).unwrap_err();
        let EngineError::ImageGeometry { detail } = err else {
            panic!("expected an image-geometry error");
        };
        assert!(detail.contains("fit+1T"), "{detail}");
        assert!(detail.contains("64 GiB"), "{detail}");
    }

    #[test]
    fn rejects_image_too_small_for_the_rootfs() {
        // 8 MiB total cannot hold a rootfs starting at 16 MiB.
        assert!(Geometry::resolve(&rk1_boot(), 8 << 20).is_err());
    }

    #[test]
    fn rejects_rootfs_area_below_the_minimum() {
        // The rootfs clears the 16 MiB gap, but the ~84 MiB left is under the
        // 128 MiB minimum a Debian rootfs needs.
        assert!(Geometry::resolve(&rk1_boot(), 100 << 20).is_err());
    }

    #[test]
    fn payload_fit_catches_overruns() {
        let g = BootRegion::resolve(&rk1_boot()).unwrap();
        let gap = |idb: u64, itb: u64| {
            g.check_payload_fit(&[("idbloader.img", idb), ("u-boot.itb", itb)])
        };
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
        let g = BootRegion::resolve(&c201_boot()).unwrap();
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
