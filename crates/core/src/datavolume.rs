//! Data volumes — a second disk the image mounts for data, kept whole across
//! reimaging.
//!
//! The layout this exists for: the whole bootable system (u-boot, kernel, rootfs)
//! on the medium the board's flashing route can actually write, and the large disk
//! carrying nothing the boot depends on. On a compute module in a cluster carrier
//! that is eMMC + an M.2 NVMe, and the property that makes it worth modelling is
//! that **reflashing the OS does not touch the data**: the new image finds the
//! volume already there, by label, and adopts it.
//!
//! That property is also what makes the safety rule non-negotiable. A volume this
//! image did not create is evidence of data someone wants, so the first-boot
//! ladder is: adopt a volume carrying our label, create one only on a genuinely
//! blank disk, and refuse anything else. A feature that reformatted on each boot
//! would destroy exactly the data it exists to preserve.
//!
//! Pure: parsing and validation. The engine writes the resolved list into the
//! rootfs as `/etc/boot2deb/data-volumes.conf`, and the `data-volume` feature's
//! first-boot hook is what reads it and walks the ladder on the board.

use crate::error::ConfigError;
use serde::{Deserialize, Serialize};

/// Longest filesystem label an ext4 volume can carry, in bytes. The label is the
/// volume's identity across reimaging, so a truncated one would silently fail to
/// be re-adopted by the next image.
pub const MAX_LABEL_BYTES: usize = 16;

/// The feature that carries the first-boot hook acting on these declarations. A
/// recipe needs it and at least one [`DataVolume`]; either alone is inert, which
/// [`ConfigError::DataVolumeFeatureMismatch`] reports.
pub const FEATURE: &str = "data-volume";

/// Path the resolved list is written to in the rootfs, and the file the hook
/// reads on the board.
pub const CONFIG_PATH: &str = "etc/boot2deb/data-volumes.conf";

/// One data volume an image mounts, as declared by a recipe.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DataVolume {
    /// Which disk to look at — [`VolumeMatch`].
    #[serde(rename = "match")]
    pub match_: VolumeMatch,
    /// Filesystem label identifying this volume, and the `LABEL=` the generated
    /// fstab entry mounts by.
    ///
    /// This is the volume's identity, not a description: it is what lets a
    /// freshly flashed image recognise the disk it must not touch. Changing it
    /// between images means the next boot sees an unlabelled foreign disk and
    /// refuses it, which is the safe outcome but not the intended one.
    pub label: String,
    /// Filesystem to create on a blank disk, and the type the fstab entry names.
    #[serde(default)]
    pub fstype: VolumeFs,
    /// Absolute path the volume is mounted at. Never `/`.
    pub mount: String,
    /// Whether the first-boot hook may create the volume, or only adopt one that
    /// already exists — [`CreatePolicy`].
    #[serde(default)]
    pub create: CreatePolicy,
}

/// How the first-boot hook finds the disk a volume lives on.
///
/// Deliberately narrow. A general predicate over disks would be a way to point
/// the formatter at the wrong one, and the blank-disk requirement is the only
/// thing standing between a typo here and someone's data.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum VolumeMatch {
    /// The single disk of this transport. Resolution to a device node happens on
    /// the board at first boot; matching more than one disk is refused there
    /// rather than guessed, since "the NVMe" is not an answer when there are two.
    Kind(DiskKind),
    /// An exact device node (`/dev/nvme0n1`). For a board where the transport is
    /// ambiguous and the operator knows which disk is meant.
    Device(String),
}

/// A disk transport, as [`VolumeMatch::Kind`] names it.
///
/// These name the bus, not the device-node spelling, because the spelling does not
/// separate the cases that matter. A SATA disk and a USB disk are both `/dev/sd*`:
/// a board with an internal SSD and a plugged-in USB drive shows two devices no
/// name pattern can tell apart, and picking the wrong one is precisely the accident
/// this type exists to prevent. The hook therefore matches on the kernel's reported
/// transport and refuses a disk whose transport it cannot read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum DiskKind {
    /// NVMe over PCIe (`/dev/nvme<n>n<n>`) — an M.2 or U.2 slot.
    Nvme,
    /// An internally attached SATA disk (`/dev/sd*` with transport `sata`).
    Sata,
    /// A USB-attached disk (`/dev/sd*` with transport `usb`). Distinct from
    /// [`Sata`](Self::Sata) despite sharing the node name: they are told apart by
    /// transport alone, so conflating them would mean a removable drive could stand
    /// in for a fixed one.
    Usb,
    /// An SD or eMMC device (`/dev/mmcblk<n>`) other than the one holding root.
    ///
    /// The name pattern excludes `mmcblk<n>boot<n>`, the read-only eMMC boot
    /// hardware partitions, which the kernel also presents as whole disks.
    Mmc,
}

impl DiskKind {
    /// The `lsblk -o TRAN` value a disk of this kind reports.
    ///
    /// The hook requires this to match before considering a disk, so a device whose
    /// transport is unreadable is skipped rather than guessed at — except for
    /// [`Mmc`](Self::Mmc), where the block driver commonly reports nothing and the
    /// `mmcblk<n>` name is already unambiguous.
    pub fn transport(self) -> &'static str {
        match self {
            DiskKind::Nvme => "nvme",
            DiskKind::Sata => "sata",
            DiskKind::Usb => "usb",
            DiskKind::Mmc => "mmc",
        }
    }

    /// The config spelling, and the token written into the generated config.
    pub fn as_str(self) -> &'static str {
        match self {
            DiskKind::Nvme => "nvme",
            DiskKind::Sata => "sata",
            DiskKind::Usb => "usb",
            DiskKind::Mmc => "mmc",
        }
    }
}

/// Filesystem a created volume gets.
///
/// One value today. It is an enum rather than a bare string so the fstab type and
/// the `mkfs` the hook runs cannot drift apart, and so adding a filesystem is a
/// change with one place to make it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VolumeFs {
    /// ext4. The default, and what the rootfs already uses.
    #[default]
    Ext4,
}

impl VolumeFs {
    /// The `/etc/fstab` type field.
    pub fn fstab_type(self) -> &'static str {
        match self {
            VolumeFs::Ext4 => "ext4",
        }
    }
}

/// Whether first boot may *create* a volume, or only adopt an existing one.
///
/// Neither value permits touching a disk that already holds something: adopting a
/// labelled volume and refusing foreign content are unconditional, and this only
/// decides what happens to a disk that is genuinely blank.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum CreatePolicy {
    /// Partition and format a blank disk, then mount it. The default: an image
    /// flashed to a board with a new disk comes up ready.
    #[default]
    IfBlank,
    /// Never write a partition table or a filesystem. A volume prepared by hand
    /// is still adopted and mounted; a blank disk is left blank and logged.
    Never,
}

impl DataVolume {
    /// Validate one declaration, in isolation.
    ///
    /// Checks what is knowable without a board: the mount path is absolute and is
    /// not root, and the label is non-empty, fits [`MAX_LABEL_BYTES`], and holds
    /// nothing that would need quoting in `/etc/fstab` or in the generated
    /// config's tab-separated lines.
    pub fn validate(&self) -> Result<(), ConfigError> {
        if !self.mount.starts_with('/') {
            return Err(ConfigError::DataVolumeMount {
                mount: self.mount.clone(),
                why: "must be an absolute path".into(),
            });
        }
        if self.mount == "/" {
            return Err(ConfigError::DataVolumeMount {
                mount: self.mount.clone(),
                why: "is the root filesystem, which the image itself provides".into(),
            });
        }
        if self.mount.len() > 1 && self.mount.ends_with('/') {
            return Err(ConfigError::DataVolumeMount {
                mount: self.mount.clone(),
                why: "must not have a trailing slash".into(),
            });
        }
        if self.label.is_empty() {
            return Err(ConfigError::DataVolumeLabel {
                label: self.label.clone(),
                why: "must not be empty — the label is how the next image re-adopts \
                      the volume instead of refusing it"
                    .into(),
            });
        }
        if self.label.len() > MAX_LABEL_BYTES {
            return Err(ConfigError::DataVolumeLabel {
                label: self.label.clone(),
                why: format!(
                    "is {} bytes; an ext4 label holds at most {MAX_LABEL_BYTES}, and a \
                     truncated one would not be recognised on the next boot",
                    self.label.len()
                ),
            });
        }
        if !self
            .label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        {
            return Err(ConfigError::DataVolumeLabel {
                label: self.label.clone(),
                why: "may hold only ASCII letters, digits, '-', '_' and '.' — it is \
                      written unquoted into /etc/fstab"
                    .into(),
            });
        }
        if let VolumeMatch::Device(dev) = &self.match_ {
            if !dev.starts_with("/dev/") {
                return Err(ConfigError::DataVolumeMatch {
                    value: dev.clone(),
                    why: "must be a device node under /dev/".into(),
                });
            }
        }
        Ok(())
    }

    /// The `/etc/fstab` line for this volume.
    ///
    /// Mounted by `LABEL=`, never by device node or PARTUUID: the label is the one
    /// identifier that survives the disk moving to another slot, and the whole
    /// point of the volume is to outlive the image that created it.
    ///
    /// `nofail` and a short device timeout are not optional. Without them a board
    /// whose data disk is absent, dead, or not yet enumerated stops in the
    /// initramfs or drops to emergency mode — turning a missing *data* disk into
    /// an unbootable system, which inverts the entire point of keeping the OS on
    /// its own medium.
    pub fn fstab_line(&self) -> String {
        format!(
            "LABEL={}\t{}\t{}\tdefaults,nofail,x-systemd.device-timeout=10s\t0 2",
            self.label,
            self.mount,
            self.fstype.fstab_type()
        )
    }
}

/// Validate a recipe's whole list: each entry on its own, plus the pairwise
/// checks that only make sense across entries.
///
/// Two volumes sharing a label or a mount point is a configuration that cannot do
/// what it says — the second would adopt or shadow the first — so it is rejected
/// here rather than producing a board where one of them silently loses.
pub fn validate_all(volumes: &[DataVolume]) -> Result<(), ConfigError> {
    for v in volumes {
        v.validate()?;
    }
    for (i, v) in volumes.iter().enumerate() {
        for other in &volumes[i + 1..] {
            if v.label == other.label {
                return Err(ConfigError::DataVolumeLabel {
                    label: v.label.clone(),
                    why: "is declared twice; a label identifies one volume".into(),
                });
            }
            if v.mount == other.mount {
                return Err(ConfigError::DataVolumeMount {
                    mount: v.mount.clone(),
                    why: "is declared twice; two volumes cannot share a mount point".into(),
                });
            }
        }
    }
    Ok(())
}

/// Render the resolved list as the on-device config at [`CONFIG_PATH`].
///
/// Tab-separated fields, one volume per line, in declaration order. The format is
/// deliberately dumb: the reader is a `/bin/sh` first-boot hook, and every field
/// has already been validated to hold no whitespace or separator, so it needs no
/// quoting rules and no parser.
///
/// An empty list still renders the header. A present-but-empty file says the image
/// was built with the feature and had nothing to mount, which is a different fact
/// from the file being absent, and the hook logs it as such rather than exiting
/// silently.
pub fn render_config(volumes: &[DataVolume]) -> String {
    let mut out = String::from(
        "# Generated by boot2deb; read once by the data-volume first-boot hook.\n\
         # <match>\t<label>\t<fstype>\t<mount>\t<create>\n",
    );
    for v in volumes {
        let match_ = match &v.match_ {
            VolumeMatch::Kind(k) => k.as_str().to_string(),
            VolumeMatch::Device(dev) => dev.clone(),
        };
        let create = match v.create {
            CreatePolicy::IfBlank => "if-blank",
            CreatePolicy::Never => "never",
        };
        out.push_str(&format!(
            "{match_}\t{}\t{}\t{}\t{create}\n",
            v.label,
            v.fstype.fstab_type(),
            v.mount
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn volume(label: &str, mount: &str) -> DataVolume {
        DataVolume {
            match_: VolumeMatch::Kind(DiskKind::Nvme),
            label: label.into(),
            fstype: VolumeFs::Ext4,
            mount: mount.into(),
            create: CreatePolicy::IfBlank,
        }
    }

    #[test]
    fn a_well_formed_volume_validates() {
        assert!(volume("b2d-data", "/srv").validate().is_ok());
    }

    #[test]
    fn the_mount_must_be_absolute_and_not_root() {
        assert!(volume("d", "srv").validate().is_err());
        assert!(volume("d", "/").validate().is_err());
        assert!(volume("d", "/srv/").validate().is_err());
        // A nested path is fine; only a trailing slash is not.
        assert!(volume("d", "/srv/media").validate().is_ok());
    }

    #[test]
    fn the_label_must_survive_a_reimage() {
        // Empty or over-long labels are the two ways the next image fails to
        // recognise its own volume, so both are rejected at config time.
        assert!(volume("", "/srv").validate().is_err());
        assert!(volume(&"x".repeat(MAX_LABEL_BYTES), "/srv")
            .validate()
            .is_ok());
        assert!(volume(&"x".repeat(MAX_LABEL_BYTES + 1), "/srv")
            .validate()
            .is_err());
    }

    #[test]
    fn a_label_needing_quotes_is_rejected() {
        // The label is written unquoted into fstab and into the tab-separated
        // generated config, so whitespace and separators cannot appear in it.
        assert!(volume("has space", "/srv").validate().is_err());
        assert!(volume("has\ttab", "/srv").validate().is_err());
        assert!(volume("ok-name_1.2", "/srv").validate().is_ok());
    }

    #[test]
    fn sata_and_usb_are_distinct_kinds_with_distinct_transports() {
        // They share the /dev/sd* name, so the transport is the only thing that
        // separates an internal disk from one somebody plugged in. If these ever
        // collapsed to one value, a removable drive could stand in for a fixed one.
        assert_ne!(DiskKind::Sata.transport(), DiskKind::Usb.transport());
        assert_eq!(DiskKind::Sata.transport(), "sata");
        assert_eq!(DiskKind::Usb.transport(), "usb");
        assert_eq!(DiskKind::Nvme.transport(), "nvme");
        // The config spelling is what the hook reads back out of the generated file,
        // so it has to round-trip through the same names the TOML uses.
        for (kind, spelling) in [
            (DiskKind::Nvme, "nvme"),
            (DiskKind::Sata, "sata"),
            (DiskKind::Usb, "usb"),
            (DiskKind::Mmc, "mmc"),
        ] {
            assert_eq!(kind.as_str(), spelling);
            let toml =
                format!("match = {{ kind = \"{spelling}\" }}\nlabel = \"d\"\nmount = \"/srv\"\n");
            let parsed: DataVolume = toml::from_str(&toml).expect(spelling);
            assert_eq!(parsed.match_, VolumeMatch::Kind(kind));
        }
    }

    #[test]
    fn an_exact_device_must_be_under_dev() {
        let mut v = volume("d", "/srv");
        v.match_ = VolumeMatch::Device("nvme0n1".into());
        assert!(v.validate().is_err());
        v.match_ = VolumeMatch::Device("/dev/nvme0n1".into());
        assert!(v.validate().is_ok());
    }

    #[test]
    fn two_volumes_cannot_share_a_label_or_a_mount() {
        assert!(validate_all(&[volume("a", "/one"), volume("b", "/two")]).is_ok());
        assert!(validate_all(&[volume("same", "/one"), volume("same", "/two")]).is_err());
        assert!(validate_all(&[volume("a", "/same"), volume("b", "/same")]).is_err());
    }

    #[test]
    fn the_config_is_tab_separated_in_declaration_order() {
        let mut second = volume("media", "/srv/media");
        second.match_ = VolumeMatch::Device("/dev/nvme0n1".into());
        second.create = CreatePolicy::Never;
        let out = render_config(&[volume("b2d-data", "/srv"), second]);
        let lines: Vec<&str> = out.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(
            lines,
            vec![
                "nvme\tb2d-data\text4\t/srv\tif-blank",
                "/dev/nvme0n1\tmedia\text4\t/srv/media\tnever",
            ]
        );
    }

    #[test]
    fn an_empty_list_still_renders_a_header() {
        // Present-and-empty and absent are different facts on the board.
        let out = render_config(&[]);
        assert!(out.starts_with('#'));
        assert!(out.lines().all(|l| l.starts_with('#')));
    }

    #[test]
    fn the_fstab_line_mounts_by_label_and_never_blocks_boot() {
        let line = volume("b2d-data", "/srv").fstab_line();
        assert!(line.starts_with("LABEL=b2d-data\t/srv\text4\t"), "{line}");
        // A missing data disk must not make the system unbootable.
        assert!(line.contains("nofail"), "{line}");
        // fsck pass 2: checked, but after root.
        assert!(line.ends_with("\t0 2"), "{line}");
    }
}
