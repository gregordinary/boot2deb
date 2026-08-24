//! Verify a written image file: re-read what was written and hold it to the
//! digest computed during the write, then re-read the partition table and hold
//! it to the one the source artifact carries.
//!
//! The two checks catch different faults with the same symptom — a truncated
//! copy (a filesystem that ran out of space mid-write), and a table that does
//! not say what the artifact's does. Cheap next to the write itself, and
//! together they keep the property `press` exists for: the file handed to a
//! flasher is exactly what the build made.
//!
//! An image written to a medium larger than itself carries its *backup* GPT at
//! the image's end, not the medium's — every tool reports that until first boot
//! moves it, and it is not a fault. Both reads therefore judge the primary
//! table only, which is the one the backup is reconstructed from.

use crate::error::EngineError;
use crate::event::Step;
use crate::press::write::{decompressed_prefix, hex, WrittenImage};
use gpt::disk::LogicalBlockSize;
use gpt::GptConfig;
use sha2::{Digest, Sha256};
use std::io::Read;
use std::path::Path;

/// Sectors the primary GPT occupies (protective MBR + header + entry array), and
/// so the prefix a table comparison needs.
const GPT_PREFIX_BYTES: usize = 34 * 512;

/// One partition entry as the table comparison sees it: the fields the firmware
/// and the kernel act on. The partition GUID is deliberately included — on
/// depthcharge it is how the booted slot knows itself — and the attribute
/// word carries the boot selection.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableEntry {
    /// Entry index (1-based).
    pub index: u32,
    /// Entry label.
    pub name: String,
    /// First LBA.
    pub first_lba: u64,
    /// Last LBA (inclusive).
    pub last_lba: u64,
    /// Type GUID, lowercase hyphenated.
    pub type_guid: String,
    /// Partition GUID, lowercase hyphenated.
    pub part_guid: String,
    /// The 64-bit attribute word.
    pub flags: u64,
}

/// Re-read `written.bytes` from `target` and hold them to `written.sha256`.
///
/// Reports progress on `step` in the 0–100 range of its own — verification is its
/// own step, so the bar restarting is the truth rather than a glitch.
///
/// # Errors
///
/// [`EngineError::ImageVerifyShortRead`] when the target hands back fewer bytes
/// than were written; [`EngineError::ImageVerifyDigest`] when the bytes differ.
pub fn verify_digest(
    target: &Path,
    written: &WrittenImage,
    step: &Step,
) -> Result<(), EngineError> {
    let mut file = std::fs::File::open(target).map_err(|e| EngineError::io(target, e))?;
    let mut hasher = Sha256::new();
    let mut remaining = written.bytes;
    let mut buf = vec![0u8; 4 << 20];
    let mut last_pct = 0u8;
    while remaining > 0 {
        let want = buf
            .len()
            .min(usize::try_from(remaining).unwrap_or(buf.len()));
        let n = file
            .read(&mut buf[..want])
            .map_err(|e| EngineError::io(target, e))?;
        if n == 0 {
            return Err(EngineError::ImageVerifyShortRead {
                target: target.display().to_string(),
                expected_bytes: written.bytes,
                read_bytes: written.bytes - remaining,
            });
        }
        hasher.update(&buf[..n]);
        remaining -= n as u64;
        if let Some(pct) = ((written.bytes - remaining) * 100).checked_div(written.bytes) {
            let pct = pct as u8;
            if pct > last_pct {
                last_pct = pct;
                step.progress(pct);
            }
        }
    }
    let actual = hex(&hasher.finalize());
    if actual != written.sha256 {
        return Err(EngineError::ImageVerifyDigest {
            target: target.display().to_string(),
            expected: written.sha256.clone(),
            actual,
        });
    }
    Ok(())
}

/// The partition table the image itself plans: its primary GPT, parsed from the
/// (decompressed) head of the artifact.
///
/// # Errors
///
/// [`EngineError::ImageVerifyGpt`] when the artifact carries no readable primary
/// table — which a `-boot.img` legitimately does not; the caller skips the table
/// comparison for an artifact this refuses.
pub fn planned_table(artifact: &Path) -> Result<Vec<TableEntry>, EngineError> {
    let prefix = decompressed_prefix(artifact, GPT_PREFIX_BYTES)?;
    read_table_from(
        std::io::Cursor::new(prefix),
        &artifact.display().to_string(),
    )
}

/// The whole-image size the artifact's own GPT states: the backup header sits on
/// the image's last LBA, so `backup_lba + 1` sectors *is* the image — how large
/// a medium the pressed file needs, read without decompressing anything past
/// the table.
///
/// # Errors
///
/// [`EngineError::ImageVerifyGpt`] when the artifact has no readable primary
/// table — a `-boot.img`, which states no size.
pub fn planned_image_bytes(artifact: &Path) -> Result<u64, EngineError> {
    let prefix = decompressed_prefix(artifact, GPT_PREFIX_BYTES)?;
    let name = artifact.display().to_string();
    let disk = GptConfig::new()
        .writable(false)
        .logical_block_size(LogicalBlockSize::Lb512)
        .open_from_device(std::io::Cursor::new(prefix))
        .map_err(|e| EngineError::ImageVerifyGpt {
            target: name.clone(),
            detail: format!("no readable primary GPT: {e}"),
        })?;
    let header = disk
        .primary_header()
        .map_err(|e| EngineError::ImageVerifyGpt {
            target: name,
            detail: format!("the primary GPT header did not validate: {e}"),
        })?;
    Ok((header.backup_lba + 1) * 512)
}

/// The partition table `target` actually carries, read back from the medium.
///
/// # Errors
///
/// [`EngineError::ImageVerifyGpt`] when no valid primary table comes back — after
/// a write that placed one, that is a verification failure, not a formatting
/// choice.
pub fn read_back_table(target: &Path) -> Result<Vec<TableEntry>, EngineError> {
    let file = std::fs::File::open(target).map_err(|e| EngineError::io(target, e))?;
    read_table_from(file, &target.display().to_string())
}

/// Hold the read-back table to the planned one, entry for entry.
///
/// # Errors
///
/// [`EngineError::ImageVerifyGpt`] naming the first entry that differs.
pub fn compare_tables(
    target: &str,
    planned: &[TableEntry],
    read_back: &[TableEntry],
) -> Result<(), EngineError> {
    if planned == read_back {
        return Ok(());
    }
    // Name the first difference precisely: "the table differs" alone sends the
    // operator diffing hexdumps.
    let detail = planned
        .iter()
        .zip(read_back.iter())
        .find(|(p, r)| p != r)
        .map(|(p, r)| {
            format!(
                "entry {} differs: image plans {:?} at LBA {}..{}, target holds {:?} at LBA {}..{}",
                p.index, p.name, p.first_lba, p.last_lba, r.name, r.first_lba, r.last_lba
            )
        })
        .unwrap_or_else(|| {
            format!(
                "the image plans {} partitions, the target holds {}",
                planned.len(),
                read_back.len()
            )
        });
    Err(EngineError::ImageVerifyGpt {
        target: target.to_string(),
        detail,
    })
}

/// Parse the primary GPT out of any readable source. The backup header is
/// expected to be absent or misplaced (see the module doc) and is not judged.
fn read_table_from<D: gpt::DiskDevice>(
    device: D,
    name: &str,
) -> Result<Vec<TableEntry>, EngineError> {
    let disk = GptConfig::new()
        .writable(false)
        .logical_block_size(LogicalBlockSize::Lb512)
        .open_from_device(device)
        .map_err(|e| EngineError::ImageVerifyGpt {
            target: name.to_string(),
            detail: format!("no readable primary GPT: {e}"),
        })?;
    if disk.primary_header().is_err() {
        return Err(EngineError::ImageVerifyGpt {
            target: name.to_string(),
            detail: "the primary GPT header did not validate".into(),
        });
    }
    Ok(disk
        .partitions()
        .iter()
        .map(|(index, p)| TableEntry {
            index: *index,
            name: p.name.clone(),
            first_lba: p.first_lba,
            last_lba: p.last_lba,
            type_guid: p.part_type_guid.guid.to_string().to_lowercase(),
            part_guid: p.part_guid.to_string().to_lowercase(),
            flags: p.flags,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;
    use crate::press::write::stream_image;
    use std::io::Write as _;

    fn sink() -> impl Fn(Event) {
        |_| {}
    }

    /// A tiny GPT-bearing "image": sized file, real table via the gpt crate.
    fn gpt_image(dir: &Path, name: &str, size: u64) -> std::path::PathBuf {
        let p = dir.join(name);
        let f = std::fs::File::create(&p).unwrap();
        f.set_len(size).unwrap();
        drop(f);
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open(&p)
            .unwrap();
        let mut disk = GptConfig::new()
            .writable(true)
            .logical_block_size(LogicalBlockSize::Lb512)
            .create_from_device(file, Some(uuid::Uuid::from_bytes([0xd0; 16])))
            .unwrap();
        disk.add_partition_at(
            "rootfs",
            1,
            2048,
            (size / 512) - 2048 - 33,
            gpt::partition_types::LINUX_FS,
            0,
        )
        .unwrap();
        let mut file = disk.write().unwrap();
        file.flush().unwrap();
        p
    }

    /// Write → verify: the round trip passes, and a flipped byte fails with the
    /// two digests in the error.
    #[test]
    fn digest_verify_passes_and_catches_corruption() {
        let tmp = tempfile::tempdir().unwrap();
        let img = gpt_image(tmp.path(), "a.img", 4 << 20);
        let dest_path = tmp.path().join("dest.img");

        let sink_fn = sink();
        let step = Step::start(&sink_fn, "press");
        let mut dest = std::fs::File::create(&dest_path).unwrap();
        let written = stream_image(&img, &mut dest, &step).unwrap();
        dest.sync_all().unwrap();
        drop(dest);
        verify_digest(&dest_path, &written, &step).unwrap();

        // Corrupt one byte mid-image: the digest must catch it.
        let mut bytes = std::fs::read(&dest_path).unwrap();
        bytes[1 << 20] ^= 0xff;
        std::fs::write(&dest_path, &bytes).unwrap();
        let err = verify_digest(&dest_path, &written, &step).unwrap_err();
        assert!(
            matches!(err, EngineError::ImageVerifyDigest { .. }),
            "got {err}"
        );
        step.finish();
    }

    /// A target that hands back fewer bytes than were written is a short read,
    /// named as such — the truncated-copy case.
    #[test]
    fn a_short_target_is_a_short_read() {
        let tmp = tempfile::tempdir().unwrap();
        let img = gpt_image(tmp.path(), "a.img", 4 << 20);
        let dest_path = tmp.path().join("dest.img");
        let sink_fn = sink();
        let step = Step::start(&sink_fn, "press");
        let mut dest = std::fs::File::create(&dest_path).unwrap();
        let written = stream_image(&img, &mut dest, &step).unwrap();
        drop(dest);

        // Truncate: the medium "lost" the tail.
        let f = std::fs::OpenOptions::new()
            .write(true)
            .open(&dest_path)
            .unwrap();
        f.set_len(1 << 20).unwrap();
        drop(f);
        let err = verify_digest(&dest_path, &written, &step).unwrap_err();
        let EngineError::ImageVerifyShortRead {
            expected_bytes,
            read_bytes,
            ..
        } = err
        else {
            panic!("expected ImageVerifyShortRead, got {err}");
        };
        assert_eq!(expected_bytes, 4 << 20);
        assert_eq!(read_bytes, 1 << 20);
        step.finish();
    }

    /// The planned table (from the artifact's decompressed prefix) matches the
    /// read-back table (from the written destination) — and a destination larger
    /// than the image, where the backup GPT is "misplaced", still reads.
    #[test]
    fn planned_and_read_back_tables_agree_even_on_a_larger_medium() {
        let tmp = tempfile::tempdir().unwrap();
        let img = gpt_image(tmp.path(), "a.img", 4 << 20);
        let dest_path = tmp.path().join("dest.img");

        let sink_fn = sink();
        let step = Step::start(&sink_fn, "press");
        // A destination twice the image: the tail stays zero, like a real card.
        {
            let mut dest = std::fs::File::create(&dest_path).unwrap();
            dest.set_len(8 << 20).unwrap();
            stream_image(&img, &mut dest, &step).unwrap();
        }
        step.finish();

        let planned = planned_table(&img).unwrap();
        assert_eq!(planned.len(), 1);
        assert_eq!(planned[0].name, "rootfs");
        let read_back = read_back_table(&dest_path).unwrap();
        compare_tables(&dest_path.display().to_string(), &planned, &read_back).unwrap();
    }

    /// A device whose table does not match the image's — the stale-cache /
    /// reordered-write symptom — fails naming the entry.
    #[test]
    fn a_differing_table_fails_naming_the_entry() {
        let tmp = tempfile::tempdir().unwrap();
        let img = gpt_image(tmp.path(), "a.img", 4 << 20);
        let other = gpt_image(tmp.path(), "b.img", 4 << 20);
        // Rewrite b's table with a different partition start.
        {
            let file = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&other)
                .unwrap();
            let mut disk = GptConfig::new()
                .writable(true)
                .logical_block_size(LogicalBlockSize::Lb512)
                .create_from_device(file, Some(uuid::Uuid::from_bytes([0xd0; 16])))
                .unwrap();
            disk.add_partition_at(
                "rootfs",
                1,
                4096,
                (4 << 20) / 512 - 4096 - 33,
                gpt::partition_types::LINUX_FS,
                0,
            )
            .unwrap();
            disk.write().unwrap();
        }
        let planned = planned_table(&img).unwrap();
        let read_back = read_back_table(&other).unwrap();
        let err = compare_tables("t", &planned, &read_back).unwrap_err();
        let EngineError::ImageVerifyGpt { detail, .. } = err else {
            panic!("expected ImageVerifyGpt, got {err}");
        };
        assert!(detail.contains("entry 1"), "{detail}");
    }

    /// An artifact with no GPT (a `-boot.img`) refuses the table check by name,
    /// which is the caller's signal to skip it.
    #[test]
    fn a_tableless_artifact_refuses_the_table_check() {
        let tmp = tempfile::tempdir().unwrap();
        let img = tmp.path().join("x-boot.img");
        std::fs::write(&img, vec![0u8; 64 * 1024]).unwrap();
        let err = planned_table(&img).unwrap_err();
        assert!(
            matches!(err, EngineError::ImageVerifyGpt { .. }),
            "got {err}"
        );
    }
}
