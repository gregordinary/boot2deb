//! Rootfs ext4 partition assembly: stage the rootfs tarball inside an
//! unprivileged user namespace and format it into a fixed-size ext4 image with
//! host `mke2fs -d` — no mount, no loop device, no root.
//!
//! Ownership is the reason for the namespace: the tar carries multi-uid
//! ownership (root, messagebus, ...), which an unprivileged extraction cannot
//! set. Inside `unshare --map-root-user --map-auto` the build user is root with
//! the host's subuid/subgid ranges mapped (the same host requirement the
//! `mmdebstrap --mode=unshare` rootfs stage already relies on), so `tar -xp`
//! preserves every owner and `mke2fs -d` records them into the filesystem.
//!
//! `mke2fs` writes the journal at format time and lays out a standard
//! `sparse_super` + `resize_inode` filesystem whose reserved GDT blocks are
//! explicitly sized for growth to [`ONLINE_RESIZE_CEILING`] (GEO-3), which the
//! kernel's online resize (`EXT4_IOC_RESIZE_FS`) grows without a meta_bg
//! conversion — first boot expands the rootfs while it is mounted as `/`.
//!
//! The staged tree is also where the unique per-image first-boot password is
//! spliced (SEC-6): the cacheable rootfs tarball leaves the default account
//! locked, and `/etc/shadow` is rewritten here — the one per-build-unique step —
//! before `mke2fs` records the tree. Editing the extracted file in place keeps
//! the `root:shadow` ownership the namespace set, so no fragile in-archive
//! member surgery is needed.
//!
//! The finished image must verify **clean**: `e2fsck -fn` runs read-only and
//! any nonzero exit fails the build. A just-formatted filesystem has nothing
//! legitimate to correct, so a "fix" here means the formatter and the checker
//! disagree about the on-disk layout — exactly the disagreement that must never
//! ship inside an image.
//!
//! The output is the standalone ext4 image the [image orchestrator](super)
//! splices into the whole-disk image at the rootfs partition offset.

use crate::build;
use crate::error::EngineError;
use crate::event::Step;
use crate::image::geometry::EXT4_BLOCK;
use std::path::Path;
use std::process::Command;
use uuid::Uuid;

/// The exact feature set the image filesystem is formatted with. Passed as an
/// explicit `-O` list so the result does not vary with the host's
/// `mke2fs.conf`: a build on any distribution produces the same layout.
///
/// `resize_inode` + `sparse_super` are the load-bearing pair — reserved GDT
/// blocks give the kernel's online resize its growth headroom (sized by
/// [`ONLINE_RESIZE_CEILING`], GEO-3) without a meta_bg conversion.
/// `metadata_csum_seed` stores the checksum seed in the superblock instead of
/// deriving it from the UUID, which is what lets first-boot's `tune2fs -U`
/// re-UUID the *mounted* root without rewriting every metadata checksum. The
/// rest is the standard modern ext4 set the target kernel and e2fsprogs both
/// support.
const EXT4_FEATURES: &str = "64bit,dir_index,dir_nlink,ext_attr,extent,extra_isize,filetype,\
                             flex_bg,has_journal,huge_file,large_file,metadata_csum,\
                             metadata_csum_seed,resize_inode,sparse_super";

/// Online-resize ceiling requested at format time — 8 TiB, passed to `mke2fs`
/// as `-E resize=` (GEO-3).
///
/// First boot grows the mounted root with `resize2fs`, an *online* resize whose
/// reach is exactly the reserved GDT blocks laid down here. Without an explicit
/// request `mke2fs` reserves for ~1024x the formatted size, so a 2 GiB image
/// tops out near 2 TiB and silently strands the rest of a larger NVMe. 8 TiB is
/// the mechanism's own ceiling under this feature set: the resize inode
/// addresses at most `block_size / 4` = 1024 reserved GDT blocks through its
/// single indirect block, each holding 64 of the 64-byte (`64bit`-feature)
/// descriptors, each descriptor one 128 MiB block group — 1024 x 64 x 128 MiB
/// = 8 TiB. It also covers the largest M.2 NVMe the supported boards take. The
/// reservation itself costs ~4 MiB in the image.
const ONLINE_RESIZE_CEILING: u64 = 8 << 40;

/// The `-E resize=` value in ext4 blocks: the ceiling, or the filesystem's own
/// block count where the formatted size is already at/above it — a filesystem
/// that large has no reserved-GDT headroom left to ask for, and `mke2fs`
/// rejects a resize target below the filesystem size.
fn online_resize_blocks(size: u64) -> u64 {
    size.max(ONLINE_RESIZE_CEILING) / EXT4_BLOCK
}

/// Format `dest` as an ext4 filesystem of exactly `size` bytes holding the
/// rootfs `tarball`'s contents, then verify it.
///
/// `size` must be a multiple of the ext4 block size (the caller's geometry
/// guarantees it). `label` is the ext4 volume label (≤ 16 bytes) the rootfs's
/// `/etc/fstab` mounts by. `uuid` is the deterministic superblock UUID the caller
/// derived from the lock, so a rebuild reproduces it instead of `mke2fs` drawing
/// a random one. `first_boot` is the per-image credential spliced into the staged
/// `/etc/shadow` after extraction and before formatting (SEC-6).
///
/// The output is *not* byte-for-byte reproducible on its own: `mke2fs` stamps
/// the superblock's format/check times from the wall clock, and the per-image
/// first-boot password is unique per build. The UUID is the one identifier the
/// reproducibility contract reaches.
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
        "formatting {}-byte ext4 rootfs at {} (mke2fs -d, userns staging)",
        size,
        dest.display()
    ));
    let staging = dest
        .parent()
        .expect("ext4 image path has a parent directory")
        .join("rootfs-staging");
    // A leftover staging tree from an aborted run is subuid-owned, so both the
    // pre-clean and the post-clean run inside the namespace.
    remove_staging(&staging, step)?;
    std::fs::create_dir_all(&staging).map_err(|s| EngineError::io(&staging, s))?;
    stage_rootfs(tarball, &staging, step)?;
    splice_first_boot_password(&staging, first_boot, step)?;
    mkfs(dest, size, &staging, label, uuid, step)?;
    remove_staging(&staging, step)?;
    verify_clean(dest, step)
}

/// The per-image first-boot credential spliced into the staged rootfs before it
/// is formatted (SEC-6). The default account is created locked in the cacheable
/// rootfs tarball; this rewrites its `/etc/shadow` line with a fresh hash and
/// forces a change at first login.
pub(crate) struct FirstBoot<'a> {
    /// The default account whose locked shadow line receives the hash.
    pub user: &'a str,
    /// A `sha512crypt` (`$6$`) hash from [`crate::secret::crypt_password`].
    pub password_hash: &'a str,
}

/// Rewrite `first_boot.user`'s locked `/etc/shadow` line in the staged tree with
/// the per-image hash (SEC-6), before `mke2fs -d` records the tree.
///
/// The unique per-image password is non-reproducible, so the cacheable rootfs
/// tarball leaves the account locked (`{user}:!:…`) and the splice happens here —
/// the one per-build-unique step. The extracted file is owner-writable (mode
/// `0640`), so an in-place content rewrite keeps its inode: the `root:shadow`
/// ownership the userns extraction set survives for `mke2fs` to read back, with
/// no fragile `tar --delete`/`--append` on the (PAX) archive. The extracted
/// file's mtime is mmdebstrap's epoch clamp; it is restored across the rewrite so
/// the splice reintroduces no build-time mtime (DET).
fn splice_first_boot_password(
    staging: &Path,
    first_boot: FirstBoot,
    step: &Step,
) -> Result<(), EngineError> {
    let shadow = staging.join("etc/shadow");
    let current = std::fs::read_to_string(&shadow).map_err(|s| EngineError::io(&shadow, s))?;
    let spliced =
        crate::rootcache::splice_shadow(&current, first_boot.user, first_boot.password_hash)
            .ok_or_else(|| EngineError::ArtifactMissing {
                what: format!("{} account in /etc/shadow", first_boot.user),
                location: shadow.display().to_string(),
            })?;
    let mtime = std::fs::metadata(&shadow)
        .and_then(|m| m.modified())
        .map_err(|s| EngineError::io(&shadow, s))?;
    std::fs::write(&shadow, spliced).map_err(|s| EngineError::io(&shadow, s))?;
    std::fs::File::options()
        .write(true)
        .open(&shadow)
        .and_then(|f| f.set_modified(mtime))
        .map_err(|s| EngineError::io(&shadow, s))?;
    step.log("spliced the unique per-image first-boot password into /etc/shadow");
    Ok(())
}

/// A command running inside a fresh user namespace: the build user mapped to
/// root plus the host subuid/subgid ranges (`--map-auto`), so multi-uid file
/// ownership can be created and read back.
fn in_userns(argv0: &str) -> Command {
    let mut cmd = Command::new("unshare");
    cmd.args(["--map-root-user", "--map-auto", "--", argv0]);
    cmd
}

/// Extract the rootfs tar into `staging` with ownership, permissions, and
/// xattrs (e.g. the POSIX ACLs on `/var/log/journal`) preserved.
///
/// `./dev/*` is excluded: `mknod` is not permitted in an unprivileged user
/// namespace, and the image does not need the nodes — the kernel mounts
/// devtmpfs over `/dev` at boot. The `/dev` directory entry itself extracts.
fn stage_rootfs(tarball: &Path, staging: &Path, step: &Step) -> Result<(), EngineError> {
    let mut cmd = in_userns("tar");
    cmd.args([
        "--extract",
        "--preserve-permissions",
        "--numeric-owner",
        "--xattrs",
        "--xattrs-include=*",
        "--exclude=./dev/*",
        "--file",
    ]);
    cmd.arg(tarball).arg("--directory").arg(staging);
    build::run(cmd, "tar", "tar --extract (stage rootfs into userns tree)", step)
}

/// Format `dest` from the staged tree with `mke2fs -d`, pinning every
/// `mke2fs.conf`-dependent knob so the layout is host-independent.
fn mkfs(
    dest: &Path,
    size: u64,
    staging: &Path,
    label: &str,
    uuid: Uuid,
    step: &Step,
) -> Result<(), EngineError> {
    // Pre-size the backing file (sparse); mke2fs formats an existing file in
    // place. The explicit block count below still pins the filesystem size.
    let f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dest)
        .map_err(|s| EngineError::io(dest, s))?;
    f.set_len(size).map_err(|s| EngineError::io(dest, s))?;
    drop(f);

    // Inside the namespace so the subuid-owned staging tree reads back as its
    // container ids — run on the host, mke2fs would record raw subuids.
    let mut cmd = in_userns("mke2fs");
    cmd.args(["-F", "-q", "-b"]);
    cmd.arg(EXT4_BLOCK.to_string());
    // Inode size/ratio pinned (conf-dependent otherwise): 256-byte inodes, one
    // inode per 16 KiB — the ext4 defaults, stated explicitly.
    cmd.args(["-I", "256", "-i", "16384"]);
    // 1% reserved for root: keeps root-owned services writable when a non-root
    // consumer fills the disk, without the default 5%'s cost on a grown NVMe.
    cmd.args(["-m", "1", "-e", "remount-ro"]);
    cmd.arg("-L").arg(label);
    cmd.arg("-U").arg(uuid.to_string());
    cmd.arg("-O").arg(EXT4_FEATURES);
    // Fully initialize inode tables and the journal at format time — the image
    // content must not depend on a first mount finishing the format. `resize=`
    // reserves GDT headroom for online growth to the ceiling (GEO-3).
    cmd.arg("-E").arg(format!(
        "lazy_itable_init=0,lazy_journal_init=0,resize={}",
        online_resize_blocks(size)
    ));
    cmd.arg("-d").arg(staging);
    cmd.arg(dest);
    cmd.arg((size / EXT4_BLOCK).to_string());
    build::run(cmd, "mke2fs", "mke2fs -d (format rootfs ext4)", step)
}

/// Verify the finished image with a read-only `e2fsck -fn`; **any** nonzero
/// exit fails the build. A freshly formatted filesystem has nothing to correct,
/// so a would-be fix means formatter and checker disagree about the layout —
/// an image that must not ship.
fn verify_clean(dest: &Path, step: &Step) -> Result<(), EngineError> {
    step.log("verifying ext4 image (e2fsck -fn, any correction fails the build)");
    let mut cmd = Command::new("e2fsck");
    cmd.arg("-fn").arg(dest);
    build::run(cmd, "e2fsck", "e2fsck -fn (verify formatted rootfs)", step)
}

/// Remove a staging tree whose contents are subuid-owned (the host user cannot
/// unlink inside root-owned directories, so the removal runs in the namespace).
fn remove_staging(staging: &Path, step: &Step) -> Result<(), EngineError> {
    if !staging.exists() {
        return Ok(());
    }
    let mut cmd = in_userns("rm");
    cmd.arg("-rf").arg(staging);
    build::run(cmd, "rm", "rm -rf (clear rootfs staging tree)", step)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Step;

    /// True when a host tool is runnable (missing binaries fail to spawn).
    fn have(tool: &str) -> bool {
        Command::new(tool).arg("--version").output().is_ok()
    }

    /// The end-to-end format needs the userns + e2fsprogs host tools; skip
    /// (or panic under `BOOT2DEB_REQUIRE_HOST_TOOLS`) where absent.
    fn host_ready() -> bool {
        let missing: Vec<&str> = ["unshare", "tar", "mke2fs", "e2fsck"]
            .into_iter()
            .filter(|t| !have(t))
            .collect();
        if missing.is_empty() {
            // `--map-auto` additionally needs subuid ranges for the build user;
            // probe the exact invocation the build uses.
            let userns_ok = Command::new("unshare")
                .args(["--map-root-user", "--map-auto", "true"])
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false);
            if userns_ok {
                return true;
            }
            assert!(
                std::env::var_os("BOOT2DEB_REQUIRE_HOST_TOOLS").is_none(),
                "BOOT2DEB_REQUIRE_HOST_TOOLS is set but `unshare --map-auto` is not usable"
            );
            eprintln!("skipping: unshare --map-auto not usable on this host");
            return false;
        }
        assert!(
            std::env::var_os("BOOT2DEB_REQUIRE_HOST_TOOLS").is_none(),
            "BOOT2DEB_REQUIRE_HOST_TOOLS is set but required host tools are missing: {missing:?}"
        );
        eprintln!("skipping: required host tools unavailable: {missing:?}");
        false
    }

    /// Little-endian field readers over the superblock (1024 bytes into the image).
    fn sb_u16(img: &[u8], off: usize) -> u16 {
        u16::from_le_bytes(img[1024 + off..1024 + off + 2].try_into().unwrap())
    }
    fn sb_u32(img: &[u8], off: usize) -> u32 {
        u32::from_le_bytes(img[1024 + off..1024 + off + 4].try_into().unwrap())
    }

    /// The formatted image must carry the supplied UUID (the reproducibility
    /// contract) and the resize-critical layout: `sparse_super` + `resize_inode`
    /// with reserved GDT blocks, at exactly the requested size, verifying clean.
    #[test]
    fn formats_resizable_filesystem_with_the_supplied_uuid() {
        if !host_ready() {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        // A small rootfs tree with root ownership recorded in the tar, as the
        // rootfs stage produces.
        let root = tmp.path().join("tree");
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::write(root.join("etc/hostname"), b"turing-rk1\n").unwrap();
        // The account is locked in the tarball; the image stage splices the
        // per-image first-boot hash into it.
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
        // s_feature_incompat at 0x60: CSUM_SEED (0x2000) — first-boot re-UUIDs
        // the mounted root, which needs the seed decoupled from the UUID.
        assert_ne!(sb_u32(&bytes, 0x60) & 0x2000, 0, "metadata_csum_seed must be set");
        // s_reserved_gdt_blocks at 0xCE: the online-resize growth headroom must
        // reach the 8 TiB ceiling (GEO-3), not mke2fs's ~1024x-formatted default
        // — growth to 8 TiB needs 1024 GDT blocks total (65536 groups x 64-byte
        // descriptors / 4 KiB blocks), one of which this small filesystem
        // already uses, so at least 1023 are reserved.
        assert!(
            sb_u16(&bytes, 0xCE) >= 1023,
            "reserved GDT blocks must cover the 8 TiB online-resize ceiling, got {}",
            sb_u16(&bytes, 0xCE)
        );

        // The staging tree is cleaned up.
        assert!(!tmp.path().join("rootfs-staging").exists());
    }

    #[test]
    fn online_resize_request_is_the_ceiling_until_the_image_reaches_it() {
        // A normal-sized rootfs asks for the full 8 TiB ceiling...
        assert_eq!(
            online_resize_blocks(2 << 30),
            ONLINE_RESIZE_CEILING / EXT4_BLOCK
        );
        // ...an image at the ceiling asks for exactly itself (no headroom, but
        // also no mke2fs rejection for a target below the filesystem size)...
        assert_eq!(
            online_resize_blocks(ONLINE_RESIZE_CEILING),
            ONLINE_RESIZE_CEILING / EXT4_BLOCK
        );
        // ...and a larger one asks for its own size.
        assert_eq!(online_resize_blocks(16 << 40), (16u64 << 40) / EXT4_BLOCK);
    }

    #[test]
    fn splice_first_boot_password_rewrites_the_locked_line_in_place() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let staging = tmp.path().join("stage");
        std::fs::create_dir_all(staging.join("etc")).unwrap();
        let shadow = staging.join("etc/shadow");
        std::fs::write(
            &shadow,
            "root:*:19000:0:99999:7:::\ndebian:!:19000:0:99999:7:::\n",
        )
        .unwrap();
        // The on-disk shape the userns extraction leaves: mode 0640 and an
        // epoch-clamped mtime, both of which the in-place rewrite must keep.
        std::fs::set_permissions(&shadow, std::fs::Permissions::from_mode(0o640)).unwrap();
        let epoch = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000);
        std::fs::File::options()
            .write(true)
            .open(&shadow)
            .unwrap()
            .set_modified(epoch)
            .unwrap();

        let sink = |_: crate::event::Event| {};
        let step = Step::start(&sink, "image");
        let first_boot = FirstBoot {
            user: "debian",
            password_hash: "$6$saltsalt$hashhashhash",
        };
        splice_first_boot_password(&staging, first_boot, &step).unwrap();

        let out = std::fs::read_to_string(&shadow).unwrap();
        // The debian line carries the hash and is expired (field 3 = 0); root is untouched.
        assert!(
            out.contains("debian:$6$saltsalt$hashhashhash:0:0:99999:7:::"),
            "spliced line missing, got: {out}"
        );
        assert!(out.contains("root:*:19000:0:99999:7:::"), "root line preserved");
        // In-place rewrite preserves both the mode and the clamped mtime.
        let meta = std::fs::metadata(&shadow).unwrap();
        assert_eq!(meta.permissions().mode() & 0o777, 0o640, "0640 preserved");
        assert_eq!(meta.modified().unwrap(), epoch, "epoch-clamped mtime preserved");
    }

    #[test]
    fn splice_first_boot_password_errors_when_the_account_is_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let staging = tmp.path().join("stage");
        std::fs::create_dir_all(staging.join("etc")).unwrap();
        std::fs::write(staging.join("etc/shadow"), "root:*:19000:0:99999:7:::\n").unwrap();
        let sink = |_: crate::event::Event| {};
        let step = Step::start(&sink, "image");
        let first_boot = FirstBoot {
            user: "debian",
            password_hash: "$6$x$y",
        };
        let err = splice_first_boot_password(&staging, first_boot, &step).unwrap_err();
        assert!(
            matches!(err, EngineError::ArtifactMissing { what, .. } if what.contains("debian account")),
            "expected a missing-account error"
        );
    }
}
