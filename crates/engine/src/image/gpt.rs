//! GPT partition table for the whole-disk image: a protective MBR plus the
//! partitions the boot method's geometry fixed. Pure Rust via the `gpt` crate — no
//! `sgdisk`/`parted`/`cgpt` shell-out.
//!
//! Two shapes, one writer. A `rockchip-rkbin` image has a single Linux-filesystem
//! partition, and its bootloader lives in a raw gap *outside* the table. Under the
//! `efi` loader an **EFI system partition** precedes the rootfs. A `depthcharge`
//! image adds a **ChromeOS kernel partition** ahead of it instead, which is the
//! entire boot mechanism on that board. The firmware scans every medium's GPT
//! for that type GUID, and picks among the candidates by the entry's attribute
//! bits.
//! Both are ordinary GPT entries, so the type GUID (`partition_types::CHROME_KERNEL`)
//! and the raw 64-bit `flags` field contain it, and no ChromeOS host tooling is needed.
//!
//! The `gpt` crate writes only the GPT structures (primary header + entry array
//! at the front, backup at the end), so the protective MBR at LBA 0 is written
//! separately. The image file must already exist at its full size before this runs.
//! The crate opens it without creating it, and lays the backup table relative to
//! the file's length.

use crate::error::EngineError;
use crate::image::esp::ESP_PARTLABEL;
use crate::image::geometry::{BootGeometry, Geometry, SECTOR};
use crate::image::ImageIdentity;
use boot2deb_core::chromeos::MAX_KPART_SLOTS;
use boot2deb_core::press::{SEED_PARTITION_BYTES, SEED_PARTLABEL};
use gpt::disk::LogicalBlockSize;
use gpt::mbr::ProtectiveMBR;
use gpt::{partition_types, GptConfig};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use uuid::Uuid;

/// GPT entry labels for the kernel slots, in on-disk order. ChromeOS names its kernel
/// partitions `KERN-A`, `KERN-B`, ... and every tool and guide on these boards speaks
/// that language, so the table says so too.
///
/// The length of this array *is*
/// [`MAX_KPART_SLOTS`](boot2deb_core::chromeos::MAX_KPART_SLOTS) — resolution rejects a
/// slot count above it, so indexing here cannot run off the end.
const KPART_NAMES: [&str; MAX_KPART_SLOTS as usize] = ["KERN-A", "KERN-B", "KERN-C", "KERN-D"];

/// GPT attribute bit 2, "legacy BIOS bootable" — the flag a scanning bootloader
/// reads to decide which partitions are boot candidates.
///
/// Required under `rockchip-rkbin`. U-Boot's `bootflow scan`, which is
/// `bootcmd` on every board this builds, considers **only** partitions having
/// this bit, and when no partition on the medium has it falls back to
/// scanning partition 1 alone. Partition 1 is the seed, so an unmarked table is
/// a board that reaches its prompt and finds no bootflow.
///
/// Set on the one partition holding what the scan must find. Under the `efi` loader
/// that is the EFI system partition, where u-boot's EFI boot method finds the loader
/// at the removable-media path. Otherwise it is the rootfs, which holds
/// `/boot/extlinux/extlinux.conf`. The rootfs keeps it through the first-boot grow
/// (`sfdisk -N` leaves fields the descriptor does not name alone), and depthcharge
/// firmware ignores it: there the selection is the type GUID plus the ChromeOS
/// attribute bits on the kernel slots.
const LEGACY_BIOS_BOOTABLE: u64 = 1 << 2;

/// One partition to write: everything the GPT entry needs.
struct PartitionSpec<'a> {
    /// Entry index (1-based), and the order the partitions appear in the table.
    index: u32,
    /// Partition name (GPT entry label).
    name: &'a str,
    /// First LBA.
    first_lba: u64,
    /// Length in sectors.
    length_lba: u64,
    /// Type GUID.
    part_type: partition_types::Type,
    /// The 64-bit attribute word: [`LEGACY_BIOS_BOOTABLE`] on the partition a
    /// scanning bootloader must open, the boot selection itself on a ChromeOS kernel
    /// partition, zero on the seed.
    flags: u64,
    /// The deterministic partition GUID (a rootfs partition's is its PARTUUID).
    guid: Uuid,
}

/// Write a protective MBR and the image's partitions into the existing, full-size
/// `image` file. `rootfs_label` names the rootfs GPT entry.
///
/// The rootfs partition spans `[rootfs_first_lba, rootfs_first_lba +
/// rootfs_length_lba)` — the exact range the ext4 filesystem is spliced into —
/// typed `LINUX_FS`, having the identity's rootfs PARTUUID. Under the `efi` loader an
/// EFI system partition precedes it, typed as one and having the
/// [`LEGACY_BIOS_BOOTABLE`] attribute that points a scanning bootloader at it; otherwise
/// the rootfs has that attribute. Under `depthcharge` a ChromeOS kernel partition
/// precedes the rootfs, having the attribute bits the firmware selects on; under
/// `rockchip-rkbin` the bootloader payloads live in the raw gap ahead of the table and
/// are not GPT entries at all.
///
/// Every GUID comes from `identity`, which the caller derived from the lock: the `gpt`
/// crate otherwise draws them from `/dev/urandom` (the disk GUID at header build, each
/// partition GUID inside `add_partition_at`), which would make the GPT region differ on
/// every rebuild. Setting them makes the whole table a function of the lock — and, on
/// depthcharge, makes the rootfs PARTUUID knowable *before* the rootfs is built, which
/// is what lets its `/etc/fstab` name the partition the signed kernel will root on.
pub(crate) fn write_table(
    image: &Path,
    geom: &Geometry,
    rootfs_label: &str,
    identity: &ImageIdentity,
) -> Result<(), EngineError> {
    let disk_guid = identity.disk_guid;
    let mut parts = Vec::with_capacity(usize::from(MAX_KPART_SLOTS) + 3);
    // The seed sits below the kernel slots on depthcharge and below the rootfs
    // on rkbin (see the geometry), so pushing it here keeps the list in on-disk
    // order for the former; the sort below handles the latter.
    parts.push(PartitionSpec {
        index: 0, // re-numbered after the sort
        name: SEED_PARTLABEL,
        first_lba: geom.seed_off / SECTOR,
        length_lba: SEED_PARTITION_BYTES / SECTOR,
        // Basic data, not Linux-filesystem: the seed is the partition an operator
        // edits from any laptop, and this is the type GUID that desktop OSes
        // auto-mount a FAT volume under. The device side finds it by its label.
        part_type: partition_types::BASIC,
        flags: 0,
        guid: identity.seed_partuuid,
    });
    if let BootGeometry::Kpart { ref slots } = geom.boot {
        for (i, slot) in slots.iter().enumerate() {
            parts.push(PartitionSpec {
                index: 0,
                // The conventional ChromeOS names. The firmware selects by type GUID
                // and attributes, never by name, so this is for whoever reads the
                // table — but it is the name every ChromeOS tool and guide uses for
                // these, so it is worth getting right.
                name: KPART_NAMES[i],
                first_lba: slot.first_lba,
                length_lba: slot.length_lba,
                part_type: partition_types::CHROME_KERNEL,
                flags: slot.flags,
                guid: identity.kpart_guids[i],
            });
        }
    }
    if let Some(esp) = geom.esp {
        parts.push(PartitionSpec {
            index: 0,
            name: ESP_PARTLABEL,
            first_lba: esp.first_lba(),
            length_lba: esp.length_lba(),
            part_type: partition_types::EFI,
            // The partition u-boot's EFI boot method opens — see the constant.
            flags: LEGACY_BIOS_BOOTABLE,
            guid: identity.esp_partuuid,
        });
    }
    parts.push(PartitionSpec {
        index: 0,
        name: rootfs_label,
        first_lba: geom.rootfs_first_lba,
        length_lba: geom.rootfs_length_lba,
        part_type: partition_types::LINUX_FS,
        // The partition a scanning bootloader must look inside, unless an ESP is the
        // one it opens — see the constant.
        flags: if geom.esp.is_some() {
            0
        } else {
            LEGACY_BIOS_BOOTABLE
        },
        guid: identity.rootfs_partuuid,
    });
    // Table order = disk order, whatever the boot method: an operator reading
    // `lsblk` sees partitions in the order they sit on the medium, and the
    // first-boot resize grows "the partition `/` is on", not "partition N".
    parts.sort_by_key(|p| p.first_lba);
    for (i, p) in parts.iter_mut().enumerate() {
        p.index = i as u32 + 1;
    }

    let cfg = GptConfig::new()
        .writable(true)
        .logical_block_size(LogicalBlockSize::Lb512);
    // `create` would pass `None` and mint a random disk GUID; open the file
    // ourselves so `create_from_device` takes the deterministic one.
    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(image)
        .map_err(|s| EngineError::io(image, s))?;
    let mut disk = cfg
        .create_from_device(file, Some(disk_guid))
        .map_err(|e| gpt_err("open image for GPT", e))?;
    for p in &parts {
        disk.add_partition_at(
            p.name,
            p.index,
            p.first_lba,
            p.length_lba,
            p.part_type.clone(),
            p.flags,
        )
        .map_err(|e| gpt_err(&format!("add {} partition", p.name), e))?;
    }
    // `add_partition_at` minted a random GUID for each; overwrite them with the
    // deterministic ones before the table is serialized.
    let mut partitions = disk.partitions().clone();
    for p in &parts {
        if let Some(entry) = partitions.get_mut(&p.index) {
            entry.part_guid = p.guid;
        }
    }
    disk.update_partitions(partitions)
        .map_err(|e| gpt_err("set deterministic partition GUIDs", e))?;
    // Writes the primary + backup GPT headers and entry arrays; returns the
    // underlying file so the protective MBR goes onto the same handle.
    let mut file = disk.write().map_err(|e| gpt_err("write GPT headers", e))?;

    // Protective MBR: one 0xEE record covering the disk (its size field caps at
    // u32::MAX LBAs for disks larger than 2 TiB).
    let total_lba = geom.total_size / SECTOR;
    let pmbr_size = total_lba.saturating_sub(1).min(u64::from(u32::MAX)) as u32;
    ProtectiveMBR::with_lb_size(pmbr_size)
        .overwrite_lba0(&mut file)
        .map_err(|e| gpt_err("write protective MBR", e))?;
    file.flush().map_err(|s| EngineError::io(image, s))?;
    Ok(())
}

/// Map a `gpt`-crate error (GPT or MBR) into a typed [`EngineError::Gpt`].
fn gpt_err(context: &str, e: impl std::fmt::Display) -> EngineError {
    EngineError::Gpt {
        context: context.to_string(),
        detail: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boot2deb_core::model::{
        InitramfsCompress, Kpart, Offsets, ResolvedBoot, ResolvedDepthchargeBoot,
        ResolvedRkbinBoot, Rkbin,
    };

    fn sized_image(dir: &Path, name: &str, size: u64) -> std::path::PathBuf {
        let p = dir.join(name);
        let f = std::fs::OpenOptions::new()
            .write(true)
            .read(true)
            .create(true)
            .truncate(true)
            .open(&p)
            .unwrap();
        f.set_len(size).unwrap();
        p
    }

    fn rkbin_boot() -> ResolvedBoot {
        ResolvedBoot::RockchipRkbin(Box::new(ResolvedRkbinBoot {
            uboot_defconfig: "d".into(),
            uboot_source: "s".into(),
            uboot_ref: "v1".into(),
            uboot_series: None,
            uboot_patches_url: None,
            uboot_patches_ref: None,
            bl31: boot2deb_core::model::Bl31::Rkbin { file: "atf".into() },
            rkbin: Rkbin {
                tpl: "tpl".into(),
                bl32: None,
                upstream: boot2deb_core::RkbinUpstream {
                    git: "https://example.invalid/rkbin.git".into(),
                    git_ref: "master".into(),
                    dir: "bin".into(),
                },
            },
            offsets: Offsets {
                idbloader: "32KiB".into(),
                uboot_itb: "8MiB".into(),
                partitions: "16MiB".into(),
            },
        }))
    }

    fn depthcharge_boot() -> ResolvedBoot {
        ResolvedBoot::Depthcharge(ResolvedDepthchargeBoot {
            board: "speedy".into(),
            kpart: Kpart {
                offset: "12MiB".into(),
                size: "16MiB".into(),
                slots: 2,
                priority: 10,
                tries: 5,
                successful: true,
                flags: 0x015A_0000_0000_0000,
            },
            cmdline: "console=tty1 rootwait ro".into(),
            rootfs_offset: "44MiB".into(),
            initramfs_compress: InitramfsCompress::Xz,
        })
    }

    const DISK_GUID: Uuid = Uuid::from_bytes([0xa1; 16]);
    const ROOTFS_GUID: Uuid = Uuid::from_bytes([0xb2; 16]);
    const SEED_GUID: Uuid = Uuid::from_bytes([0x5e; 16]);
    const ESP_GUID: Uuid = Uuid::from_bytes([0xe5; 16]);
    const KPART_GUIDS: [Uuid; MAX_KPART_SLOTS as usize] = [
        Uuid::from_bytes([0xc3; 16]),
        Uuid::from_bytes([0xc4; 16]),
        Uuid::from_bytes([0xc5; 16]),
        Uuid::from_bytes([0xc6; 16]),
    ];

    /// The fixed identifiers every table below is written with.
    fn identity() -> ImageIdentity {
        ImageIdentity {
            ext4_uuid: Uuid::from_bytes([0xe4; 16]),
            disk_guid: DISK_GUID,
            rootfs_partuuid: ROOTFS_GUID,
            seed_partuuid: SEED_GUID,
            esp_partuuid: ESP_GUID,
            kpart_guids: KPART_GUIDS,
        }
    }

    /// With the disk + partition GUIDs fixed, the whole GPT-bearing image is
    /// byte-identical across independent writes — the last random inputs in the
    /// table are gone. Also asserts the GUIDs actually land in the header /
    /// entry (a table that ignored them would still be self-consistent).
    #[test]
    fn gpt_bytes_are_reproducible_with_fixed_guids() {
        let tmp = tempfile::tempdir().unwrap();
        let size = 192 * 1024 * 1024;
        let geom = Geometry::resolve(&rkbin_boot(), None, 192 << 20).unwrap();

        let a = sized_image(tmp.path(), "a.img", size);
        let b = sized_image(tmp.path(), "b.img", size);
        write_table(&a, &geom, "rootfs", &identity()).unwrap();
        write_table(&b, &geom, "rootfs", &identity()).unwrap();

        let ba = std::fs::read(&a).unwrap();
        assert_eq!(
            ba,
            std::fs::read(&b).unwrap(),
            "GPT image must reproduce byte-for-byte"
        );

        // Both GUIDs appear in the table (GPT stores GUIDs mixed-endian, so match
        // the raw fields via the crate's own writer round-trip rather than raw bytes).
        let disk = GptConfig::new()
            .writable(false)
            .logical_block_size(LogicalBlockSize::Lb512)
            .open(&a)
            .unwrap();
        assert_eq!(
            disk.guid(),
            &DISK_GUID,
            "header contains the derived disk GUID"
        );
        let parts = disk.partitions();
        assert_eq!(
            parts.len(),
            2,
            "seed + rootfs — a raw-gap bootloader is not a GPT entry"
        );
        // The seed sits in the MiB directly below the rootfs (16 MiB here), so
        // in disk order it is entry 1 and the rootfs entry 2.
        let seed = parts.get(&1).unwrap();
        assert_eq!(seed.name, "b2d-seed");
        assert_eq!(seed.first_lba, 15 * 1024 * 1024 / 512);
        assert_eq!(seed.part_guid, SEED_GUID);
        assert_eq!(
            parts.get(&2).unwrap().part_guid,
            ROOTFS_GUID,
            "partition has the derived GUID"
        );
    }

    /// The rootfs entry has the legacy-BIOS-bootable attribute and the seed does
    /// not — the difference between a board that boots and one that stops at its
    /// prompt.
    ///
    /// U-Boot's `bootflow scan` narrows to the partitions marked bootable as soon as
    /// any partition is, and scans partition 1 alone when none is. The seed is
    /// partition 1 on every image, so an unmarked rootfs is never opened and no
    /// bootflow is ever found. Asserted on both boot shapes because the seed precedes
    /// the rootfs under each of them.
    #[test]
    fn the_rootfs_is_the_bootable_partition_and_the_seed_is_not() {
        let tmp = tempfile::tempdir().unwrap();
        for (name, boot) in [("rkbin", rkbin_boot()), ("depthcharge", depthcharge_boot())] {
            let geom = Geometry::resolve(&boot, None, 192 << 20).unwrap();
            let img = sized_image(
                tmp.path(),
                &format!("{name}-boot-flag.img"),
                geom.total_size,
            );
            write_table(&img, &geom, "rootfs", &identity()).unwrap();

            let disk = GptConfig::new()
                .writable(false)
                .logical_block_size(LogicalBlockSize::Lb512)
                .open(&img)
                .unwrap();
            let parts = disk.partitions().clone();
            let seed = parts
                .values()
                .find(|p| p.name == SEED_PARTLABEL)
                .unwrap_or_else(|| panic!("{name}: no seed partition"));
            let rootfs = parts
                .values()
                .find(|p| p.name == "rootfs")
                .unwrap_or_else(|| panic!("{name}: no rootfs partition"));
            assert_eq!(
                rootfs.flags & LEGACY_BIOS_BOOTABLE,
                LEGACY_BIOS_BOOTABLE,
                "{name}: the rootfs must be the partition a bootloader scans"
            );
            assert_eq!(
                seed.flags, 0,
                "{name}: the seed holds personalization, never a bootflow"
            );
            assert!(
                seed.first_lba < rootfs.first_lba,
                "{name}: the seed precedes the rootfs, which is why the flag is needed"
            );
        }
    }

    /// A fitted image sizes the disk to the *exact* minimum that contains its rootfs plus
    /// the backup table, so its last partition LBA is the last usable LBA and there is no
    /// spare sector anywhere to absorb an off-by-one.
    ///
    /// [`Geometry::resolve`] cannot catch that: it starts from a disk size and floors the
    /// partition into what is left, so it always ends at or below the limit however the
    /// reservation is counted. Only the inverse direction puts the arithmetic against the
    /// real GPT writer, which is the one that decides whether the range is legal.
    ///
    /// Both boot shapes, because they place the rootfs at different offsets and the
    /// depthcharge one also has to fit its kernel entries into the same table.
    #[test]
    fn a_fitted_geometry_ends_exactly_on_the_last_usable_lba() {
        let tmp = tempfile::tempdir().unwrap();
        for (name, boot) in [("rkbin", rkbin_boot()), ("depthcharge", depthcharge_boot())] {
            let rootfs_bytes = 256 * 1024 * 1024;
            let geom = Geometry::around_rootfs(&boot, None, rootfs_bytes).unwrap();
            let img = sized_image(tmp.path(), &format!("{name}-fit.img"), geom.total_size);
            // The writer refuses a partition past the usable range, so reaching here at
            // all is the assertion: the disk is one sector-perfect fit.
            write_table(&img, &geom, "rootfs", &identity()).unwrap_or_else(|e| {
                panic!("{name}: fitted geometry rejected by the GPT writer: {e}")
            });

            let disk = GptConfig::new()
                .writable(false)
                .logical_block_size(LogicalBlockSize::Lb512)
                .open(&img)
                .unwrap();
            let rootfs = disk.partitions().values().last().unwrap().clone();
            assert_eq!(
                rootfs.first_lba, geom.rootfs_first_lba,
                "{name}: rootfs starts where the geometry said"
            );
            assert_eq!(
                (rootfs.last_lba - rootfs.first_lba + 1) * SECTOR,
                rootfs_bytes,
                "{name}: the partition is exactly the filesystem, with nothing floored away"
            );
            // The crate's own header is the independent statement of where the usable
            // range ends; the fitted partition must reach it and not pass it.
            let header = disk.primary_header().expect("a primary header was written");
            assert_eq!(
                rootfs.last_lba, header.last_usable,
                "{name}: a fitted disk wastes no sector — the rootfs ends on the last usable LBA"
            );
            assert!(
                rootfs.first_lba >= header.first_usable,
                "{name}: the rootfs must clear the primary table ({} < {})",
                rootfs.first_lba,
                header.first_usable
            );
        }
    }

    /// The depthcharge table is the whole boot mechanism, so every field the firmware
    /// reads is asserted: the ChromeOS kernel type GUID, the exact LBA range, and the
    /// attribute word that says "boot this". A wrong value here is a board that
    /// silently refuses the image.
    ///
    /// The **spare** slot is asserted just as closely, because it is what makes a
    /// kernel upgrade recoverable and each of its fields is required for a different
    /// reason: the type GUID is how `depthchargectl` finds it as an upgrade target
    /// at all, and priority 0 is what stops the firmware booting a partition that so
    /// far contains nothing but zeroes.
    #[test]
    fn a_depthcharge_table_contains_a_bootable_kernel_slot_and_an_unbootable_spare() {
        let tmp = tempfile::tempdir().unwrap();
        let size = 512 * 1024 * 1024;
        let geom = Geometry::resolve(&depthcharge_boot(), None, 512 << 20).unwrap();
        let img = sized_image(tmp.path(), "c201.img", size);
        write_table(&img, &geom, "rootfs", &identity()).unwrap();

        let disk = GptConfig::new()
            .writable(false)
            .logical_block_size(LogicalBlockSize::Lb512)
            .open(&img)
            .unwrap();
        let parts = disk.partitions();
        assert_eq!(parts.len(), 4, "seed + KERN-A + KERN-B + rootfs");

        // The seed sits below the kernel slots (11 MiB, directly under the 12 MiB
        // slot offset), so in a disk-ordered table it is entry 1 and the slots
        // follow. The firmware selects by type GUID, never by index, so the shift
        // costs nothing on the boot path.
        let seed = parts.get(&1).unwrap();
        assert_eq!(seed.name, "b2d-seed");
        assert_eq!(seed.first_lba, 11 * 1024 * 1024 / 512);
        assert_eq!(seed.last_lba - seed.first_lba + 1, 2048, "1 MiB");
        assert_eq!(seed.part_type_guid, partition_types::BASIC);
        assert_eq!(seed.part_guid, SEED_GUID);
        assert_eq!(seed.flags, 0);

        let cros_kernel = Uuid::parse_str("FE3A2A5D-4F32-41A7-B725-ACCC3285A309").unwrap();

        let kern_a = parts.get(&2).unwrap();
        assert_eq!(
            kern_a.part_type_guid.guid, cros_kernel,
            "the firmware finds the kernel by this type GUID and nothing else"
        );
        assert_eq!(kern_a.name, "KERN-A");
        assert_eq!(kern_a.first_lba, 24_576);
        assert_eq!(kern_a.last_lba - kern_a.first_lba + 1, 32_768);
        assert_eq!(
            kern_a.flags, 0x015A_0000_0000_0000,
            "priority=10 tries=5 successful=1 — the bits that make it boot"
        );
        assert_eq!(kern_a.part_guid, KPART_GUIDS[0]);

        let kern_b = parts.get(&3).unwrap();
        assert_eq!(
            kern_b.part_type_guid.guid, cros_kernel,
            "the spare has the kernel type GUID too — that is how depthchargectl \
             discovers it as an upgrade target, since it selects by type, not attributes"
        );
        assert_eq!(kern_b.name, "KERN-B");
        assert_eq!(
            kern_b.first_lba, 57_344,
            "the spare abuts KERN-A, so a payload sized for one slot cannot overrun into \
             the other"
        );
        assert_eq!(kern_b.last_lba - kern_b.first_lba + 1, 32_768);
        assert_eq!(
            kern_b.flags, 0,
            "priority 0 — the spare ships empty, and the firmware must never boot it \
             until an upgrade has put a kernel in it"
        );
        assert_ne!(
            kern_b.part_guid, kern_a.part_guid,
            "the slots must be distinguishable: depthcharge substitutes the booted slot's \
             PARTUUID into kern_guid=, which is how the running system knows which slot it \
             came up from and therefore which one it may overwrite"
        );
        assert_eq!(kern_b.part_guid, KPART_GUIDS[1]);

        let root = parts.get(&4).unwrap();
        assert_eq!(root.part_type_guid, partition_types::LINUX_FS);
        assert_eq!(root.first_lba, 90_112, "the rootfs sits behind both slots");
        assert_eq!(
            root.flags, LEGACY_BIOS_BOOTABLE,
            "the rootfs is marked bootable and has nothing else — none of the \
             selection bits, which live on the kernel slots the firmware picks among"
        );
        assert_eq!(
            root.part_guid, ROOTFS_GUID,
            "the PARTUUID the signed kernel cmdline roots on"
        );
    }

    /// Under the efi loader the ESP sits between the seed and the rootfs, typed as an
    /// EFI system partition and having the derived GUID, and it is the one partition
    /// marked bootable: u-boot's scan then opens it and finds the loader, and never
    /// opens the rootfs, which holds no extlinux menu.
    #[test]
    fn under_the_efi_loader_the_esp_is_the_bootable_partition() {
        let tmp = tempfile::tempdir().unwrap();
        let geom = Geometry::resolve(&rkbin_boot(), Some(64 << 20), 512 << 20).unwrap();
        let img = sized_image(tmp.path(), "efi.img", geom.total_size);
        write_table(&img, &geom, "rootfs", &identity()).unwrap();

        let disk = GptConfig::new()
            .writable(false)
            .logical_block_size(LogicalBlockSize::Lb512)
            .open(&img)
            .unwrap();
        let parts = disk.partitions();
        assert_eq!(parts.len(), 3, "seed + esp + rootfs");
        assert_eq!(parts.get(&1).unwrap().name, SEED_PARTLABEL);
        let esp = parts.get(&2).unwrap();
        assert_eq!(esp.name, ESP_PARTLABEL);
        assert_eq!(esp.part_type_guid, partition_types::EFI);
        assert_eq!(esp.first_lba, 16 * 1024 * 1024 / 512);
        assert_eq!(esp.last_lba - esp.first_lba + 1, 64 * 1024 * 1024 / 512);
        assert_eq!(esp.part_guid, ESP_GUID);
        assert_eq!(esp.flags, LEGACY_BIOS_BOOTABLE);
        let rootfs = parts.get(&3).unwrap();
        assert_eq!(
            rootfs.first_lba,
            80 * 1024 * 1024 / 512,
            "the rootfs follows the ESP"
        );
        assert_eq!(rootfs.flags, 0, "the ESP is the partition u-boot opens");
        assert_eq!(rootfs.part_guid, ROOTFS_GUID);
    }

    /// The rootfs's entry number the boot region reports is the one the table gives it,
    /// in every layout: GRUB's core image finds the rootfs by that number.
    #[test]
    fn the_rootfs_index_the_region_reports_is_the_one_the_table_writes() {
        let tmp = tempfile::tempdir().unwrap();
        for (name, boot, esp) in [
            ("rkbin", rkbin_boot(), None),
            ("rkbin-efi", rkbin_boot(), Some(64 << 20)),
            ("depthcharge", depthcharge_boot(), None),
        ] {
            let geom = Geometry::resolve(&boot, esp, 512 << 20).unwrap();
            let region = crate::image::geometry::BootRegion::resolve(&boot, esp).unwrap();
            let img = sized_image(tmp.path(), &format!("{name}-index.img"), geom.total_size);
            write_table(&img, &geom, "rootfs", &identity()).unwrap();
            let disk = GptConfig::new()
                .writable(false)
                .logical_block_size(LogicalBlockSize::Lb512)
                .open(&img)
                .unwrap();
            let (index, _) = disk
                .partitions()
                .iter()
                .find(|(_, p)| p.name == "rootfs")
                .unwrap();
            assert_eq!(*index, region.rootfs_index(), "{name}");
        }
    }
}
