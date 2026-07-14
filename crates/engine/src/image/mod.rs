//! The image node — unprivileged image assembly with no loop mount, no `dd`,
//! and no `sudo`.
//!
//! It takes a rootfs tarball plus the boot method's payload and writes a
//! bootable disk image with no `sudo`, no loop device, and no mount: the ext4
//! filesystem is formatted by host `mke2fs -d` from a tree staged inside an
//! unprivileged user namespace (the `ext4` submodule), the partition table is written
//! in Rust (`gpt`), the boot payload is placed by seek+write, and the result is
//! `.xz`-compressed with a pure-Rust encoder (`lzma-rust2`). All byte/LBA arithmetic
//! is resolved and validated up front by the `geometry` submodule.
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

use crate::error::EngineError;
use crate::event::{EventSink, Step};
use boot2deb_core::model::{Layout, ResolvedBoot};
use boot2deb_core::ResolvedBuild;
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
    /// Scratch directory for the intermediate ext4 partition image.
    pub work_dir: &'a Path,
    /// ext4 volume label and GPT partition name (≤ 16 bytes), e.g. `rootfs`.
    pub rootfs_label: &'a str,
    /// The image's deterministic on-disk identifiers ([`ImageIdentity`]).
    pub identity: ImageIdentity,
    /// Also emit a `.xz` alongside each raw image.
    pub compress: bool,
    /// Keep the raw `.img` after compressing it. Default (`false`): with
    /// compression on, the raw image is derivable from the `.xz`, so it is deleted
    /// once compression succeeds to save disk on the largest artifact.
    /// Ignored when `compress` is off.
    pub keep_raw: bool,
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
    /// The ChromeOS kernel partition's GUID. Unused under a boot method that writes
    /// no such partition.
    pub kpart_guid: Uuid,
}

impl ImageIdentity {
    /// Derive every identifier from a lock-stable `seed` and the `device`.
    ///
    /// `seed` identifies the build point (the recipe), so two images of the same
    /// recipe reproduce each other and two different recipes — `asus-c201-forky` and
    /// `asus-c201-trixie`, say — never collide on a PARTUUID, which would make two
    /// cards indistinguishable to a kernel that has both in front of it.
    pub fn derive(seed: &str, device: &str) -> Self {
        ImageIdentity {
            ext4_uuid: derive_uuid(seed, device, "ext4-rootfs"),
            disk_guid: derive_uuid(seed, device, "gpt-disk"),
            rootfs_partuuid: derive_uuid(seed, device, "gpt-partition"),
            kpart_guid: derive_uuid(seed, device, "gpt-kernel-partition"),
        }
    }
}

/// The image artifact(s) produced, per the resolved [`Layout`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageOutput {
    /// One whole-disk image with the bootloader in the raw gap.
    Combined {
        /// The `<device>.img` file.
        image: PathBuf,
    },
    /// Separate bootloader and rootfs images for a two-medium install.
    Split {
        /// `<device>-boot.img` — raw bootloader payloads for the boot medium.
        bootloader: PathBuf,
        /// `<device>-rootfs.img` — GPT + rootfs partition, bootloader-agnostic.
        rootfs: PathBuf,
    },
}

impl ImageOutput {
    /// The raw image files, in a stable order — the inputs to compression.
    fn images(&self) -> Vec<&Path> {
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
    /// The `.xz` artifacts (one per raw image), empty when compression was off.
    pub compressed: Vec<PathBuf>,
    /// Whether the raw image files were deleted after compression, so a
    /// consumer knows only the `.xz` remains.
    pub raw_removed: bool,
    /// The per-image first-boot password spliced into [`crate::rootfs::DEFAULT_USER`]'s
    /// account — unique per build, expired so it must be changed at first
    /// login. The caller surfaces it and records it in the provenance manifest; it
    /// is written to no committed file.
    pub password: String,
}

/// Validate the resolved build's image geometry (offsets, size, GPT/rootfs fit)
/// without writing anything — the cheap up-front check `build` runs right after
/// resolution so a bad layout fails before any stage compiles (COR-10).
pub fn validate_geometry(build: &ResolvedBuild) -> Result<(), EngineError> {
    Geometry::resolve(&build.boot, &build.image_size).map(|_| ())
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
    build: &ResolvedBuild,
    opts: &ImageOptions,
    sink: &dyn EventSink,
) -> Result<ImageArtifacts, EngineError> {
    let step = Step::start(sink, "image");
    let geom = Geometry::resolve(&build.boot, &build.image_size)?;
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
    // formatting the whole rootfs (COR-10).
    let payloads = boot_payloads(&opts.boot, kpart.as_deref())?;
    geom.check_payload_fit(&payloads)?;

    // The per-image first-boot password: generated here so the shared,
    // cacheable rootfs tarball stays password-free (the account is locked in it)
    // and each built image gets its own credential — spliced into the staged
    // `/etc/shadow` before formatting, not surgically into the tar.
    let password = crate::secret::generate_password()?;
    let password_hash = crate::secret::crypt_password(&password)?;

    // The ext4 rootfs partition is identical across layouts — build it once.
    let ext4 = opts.work_dir.join("rootfs.ext4");
    ext4::build_rootfs_ext4(
        &ext4,
        geom.rootfs_bytes,
        opts.rootfs_tar,
        opts.rootfs_label,
        opts.identity.ext4_uuid,
        ext4::FirstBoot {
            user: crate::rootfs::DEFAULT_USER,
            password_hash: &password_hash,
        },
        &step,
    )?;
    step.progress(50);

    let output = match build.layout {
        Layout::Combined => {
            let image = opts.out_dir.join(format!("{}.img", build.device));
            assemble_disk(&image, &geom, &ext4, kpart.as_deref(), true, opts, &step)?;
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
            let BootPayload::RockchipRkbin { idbloader, uboot_itb } = opts.boot else {
                return Err(EngineError::StageNotApplicable {
                    stage: "image (split layout)",
                    why: "no bootloader payloads were supplied",
                });
            };
            // Rootfs image: GPT + rootfs partition, empty raw gap (bootloader-agnostic).
            let rootfs = opts.out_dir.join(format!("{}-rootfs.img", build.device));
            assemble_disk(&rootfs, &geom, &ext4, None, false, opts, &step)?;
            // Bootloader image: just the raw-gap payloads on a gap-sized medium.
            let bootloader = opts.out_dir.join(format!("{}-boot.img", build.device));
            assemble_bootloader(&bootloader, &geom, idbloader, uboot_itb, &step)?;
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
    if opts.compress {
        for image in output.images() {
            let dst = append_xz(image);
            compress_xz(image, &dst, &step)?;
            step.log(format!("compressed {}", dst.display()));
            compressed.push(dst);
        }
        // The raw image is derivable from its `.xz`, so drop it unless asked to keep
        // it — it is the largest artifact.
        if !opts.keep_raw {
            for image in output.images() {
                std::fs::remove_file(image).map_err(|s| EngineError::io(image, s))?;
            }
            raw_removed = true;
            step.log("removed raw image(s); keeping .xz only");
        }
    }

    step.progress(100);
    step.finish();
    Ok(ImageArtifacts {
        output,
        compressed,
        raw_removed,
        password,
    })
}

/// Assemble just the bootloader image from the u-boot payloads, returning its
/// path — a flashable, GPT-less raw medium sized to the raw gap, holding
/// `idbloader.img` and `u-boot.itb` at their offsets.
///
/// Unlike [`build_image`] this needs no rootfs, so a `--stage uboot` run can emit
/// a directly-flashable boot medium — an eMMC (or SPI) that chain-loads the OS
/// from a separate disk — without bootstrapping a Debian rootfs first. The image
/// is the same `<device>-boot.img` the [`Split`](Layout::Split) layout produces.
/// It is left raw and uncompressed: gap-sized (a few MiB) and written straight to
/// the medium, so `.xz` would only add a decompress step before flashing.
pub fn build_bootloader_image(
    build: &ResolvedBuild,
    idbloader: &Path,
    uboot_itb: &Path,
    out_dir: &Path,
    sink: &dyn EventSink,
) -> Result<PathBuf, EngineError> {
    let step = Step::start(sink, "bootloader-image");
    let geom = Geometry::resolve(&build.boot, &build.image_size)?;
    // The same fail-fast fit check the full image node runs before placing bytes.
    geom.check_payload_fit(&[
        ("idbloader.img", file_len(idbloader)?),
        ("u-boot.itb", file_len(uboot_itb)?),
    ])?;
    std::fs::create_dir_all(out_dir).map_err(|s| EngineError::io(out_dir, s))?;
    let image = out_dir.join(format!("{}-boot.img", build.device));
    assemble_bootloader(&image, &geom, idbloader, uboot_itb, &step)?;
    step.log(format!("wrote bootloader image {}", image.display()));
    step.finish();
    Ok(image)
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

/// Write a whole-disk image: a full-size file, the GPT table, the rootfs ext4
/// filesystem spliced at its partition offset, and — when `with_boot` — the boot
/// method's payload. Shared by combined (with the boot payload) and the split rootfs
/// image (without).
fn assemble_disk(
    image: &Path,
    geom: &Geometry,
    ext4: &Path,
    kpart: Option<&Path>,
    with_boot: bool,
    opts: &ImageOptions,
    step: &Step,
) -> Result<(), EngineError> {
    create_sized_image(image, geom.total_size)?;
    gpt::write_table(
        image,
        geom,
        opts.rootfs_label,
        opts.identity.disk_guid,
        opts.identity.rootfs_partuuid,
        opts.identity.kpart_guid,
    )?;
    splice_file(image, geom.rootfs_off, ext4)?;
    if with_boot {
        match (&geom.boot, &opts.boot) {
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
            (BootGeometry::Kpart { offset, .. }, BootPayload::Depthcharge) => {
                let kpart = kpart.ok_or(EngineError::StageNotApplicable {
                    stage: "image",
                    why: "the signed kernel partition was not extracted from the rootfs",
                })?;
                splice_file(image, *offset, kpart)?;
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
        if with_boot { " + boot payload" } else { "" },
        image.display()
    ));
    Ok(())
}

/// Write a bootloader-only image: a raw medium sized to the gap, holding just the
/// two payloads at their offsets (no GPT — this medium carries only the
/// bootloader). Shared by the split layout and [`build_bootloader_image`].
fn assemble_bootloader(
    image: &Path,
    geom: &Geometry,
    idbloader: &Path,
    uboot_itb: &Path,
    step: &Step,
) -> Result<(), EngineError> {
    let BootGeometry::RawGap {
        idbloader_off,
        uboot_itb_off,
    } = geom.boot
    else {
        return Err(EngineError::StageNotApplicable {
            stage: "bootloader-image",
            why: "this boot method writes no bootloader into a raw gap",
        });
    };
    create_sized_image(image, geom.rootfs_off)?;
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
            dst.write_all(chunk).map_err(|s| EngineError::io(image, s))?;
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
/// encode across the host's cores ([`XzWriterMt`], one block per worker); a small
/// input degenerates to a single block. The container is standard `.xz` either
/// way.
fn compress_xz(src: &Path, dst: &Path, step: &Step) -> Result<(), EngineError> {
    let workers = std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(1) as u32;
    step.log(format!(
        "compressing {} -> {} (xz preset {XZ_PRESET}, {workers} worker(s))",
        src.display(),
        dst.display()
    ));
    let input = std::fs::File::open(src).map_err(|s| EngineError::io(src, s))?;
    let output = std::fs::File::create(dst).map_err(|s| EngineError::io(dst, s))?;
    let mut opts = XzOptions::with_preset(XZ_PRESET);
    // MT requires an explicit block size — it is the work-unit boundary.
    opts.set_block_size(Some(NonZeroU64::new(XZ_BLOCK_SIZE).expect("block size is non-zero")));
    let mut writer =
        XzWriterMt::new(output, opts, workers).map_err(|s| EngineError::io(dst, s))?;
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

/// `foo.img` → `foo.img.xz`.
fn append_xz(path: &Path) -> PathBuf {
    let mut s = path.as_os_str().to_owned();
    s.push(".xz");
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
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
        let mut b = resolve_recipe(&repo_root(), "turing-rk1-forky", &Overrides::default()).unwrap();
        b.image_size = image_size.to_string();
        b
    }

    /// True when a host tool is runnable — the image path is Linux-only, so tests
    /// that need e2fsprogs/tar skip cleanly where it is absent.
    ///
    /// Presence is detected by whether the probe **spawns** (a missing binary fails to
    /// exec with `ENOENT`), not by its exit status: some present tools — e2fsprogs
    /// binaries — reject `--version` and exit non-zero, so a `status.success()` check
    /// would wrongly report them absent and silently skip the end-to-end image tests
    /// even on a capable host.
    fn have(tool: &str) -> bool {
        Command::new(tool).arg("--version").output().is_ok()
    }

    /// Whether the end-to-end image path can run: every tool in `tools` is
    /// runnable, and — since the ext4 step stages inside a user namespace — the
    /// exact `unshare` invocation it uses works (binary presence alone does not
    /// imply subuid ranges are configured). When something is missing the behavior
    /// depends on `BOOT2DEB_REQUIRE_HOST_TOOLS`: a CI job that guarantees the tools
    /// sets it, and a miss then **panics** so the most important image assertions
    /// cannot silently drop out of the run; unset (a tool-minimal dev
    /// host), the caller skips with a printed note.
    fn require_host_tools(tools: &[&str]) -> bool {
        let mut missing: Vec<String> =
            tools.iter().filter(|t| !have(t)).map(|t| t.to_string()).collect();
        if missing.is_empty() {
            let userns_ok = Command::new("unshare")
                .args(["--map-root-user", "--map-auto", "true"])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if !userns_ok {
                missing.push("unshare --map-root-user --map-auto (subuid ranges)".into());
            }
        }
        if missing.is_empty() {
            return true;
        }
        assert!(
            std::env::var_os("BOOT2DEB_REQUIRE_HOST_TOOLS").is_none(),
            "BOOT2DEB_REQUIRE_HOST_TOOLS is set but required host tools are missing: \
             {missing:?} — this CI job must provide them so the end-to-end image tests \
             do not skip"
        );
        eprintln!("skipping: required host tools unavailable: {missing:?}");
        false
    }

    /// Build a tiny rootfs tarball (a few dirs + files) at `path`.
    fn make_rootfs_tar(dir: &Path, path: &Path) {
        let root = dir.join("rootfs");
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::create_dir_all(root.join("usr/bin")).unwrap();
        std::fs::write(root.join("etc/hostname"), b"turing-rk1\n").unwrap();
        std::fs::write(root.join("usr/bin/true"), b"#!/bin/true\n").unwrap();
        // The default account is locked in the tarball; the image stage splices the
        // per-image first-boot hash into it before formatting.
        std::fs::write(
            root.join("etc/shadow"),
            b"root:*:19000:0:99999:7:::\ndebian:!:19000:0:99999:7:::\n",
        )
        .unwrap();
        let out = std::fs::File::create(path).unwrap();
        // Record root ownership like the real rootfs tar (mmdebstrap emits uid 0),
        // so the userns extraction maps it back to the build user — which the image
        // stage's in-place shadow splice needs to be able to rewrite the file.
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
    fn append_xz_adds_suffix() {
        assert_eq!(append_xz(Path::new("/o/turing-rk1.img")), Path::new("/o/turing-rk1.img.xz"));
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
        compress_xz(&src, &xz, &step).unwrap();

        let out = Command::new("xz").args(["-dc"]).arg(&xz).output().unwrap();
        assert!(out.status.success(), "xz -d failed: {}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(out.stdout, payload);
    }

    #[test]
    fn bootloader_image_is_gap_sized_with_payloads_and_no_gpt() {
        // No ext4/rootfs here — pure geometry + splice — so this runs on any host
        // (no tar/mke2fs gate), unlike the whole-disk tests below.
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("out");
        let idb = tmp.path().join("idbloader.img");
        let itb = tmp.path().join("u-boot.itb");
        std::fs::write(&idb, b"IDBLOADER-PAYLOAD").unwrap();
        std::fs::write(&itb, b"UBOOT-ITB-PAYLOAD").unwrap();

        let build = small_rk1_build("192MiB");
        let sink = |_: crate::event::Event| {};
        let image = build_bootloader_image(&build, &idb, &itb, &out, &sink).unwrap();

        // Named after the device, and sized to the raw gap (rootfs offset = 16 MiB),
        // NOT the 48 MiB image size — this medium carries only the bootloader.
        assert_eq!(image.file_name().unwrap(), "turing-rk1-boot.img");
        assert_eq!(std::fs::metadata(&image).unwrap().len(), 16 * 1024 * 1024);

        let bytes = std::fs::read(&image).unwrap();
        let at = |off: usize, tag: &[u8]| assert_eq!(&bytes[off..off + tag.len()], tag);
        at(32 * 1024, b"IDBLOADER-PAYLOAD");
        at(8 * 1024 * 1024, b"UBOOT-ITB-PAYLOAD");
        // No GPT: the protective-MBR signature slot stays zero (the combined and
        // rootfs images write 0x55AA there; this one must not).
        assert_eq!(&bytes[510..512], &[0x00, 0x00]);
    }

    #[test]
    fn combined_image_has_gpt_rootfs_and_bootloader_at_offsets() {
        // End-to-end (Linux only): userns staging + mke2fs + GPT + splices.
        if !require_host_tools(&["tar", "unshare", "mke2fs", "e2fsck", "openssl"]) {
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
            work_dir: &work,
            rootfs_label: "rootfs",
            identity: ImageIdentity::derive("test-seed", "turing-rk1"),
            compress: false,
            keep_raw: false,
        };
        let sink = |_: crate::event::Event| {};
        let arts = build_image(&build, &opts, &sink).unwrap();
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
        let geom = Geometry::resolve(&build.boot, &build.image_size).unwrap();
        let sb = 16 * 1024 * 1024 + 1024;
        let blocks_count = u32::from_le_bytes(bytes[sb + 4..sb + 8].try_into().unwrap()) as u64;
        assert_eq!(blocks_count, geom.rootfs_bytes / 4096, "fs block count matches geometry");
        assert!(
            blocks_count * 4096 <= geom.rootfs_length_lba * 512,
            "filesystem ({blocks_count} blocks) must fit its partition ({} sectors)",
            geom.rootfs_length_lba,
        );

        // If `sfdisk` is around, the GPT must be parseable and name the partition —
        // an sfdisk *failure* means a corrupt table and fails the test (MNT-8).
        if have("sfdisk") {
            let o = Command::new("sfdisk").arg("-d").arg(&image).output().unwrap();
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
        if !require_host_tools(&["tar", "unshare", "mke2fs", "e2fsck", "openssl"]) {
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
                work_dir: &out.join("work"),
                rootfs_label: "rootfs",
                identity: ImageIdentity::derive("test-seed", "turing-rk1"),
                compress: true,
                keep_raw,
            };
            build_image(&small_rk1_build("192MiB"), &opts, &sink).unwrap()
        };

        // Default: raw deleted, only .xz remains.
        let out = tmp.path().join("out-default");
        let arts = run(&out, false);
        assert!(arts.raw_removed);
        assert_eq!(arts.compressed.len(), 1);
        assert!(arts.compressed[0].exists());
        match &arts.output {
            ImageOutput::Combined { image } => assert!(!image.exists(), "raw should be gone"),
            other => panic!("expected combined, got {other:?}"),
        }

        // --keep-raw: both the raw and the .xz survive.
        let out = tmp.path().join("out-keep");
        let arts = run(&out, true);
        assert!(!arts.raw_removed);
        assert!(arts.compressed[0].exists());
        match &arts.output {
            ImageOutput::Combined { image } => assert!(image.exists(), "raw should be kept"),
            other => panic!("expected combined, got {other:?}"),
        }
    }

    #[test]
    fn split_layout_emits_bootloader_and_rootfs_images() {
        if !require_host_tools(&["tar", "unshare", "mke2fs", "e2fsck", "openssl"]) {
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
            work_dir: &tmp.path().join("work"),
            rootfs_label: "rootfs",
            identity: ImageIdentity::derive("test-seed", "turing-rk1"),
            compress: false,
            keep_raw: false,
        };
        let sink = |_: crate::event::Event| {};
        let arts = build_image(&build, &opts, &sink).unwrap();
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
