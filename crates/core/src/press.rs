//! What a build point presses: the image-artifact roles a resolved build
//! produces, and the seed-partition constants shared by the image node and the
//! press path.
//!
//! Pure and deterministic — a total mapping from the resolved boot method,
//! layout, and deliverable onto the ordered list of image files a build has,
//! unit-tested without touching a disk. Deriving the roles from the
//! [`ResolvedBuild`] rather than from flags is what makes pressing a `split`
//! build to one output path a resolution-shaped error naming both artifacts,
//! instead of prose in a board page.
//!
//! The mapping is small because resolution has already narrowed the space: a
//! depthcharge build cannot split (its kernel slots live inside the image's own
//! GPT), so a split role list is always `rockchip-rkbin`, and a
//! [`Deliverable::Uboot`](crate::model::Deliverable::Uboot) build produces
//! exactly the bootloader image a split layout's boot half is.

use crate::model::{Layout, ResolvedBuild};

/// Size of the per-unit seed partition every GPT-bearing image carries: a FAT12
/// volume holding `seed.txt`, regenerated whole by `press` and `seed` — 1 MiB,
/// which is roomy for a hostname and a few keys and small enough to rebuild in
/// memory.
///
/// One number for the geometry that reserves it, the generator that fills it,
/// and the writer that replaces it, so the three cannot disagree. A multiple of
/// every alignment in play (sectors, ext4 blocks), which is what lets the
/// geometry place it by subtraction from an already-aligned offset.
pub const SEED_PARTITION_BYTES: u64 = 1 << 20;

/// The GPT entry label of the seed partition — how the device's first-boot hook
/// finds it (`/dev/disk/by-partlabel/b2d-seed`) and how `boot2deb seed` locates
/// it in an already-pressed image file.
pub const SEED_PARTLABEL: &str = "b2d-seed";

/// Which of a build's image artifacts one pressed output derives from.
///
/// The role also names the artifact on disk: a build's images share one stem and
/// differ only in the suffix this role carries ([`file_name`](Self::file_name)).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ArtifactRole {
    /// The whole-disk image of a `combined` build — boot payload and rootfs on one
    /// medium.
    Combined,
    /// The bootloader-only image of a `split` build or a u-boot deliverable: the
    /// medium the board's boot ROM reads (eMMC or SPI), with no OS on it.
    Boot,
    /// The bootloader-agnostic rootfs image of a `split` build, for a disk of its
    /// own (NVMe, USB, SD — whatever the board boots its OS from).
    Rootfs,
}

impl ArtifactRole {
    /// The artifact's file name for a build with this `stem`, without the
    /// compression extension — the image node's own naming, stated once here so the
    /// press side cannot drift from it.
    #[must_use]
    pub fn file_name(self, stem: &str) -> String {
        match self {
            ArtifactRole::Combined => format!("{stem}.img"),
            ArtifactRole::Boot => format!("{stem}-boot.img"),
            ArtifactRole::Rootfs => format!("{stem}-rootfs.img"),
        }
    }

    /// What this artifact is, for messages: `"combined image"`, `"boot image"`,
    /// `"rootfs image"`.
    #[must_use]
    pub fn describe(self) -> &'static str {
        match self {
            ArtifactRole::Combined => "combined image",
            ArtifactRole::Boot => "boot image",
            ArtifactRole::Rootfs => "rootfs image",
        }
    }

    /// Whether this artifact carries the rootfs — and with it the seed partition
    /// and everything post-hoc customization can touch. A boot image carries
    /// neither a GPT nor a filesystem, so seed keys and tree additions do not
    /// apply to it.
    #[must_use]
    pub fn carries_rootfs(self) -> bool {
        !matches!(self, ArtifactRole::Boot)
    }
}

/// The image artifacts a resolved build produces, in output order.
///
/// Total over everything resolution can produce — every boot method, layout, and
/// deliverable — so `press` cannot meet a build it has no answer for:
///
/// - a u-boot deliverable is its standalone boot image;
/// - a `combined` build is one whole-disk image;
/// - a `split` build is its boot image plus its rootfs image, boot half first —
///   the order the split image node emits them.
///
/// One role means the single positional output path names it; two mean the
/// caller must name both outputs (`--boot-out` + `--rootfs-out`), which is what
/// turns "two artifacts, two files" from prose into an error.
#[must_use]
pub fn roles(build: &ResolvedBuild) -> Vec<ArtifactRole> {
    if !build.produces_image() {
        // A u-boot deliverable: exactly the boot image a split layout emits.
        return vec![ArtifactRole::Boot];
    }
    match build.layout {
        Layout::Combined => vec![ArtifactRole::Combined],
        // Resolution refuses a depthcharge split, so a split role list is always
        // rockchip-rkbin: the boot image for the eMMC/SPI medium, the rootfs for
        // a disk of its own.
        Layout::Split => vec![ArtifactRole::Boot, ArtifactRole::Rootfs],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roles_name_the_image_nodes_artifacts() {
        assert_eq!(
            ArtifactRole::Combined.file_name("rk1-forky"),
            "rk1-forky.img"
        );
        assert_eq!(
            ArtifactRole::Boot.file_name("rk1-forky"),
            "rk1-forky-boot.img"
        );
        assert_eq!(
            ArtifactRole::Rootfs.file_name("rk1-forky"),
            "rk1-forky-rootfs.img"
        );
    }

    #[test]
    fn only_the_boot_image_carries_no_rootfs() {
        assert!(ArtifactRole::Combined.carries_rootfs());
        assert!(ArtifactRole::Rootfs.carries_rootfs());
        assert!(!ArtifactRole::Boot.carries_rootfs());
    }
}
