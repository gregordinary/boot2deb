//! The artifact inventory: which files each stage of a build point last published into
//! its artifact directory, under which lock, and with what digests.
//!
//! An artifact directory outlives the runs that write into it, and old files stay:
//!
//! - A run that switches `--compress` or `--layout` leaves the previous run's other
//!   containers behind.
//! - A re-pin leaves the rootfs tar and the bootloader payloads of the lock before it.
//! - A rootfs run whose solve the pinned manifest rejects has still written its tar.
//!
//! Every one of those files has the name a current one would have. A file's name cannot
//! say which run wrote it, or whether that run's checks passed.
//!
//! The inventory says it. It has an entry for each stage whose outputs a later stage or
//! command reads by name: the u-boot, rootfs and image stages. A stage removes its entry
//! when it starts, and writes it back only once its outputs passed every check the stage
//! makes. The entry names the lock the stage ran under by the digest of its bytes, and
//! every file by its digest.
//!
//! A consumer reads the entry and holds its lock digest to the
//! lock it is acting on. It then holds each file it reads to the recorded digest. A file
//! the inventory does not list is not an artifact of the point, whatever its name.
//!
//! The lock digest is the generation the inventory binds a file to. Every pin that reaches
//! an artifact is in the lock, so a re-pin is a new generation. A change to the config
//! tree that no pin records, an overlay edit for instance, is outside that binding. Such a
//! change is rebuilt on the operator's word, as it always is.
//!
//! The document is `<stem>.artifacts.toml`, beside the artifacts it lists
//! ([`inventory_name`]).
//!
//! Pure: the document type, its queries, and its TOML form. The CLI hashes the files and
//! writes the document.

use crate::error::ConfigError;
use crate::model::Layout;
use crate::press::ArtifactRole;
use serde::{Deserialize, Serialize};

/// The file name of a build point's inventory, beside its other artifacts.
pub fn inventory_name(stem: &str) -> String {
    format!("{stem}.artifacts.toml")
}

/// The roles a stage records its non-image files under. A disk image's role is its
/// [`ArtifactRole::key`] instead. That lets a consumer ask for "the combined image"
/// without knowing which container the build chose.
pub mod role {
    /// The rootfs stage's tarball, the tree every image of the point is formatted from.
    pub const ROOTFS_TAR: &str = "rootfs-tar";
    /// The rootfs stage's solved package manifest.
    pub const SOLVED_MANIFEST: &str = "solved-manifest";
    /// The rootfs stage's published plan document.
    pub const PLAN: &str = "plan";
    /// The u-boot stage's `idbloader.img` (TPL and SPL).
    pub const IDBLOADER: &str = "idbloader";
    /// The u-boot stage's `u-boot.itb` (u-boot proper, BL31 and the device tree).
    pub const UBOOT_ITB: &str = "uboot-itb";
    /// The u-boot stage's `u-boot-<device>` package.
    pub const UBOOT_DEB: &str = "uboot-deb";
    /// The u-boot stage's CODE471 maskrom payload.
    pub const USB471: &str = "usb471";
    /// The u-boot stage's CODE472 maskrom payload.
    pub const USB472: &str = "usb472";
    /// The u-boot stage's packed `rkdeveloptool db` loader.
    pub const MASKROM_LOADER: &str = "maskrom-loader";
    /// The image stage's rootfs file manifest, `<stem>.rootfs.uapi16`.
    pub const FILES_MANIFEST: &str = "files-manifest";
}

/// A stage that records its outputs in the inventory.
///
/// Only the stages whose outputs something reads back by name have an entry. The compile
/// stages' packages reach the rootfs through the artifact ledger instead, by package name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Stage {
    /// The bootloader payloads, the u-boot package, and the standalone boot image a
    /// u-boot deliverable or an explicit `--stage uboot` emits.
    Uboot,
    /// The rootfs tarball and the manifest and plan published with it.
    Rootfs,
    /// The disk images, their containers, and the rootfs file manifest.
    Image,
}

impl Stage {
    /// The stage's `--stage` name, for messages.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Stage::Uboot => "uboot",
            Stage::Rootfs => "rootfs",
            Stage::Image => "image",
        }
    }
}

/// A build point's inventory: one entry per stage that has published, in stage order.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Inventory {
    /// The entries, at most one per [`Stage`], sorted by stage.
    #[serde(default, rename = "stage")]
    pub stages: Vec<StageEntry>,
}

/// What one stage last published, once every check it makes had passed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StageEntry {
    /// The stage.
    pub stage: Stage,
    /// Lowercase-hex sha256 of the lock file's bytes the stage ran under: the generation
    /// every file below belongs to.
    pub lock_sha256: String,
    /// When the stage finished, RFC 3339 UTC.
    pub finished: String,
    /// The layout the image stage laid the disk out as. Present on the image stage's
    /// entry only, since nothing the other stages publish depends on it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layout: Option<Layout>,
    /// Whether the rootfs the image stage formatted came from outside the point
    /// (`--rootfs-tar`) rather than from its own rootfs stage under the same lock. Set on
    /// the image stage's entry only. An image from an external tree is published and
    /// listed like any other, and is not vouched for as the lock's.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub external_rootfs: bool,
    /// The files the stage consumed from earlier stages, as it consumed them. The image
    /// stage lists the rootfs tar and the bootloader payloads it placed, which is what
    /// binds the image's boot region to recorded bytes.
    #[serde(default, rename = "input", skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<InventoryFile>,
    /// The files the stage published, in the order it published them.
    #[serde(default, rename = "artifact")]
    pub artifacts: Vec<InventoryFile>,
}

/// One file an entry lists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct InventoryFile {
    /// What the file is: one of [`role`]'s names, or a disk image's
    /// [`ArtifactRole::key`].
    pub role: String,
    /// Its file name in the artifact directory.
    pub file: String,
    /// Its size in bytes, as stored.
    pub size: u64,
    /// The lowercase-hex sha256 of its bytes, as stored.
    pub sha256: String,
    /// For a disk image, the disk it holds: the same as the file for a raw `.img`, and
    /// the decompressed bytes for a container. Absent for every other file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disk: Option<DiskDigest>,
}

/// A disk image's own length and digest, whatever container it is stored in.
///
/// A consumer that decompresses a container holds what comes out to this. The check is
/// then against the build's record, not against the bytes the consumer just read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DiskDigest {
    /// The disk's length in bytes.
    pub bytes: u64,
    /// The lowercase-hex sha256 of the disk's bytes.
    pub sha256: String,
}

impl InventoryFile {
    /// Whether this is a raw disk image rather than a compressed container of one. It is
    /// read from the name, which is how the image node names its containers.
    #[must_use]
    pub fn is_raw_disk(&self) -> bool {
        self.disk.is_some() && !self.file.ends_with(".xz") && !self.file.ends_with(".gz")
    }
}

impl StageEntry {
    /// Every file this entry publishes under `role`, in recorded order.
    pub fn files<'a>(&'a self, role: &'a str) -> impl Iterator<Item = &'a InventoryFile> + 'a {
        self.artifacts.iter().filter(move |f| f.role == role)
    }

    /// The one file this entry publishes under `role`.
    #[must_use]
    pub fn file(&self, role: &str) -> Option<&InventoryFile> {
        self.artifacts.iter().find(|f| f.role == role)
    }

    /// The file this entry consumed under `role`.
    #[must_use]
    pub fn input(&self, role: &str) -> Option<&InventoryFile> {
        self.inputs.iter().find(|f| f.role == role)
    }

    /// The disk image this entry publishes for `role` that a reader should take. That is
    /// the raw image where the build kept one, since it reads in place. Otherwise it is
    /// the first container in the build's own preference order.
    #[must_use]
    pub fn disk_image(&self, role: ArtifactRole) -> Option<&InventoryFile> {
        let mut images = self.files(role.key()).filter(|f| f.disk.is_some());
        let first = images.next()?;
        Some(
            std::iter::once(first)
                .chain(images)
                .find(|f| f.is_raw_disk())
                .unwrap_or(first),
        )
    }

    /// The compressed containers this entry publishes for `role`, in the build's
    /// preference order.
    pub fn containers(&self, role: ArtifactRole) -> impl Iterator<Item = &InventoryFile> + '_ {
        self.files(role.key())
            .filter(|f| f.disk.is_some() && !f.is_raw_disk())
    }

    /// Whether this entry's files belong to the lock whose bytes hash to `lock_sha256`.
    ///
    /// # Errors
    ///
    /// Why they do not, phrased for an operator, naming the stage to re-run.
    pub fn of_lock(&self, lock_sha256: &str) -> Result<(), String> {
        if self.lock_sha256 == lock_sha256 {
            return Ok(());
        }
        Err(format!(
            "the {} stage's artifacts were built under a different lock (sha256 {}, the \
             lock is now {}) — run the {} stage again",
            self.stage.as_str(),
            short(&self.lock_sha256),
            short(lock_sha256),
            self.stage.as_str()
        ))
    }
}

impl Inventory {
    /// The entry `stage` last wrote, if it has one.
    #[must_use]
    pub fn entry(&self, stage: Stage) -> Option<&StageEntry> {
        self.stages.iter().find(|e| e.stage == stage)
    }

    /// Drop `stage`'s entry: the stage has started, and until it finishes nothing it
    /// published before is current.
    pub fn remove(&mut self, stage: Stage) {
        self.stages.retain(|e| e.stage != stage);
    }

    /// Record `entry`, replacing its stage's previous one, and keep the entries in stage
    /// order so the document is deterministic.
    pub fn put(&mut self, entry: StageEntry) {
        self.remove(entry.stage);
        self.stages.push(entry);
        self.stages.sort_by_key(|e| e.stage);
    }

    /// Move every entry written under the lock `from` to the lock `to`.
    ///
    /// For a run that rewrites its own lock after the stages finished (`--save-manifest`,
    /// `--save-snapshot`). The new lock pins what the run built, so the run's artifacts
    /// are that lock's. Entries under any other lock keep theirs.
    pub fn restamp(&mut self, from: &str, to: &str) {
        for entry in self.stages.iter_mut().filter(|e| e.lock_sha256 == from) {
            entry.lock_sha256 = to.to_string();
        }
    }

    /// Every file name any entry publishes, the set an artifact directory's own files are.
    pub fn artifact_names(&self) -> impl Iterator<Item = &str> {
        self.stages
            .iter()
            .flat_map(|e| e.artifacts.iter().map(|f| f.file.as_str()))
    }

    /// The inventory as the TOML document `build` writes.
    pub fn to_toml(&self) -> String {
        let body = toml::to_string(self).expect("an inventory always serializes");
        format!(
            "# Written by `boot2deb build`: what each stage of this point last published,\n\
             # under which lock, once its checks passed. A file not listed here is not an\n\
             # artifact of this point.\n{body}"
        )
    }

    /// Parse an inventory. `origin` names the file in an error.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Parse`] for a document that is not an inventory.
    pub fn from_toml_str(text: &str, origin: &str) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: origin.to_string(),
            source,
        })
    }
}

/// The first 12 hex digits of a digest, for messages.
fn short(hex: &str) -> &str {
    hex.get(..12).unwrap_or(hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(role: &str, name: &str, disk: bool) -> InventoryFile {
        InventoryFile {
            role: role.into(),
            file: name.into(),
            size: 10,
            sha256: "a".repeat(64),
            disk: disk.then(|| DiskDigest {
                bytes: 100,
                sha256: "b".repeat(64),
            }),
        }
    }

    fn entry(stage: Stage, lock: &str, artifacts: Vec<InventoryFile>) -> StageEntry {
        StageEntry {
            stage,
            lock_sha256: lock.into(),
            finished: "2026-11-21T00:00:00Z".into(),
            layout: (stage == Stage::Image).then_some(Layout::Combined),
            external_rootfs: false,
            inputs: Vec::new(),
            artifacts,
        }
    }

    /// The document round-trips, and the entries come back in stage order whatever
    /// order they were put in.
    #[test]
    fn an_inventory_round_trips_in_stage_order() {
        let mut inv = Inventory::default();
        let mut image = entry(
            Stage::Image,
            "l",
            vec![file(ArtifactRole::Combined.key(), "p.img.xz", true)],
        );
        image.inputs = vec![file(role::ROOTFS_TAR, "p-rootfs.tar", false)];
        inv.put(image);
        inv.put(entry(
            Stage::Rootfs,
            "l",
            vec![file(role::ROOTFS_TAR, "p-rootfs.tar", false)],
        ));
        assert_eq!(inv.stages[0].stage, Stage::Rootfs);
        let text = inv.to_toml();
        assert!(text.contains("[[stage.artifact.disk]]") || text.contains("[stage.artifact.disk]"));
        let back = Inventory::from_toml_str(&text, "p.artifacts.toml").unwrap();
        assert_eq!(back, inv);
        assert!(Inventory::from_toml_str("[[stage]]\nstage = \"kernel\"\n", "x").is_err());
    }

    /// Putting a stage's entry replaces its previous one, and removing it leaves the
    /// others alone.
    #[test]
    fn a_stage_has_one_entry_and_removing_it_touches_no_other() {
        let mut inv = Inventory::default();
        inv.put(entry(Stage::Uboot, "old", vec![]));
        inv.put(entry(Stage::Rootfs, "old", vec![]));
        inv.put(entry(Stage::Uboot, "new", vec![]));
        assert_eq!(inv.stages.len(), 2);
        assert_eq!(inv.entry(Stage::Uboot).unwrap().lock_sha256, "new");
        inv.remove(Stage::Uboot);
        assert!(inv.entry(Stage::Uboot).is_none());
        assert!(inv.entry(Stage::Rootfs).is_some());
    }

    /// A raw image is the one a reader takes when the build kept it; otherwise the first
    /// container, in the build's preference order. A role the entry lists nothing for
    /// has no image, however a file in the directory is named.
    #[test]
    fn the_preferred_disk_image_is_the_raw_one_then_the_first_container() {
        let combined = ArtifactRole::Combined.key();
        let gz_first = entry(
            Stage::Image,
            "l",
            vec![
                file(combined, "p.img.gz", true),
                file(combined, "p.img.xz", true),
                file(role::FILES_MANIFEST, "p.rootfs.uapi16", false),
            ],
        );
        assert_eq!(
            gz_first.disk_image(ArtifactRole::Combined).unwrap().file,
            "p.img.gz"
        );
        assert_eq!(gz_first.containers(ArtifactRole::Combined).count(), 2);
        assert!(gz_first.disk_image(ArtifactRole::Rootfs).is_none());

        let kept = entry(
            Stage::Image,
            "l",
            vec![
                file(combined, "p.img.xz", true),
                file(combined, "p.img", true),
            ],
        );
        assert_eq!(
            kept.disk_image(ArtifactRole::Combined).unwrap().file,
            "p.img"
        );
        assert_eq!(kept.containers(ArtifactRole::Combined).count(), 1);
    }

    /// An entry is current for the lock it names and no other, and the refusal names
    /// the stage to run again.
    #[test]
    fn an_entry_belongs_to_the_lock_it_was_built_under() {
        let e = entry(Stage::Rootfs, &"1".repeat(64), vec![]);
        assert!(e.of_lock(&"1".repeat(64)).is_ok());
        let err = e.of_lock(&"2".repeat(64)).unwrap_err();
        assert!(
            err.contains("rootfs stage") && err.contains("111111111111"),
            "{err}"
        );
    }

    /// A run that rewrites its lock moves its own entries to the new lock, and only
    /// those.
    #[test]
    fn restamping_moves_only_the_entries_of_the_old_lock() {
        let mut inv = Inventory::default();
        inv.put(entry(Stage::Uboot, "older", vec![]));
        inv.put(entry(Stage::Rootfs, "run", vec![]));
        inv.put(entry(Stage::Image, "run", vec![]));
        inv.restamp("run", "saved");
        assert_eq!(inv.entry(Stage::Uboot).unwrap().lock_sha256, "older");
        assert_eq!(inv.entry(Stage::Rootfs).unwrap().lock_sha256, "saved");
        assert_eq!(inv.entry(Stage::Image).unwrap().lock_sha256, "saved");
    }
}
