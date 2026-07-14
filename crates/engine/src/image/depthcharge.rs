//! The depthcharge boot payload: the vboot-signed kernel FIT that goes into the
//! ChromeOS kernel partition.
//!
//! Unlike a bootloader we compile, this payload is produced **inside the rootfs**,
//! by `depthchargectl`, during the rootfs customize step. That is deliberate: the
//! same packaged tool, reading the same `/etc/fstab`, re-signs and re-writes the
//! kernel partition through its `/etc/kernel/postinst.d` hook whenever the kernel is
//! upgraded on the running board. Producing the image's payload any other way would
//! mean the shipped image and the installed system disagreed the first time `apt`
//! touched the kernel.
//!
//! So the image node's job here is narrow: take the blob out of the rootfs tarball,
//! check that it is one *this* image can actually boot, and place it.

use crate::error::EngineError;
use crate::event::Step;
use std::path::{Path, PathBuf};
use std::process::Command;
use uuid::Uuid;

/// Where `depthchargectl` writes what it builds, inside the rootfs. The filename
/// carries the kernel version, so the payload is found by glob rather than by name.
const KPART_GLOB: &str = "./boot/depthcharge/*.img";

/// The magic at the head of a vboot keyblock — the first thing the firmware reads,
/// and a cheap proof that what we are about to write is a signed image at all rather
/// than, say, a bare kernel.
const VBOOT_MAGIC: &[u8] = b"CHROMEOS";

/// Extract the signed kernel partition image from the rootfs tarball.
///
/// A missing payload means `depthchargectl build` did not run (or failed) inside the
/// rootfs, which would otherwise surface as an unbootable image with an empty kernel
/// partition — so it is a hard error naming the cause.
pub(crate) fn extract_kpart(
    rootfs_tar: &Path,
    work_dir: &Path,
    step: &Step,
) -> Result<PathBuf, EngineError> {
    let dir = work_dir.join("kpart");
    if dir.exists() {
        std::fs::remove_dir_all(&dir).map_err(|s| EngineError::io(&dir, s))?;
    }
    std::fs::create_dir_all(&dir).map_err(|s| EngineError::io(&dir, s))?;

    let mut tar = Command::new("tar");
    tar.arg("--extract")
        .arg("--file")
        .arg(rootfs_tar)
        .arg("--directory")
        .arg(&dir)
        .arg("--strip-components=3")
        .arg("--wildcards")
        .arg("--")
        .arg(KPART_GLOB);
    // A `tar` whose wildcard matches nothing exits non-zero ("Not found in archive"),
    // which is the *same* condition as finding no payload. Its result is therefore not
    // the answer — what was actually extracted is. The caller has already proved this
    // tarball is a readable rootfs (`validate_tar`), so a failure here means the member
    // is absent, and saying *that* is far more useful than relaying tar's complaint;
    // tar's own output has already reached the event stream either way.
    let _ = crate::build::run(tar, "tar", "extract the signed kernel partition", step);

    let mut found: Vec<PathBuf> = std::fs::read_dir(&dir)
        .map_err(|s| EngineError::io(&dir, s))?
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|e| e == "img"))
        .collect();
    found.sort();
    match found.len() {
        1 => Ok(found.remove(0)),
        0 => Err(EngineError::ArtifactMissing {
            what: "the signed kernel partition — `depthchargectl build` did not run in the \
                   rootfs, so this image has no kernel to boot"
                .into(),
            location: format!("{KPART_GLOB} in {}", rootfs_tar.display()),
        }),
        // More than one kernel in the image: we would have to guess which the
        // firmware boots. The rootfs stage installs exactly one, so this means
        // something else put a kernel there.
        n => Err(EngineError::KpartInvalid {
            detail: format!(
                "the rootfs carries {n} signed kernel images under /boot/depthcharge — \
                 exactly one kernel is expected, so which would boot is ambiguous"
            ),
        }),
    }
}

/// Check that the signed payload is one *this* image can boot.
///
/// The cmdline is baked into the vboot signature, so it cannot be repaired after the
/// fact — and every way it can be wrong produces the same symptom on the hardware: a
/// board that powers up, finds no root filesystem, and reboots without a word (there
/// is no serial console on these units). Each check below is one of those silent
/// failures, turned into a build error:
///
///  - **The vboot magic.** Proves the firmware will recognize the payload at all.
///  - **`root=PARTUUID=<rootfs>`.** `depthchargectl` derives root from the image's
///    `/etc/fstab`; this asserts the fstab it read names the partition this image
///    actually writes. A mismatch is an image that boots its kernel and then cannot
///    find its own disk.
///  - **`kern_guid=%U`.** The firmware substitutes the booted kernel partition's
///    PARTUUID here, and `depthchargectl` reads it back out of `/proc/cmdline` to know
///    which partition to re-sign on a kernel upgrade. Without it, on-device upgrades
///    have no target.
///  - **No `PARTNROFF`.** A regression guard. `root=PARTUUID=%U/PARTNROFF=1` is the
///    idiom every Veyron guide reaches for, and it cannot work on a Debian initramfs:
///    initramfs-tools has no PARTNROFF support at all, so root never resolves.
pub(crate) fn verify_kpart(kpart: &Path, rootfs_partuuid: Uuid) -> Result<(), EngineError> {
    let bytes = std::fs::read(kpart).map_err(|s| EngineError::io(kpart, s))?;
    let bad = |detail: String| Err(EngineError::KpartInvalid { detail });

    if !bytes.starts_with(VBOOT_MAGIC) {
        return bad(format!(
            "it does not start with the vboot keyblock magic ({}), so the firmware would \
             not recognize it as a signed kernel",
            String::from_utf8_lossy(VBOOT_MAGIC)
        ));
    }

    // The cmdline is stored as plain text inside the signed blob, so it is checked by
    // scanning the bytes — the same thing the firmware and a hex editor both see.
    let root = format!("root=PARTUUID={}", hyphenated_lower(rootfs_partuuid));
    if !contains(&bytes, root.as_bytes()) {
        return bad(format!(
            "its signed command line does not carry `{root}` — depthchargectl derives root \
             from the image's /etc/fstab, so the fstab it read names a different partition \
             than the one this image writes"
        ));
    }
    if !contains(&bytes, b"kern_guid=%U") {
        return bad(
            "its signed command line does not carry `kern_guid=%U`, which the firmware \
             substitutes with the booted kernel partition's PARTUUID — without it, an \
             on-device kernel upgrade has no partition to re-sign"
                .into(),
        );
    }
    if contains(&bytes, b"PARTNROFF") {
        return bad(
            "its signed command line uses `PARTNROFF`, which initramfs-tools cannot \
             resolve — root would never be found. Use a concrete root=PARTUUID (which is \
             what depthchargectl derives from /etc/fstab)"
                .into(),
        );
    }
    Ok(())
}

/// A UUID in the lowercase hyphenated form the kernel's `PARTUUID=` matcher and
/// `/dev/disk/by-partuuid` both use — which is also the form `depthchargectl` reads
/// out of `/etc/fstab` and writes into the cmdline.
fn hyphenated_lower(uuid: Uuid) -> String {
    uuid.hyphenated().to_string().to_ascii_lowercase()
}

/// Whether `haystack` contains `needle` — a plain substring scan over the signed
/// blob's bytes.
fn contains(haystack: &[u8], needle: &[u8]) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    const ROOTFS_UUID: Uuid = Uuid::from_bytes([0xb2; 16]);

    /// A synthetic signed payload: the keyblock magic, then the cmdline as the
    /// firmware would find it embedded in the blob.
    fn kpart_bytes(cmdline: &str) -> Vec<u8> {
        let mut v = VBOOT_MAGIC.to_vec();
        v.extend_from_slice(&[0u8; 32]); // keyblock padding
        v.extend_from_slice(cmdline.as_bytes());
        v.extend_from_slice(&[0u8; 32]);
        v
    }

    fn write(dir: &Path, cmdline: &str) -> PathBuf {
        let p = dir.join("vmlinuz.kpart");
        std::fs::write(&p, kpart_bytes(cmdline)).unwrap();
        p
    }

    /// The cmdline a good build produces — exactly what was read back out of the
    /// image that boots the hardware.
    fn good_cmdline() -> String {
        format!(
            "kern_guid=%U console=tty1 rootwait ro panic=30 root=PARTUUID={}",
            hyphenated_lower(ROOTFS_UUID)
        )
    }

    #[test]
    fn accepts_the_cmdline_a_booting_image_carries() {
        let tmp = tempfile::tempdir().unwrap();
        let kpart = write(tmp.path(), &good_cmdline());
        verify_kpart(&kpart, ROOTFS_UUID).unwrap();
    }

    #[test]
    fn rejects_each_way_the_signed_cmdline_can_silently_fail_to_boot() {
        let tmp = tempfile::tempdir().unwrap();
        let uuid = hyphenated_lower(ROOTFS_UUID);

        // Rooting on a *different* partition than the image writes — the fstab and
        // the GPT disagreed. This is the failure the check exists for.
        let other = Uuid::from_bytes([0xcc; 16]);
        let wrong_root = format!(
            "kern_guid=%U console=tty1 ro root=PARTUUID={}",
            hyphenated_lower(other)
        );
        let kpart = write(tmp.path(), &wrong_root);
        assert!(verify_kpart(&kpart, ROOTFS_UUID).is_err());

        // No root at all.
        let kpart = write(tmp.path(), "kern_guid=%U console=tty1 ro");
        assert!(verify_kpart(&kpart, ROOTFS_UUID).is_err());

        // No kern_guid: boots once, then on-device kernel upgrades have no target.
        let kpart = write(tmp.path(), &format!("console=tty1 ro root=PARTUUID={uuid}"));
        assert!(verify_kpart(&kpart, ROOTFS_UUID).is_err());

        // The PARTNROFF idiom, which initramfs-tools cannot resolve.
        let kpart = write(
            tmp.path(),
            "kern_guid=%U console=tty1 ro root=PARTUUID=%U/PARTNROFF=1",
        );
        assert!(verify_kpart(&kpart, ROOTFS_UUID).is_err());

        // Not a signed image at all (an unsigned kernel would be silently unbootable).
        let unsigned = tmp.path().join("bare.img");
        std::fs::write(&unsigned, b"\x1f\x8b\x08not a keyblock").unwrap();
        assert!(verify_kpart(&unsigned, ROOTFS_UUID).is_err());
    }
}
