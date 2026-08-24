//! Rootfs ext4 partition assembly: format the rootfs tarball into a fixed-size,
//! resize-safe ext4 image with the pure-Rust [`ferrosys::ext`] formatter — no mount,
//! no loop device, no root, no `mke2fs`, and no user namespace.
//!
//! The formatter reads the rootfs `tar` in-process through [`ArchiveSource`], taking
//! each entry's ownership, mode, times, extended attributes, and POSIX ACLs straight
//! from the (PAX) headers, and writes those owner ids directly into the inodes. That
//! removes the reason the old path needed a user namespace: nothing is extracted to a
//! staging tree whose multi-uid ownership an unprivileged process cannot set, so the
//! whole step runs as the plain build user.
//!
//! The image is resize-safe by construction: [`GrowReservation::Max`] sizes the
//! reserved group-descriptor-table blocks to the most the format can address (~8 TiB
//! under this feature set, at a cost of ~4 MiB), so first boot grows the mounted root
//! onto a larger NVMe with `resize2fs` and no descriptor-table relocation. The feature
//! set is chosen here rather than left to the formatter's default, but it is expressed
//! relative to that default and so still moves with it: what the image was formatted to
//! is recorded by [`rootfs_filesystem_pin`] and pinned by test, not guaranteed by this
//! module. `metadata_csum_seed` stores the checksum seed in the superblock, decoupled
//! from the UUID, so an operator's `tune2fs -U` (rescue, cloning hygiene) never has to
//! rewrite every metadata checksum.
//!
//! The per-image first-boot password is spliced into `/etc/shadow` here — the one
//! per-build-unique step. The cacheable rootfs tarball leaves the default account
//! locked; the splice rewrites the entry's bytes in the parsed entry list, before the
//! filesystem is written, leaving the entry's ownership and mtime untouched (DET).
//!
//! The finished image is verified two ways: always by re-reading it with the crate's
//! own [`Reader`] and checking every metadata checksum (a failure means the formatter
//! wrote an image its own reader rejects), and — when `e2fsck` is present — by a
//! read-only `e2fsck -fn` cross-check whose any correction fails the build. `e2fsprogs`
//! is no longer required; where it is absent the pure-Rust gate stands alone.
//!
//! The output is the standalone ext4 image the [image orchestrator](super) splices
//! into the whole-disk image at the rootfs partition offset.

use crate::build;
use crate::error::EngineError;
use crate::event::Step;
use crate::image::geometry::EXT4_BLOCK;
use boot2deb_core::provenance::FilesystemProvenance;
use ferrosys::ext::ondisk::Timestamp;
use ferrosys::ext::{
    ArchiveSource, EntryKind, ErrorBehavior, FeatureSet, FormatOptions, GrowReservation, InodeCount,
    Reader, ReservedRatio, Source, SourceEntry, format_to,
};
use sha2::{Digest, Sha256};
use std::num::NonZeroU64;
use std::path::Path;
use std::process::Command;
use uuid::Uuid;

/// One inode per this many bytes of filesystem — the ext4 default, pinned so the
/// inode count does not vary with a library or host default.
const BYTES_PER_INODE: u64 = 16384;

/// Blocks held back for the super-user, in hundredths of one percent: 1%, not the 5%
/// default — enough to keep root-owned services writable when a non-root consumer fills
/// the disk, without 5%'s cost on a grown NVMe.
const RESERVED_HUNDREDTHS: u16 = 100;

/// Everything under `/dev` is dropped from the rootfs: `mknod`-style nodes are
/// unnecessary because the kernel mounts devtmpfs over `/dev` at boot. The `/dev`
/// directory entry itself is kept.
const DEV_PREFIX: &[u8] = b"/dev/";

/// Format `dest` as an ext4 filesystem of exactly `size` bytes holding the rootfs
/// `tarball`'s contents, then verify it.
///
/// `size` must be a multiple of the ext4 block size (the caller's geometry guarantees
/// it). `label` is the ext4 volume label (≤ 16 bytes) the rootfs's `/etc/fstab` mounts
/// by. `uuid` is the deterministic superblock UUID the caller derived from the lock, so
/// a rebuild reproduces it. `first_boot` is the per-image credential spliced into the
/// rootfs's `/etc/shadow` before the filesystem is written.
///
/// Unlike the old `mke2fs` path, the superblock's format times are deterministic too:
/// they take the newest source mtime, which `mmdebstrap` has already clamped to the
/// lock's `SOURCE_DATE_EPOCH`. The per-image first-boot password is still unique per
/// build, so the image as a whole is not byte-for-byte reproducible.
pub(crate) fn build_rootfs_ext4(
    dest: &Path,
    size: u64,
    tarball: &Path,
    label: &str,
    uuid: Uuid,
    first_boot: FirstBoot,
    step: &Step,
) -> Result<(), EngineError> {
    assert!(
        size.is_multiple_of(EXT4_BLOCK),
        "ext4 size must be block-aligned (geometry guarantees this)"
    );
    step.log(format!(
        "formatting {size}-byte ext4 rootfs at {} (ferrosys, pure-Rust: no mke2fs, no userns)",
        dest.display()
    ));

    // 1. Parse the rootfs tar into an entry list in-process. `ArchiveSource` reads
    //    ownership, mode, times, xattrs, ACLs, and device nodes straight from the
    //    (PAX) headers — no privileged extraction, so no user namespace.
    let file = std::fs::File::open(tarball).map_err(|s| EngineError::io(tarball, s))?;
    let mut entries = ArchiveSource::from_reader(std::io::BufReader::new(file))
        .map_err(|e| EngineError::Ext4Format {
            detail: format!("parsing rootfs tar {}: {e}", tarball.display()),
        })?
        .into_entries();

    // 2. Drop everything under /dev (devtmpfs covers it at boot), keeping the /dev
    //    directory itself — matching what the build has always materialized.
    entries.retain(|e| !e.path.starts_with(DEV_PREFIX));

    // 3. Splice the unique per-image first-boot password into /etc/shadow: the one
    //    per-build-unique step, done on the parsed entry rather than a staged file.
    splice_first_boot_password(&mut entries, first_boot)?;
    step.log("spliced the unique per-image first-boot password into /etc/shadow");

    // 4. Format straight into `dest`. `format_to` streams only the blocks it uses into
    //    the (sparse) file and extends it to the full size, so the whole image never
    //    lives in memory; a freshly truncated file gives it the zeroed holes it needs.
    let options = format_options(uuid, label, &entries);
    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(dest)
        .map_err(|s| EngineError::io(dest, s))?;
    format_to(Entries(entries), size, options, &mut out).map_err(|e| EngineError::Ext4Format {
        detail: e.to_string(),
    })?;
    out.sync_all().map_err(|s| EngineError::io(dest, s))?;
    drop(out);

    verify_clean(dest, step)
}

/// The per-image first-boot credential spliced into the rootfs before it is formatted.
/// The default account is created locked in the cacheable rootfs tarball; this rewrites
/// its `/etc/shadow` line with a fresh hash and forces a change at first login.
pub(crate) struct FirstBoot<'a> {
    /// The default account whose locked shadow line receives the hash.
    pub user: &'a str,
    /// A `sha512crypt` (`$6$`) hash from [`crate::secret::crypt_password`].
    pub password_hash: &'a str,
}

/// The options a format is a function of: identity (UUID, deterministic times, hash
/// seed), the pinned feature set, and the size-independent tunables.
fn format_options(uuid: Uuid, label: &str, entries: &[SourceEntry]) -> FormatOptions {
    // A deterministic creation time: the newest source mtime, which mmdebstrap has
    // already clamped to the lock's SOURCE_DATE_EPOCH — so the superblock's format
    // times are a function of the lock, not the wall clock mke2fs stamped. Clamped to
    // the superblock's 32-bit range for safety; real rootfs mtimes are well inside it.
    let time_secs = entries
        .iter()
        .map(|e| e.meta.mtime.secs)
        .max()
        .unwrap_or(0)
        .clamp(0, i64::from(u32::MAX));
    let time = Timestamp::from_secs(time_secs);

    let mut options = FormatOptions::new(uuid.into_bytes(), time, derive_hash_seed(uuid));
    options.feature = feature_set();
    // Reserve the most online-grow headroom the format allows, so a small image grows
    // in place onto a large NVMe at first boot without relocating its descriptor table.
    options.grow = GrowReservation::Max;
    options.inodes =
        InodeCount::BytesPerInode(NonZeroU64::new(BYTES_PER_INODE).expect("nonzero ratio"));
    options.reserved =
        ReservedRatio::from_hundredths_of_percent(RESERVED_HUNDREDTHS).expect("1% is in range");
    // Remount read-only on a detected error, so an inconsistency cannot spread through
    // further writes (the safety policy the old `mke2fs -e remount-ro` set).
    options.errors = ErrorBehavior::RemountReadOnly;
    options.volume_name = volume_name(label);
    options
}

/// The feature set the image is formatted with: the modern ext4 set the target kernel
/// and the online-resize path need — `resize_inode` + `sparse_super` for growth,
/// `metadata_csum` + `metadata_csum_seed` with the checksum seed stored independent of
/// the UUID, extents, `64bit`, `dir_index`, `has_journal` — and `orphan_file` off.
///
/// It is expressed as [`FeatureSet::DEFAULT`] minus one feature, which makes the set
/// **library-dependent**: `DEFAULT` is documented as the formatter's current good ext4
/// configuration and is free to grow, so a feature added there lands in every image
/// with no change here and no compile error. That is what [`rootfs_filesystem_pin`]
/// records into the provenance manifest and what
/// `the_pinned_feature_set_is_exactly_these_words` fails on, so the drift is caught in
/// CI rather than discovered in an image.
fn feature_set() -> FeatureSet {
    FeatureSet::DEFAULT
        .with_feature("orphan_file", false)
        .expect("orphan_file is a known feature name")
}

/// The rootfs filesystem's on-disk contract as provenance data: the feature set this
/// build resolves, projected into the record the manifest carries.
///
/// This is a *resolved* value, not a declared one. It is computed from the same
/// expression the formatter is handed, so the manifest reports the words an image
/// actually carries — including a feature that arrived from the formatter's baseline
/// rather than from anything in this file.
pub fn rootfs_filesystem_pin() -> FilesystemProvenance {
    let f = feature_set();
    FilesystemProvenance {
        kind: "ext4".to_string(),
        features: f.names().into_iter().map(str::to_string).collect(),
        compat: format!("{:#010x}", f.compat.bits()),
        incompat: format!("{:#010x}", f.incompat.bits()),
        ro_compat: format!("{:#010x}", f.ro_compat.bits()),
        block_size: f.block_size,
        inode_size: f.inode_size,
    }
}

/// The 16-byte `s_volume_name`: the label's bytes, truncated to sixteen and NUL-padded.
fn volume_name(label: &str) -> [u8; 16] {
    let mut name = [0u8; 16];
    let bytes = label.as_bytes();
    let n = bytes.len().min(16);
    name[..n].copy_from_slice(&bytes[..n]);
    name
}

/// The directory-hash seed (`s_hash_seed`): a deterministic function of the
/// lock-derived UUID, so a hash-indexed directory's ordering — and the image bytes —
/// do not depend on the build host, while staying distinct from the UUID itself.
fn derive_hash_seed(uuid: Uuid) -> [u8; 16] {
    let mut hasher = Sha256::new();
    hasher.update(b"boot2deb-ext4-hash-seed\0");
    hasher.update(uuid.as_bytes());
    let digest = hasher.finalize();
    let mut seed = [0u8; 16];
    seed.copy_from_slice(&digest[..16]);
    seed
}

/// Rewrite `first_boot.user`'s locked `/etc/shadow` line in the parsed entry list with
/// the per-image hash, before the filesystem is written.
///
/// The unique per-image password is non-reproducible, so the cacheable rootfs tarball
/// leaves the account locked (`{user}:!:…`) and the splice happens here. Only the shadow
/// entry's content bytes change; its mode, ownership, and (epoch-clamped) mtime are the
/// entry's own metadata and are untouched, so the splice reintroduces no build-time
/// state (DET).
fn splice_first_boot_password(
    entries: &mut [SourceEntry],
    first_boot: FirstBoot,
) -> Result<(), EngineError> {
    let shadow = entries
        .iter_mut()
        .find(|e| e.path == b"/etc/shadow")
        .ok_or_else(|| EngineError::ArtifactMissing {
            what: "/etc/shadow".into(),
            location: "rootfs tar".into(),
        })?;
    let EntryKind::File(content) = &shadow.kind else {
        return Err(EngineError::Ext4Format {
            detail: "/etc/shadow in the rootfs tar is not a regular file".into(),
        });
    };
    let current = std::str::from_utf8(content).map_err(|_| EngineError::Ext4Format {
        detail: "/etc/shadow in the rootfs tar is not valid UTF-8".into(),
    })?;
    let spliced =
        crate::rootcache::splice_shadow(current, first_boot.user, first_boot.password_hash)
            .ok_or_else(|| EngineError::ArtifactMissing {
                what: format!("{} account in /etc/shadow", first_boot.user),
                location: "rootfs tar".into(),
            })?;
    shadow.kind = EntryKind::File(spliced.into_bytes());
    Ok(())
}

/// A parsed, post-processed entry list handed to the formatter as a [`Source`].
struct Entries(Vec<SourceEntry>);

impl Source for Entries {
    fn into_entries(self) -> Vec<SourceEntry> {
        self.0
    }
}

/// Verify the finished image: always with the crate's own [`Reader`] (every metadata
/// checksum), and additionally with `e2fsck -fn` when it is available.
///
/// A checksum failure means the formatter wrote an image its own reader rejects; an
/// `e2fsck` correction means the formatter and an independent checker disagree about the
/// layout. Either fails the build — a disagreement that must never ship inside an image.
fn verify_clean(dest: &Path, step: &Step) -> Result<(), EngineError> {
    step.log("verifying ext4 image (ferrosys reader: every metadata checksum)");
    let file = std::fs::File::open(dest).map_err(|s| EngineError::io(dest, s))?;
    let mut reader = Reader::open(file).map_err(|e| EngineError::Ext4Format {
        detail: format!("re-reading the formatted image: {e}"),
    })?;
    reader
        .verify_checksums()
        .map_err(|e| EngineError::Ext4Format {
            detail: format!("metadata checksum verification failed: {e}"),
        })?;

    if have_tool("e2fsck") {
        step.log("cross-checking with e2fsck -fn (any correction fails the build)");
        let mut cmd = Command::new("e2fsck");
        cmd.arg("-fn").arg(dest);
        build::run(cmd, "e2fsck", "e2fsck -fn (verify formatted rootfs)", step)?;
    } else {
        step.log("e2fsck not found; skipping the external cross-check (ferrosys reader verified)");
    }
    Ok(())
}

/// True when a host tool is runnable (a missing binary fails to spawn).
fn have_tool(tool: &str) -> bool {
    Command::new(tool).arg("--version").output().is_ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrosys::ext::Metadata;

    /// True when `tar` is runnable — needed only to build the fixture archive, not to
    /// format it (the formatter is pure Rust). Panics under `BOOT2DEB_REQUIRE_HOST_TOOLS`.
    fn tar_ready() -> bool {
        if have_tool("tar") {
            return true;
        }
        assert!(
            std::env::var_os("BOOT2DEB_REQUIRE_HOST_TOOLS").is_none(),
            "BOOT2DEB_REQUIRE_HOST_TOOLS is set but `tar` is unavailable to build the fixture"
        );
        eprintln!("skipping: tar unavailable to build the fixture");
        false
    }

    /// Little-endian field readers over the superblock (1024 bytes into the image).
    fn sb_u16(img: &[u8], off: usize) -> u16 {
        u16::from_le_bytes(img[1024 + off..1024 + off + 2].try_into().unwrap())
    }
    fn sb_u32(img: &[u8], off: usize) -> u32 {
        u32::from_le_bytes(img[1024 + off..1024 + off + 4].try_into().unwrap())
    }

    /// A shadow entry as `ArchiveSource` would parse it: a `0640` regular file.
    fn shadow_entry(content: &[u8], mtime: Timestamp) -> SourceEntry {
        SourceEntry {
            path: b"/etc/shadow".to_vec(),
            kind: EntryKind::File(content.to_vec()),
            meta: Metadata::new(0o640, mtime),
            xattrs: Vec::new(),
        }
    }

    /// The formatted image must carry the supplied UUID, the resize-critical layout
    /// (`sparse_super` + `resize_inode` with reserved GDT blocks reaching the format
    /// ceiling), the pinned `remount-ro` error policy, and exactly the requested size —
    /// and pass the in-formatter checksum verification `build_rootfs_ext4` runs.
    #[test]
    fn formats_resizable_filesystem_with_the_supplied_uuid() {
        if !tar_ready() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        // A small rootfs tree with root ownership recorded in the tar, as the rootfs
        // stage produces. The account is locked; the image stage splices the first-boot
        // hash into it.
        let root = tmp.path().join("tree");
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::write(root.join("etc/hostname"), b"turing-rk1\n").unwrap();
        std::fs::write(
            root.join("etc/shadow"),
            b"root:*:19000:0:99999:7:::\ndebian:!:19000:0:99999:7:::\n",
        )
        .unwrap();
        let tar = tmp.path().join("rootfs.tar");
        let status = Command::new("tar")
            .args(["--owner=0", "--group=0", "--numeric-owner", "-C"])
            .arg(&root)
            .arg("-cf")
            .arg(&tar)
            .arg(".")
            .status()
            .unwrap();
        assert!(status.success(), "tar failed");

        let img = tmp.path().join("rootfs.ext4");
        let uuid = Uuid::from_bytes([0x5a; 16]);
        let size: u64 = 64 * 1024 * 1024;
        let sink = |_: crate::event::Event| {};
        let step = Step::start(&sink, "image");
        let first_boot = FirstBoot {
            user: "debian",
            password_hash: "$6$saltsalt$0123456789abcdef",
        };
        build_rootfs_ext4(&img, size, &tar, "rootfs", uuid, first_boot, &step).unwrap();

        let bytes = std::fs::read(&img).unwrap();
        // s_uuid at superblock offset 0x68.
        assert_eq!(&bytes[1024 + 0x68..1024 + 0x78], uuid.as_bytes());
        // s_blocks_count_lo at 0x04: exactly the requested size.
        assert_eq!(sb_u32(&bytes, 0x04) as u64, size / EXT4_BLOCK);
        // s_feature_compat at 0x5C: RESIZE_INODE (0x0010) + HAS_JOURNAL (0x0004).
        let compat = sb_u32(&bytes, 0x5C);
        assert_ne!(compat & 0x0010, 0, "resize_inode must be set");
        assert_ne!(compat & 0x0004, 0, "has_journal must be set");
        // s_feature_ro_compat at 0x64: SPARSE_SUPER (0x0001).
        assert_ne!(sb_u32(&bytes, 0x64) & 0x0001, 0, "sparse_super must be set");
        // s_feature_incompat at 0x60: CSUM_SEED (0x2000) — the checksum seed lives in
        // the superblock, decoupled from the UUID, so a UUID change never rewrites
        // every metadata checksum.
        assert_ne!(
            sb_u32(&bytes, 0x60) & 0x2000,
            0,
            "metadata_csum_seed must be set"
        );
        // s_errors at 0x3c: 2 (remount-ro), the safety policy pinned for the image.
        assert_eq!(sb_u16(&bytes, 0x3c), 2, "errors must be remount-ro");
        // s_reserved_gdt_blocks at 0xCE: the online-resize headroom reaches the format
        // ceiling — 1024 GDT blocks (8 TiB) under this feature set, one of which this
        // small filesystem already uses, so at least 1023 are reserved.
        assert!(
            sb_u16(&bytes, 0xCE) >= 1023,
            "reserved GDT blocks must reach the resize ceiling, got {}",
            sb_u16(&bytes, 0xCE)
        );
    }

    #[test]
    fn splice_first_boot_password_rewrites_the_shadow_entry() {
        let mtime = Timestamp::from_secs(1_600_000_000);
        let mut entries = vec![
            shadow_entry(
                b"root:*:19000:0:99999:7:::\ndebian:!:19000:0:99999:7:::\n",
                mtime,
            ),
            SourceEntry {
                path: b"/etc/hostname".to_vec(),
                kind: EntryKind::File(b"turing-rk1\n".to_vec()),
                meta: Metadata::new(0o644, mtime),
                xattrs: Vec::new(),
            },
        ];
        let first_boot = FirstBoot {
            user: "debian",
            password_hash: "$6$saltsalt$hashhashhash",
        };
        splice_first_boot_password(&mut entries, first_boot).unwrap();

        let EntryKind::File(content) = &entries[0].kind else {
            panic!("shadow is a file");
        };
        let out = std::str::from_utf8(content).unwrap();
        // The debian line carries the hash and is expired (field 3 = 0); root is untouched.
        assert!(
            out.contains("debian:$6$saltsalt$hashhashhash:0:0:99999:7:::"),
            "spliced line missing, got: {out}"
        );
        assert!(out.contains("root:*:19000:0:99999:7:::"), "root line preserved");
        // Only the content changes: the entry's mode and mtime are its own metadata.
        assert_eq!(entries[0].meta.mode, 0o640, "shadow mode preserved");
        assert_eq!(entries[0].meta.mtime, mtime, "shadow mtime preserved");
    }

    #[test]
    fn splice_first_boot_password_errors_when_the_account_is_absent() {
        let mut entries = vec![shadow_entry(b"root:*:19000:0:99999:7:::\n", Timestamp::from_secs(1))];
        let first_boot = FirstBoot {
            user: "debian",
            password_hash: "$6$x$y",
        };
        let err = splice_first_boot_password(&mut entries, first_boot).unwrap_err();
        assert!(
            matches!(err, EngineError::ArtifactMissing { what, .. } if what.contains("debian account")),
            "expected a missing-account error"
        );
    }

    #[test]
    fn splice_first_boot_password_errors_when_shadow_is_absent() {
        let mut entries = vec![SourceEntry {
            path: b"/etc/hostname".to_vec(),
            kind: EntryKind::File(b"turing-rk1\n".to_vec()),
            meta: Metadata::new(0o644, Timestamp::from_secs(1)),
            xattrs: Vec::new(),
        }];
        let first_boot = FirstBoot {
            user: "debian",
            password_hash: "$6$x$y",
        };
        let err = splice_first_boot_password(&mut entries, first_boot).unwrap_err();
        assert!(
            matches!(err, EngineError::ArtifactMissing { what, .. } if what == "/etc/shadow"),
            "expected a missing-/etc/shadow error"
        );
    }

    #[test]
    fn the_volume_label_is_truncated_and_nul_padded() {
        assert_eq!(&volume_name("rootfs"), b"rootfs\0\0\0\0\0\0\0\0\0\0");
        // Over sixteen bytes is truncated, as mke2fs truncates -L.
        assert_eq!(&volume_name("0123456789abcdefghij"), b"0123456789abcdef");
    }

    #[test]
    fn the_pinned_feature_set_matches_the_on_disk_contract() {
        let f = feature_set();
        assert!(f.has_resize_inode(), "resize_inode for online growth");
        assert!(f.is_sparse_super(), "sparse_super");
        assert!(f.has_journal(), "has_journal");
        assert!(f.has_metadata_csum(), "metadata_csum");
        assert!(f.has_csum_seed(), "metadata_csum_seed (seed decoupled from the UUID)");
        assert!(f.has_extents(), "extents");
        assert!(f.is_64bit(), "64bit");
        assert!(f.has_dir_index(), "dir_index");
        // orphan_file is intentionally off, matching the validated mke2fs feature set.
        assert!(!f.has_orphan_file(), "orphan_file pinned off");
        assert_eq!(f.block_size, EXT4_BLOCK as u32);
        assert_eq!(f.inode_size, 256);
    }

    /// The exact on-disk contract, pinned as bytes rather than as predicates.
    ///
    /// The test above asserts what the set must *contain*, which by construction cannot
    /// fail on a feature the formatter's baseline gains: [`feature_set`] is
    /// `FeatureSet::DEFAULT` minus one feature, and `DEFAULT` is documented as free to
    /// grow. So a formatter upgrade can change every image's layout while every
    /// predicate above still passes. This is the assertion that fails instead.
    ///
    /// A failure here is not necessarily a defect — it means the on-disk layout moved,
    /// and the decision (adopt the new feature, or pin it off in [`feature_set`]) has to
    /// be made deliberately. Update these words only together with that decision.
    #[test]
    fn the_pinned_feature_set_is_exactly_these_words() {
        let pin = rootfs_filesystem_pin();
        assert_eq!(pin.kind, "ext4");
        assert_eq!(pin.compat, "0x0000003c");
        assert_eq!(pin.incompat, "0x000022c2");
        assert_eq!(pin.ro_compat, "0x0000046b");
        assert_eq!(pin.block_size, 4096);
        assert_eq!(pin.inode_size, 256);
        assert_eq!(
            pin.features,
            [
                // compat
                "has_journal",
                "ext_attr",
                "resize_inode",
                "dir_index",
                // incompat
                "filetype",
                "extent",
                "64bit",
                "flex_bg",
                "metadata_csum_seed",
                // ro_compat
                "sparse_super",
                "large_file",
                "huge_file",
                "dir_nlink",
                "extra_isize",
                "metadata_csum",
            ]
        );
    }

    /// The names and the raw words are two spellings of one set, so they cannot be
    /// allowed to drift apart: a name the formatter renames would change `features`
    /// while every word stayed put, and the pin above would still pass on the half a
    /// human reads. This checks they agree by round-tripping the names back to bits.
    #[test]
    fn the_pinned_names_and_raw_words_describe_the_same_set() {
        use ferrosys::ext::{Compat, Incompat, RoCompat};
        let pin = rootfs_filesystem_pin();
        // Start from no features at all, so the result is what the names say and
        // nothing the formatter's baseline contributed.
        let empty = FeatureSet {
            compat: Compat::from_bits(0),
            incompat: Incompat::from_bits(0),
            ro_compat: RoCompat::from_bits(0),
            block_size: pin.block_size,
            inode_size: pin.inode_size,
        };
        // Every recorded name is one the formatter still knows...
        let rebuilt = pin
            .features
            .iter()
            .try_fold(empty, |acc, name| acc.with_feature(name, true))
            .expect("every pinned feature name is known to the formatter");
        // ...and the set they name is the set the words pin.
        assert_eq!(format!("{:#010x}", rebuilt.compat.bits()), pin.compat);
        assert_eq!(format!("{:#010x}", rebuilt.incompat.bits()), pin.incompat);
        assert_eq!(format!("{:#010x}", rebuilt.ro_compat.bits()), pin.ro_compat);
    }

    #[test]
    fn the_hash_seed_is_deterministic_and_distinct_from_the_uuid() {
        let uuid = Uuid::from_bytes([0x5a; 16]);
        // Deterministic: the same UUID yields the same seed.
        assert_eq!(derive_hash_seed(uuid), derive_hash_seed(uuid));
        // Distinct from the UUID bytes, and distinct for a different UUID.
        assert_ne!(derive_hash_seed(uuid), *uuid.as_bytes());
        assert_ne!(derive_hash_seed(uuid), derive_hash_seed(Uuid::from_bytes([0x5b; 16])));
    }
}
