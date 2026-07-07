//! Rootfs ext4 partition assembly: format a fixed-size ext4 image from a
//! rootfs tarball with pure-Rust `arcbox-ext4` (`tar` → ext4, unprivileged — no
//! mount, no loop, no root), then add a journal with a host-side `tune2fs`.
//!
//! arcbox-ext4 produces a journal-less filesystem (ext2 crash semantics). This is
//! an appliance that loses power, so a journal is required: `tune2fs -O
//! has_journal` runs on the finished *image file* on the host — a small, contained
//! e2fsprogs shell-out with no mount. Teaching arcbox-ext4 mkfs-time
//! journaling would drop this step.
//!
//! arcbox-ext4 also writes the filesystem with the `sparse_super2` feature, which
//! the kernel's online resize cannot grow. First-boot expands the rootfs while it
//! is mounted as `/`, so a second host step (`debugfs` clears the feature, then
//! `e2fsck` verifies) makes the image online-resizable.
//!
//! The output is a standalone ext4 image the [image orchestrator](super) splices
//! into the whole-disk image at the rootfs partition offset.

use crate::build;
use crate::error::EngineError;
use crate::event::Step;
use crate::image::geometry::EXT4_BLOCK;
use arcbox_ext4::{FormatOptions, Formatter};
use std::io::BufReader;
use std::path::Path;
use std::process::Command;
use uuid::Uuid;

/// Format `dest` as an ext4 filesystem of exactly `size` bytes and unpack the
/// rootfs `tarball` into it, then journal it.
///
/// `size` must be a multiple of the ext4 block size (the caller's geometry
/// guarantees it). `label` is the ext4 volume label (≤ 16 bytes) the rootfs's
/// `/etc/fstab` mounts by. `uuid` is the deterministic superblock UUID the caller
/// derived from the lock, so a rebuild reproduces it instead of arcbox-ext4
/// drawing a random one.
///
/// The output is *not* byte-for-byte reproducible on its own: arcbox-ext4 0.1.2
/// stamps each inode's ctime from the wall clock (its `FormatOptions` exposes only
/// the label and UUID, not a fixed timestamp), so two builds differ in the ctime
/// fields. Fixing the UUID removes the one identifier we can reach; teaching
/// arcbox-ext4 a `SOURCE_DATE_EPOCH`-style timestamp is a planned arcbox
/// contribution that closes the residual.
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
        "formatting {}-byte ext4 rootfs at {} (arcbox-ext4)",
        size,
        dest.display()
    ));
    format_and_unpack(dest, size, tarball, label, uuid)?;
    add_journal(dest, step)?;
    make_online_resizable(dest, step)?;
    Ok(())
}

/// Run the pure-Rust format + tar unpack. Split out so the arcbox errors map in
/// one place and the (fallible) formatter is dropped/closed before journaling.
fn format_and_unpack(
    dest: &Path,
    size: u64,
    tarball: &Path,
    label: &str,
    uuid: Uuid,
) -> Result<(), EngineError> {
    let opts = FormatOptions::new(size).label(label).uuid(uuid);
    let mut fmt = Formatter::with_options(dest, opts)
        .map_err(|e| ext4_err("create ext4 formatter", e))?;
    let tar = std::fs::File::open(tarball).map_err(|s| EngineError::io(tarball, s))?;
    fmt.unpack_tar(BufReader::new(tar))
        .map_err(|e| ext4_err("unpack rootfs tar into ext4", e))?;
    // close() finalizes the superblock, group descriptors, bitmaps, and inode
    // table — the image is not a valid filesystem until it returns.
    fmt.close().map_err(|e| ext4_err("finalize ext4 image", e))?;
    Ok(())
}

/// Add an ext4 journal to the finished image file with host `tune2fs`. No
/// mount and no loop device — `tune2fs` rewrites the on-disk structures of the
/// image file in place.
fn add_journal(image: &Path, step: &Step) -> Result<(), EngineError> {
    step.log("adding ext4 journal (tune2fs -O has_journal)");
    let mut cmd = Command::new("tune2fs");
    cmd.arg("-O").arg("has_journal").arg(image);
    build::run(cmd, "tune2fs", "tune2fs -O has_journal", step)
}

/// Clear the ext4 `sparse_super2` feature so first-boot can grow the rootfs while
/// it is mounted as `/`. `arcbox-ext4` writes the filesystem with `sparse_super2`
/// (a compact two-backup-superblock layout), but the kernel's online resize
/// (`EXT4_IOC_RESIZE_FS`) does not support that layout — a mounted `resize2fs`
/// fails with "kernel does not support online resize with sparse_super2". Since
/// first-boot expands the rootfs into the enlarged partition while it is the live
/// root, the shipped image must not carry the feature.
///
/// `tune2fs -O ^sparse_super2` cannot clear it ("not supported"), but `debugfs`
/// can: it relocates the backup superblocks to the standard `sparse_super`
/// positions and updates the superblock checksum, leaving a filesystem `e2fsck`
/// reports as clean. `debugfs` exits 0 even when its scripted command fails, so the
/// clear is verified explicitly rather than trusted, and a final `e2fsck` confirms
/// consistency.
fn make_online_resizable(image: &Path, step: &Step) -> Result<(), EngineError> {
    step.log("clearing ext4 sparse_super2 for online resize (debugfs feature -sparse_super2)");
    let mut clear = Command::new("debugfs");
    clear.arg("-w").arg("-R").arg("feature -sparse_super2").arg(image);
    build::run(clear, "debugfs", "debugfs feature -sparse_super2", step)?;

    ensure_sparse_super2_cleared(image)?;

    step.log("verifying ext4 image after feature change (e2fsck -fy)");
    let mut fsck = Command::new("e2fsck");
    fsck.arg("-fy").arg(image);
    // e2fsck exit 1 = "filesystem errors corrected"; tolerated in case the clear
    // leaves anything for e2fsck to rewrite (debugfs leaves it clean in practice).
    build::run_allowing(fsck, "e2fsck", "e2fsck -fy", &[1], step)
}

/// Confirm `debugfs` actually removed `sparse_super2` — it returns 0 even when its
/// scripted command fails, so a silent failure would otherwise ship a
/// non-online-resizable image. Reads the feature list with `dumpe2fs -h` and errors
/// if the feature is still present.
fn ensure_sparse_super2_cleared(image: &Path) -> Result<(), EngineError> {
    let out = Command::new("dumpe2fs")
        .arg("-h")
        .arg(image)
        .output()
        .map_err(|source| EngineError::CommandSpawn {
            command: "dumpe2fs".to_string(),
            context: "dumpe2fs -h (verify sparse_super2 cleared)".to_string(),
            source,
        })?;
    let text = String::from_utf8_lossy(&out.stdout);
    // A missing features line means dumpe2fs did not read the superblock — treat that
    // as "could not verify" (a failure), not as "feature absent".
    let features = text
        .lines()
        .find_map(|l| l.strip_prefix("Filesystem features:"))
        .ok_or_else(|| EngineError::Ext4 {
            context: "clear sparse_super2".to_string(),
            detail: "dumpe2fs -h printed no feature list; could not verify the clear".to_string(),
        })?;
    if features.split_whitespace().any(|f| f == "sparse_super2") {
        return Err(EngineError::Ext4 {
            context: "clear sparse_super2".to_string(),
            detail: "sparse_super2 still set after debugfs; image would not be online-resizable"
                .to_string(),
        });
    }
    Ok(())
}

/// Map an `arcbox_ext4` error into a typed [`EngineError::Ext4`].
fn ext4_err(context: &str, e: impl std::fmt::Display) -> EngineError {
    EngineError::Ext4 {
        context: context.to_string(),
        detail: e.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The superblock UUID must be exactly the caller-supplied one and reproduce
    /// across independent builds — the identifier we can reach toward the
    /// contract. (Full ext4 byte-reproducibility is blocked on arcbox-ext4's
    /// wall-clock inode ctime; see [`build_rootfs_ext4`].)
    #[test]
    fn superblock_uuid_is_the_supplied_one_and_stable() {
        let tmp = tempfile::tempdir().unwrap();
        // A pair of zero blocks is a valid empty tar archive → a bare rootfs.
        let tar = tmp.path().join("empty.tar");
        std::fs::write(&tar, [0u8; 1024]).unwrap();
        let uuid = Uuid::from_bytes([0x5a; 16]);
        let size = 8 * 1024 * 1024;

        let uuid_of = |name: &str| -> [u8; 16] {
            let img = tmp.path().join(name);
            format_and_unpack(&img, size, &tar, "rootfs", uuid).unwrap();
            let bytes = std::fs::read(&img).unwrap();
            // superblock starts at byte 1024; s_uuid is at superblock offset 0x68.
            let off = 1024 + 0x68;
            bytes[off..off + 16].try_into().unwrap()
        };

        let first = uuid_of("a.ext4");
        assert_eq!(first, *uuid.as_bytes(), "superblock carries the derived UUID");
        assert_eq!(first, uuid_of("b.ext4"), "UUID reproduces across builds");
    }
}
