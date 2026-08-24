//! Rootfs ext4 partition assembly: format the rootfs tarball into a fixed-size,
//! resize-safe ext4 image with the pure-Rust [`ferrosys::ext`] formatter — no mount,
//! no loop device, no root, no `mke2fs`, and no user namespace.
//!
//! The formatter reads the rootfs `tar` in-process through [`ArchiveSource`], taking
//! each entry's ownership, mode, times, extended attributes, and POSIX ACLs straight
//! from the (PAX) headers, and writes those owner ids directly into the inodes. That
//! removes the reason the old path needed a user namespace: nothing is extracted to a
//! staging tree whose multi-uid ownership an unprivileged process cannot set, so the
//! whole step runs as the plain build user. Only the headers are read up front: each
//! file's bytes stay in the archive until that file is placed, so a multi-gigabyte
//! rootfs costs the largest single file in it rather than the sum.
//!
//! The image is resize-safe by construction: [`GrowReservation::Max`] sizes the
//! reserved group-descriptor-table blocks to the most the format can address (~8 TiB
//! under this feature set, at a cost of ~4 MiB), so first boot grows the mounted root
//! onto a larger NVMe with `resize2fs` and no descriptor-table relocation. (`Max` never
//! spends more than a sixty-fourth of the filesystem on headroom, so a filesystem below
//! 256 MiB reserves proportionally less; every shipped image is well past that knee.)
//! The feature set is chosen here rather than left to the formatter's default, and it is
//! expressed relative to that default, which is itself a fixed set — so the on-disk
//! contract is stable across formatter releases. It is recorded rather than assumed
//! either way: the provenance manifest's `[filesystem]` carries the formatter's own
//! policy pin (every format option by name), its geometry pin planned at a fixed
//! reference size (what those options *lay out*, which is what catches a change to the
//! formula behind an unchanged option name), and the geometry this image's own size
//! realized — see [`filesystem_provenance`].
//! `metadata_csum_seed` stores the checksum seed in the superblock, decoupled
//! from the UUID, so an operator's `tune2fs -U` (rescue, cloning hygiene) never has to
//! rewrite every metadata checksum.
//!
//! The per-image first-boot password is spliced into `/etc/shadow` here — the one
//! per-build-unique step. The cacheable rootfs tarball leaves the default account
//! locked; the splice rewrites the entry's bytes in the parsed entry list, before the
//! filesystem is written, leaving the entry's ownership and mtime untouched (DET).
//!
//! The finished image is verified two ways: always by re-reading it with the crate's
//! own [`Reader`] and scanning it whole (a finding means the formatter wrote an image
//! its own reader disagrees with), and — when a new enough `e2fsck` is present — by a
//! read-only `e2fsck -fn` cross-check whose any correction fails the build. The two are
//! not redundant, and the second is not a weaker copy of the first: the scan is deeper
//! but it is one implementation checking itself, so a wrong shared assumption passes it,
//! and `e2fsck` is the only reader here that does not share the writer's code.
//! `e2fsprogs` is not required; where it is absent the pure-Rust gate stands alone and
//! the image's provenance records that only it ran.
//!
//! "New enough" is [`E2FSCK_MIN`], and the floor is not a formality. The pinned feature
//! set includes `metadata_csum_seed`, which e2fsprogs only learned in 1.43 — an older
//! `e2fsck` rejects the image over a feature it does not know, so without the floor a
//! host carrying an ancient `e2fsck` fails a build that a host with **no** `e2fsck`
//! completes. An optional cross-check must never be worse than absent, so below the
//! floor it is skipped with a log line.
//!
//! The output is the standalone ext4 image the [image orchestrator](super) splices
//! into the whole-disk image at the rootfs partition offset.

use crate::build;
use crate::error::EngineError;
use crate::event::Step;
use crate::hosttool;
use crate::image::geometry::EXT4_BLOCK;
use boot2deb_core::provenance::{FilesystemGeometry, FilesystemProvenance};
use ferrosys::ext::ondisk::Timestamp;
use ferrosys::ext::{
    ArchiveSource, EntryKind, ErrorBehavior, FeatureSet, FileContent, FormatOptions, FormatPlan,
    GrowReservation, InodeCount, Layout, Location, Reader, ReservedRatio, ScanReport, Slack,
    Source, SourceEntry, TreeBuilder,
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

/// The size the reference geometry pin is planned at
/// ([`FilesystemProvenance::reference_geometry_pin`]) — 4 GiB.
///
/// **Chosen once; never move it.** Its whole value is that it is the *same* size in
/// every build's record, so a difference between two records is a difference in what the
/// formatter does rather than in what it was asked to make. Changing it would move every
/// number in the pin at once and say nothing.
///
/// 4 GiB rather than something smaller because the pin should exercise the regime
/// shipped images are formatted in. [`GrowReservation::Max`] is
/// `min(map ceiling, descriptor ceiling, total_blocks / 64)`, so below about 256 MiB the
/// proportional term binds and the reservation shrinks with the filesystem; past that
/// knee the ceilings bind, which is where every real rootfs sits.
const REFERENCE_PIN_BYTES: u64 = 4 << 30;

/// Everything under `/dev` is dropped from the rootfs: `mknod`-style nodes are
/// unnecessary because the kernel mounts devtmpfs over `/dev` at boot. The `/dev`
/// directory entry itself is kept.
const DEV_PREFIX: &[u8] = b"/dev/";

/// The filesystem the rootfs partition carries.
///
/// A constant rather than a value read back off a formatted image, because two consumers
/// need it *before* the image node runs: the rootfs node writes it into the initramfs
/// configuration (`FSTYPE=`, which decides the fsck helper the initramfs carries), and
/// its own test asserts that. `feature_set` selects it — the ext4 baseline plus this
/// module's choices — so the two cannot be changed independently without the pin moving.
pub const ROOTFS_FS_KIND: &str = "ext4";

/// Format `dest` as an ext4 filesystem holding the rootfs `tarball`'s contents, then
/// verify it.
///
/// `size` either states the filesystem's size or asks for it to be found — see
/// [`RootfsSize`]. `label` is the ext4 volume label (≤ 16 bytes) the rootfs's
/// `/etc/fstab` mounts by. `uuid` is the deterministic superblock UUID the caller derived
/// from the lock, so a rebuild reproduces it. `first_boot` is the per-image credential
/// spliced into the rootfs's `/etc/shadow` before the filesystem is written.
///
/// Unlike the old `mke2fs` path, the superblock's format times are deterministic too:
/// they take the newest source mtime, which the rootfs export has already clamped to the
/// lock's `SOURCE_DATE_EPOCH`. The per-image first-boot password is still unique per
/// build, so the image as a whole is not byte-for-byte reproducible.
///
/// Returns what the format realized and what it was checked with — see
/// [`RootfsFilesystem`].
pub(crate) fn build_rootfs_ext4(
    dest: &Path,
    size: RootfsSize,
    tarball: &Path,
    label: &str,
    uuid: Uuid,
    first_boot: FirstBoot,
    step: &Step,
) -> Result<RootfsFilesystem, EngineError> {
    if let RootfsSize::Exact(bytes) = size {
        assert!(
            bytes.is_multiple_of(EXT4_BLOCK),
            "ext4 size must be block-aligned (geometry guarantees this)"
        );
    }
    step.log(match size {
        RootfsSize::Exact(bytes) => format!(
            "formatting {bytes}-byte ext4 rootfs at {} (ferrosys, pure-Rust: no mke2fs, no userns)",
            dest.display()
        ),
        RootfsSize::Fit(slack) => format!(
            "fitting an ext4 rootfs to its contents ({}) at {} (ferrosys, pure-Rust)",
            describe_slack(slack),
            dest.display()
        ),
    });

    // 1. Parse the rootfs tar into an entry list in-process. `ArchiveSource` reads
    //    ownership, mode, times, xattrs, ACLs, and device nodes straight from the
    //    (PAX) headers — no privileged extraction, so no user namespace.
    //
    //    `from_path` leaves each member's bytes in the archive and reads them as that
    //    file is placed, so peak memory is the largest single file in the rootfs rather
    //    than the sum of them all — for a media-accel rootfs, megabytes instead of
    //    gigabytes. It holds the archive open for the whole format, so `tarball` must
    //    not be rewritten in place until this returns — the rootfs node finishes writing
    //    it before the image node runs, so nothing holds it open for writing here.
    let mut entries = ArchiveSource::from_path(tarball)
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
    //    It returns the geometry it realized, which is the writer's own account of what
    //    it wrote — including a decision no superblock field states, like how far the
    //    reserved descriptor blocks let this filesystem grow.
    //    The plan is taken before the destination is touched, so a rootfs that cannot be
    //    written at all leaves the previous image file intact rather than truncated. A
    //    fitted plan additionally *decides* the size here, by placing the source into
    //    candidate geometries until it holds the smallest one that leaves the slack —
    //    the same placement a format performs, so the size it returns is one that
    //    formats. The model it built is kept, so the write walks the source once.
    let options = format_options(uuid, label, &entries);
    let plan = match size {
        RootfsSize::Exact(bytes) => FormatPlan::new(Entries(entries), bytes, options),
        RootfsSize::Fit(slack) => FormatPlan::fit(Entries(entries), options, slack),
    }
    .map_err(|e| EngineError::Ext4Format {
        detail: e.to_string(),
    })?;
    let size_bytes = plan.size_bytes();
    if let RootfsSize::Fit(_) = size {
        step.log(format!(
            "fitted the rootfs into {size_bytes} bytes — one ext4 block less does not hold it"
        ));
    }
    let mut out = std::fs::OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(true)
        .open(dest)
        .map_err(|s| EngineError::io(dest, s))?;
    let layout = plan
        .write_to(&mut out)
        .map_err(|e| EngineError::Ext4Format {
            detail: e.to_string(),
        })?;
    out.sync_all().map_err(|s| EngineError::io(dest, s))?;
    drop(out);
    step.log(format!(
        "formatted {} blocks in {} groups; grows in place to {} blocks ({} GiB)",
        layout.total_blocks,
        layout.group_count,
        layout.max_grow_blocks,
        layout.max_grow_blocks * u64::from(layout.block_size) / (1 << 30),
    ));

    Ok(RootfsFilesystem {
        size_bytes,
        provenance: filesystem_provenance(&options, &layout)?,
        verified_with: verify_clean(dest, step)?,
    })
}

/// How large the rootfs filesystem is: stated by the geometry, or found from the rootfs.
///
/// The two are the same format performed in a different order. [`Exact`](Self::Exact) is
/// the partition an authored `image_size` already laid out; [`Fit`](Self::Fit) searches
/// for the smallest filesystem that holds the rootfs with the stated room left free, and
/// the disk is then laid out around what it found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RootfsSize {
    /// Exactly this many bytes, which must be a multiple of the ext4 block size.
    Exact(u64),
    /// The smallest filesystem holding the rootfs with this much of it left free.
    Fit(Slack),
}

/// Render a slack for a log line, in the terms the recipe wrote it in.
fn describe_slack(slack: Slack) -> String {
    match slack {
        Slack::Bytes(bytes) => format!("{bytes} bytes free"),
        // Hundredths of one percent, so 2000 reads as 20% and 150 as 1.5%.
        Slack::Share(hundredths) => format!("{}.{:02}% free", hundredths / 100, hundredths % 100),
        // The formatter's slack is non-exhaustive: a variant it gains renders as its own
        // Debug rather than being mistaken for one of these.
        other => format!("{other:?}"),
    }
}

/// What formatting the rootfs produced, beyond the image file itself: the record of what
/// was written, and the record of what checked it.
///
/// Both are *reported by the node that did the work* rather than re-derived by the
/// caller, and for the same reason in each case. The geometry is a function of the
/// image's size as well as of this module's constants, so no caller can compute it
/// without repeating the format; the check list is host-dependent, so no caller can
/// compute it without repeating the probe.
pub(crate) struct RootfsFilesystem {
    /// The filesystem's size in bytes, as the format realized it.
    ///
    /// Reported rather than echoed back from the request, because under
    /// [`RootfsSize::Fit`] the caller did not state it: the search decided it, and the
    /// rootfs partition is then laid out around this number.
    pub size_bytes: u64,
    /// The on-disk contract, for the provenance manifest's `[filesystem]`.
    pub provenance: FilesystemProvenance,
    /// The checks the filesystem passed, in the order they ran, for the provenance
    /// manifest's `[verification]` — see [`verify_clean`].
    pub verified_with: Vec<String>,
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
    // A deterministic creation time: the newest source mtime, which the rootfs node's
    // tar export has already clamped to the lock's SOURCE_DATE_EPOCH — so the
    // superblock's format times are a function of the lock, not of the wall clock.
    // Clamped to the superblock's 32-bit range for safety; real rootfs mtimes are well
    // inside it.
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
/// It is expressed as [`FeatureSet::DEFAULT`] minus one feature. `DEFAULT` is the
/// formatter's *fixed* set — it names one exact set of words and sizes and keeps naming
/// them across releases, which is what makes it safe to base an on-disk contract on.
/// (Its counterpart `LATEST` tracks whatever `mke2fs` currently writes and may move in
/// any release; an image's layout must not, so it is not used here.)
///
/// The set is still recorded rather than trusted: [`filesystem_provenance`] writes what
/// this resolves to into the provenance manifest as part of the whole format policy, and
/// `the_format_policy_is_exactly_this_document` fails on any change to it, so drift —
/// whether from this expression or from the baseline underneath it — is caught in CI
/// rather than discovered in an image.
fn feature_set() -> FeatureSet {
    FeatureSet::DEFAULT
        .with_feature("orphan_file", false)
        .expect("orphan_file is a known feature name")
}

/// The rootfs filesystem's on-disk contract as provenance data: the formatter's own
/// policy pin for the options this build formats with, the geometry that policy lays out
/// at [`REFERENCE_PIN_BYTES`], and the geometry the format actually realized.
///
/// All three are *resolved* values, not declared ones. The pins come from the same
/// [`FormatOptions`] the formatter was handed, so the manifest reports what an image
/// actually carries — including a feature that arrived from the formatter's baseline
/// rather than from anything in this file. The geometry comes from the format itself.
///
/// The pins are taken whole rather than re-spelled field by field, and that is the point.
/// The formatter builds each by destructuring its own options exhaustively, so a field it
/// gains is carried here with no change to this function. A record assembled here would
/// keep compiling and silently stop covering it — which is exactly what a feature set
/// gaining a size (bigalloc's cluster size is both a feature bit *and* a size) would do
/// to a hand-written projection of three words and two sizes.
///
/// The reference plan is over an **empty** source rather than the rootfs, because the
/// geometry a size implies is a function of the policy options and the size alone: the
/// source only has to fit in the inodes that geometry provides, and an empty tree needs
/// none. So the pin describes the layout without describing the image.
///
/// # Errors
///
/// [`EngineError::Ext4Format`] if the reference size cannot be planned under these
/// options — which means the policy itself is unrealizable, since the image it is about
/// to describe formatted successfully.
fn filesystem_provenance(
    options: &FormatOptions,
    layout: &Layout,
) -> Result<FilesystemProvenance, EngineError> {
    let reference =
        FormatPlan::new(TreeBuilder::new(), REFERENCE_PIN_BYTES, *options).map_err(|e| {
            EngineError::Ext4Format {
                detail: format!(
                    "planning the {REFERENCE_PIN_BYTES}-byte reference geometry pin: {e}"
                ),
            }
        })?;
    Ok(FilesystemProvenance {
        kind: ROOTFS_FS_KIND.to_string(),
        policy_pin: options.policy_pin(),
        reference_geometry_pin: reference.geometry_pin(),
        geometry: FilesystemGeometry {
            block_size: layout.block_size,
            total_blocks: layout.total_blocks,
            total_inodes: layout.total_inodes,
            blocks_per_group: layout.blocks_per_group,
            inodes_per_group: layout.inodes_per_group,
            group_count: layout.group_count,
            first_data_block: layout.first_data_block,
            flex_bg_size: layout.flex_bg_size,
            gdt_blocks: layout.gdt_blocks,
            reserved_gdt_blocks: layout.reserved_gdt_blocks,
            inode_table_blocks: layout.inode_table_blocks,
            reserved_blocks: layout.reserved_blocks,
            max_grow_blocks: layout.max_grow_blocks,
        },
    })
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
    // The entry's contents are a range of the still-open archive, so reading them is a
    // seek and a read rather than a buffer already in hand.
    let bytes = content.read().map_err(|s| EngineError::Ext4Format {
        detail: format!("reading /etc/shadow from the rootfs tar: {s}"),
    })?;
    let current = std::str::from_utf8(&bytes).map_err(|_| EngineError::Ext4Format {
        detail: "/etc/shadow in the rootfs tar is not valid UTF-8".into(),
    })?;
    let spliced =
        crate::rootcache::splice_shadow(current, first_boot.user, first_boot.password_hash)
            .ok_or_else(|| EngineError::ArtifactMissing {
                what: format!("{} account in /etc/shadow", first_boot.user),
                location: "rootfs tar".into(),
            })?;
    // The one entry whose bytes this build computes rather than reads: owned, not a
    // range, so it replaces the archive's own /etc/shadow when the file is placed.
    shadow.kind = EntryKind::File(FileContent::Owned(spliced.into_bytes()));
    Ok(())
}

/// A parsed, post-processed entry list handed to the formatter as a [`Source`].
struct Entries(Vec<SourceEntry>);

impl Source for Entries {
    fn into_entries(self) -> Vec<SourceEntry> {
        self.0
    }
}

/// Minimum e2fsprogs version the `e2fsck` cross-check is trusted at, as
/// `(major, minor)`.
///
/// `metadata_csum_seed` is in the pinned feature set, and e2fsprogs only learned it in
/// 1.43 (2016). An older `e2fsck` reports the seed as an unknown feature and exits
/// non-zero on a perfectly good image — so without this floor, a host carrying an
/// ancient or non-GNU `e2fsck` on `PATH` fails a build that a host with **no** `e2fsck`
/// completes. An optional cross-check must never be worse than absent.
const E2FSCK_MIN: (u32, u32) = (1, 43);

/// Verify the finished image: always by scanning it with the crate's own [`Reader`],
/// and additionally with `e2fsck -fn` where a new enough one is available.
///
/// A scan finding means the formatter wrote an image its own reader disagrees with; an
/// `e2fsck` correction means the formatter and an independent checker disagree about the
/// layout. Either fails the build — a disagreement that must never ship inside an image.
///
/// The scan is the full one, not a checksum pass: every metadata checksum, plus the
/// placement of each group's own metadata, each in-use inode's block map, its directory
/// records and its extended attributes, the journal superblock, and the coherence of
/// what an inode's bytes claim against what the feature words promise. It reports
/// everything it finds rather than stopping at the first, which is what makes a
/// formatter defect diagnosable from one build's output.
///
/// **Any** anomaly fails, at any severity, including a cosmetic one. The general-purpose
/// threshold does not apply here: this image was written seconds ago by the formatter
/// whose reader is now judging it, so there is no such thing as a benign disagreement
/// between the two — a cosmetic finding on a fresh image is a formatter defect that
/// happens to be harmless *this* time.
///
/// The scan is compiled in and always runs; the `e2fsck` cross-check runs only where the
/// host carries an `e2fsprogs` of at least [`E2FSCK_MIN`], which makes verification
/// *depth* host-dependent. Its value is not that it is deeper — the scan is deeper — but
/// that it is *independent*: the scan is one implementation checking its own output, so
/// an assumption the writer and the reader share passes it unchallenged, and `e2fsck` is
/// the only reader in the build that does not share that code. So the checks that ran
/// are returned and recorded in the image's provenance (`[verification]`) rather than
/// left to a log line. `doctor` lists `e2fsck` as an optional tool for the same reason.
///
/// Below the floor the check is skipped with a log line rather than run: an e2fsprogs
/// that predates `metadata_csum_seed` would reject the image over a feature it does not
/// know, which is a disagreement about the *checker*, not about the layout.
fn verify_clean(dest: &Path, step: &Step) -> Result<Vec<String>, EngineError> {
    step.log("verifying ext4 image (ferrosys scan: checksums, placement, every inode)");
    let file = std::fs::File::open(dest).map_err(|s| EngineError::io(dest, s))?;
    let mut reader = Reader::open(file).map_err(|e| EngineError::Ext4Format {
        detail: format!("re-reading the formatted image: {e}"),
    })?;
    let report = reader.scan();
    if !report.is_clean() {
        return Err(EngineError::Ext4Format {
            detail: scan_failure(&report),
        });
    }
    let mut ran = vec!["ferrosys-scan".to_string()];

    // `-V`, not `--version`: e2fsck rejects the long form (exit 16) and prints its
    // banner on stderr under the short one.
    match e2fsck_usable(hosttool::version("e2fsck", "-V").as_deref()) {
        E2fsck::Run => {
            step.log("cross-checking with e2fsck -fn (any correction fails the build)");
            let mut cmd = Command::new("e2fsck");
            cmd.arg("-fn").arg(dest);
            build::run(cmd, "e2fsck", "e2fsck -fn (verify formatted rootfs)", step)?;
            ran.push("e2fsck".to_string());
        }
        E2fsck::Skip(why) => step.log(format!(
            "{why}; skipping the independent cross-check (the ferrosys scan passed) \
             — the image's provenance records which checks ran"
        )),
    }
    Ok(ran)
}

/// How many of a failing scan's findings the error message carries.
///
/// A scan reports everything it finds, which for a systematically wrong write is one
/// finding per inode. The first few identify the defect; the rest restate it. The count
/// is always reported, so a truncated list never reads as the whole of it.
const REPORTED_ANOMALIES: usize = 8;

/// Render a failing scan as the error text the build stops with.
///
/// Pure, so the rendering is unit-testable against a constructed report rather than
/// against a deliberately corrupted image.
fn scan_failure(report: &ScanReport) -> String {
    let found = report.anomalies().len();
    let at_least = if report.is_truncated() {
        "at least "
    } else {
        ""
    };
    let mut detail = format!(
        "the formatter wrote an image its own reader disagrees with: {at_least}{found} \
         anomal{} found scanning it back",
        if found == 1 { "y" } else { "ies" },
    );
    for anomaly in report.anomalies().iter().take(REPORTED_ANOMALIES) {
        detail.push_str(&format!(
            "\n  [{}/{}] {}{}",
            anomaly.severity.as_str(),
            anomaly.category.as_str(),
            anomaly.detail,
            at(&anomaly.location),
        ));
    }
    if found > REPORTED_ANOMALIES {
        detail.push_str(&format!("\n  ... and {} more", found - REPORTED_ANOMALIES));
    }
    detail
}

/// One anomaly's location as a trailing ` (inode N, group N, block N)`, empty when the
/// finding names none — a superblock fault is located by being one.
fn at(location: &Location) -> String {
    let mut parts = Vec::new();
    if let Some(inode) = location.inode {
        parts.push(format!("inode {inode}"));
    }
    if let Some(group) = location.group {
        parts.push(format!("group {group}"));
    }
    if let Some(block) = location.block {
        parts.push(format!("block {block}"));
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!(" ({})", parts.join(", "))
    }
}

/// Whether the host's `e2fsck` is one this cross-check will run.
#[derive(Debug, PartialEq, Eq)]
enum E2fsck {
    /// Run `e2fsck -fn` and fail the build on any correction.
    Run,
    /// Skip it, carrying the reason to log.
    Skip(String),
}

/// Decide from `e2fsck -V`'s banner (`None` when the binary could not be spawned).
///
/// Pure, so the floor is unit-testable against real banner text rather than against
/// whatever e2fsprogs the test host happens to carry. An unparseable banner runs the
/// check: a version this cannot read is far more likely to be a newer format than a
/// pre-2016 one, and refusing to check on a spelling change would quietly weaken every
/// build.
fn e2fsck_usable(banner: Option<&str>) -> E2fsck {
    let Some(banner) = banner else {
        return E2fsck::Skip("e2fsck not found".to_string());
    };
    match hosttool::major_minor(banner) {
        Some(v) if v < E2FSCK_MIN => E2fsck::Skip(format!(
            "e2fsck {}.{} predates metadata_csum_seed (needs {}.{}), so it would reject \
             this image over a feature it does not know",
            v.0, v.1, E2FSCK_MIN.0, E2FSCK_MIN.1
        )),
        _ => E2fsck::Run,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrosys::ext::Metadata;
    use std::path::PathBuf;

    /// True when `tar` is runnable — needed only to build the fixture archive, not to
    /// format it (the formatter is pure Rust). Panics under `BOOT2DEB_REQUIRE_HOST_TOOLS`.
    fn tar_ready() -> bool {
        hosttool::require(&["tar"])
    }

    #[test]
    fn the_e2fsck_cross_check_declines_an_e2fsprogs_that_predates_the_feature_set() {
        // An optional cross-check that is strictly *worse* than absent is the failure
        // mode here: pre-1.43 e2fsprogs does not know `metadata_csum_seed`, which is in
        // the pinned feature set, so it would reject a perfectly good image — failing a
        // build that a host with no e2fsck at all completes.
        let skip = |banner: &str| match e2fsck_usable(Some(banner)) {
            E2fsck::Skip(why) => why,
            E2fsck::Run => panic!("expected a skip for {banner:?}"),
        };
        let why = skip("e2fsck 1.42.13 (17-May-2015)");
        assert!(why.contains("1.42"), "{why}");
        assert!(why.contains("metadata_csum_seed"), "{why}");
        assert!(why.contains("1.43"), "the floor is named: {why}");
        assert!(skip("e2fsck 0.9 (old)").contains("predates"));
        assert!(skip("e2fsck 1.42.0").contains("predates"));

        // At and above the floor it runs — including the version this project is
        // developed against.
        for banner in [
            "e2fsck 1.43 (17-May-2016)",
            "e2fsck 1.43.4 (31-Jan-2017)",
            "e2fsck 1.47.0 (5-Feb-2023)",
            "e2fsck 2.0 (some-future-day)",
        ] {
            assert_eq!(e2fsck_usable(Some(banner)), E2fsck::Run, "{banner}");
        }

        // A banner this cannot parse still runs the check: a spelling change is far
        // likelier than a pre-2016 binary, and declining would quietly weaken every
        // build on that host.
        assert_eq!(e2fsck_usable(Some("e2fsck (unknown build)")), E2fsck::Run);

        // Absent entirely is the documented, supported case — the pure-Rust gate
        // stands alone and the provenance records that it did.
        assert_eq!(
            e2fsck_usable(None),
            E2fsck::Skip("e2fsck not found".to_string())
        );
    }

    /// A shadow entry as `ArchiveSource` would parse it: a `0640` regular file.
    fn shadow_entry(content: &[u8], mtime: Timestamp) -> SourceEntry {
        SourceEntry {
            path: b"/etc/shadow".to_vec(),
            kind: EntryKind::File(FileContent::Owned(content.to_vec())),
            meta: Metadata::new(0o640, mtime),
            xattrs: Vec::new(),
        }
    }

    /// The size every fixture image below is formatted at.
    ///
    /// 512 MiB, not the smallest filesystem that would exercise the code: the grow
    /// reservation is `min(map ceiling, descriptor ceiling, blocks / 64)`, so only at
    /// 256 MiB and above does the map's 1024 blocks become the binding term. A real
    /// rootfs is always in that regime, and it is the regime the assertions below
    /// describe; a smaller fixture would pin the proportional share instead and say
    /// nothing about what a shipped image gets.
    const FIXTURE_SIZE: u64 = 512 * 1024 * 1024;

    /// Format a fixture rootfs image in `dir`, exactly as the image node does, and
    /// return it beside what the format reported.
    ///
    /// The tree is small but rootfs-shaped: root ownership recorded in the tar, as the
    /// rootfs stage produces, and a locked account for the first-boot splice to rewrite.
    fn format_fixture(
        dir: &Path,
        size: RootfsSize,
        uuid: Uuid,
    ) -> Result<(PathBuf, RootfsFilesystem), EngineError> {
        let root = dir.join("tree");
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::write(root.join("etc/hostname"), b"turing-rk1\n").unwrap();
        std::fs::write(
            root.join("etc/shadow"),
            b"root:*:19000:0:99999:7:::\ndebian:!:19000:0:99999:7:::\n",
        )
        .unwrap();
        let tar = dir.join("rootfs.tar");
        let status = Command::new("tar")
            .args(["--owner=0", "--group=0", "--numeric-owner", "-C"])
            .arg(&root)
            .arg("-cf")
            .arg(&tar)
            .arg(".")
            .status()
            .unwrap();
        assert!(status.success(), "tar failed");

        let img = dir.join("rootfs.ext4");
        let sink = |_: crate::event::Event| {};
        let step = Step::start(&sink, "image");
        let first_boot = FirstBoot {
            user: "debian",
            password_hash: "$6$saltsalt$0123456789abcdef",
        };
        let fs = build_rootfs_ext4(&img, size, &tar, "rootfs", uuid, first_boot, &step)?;
        Ok((img, fs))
    }

    /// The formatted image must carry the supplied UUID, the resize-critical layout
    /// (`sparse_super` + `resize_inode` with reserved GDT blocks reaching the format
    /// ceiling), the pinned `remount-ro` error policy, and exactly the requested size —
    /// and pass the scan `build_rootfs_ext4` runs over it.
    ///
    /// Read back through the formatter's own reader rather than by indexing superblock
    /// offsets by hand. Each field is then named by the format rather than located by
    /// this test's arithmetic, so an assertion cannot quietly pass by reading the wrong
    /// two bytes — and the feature words are checked as a feature set rather than as bit
    /// A fitted size is decided by the format rather than stated to it, so the two
    /// things worth asserting are that the size comes back reported — the geometry is
    /// laid out around it, and a wrong number there is a filesystem that does not
    /// mount — and that the slack was actually left. The fixture rootfs is a few
    /// kilobytes, so a fitted image is orders of magnitude smaller than the stated
    /// [`FIXTURE_SIZE`]; that gap is the feature.
    #[test]
    fn a_fitted_format_reports_the_size_it_chose_and_leaves_the_slack() {
        if !tar_ready() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let uuid = Uuid::from_bytes([0x5a; 16]);
        let (img, fs) =
            format_fixture(tmp.path(), RootfsSize::Fit(Slack::Share(2000)), uuid).unwrap();

        assert!(
            fs.size_bytes.is_multiple_of(EXT4_BLOCK),
            "a fitted size must still be whole ext4 blocks, got {}",
            fs.size_bytes
        );
        assert!(
            fs.size_bytes < FIXTURE_SIZE,
            "fitting a few-kilobyte tree must beat the stated fixture size, got {}",
            fs.size_bytes
        );

        let reader = Reader::open(std::fs::File::open(&img).unwrap()).unwrap();
        let sb = reader.superblock();
        assert_eq!(
            sb.blocks_count,
            fs.size_bytes / EXT4_BLOCK,
            "the reported size is the one on disk — the geometry is laid out around it"
        );
        // A fifth of the filesystem free, as asked. The share is of the whole filesystem
        // rather than of the source, so this is a statement about the finished image.
        assert!(
            sb.free_blocks_count * 5 >= sb.blocks_count,
            "at least a fifth must be free: {} of {}",
            sb.free_blocks_count,
            sb.blocks_count
        );
    }

    /// masks this file would have to keep in step with the format's own.
    #[test]
    fn formats_resizable_filesystem_with_the_supplied_uuid() {
        if !tar_ready() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let uuid = Uuid::from_bytes([0x5a; 16]);
        let (img, fs) = format_fixture(tmp.path(), RootfsSize::Exact(FIXTURE_SIZE), uuid).unwrap();

        let reader = Reader::open(std::fs::File::open(&img).unwrap()).unwrap();
        let sb = reader.superblock();
        assert_eq!(&sb.uuid, uuid.as_bytes());
        assert_eq!(
            sb.blocks_count,
            FIXTURE_SIZE / EXT4_BLOCK,
            "exactly the size asked for"
        );
        // `s_errors = 2` is remount-ro. A literal because the format's own spelling of
        // the policy is not readable back as the typed value that set it.
        assert_eq!(sb.errors, 2, "errors must be remount-ro");
        // The online-resize headroom reaches the format ceiling — 1024 GDT blocks
        // (8 TiB) under this feature set, one of which this filesystem already uses, so
        // at least 1023 are reserved. This is what lets a small image grow onto a large
        // disk at first boot without relocating its descriptor table.
        assert!(
            sb.reserved_gdt_blocks >= 1023,
            "reserved GDT blocks must reach the resize ceiling, got {}",
            sb.reserved_gdt_blocks
        );

        // The features the image came out with, as the reader classifies them — not as
        // bit masks restated here.
        let feature = reader.feature();
        assert!(feature.has_resize_inode(), "resize_inode");
        assert!(feature.has_journal(), "has_journal");
        assert!(feature.is_sparse_super(), "sparse_super");
        // The checksum seed lives in the superblock, decoupled from the UUID, so a UUID
        // change never rewrites every metadata checksum.
        assert!(feature.has_csum_seed(), "metadata_csum_seed");

        // The formatter's own account of what it wrote must describe the image that is
        // actually on the disk. Recording a geometry the image does not have would be a
        // provenance manifest asserting something false, which no amount of checking the
        // image alone would catch.
        let geom = &fs.provenance.geometry;
        assert_eq!(geom.block_size, EXT4_BLOCK as u32);
        assert_eq!(geom.total_blocks, sb.blocks_count);
        assert_eq!(geom.total_inodes, sb.inodes_count);
        assert_eq!(geom.blocks_per_group, sb.blocks_per_group);
        assert_eq!(geom.inodes_per_group, sb.inodes_per_group);
        assert_eq!(geom.reserved_blocks, sb.r_blocks_count);
        assert_eq!(geom.first_data_block, sb.first_data_block);
        assert_eq!(geom.group_count, reader.group_count());
        assert_eq!(
            geom.reserved_gdt_blocks,
            u32::from(sb.reserved_gdt_blocks),
            "the recorded headroom is the headroom the image carries"
        );
        // The one number no superblock field states: what those reserved blocks buy.
        // The floor is a 2 TiB disk — a size a board this image ships to can plausibly
        // be given, and one a filesystem built at 512 MiB must still be able to fill.
        assert!(
            geom.max_grow_blocks >= 2 * 1024 * 1024 * 1024 * 1024 / EXT4_BLOCK,
            "must grow onto a large disk in place, ceiling is {} blocks",
            geom.max_grow_blocks
        );
        // And the record says which checks it passed, the in-process one first.
        assert_eq!(fs.verified_with.first().unwrap(), "ferrosys-scan");
    }

    /// The scan gate must actually stop a bad image, which a test over good input cannot
    /// show: a `verify_clean` that inspected nothing would pass every assertion above.
    ///
    /// The damage is a byte of the volume label, which changes what the superblock
    /// checksum covers without changing anything the reader needs to parse it — so the
    /// image still opens, and the disagreement is exactly the kind a formatter defect
    /// would produce: bytes that describe themselves inconsistently rather than bytes
    /// that cannot be read at all. That is the case a gate has to catch, because it is
    /// the one that would otherwise ship.
    #[test]
    fn the_scan_gate_fails_an_image_whose_superblock_disagrees_with_itself() {
        if !tar_ready() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let (img, _) = format_fixture(
            tmp.path(),
            RootfsSize::Exact(FIXTURE_SIZE),
            Uuid::from_bytes([0x5a; 16]),
        )
        .expect("the fixture formats clean");

        // `s_volume_name` runs 16 bytes from offset 0x78 of the superblock, which itself
        // begins 1024 bytes into the image. Nothing reads it to find anything else.
        let mut bytes = std::fs::read(&img).unwrap();
        bytes[1024 + 0x78] ^= 0xff;
        std::fs::write(&img, &bytes).unwrap();

        let sink = |_: crate::event::Event| {};
        let step = Step::start(&sink, "image");
        let err = verify_clean(&img, &step).expect_err("a corrupted image must not pass");
        let EngineError::Ext4Format { detail } = err else {
            panic!("expected an ext4 format error, got {err:?}");
        };
        assert!(
            detail.contains("its own reader disagrees with"),
            "the message must say what went wrong: {detail}"
        );
        assert!(
            detail.contains("superblock"),
            "and name the structure at fault: {detail}"
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
                kind: EntryKind::File(FileContent::Owned(b"turing-rk1\n".to_vec())),
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
        let bytes = content.read().unwrap();
        let out = std::str::from_utf8(&bytes).unwrap();
        // The debian line carries the hash and is expired (field 3 = 0); root is untouched.
        assert!(
            out.contains("debian:$6$saltsalt$hashhashhash:0:0:99999:7:::"),
            "spliced line missing, got: {out}"
        );
        assert!(
            out.contains("root:*:19000:0:99999:7:::"),
            "root line preserved"
        );
        // Only the content changes: the entry's mode and mtime are its own metadata.
        assert_eq!(entries[0].meta.mode, 0o640, "shadow mode preserved");
        assert_eq!(entries[0].meta.mtime, mtime, "shadow mtime preserved");
    }

    #[test]
    fn splice_first_boot_password_errors_when_the_account_is_absent() {
        let mut entries = vec![shadow_entry(
            b"root:*:19000:0:99999:7:::\n",
            Timestamp::from_secs(1),
        )];
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
            kind: EntryKind::File(FileContent::Owned(b"turing-rk1\n".to_vec())),
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
        assert!(
            f.has_csum_seed(),
            "metadata_csum_seed (seed decoupled from the UUID)"
        );
        assert!(f.has_extents(), "extents");
        assert!(f.is_64bit(), "64bit");
        assert!(f.has_dir_index(), "dir_index");
        // orphan_file is intentionally off, matching the validated mke2fs feature set.
        assert!(!f.has_orphan_file(), "orphan_file pinned off");
        assert_eq!(f.block_size, EXT4_BLOCK as u32);
        assert_eq!(f.inode_size, 256);
    }

    /// The format options as the image stage really assembles them. The identity inputs
    /// are stand-ins because neither pin under test carries any of them: the policy pin
    /// is documented as holding nothing image-specific, and the geometry a size implies
    /// is a function of the policy options and the size alone.
    fn pinned_options() -> FormatOptions {
        format_options(Uuid::nil(), "boot2deb", &[])
    }

    /// The exact format policy, pinned as the formatter's own document rather than as
    /// predicates.
    ///
    /// The test above asserts what the feature set must *contain*, which by construction
    /// cannot fail on a feature the formatter's baseline gains: [`feature_set`] is
    /// `FeatureSet::DEFAULT` minus one feature, and `DEFAULT` is documented as free to
    /// grow. So a formatter upgrade can change every image's layout while every
    /// predicate above still passes. This is the assertion that fails instead.
    ///
    /// It is byte-exact against the whole document, which is what makes it complete. The
    /// document names every feature word twice over — as bits and as names, which
    /// therefore cannot drift apart — plus the two sizes that are not features, plus the
    /// seven options outside the feature set entirely. `errors remount_read_only` is the
    /// one that is *only* here: the error behaviour reaches neither a feature word nor
    /// the geometry, so no other record in the tree would notice it changing. And because
    /// the formatter builds the document by destructuring its own options exhaustively, a
    /// *field* it gains lands in this string too, where a projection written here would
    /// have kept compiling and silently stopped covering it.
    ///
    /// A failure here is not necessarily a defect — it means the contract images are
    /// built to moved, and the decision (adopt the change, or pin it back in
    /// [`format_options`]) has to be made deliberately. Update this document only
    /// together with that decision.
    #[test]
    fn the_format_policy_is_exactly_this_document() {
        assert_eq!(
            pinned_options().policy_pin(),
            "ferrosys-policy-pin 1\n\
             compat 0x0000003c has_journal ext_attr resize_inode dir_index\n\
             incompat 0x000022c2 filetype extent 64bit flex_bg metadata_csum_seed\n\
             ro_compat 0x0000046b sparse_super large_file huge_file dir_nlink extra_isize \
             metadata_csum\n\
             block_size 4096\n\
             inode_size 256\n\
             grow max\n\
             inodes bytes_per_inode 16384\n\
             reserved 100\n\
             errors remount_read_only\n\
             journal auto\n\
             hash_version half_md4\n\
             hash_signedness unsigned\n\
             timestamp_clamp none\n"
        );
    }

    /// What that policy lays out at [`REFERENCE_PIN_BYTES`], byte-exact.
    ///
    /// This is the half the policy pin cannot cover. A policy pin records options *by
    /// name*, so it moves when an option is renamed, re-defaulted, or set differently
    /// here — and not when the formula behind an unchanged name changes underneath it.
    /// `grow max` reads identically before and after a change to what `Max` reserves;
    /// `reserved_gdt_blocks 1024` does not. Planning at one fixed size is what turns that
    /// class of change into a diff.
    ///
    /// The `groups` line is a crc32c over every field of every group in order rather than
    /// one line per group, so a placement that moves changes this string while the string
    /// stays a fixed size.
    ///
    /// Same rule as the policy pin on failure: the layout moved, and updating this
    /// document is the second half of deciding that it should have.
    #[test]
    fn the_reference_geometry_is_exactly_this_document() {
        let plan = FormatPlan::new(TreeBuilder::new(), REFERENCE_PIN_BYTES, pinned_options())
            .expect("the reference size plans under the pinned policy");
        assert_eq!(
            plan.geometry_pin(),
            "ferrosys-geometry-pin 1\n\
             block_size 4096\n\
             total_blocks 1048576\n\
             blocks_per_group 32768\n\
             first_data_block 0\n\
             group_count 32\n\
             inodes_per_group 8192\n\
             inode_table_blocks 512\n\
             total_inodes 262144\n\
             gdt_blocks 1\n\
             reserved_gdt_blocks 1024\n\
             flex_bg_size 16\n\
             max_grow_blocks 2149580800\n\
             reserved_blocks 10485\n\
             groups 32 crc32c 0xb1e58475\n\
             journal_blocks 16384\n"
        );
    }

    /// The reference pin describes the *policy*, not the image: it is planned over an
    /// empty source, and the geometry a size implies does not answer to what goes in it.
    /// So two builds whose rootfs contents differ record the same reference pin, which is
    /// the property that makes comparing two builds' records mean anything.
    #[test]
    fn the_reference_geometry_pin_does_not_answer_to_the_rootfs() {
        let empty = FormatPlan::new(TreeBuilder::new(), REFERENCE_PIN_BYTES, pinned_options())
            .unwrap()
            .geometry_pin();
        let meta = |mode| Metadata::new(mode, Timestamp::from_secs(0));
        let populated = FormatPlan::new(
            TreeBuilder::new()
                .directory(b"/etc".to_vec(), meta(0o755))
                .file(b"/etc/hostname".to_vec(), b"rk1\n".as_slice(), meta(0o644)),
            REFERENCE_PIN_BYTES,
            pinned_options(),
        )
        .unwrap()
        .geometry_pin();
        assert_eq!(empty, populated);
    }

    #[test]
    fn the_hash_seed_is_deterministic_and_distinct_from_the_uuid() {
        let uuid = Uuid::from_bytes([0x5a; 16]);
        // Deterministic: the same UUID yields the same seed.
        assert_eq!(derive_hash_seed(uuid), derive_hash_seed(uuid));
        // Distinct from the UUID bytes, and distinct for a different UUID.
        assert_ne!(derive_hash_seed(uuid), *uuid.as_bytes());
        assert_ne!(
            derive_hash_seed(uuid),
            derive_hash_seed(Uuid::from_bytes([0x5b; 16]))
        );
    }
}
