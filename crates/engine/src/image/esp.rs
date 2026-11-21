//! The EFI system partition: a FAT32 volume the image node builds whole from the
//! rootfs tree's `/boot/efi/` subtree, under the `efi` loader.
//!
//! The rootfs node installs GRUB into a plain `/boot/efi` directory, as it would be
//! on the board with the partition mounted there
//! ([`ESP_MOUNT_POINT`](boot2deb_core::model::ESP_MOUNT_POINT)). This module moves that
//! subtree out of the tree the ext4 is formatted from and into a volume of its own. The
//! directory stays behind, empty, as the mount point. The rootfs therefore holds one
//! source of truth for the partition, written by the steps an on-device `grub-install`
//! and kernel hook repeat.
//!
//! The pure-Rust `ferrosys` formatter writes the volume in-process, as it writes the
//! seed partition. The volume is a function of the tree. Its serial comes off the
//! image's derived identity, and every timestamp is the tree's own, already clamped to
//! the lock's `SOURCE_DATE_EPOCH`.
//!
//! The split also checks for what the firmware will look for: the loader at the
//! removable-media path, and the board DTB under `\dtb\`. u-boot's UEFI boot reads the
//! DTB from the partition it started the loader from. A tree missing either fails the
//! build here rather than a board at power-on.

use crate::error::EngineError;
use crate::event::Step;
use boot2deb_core::model::{ResolvedEfi, ResolvedImage, ESP_MOUNT_POINT};
use ferrosys::fat::{self, FatType, FatTypeRequest, FormatOptions, PlanRequest, VolumeLabel};
use ferrosys::{EntryKind, Metadata, SourceEntry, Timestamp};
use std::path::Path;

/// The GPT entry name of the EFI system partition.
pub(crate) const ESP_PARTLABEL: &str = "esp";

/// The FAT volume label, so the partition reads as what it is in any tool that lists
/// volumes.
const ESP_LABEL: &str = "EFI";

/// FAT's earliest representable instant (1980-01-01T00:00:00Z). A tree time below it is
/// raised to it, as the seed partition's is.
const FAT_EPOCH_MIN: i64 = 315_532_800;

/// The directory under the ESP's root that holds the board DTB, where u-boot's UEFI
/// boot looks first (`/dtb/<fdtfile>`). The DTB's own path below it is the device's
/// `kernel_dtb`, which is what u-boot's `fdtfile` names for every board this builds.
pub(crate) const ESP_DTB_DIR: &str = "dtb";

/// The ESP's size in bytes under the `efi` loader, or `None` under a loader with no
/// partition.
///
/// # Errors
///
/// [`EngineError::ImageGeometry`] when the resolved size does not parse, which
/// resolution has already ruled out.
pub(crate) fn esp_bytes(image: &ResolvedImage) -> Result<Option<u64>, EngineError> {
    image
        .loader
        .as_ref()
        .and_then(|loader| loader.efi())
        .map(|efi| {
            boot2deb_core::size::parse_size(&efi.esp_size).map_err(|e| EngineError::ImageGeometry {
                detail: format!("esp_size '{}': {e}", efi.esp_size),
            })
        })
        .transpose()
}

/// Check that `bytes` holds a conformant FAT32 volume, without writing anything.
///
/// FAT32 rather than whatever the size reaches: the UEFI specification requires it of a
/// system partition on fixed media, and edk2's FAT driver and u-boot's both read it.
/// A size below FAT32's cluster minimum (about 33 MiB at one sector per cluster) is
/// refused here, at resolution, rather than formatted as FAT16.
///
/// # Errors
///
/// [`EngineError::ImageGeometry`] naming the size and what the planner refused.
pub(crate) fn check_size(bytes: u64) -> Result<(), EngineError> {
    fat::plan_layout(&plan_request(bytes))
        .map(|_| ())
        .map_err(|e| EngineError::ImageGeometry {
            detail: format!(
                "an EFI system partition of {bytes} bytes cannot hold a FAT32 volume: {e}"
            ),
        })
}

/// The geometry request every ESP is planned from: the size, and FAT32 exactly.
fn plan_request(bytes: u64) -> PlanRequest {
    PlanRequest::new(bytes).fat_type(FatTypeRequest::Exactly(FatType::Fat32))
}

/// Move the ESP's subtree out of `entries` and return it re-rooted at `/`.
///
/// Every entry strictly below [`ESP_MOUNT_POINT`] moves. The mount point itself stays in
/// `entries`, emptied, because the board mounts the partition there. Each moved entry's
/// times are brought to what FAT records (two-second precision, nothing before 1980), so
/// the format loses nothing the tree states; any other property FAT cannot hold, such as
/// a symbolic link or a mode other than root's `0644`/`0755`, fails the format by name.
///
/// The returned tree is then checked for the two things the firmware reads: the
/// architecture's loader at the removable-media path, and the board DTB under
/// [`ESP_DTB_DIR`], at the name the board's u-boot loads by ([`ResolvedEfi::dtb`]).
///
/// # Errors
///
/// [`EngineError::ArtifactMissing`] when the tree has no ESP subtree, or lacks the loader
/// or the DTB.
pub(crate) fn split_tree(
    entries: &mut Vec<SourceEntry>,
    efi: &ResolvedEfi,
    step: &Step,
) -> Result<Vec<SourceEntry>, EngineError> {
    let dtb = &efi.dtb;
    let mount = components(ESP_MOUNT_POINT.as_bytes());
    let mut esp = Vec::new();
    let mut kept = Vec::with_capacity(entries.len());
    for mut entry in std::mem::take(entries) {
        let parts = components(&entry.path);
        if parts.len() > mount.len() && parts[..mount.len()] == mount[..] {
            entry.path = join(&parts[mount.len()..]);
            entry.meta = fat_meta(&entry.meta);
            esp.push(entry);
        } else {
            kept.push(entry);
        }
    }
    *entries = kept;
    if esp.is_empty() {
        return Err(EngineError::ArtifactMissing {
            what: format!("the EFI system partition's tree ({ESP_MOUNT_POINT}/)"),
            location: "rootfs tar".into(),
        });
    }
    for required in [
        format!("/EFI/BOOT/{}", efi.arch.removable_file),
        format!("/{ESP_DTB_DIR}/{dtb}"),
    ] {
        let present = esp.iter().any(|e| {
            matches!(e.kind, EntryKind::File(_))
                && components(&e.path) == components(required.as_bytes())
        });
        if !present {
            return Err(EngineError::ArtifactMissing {
                what: format!("{required} on the EFI system partition"),
                location: format!("rootfs tar ({ESP_MOUNT_POINT}{required})"),
            });
        }
    }
    step.log(format!(
        "split {} entries out of {ESP_MOUNT_POINT}/ for the EFI system partition \
         (loader at /EFI/BOOT/{}, DTB at /{ESP_DTB_DIR}/{dtb})",
        esp.len(),
        efi.arch.removable_file
    ));
    Ok(esp)
}

/// Format the ESP's `tree` as a FAT32 volume of exactly `bytes` into `dest`.
///
/// `volume_id` is the serial the boot sector records, from the image's derived identity;
/// `time_secs` stamps the volume label entry, and is the rootfs's own format time so
/// every timestamp on the image answers to one clock. `offset_lba` is the partition's
/// first sector, recorded as the volume's hidden sectors.
///
/// # Errors
///
/// [`EngineError::ImageGeometry`] when the volume cannot be planned or the tree holds
/// something FAT would lose; I/O errors naming `dest`.
pub(crate) fn format(
    dest: &Path,
    tree: Vec<SourceEntry>,
    bytes: u64,
    volume_id: u32,
    time_secs: i64,
    offset_lba: u64,
    step: &Step,
) -> Result<(), EngineError> {
    let fail = |detail: String| EngineError::ImageGeometry { detail };
    let mut options = FormatOptions::new(volume_id, fat_time(time_secs))
        .label(VolumeLabel::new(ESP_LABEL).expect("a constant label the format accepts"))
        .plan(plan_request(bytes));
    options.hidden_sectors = u32::try_from(offset_lba).map_err(|_| {
        fail(format!(
            "the ESP's first sector ({offset_lba}) exceeds 32 bits"
        ))
    })?;
    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(dest)
        .map_err(|s| EngineError::io(dest, s))?;
    let plan = fat::format_to(tree, bytes, options, &mut out)
        .map_err(|e| fail(format!("the EFI system partition failed to format: {e}")))?;
    out.sync_all().map_err(|s| EngineError::io(dest, s))?;
    drop(out);
    verify_clean(dest)?;
    let layout = plan.layout();
    step.log(format!(
        "formatted the {bytes}-byte EFI system partition: FAT32, {} clusters of {} bytes, \
         serial {volume_id:08X}, scanned clean",
        layout.clusters,
        layout.bytes_per_cluster()
    ));
    Ok(())
}

/// Re-read the volume at `dest` with the crate's own reader and scan it whole. Any
/// finding fails, as it does for the rootfs: the volume was written seconds ago by the
/// formatter whose reader is judging it, so a disagreement is a formatter defect.
fn verify_clean(dest: &Path) -> Result<(), EngineError> {
    let file = std::fs::File::open(dest).map_err(|s| EngineError::io(dest, s))?;
    let mut reader = fat::Reader::open(file).map_err(|e| EngineError::ImageGeometry {
        detail: format!("re-reading the formatted EFI system partition: {e}"),
    })?;
    let report = reader.scan();
    if report.is_clean() {
        return Ok(());
    }
    Err(EngineError::ImageGeometry {
        detail: format!(
            "the formatted EFI system partition does not scan clean: {}",
            report.to_report().to_table()
        ),
    })
}

/// `meta` as FAT records it: the same mode and owner, every time the modification time
/// floored to FAT's two-second precision and raised to its epoch, and no birth time.
fn fat_meta(meta: &Metadata) -> Metadata {
    let mut fat = Metadata::new(meta.mode, fat_time(meta.mtime.secs));
    fat.uid = meta.uid;
    fat.gid = meta.gid;
    fat
}

/// `secs` at FAT's precision and in its range.
fn fat_time(secs: i64) -> Timestamp {
    Timestamp::from_secs(secs.max(FAT_EPOCH_MIN) & !1)
}

/// The non-empty, non-`.` components of `path`.
fn components(path: &[u8]) -> Vec<&[u8]> {
    path.split(|&b| b == b'/')
        .filter(|c| !c.is_empty() && *c != b".")
        .collect()
}

/// `parts` as an absolute path.
fn join(parts: &[&[u8]]) -> Vec<u8> {
    let mut path = Vec::new();
    for part in parts {
        path.push(b'/');
        path.extend_from_slice(part);
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;
    use boot2deb_core::model::Arch;
    use ferrosys::{Source, TreeBuilder};

    fn efi() -> ResolvedEfi {
        efi_dtb("rockchip/rk3588s-nanopi-r6s.dtb")
    }

    fn efi_dtb(dtb: &str) -> ResolvedEfi {
        ResolvedEfi {
            esp_size: "256MiB".into(),
            arch: Arch::Arm64.efi(),
            dtb: dtb.into(),
        }
    }

    fn step_sink() -> impl Fn(crate::event::Event) {
        |_| {}
    }

    /// A rootfs tree with GRUB installed under `/boot/efi`, as the customize step
    /// leaves it, at an odd-second time FAT cannot record.
    fn tree() -> Vec<SourceEntry> {
        let t = Timestamp::from_secs(1_794_847_897);
        let dir = Metadata::new(0o755, t);
        let file = Metadata::new(0o644, t);
        TreeBuilder::new()
            .directory(b"/boot".to_vec(), dir)
            .directory(b"/boot/efi".to_vec(), dir)
            .directory(b"/boot/efi/EFI".to_vec(), dir)
            .directory(b"/boot/efi/EFI/BOOT".to_vec(), dir)
            .file(b"/boot/efi/EFI/BOOT/BOOTAA64.EFI".to_vec(), b"MZ", file)
            .directory(b"/boot/efi/EFI/debian".to_vec(), dir)
            .file(b"/boot/efi/EFI/debian/grubaa64.efi".to_vec(), b"MZ", file)
            .directory(b"/boot/efi/dtb".to_vec(), dir)
            .directory(b"/boot/efi/dtb/rockchip".to_vec(), dir)
            .file(
                b"/boot/efi/dtb/rockchip/rk3588s-nanopi-r6s.dtb".to_vec(),
                b"\xd0\x0d\xfe\xed",
                file,
            )
            .directory(b"/boot/grub".to_vec(), dir)
            .file(b"/boot/grub/grub.cfg".to_vec(), b"menuentry", file)
            .into_entries()
    }

    #[test]
    fn the_subtree_moves_and_the_mount_point_stays() {
        let sink = step_sink();
        let step = Step::start(&sink, "image");
        let mut entries = tree();
        let esp = split_tree(&mut entries, &efi(), &step).unwrap();
        let rootfs: Vec<_> = entries.iter().map(|e| e.path.clone()).collect();
        assert!(
            rootfs.contains(&b"/boot/efi".to_vec()),
            "the mount point stays"
        );
        assert!(rootfs.contains(&b"/boot/grub/grub.cfg".to_vec()));
        assert!(!rootfs.iter().any(|p| p.starts_with(b"/boot/efi/")));
        let moved: Vec<_> = esp.iter().map(|e| e.path.clone()).collect();
        assert!(moved.contains(&b"/EFI/BOOT/BOOTAA64.EFI".to_vec()));
        assert!(moved.contains(&b"/dtb/rockchip/rk3588s-nanopi-r6s.dtb".to_vec()));
        // Times are FAT's: even seconds.
        assert!(esp.iter().all(|e| e.meta.mtime.secs % 2 == 0));
    }

    #[test]
    fn a_tree_without_the_loader_or_the_dtb_is_refused() {
        let sink = step_sink();
        let step = Step::start(&sink, "image");
        let mut entries = tree();
        let err = split_tree(&mut entries, &efi_dtb("rockchip/other.dtb"), &step).unwrap_err();
        assert!(err.to_string().contains("/dtb/rockchip/other.dtb"), "{err}");

        let mut no_loader: Vec<_> = tree()
            .into_iter()
            .filter(|e| !e.path.ends_with(b"BOOTAA64.EFI"))
            .collect();
        let err = split_tree(&mut no_loader, &efi(), &step).unwrap_err();
        assert!(err.to_string().contains("/EFI/BOOT/BOOTAA64.EFI"), "{err}");

        let mut none: Vec<_> = tree()
            .into_iter()
            .filter(|e| !e.path.starts_with(b"/boot/efi/"))
            .collect();
        assert!(split_tree(&mut none, &efi(), &step).is_err());
    }

    #[test]
    fn the_volume_is_fat32_reproducible_and_reads_back() {
        let sink = step_sink();
        let step = Step::start(&sink, "image");
        let tmp = tempfile::tempdir().unwrap();
        let mut entries = tree();
        let esp = split_tree(&mut entries, &efi(), &step).unwrap();
        let bytes = 64 << 20;
        let a = tmp.path().join("a.fat");
        let b = tmp.path().join("b.fat");
        format(
            &a,
            esp.clone(),
            bytes,
            0x1234_abcd,
            1_794_847_897,
            32768,
            &step,
        )
        .unwrap();
        format(&b, esp, bytes, 0x1234_abcd, 1_794_847_897, 32768, &step).unwrap();
        let (a, b) = (std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
        assert_eq!(a.len() as u64, bytes);
        assert_eq!(a, b, "two formats of one tree are the same bytes");
        let mut reader = fat::Reader::open(std::io::Cursor::new(a)).unwrap();
        assert_eq!(reader.layout().fat_type, FatType::Fat32);
        let node = reader.lookup(b"/EFI/BOOT/BOOTAA64.EFI").unwrap();
        assert_eq!(reader.read_data(&node).unwrap(), b"MZ");
        let node = reader
            .lookup(b"/dtb/rockchip/rk3588s-nanopi-r6s.dtb")
            .unwrap();
        assert_eq!(reader.read_data(&node).unwrap(), b"\xd0\x0d\xfe\xed");
    }

    #[test]
    fn a_partition_too_small_for_fat32_is_refused() {
        assert!(check_size(16 << 20).is_err());
        assert!(check_size(256 << 20).is_ok());
    }
}
