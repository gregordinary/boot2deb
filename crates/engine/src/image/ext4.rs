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
//! `sparse_super` + `resize_inode` filesystem (reserved GDT blocks), which the
//! kernel's online resize (`EXT4_IOC_RESIZE_FS`) grows without a meta_bg
//! conversion — first boot expands the rootfs while it is mounted as `/`.
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
/// blocks give the kernel's online resize its growth headroom (~1024x the
/// formatted size) without a meta_bg conversion. `metadata_csum_seed` stores
/// the checksum seed in the superblock instead of deriving it from the UUID,
/// which is what lets first-boot's `tune2fs -U` re-UUID the *mounted* root
/// without rewriting every metadata checksum. The rest is the standard modern
/// ext4 set the target kernel and e2fsprogs both support.
const EXT4_FEATURES: &str = "64bit,dir_index,dir_nlink,ext_attr,extent,extra_isize,filetype,\
                             flex_bg,has_journal,huge_file,large_file,metadata_csum,\
                             metadata_csum_seed,resize_inode,sparse_super";

/// Format `dest` as an ext4 filesystem of exactly `size` bytes holding the
/// rootfs `tarball`'s contents, then verify it.
///
/// `size` must be a multiple of the ext4 block size (the caller's geometry
/// guarantees it). `label` is the ext4 volume label (≤ 16 bytes) the rootfs's
/// `/etc/fstab` mounts by. `uuid` is the deterministic superblock UUID the caller
/// derived from the lock, so a rebuild reproduces it instead of `mke2fs` drawing
/// a random one.
///
/// The output is *not* byte-for-byte reproducible on its own: `mke2fs` stamps
/// the superblock's format/check times from the wall clock. The UUID is the one
/// identifier the reproducibility contract reaches; two builds differ only in
/// those timestamp fields.
pub(crate) fn build_rootfs_ext4(
    dest: &Path,
    size: u64,
    tarball: &Path,
    label: &str,
    uuid: Uuid,
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
    mkfs(dest, size, &staging, label, uuid, step)?;
    remove_staging(&staging, step)?;
    verify_clean(dest, step)
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
    // content must not depend on a first mount finishing the format.
    cmd.args(["-E", "lazy_itable_init=0,lazy_journal_init=0"]);
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
        build_rootfs_ext4(&img, size, &tar, "rootfs", uuid, &step).unwrap();

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
        // s_reserved_gdt_blocks at 0xCE: the online-resize growth headroom.
        assert!(sb_u16(&bytes, 0xCE) > 0, "reserved GDT blocks must be present");

        // The staging tree is cleaned up.
        assert!(!tmp.path().join("rootfs-staging").exists());
    }
}
