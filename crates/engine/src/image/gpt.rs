//! GPT partition table for the whole-disk image: a protective MBR plus a
//! single Linux-filesystem partition for the rootfs, at the offset the geometry
//! fixed. Pure Rust via the `gpt` crate — no `sgdisk`/`parted` shell-out.
//!
//! The `gpt` crate writes only the GPT structures (primary header + entry array
//! at the front, backup at the end), so the protective MBR at LBA 0 is written
//! separately. The image file must already exist at its full size before this
//! runs — the crate opens it without creating it, and lays the backup table
//! relative to the file's length.

use crate::error::EngineError;
use crate::image::geometry::{Geometry, SECTOR};
use gpt::disk::LogicalBlockSize;
use gpt::mbr::ProtectiveMBR;
use gpt::{partition_types, GptConfig};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::Path;
use uuid::Uuid;

/// Write a protective MBR and a single rootfs partition into the existing,
/// full-size `image` file. `label` names the GPT partition entry.
///
/// The partition spans `[rootfs_first_lba, rootfs_first_lba + rootfs_length_lba)`
/// — the exact range the ext4 filesystem is spliced into — typed `LINUX_FS`. The
/// bootloader payloads live in the raw gap *ahead* of this partition and are not
/// GPT entries.
///
/// `disk_guid` and `part_guid` are the deterministic identifiers the caller
/// derived from the lock: the `gpt` crate otherwise draws both from
/// `/dev/urandom` (the disk GUID at header build, the partition GUID inside
/// `add_partition_at`), which would make the GPT region differ on every rebuild.
/// Setting both makes the whole table a function of the lock.
pub(crate) fn write_table(
    image: &Path,
    geom: &Geometry,
    label: &str,
    disk_guid: Uuid,
    part_guid: Uuid,
) -> Result<(), EngineError> {
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
    disk.add_partition_at(
        label,
        1,
        geom.rootfs_first_lba,
        geom.rootfs_length_lba,
        partition_types::LINUX_FS,
        0,
    )
    .map_err(|e| gpt_err("add rootfs partition", e))?;
    // `add_partition_at` minted a random partition GUID; overwrite it with the
    // deterministic one before the table is serialized.
    let mut partitions = disk.partitions().clone();
    if let Some(part) = partitions.get_mut(&1) {
        part.part_guid = part_guid;
    }
    disk.update_partitions(partitions)
        .map_err(|e| gpt_err("set deterministic partition GUID", e))?;
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
    use boot2deb_core::model::Offsets;

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

    /// With the disk + partition GUIDs fixed, the whole GPT-bearing image is
    /// byte-identical across independent writes — the last random inputs in the
    /// table are gone. Also asserts the GUIDs actually land in the header /
    /// entry (a table that ignored them would still be self-consistent).
    #[test]
    fn gpt_bytes_are_reproducible_with_fixed_guids() {
        let tmp = tempfile::tempdir().unwrap();
        let offsets = Offsets {
            idbloader: "32KiB".into(),
            uboot_itb: "8MiB".into(),
            rootfs: "16MiB".into(),
        };
        let size = 192 * 1024 * 1024;
        let geom = Geometry::resolve(&offsets, "192MiB").unwrap();
        let disk_guid = Uuid::from_bytes([0xa1; 16]);
        let part_guid = Uuid::from_bytes([0xb2; 16]);

        let a = sized_image(tmp.path(), "a.img", size);
        let b = sized_image(tmp.path(), "b.img", size);
        write_table(&a, &geom, "rootfs", disk_guid, part_guid).unwrap();
        write_table(&b, &geom, "rootfs", disk_guid, part_guid).unwrap();

        let ba = std::fs::read(&a).unwrap();
        assert_eq!(ba, std::fs::read(&b).unwrap(), "GPT image must reproduce byte-for-byte");

        // Both GUIDs appear in the table (GPT stores GUIDs mixed-endian, so match
        // the raw fields via the crate's own writer round-trip rather than raw bytes).
        let disk = GptConfig::new()
            .writable(false)
            .logical_block_size(LogicalBlockSize::Lb512)
            .open(&a)
            .unwrap();
        assert_eq!(disk.guid(), &disk_guid, "header carries the derived disk GUID");
        assert_eq!(
            disk.partitions().get(&1).unwrap().part_guid,
            part_guid,
            "partition carries the derived GUID"
        );
    }
}
