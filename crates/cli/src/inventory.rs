//! The artifact inventory's file side: reading and writing `<stem>.artifacts.toml`,
//! hashing what a stage publishes into it, and holding what a consumer reads to it.
//!
//! The document and its rules are [`boot2deb_core::inventory`]'s. The side effects are
//! here: the lock's digest, the atomic write, and the hash of every file on the way in and
//! out. A consumer never takes a file because of its name. It takes the entry for the
//! stage that publishes it, and holds the entry to the lock it is acting on. It then holds
//! the file's bytes to the digest the entry records.

use boot2deb_core::inventory::{DiskDigest, Inventory, InventoryFile, Stage, StageEntry};
use boot2deb_core::ConfigRoot;
use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// The lowercase-hex sha256 of `recipe`'s lock file: the generation every inventory
/// entry of the point is bound to.
pub(crate) fn lock_digest(root: &ConfigRoot, recipe: &str) -> Result<String> {
    let path = root.lock_path(recipe)?;
    let bytes =
        std::fs::read(&path).map_err(|e| format!("cannot read {} ({e})", path.display()))?;
    Ok(boot2deb_engine::blobs::sha256_hex(&bytes))
}

/// The inventory beside `stem`'s artifacts in `out_dir`; empty when none has been written.
pub(crate) fn load(out_dir: &Path, stem: &str) -> Result<Inventory> {
    let path = out_dir.join(boot2deb_core::inventory::inventory_name(stem));
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(Inventory::from_toml_str(
            &text,
            &path.display().to_string(),
        )?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Inventory::default()),
        Err(e) => Err(format!("cannot read {} ({e})", path.display()).into()),
    }
}

/// Write `inventory` in place of `stem`'s, atomically, so a reader never sees half of
/// one: staged under a `.partial` name the artifact directory's sweep knows, then renamed.
pub(crate) fn save(out_dir: &Path, stem: &str, inventory: &Inventory) -> Result<()> {
    let name = boot2deb_core::inventory::inventory_name(stem);
    let path = out_dir.join(&name);
    let tmp = out_dir.join(format!(".{name}.{}.partial", std::process::id()));
    std::fs::write(&tmp, inventory.to_toml())
        .and_then(|()| std::fs::rename(&tmp, &path))
        .map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("cannot write {} ({e})", path.display())
        })?;
    Ok(())
}

/// Drop `stage`'s entry and write the inventory back: the stage has started, so nothing
/// it published before is current until it finishes again.
pub(crate) fn begin(out_dir: &Path, stem: &str, stage: Stage) -> Result<()> {
    let mut inventory = load(out_dir, stem)?;
    if inventory.entry(stage).is_some() {
        inventory.remove(stage);
        save(out_dir, stem, &inventory)?;
    }
    Ok(())
}

/// Record `entry`, replacing its stage's previous one, and write the inventory back.
pub(crate) fn publish(out_dir: &Path, stem: &str, entry: StageEntry) -> Result<()> {
    let mut inventory = load(out_dir, stem)?;
    inventory.put(entry);
    save(out_dir, stem, &inventory)
}

/// `path` as an inventory row under `role`: its file name, size and sha256.
pub(crate) fn hashed(path: &Path, role: &str) -> Result<InventoryFile> {
    let (size, sha256) = boot2deb_engine::blobs::sha256_file(path)?;
    Ok(InventoryFile {
        role: role.to_string(),
        file: file_name(path)?,
        size,
        sha256,
        disk: None,
    })
}

/// A disk image as an inventory row: hashed as stored, with the disk it holds.
pub(crate) fn hashed_disk(path: &Path, role: &str, disk: DiskDigest) -> Result<InventoryFile> {
    Ok(InventoryFile {
        disk: Some(disk),
        ..hashed(path, role)?
    })
}

/// The entry `stage` published for the lock whose digest is `lock_sha256`.
///
/// # Errors
///
/// When there is no entry, naming the stage to run; when the entry is another lock's,
/// saying so.
pub(crate) fn current<'a>(
    inventory: &'a Inventory,
    stage: Stage,
    lock_sha256: &str,
    out_dir: &Path,
    recipe: &str,
) -> Result<&'a StageEntry> {
    let entry = inventory.entry(stage).ok_or_else(|| {
        format!(
            "no {} artifacts in {} are recorded as current: none has been published, or \
             its last run did not finish — run `boot2deb build {recipe} --stage {}`",
            stage.as_str(),
            out_dir.display(),
            stage.as_str()
        )
    })?;
    entry.of_lock(lock_sha256)?;
    Ok(entry)
}

/// `file`'s path in `out_dir`, once its bytes are the ones the inventory records.
///
/// Hashes the whole file. For a multi-gigabyte container that is seconds of reading,
/// which is the price of not trusting a name.
///
/// # Errors
///
/// When the file is missing, is not a regular file, or its size or digest differs from
/// the record.
pub(crate) fn verified(out_dir: &Path, file: &InventoryFile) -> Result<PathBuf> {
    let path = present(out_dir, file)?;
    let (_, sha256) = boot2deb_engine::blobs::sha256_file(&path)?;
    if sha256 != file.sha256 {
        return Err(format!(
            "{} is not the file its build recorded (sha256 {}, recorded {}) — it was \
             changed after the build; rebuild",
            path.display(),
            short(&sha256),
            short(&file.sha256)
        )
        .into());
    }
    Ok(path)
}

/// `file`'s path in `out_dir`, once it is a regular file of the recorded size. For a
/// consumer that hashes the bytes as it reads them, and compares the digest itself.
///
/// # Errors
///
/// When the file is missing, is not a regular file, or its size differs from the record.
pub(crate) fn present(out_dir: &Path, file: &InventoryFile) -> Result<PathBuf> {
    let path = out_dir.join(&file.file);
    let meta = std::fs::symlink_metadata(&path).map_err(|e| {
        format!(
            "{} is recorded as a current artifact but cannot be read ({e}) — rebuild",
            path.display()
        )
    })?;
    if !meta.file_type().is_file() {
        return Err(format!(
            "{} is recorded as a current artifact but is not a regular file",
            path.display()
        )
        .into());
    }
    if meta.len() != file.size {
        return Err(format!(
            "{} is {} bytes, and its build recorded {} — it was changed after the build; \
             rebuild",
            path.display(),
            meta.len(),
            file.size
        )
        .into());
    }
    Ok(path)
}

/// The final component of `path`, as the inventory names files.
fn file_name(path: &Path) -> Result<String> {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .ok_or_else(|| format!("{} has no UTF-8 file name", path.display()).into())
}

/// The first 12 hex digits of a digest, for messages.
fn short(hex: &str) -> &str {
    hex.get(..12).unwrap_or(hex)
}

#[cfg(test)]
mod tests {
    use super::*;
    use boot2deb_core::inventory::role;

    fn entry(lock: &str, artifacts: Vec<InventoryFile>) -> StageEntry {
        StageEntry {
            stage: Stage::Rootfs,
            lock_sha256: lock.into(),
            finished: "2026-11-21T00:00:00Z".into(),
            layout: None,
            external_rootfs: false,
            inputs: Vec::new(),
            artifacts,
        }
    }

    /// A stage's start drops its entry from disk, its finish writes it back, and the
    /// document reads back as written.
    #[test]
    fn a_stage_entry_is_absent_while_the_stage_runs() {
        let tmp = tempfile::tempdir().unwrap();
        let tar = tmp.path().join("p-rootfs.tar");
        std::fs::write(&tar, b"tar bytes").unwrap();
        publish(
            tmp.path(),
            "p",
            entry("l", vec![hashed(&tar, role::ROOTFS_TAR).unwrap()]),
        )
        .unwrap();
        assert!(load(tmp.path(), "p")
            .unwrap()
            .entry(Stage::Rootfs)
            .is_some());
        begin(tmp.path(), "p", Stage::Rootfs).unwrap();
        assert!(load(tmp.path(), "p")
            .unwrap()
            .entry(Stage::Rootfs)
            .is_none());
        assert_eq!(load(tmp.path(), "absent").unwrap(), Inventory::default());
    }

    /// A consumer gets a file only when it is the recorded bytes: a changed one, a
    /// swapped one of the same size, a missing one, and a symlink in its place are each
    /// refused.
    #[test]
    fn a_recorded_file_is_taken_only_as_recorded() {
        let tmp = tempfile::tempdir().unwrap();
        let tar = tmp.path().join("p-rootfs.tar");
        std::fs::write(&tar, b"tar bytes").unwrap();
        let row = hashed(&tar, role::ROOTFS_TAR).unwrap();
        assert_eq!(verified(tmp.path(), &row).unwrap(), tar);

        std::fs::write(&tar, b"tar BYTES").unwrap();
        assert!(verified(tmp.path(), &row)
            .unwrap_err()
            .to_string()
            .contains("changed after the build"));
        assert!(
            present(tmp.path(), &row).is_ok(),
            "same size passes presence"
        );

        std::fs::write(&tar, b"longer tar bytes").unwrap();
        assert!(present(tmp.path(), &row).is_err());

        std::fs::remove_file(&tar).unwrap();
        let other = tmp.path().join("elsewhere");
        std::fs::write(&other, b"tar bytes").unwrap();
        std::os::unix::fs::symlink(&other, &tar).unwrap();
        assert!(verified(tmp.path(), &row)
            .unwrap_err()
            .to_string()
            .contains("not a regular file"));
    }

    /// The current entry is the one for this lock; another lock's, or none, is an error
    /// naming the stage to run.
    #[test]
    fn only_the_current_locks_entry_is_current() {
        let mut inv = Inventory::default();
        let dir = Path::new("/out");
        let err = current(&inv, Stage::Rootfs, "l", dir, "b/f")
            .unwrap_err()
            .to_string();
        assert!(err.contains("--stage rootfs"), "{err}");
        inv.put(entry("old", vec![]));
        assert!(current(&inv, Stage::Rootfs, "new", dir, "b/f").is_err());
        assert!(current(&inv, Stage::Rootfs, "old", dir, "b/f").is_ok());
    }
}
