//! GPT partition table for the whole-disk image: a protective MBR plus the
//! partitions the boot method's geometry fixed. Pure Rust via the `gpt` crate — no
//! `sgdisk`/`parted`/`cgpt` shell-out.
//!
//! Two shapes, one writer. A `rockchip-rkbin` image has a single Linux-filesystem
//! partition (its bootloader lives in a raw gap *outside* the table); a
//! `depthcharge` image adds a **ChromeOS kernel partition** ahead of it, which is
//! the entire boot mechanism on that board — the firmware scans every medium's GPT
//! for that type GUID and picks among the candidates by the entry's attribute bits.
//! Both are ordinary GPT entries, so the type GUID (`partition_types::CHROME_KERNEL`)
//! and the raw 64-bit `flags` field carry it, and no ChromeOS host tooling is needed.
//!
//! The `gpt` crate writes only the GPT structures (primary header + entry array
//! at the front, backup at the end), so the protective MBR at LBA 0 is written
//! separately. The image file must already exist at its full size before this
//! runs — the crate opens it without creating it, and lays the backup table
//! relative to the file's length.

use crate::error::EngineError;
use crate::image::geometry::{BootGeometry, Geometry, SECTOR};
use gpt::disk::LogicalBlockSize;
use gpt::mbr::ProtectiveMBR;
use gpt::{partition_types, GptConfig};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use uuid::Uuid;

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
    /// The 64-bit attribute word. Zero except on a ChromeOS kernel partition,
    /// where it *is* the boot selection.
    flags: u64,
    /// The deterministic partition GUID (a rootfs partition's is its PARTUUID).
    guid: Uuid,
}

/// Write a protective MBR and the image's partitions into the existing, full-size
/// `image` file. `rootfs_label` names the rootfs GPT entry.
///
/// The rootfs partition spans `[rootfs_first_lba, rootfs_first_lba +
/// rootfs_length_lba)` — the exact range the ext4 filesystem is spliced into —
/// typed `LINUX_FS`, carrying `rootfs_guid` as its PARTUUID. Under `depthcharge` a
/// ChromeOS kernel partition precedes it, carrying the attribute bits the firmware
/// selects on; under `rockchip-rkbin` the bootloader payloads live in the raw gap
/// ahead of the table and are not GPT entries at all.
///
/// `disk_guid` and `rootfs_guid` are the deterministic identifiers the caller
/// derived from the lock: the `gpt` crate otherwise draws both from
/// `/dev/urandom` (the disk GUID at header build, the partition GUID inside
/// `add_partition_at`), which would make the GPT region differ on every rebuild.
/// Setting both makes the whole table a function of the lock — and, on depthcharge,
/// makes the rootfs PARTUUID knowable *before* the rootfs is built, which is what
/// lets its `/etc/fstab` name the partition the signed kernel will root on.
pub(crate) fn write_table(
    image: &Path,
    geom: &Geometry,
    rootfs_label: &str,
    disk_guid: Uuid,
    rootfs_guid: Uuid,
    kpart_guid: Uuid,
) -> Result<(), EngineError> {
    let mut parts = Vec::with_capacity(2);
    // The kernel partition comes first, both on the medium and in the table.
    if let BootGeometry::Kpart {
        first_lba,
        length_lba,
        flags,
        ..
    } = geom.boot
    {
        parts.push(PartitionSpec {
            index: 1,
            // The conventional ChromeOS name. The firmware selects by type GUID and
            // attributes, never by name, so this is for whoever reads the table.
            name: "KERN-A",
            first_lba,
            length_lba,
            part_type: partition_types::CHROME_KERNEL,
            flags,
            guid: kpart_guid,
        });
    }
    parts.push(PartitionSpec {
        index: parts.len() as u32 + 1,
        name: rootfs_label,
        first_lba: geom.rootfs_first_lba,
        length_lba: geom.rootfs_length_lba,
        part_type: partition_types::LINUX_FS,
        flags: 0,
        guid: rootfs_guid,
    });

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
        Kpart, Offsets, ResolvedBoot, ResolvedDepthchargeBoot, ResolvedRkbinBoot, Rkbin,
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
        ResolvedBoot::RockchipRkbin(ResolvedRkbinBoot {
            uboot_defconfig: "d".into(),
            uboot_source: "s".into(),
            uboot_ref: "v1".into(),
            rkbin: Rkbin {
                atf: "atf".into(),
                tpl: "tpl".into(),
                bl32: None,
            },
            offsets: Offsets {
                idbloader: "32KiB".into(),
                uboot_itb: "8MiB".into(),
                rootfs: "16MiB".into(),
            },
        })
    }

    fn depthcharge_boot() -> ResolvedBoot {
        ResolvedBoot::Depthcharge(ResolvedDepthchargeBoot {
            board: "speedy".into(),
            kpart: Kpart {
                offset: "12MiB".into(),
                size: "16MiB".into(),
                priority: 10,
                tries: 5,
                successful: true,
                flags: 0x015A_0000_0000_0000,
            },
            cmdline: "console=tty1 rootwait ro".into(),
            rootfs_offset: "28MiB".into(),
        })
    }

    const DISK_GUID: Uuid = Uuid::from_bytes([0xa1; 16]);
    const ROOTFS_GUID: Uuid = Uuid::from_bytes([0xb2; 16]);
    const KPART_GUID: Uuid = Uuid::from_bytes([0xc3; 16]);

    /// With the disk + partition GUIDs fixed, the whole GPT-bearing image is
    /// byte-identical across independent writes — the last random inputs in the
    /// table are gone. Also asserts the GUIDs actually land in the header /
    /// entry (a table that ignored them would still be self-consistent).
    #[test]
    fn gpt_bytes_are_reproducible_with_fixed_guids() {
        let tmp = tempfile::tempdir().unwrap();
        let size = 192 * 1024 * 1024;
        let geom = Geometry::resolve(&rkbin_boot(), "192MiB").unwrap();

        let a = sized_image(tmp.path(), "a.img", size);
        let b = sized_image(tmp.path(), "b.img", size);
        write_table(&a, &geom, "rootfs", DISK_GUID, ROOTFS_GUID, KPART_GUID).unwrap();
        write_table(&b, &geom, "rootfs", DISK_GUID, ROOTFS_GUID, KPART_GUID).unwrap();

        let ba = std::fs::read(&a).unwrap();
        assert_eq!(ba, std::fs::read(&b).unwrap(), "GPT image must reproduce byte-for-byte");

        // Both GUIDs appear in the table (GPT stores GUIDs mixed-endian, so match
        // the raw fields via the crate's own writer round-trip rather than raw bytes).
        let disk = GptConfig::new()
            .writable(false)
            .logical_block_size(LogicalBlockSize::Lb512)
            .open(&a)
            .unwrap();
        assert_eq!(disk.guid(), &DISK_GUID, "header carries the derived disk GUID");
        let parts = disk.partitions();
        assert_eq!(parts.len(), 1, "a raw-gap bootloader is not a GPT entry");
        assert_eq!(
            parts.get(&1).unwrap().part_guid,
            ROOTFS_GUID,
            "partition carries the derived GUID"
        );
    }

    /// The depthcharge table is the whole boot mechanism, so every field the
    /// firmware reads is asserted: the ChromeOS kernel type GUID, the exact LBA
    /// range, and the attribute word that says "boot this". A wrong value here is a
    /// board that silently refuses the image.
    #[test]
    fn a_depthcharge_table_carries_a_bootable_chromeos_kernel_partition() {
        let tmp = tempfile::tempdir().unwrap();
        let size = 512 * 1024 * 1024;
        let geom = Geometry::resolve(&depthcharge_boot(), "512MiB").unwrap();
        let img = sized_image(tmp.path(), "c201.img", size);
        write_table(&img, &geom, "rootfs", DISK_GUID, ROOTFS_GUID, KPART_GUID).unwrap();

        let disk = GptConfig::new()
            .writable(false)
            .logical_block_size(LogicalBlockSize::Lb512)
            .open(&img)
            .unwrap();
        let parts = disk.partitions();
        assert_eq!(parts.len(), 2);

        let kern = parts.get(&1).unwrap();
        assert_eq!(
            kern.part_type_guid.guid,
            Uuid::parse_str("FE3A2A5D-4F32-41A7-B725-ACCC3285A309").unwrap(),
            "the firmware finds the kernel by this type GUID and nothing else"
        );
        assert_eq!(kern.first_lba, 24_576);
        assert_eq!(kern.last_lba - kern.first_lba + 1, 32_768);
        assert_eq!(
            kern.flags, 0x015A_0000_0000_0000,
            "priority=10 tries=5 successful=1 — the bits that make it boot"
        );
        assert_eq!(kern.part_guid, KPART_GUID);

        let root = parts.get(&2).unwrap();
        assert_eq!(root.part_type_guid, partition_types::LINUX_FS);
        assert_eq!(root.first_lba, 57_344);
        assert_eq!(root.flags, 0, "an ordinary rootfs carries no attributes");
        assert_eq!(
            root.part_guid, ROOTFS_GUID,
            "the PARTUUID the signed kernel cmdline roots on"
        );
    }
}
