//! The image node — unprivileged image assembly with no loop mount, no `dd`,
//! and no `sudo`.
//!
//! It takes a rootfs tarball plus the boot method's payload and writes a
//! bootable disk image with no `sudo`, no loop device, and no mount: the ext4
//! filesystem is formatted in-process by the pure-Rust `ferrosys` formatter, straight
//! from the rootfs tar (the `ext4` submodule), the partition table is written
//! in Rust (`gpt`), the boot payload is placed by seek+write, and the result is
//! compressed with pure-Rust encoders — `.xz` via `lzma-rust2`, `.gz` via `flate2`
//! ([`ImageCompression`]). All byte/LBA arithmetic is resolved and validated up front
//! by the `geometry` submodule.
//!
//! **Where the boot payload comes from is the boot method's business.** Under
//! `rockchip-rkbin` it is two blobs the u-boot stage compiled, written into a raw gap
//! outside any partition. Under `depthcharge` it is one vboot-signed kernel FIT that
//! `depthchargectl` built *inside the rootfs*, which this node reads back out of the
//! tarball and places in a ChromeOS kernel partition (the `depthcharge` submodule).
//!
//! Two layouts, selected by the resolved [`Layout`]:
//! - **combined** — one image, boot payload and rootfs on a single medium.
//! - **split** — a bootloader-only image for the boot medium (eMMC/SPI) plus a
//!   bootloader-agnostic rootfs image for a separate disk; mainline u-boot's
//!   distro-boot discovers the rootfs at runtime, so both share one rootfs build.
//!   Only `rockchip-rkbin` has a bootloader to split off; resolution rejects the
//!   combination for any method that does not.

mod depthcharge;
mod ext4;
mod geometry;
mod gpt;
pub mod inspect;

pub use ext4::ROOTFS_FS_KIND;

use crate::error::EngineError;
use crate::event::{EventSink, Step};
use crate::press::additions::TreeAdditions;
use boot2deb_core::chromeos::MAX_KPART_SLOTS;
use boot2deb_core::model::{Layout, ResolvedBoot};
use boot2deb_core::press::ArtifactRole;
use boot2deb_core::provenance::FilesystemProvenance;
use boot2deb_core::size::{parse_image_size, ImageSize};
use boot2deb_core::{ImageBuild, ResolvedBuild};
use geometry::{BootGeometry, Geometry};
use lzma_rust2::{XzOptions, XzWriterMt};
use sha2::{Digest, Sha256};
use std::io::{Read, Seek, SeekFrom, Write};
use std::num::NonZeroU64;
use std::path::{Path, PathBuf};
use uuid::Uuid;

/// `.xz` compression preset. Level 6 is the `xz(1)` default — a balanced size/time
/// point; `lzma-rust2` matches liblzma from level 4 up.
const XZ_PRESET: u32 = 6;

/// `.gz` compression level. 6 is the `gzip(1)` default, chosen for the same
/// size/time balance as [`XZ_PRESET`].
const GZ_LEVEL: u32 = 6;

/// A container a finished image is compressed into.
///
/// The two are not interchangeable, and each has a distinct reason to exist: `.xz`
/// is the smallest and is what an operator pipes through `xzcat` into `dd`; `.gz`
/// exists because **u-boot has no xz decompressor** — its `gzwrite` command reads
/// gzip only — so an image meant to be written to a disk by the bootloader itself
/// has to be in that container.
///
/// Size and speed run in opposite directions: `.xz` is the smaller and the slower
/// to produce, `.gz` the larger and the faster.
///
/// [`ImageOptions::compress`] is an ordered preference, not a set: the first
/// format a build asks for is the one the finished-build hint points at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageCompression {
    /// `.xz`, via the pure-Rust multithreaded `lzma-rust2` encoder. The default,
    /// and the smaller artifact.
    Xz,
    /// `.gz`, via the pure-Rust `miniz_oxide` backend of `flate2`. Single-threaded
    /// (the format has no parallel container the way `.xz` blocks do), and a worse
    /// ratio than `.xz` — its reason to exist is that u-boot can read it.
    Gz,
}

impl ImageCompression {
    /// The suffix appended to the raw `<stem>.img`, without the dot.
    pub fn extension(self) -> &'static str {
        match self {
            ImageCompression::Xz => "xz",
            ImageCompression::Gz => "gz",
        }
    }

    /// The host command that streams this container back to raw bytes on stdout,
    /// for the `dd` pipe in a finished build's hint.
    pub fn decompressor(self) -> &'static str {
        match self {
            ImageCompression::Xz => "xzcat",
            ImageCompression::Gz => "zcat",
        }
    }
}

impl std::str::FromStr for ImageCompression {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "xz" => Ok(ImageCompression::Xz),
            "gz" | "gzip" => Ok(ImageCompression::Gz),
            other => Err(format!(
                "unknown image compression `{other}` (want xz or gz)"
            )),
        }
    }
}

impl std::fmt::Display for ImageCompression {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.extension())
    }
}

/// One compressed artifact, and the raw image it came from.
///
/// A build may ask for more than one container, so this names its `source`
/// rather than relying on position: with two formats there is no longer one
/// compressed file per raw image to pair index-wise.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressedImage {
    /// The raw image this was compressed from — one of [`ImageOutput::images`].
    pub source: PathBuf,
    /// The compressed file on disk.
    pub path: PathBuf,
    /// Which container [`path`](Self::path) is in.
    pub format: ImageCompression,
}

/// Per-block size for the multithreaded `.xz` encoder. Blocks are the unit of
/// parallelism (one worker per block) and of the seekable index, so an
/// image-sized input splits into many blocks across cores. 32 MiB comfortably
/// exceeds the preset-6 dictionary (8 MiB), so the ratio hit from blocking is
/// negligible while a multi-GiB image still parallelizes well.
const XZ_BLOCK_SIZE: u64 = 32 * 1024 * 1024;

/// Where the image's boot payload comes from, per boot method.
#[derive(Debug, Clone, Copy)]
pub enum BootPayload<'a> {
    /// `rockchip-rkbin`: the two raw-gap payloads the u-boot stage compiled
    /// ([`UbootArtifacts`](crate::build::uboot::UbootArtifacts)).
    RockchipRkbin {
        /// `idbloader.img`.
        idbloader: &'a Path,
        /// `u-boot.itb`.
        uboot_itb: &'a Path,
    },
    /// `depthcharge`: the signed kernel FIT, which carries no path because it is not
    /// produced by a compile stage at all — `depthchargectl` built it *inside the
    /// rootfs*, so the image node reads it out of the rootfs tarball (see the
    /// `depthcharge` submodule for why that is the right place for it to be built).
    Depthcharge,
}

/// Filesystem inputs for the image node.
pub struct ImageOptions<'a> {
    /// Rootfs as a `tar` archive — the artifact of the rootfs backend, staged
    /// and formatted by the `ext4` submodule. Device nodes under `./dev/` are not
    /// materialized (the kernel mounts devtmpfs over `/dev` at boot). Under
    /// `depthcharge` it also carries the signed kernel partition image.
    pub rootfs_tar: &'a Path,
    /// The boot payload to place, per the resolved boot method.
    pub boot: BootPayload<'a>,
    /// Directory the finished image(s) are written to.
    pub out_dir: &'a Path,
    /// The build point's
    /// [artifact stem](boot2deb_core::buildpoint::BuildPoint::artifact_stem) — the
    /// finished images are `<stem>.img`, `<stem>-boot.img`, `<stem>-rootfs.img`. Named
    /// for the point rather than the board because a board has several recipes and
    /// only one of their images can hold a given file name.
    pub stem: &'a str,
    /// Scratch directory for the intermediate ext4 partition image.
    pub work_dir: &'a Path,
    /// ext4 volume label and GPT partition name (≤ 16 bytes), e.g. `rootfs`.
    pub rootfs_label: &'a str,
    /// The image's deterministic on-disk identifiers ([`ImageIdentity`]).
    pub identity: ImageIdentity,
    /// Containers to emit alongside each raw image, in preference order — the
    /// first is what the finished-build hint points an operator at. Empty leaves
    /// the raw `.img` as the only output.
    ///
    /// Ordered rather than a set because the two formats serve different
    /// consumers: see [`ImageCompression`].
    pub compress: &'a [ImageCompression],
    /// Keep the raw `.img` after compressing it. Default (`false`): with
    /// compression on, the raw image is derivable from any of its containers, so
    /// it is deleted once every requested one is written, to save disk on the
    /// largest artifact. Ignored when [`compress`](Self::compress) is empty.
    pub keep_raw: bool,
    /// Upper bound on the `.xz` encoder's worker pool — the build's
    /// [`jobs`](crate::build::BuildEnv::jobs). `None` uses the host's available
    /// parallelism.
    ///
    /// Compression is the one image-node step with real concurrency, so `--jobs N`
    /// has to reach it: a flag that bounds the compile and then fans the encode
    /// across every core does not mean what it says on a shared machine.
    pub jobs: Option<usize>,
}

/// The image's on-disk identifiers, all derived from one lock-stable seed rather
/// than drawn from `/dev/urandom` — so a rebuild from the same lock reproduces them,
/// which is the reproducibility contract, while distinct recipes (or devices) still
/// get distinct values.
///
/// It is computed **once, by the caller**, and shared by the rootfs and image nodes,
/// because under `depthcharge` the rootfs's own `/etc/fstab` has to name the
/// partition the signed kernel will root on. That makes the rootfs PARTUUID an input
/// to the rootfs, not an output of the partition table — the one identifier that must
/// be known before the filesystem that references it exists.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageIdentity {
    /// The ext4 superblock UUID of the rootfs filesystem.
    pub ext4_uuid: Uuid,
    /// The GPT header's disk GUID.
    pub disk_guid: Uuid,
    /// The rootfs partition's GUID — its **PARTUUID**.
    pub rootfs_partuuid: Uuid,
    /// The seed partition's GUID. Its leading four bytes double as the seed
    /// FAT's volume serial, so the volume needs no identity source of its own
    /// and a `--seed-only` rewrite (which reads the GUID back off the GPT)
    /// reproduces the same serial.
    pub seed_partuuid: Uuid,
    /// The ChromeOS kernel slots' partition GUIDs, in on-disk order. Unused under a
    /// boot method that writes no kernel partition.
    ///
    /// **Distinct per slot, and that is load-bearing.** depthcharge substitutes the
    /// booted slot's PARTUUID into `kern_guid=` on the kernel command line, which is
    /// how the running system knows *which* slot it came up from — and therefore which
    /// slot it may safely overwrite on the next kernel upgrade. Two slots sharing a
    /// GUID would make that answer ambiguous, and the upgrade could overwrite the
    /// kernel it is running.
    pub kpart_guids: [Uuid; MAX_KPART_SLOTS as usize],
}

impl ImageIdentity {
    /// Derive every identifier from a lock-stable `seed` and the `device`.
    ///
    /// `seed` identifies the build point (the recipe), so two images of the same
    /// recipe reproduce each other and two different recipes — `asus-c201/forky` and
    /// `asus-c201/trixie`, say — never collide on a PARTUUID, which would make two
    /// cards indistinguishable to a kernel that has both in front of it.
    pub fn derive(seed: &str, device: &str) -> Self {
        ImageIdentity {
            ext4_uuid: derive_uuid(seed, device, "ext4-rootfs"),
            disk_guid: derive_uuid(seed, device, "gpt-disk"),
            rootfs_partuuid: derive_uuid(seed, device, "gpt-partition"),
            seed_partuuid: derive_uuid(seed, device, "gpt-seed-partition"),
            // A distinct domain per slot, so the slots never collide with each other
            // (see the field's own note on why that matters), while each stays a pure
            // function of the seed.
            kpart_guids: std::array::from_fn(|i| {
                derive_uuid(seed, device, &format!("gpt-kernel-partition-{i}"))
            }),
        }
    }

    /// The seed FAT's volume serial: the [`seed_partuuid`](Self::seed_partuuid)'s
    /// leading four bytes, read the way `flash --seed-only` re-derives it from
    /// the GPT it finds on the medium.
    #[must_use]
    pub fn seed_volume_id(&self) -> u32 {
        u32::from_le_bytes(
            self.seed_partuuid.as_bytes()[..4]
                .try_into()
                .expect("a UUID has 16 bytes"),
        )
    }
}

/// The image artifact(s) produced, per the resolved [`Layout`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageOutput {
    /// One whole-disk image with the bootloader in the raw gap.
    Combined {
        /// The `<stem>.img` file.
        image: PathBuf,
    },
    /// Separate bootloader and rootfs images for a two-medium install.
    Split {
        /// `<stem>-boot.img` — raw bootloader payloads for the boot medium.
        bootloader: PathBuf,
        /// `<stem>-rootfs.img` — GPT + rootfs partition, bootloader-agnostic.
        rootfs: PathBuf,
    },
}

impl ImageOutput {
    /// The raw image files, in a stable order — the inputs to compression, and the
    /// order a consumer walks them in. Each is named as the `source` of any
    /// [`CompressedImage`] made from it.
    pub fn images(&self) -> Vec<&Path> {
        match self {
            ImageOutput::Combined { image } => vec![image],
            ImageOutput::Split { bootloader, rootfs } => vec![bootloader, rootfs],
        }
    }
}

/// What [`build_image`] produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImageArtifacts {
    /// The raw image(s), per layout. When [`raw_removed`](Self::raw_removed) is
    /// true these paths no longer exist on disk (compressed, then deleted).
    pub output: ImageOutput,
    /// The compressed artifacts — one per (raw image × requested container),
    /// grouped by image in [`ImageOutput::images`] order and, within an image, in
    /// the order the containers were requested. Empty when compression was off.
    pub compressed: Vec<CompressedImage>,
    /// Whether the raw image files were deleted after compression, so a
    /// consumer knows only the compressed forms remain.
    pub raw_removed: bool,
    /// The per-image first-boot password spliced into [`crate::rootfs::DEFAULT_USER`]'s
    /// account — unique per build, expired so it must be changed at first
    /// login. The caller surfaces it and records it in the provenance manifest; it
    /// is written to no committed file.
    pub password: String,
    /// The checks the rootfs filesystem passed before it shipped, in the order they
    /// ran. Reported rather than re-probed by the caller: one of them is present only
    /// where the build host carries `e2fsprogs`, so the record has to come from the
    /// node that actually ran it. Recorded in the provenance manifest's
    /// `[verification]`.
    pub rootfs_verified_with: Vec<String>,
    /// The on-disk contract the rootfs filesystem was formatted to, and the geometry
    /// that came out. Reported for the same reason as the checks above: the geometry is
    /// a function of the image's size as well as of the formatter's settings, so it
    /// cannot be computed without repeating the format. Recorded in the provenance
    /// manifest's `[filesystem]`.
    pub rootfs_filesystem: FilesystemProvenance,
    /// The whole-disk size this build laid out, in bytes.
    ///
    /// Reported rather than re-parsed from the recipe because under a fitted
    /// `image_size` the recipe does not carry it: the format decided how large the
    /// rootfs is and the disk was sized around the answer, so this node is the only
    /// place the number exists. Recorded in the provenance manifest's `[image]`.
    pub image_bytes: u64,
}

/// Validate the resolved build's image geometry (offsets, size, GPT/rootfs fit)
/// without writing anything — the cheap up-front check `build` runs right after
/// resolution so a bad layout fails before any stage compiles.
///
/// A fitted `image_size` has no disk size to check: the rootfs decides it, and the rootfs
/// does not exist until several stages later. What *is* checkable now is the slack spec
/// and the head of the disk — which is where a mis-authored offset lives, and the whole
/// of what this check was ever catching for an authored size beyond the size itself.
///
/// A u-boot deliverable has neither a size nor a rootfs, so only the boot region is
/// resolved: that is the whole of the disk it writes.
pub fn validate_geometry(build: &ResolvedBuild) -> Result<(), EngineError> {
    let Some(image) = build.image.as_ref() else {
        return geometry::BootRegion::resolve(&build.boot).map(|_| ());
    };
    match parse_image_size(&image.image_size)? {
        ImageSize::Fixed(total) => Geometry::resolve(&build.boot, total).map(|_| ()),
        ImageSize::Fit(slack) => {
            geometry::fit_slack(&image.image_size, slack)?;
            geometry::BootRegion::resolve(&build.boot).map(|_| ())
        }
    }
}

/// Derive a deterministic, RFC-4122-shaped UUID for one image identifier from the
/// lock `seed`, the `device`, and a per-purpose `domain` tag.
///
/// The identifier is a function of the locked build, so a rebuild reproduces it
/// — no `/dev/urandom`. `domain` separates the ext4 UUID from the two GPT
/// GUIDs (a shared seed must not collapse them into one value); `device` keeps two
/// boards' images distinct. The 16-byte SHA-256 prefix is stamped with the
/// version-4/variant bits so the result is a well-formed UUID any tool accepts,
/// while remaining fully determined by the inputs.
fn derive_uuid(seed: &str, device: &str, domain: &str) -> Uuid {
    let mut hasher = Sha256::new();
    // NUL separators keep the fields unambiguous — no concatenation collision
    // between e.g. ("ab", "c") and ("a", "bc").
    hasher.update(domain.as_bytes());
    hasher.update([0u8]);
    hasher.update(device.as_bytes());
    hasher.update([0u8]);
    hasher.update(seed.as_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    // `from_random_bytes` sets the version-4 + RFC-4122 variant nibbles; the input
    // is our hash, not randomness, so the UUID stays fully determined by the seed.
    uuid::Builder::from_random_bytes(bytes).into_uuid()
}

/// Assemble the disk image(s) for `build`, emitting the image step's
/// [`Event`](crate::event::Event)s to `sink`.
///
/// Resolves and validates the geometry, formats the rootfs ext4 partition once
/// (shared by both layouts), then writes the layout the device resolved to.
/// The boot payload's size is checked against the space the geometry gave it
/// before any bytes are placed.
pub fn build_image(
    ib: ImageBuild,
    opts: &ImageOptions,
    sink: &dyn EventSink,
) -> Result<ImageArtifacts, EngineError> {
    let ImageBuild { build, image } = ib;
    let step = Step::start(sink, "image");
    let size = parse_image_size(&image.image_size)?;
    // The head of the disk resolves first and on its own, because under a fitted size the
    // rest of the layout answers to the filesystem rather than the other way round: the
    // format decides how large the rootfs is, and the disk is then sized to carry it. The
    // boot region is the half that is the same either way, and it is the half the payload
    // check below asks about.
    let region = geometry::BootRegion::resolve(&build.boot)?;
    std::fs::create_dir_all(opts.out_dir).map_err(|s| EngineError::io(opts.out_dir, s))?;
    std::fs::create_dir_all(opts.work_dir).map_err(|s| EngineError::io(opts.work_dir, s))?;

    // Resolve the boot payload to concrete bytes. Under depthcharge that means
    // taking the signed kernel out of the rootfs tarball, and checking it is one
    // *this* image can boot — its cmdline must root on the partition this image is
    // about to write, and that cannot be repaired later because it is signed.
    let kpart = match opts.boot {
        BootPayload::Depthcharge => {
            let kpart = depthcharge::extract_kpart(opts.rootfs_tar, opts.work_dir, &step)?;
            depthcharge::verify_kpart(&kpart, opts.identity.rootfs_partuuid)?;
            step.log(format!(
                "verified the signed kernel partition ({} bytes) roots on PARTUUID={}",
                file_len(&kpart)?,
                opts.identity.rootfs_partuuid
            ));
            Some(kpart)
        }
        BootPayload::RockchipRkbin { .. } => None,
    };

    // The payload must fit the space it was given — checked before the expensive
    // ext4 build, so an oversized boot payload fails fast rather than after
    // formatting the whole rootfs.
    let payloads = boot_payloads(&opts.boot, kpart.as_deref())?;
    region.check_payload_fit(&payloads)?;

    // The per-image first-boot password: generated here so the shared,
    // cacheable rootfs tarball stays password-free (the account is locked in it)
    // and each built image gets its own credential — spliced into the staged
    // `/etc/shadow` before formatting, not surgically into the tar. Its length is the
    // resolved config's, already bounded there.
    let password = crate::secret::generate_password(image.first_boot_password_length as usize)?;
    let password_hash = crate::secret::crypt_password(&password)?;

    // The ext4 rootfs partition is identical across layouts — build it once. An authored
    // size lays out the disk first and formats into the partition it leaves; a fitted one
    // formats first and lays out the disk around the size the search returned. Both end
    // holding a geometry whose partition is exactly the filesystem written into it, which
    // is the invariant a mount depends on.
    let ext4 = opts.work_dir.join("rootfs.ext4");
    let (geom, rootfs_size) = match size {
        ImageSize::Fixed(total) => {
            let geom = Geometry::resolve(&build.boot, total)?;
            let rootfs_size = ext4::RootfsSize::Exact(geom.rootfs_bytes);
            (Some(geom), rootfs_size)
        }
        ImageSize::Fit(slack) => (
            None,
            ext4::RootfsSize::Fit(geometry::fit_slack(&image.image_size, slack)?),
        ),
    };
    let rootfs_fs = ext4::build_rootfs_ext4(
        &ext4,
        rootfs_size,
        opts.rootfs_tar,
        opts.rootfs_label,
        opts.identity.ext4_uuid,
        ext4::FirstBoot {
            user: crate::rootfs::DEFAULT_USER,
            password_hash: &password_hash,
        },
        None,
        &step,
    )?;
    let geom = match geom {
        Some(geom) => geom,
        None => {
            let geom = Geometry::around_rootfs(&build.boot, rootfs_fs.size_bytes)?;
            step.log(format!(
                "sized the image to its contents: {} bytes on disk",
                geom.total_size
            ));
            geom
        }
    };
    step.progress(50);

    // The built-in seed: an empty template an operator (or `flash --hostname`)
    // later replaces. Deterministic — the volume serial comes off the derived
    // identity and the timestamp is the rootfs's own — so the image stays a
    // function of the lock.
    let seed_image = crate::press::seed::partition_image(
        &crate::press::seed::SeedKeys::default(),
        opts.identity.seed_volume_id(),
        rootfs_fs.time_secs,
        u32::try_from(geom.seed_off / geometry::SECTOR).unwrap_or(0),
    )?;

    let output = match build.layout {
        Layout::Combined => {
            let image = opts.out_dir.join(format!("{}.img", opts.stem));
            assemble_disk(
                &image,
                &geom,
                &DiskContents {
                    ext4: &ext4,
                    seed_image: &seed_image,
                    kpart: kpart.as_deref(),
                },
                Some(&opts.boot),
                opts.rootfs_label,
                &opts.identity,
                &step,
            )?;
            step.log(format!("wrote combined image {}", image.display()));
            ImageOutput::Combined { image }
        }
        Layout::Split => {
            // Only a method with a bootloader of its own can be split off onto a
            // separate medium; resolution rejects the combination for any other.
            let ResolvedBoot::RockchipRkbin(_) = &build.boot else {
                return Err(EngineError::StageNotApplicable {
                    stage: "image (split layout)",
                    why: "this boot method has no separate bootloader medium to emit",
                });
            };
            let BootPayload::RockchipRkbin {
                idbloader,
                uboot_itb,
            } = opts.boot
            else {
                return Err(EngineError::StageNotApplicable {
                    stage: "image (split layout)",
                    why: "no bootloader payloads were supplied",
                });
            };
            // Rootfs image: GPT + rootfs partition, empty raw gap (bootloader-agnostic).
            // The seed rides with the rootfs — it is the OS that reads it.
            let rootfs = opts.out_dir.join(format!("{}-rootfs.img", opts.stem));
            assemble_disk(
                &rootfs,
                &geom,
                &DiskContents {
                    ext4: &ext4,
                    seed_image: &seed_image,
                    kpart: None,
                },
                None,
                opts.rootfs_label,
                &opts.identity,
                &step,
            )?;
            // Bootloader image: just the raw-gap payloads on a gap-sized medium.
            let bootloader = opts.out_dir.join(format!("{}-boot.img", opts.stem));
            assemble_bootloader(&bootloader, &region, idbloader, uboot_itb, &step)?;
            step.log(format!(
                "wrote split images {} + {}",
                bootloader.display(),
                rootfs.display()
            ));
            ImageOutput::Split { bootloader, rootfs }
        }
    };
    step.progress(80);

    let mut compressed = Vec::new();
    let mut raw_removed = false;
    if !opts.compress.is_empty() {
        for image in output.images() {
            for &format in opts.compress {
                let dst = append_ext(image, format.extension());
                match format {
                    ImageCompression::Xz => compress_xz(image, &dst, opts.jobs, &step)?,
                    ImageCompression::Gz => compress_gz(image, &dst, &step)?,
                }
                step.log(format!("compressed {}", dst.display()));
                compressed.push(CompressedImage {
                    source: image.to_path_buf(),
                    path: dst,
                    format,
                });
            }
        }
        // The raw image is derivable from any of its containers, so drop it unless
        // asked to keep it — it is the largest artifact.
        if !opts.keep_raw {
            for image in output.images() {
                std::fs::remove_file(image).map_err(|s| EngineError::io(image, s))?;
            }
            raw_removed = true;
            let kept = opts
                .compress
                .iter()
                .map(|f| format!(".{f}"))
                .collect::<Vec<_>>()
                .join(" + ");
            step.log(format!("removed raw image(s); keeping {kept} only"));
        }
    }

    step.progress(100);
    step.finish();
    Ok(ImageArtifacts {
        output,
        compressed,
        raw_removed,
        password,
        rootfs_verified_with: rootfs_fs.verified_with,
        rootfs_filesystem: rootfs_fs.provenance,
        image_bytes: geom.total_size,
    })
}

/// Assemble just the bootloader image from the u-boot payloads, returning its
/// path — a flashable, GPT-less raw medium sized to the raw gap, holding
/// `idbloader.img` and `u-boot.itb` at their offsets.
///
/// Unlike [`build_image`] this needs no rootfs, so a `--stage uboot` run can emit
/// a directly-flashable boot medium — an eMMC (or SPI) that chain-loads the OS
/// from a separate disk — without bootstrapping a Debian rootfs first. The image
/// is the same `<stem>-boot.img` the [`Split`](Layout::Split) layout produces, named
/// for the build point's
/// [artifact stem](boot2deb_core::buildpoint::BuildPoint::artifact_stem).
/// It is left raw and uncompressed: gap-sized (a few MiB) and written straight to
/// the medium, so `.xz` would only add a decompress step before flashing.
pub fn build_bootloader_image(
    build: &ResolvedBuild,
    stem: &str,
    idbloader: &Path,
    uboot_itb: &Path,
    out_dir: &Path,
    sink: &dyn EventSink,
) -> Result<PathBuf, EngineError> {
    let step = Step::start(sink, "bootloader-image");
    // Only the head of the disk: this image *is* the boot region, so it has no rootfs
    // partition and no backup GPT, and the recipe's `image_size` says nothing about it.
    let region = geometry::BootRegion::resolve(&build.boot)?;
    // The same fail-fast fit check the full image node runs before placing bytes.
    region.check_payload_fit(&[
        ("idbloader.img", file_len(idbloader)?),
        ("u-boot.itb", file_len(uboot_itb)?),
    ])?;
    std::fs::create_dir_all(out_dir).map_err(|s| EngineError::io(out_dir, s))?;
    let image = out_dir.join(format!("{stem}-boot.img"));
    assemble_bootloader(&image, &region, idbloader, uboot_itb, &step)?;
    step.log(format!("wrote bootloader image {}", image.display()));
    step.finish();
    Ok(image)
}

/// Inputs for one pressed re-assembly — see [`press_image`].
pub struct PressOptions<'a> {
    /// The build's kept rootfs tarball (`<stem>-rootfs.tar`), exactly as the
    /// image node consumed it.
    pub rootfs_tar: &'a Path,
    /// The boot payload, required for a [`Combined`](ArtifactRole::Combined)
    /// press and absent for a split rootfs image (whose boot half is a separate
    /// file `press` streams unchanged).
    pub boot: Option<BootPayload<'a>>,
    /// Which artifact this output derives from: `Combined` or `Rootfs`. A
    /// [`Boot`](ArtifactRole::Boot) image carries no filesystem, so it is never
    /// re-assembled.
    pub role: ArtifactRole,
    /// The file to write — the caller's named output, not an artifact directory.
    pub output: &'a Path,
    /// Scratch directory for the intermediate ext4 partition image.
    pub work_dir: &'a Path,
    /// ext4 volume label and GPT partition name (≤ 16 bytes), e.g. `rootfs`.
    pub rootfs_label: &'a str,
    /// The recipe's deterministic on-disk identifiers — the same values the
    /// build derived, so a pressed image keeps the artifact's identity.
    pub identity: ImageIdentity,
    /// What this press adds to the tree. Never empty: a press with nothing to
    /// add streams the existing artifact instead of re-assembling.
    pub additions: &'a TreeAdditions,
}

/// What [`press_image`] produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PressedImage {
    /// The pressed image's own first-boot password — fresh for this file, since
    /// re-assembly runs the same per-image credential step a build does. The
    /// caller surfaces it; the recipe's provenance manifest still describes the
    /// build's artifact, not this derivative.
    pub password: String,
    /// The whole-disk size this press laid out, in bytes. Under a fitted
    /// `image_size` it grows with the additions.
    pub image_bytes: u64,
}

/// Re-assemble one pressed image from a build's kept artifacts, with
/// [`TreeAdditions`] merged into the rootfs and the `[pressed]` marker stamped
/// into its `/etc/boot2deb/image.toml`.
///
/// The same assembly a build runs — geometry, ext4-from-tar, seed template, GPT,
/// splice — pointed at one output file: no compression, no artifact directory,
/// and the recipe's artifacts untouched. Under a `fit`-sized recipe the
/// filesystem grows to hold whatever was added; under a fixed `image_size` a
/// press that does not fit fails in the format, naming the size. The rootfs is
/// verified exactly as a build's is (the in-process scan, plus `e2fsck -fn`
/// where present) before the disk is laid out.
///
/// # Errors
///
/// [`EngineError::StageNotApplicable`] for a role that carries no rootfs or a
/// combined press without its boot payload; [`EngineError::PressAddition`] when
/// an addition cannot be placed; otherwise the image node's own geometry,
/// format, and I/O errors.
pub fn press_image(
    ib: ImageBuild,
    opts: &PressOptions,
    sink: &dyn EventSink,
) -> Result<PressedImage, EngineError> {
    let ImageBuild { build, image } = ib;
    let step = Step::start(sink, "press");
    let with_boot = match opts.role {
        ArtifactRole::Combined => true,
        ArtifactRole::Rootfs => false,
        ArtifactRole::Boot => {
            return Err(EngineError::StageNotApplicable {
                stage: "press (re-assembly)",
                why: "a boot image carries no rootfs to add anything to",
            })
        }
    };
    let size = parse_image_size(&image.image_size)?;
    let region = geometry::BootRegion::resolve(&build.boot)?;
    std::fs::create_dir_all(opts.work_dir).map_err(|s| EngineError::io(opts.work_dir, s))?;
    if let Some(parent) = opts.output.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent).map_err(|s| EngineError::io(parent, s))?;
    }

    // The boot payload, exactly as the image node resolves it: a combined press
    // places it, and under depthcharge that means taking the signed kernel out
    // of the rootfs tarball and holding it to this identity's root PARTUUID.
    let kpart = match (with_boot, &opts.boot) {
        (true, Some(BootPayload::Depthcharge)) => {
            let kpart = depthcharge::extract_kpart(opts.rootfs_tar, opts.work_dir, &step)?;
            depthcharge::verify_kpart(&kpart, opts.identity.rootfs_partuuid)?;
            Some(kpart)
        }
        (true, Some(BootPayload::RockchipRkbin { .. })) => None,
        (true, None) => {
            return Err(EngineError::StageNotApplicable {
                stage: "press (re-assembly)",
                why: "a combined image needs its boot payload",
            })
        }
        (false, _) => None,
    };
    if with_boot {
        let boot = opts.boot.as_ref().expect("checked above");
        let payloads = boot_payloads(boot, kpart.as_deref())?;
        region.check_payload_fit(&payloads)?;
    }

    // A fresh per-unit credential: the kept rootfs tar has the account locked
    // (the build's password was spliced into the build's image, not the tar),
    // so a pressed image gets its own, surfaced by the caller.
    let password = crate::secret::generate_password(image.first_boot_password_length as usize)?;
    let password_hash = crate::secret::crypt_password(&password)?;

    let ext4 = opts.work_dir.join("press-rootfs.ext4");
    let (geom, rootfs_size) = match size {
        ImageSize::Fixed(total) => {
            let geom = Geometry::resolve(&build.boot, total)?;
            let rootfs_size = ext4::RootfsSize::Exact(geom.rootfs_bytes);
            (Some(geom), rootfs_size)
        }
        ImageSize::Fit(slack) => (
            None,
            ext4::RootfsSize::Fit(geometry::fit_slack(&image.image_size, slack)?),
        ),
    };
    let rootfs_fs = ext4::build_rootfs_ext4(
        &ext4,
        rootfs_size,
        opts.rootfs_tar,
        opts.rootfs_label,
        opts.identity.ext4_uuid,
        ext4::FirstBoot {
            user: crate::rootfs::DEFAULT_USER,
            password_hash: &password_hash,
        },
        Some(opts.additions),
        &step,
    )?;
    let geom = match geom {
        Some(geom) => geom,
        None => {
            let geom = Geometry::around_rootfs(&build.boot, rootfs_fs.size_bytes)?;
            step.log(format!(
                "sized the pressed image to its contents: {} bytes on disk",
                geom.total_size
            ));
            geom
        }
    };

    // The built-in empty seed template, exactly as a build splices it; the
    // caller personalizes it afterwards where keys were named.
    let seed_image = crate::press::seed::partition_image(
        &crate::press::seed::SeedKeys::default(),
        opts.identity.seed_volume_id(),
        rootfs_fs.time_secs,
        u32::try_from(geom.seed_off / geometry::SECTOR).unwrap_or(0),
    )?;
    assemble_disk(
        opts.output,
        &geom,
        &DiskContents {
            ext4: &ext4,
            seed_image: &seed_image,
            kpart: kpart.as_deref(),
        },
        opts.boot.as_ref().filter(|_| with_boot),
        opts.rootfs_label,
        &opts.identity,
        &step,
    )?;
    // The scratch ext4 is press-local; a build's is kept for stage reuse, but a
    // press leaves nothing behind except its output.
    std::fs::remove_file(&ext4).map_err(|s| EngineError::io(&ext4, s))?;
    step.log(format!(
        "pressed {} ({} bytes)",
        opts.output.display(),
        geom.total_size
    ));
    step.finish();
    Ok(PressedImage {
        password,
        image_bytes: geom.total_size,
    })
}

/// The boot payloads to place, as `(name, length)` pairs in the order the boot
/// method writes them — the input to [`Geometry::check_payload_fit`].
fn boot_payloads<'a>(
    boot: &BootPayload<'a>,
    kpart: Option<&'a Path>,
) -> Result<Vec<(&'a str, u64)>, EngineError> {
    match (boot, kpart) {
        (
            BootPayload::RockchipRkbin {
                idbloader,
                uboot_itb,
            },
            _,
        ) => Ok(vec![
            ("idbloader.img", file_len(idbloader)?),
            ("u-boot.itb", file_len(uboot_itb)?),
        ]),
        (BootPayload::Depthcharge, Some(kpart)) => {
            Ok(vec![("the signed kernel partition", file_len(kpart)?)])
        }
        (BootPayload::Depthcharge, None) => Err(EngineError::StageNotApplicable {
            stage: "image",
            why: "the signed kernel partition was not extracted from the rootfs",
        }),
    }
}

/// What goes onto a disk beyond its table: the filesystems, and (for a combined
/// image) the signed kernel partition. One value because the three travel
/// together through both `assemble_disk` calls.
struct DiskContents<'a> {
    /// The rootfs ext4 partition image, spliced at the rootfs offset.
    ext4: &'a Path,
    /// The seed partition's whole content, spliced at the seed offset.
    seed_image: &'a [u8],
    /// The signed kernel partition, under `depthcharge` with a boot payload.
    kpart: Option<&'a Path>,
}

/// Write a whole-disk image: a full-size file, the GPT table, the rootfs ext4
/// filesystem and seed partition spliced at their offsets, and — when `boot` is
/// given — the boot method's payload. Shared by combined (with the boot
/// payload), the split rootfs image (without), and a press re-assembly (either).
fn assemble_disk(
    image: &Path,
    geom: &Geometry,
    contents: &DiskContents<'_>,
    boot: Option<&BootPayload<'_>>,
    rootfs_label: &str,
    identity: &ImageIdentity,
    step: &Step,
) -> Result<(), EngineError> {
    let kpart = contents.kpart;
    create_sized_image(image, geom.total_size)?;
    gpt::write_table(
        image,
        geom,
        rootfs_label,
        identity.disk_guid,
        identity.rootfs_partuuid,
        identity.seed_partuuid,
        &identity.kpart_guids,
    )?;
    splice_bytes(image, geom.seed_off, contents.seed_image)?;
    splice_file(image, geom.rootfs_off, contents.ext4)?;
    if let Some(boot) = boot {
        match (&geom.boot, boot) {
            (
                BootGeometry::RawGap {
                    idbloader_off,
                    uboot_itb_off,
                },
                BootPayload::RockchipRkbin {
                    idbloader,
                    uboot_itb,
                },
            ) => {
                splice_file(image, *idbloader_off, idbloader)?;
                splice_file(image, *uboot_itb_off, uboot_itb)?;
            }
            (BootGeometry::Kpart { slots }, BootPayload::Depthcharge) => {
                let kpart = kpart.ok_or(EngineError::StageNotApplicable {
                    stage: "image",
                    why: "the signed kernel partition was not extracted from the rootfs",
                })?;
                // Only the first slot. The spares are left as the zeroes the sparse
                // image file already holds — an empty slot at priority 0, which the
                // firmware will not boot and the first on-device kernel upgrade will
                // fill. Writing a copy of the payload into them instead would give the
                // upgrade nothing to fall back *to*: both slots would then hold the
                // same kernel, and a bad upgrade would have overwritten the only other
                // copy of the one that worked.
                let payload = slots.first().ok_or(EngineError::StageNotApplicable {
                    stage: "image",
                    why: "the depthcharge geometry resolved to no kernel slots",
                })?;
                splice_file(image, payload.offset, kpart)?;
            }
            // The geometry and the payload both come from the same resolved boot
            // method, so they cannot disagree — but they are separate values, and a
            // mismatch would write a bootloader into a kernel partition.
            _ => {
                return Err(EngineError::StageNotApplicable {
                    stage: "image",
                    why: "the boot payload does not match the resolved boot geometry",
                })
            }
        }
    }
    step.log(format!(
        "laid GPT + rootfs partition{} into {}",
        if boot.is_some() {
            " + boot payload"
        } else {
            ""
        },
        image.display()
    ));
    Ok(())
}

/// Write a bootloader-only image: a raw medium sized to the gap, holding just the
/// two payloads at their offsets (no GPT — this medium carries only the
/// bootloader). Shared by the split layout and [`build_bootloader_image`].
fn assemble_bootloader(
    image: &Path,
    region: &geometry::BootRegion,
    idbloader: &Path,
    uboot_itb: &Path,
    step: &Step,
) -> Result<(), EngineError> {
    let BootGeometry::RawGap {
        idbloader_off,
        uboot_itb_off,
    } = region.boot
    else {
        return Err(EngineError::StageNotApplicable {
            stage: "bootloader-image",
            why: "this boot method writes no bootloader into a raw gap",
        });
    };
    create_sized_image(image, region.rootfs_off)?;
    splice_file(image, idbloader_off, idbloader)?;
    splice_file(image, uboot_itb_off, uboot_itb)?;
    step.log(format!("laid bootloader payloads into {}", image.display()));
    Ok(())
}

/// Create (truncate) `path` and set it to exactly `size` bytes (sparse). The GPT
/// writer opens the file without creating it and places the backup table
/// relative to its length, so the file must be full-size first.
fn create_sized_image(path: &Path, size: u64) -> Result<(), EngineError> {
    let f = std::fs::OpenOptions::new()
        .write(true)
        .read(true)
        .create(true)
        .truncate(true)
        .open(path)
        .map_err(|s| EngineError::io(path, s))?;
    f.set_len(size).map_err(|s| EngineError::io(path, s))?;
    Ok(())
}

/// Copy the whole of `src` into `image` starting at byte `offset`, without
/// truncating `image` (the surrounding partitions/tables are already written).
///
/// A **sparse** copy: runs of zero bytes in the source are skipped by seeking the
/// destination forward rather than writing them, so the output keeps the ~2 GB
/// ext4's holes instead of materializing every zero block — halving write I/O on
/// the largest artifact. Correct only because the caller pre-sizes `image`
/// (via [`create_sized_image`]) to cover `offset + len(src)`, so seeking over a
/// trailing hole never shortens the file; the skipped bytes were already zero from
/// the sparse `set_len`.
/// Write `bytes` into the image at `offset` — the seed partition, whose whole
/// content is built in memory. Zero runs are written as-is rather than
/// hole-punched: the partition is 1 MiB, so sparseness buys nothing.
fn splice_bytes(image: &Path, offset: u64, bytes: &[u8]) -> Result<(), EngineError> {
    let mut dst = std::fs::OpenOptions::new()
        .write(true)
        .open(image)
        .map_err(|s| EngineError::io(image, s))?;
    dst.seek(SeekFrom::Start(offset))
        .map_err(|s| EngineError::io(image, s))?;
    dst.write_all(bytes)
        .map_err(|s| EngineError::io(image, s))?;
    dst.flush().map_err(|s| EngineError::io(image, s))?;
    Ok(())
}

fn splice_file(image: &Path, offset: u64, src: &Path) -> Result<(), EngineError> {
    /// Sparse-copy block size; also the zero-run granularity.
    const CHUNK: usize = 1 << 20; // 1 MiB
    let mut dst = std::fs::OpenOptions::new()
        .write(true)
        .read(true)
        .open(image)
        .map_err(|s| EngineError::io(image, s))?;
    dst.seek(SeekFrom::Start(offset))
        .map_err(|s| EngineError::io(image, s))?;
    let mut source = std::fs::File::open(src).map_err(|s| EngineError::io(src, s))?;
    let mut buf = vec![0u8; CHUNK];
    loop {
        let n = read_chunk(&mut source, &mut buf).map_err(|s| EngineError::io(src, s))?;
        if n == 0 {
            break;
        }
        let chunk = &buf[..n];
        if chunk.iter().all(|&b| b == 0) {
            // Leave the destination's existing zeros (from set_len) as a hole.
            dst.seek(SeekFrom::Current(n as i64))
                .map_err(|s| EngineError::io(image, s))?;
        } else {
            dst.write_all(chunk)
                .map_err(|s| EngineError::io(image, s))?;
        }
    }
    Ok(())
}

/// Read up to `buf.len()` bytes, looping over short reads until the buffer is full
/// or EOF; returns the number of bytes read (0 at EOF). Lets [`splice_file`] test
/// whole `CHUNK`-sized blocks for zero-ness rather than whatever a single `read`
/// returned.
fn read_chunk<R: Read>(reader: &mut R, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => break,
            Ok(n) => filled += n,
            Err(ref e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        }
    }
    Ok(filled)
}

/// `.xz`-compress `src` to `dst` with the pure-Rust multithreaded encoder.
///
/// Image-sized inputs make single-threaded LZMA impractical, so this fans the
/// encode across `jobs` workers ([`XzWriterMt`], one block per worker), defaulting to
/// the host's available parallelism when the build set no cap; a small input
/// degenerates to a single block. The container is standard `.xz` either way.
fn compress_xz(
    src: &Path,
    dst: &Path,
    jobs: Option<usize>,
    step: &Step,
) -> Result<(), EngineError> {
    let workers = xz_workers(jobs);
    step.log(format!(
        "compressing {} -> {} (xz preset {XZ_PRESET}, {workers} worker(s))",
        src.display(),
        dst.display()
    ));
    let input = std::fs::File::open(src).map_err(|s| EngineError::io(src, s))?;
    let output = std::fs::File::create(dst).map_err(|s| EngineError::io(dst, s))?;
    let mut opts = XzOptions::with_preset(XZ_PRESET);
    // MT requires an explicit block size — it is the work-unit boundary.
    opts.set_block_size(Some(
        NonZeroU64::new(XZ_BLOCK_SIZE).expect("block size is non-zero"),
    ));
    let mut writer = XzWriterMt::new(output, opts, workers).map_err(|s| EngineError::io(dst, s))?;
    std::io::copy(&mut std::io::BufReader::new(input), &mut writer)
        .map_err(|s| EngineError::io(src, s))?;
    writer.finish().map_err(|s| EngineError::io(dst, s))?;
    Ok(())
}

/// The `.xz` worker count for a build's [`jobs`](crate::build::BuildEnv::jobs) cap:
/// the cap where one is set, else the host's available parallelism, and never zero
/// (the encoder needs at least one worker, and `--jobs 0` is not a request for none).
///
/// Pure, so the bound is testable without compressing anything.
fn xz_workers(jobs: Option<usize>) -> u32 {
    let n = jobs.unwrap_or_else(|| {
        std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
    });
    n.clamp(1, u32::MAX as usize) as u32
}

/// `.gz`-compress `src` to `dst` with the pure-Rust `miniz_oxide` backend.
///
/// Single-threaded: gzip is one deflate stream with no block index to fan out
/// across, so unlike [`compress_xz`] there is no worker count to size. The header's
/// mtime is pinned to zero so the container is a function of the image bytes alone
/// — a build that replays to the same image replays to the same `.gz`.
fn compress_gz(src: &Path, dst: &Path, step: &Step) -> Result<(), EngineError> {
    step.log(format!(
        "compressing {} -> {} (gzip level {GZ_LEVEL})",
        src.display(),
        dst.display()
    ));
    let input = std::fs::File::open(src).map_err(|s| EngineError::io(src, s))?;
    let output = std::fs::File::create(dst).map_err(|s| EngineError::io(dst, s))?;
    let mut writer = flate2::GzBuilder::new()
        .mtime(0)
        .write(output, flate2::Compression::new(GZ_LEVEL));
    std::io::copy(&mut std::io::BufReader::new(input), &mut writer)
        .map_err(|s| EngineError::io(src, s))?;
    writer.finish().map_err(|s| EngineError::io(dst, s))?;
    Ok(())
}

/// The byte length of `path`.
fn file_len(path: &Path) -> Result<u64, EngineError> {
    Ok(std::fs::metadata(path)
        .map_err(|s| EngineError::io(path, s))?
        .len())
}

/// `foo.img` + `xz` → `foo.img.xz`. Appends rather than replaces, so the raw
/// image's own extension stays visible in the compressed name.
fn append_ext(path: &Path, ext: &str) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".");
    s.push(ext);
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {

    /// The image half of a fixture build. Every fixture here resolves a shipped image
    /// recipe, so the axis is there; the unwrap states that rather than threading an
    /// `Option` through every assertion.
    fn image_of(build: &boot2deb_core::ResolvedBuild) -> &boot2deb_core::ResolvedImage {
        pair_of(build).image
    }

    /// The same fixture build as an [`ImageBuild`] pair, for the stages that take one.
    fn pair_of(build: &boot2deb_core::ResolvedBuild) -> boot2deb_core::ImageBuild<'_> {
        build.as_image().expect("the fixture recipes build images")
    }
    use super::*;
    use boot2deb_core::{resolve_recipe, ConfigRoot, Overrides};
    use std::process::Command;

    /// Repo root two levels up from this crate.
    fn repo_root() -> ConfigRoot {
        ConfigRoot::new(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(2)
                .unwrap()
                .to_path_buf(),
        )
    }

    /// Resolve the RK1 build, overriding the image size so tests build a small
    /// (but geometry-valid) image quickly.
    fn small_rk1_build(image_size: &str) -> ResolvedBuild {
        let mut b =
            resolve_recipe(&repo_root(), "turing-rk1/forky", &Overrides::default()).unwrap();
        b.image.as_mut().unwrap().image_size = image_size.to_string();
        b
    }

    /// Whether the end-to-end image path can run: every tool in `tools` is runnable.
    /// The ext4 format itself is pure Rust and needs no host tool; `tools` covers only
    /// the fixture helper the test drives (`tar`).
    fn require_host_tools(tools: &[&str]) -> bool {
        crate::hosttool::require(tools)
    }

    /// Build a tiny rootfs tarball (a few dirs + files) at `path`.
    fn make_rootfs_tar(dir: &Path, path: &Path) {
        let root = dir.join("rootfs");
        std::fs::create_dir_all(root.join("etc/boot2deb")).unwrap();
        std::fs::create_dir_all(root.join("usr/bin")).unwrap();
        std::fs::write(root.join("etc/hostname"), b"turing-rk1\n").unwrap();
        std::fs::write(root.join("usr/bin/true"), b"#!/bin/true\n").unwrap();
        // The identity document every rootfs carries — what the pressed marker is
        // written into.
        std::fs::write(
            root.join("etc/boot2deb/image.toml"),
            "version = 1\n\
             [image]\n\
             device = \"turing-rk1\"\n\
             description = \"fixture\"\n\
             arch = \"arm64\"\n\
             soc = \"rk3588\"\n\
             boot_method = \"rockchip-rkbin\"\n\
             suite = \"forky\"\n\
             features = []\n\
             layout = \"combined\"\n\
             hostname = \"turing-rk1\"\n\
             [kernel]\n\
             id = \"fixture\"\n\
             flavor = \"mainline\"\n",
        )
        .unwrap();
        // The default account is locked in the tarball; the image stage splices the
        // per-image first-boot hash into it before formatting.
        std::fs::write(
            root.join("etc/shadow"),
            b"root:*:19000:0:99999:7:::\ndebian:!:19000:0:99999:7:::\n",
        )
        .unwrap();
        let out = std::fs::File::create(path).unwrap();
        // Record root ownership like the real rootfs tar (which emits uid 0); the
        // formatter reads each entry's ownership straight from the tar headers.
        let status = Command::new("tar")
            .args(["--owner=0", "--group=0", "--numeric-owner", "-C"])
            .arg(&root)
            .arg("-cf")
            .arg(path)
            .arg(".")
            .status()
            .unwrap();
        assert!(status.success(), "tar failed");
        drop(out);
    }

    #[test]
    fn append_ext_adds_the_containers_suffix() {
        // Appended, not substituted: the raw `.img` stays visible in the name.
        assert_eq!(
            append_ext(
                Path::new("/o/turing-rk1.img"),
                ImageCompression::Xz.extension()
            ),
            Path::new("/o/turing-rk1.img.xz")
        );
        assert_eq!(
            append_ext(
                Path::new("/o/turing-rk1.img"),
                ImageCompression::Gz.extension()
            ),
            Path::new("/o/turing-rk1.img.gz")
        );
    }

    #[test]
    fn every_container_parses_from_its_own_names_and_pairs_with_a_decompressor() {
        // The `next:` hint pipes the artifact through `decompressor()`, so a wrong
        // pairing here hands an operator a command that cannot read their image.
        use std::str::FromStr;
        for (spelling, want) in [
            ("xz", ImageCompression::Xz),
            ("gz", ImageCompression::Gz),
            ("gzip", ImageCompression::Gz),
        ] {
            assert_eq!(ImageCompression::from_str(spelling), Ok(want), "{spelling}");
        }
        assert!(ImageCompression::from_str("bz2").is_err());
        assert!(ImageCompression::from_str("zst").is_err());
        assert_eq!(ImageCompression::Xz.decompressor(), "xzcat");
        assert_eq!(ImageCompression::Gz.decompressor(), "zcat");
    }

    #[test]
    fn derive_uuid_is_deterministic_distinct_and_well_formed() {
        // Same inputs → same UUID (the reproducibility contract).
        let a = derive_uuid("commitsha", "turing-rk1", "ext4-rootfs");
        let b = derive_uuid("commitsha", "turing-rk1", "ext4-rootfs");
        assert_eq!(a, b);

        // The three per-purpose domains must not collapse to one value under a
        // shared seed, and a different seed or device must move the result.
        let disk = derive_uuid("commitsha", "turing-rk1", "gpt-disk");
        let part = derive_uuid("commitsha", "turing-rk1", "gpt-partition");
        assert_ne!(a, disk);
        assert_ne!(a, part);
        assert_ne!(disk, part);
        assert_ne!(a, derive_uuid("othersha", "turing-rk1", "ext4-rootfs"));
        assert_ne!(a, derive_uuid("commitsha", "other-board", "ext4-rootfs"));

        // NUL framing: ("ab","c",..) and ("a","bc",..) must not collide.
        assert_ne!(
            derive_uuid("ab", "c", "ext4-rootfs"),
            derive_uuid("a", "bc", "ext4-rootfs")
        );

        // Well-formed version-4 / RFC-4122 UUID, so any tool accepts it.
        assert_eq!(a.get_version_num(), 4);
        assert_eq!(a.get_variant(), uuid::Variant::RFC4122);
    }

    #[test]
    fn compress_xz_roundtrips_via_xz_container() {
        // Pure-Rust encode; decode with host `xz -d` to prove the container is valid.
        if !require_host_tools(&["xz"]) {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("data.bin");
        let payload: Vec<u8> = (0..64u32 * 1024)
            .map(|i| i.wrapping_mul(2654435761) as u8)
            .collect();
        std::fs::write(&src, &payload).unwrap();
        let xz = tmp.path().join("data.bin.xz");
        let sink = |_: crate::event::Event| {};
        let step = Step::start(&sink, "image");
        compress_xz(&src, &xz, None, &step).unwrap();

        let out = Command::new("xz").args(["-dc"]).arg(&xz).output().unwrap();
        assert!(
            out.status.success(),
            "xz -d failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.stdout, payload);
    }

    #[test]
    fn compress_gz_roundtrips_via_gzip_container() {
        // Pure-Rust encode; decode with host `gzip` to prove the container is one a
        // gzip reader accepts — which is the whole reason this format exists here,
        // since u-boot's `gzwrite` is the consumer and cannot read `.xz`.
        if !require_host_tools(&["gzip"]) {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("data.bin");
        let payload: Vec<u8> = (0..64u32 * 1024)
            .map(|i| i.wrapping_mul(2654435761) as u8)
            .collect();
        std::fs::write(&src, &payload).unwrap();
        let gz = tmp.path().join("data.bin.gz");
        let sink = |_: crate::event::Event| {};
        let step = Step::start(&sink, "image");
        compress_gz(&src, &gz, &step).unwrap();

        let out = Command::new("gzip")
            .args(["-dc"])
            .arg(&gz)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "gzip -d failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(out.stdout, payload);
    }

    #[test]
    fn compress_gz_is_a_function_of_the_input_bytes_alone() {
        // The gzip header carries an mtime field, which the encoder pins to zero.
        // Without that a rebuild would produce a different `.gz` for an identical
        // image, and the reproducibility claim is about artifact bytes.
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("data.bin");
        std::fs::write(&src, b"the same bytes both times").unwrap();
        let sink = |_: crate::event::Event| {};
        let step = Step::start(&sink, "image");
        let (a, b) = (tmp.path().join("a.gz"), tmp.path().join("b.gz"));
        compress_gz(&src, &a, &step).unwrap();
        compress_gz(&src, &b, &step).unwrap();
        assert_eq!(std::fs::read(&a).unwrap(), std::fs::read(&b).unwrap());
    }

    #[test]
    fn jobs_bounds_the_xz_worker_pool() {
        // The flag means "for this build", not "for `make`": an unbounded compressor
        // on a shared machine would fan across every core no matter what `--jobs` said.
        assert_eq!(xz_workers(Some(4)), 4);
        assert_eq!(xz_workers(Some(1)), 1);
        // `--jobs 0` is not a request for no workers; the encoder needs at least one.
        assert_eq!(xz_workers(Some(0)), 1);
        // Unset falls back to the host, which is whatever this machine has — assert the
        // property, not the number.
        let host = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1) as u32;
        assert_eq!(xz_workers(None), host);
        assert!(xz_workers(None) >= 1);
    }

    #[test]
    fn bootloader_image_is_gap_sized_with_payloads_and_no_gpt() {
        // No ext4/rootfs here — pure geometry + splice — so this runs on any host
        // (no host-tool gate), unlike the whole-disk tests below.
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out");
        let idb = tmp.path().join("idbloader.img");
        let itb = tmp.path().join("u-boot.itb");
        std::fs::write(&idb, b"IDBLOADER-PAYLOAD").unwrap();
        std::fs::write(&itb, b"UBOOT-ITB-PAYLOAD").unwrap();

        let build = small_rk1_build("192MiB");
        let sink = |_: crate::event::Event| {};
        let image =
            build_bootloader_image(&build, "turing-rk1-forky", &idb, &itb, &out, &sink).unwrap();

        // Named after the build point, and sized to the raw gap (rootfs offset =
        // 16 MiB), NOT the 48 MiB image size — this medium carries only the bootloader.
        assert_eq!(image.file_name().unwrap(), "turing-rk1-forky-boot.img");
        assert_eq!(std::fs::metadata(&image).unwrap().len(), 16 * 1024 * 1024);

        let bytes = std::fs::read(&image).unwrap();
        let at = |off: usize, tag: &[u8]| assert_eq!(&bytes[off..off + tag.len()], tag);
        at(32 * 1024, b"IDBLOADER-PAYLOAD");
        at(8 * 1024 * 1024, b"UBOOT-ITB-PAYLOAD");
        // No GPT: the protective-MBR signature slot stays zero (the combined and
        // rootfs images write 0x55AA there; this one must not).
        assert_eq!(&bytes[510..512], &[0x00, 0x00]);
    }

    /// A press with additions re-assembles from the kept tar: the added files land
    /// with their modes and synthesized parents, the pressed marker is stamped —
    /// and every other file in the rootfs is identical to what a plain build of
    /// the same inputs carries. `/etc/shadow` is the one deliberate exception:
    /// each press draws its own first-boot password.
    #[test]
    fn a_pressed_image_adds_files_and_changes_nothing_else() {
        use ferrosys::ext::{OpenOptions as ExtOpen, Reader as ExtReader};
        use std::collections::BTreeMap;

        if !require_host_tools(&["tar"]) {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let rootfs_tar = tmp.path().join("rootfs.tar");
        make_rootfs_tar(tmp.path(), &rootfs_tar);
        let idb = tmp.path().join("idbloader.img");
        let itb = tmp.path().join("u-boot.itb");
        std::fs::write(&idb, b"IDBLOADER-PAYLOAD").unwrap();
        std::fs::write(&itb, b"UBOOT-ITB-PAYLOAD").unwrap();
        let build = small_rk1_build("fit+20%");
        let identity = ImageIdentity::derive("test-seed", "turing-rk1");
        let sink = |_: crate::event::Event| {};

        // The baseline: the build's own combined image, raw.
        let out = tmp.path().join("out");
        let arts = build_image(
            pair_of(&build),
            &ImageOptions {
                rootfs_tar: &rootfs_tar,
                boot: BootPayload::RockchipRkbin {
                    idbloader: &idb,
                    uboot_itb: &itb,
                },
                out_dir: &out,
                stem: "turing-rk1-forky",
                work_dir: &tmp.path().join("work-build"),
                rootfs_label: "rootfs",
                identity,
                compress: &[],
                keep_raw: false,
                jobs: None,
            },
            &sink,
        )
        .unwrap();
        let ImageOutput::Combined { image: baseline } = &arts.output else {
            panic!("expected combined, got {:?}", arts.output)
        };

        // The press: one config copied over a shipped file, one new file with
        // missing parents, one first-boot deb, one embedded artifact.
        let site = tmp.path().join("site.conf");
        std::fs::write(&site, b"site\n").unwrap();
        let hostname = tmp.path().join("hostname");
        std::fs::write(&hostname, b"rk1-site\n").unwrap();
        let deb = tmp.path().join("app_1.0_arm64.deb");
        std::fs::write(&deb, b"DEB-BYTES").unwrap();
        let embedded = tmp.path().join("turing-rk1-forky.img.xz");
        std::fs::write(&embedded, b"EMBEDDED-ARTIFACT").unwrap();
        let mut additions = TreeAdditions::new("turing-rk1-forky", "turing-rk1/forky", identity);
        additions.copy(&site, "/opt/site/site.conf").unwrap();
        additions.copy(&hostname, "/etc/hostname").unwrap();
        additions.deb(&deb).unwrap();
        additions.embed_image(&embedded).unwrap();

        let pressed_path = tmp.path().join("card.img");
        let pressed = press_image(
            pair_of(&build),
            &PressOptions {
                rootfs_tar: &rootfs_tar,
                boot: Some(BootPayload::RockchipRkbin {
                    idbloader: &idb,
                    uboot_itb: &itb,
                }),
                role: ArtifactRole::Combined,
                output: &pressed_path,
                work_dir: &tmp.path().join("work-press"),
                rootfs_label: "rootfs",
                identity,
                additions: &additions,
            },
            &sink,
        )
        .unwrap();
        assert!(!pressed.password.is_empty());
        assert_eq!(
            std::fs::metadata(&pressed_path).unwrap().len(),
            pressed.image_bytes
        );

        // The boot payload is placed exactly as a build places it.
        let bytes = std::fs::read(&pressed_path).unwrap();
        assert_eq!(&bytes[32 * 1024..32 * 1024 + 17], b"IDBLOADER-PAYLOAD");

        // Walk both filesystems (they sit at the same 16 MiB offset) into
        // path -> (mode, uid, content) maps.
        type Snapshot = BTreeMap<Vec<u8>, (u16, u32, Option<Vec<u8>>)>;
        let walk = |path: &Path| -> Snapshot {
            let file = std::fs::File::open(path).unwrap();
            let mut reader =
                ExtReader::open_with(file, &ExtOpen::new().base(16 * 1024 * 1024)).unwrap();
            let entries = reader.walk().unwrap();
            entries
                .into_iter()
                .map(|e| {
                    // Regular files compare by content; everything else by shape.
                    let body = (e.inode.mode & 0xF000 == 0x8000)
                        .then(|| reader.read_data(&e.inode).unwrap());
                    (e.path, (e.inode.mode, e.inode.uid, body))
                })
                .collect()
        };
        let base_map = walk(baseline);
        let press_map = walk(&pressed_path);

        // The additions landed: contents, modes, synthesized parents.
        let added = &press_map[b"/opt/site/site.conf".as_slice()];
        assert_eq!(added.2.as_deref(), Some(b"site\n".as_slice()));
        assert_eq!(added.0 & 0o7777, 0o644);
        assert_eq!(added.1, 0, "additions are root-owned");
        assert!(press_map[b"/opt/site".as_slice()].0 & 0xF000 == 0x4000);
        assert_eq!(
            press_map[b"/etc/hostname".as_slice()].2.as_deref(),
            Some(b"rk1-site\n".as_slice()),
            "a copy replaces the shipped file"
        );
        assert_eq!(
            press_map[b"/var/lib/boot2deb/firstboot-debs/app_1.0_arm64.deb".as_slice()]
                .2
                .as_deref(),
            Some(b"DEB-BYTES".as_slice())
        );
        assert_eq!(
            press_map[b"/var/lib/boot2deb/install/turing-rk1-forky.img.xz".as_slice()]
                .2
                .as_deref(),
            Some(b"EMBEDDED-ARTIFACT".as_slice())
        );

        // The marker names the source and everything added, by kind.
        let marker = press_map[b"/etc/boot2deb/image.toml".as_slice()]
            .2
            .clone()
            .unwrap();
        let identity_doc = boot2deb_core::provenance::SystemIdentity::from_toml_str(
            std::str::from_utf8(&marker).unwrap(),
            "image.toml",
        )
        .unwrap();
        let pressed_table = identity_doc.pressed.expect("pressed table present");
        assert_eq!(pressed_table.source, "turing-rk1-forky");
        assert_eq!(
            pressed_table.copies,
            ["/etc/hostname", "/opt/site/site.conf"]
        );
        assert_eq!(pressed_table.debs, ["app_1.0_arm64.deb"]);
        assert_eq!(
            pressed_table.embedded_image.as_deref(),
            Some("turing-rk1-forky.img.xz")
        );
        assert!(
            base_map[b"/etc/boot2deb/image.toml".as_slice()]
                .2
                .as_deref()
                .is_some_and(|body| !body.windows(9).any(|w| w == b"[pressed]")),
            "the build's own image carries no pressed table"
        );

        // Everything the press did not touch is identical to the baseline —
        // content, mode, and ownership alike.
        let touched: &[&[u8]] = &[
            b"/etc/hostname",
            b"/etc/boot2deb/image.toml",
            b"/etc/shadow",
        ];
        for (path, entry) in &base_map {
            if touched.contains(&path.as_slice()) || !press_map.contains_key(path) {
                assert!(
                    touched.contains(&path.as_slice()),
                    "baseline path {} missing from the pressed image",
                    String::from_utf8_lossy(path)
                );
                continue;
            }
            assert_eq!(
                entry,
                &press_map[path],
                "untouched path {} differs",
                String::from_utf8_lossy(path)
            );
        }
        // The password entry differs by design: each press draws its own.
        assert_ne!(
            base_map[b"/etc/shadow".as_slice()].2,
            press_map[b"/etc/shadow".as_slice()].2
        );
    }

    /// A fitted image is the whole orchestration in the other direction: the format
    /// decides the filesystem's size, and every later step — the GPT, the splices, the
    /// disk itself — is laid out around what it decided. Nothing else covers that path;
    /// the fixed-size test above cannot, because there the size is known before the
    /// rootfs exists.
    #[test]
    fn a_fitted_image_sizes_the_disk_to_what_the_format_chose() {
        if !require_host_tools(&["tar"]) {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        let out = tmp.path().join("out");
        let rootfs_tar = tmp.path().join("rootfs.tar");
        make_rootfs_tar(tmp.path(), &rootfs_tar);
        let idb = tmp.path().join("idbloader.img");
        let itb = tmp.path().join("u-boot.itb");
        std::fs::write(&idb, b"IDBLOADER-PAYLOAD").unwrap();
        std::fs::write(&itb, b"UBOOT-ITB-PAYLOAD").unwrap();

        let build = small_rk1_build("fit+20%");
        let opts = ImageOptions {
            rootfs_tar: &rootfs_tar,
            boot: BootPayload::RockchipRkbin {
                idbloader: &idb,
                uboot_itb: &itb,
            },
            out_dir: &out,
            stem: "turing-rk1-forky",
            work_dir: &work,
            rootfs_label: "rootfs",
            identity: ImageIdentity::derive("test-seed", "turing-rk1"),
            compress: &[],
            keep_raw: false,
            jobs: None,
        };
        let sink = |_: crate::event::Event| {};
        let arts = build_image(pair_of(&build), &opts, &sink).unwrap();
        let ImageOutput::Combined { image } = &arts.output else {
            panic!("expected combined, got {:?}", arts.output)
        };

        let len = std::fs::metadata(image).unwrap().len();
        let bytes = std::fs::read(image).unwrap();
        // The boot region is untouched by the inversion.
        let at = |off: usize, tag: &[u8]| assert_eq!(&bytes[off..off + tag.len()], tag);
        at(32 * 1024, b"IDBLOADER-PAYLOAD");
        at(8 * 1024 * 1024, b"UBOOT-ITB-PAYLOAD");
        assert_eq!(&bytes[510..512], &[0x55, 0xAA], "protective MBR");
        let sb = 16 * 1024 * 1024 + 1024;
        assert_eq!(&bytes[sb - 1024 + 0x438..sb - 1024 + 0x43a], &[0x53, 0xEF]);

        // The disk is the head, the filesystem, and the backup table — nothing more.
        let blocks = u32::from_le_bytes(bytes[sb + 4..sb + 8].try_into().unwrap()) as u64;
        assert_eq!(
            len,
            16 * 1024 * 1024 + blocks * 4096 + 33 * 512,
            "a fitted disk is exactly its contents plus the backup GPT"
        );
        // And it really is smaller than the authored fixture size, which is the point.
        assert!(
            len < 192 * 1024 * 1024,
            "a few-kilobyte rootfs must fit well under the fixed fixture size, got {len}"
        );
    }

    #[test]
    fn combined_image_has_gpt_rootfs_and_bootloader_at_offsets() {
        // End-to-end (Linux only): ferrosys ext4 format + GPT + splices.
        if !require_host_tools(&["tar"]) {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let work = tmp.path().join("work");
        let out = tmp.path().join("out");
        let rootfs_tar = tmp.path().join("rootfs.tar");
        make_rootfs_tar(tmp.path(), &rootfs_tar);
        // Distinctive payloads to find at their offsets.
        let idb = tmp.path().join("idbloader.img");
        let itb = tmp.path().join("u-boot.itb");
        std::fs::write(&idb, b"IDBLOADER-PAYLOAD").unwrap();
        std::fs::write(&itb, b"UBOOT-ITB-PAYLOAD").unwrap();

        // 192 MiB total: rootfs at 16 MiB leaves ~176 MiB — above the geometry's
        // 128 MiB rootfs minimum, small enough to format quickly.
        let build = small_rk1_build("192MiB");
        let opts = ImageOptions {
            rootfs_tar: &rootfs_tar,
            boot: BootPayload::RockchipRkbin {
                idbloader: &idb,
                uboot_itb: &itb,
            },
            out_dir: &out,
            stem: "turing-rk1-forky",
            work_dir: &work,
            rootfs_label: "rootfs",
            identity: ImageIdentity::derive("test-seed", "turing-rk1"),
            compress: &[],
            keep_raw: false,
            jobs: None,
        };
        let sink = |_: crate::event::Event| {};
        let arts = build_image(pair_of(&build), &opts, &sink).unwrap();
        let image = match &arts.output {
            ImageOutput::Combined { image } => image.clone(),
            other => panic!("expected combined, got {other:?}"),
        };
        assert!(arts.compressed.is_empty());

        // Whole-disk image is exactly the resolved size.
        assert_eq!(std::fs::metadata(&image).unwrap().len(), 192 * 1024 * 1024);

        // Payloads land at their raw-gap byte offsets.
        let bytes = std::fs::read(&image).unwrap();
        let at = |off: usize, tag: &[u8]| assert_eq!(&bytes[off..off + tag.len()], tag);
        at(32 * 1024, b"IDBLOADER-PAYLOAD");
        at(8 * 1024 * 1024, b"UBOOT-ITB-PAYLOAD");
        // Protective MBR signature at 0x1FE, ext4 magic (0xEF53) at partition + 0x438.
        assert_eq!(&bytes[510..512], &[0x55, 0xAA]);
        let ext4_magic = 16 * 1024 * 1024 + 0x438;
        assert_eq!(&bytes[ext4_magic..ext4_magic + 2], &[0x53, 0xEF]);

        // The formatted filesystem must not claim more blocks than its GPT
        // partition holds — a filesystem larger than its device is "bad geometry:
        // block count N exceeds size of device" and will not mount. The geometry
        // sizes the filesystem to exactly the partition; assert the on-disk
        // superblock agrees. s_blocks_count_lo is a little-endian u32 at superblock
        // offset 0x04, and the superblock starts 1024 bytes into the partition.
        let ImageSize::Fixed(total) = parse_image_size(&image_of(&build).image_size).unwrap()
        else {
            unreachable!("the fixture names an explicit size")
        };
        let geom = Geometry::resolve(&build.boot, total).unwrap();
        let sb = 16 * 1024 * 1024 + 1024;
        let blocks_count = u32::from_le_bytes(bytes[sb + 4..sb + 8].try_into().unwrap()) as u64;
        assert_eq!(
            blocks_count,
            geom.rootfs_bytes / 4096,
            "fs block count matches geometry"
        );
        assert!(
            blocks_count * 4096 <= geom.rootfs_length_lba * 512,
            "filesystem ({blocks_count} blocks) must fit its partition ({} sectors)",
            geom.rootfs_length_lba,
        );

        // If `sfdisk` is around, the GPT must be parseable and name the partition —
        // an sfdisk *failure* means a corrupt table and fails the test.
        if crate::hosttool::have("sfdisk") {
            let o = Command::new("sfdisk")
                .arg("-d")
                .arg(&image)
                .output()
                .unwrap();
            assert!(
                o.status.success(),
                "sfdisk -d failed on the image (corrupt GPT?): {}",
                String::from_utf8_lossy(&o.stderr)
            );
            let dump = String::from_utf8_lossy(&o.stdout);
            assert!(dump.contains("label: gpt"), "sfdisk dump: {dump}");
        }
    }

    #[test]
    fn compression_deletes_the_raw_image_unless_kept() {
        // End-to-end (Linux only): compress, then confirm the raw is dropped and
        // only the .xz remains, and that --keep-raw retains it.
        if !require_host_tools(&["tar"]) {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let rootfs_tar = tmp.path().join("rootfs.tar");
        make_rootfs_tar(tmp.path(), &rootfs_tar);
        let idb = tmp.path().join("idbloader.img");
        let itb = tmp.path().join("u-boot.itb");
        std::fs::write(&idb, b"IDB").unwrap();
        std::fs::write(&itb, b"ITB").unwrap();
        let sink = |_: crate::event::Event| {};

        let run = |out: &Path, keep_raw: bool| {
            let opts = ImageOptions {
                rootfs_tar: &rootfs_tar,
                boot: BootPayload::RockchipRkbin {
                    idbloader: &idb,
                    uboot_itb: &itb,
                },
                out_dir: out,
                stem: "turing-rk1-forky",
                work_dir: &out.join("work"),
                rootfs_label: "rootfs",
                identity: ImageIdentity::derive("test-seed", "turing-rk1"),
                compress: &[ImageCompression::Xz],
                keep_raw,
                jobs: None,
            };
            {
                let b = small_rk1_build("192MiB");
                build_image(pair_of(&b), &opts, &sink).unwrap()
            }
        };

        // Default: raw deleted, only .xz remains.
        let out = tmp.path().join("out-default");
        let arts = run(&out, false);
        assert!(arts.raw_removed);
        assert_eq!(arts.compressed.len(), 1);
        assert!(arts.compressed[0].path.exists());
        match &arts.output {
            ImageOutput::Combined { image } => assert!(!image.exists(), "raw should be gone"),
            other => panic!("expected combined, got {other:?}"),
        }

        // --keep-raw: both the raw and the .xz survive.
        let out = tmp.path().join("out-keep");
        let arts = run(&out, true);
        assert!(!arts.raw_removed);
        assert!(arts.compressed[0].path.exists());
        match &arts.output {
            ImageOutput::Combined { image } => assert!(image.exists(), "raw should be kept"),
            other => panic!("expected combined, got {other:?}"),
        }
    }

    #[test]
    fn two_containers_both_land_and_name_the_same_source() {
        // `--compress xz,gz`: one raw image, two artifacts, each recording the raw
        // image it came from so a consumer can group them without counting.
        if !require_host_tools(&["tar"]) {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let rootfs_tar = tmp.path().join("rootfs.tar");
        make_rootfs_tar(tmp.path(), &rootfs_tar);
        let idb = tmp.path().join("idbloader.img");
        let itb = tmp.path().join("u-boot.itb");
        std::fs::write(&idb, b"IDB").unwrap();
        std::fs::write(&itb, b"ITB").unwrap();
        let out = tmp.path().join("out");
        let opts = ImageOptions {
            rootfs_tar: &rootfs_tar,
            boot: BootPayload::RockchipRkbin {
                idbloader: &idb,
                uboot_itb: &itb,
            },
            out_dir: &out,
            stem: "turing-rk1-forky",
            work_dir: &out.join("work"),
            rootfs_label: "rootfs",
            identity: ImageIdentity::derive("test-seed", "turing-rk1"),
            compress: &[ImageCompression::Xz, ImageCompression::Gz],
            keep_raw: false,
            jobs: None,
        };
        let sink = |_: crate::event::Event| {};
        let b = small_rk1_build("192MiB");
        let arts = build_image(pair_of(&b), &opts, &sink).unwrap();

        assert_eq!(arts.compressed.len(), 2);
        // Request order is preserved, so the first entry is the preferred container.
        assert_eq!(arts.compressed[0].format, ImageCompression::Xz);
        assert_eq!(arts.compressed[1].format, ImageCompression::Gz);
        assert_eq!(arts.compressed[0].source, arts.compressed[1].source);
        for c in &arts.compressed {
            assert!(c.path.exists(), "{} missing", c.path.display());
            assert!(c.path.to_string_lossy().ends_with(c.format.extension()));
        }
        // The raw is still dropped: it is derivable from either container.
        assert!(arts.raw_removed);
    }

    #[test]
    fn split_layout_emits_bootloader_and_rootfs_images() {
        if !require_host_tools(&["tar"]) {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let rootfs_tar = tmp.path().join("rootfs.tar");
        make_rootfs_tar(tmp.path(), &rootfs_tar);
        let idb = tmp.path().join("idbloader.img");
        let itb = tmp.path().join("u-boot.itb");
        std::fs::write(&idb, b"IDB").unwrap();
        std::fs::write(&itb, b"ITB").unwrap();

        let mut build = small_rk1_build("192MiB");
        build.layout = Layout::Split;
        let opts = ImageOptions {
            rootfs_tar: &rootfs_tar,
            boot: BootPayload::RockchipRkbin {
                idbloader: &idb,
                uboot_itb: &itb,
            },
            out_dir: &tmp.path().join("out"),
            stem: "turing-rk1-forky",
            work_dir: &tmp.path().join("work"),
            rootfs_label: "rootfs",
            identity: ImageIdentity::derive("test-seed", "turing-rk1"),
            compress: &[],
            keep_raw: false,
            jobs: None,
        };
        let sink = |_: crate::event::Event| {};
        let arts = build_image(pair_of(&build), &opts, &sink).unwrap();
        match &arts.output {
            ImageOutput::Split { bootloader, rootfs } => {
                // Bootloader image is gap-sized with the payloads at their offsets.
                let boot = std::fs::read(bootloader).unwrap();
                assert_eq!(boot.len() as u64, 16 * 1024 * 1024);
                assert_eq!(&boot[32 * 1024..32 * 1024 + 3], b"IDB");
                assert_eq!(&boot[8 * 1024 * 1024..8 * 1024 * 1024 + 3], b"ITB");
                // Rootfs image is full-size with the ext4 magic, no bootloader in the gap.
                let rf = std::fs::metadata(rootfs).unwrap().len();
                assert_eq!(rf, 192 * 1024 * 1024);
                let rfbytes = std::fs::read(rootfs).unwrap();
                assert_eq!(&rfbytes[32 * 1024..32 * 1024 + 3], b"\0\0\0"); // gap empty
                let m = 16 * 1024 * 1024 + 0x438;
                assert_eq!(&rfbytes[m..m + 2], &[0x53, 0xEF]);
            }
            other => panic!("expected split, got {other:?}"),
        }
    }
}
