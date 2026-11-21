//! `boot2deb try` — boot the built image under QEMU system emulation and assert
//! the userland works, before anything is flashed.
//!
//! This tests the **userland**, not the board. The guest machine is `-M virt`, so no
//! board device exists in it. What it proves is:
//!
//! - Systemd reaches `multi-user.target` with no failed unit.
//! - The generated first-boot password authenticates.
//! - `first-boot` ran to completion.
//! - The image survives its second boot, because it boots the same disk **twice**.
//!   That is the failure class no single-boot smoke test finds.
//!
//! The on-image selftest runs inside the guest in userland mode, so the disk-content
//! half of the board's expectations is checked too.
//!
//! **An image whose loader is `efi` is booted once more, through firmware, before
//! that.** The guest gets the disk and the distribution's UEFI firmware for `-M virt`
//! (ArmVirtQemu, [`uefi_firmware`]), and nothing else: no kernel, no firmware boot
//! entries, no network.
//!
//! The firmware has to find GRUB at the removable-media path on the image's EFI system
//! partition. GRUB's core image has to find `/boot/grub` on the rootfs partition beside
//! it. The menu update-grub wrote has to load the image's own kernel and initrd.
//!
//! The phase passes when the kernel GRUB handed off to is running. The QEMU monitor reads
//! that from the CPU's program counter, which arm64 Linux moves to the upper half of
//! the address space. A CPU already in user mode passes too. That covers the boot path a board runs from
//! its firmware onwards, with the image's own kernel, until the board's hardware matters.
//!
//! The userland boots do not boot the board kernel. It is configured from the board's
//! fragments, for the board's console and devices, and nothing guarantees it reaches a
//! login under `-M virt`. Changing it to serve the test would change what ships. The
//! guest instead boots the suite's own generic kernel ([`fixture_kernel`]) — the kernel
//! is a fixture, the userland is what is under test. No bootloader is in the loop
//! either: QEMU loads the kernel directly. The UEFI phase above is where the boot path
//! is tested.
//!
//! Everything in the guest is driven over the serial console. `try` logs in as the
//! image's account with the password the build generated, handling the forced
//! first-login change. It then runs the assertions as shell commands and powers the
//! guest off. Driving the real login path is the point — it is the assertion that the
//! account works, not a side channel around it.
//!
//! Runtime is minutes, not seconds, under TCG on an x86 host: this replaces a
//! flash-plus-serial-console cycle, not a unit test. With KVM on a matching
//! host it is fast, and `try` uses KVM when `/dev/kvm` is usable.

use crate::error::{cage_message, EngineError};
use crate::event::{EventSink, Step, Stream};
use crate::rootfs::{provisioned_dir, sweep_provisioned, ProvisionedRoot};
use crate::sandbox::{forward_bootstrap_event, SandboxRun};
use boot2deb_core::distro::ResolvedDistro;
use boot2deb_core::model::{Arch, Loader, ResolvedBuild, ResolvedImage, SudoPolicy};
use ferroday_cage::provision;
use ferroday_cage::provision::debian::{Debian, DebianEvent};
use ferroday_cage::IdentityMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Names the fixture harvest: the step a `try` run reports it under, and the label
/// its transient root has in the work dir.
pub const FIXTURE_STAGE: &str = "try-fixture";

/// How long the guest gets to power off after the assertions, before the
/// harness concludes the shutdown is wedged and kills it. Generous because a
/// TCG guest stops slowly; a killed guest after a *successful* run is only a
/// log line, since every assertion already passed and the disk is synced.
const POWEROFF_TIMEOUT: Duration = Duration::from_secs(300);

/// Per-command timeout inside the guest. Commands here are `stat`, `systemctl
/// --failed` and the selftest — nothing long-running — but a TCG guest under a
/// loaded host is slow enough that a tight bound would flake.
const COMMAND_TIMEOUT: Duration = Duration::from_secs(600);

/// The distribution's generic kernel this architecture boots under `-M virt`, the
/// fixture: [`ResolvedDistro::virt_kernel`]. A distro-package kernel build coincides
/// with it, which `try`'s report says out loud.
pub fn fixture_package(distro: &ResolvedDistro, arch: Arch) -> Result<&str, EngineError> {
    distro
        .virt_kernel(arch)
        .ok_or_else(|| EngineError::TryBoot {
            context: "select the fixture kernel".into(),
            message: format!(
                "the {} distro layer names no generic kernel for {arch} guests \
                 (`virt_kernels`), so try cannot boot this image",
                distro.name
            ),
        })
}

/// The QEMU system emulator for the guest architecture — also the name `doctor`
/// checks as a fallback-only tool.
pub fn qemu_system(arch: Arch) -> Result<&'static str, EngineError> {
    match arch {
        Arch::Arm64 => Ok("qemu-system-aarch64"),
        Arch::Armv7 => Ok("qemu-system-arm"),
        other => Err(EngineError::TryBoot {
            context: "select the emulator".into(),
            message: format!("no QEMU machine is wired up for {other} guests"),
        }),
    }
}

/// The UEFI firmware a guest boots an `efi` image's EFI system partition under. It
/// names the distribution package that ships it, and the image QEMU loads with `-bios`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UefiFirmware {
    /// The package, from the image's own archive.
    pub package: &'static str,
    /// The firmware image's path inside the package.
    pub path: &'static str,
}

/// The UEFI firmware for `arch`: edk2's ArmVirtQemu build for `-M virt`, which Debian
/// and Ubuntu both ship as `qemu-efi-aarch64`. Loaded with `-bios`, it keeps no
/// variable between boots, as a board's firmware keeps none. The image has to boot from
/// the removable-media path alone.
pub fn uefi_firmware(arch: Arch) -> Result<UefiFirmware, EngineError> {
    match arch {
        Arch::Arm64 => Ok(UefiFirmware {
            package: "qemu-efi-aarch64",
            path: "/usr/share/qemu-efi-aarch64/QEMU_EFI.fd",
        }),
        other => Err(EngineError::TryBoot {
            context: "select the UEFI firmware".into(),
            message: format!("no UEFI firmware is wired up for {other} guests"),
        }),
    }
}

/// The fixture a guest boots with. That is the suite's generic kernel, an initramfs
/// that can find a virtio root, and the UEFI firmware an `efi` image needs. Each is
/// harvested from the archive's own `.deb`s ([`fixture_kernel`]).
pub struct FixtureKernel {
    /// The kernel image (`vmlinuz`).
    pub kernel: PathBuf,
    /// The matching initramfs, built by `initramfs-tools` inside a target-arch
    /// root at harvest time, with its `MODULES=most` default. That default is what
    /// puts the virtio drivers in it.
    pub initrd: PathBuf,
    /// The UEFI firmware ([`uefi_firmware`]), harvested when the spec asked for it
    /// ([`FixtureSpec::uefi`]).
    pub firmware: Option<PathBuf>,
}

/// What the fixture root is provisioned from: a suite, an architecture, an archive to
/// resolve it against, and the two directories a transient tree needs.
///
/// `try` stands this root up for itself rather than borrowing the build's.
///
/// A `systemd` postinst cannot be configured under the **single-identity** map, which
/// is the map every [`BuildSandbox`](crate::sandbox::BuildSandbox) root uses. The
/// postinst `fchownat`s `/var/lib/systemd/network` to `systemd-network` and sets a
/// POSIX ACL naming `adm`. A user namespace that maps no id but root returns `EINVAL`
/// for both.
///
/// The map decides this and the overlay does not: an overlay upper under a subordinate
/// map takes the same chown and the same ACL. A build root's map is nonetheless fixed
/// by its base. `stage_layer` requires a layer to match the base it increments. A build
/// root is mapped single so that the artifacts it writes back through a bind belong to
/// the calling user.
///
/// So the fixture gets a root of its own, provisioned the way the image's userland is
/// ([`crate::rootfs`]). It is a plain directory under the subordinate map, where the
/// postinst's real ids exist.
pub struct FixtureSpec<'a> {
    /// The guest architecture, which names the emulator.
    pub arch: Arch,
    /// The generic kernel package the guest boots, from [`fixture_package`].
    pub kernel: &'a str,
    /// The archive the fixture kernel comes from — the image's, so the guest boots the
    /// generic kernel of the release it runs.
    pub archive: &'a crate::Archive,
    /// The archive components to resolve against: the distribution's full set.
    pub components: &'a [String],
    /// The archive keyring verifying every pocket's `Release` signature.
    pub keyring: &'a Path,
    /// Where downloaded `.deb`s are cached, shared with the build's own provisioner
    /// runs so a `try` after a `build` refetches nothing it already holds.
    pub deb_cache: &'a Path,
    /// The work dir the transient root is provisioned under. Not `TMPDIR`: the tree is
    /// a Debian userland with real ownership, and `clean` reclaims it from here.
    pub scratch_dir: &'a Path,
    /// Harvest the UEFI firmware as well ([`uefi_firmware`]), for an image whose loader
    /// is `efi`.
    pub uefi: bool,
}

/// Obtain the fixture kernel for `spec`, cached under `dir`.
///
/// The distribution's generic kernel metapackage ([`FixtureSpec::kernel`]) and
/// `initramfs-tools` are installed into a freshly provisioned target-arch root of the
/// image's suite. The kernel's own postinst therefore builds the
/// initramfs inside a real userland of the pinned suite. That is what puts the virtio
/// drivers in it, and the reason this needs no host `dpkg`.
///
/// The root is a plain directory under the subordinate identity map, and is removed
/// through that map when the harvest returns. [`FixtureSpec`] says why it cannot be one
/// of the build's layered roots.
///
/// The kernel and initramfs are copied out and reused on later runs. `refresh`
/// discards the cached pair and harvests again, which is how a new point release of
/// the suite kernel is picked up.
pub fn fixture_kernel(
    spec: &FixtureSpec,
    dir: &Path,
    refresh: bool,
    step: &Step,
) -> Result<FixtureKernel, EngineError> {
    let kernel = dir.join("vmlinuz");
    let initrd = dir.join("initrd.img");
    let firmware = match spec.uefi {
        true => Some((uefi_firmware(spec.arch)?, dir.join("uefi-firmware.fd"))),
        false => None,
    };
    let firmware_path = firmware.as_ref().map(|(_, path)| path.clone());
    if !refresh
        && kernel.is_file()
        && initrd.is_file()
        && firmware_path.as_ref().is_none_or(|path| path.is_file())
    {
        step.log(format!(
            "reusing the fixture at {} (--refresh-fixture re-harvests)",
            dir.display()
        ));
        return Ok(FixtureKernel {
            kernel,
            initrd,
            firmware: firmware_path,
        });
    }
    std::fs::create_dir_all(dir).map_err(|s| EngineError::io(dir, s))?;
    let package = spec.kernel;
    let arch = spec.arch.debian_arch();
    step.log(format!(
        "harvesting the fixture kernel: {package} + initramfs-tools in a {arch} {} root",
        spec.archive.suite()
    ));

    std::fs::create_dir_all(spec.scratch_dir).map_err(|s| EngineError::io(spec.scratch_dir, s))?;
    sweep_provisioned(spec.scratch_dir);
    let root = provisioned_dir(spec.scratch_dir, "fixture");
    let mut debian = fixture_provisioner(spec, package, firmware.as_ref().map(|(f, _)| f.package))?;
    let mut sink = |event: DebianEvent<'_>| forward_bootstrap_event(step, event);
    provision::ensure(&root, &mut debian.observe(&mut sink)).map_err(|source| {
        EngineError::Bootstrap {
            context: format!("provision the {arch} {} fixture root", spec.archive.suite()),
            message: cage_message(&source),
        }
    })?;
    let _provisioned = ProvisionedRoot::new(root.clone());

    // One kernel exists in a fresh root, so the globs are unambiguous. The copy runs
    // inside the root because `initramfs-tools` writes its initrd `0600 root:root`,
    // which the calling user cannot read from outside; inside the subordinate map the
    // command is that root, and `dir` is bound at its own path so the pair is written to the
    // host. The map's inside-root is the calling user, so the harvested pair belongs to
    // the caller rather than to a subuid.
    let mut script = format!(
        "set -e; cp /boot/vmlinuz-* '{dir}/vmlinuz'; cp /boot/initrd.img-* '{dir}/initrd.img'; \
         chmod 0644 '{dir}/vmlinuz' '{dir}/initrd.img'",
        dir = dir.display()
    );
    if let Some((firmware, path)) = &firmware {
        script.push_str(&format!(
            "; cp '{}' '{}'; chmod 0644 '{}'",
            firmware.path,
            path.display(),
            path.display()
        ));
    }
    crate::sandbox::run_in(
        crate::sandbox::baseline(&root).identity_map(IdentityMap::Subordinate),
        &SandboxRun {
            work: dir,
            binds: &[dir.to_path_buf()],
            env: &[],
            argv: &["sh".into(), "-c".into(), script],
            context: "harvest the fixture kernel from its installed deb",
            probe: None,
        },
        step,
    )?;
    unwrap_fixture_kernel(&kernel, spec.arch, step)?;
    Ok(FixtureKernel {
        kernel,
        initrd,
        firmware: firmware_path,
    })
}

/// Rewrite the harvested kernel at `path` into a form QEMU's `-kernel` loader boots,
/// when it is wrapped in one that loader cannot read ([`loadable_kernel`]). A kernel
/// that needs nothing is left untouched.
fn unwrap_fixture_kernel(path: &Path, arch: Arch, step: &Step) -> Result<(), EngineError> {
    let fail = |message: String| EngineError::TryBoot {
        context: format!("unwrap the fixture kernel {}", path.display()),
        message,
    };
    let bytes = std::fs::read(path).map_err(|s| EngineError::io(path, s))?;
    let loadable = loadable_kernel(&bytes).map_err(fail)?;
    if loadable == bytes {
        return Ok(());
    }
    // An arm64 kernel unwraps to the raw `Image`, whose header names it at 0x38.
    if arch == Arch::Arm64 && loadable.get(0x38..0x3c) != Some(b"ARM\x64") {
        return Err(fail(
            "it unwrapped to something that is not an arm64 Image".into(),
        ));
    }
    std::fs::write(path, &loadable).map_err(|s| EngineError::io(path, s))?;
    step.log(format!(
        "unwrapped the fixture kernel to a raw Image QEMU loads ({} -> {} bytes)",
        bytes.len(),
        loadable.len()
    ));
    Ok(())
}

/// The kernel QEMU's `-kernel` loader can boot, from `image` as a distribution ships it.
///
/// Two wrappers stand between a shipped arm64 kernel and that loader, and each is
/// removed:
///
///  - A PE image with a `.linux` section, the layout of a systemd-stub kernel such as
///    Ubuntu's, which adds devicetree and hardware-ID sections a firmware stub picks
///    from. The kernel is the `.linux` section. QEMU reads none of the PE.
///  - An EFI zboot image (`zimg` at offset 4), whose payload the EFI stub decompresses
///    when firmware runs it. QEMU unpacks a gzip payload alone, and a zstd one is
///    unpacked here, gzip too, so the guest sees one form.
///
/// Anything else is returned as it is, for QEMU to load as it would. Debian's gzip
/// `Image` and its armhf `zImage` are both that.
fn loadable_kernel(image: &[u8]) -> Result<Vec<u8>, String> {
    if let Some(linux) = pe_section(image, b".linux") {
        return loadable_kernel(linux);
    }
    if image.get(4..8) == Some(b"zimg") {
        return unpack_zboot(image);
    }
    Ok(image.to_vec())
}

/// The bytes of the section named `name` in the PE image `image`, or `None` when
/// `image` is not a PE image or has no such section.
fn pe_section<'a>(image: &'a [u8], name: &[u8]) -> Option<&'a [u8]> {
    let u16_at = |at: usize| Some(u16::from_le_bytes(image.get(at..at + 2)?.try_into().ok()?));
    let u32_at = |at: usize| Some(u32::from_le_bytes(image.get(at..at + 4)?.try_into().ok()?));
    if image.get(0..2) != Some(b"MZ") {
        return None;
    }
    let pe = u32_at(0x3c)? as usize;
    if image.get(pe..pe + 4) != Some(b"PE\0\0") {
        return None;
    }
    let sections = u16_at(pe + 6)? as usize;
    let table = pe + 24 + u16_at(pe + 20)? as usize;
    (0..sections).find_map(|i| {
        let entry = table + i * 40;
        let raw_name = image.get(entry..entry + 8)?;
        let len = raw_name.iter().position(|&b| b == 0).unwrap_or(8);
        if &raw_name[..len] != name {
            return None;
        }
        // The data is the virtual size's worth of the raw data: the raw size is padded
        // to the file alignment.
        let size = u32_at(entry + 8)?.min(u32_at(entry + 16)?) as usize;
        let start = u32_at(entry + 20)? as usize;
        image.get(start..start.checked_add(size)?)
    })
}

/// The payload of the EFI zboot image `image`, decompressed.
///
/// The header the kernel's `zboot-header.S` writes: the payload's offset at 8 and its
/// size at 12, both little-endian `u32`, and the compression's name at 24 as a
/// NUL-terminated string.
fn unpack_zboot(image: &[u8]) -> Result<Vec<u8>, String> {
    use std::io::Read;
    let u32_at = |at: usize| {
        image
            .get(at..at + 4)
            .map(|b| u32::from_le_bytes(b.try_into().unwrap()) as usize)
            .ok_or_else(|| "its EFI zboot header is truncated".to_string())
    };
    let (offset, size) = (u32_at(8)?, u32_at(12)?);
    let payload = offset
        .checked_add(size)
        .and_then(|end| image.get(offset..end))
        .ok_or_else(|| "its EFI zboot payload lies outside the file".to_string())?;
    let name = image.get(24..56).unwrap_or_default();
    let name = &name[..name.iter().position(|&b| b == 0).unwrap_or(name.len())];
    let mut out = Vec::new();
    let read = match name {
        b"gzip" => flate2::read::GzDecoder::new(payload).read_to_end(&mut out),
        b"zstd" => ruzstd::decoding::StreamingDecoder::new(payload)
            .map_err(|e| format!("its zstd payload does not open: {e}"))?
            .read_to_end(&mut out),
        other => {
            return Err(format!(
                "it is an EFI zboot image compressed with '{}', which try does not unpack",
                String::from_utf8_lossy(other)
            ))
        }
    };
    read.map_err(|e| format!("its EFI zboot payload does not decompress: {e}"))?;
    Ok(out)
}

/// The provisioner the fixture root is bootstrapped with: `spec`'s archive, the
/// subordinate identity map, and the kernel packages as the only includes over a
/// `required`-priority base.
///
/// `busybox` is named outright because Debian's `initramfs-tools` only Recommends it,
/// and Recommends are never installed — an initramfs built without it has no rescue
/// shell. Ubuntu's depends on its own `busybox-initramfs`, and `busybox` beside it
/// changes nothing. `firmware` is the UEFI firmware's package, where the run needs it.
/// The base stays at the library's `required` minimum: this root exists to run one
/// postinst, not to be a userland anyone works in.
fn fixture_provisioner(
    spec: &FixtureSpec,
    package: &str,
    firmware: Option<&str>,
) -> Result<Debian, EngineError> {
    let b = Debian::builder(spec.archive.suite())
        .architecture(spec.arch.debian_arch())
        .identity_map(IdentityMap::Subordinate)
        .cache_dir(spec.deb_cache)
        .include(
            [package, "initramfs-tools", "busybox"]
                .into_iter()
                .chain(firmware),
        );
    let b = spec.archive.configure(b, spec.components, spec.keyring)?;
    b.build().map_err(|source| EngineError::Bootstrap {
        context: format!(
            "configure the {} {} fixture bootstrap",
            spec.arch.debian_arch(),
            spec.archive.suite()
        ),
        message: cage_message(&source),
    })
}

/// One `try` run: what to boot, as what, and how patient to be.
pub struct TryOptions<'a> {
    /// The resolved build the image was built from — supplies the architecture, and
    /// the account name.
    pub build: &'a ResolvedBuild,
    /// The image half of that build: the sudo policy, which decides how root is
    /// reached in the guest. `try` boots an image, so a deliverable without one never
    /// reaches here.
    pub resolved_image: &'a ResolvedImage,
    /// The built image artifact (`.img`, or its `.xz`/`.gz` compression).
    /// Never mutated: the run boots a decompressed copy.
    pub image: &'a Path,
    /// Where the disk copy lives for the run. Deleted afterwards unless
    /// [`keep_disk`](Self::keep_disk).
    pub disk: PathBuf,
    /// The fixture kernel pair to boot with.
    pub fixture: &'a FixtureKernel,
    /// The image's account name.
    pub user: &'a str,
    /// The generated first-boot password, from the image's credential document
    /// ([`boot2deb_core::credential`]).
    pub password: &'a str,
    /// How long one boot is allowed to take to reach a login prompt (and how long
    /// `systemctl is-system-running --wait` is allowed to take after it). Under TCG
    /// this is minutes.
    pub boot_timeout: Duration,
    /// Keep the disk copy after the run — for a post-mortem, or to boot it by
    /// hand. Note the first login was forced to change the account password. The
    /// report contains the one that is now set.
    pub keep_disk: bool,
}

/// What one boot of the guest established.
pub struct BootReport {
    /// `systemctl is-system-running` after settling — `running` is the pass.
    pub state: String,
    /// Modification time (epoch seconds) of the first-boot stamp. Compared
    /// across the two boots: an unchanged stamp is the proof first-boot did not
    /// re-run.
    pub stamp: String,
    /// The selftest's summary line from its userland-mode run in the guest.
    pub selftest: String,
}

/// What the UEFI phase established ([`try_boot`] on an `efi` image).
pub struct UefiReport {
    /// The kernel release GRUB's default entry loaded, as its `Loading Linux` line
    /// names it.
    pub kernel: String,
    /// The CPU state the QEMU monitor read once the kernel was running: its program
    /// counter and exception level.
    pub cpu: String,
}

/// The whole run: every boot passed everything it asserts.
pub struct TryReport {
    /// The firmware boot of an `efi` image, before the userland boots. `None` under
    /// any other loader.
    pub uefi: Option<UefiReport>,
    /// The first boot — the one that runs first-boot and is forced to change
    /// the account password at login.
    pub first: BootReport,
    /// The second boot of the same disk.
    pub second: BootReport,
    /// The account password now set on the disk copy (the forced first-login
    /// change replaces the generated one). Only meaningful with
    /// [`TryOptions::keep_disk`]. The built image is untouched.
    pub disk_password: String,
}

/// Whether KVM can accelerate a guest of `arch` on this host: the host must be
/// that architecture and `/dev/kvm` must exist. TCG otherwise.
fn kvm_usable(arch: Arch) -> bool {
    let host_matches = match arch {
        Arch::Arm64 => std::env::consts::ARCH == "aarch64",
        // 32-bit guests on an aarch64 host would need EL1 AArch32 support,
        // which recent cores dropped; TCG is the dependable answer.
        _ => false,
    };
    host_matches && Path::new("/dev/kvm").exists()
}

/// The kernel command line the guest boots with. `root=PARTUUID=` because the
/// partition index varies by boot method (a seed partition precedes the rootfs
/// everywhere, and ChromeOS kernel slots precede it on depthcharge);
/// `systemd.mask=systemd-modules-load.service` because the image's
/// `modules-load.d` names board-kernel modules the fixture kernel cannot have —
/// masking it here keeps that a property of the try boot, not of the image. `masks`
/// are the further units the image's fstab would have the fixture kernel fail on
/// ([`fixture_masks`]).
/// `panic=-1` plus QEMU's `-no-reboot` turns a kernel panic into a prompt QEMU
/// exit instead of a hung run.
fn append_line(rootfs_partuuid: &str, masks: &[&str]) -> String {
    let mut line = format!(
        "root=PARTUUID={rootfs_partuuid} rw rootwait console=ttyAMA0 panic=-1 \
         systemd.mask=systemd-modules-load.service"
    );
    for unit in masks {
        line.push_str(&format!(" systemd.mask={unit}"));
    }
    line
}

/// The units the fixture kernel boots this image with masked, beyond module loading.
///
/// Under the `efi` loader that is the EFI system partition's mount. Its filesystem
/// driver is a module of the fixture kernel's, which the image's `/lib/modules` (the
/// board kernel's) cannot provide, so the mount would fail and leave a failed unit.
/// The image mounts it `nofail`, so the masked unit holds nothing else back. The
/// partition is tested by the UEFI phase instead, and the selftest reports its files
/// not applicable in userland mode.
fn fixture_masks(image: &ResolvedImage) -> Vec<&'static str> {
    match image.loader.as_ref().map(|l| l.loader()) {
        Some(Loader::Efi) => vec![ESP_MOUNT_UNIT],
        _ => Vec::new(),
    }
}

/// systemd's mount unit for [`ESP_MOUNT_POINT`](boot2deb_core::model::ESP_MOUNT_POINT).
const ESP_MOUNT_UNIT: &str = "boot-efi.mount";

/// The QEMU invocation for one boot. Pure so the shape is testable; `kvm` is a
/// host fact the caller supplies ([`kvm_usable`]).
fn qemu_argv(
    arch: Arch,
    fixture: &FixtureKernel,
    disk: &Path,
    append: &str,
    kvm: bool,
) -> Result<Vec<String>, EngineError> {
    // `max` is TCG's most capable CPU either way; under KVM the host CPU is the
    // only honest choice.
    let (cpu, memory) = match arch {
        Arch::Arm64 => (if kvm { "host" } else { "max" }, "2048"),
        _ => ("max", "1024"),
    };
    let mut argv: Vec<String> = vec![
        "-M".into(),
        "virt".into(),
        "-cpu".into(),
        cpu.into(),
        "-m".into(),
        memory.into(),
        "-smp".into(),
        "2".into(),
        "-nographic".into(),
        "-no-reboot".into(),
        "-kernel".into(),
        fixture.kernel.display().to_string(),
        "-initrd".into(),
        fixture.initrd.display().to_string(),
        "-append".into(),
        append.to_string(),
        "-drive".into(),
        format!("file={},format=raw,if=virtio", disk.display()),
        // User-mode networking: the guest gets a NIC and DHCP with no host
        // privileges, so dhcpcd/NetworkManager configure something real.
        "-netdev".into(),
        "user,id=net0".into(),
        "-device".into(),
        "virtio-net-pci,netdev=net0".into(),
    ];
    if kvm {
        argv.push("-enable-kvm".into());
    }
    let _ = qemu_system(arch)?; // arch gate; the program name is the caller's
    Ok(argv)
}

/// The rootfs partition's PARTUUID, read back from the prepared disk's own GPT —
/// no assumption about which index the rootfs was placed at.
fn rootfs_partuuid(disk: &Path) -> Result<String, EngineError> {
    let table = crate::press::verify::read_back_table(disk)?;
    table
        .iter()
        .find(|e| e.name == "rootfs")
        .map(|e| e.part_guid.clone())
        .ok_or_else(|| EngineError::TryBoot {
            context: "find the rootfs partition".into(),
            message: format!(
                "no GPT entry named 'rootfs' on {} — is this artifact a boot image?",
                disk.display()
            ),
        })
}

fn err(context: &str, message: impl Into<String>) -> EngineError {
    EngineError::TryBoot {
        context: context.to_string(),
        message: message.into(),
    }
}

// ---------------------------------------------------------------------------
// The serial console
// ---------------------------------------------------------------------------

/// Largest control sequence held while waiting for its terminator. A shell's
/// semantic-prompt marker is a few hundred bytes; past this the stream is not
/// speaking a terminal protocol, and [`VtFilter`] stops treating it as one.
const MAX_SEQUENCE: usize = 4096;

/// Reduces the guest's console bytes to the text they contain: control sequences
/// dropped, and `\r` with them.
///
/// Every sentinel here is anchored at a line start (`\nB2D-READY`, `\nB2D-RC-`), and
/// on a real console **nothing** sits at a line start. A terminal ends a line `\r\n`,
/// and the guest's shell then announces the command with an OSC marker and a
/// bracketed-paste `CSI`. So the byte after the newline is a carriage return or an
/// escape, and against a raw stream none of the sentinels ever matches. Stripping is
/// also what keeps a marker's payload from matching by accident: it names the user,
/// the host and the command, which are the words the login conversation looks for.
///
/// Dropping `\r` outright is what a driver reading line-oriented output wants. A
/// terminal's `\r` is carriage control, not content: it ends a line together with
/// `\n`, or it returns the cursor for a redraw — and a transcript has no cursor, so
/// a redrawn line reads as its successive states either way.
///
/// A sequence can straddle a read, so a partial one is held until its terminator
/// arrives.
#[derive(Default)]
struct VtFilter {
    /// The control sequence being read, from its `ESC` onwards. Empty between
    /// sequences.
    pending: Vec<u8>,
}

impl VtFilter {
    /// Feed `chunk`; return the printable text it contained.
    fn push(&mut self, chunk: &[u8]) -> String {
        const ESC: u8 = 0x1b;
        let mut out: Vec<u8> = Vec::with_capacity(chunk.len());
        for &byte in chunk {
            if self.pending.is_empty() {
                match byte {
                    ESC => self.pending.push(byte),
                    b'\r' => {}
                    _ => out.push(byte),
                }
                continue;
            }
            self.pending.push(byte);
            if Self::complete(&self.pending) {
                self.pending.clear();
            } else if self.pending.len() > MAX_SEQUENCE {
                // Never swallow the guest's output. An unterminated sequence this
                // long is not one, so the bytes after the `ESC` are text after all.
                out.extend_from_slice(&self.pending[1..]);
                self.pending.clear();
            }
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    /// Whether `seq` — which starts with `ESC` — is a whole control sequence.
    ///
    /// The three families that matter are the CSI sequences a terminal draws with,
    /// the string sequences (`OSC`, `DCS`, `SOS`, `PM`, `APC`) a shell writes its
    /// markers as, and the short two- and three-byte escapes. A string sequence ends
    /// at `BEL` or at `ST`, and `ST` is itself an `ESC`, which is why the terminator
    /// is read as the last two bytes rather than the last one.
    fn complete(seq: &[u8]) -> bool {
        if seq.len() < 2 {
            return false;
        }
        match seq[1] {
            b'[' => seq.len() >= 3 && (0x40..=0x7e).contains(&seq[seq.len() - 1]),
            b']' | b'P' | b'X' | b'^' | b'_' => {
                let last = seq[seq.len() - 1];
                last == 0x07 || (seq.len() >= 4 && seq[seq.len() - 2] == 0x1b && last == b'\\')
            }
            b'(' | b')' | b'*' | b'+' | b'-' | b'.' | b'/' | b'%' | b'#' | b' ' => seq.len() >= 3,
            _ => true,
        }
    }
}

/// The guest's serial console: bytes in from a reader thread, lines out through
/// a writer, and an accumulated transcript the expect calls scan.
///
/// Generic over the transport so the login and command drivers are tested
/// against a scripted fake guest (a `UnixStream` pair) — the QEMU integration
/// contributes only the transport.
struct Console {
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    writer: Box<dyn Write + Send>,
    /// Strips the guest's control sequences before anything is matched or relayed.
    filter: VtFilter,
    /// The transcript so far, lossy UTF-8 and free of control sequences and `\r`.
    buf: String,
    /// Where the next expect scan starts — advanced past each match so a prompt
    /// is never matched twice.
    cursor: usize,
    /// Start of the first not-yet-relayed line, for streaming the transcript to
    /// the event sink as it arrives.
    line_start: usize,
}

impl Console {
    fn new<R, W>(reader: R, writer: W) -> Console
    where
        R: Read + Send + 'static,
        W: Write + Send + 'static,
    {
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = reader;
            let mut chunk = [0u8; 4096];
            loop {
                match reader.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if tx.send(chunk[..n].to_vec()).is_err() {
                            break;
                        }
                    }
                }
            }
        });
        Console {
            rx,
            writer: Box::new(writer),
            filter: VtFilter::default(),
            buf: String::new(),
            cursor: 0,
            line_start: 0,
        }
    }

    /// Send one line to the guest (a newline is appended).
    fn send_line(&mut self, line: &str) -> Result<(), EngineError> {
        self.writer
            .write_all(format!("{line}\n").as_bytes())
            .and_then(|()| self.writer.flush())
            .map_err(|e| err("write to the guest console", e.to_string()))
    }

    /// Relay every complete line received since the last relay to `step`, so
    /// `--verbose` streams the guest console live.
    fn relay_lines(&mut self, step: &Step) {
        while let Some(nl) = self.buf[self.line_start..].find('\n') {
            let end = self.line_start + nl;
            step.relay(Stream::Stdout, self.buf[self.line_start..end].to_string());
            self.line_start = end + 1;
        }
    }

    /// Pull whatever the guest has produced, without blocking longer than
    /// `wait`.
    fn pump(&mut self, wait: Duration, step: &Step) {
        if let Ok(chunk) = self.rx.recv_timeout(wait) {
            let text = self.filter.push(&chunk);
            self.buf.push_str(&text);
            // Drain anything else already queued.
            while let Ok(more) = self.rx.try_recv() {
                let text = self.filter.push(&more);
                self.buf.push_str(&text);
            }
            self.relay_lines(step);
        }
    }

    /// Wait until any of `patterns` appears in the transcript after the cursor;
    /// return the matched index and the text between the old cursor and the
    /// match. The earliest occurrence wins when several patterns are present,
    /// so a prompt is handled in the order the guest produced it.
    fn wait_for(
        &mut self,
        patterns: &[&str],
        timeout: Duration,
        step: &Step,
    ) -> Result<(usize, String), EngineError> {
        let deadline = Instant::now() + timeout;
        loop {
            let window = &self.buf[self.cursor..];
            let hit = patterns
                .iter()
                .enumerate()
                .filter_map(|(i, p)| window.find(p).map(|pos| (pos, i, p.len())))
                .min();
            if let Some((pos, idx, len)) = hit {
                let before = window[..pos].to_string();
                self.cursor += pos + len;
                return Ok((idx, before));
            }
            let now = Instant::now();
            if now >= deadline {
                let tail: String = self.buf.chars().rev().take(2000).collect::<Vec<_>>()[..]
                    .iter()
                    .rev()
                    .collect();
                return Err(err(
                    "wait for the guest console",
                    format!(
                        "timed out after {}s waiting for one of {:?}; console tail:\n{}",
                        timeout.as_secs(),
                        patterns,
                        tail
                    ),
                ));
            }
            self.pump((deadline - now).min(Duration::from_millis(500)), step);
        }
    }
}

// ---------------------------------------------------------------------------
// Driving the guest
// ---------------------------------------------------------------------------

/// The password the forced first-login change sets, derived from the generated
/// one. Unrelated text on purpose: `pam_unix`'s obscure checks reject a new
/// password that is a rotation, reversal, or case change of the old, so a
/// derived-but-similar value would fail interactively.
fn changed_password(original: &str) -> String {
    format!(
        "b2d.{}",
        &crate::blobs::sha256_hex(original.as_bytes())[..16]
    )
}

const P_LOGIN: &str = "login:";
const P_PASSWORD: &str = "Password:";
const P_CURRENT: &str = "Current password:";
const P_NEW: &str = "New password:";
const P_RETYPE: &str = "Retype new password:";
const P_INCORRECT: &str = "Login incorrect";
/// Debian's default interactive prompts end `:~$` (user) / `:~#` (root) in the
/// login shell's home directory — the anchor that says a shell arrived. The
/// probe command then replaces the prompt with an unambiguous sentinel.
const P_SHELL_USER: &str = ":~$";
const P_SHELL_ROOT: &str = ":~#";

/// Log in on the console as `user`, handling the forced first-login password
/// change (`chage -d 0` semantics: the image ships the password expired).
/// Returns the password that is active once a shell is reached.
fn login(
    console: &mut Console,
    user: &str,
    password: &str,
    boot_timeout: Duration,
    step: &Step,
) -> Result<String, EngineError> {
    let new_password = changed_password(password);
    console.wait_for(&[P_LOGIN], boot_timeout, step)?;
    console.send_line(user)?;
    let mut active = password.to_string();
    let mut changed = false;
    // Bounded: the longest legitimate path is password → forced change (three
    // prompts) → logout → re-login (login + password) → shell.
    for _ in 0..12 {
        let (idx, _) = console.wait_for(
            &[
                P_CURRENT,
                P_RETYPE,
                P_NEW,
                P_PASSWORD,
                P_INCORRECT,
                P_LOGIN,
                P_SHELL_USER,
                P_SHELL_ROOT,
            ],
            COMMAND_TIMEOUT,
            step,
        )?;
        match idx {
            0 => console.send_line(password)?,
            1 => {
                console.send_line(&new_password)?;
                changed = true;
                active = new_password.clone();
            }
            2 => console.send_line(&new_password)?,
            3 => console.send_line(&active)?,
            4 => {
                return Err(err(
                    "log in to the guest",
                    format!(
                        "the guest rejected the {} password for '{user}' — \
                         the account or the generated password does not work",
                        if changed { "changed" } else { "generated" }
                    ),
                ))
            }
            5 => console.send_line(user)?, // logged out after the change; again
            _ => {
                // A shell. Pin the prompt to a sentinel so command parsing
                // never depends on PS1's default shape again.
                console.send_line("PS1='B2D> '; echo B2D-READY")?;
                console.wait_for(&["\nB2D-READY"], COMMAND_TIMEOUT, step)?;
                return Ok(active);
            }
        }
    }
    Err(err(
        "log in to the guest",
        "the login conversation did not converge (looping prompts)",
    ))
}

/// Run one command in the logged-in shell; return its exit code and output.
///
/// The command line is echoed back by the tty before it runs, so the exit-code
/// sentinel is matched only at a line start (`\nB2D-RC-`) and only when digits
/// follow — the echo contains the unexpanded `$?`, which fails the digit check.
fn run_cmd(
    console: &mut Console,
    cmd: &str,
    timeout: Duration,
    step: &Step,
) -> Result<(i32, String), EngineError> {
    console.send_line(&format!("{cmd}; echo B2D-RC-$?"))?;
    let start = console.cursor;
    let deadline = Instant::now() + timeout;
    loop {
        let window = &console.buf[start..];
        let mut search = 0;
        let mut found = None;
        while let Some(pos) = window[search..].find("\nB2D-RC-") {
            let digits_at = search + pos + "\nB2D-RC-".len();
            let digits: String = window[digits_at..]
                .chars()
                .take_while(|c| c.is_ascii_digit())
                .collect();
            // The code line is complete once a non-digit follows the digits
            // (the tty ends the line with \r or \n).
            if !digits.is_empty() && window[digits_at + digits.len()..].chars().next().is_some() {
                found = Some((search + pos, digits_at + digits.len(), digits));
                break;
            }
            search += pos + 1;
        }
        if let Some((rel_pos, rel_end, digits)) = found {
            let output = window[..rel_pos].to_string();
            console.cursor = start + rel_end;
            let rc = digits
                .parse::<i32>()
                .map_err(|_| err("parse a guest exit code", format!("bad digits '{digits}'")))?;
            // Drop everything through the tty's echo of the command line —
            // recognizable by the unexpanded `$?` in its sentinel tail, which
            // command output cannot reproduce (the guest expands it to digits).
            // Through the last echo: a line typed while the shell is still busy is
            // echoed by the tty and again when readline redraws it.
            let output = match output.rfind("B2D-RC-$?") {
                Some(echo) => match output[echo..].find('\n') {
                    Some(nl) => output[echo + nl + 1..].to_string(),
                    None => String::new(),
                },
                None => output,
            };
            return Ok((rc, output));
        }
        let now = Instant::now();
        if now >= deadline {
            return Err(err(
                "run a command in the guest",
                format!(
                    "'{cmd}' produced no exit code within {}s",
                    timeout.as_secs()
                ),
            ));
        }
        console.pump((deadline - now).min(Duration::from_millis(500)), step);
    }
}

/// `cmd` run as root under the image's sudo policy: `nopasswd` directly, and `password`
/// by handing `sudo -S` the account's `active` password on stdin — the one [`login`]
/// set, which the run's report already contains. `-p ''` keeps sudo's prompt out of the
/// output a result is read from.
fn as_root(sudo: SudoPolicy, active: &str, cmd: &str) -> String {
    match sudo {
        SudoPolicy::Nopasswd => format!("sudo -n {cmd}"),
        SudoPolicy::Password => {
            let quoted = active.replace('\'', r"'\''");
            format!("printf '%s\\n' '{quoted}' | sudo -S -p '' {cmd}")
        }
    }
}

/// The command that runs the on-image selftest as root ([`as_root`]), in the mode
/// built for a guest that is not the board. Root is what makes dmesg and the
/// initramfs readable.
fn selftest_command(sudo: SudoPolicy, active: &str) -> String {
    as_root(sudo, active, "/usr/lib/boot2deb/selftest --mode userland")
}

/// The command that powers the guest off, as root ([`as_root`]). An image ships no
/// polkit, so logind refuses a power-off asked for by the account itself, and the guest
/// would run until the harness kills it.
fn poweroff_command(sudo: SudoPolicy, active: &str) -> String {
    as_root(sudo, active, "poweroff")
}

/// One boot's assertions, already logged in as the account whose password is `active`:
/// system state, first-boot stamp, and the selftest.
fn assert_booted(
    console: &mut Console,
    image: &ResolvedImage,
    active: &str,
    boot_timeout: Duration,
    step: &Step,
) -> Result<BootReport, EngineError> {
    // Settle first: `--wait` blocks until startup finishes, so everything after
    // it sees the final state rather than a race.
    let (_, state_out) = run_cmd(
        console,
        "systemctl is-system-running --wait",
        boot_timeout,
        step,
    )?;
    let state = state_out
        .split_whitespace()
        .last()
        .unwrap_or("")
        .to_string();
    if state != "running" {
        // `--no-pager`: the serial console is a tty, so systemctl would otherwise hand
        // the list to `less`, which waits on the console for a key.
        let (_, failed) = run_cmd(
            console,
            "systemctl --failed --no-legend --plain --no-pager",
            COMMAND_TIMEOUT,
            step,
        )?;
        return Err(err(
            "reach multi-user cleanly",
            format!(
                "the guest settled as '{state}', not 'running'; failed units:\n{}",
                failed.trim()
            ),
        ));
    }
    let (rc, stamp_out) = run_cmd(
        console,
        "stat -c %Y /var/lib/boot2deb/first-boot.done",
        COMMAND_TIMEOUT,
        step,
    )?;
    if rc != 0 {
        return Err(err(
            "check the first-boot stamp",
            "first-boot never completed: /var/lib/boot2deb/first-boot.done is absent",
        ));
    }
    let stamp = stamp_out.trim().to_string();
    let selftest_cmd = selftest_command(image.sudo, active);
    let (rc, selftest_out) = run_cmd(console, &selftest_cmd, COMMAND_TIMEOUT, step)?;
    let summary = selftest_out
        .lines()
        .rev()
        .find(|l| !l.trim().is_empty())
        .unwrap_or("")
        .trim()
        .to_string();
    if rc != 0 {
        return Err(err(
            "pass the on-image selftest",
            format!("selftest (userland mode) failed in the guest:\n{selftest_out}"),
        ));
    }
    Ok(BootReport {
        state,
        stamp,
        selftest: summary,
    })
}

/// A spawned guest that is killed (and reaped) however the run ends.
struct Guest {
    child: std::process::Child,
}

impl Guest {
    /// Wait for the guest to exit on its own — the post-poweroff path.
    fn wait_exit(&mut self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => return true,
                Ok(None) => std::thread::sleep(Duration::from_millis(500)),
                Err(_) => return false,
            }
        }
        false
    }
}

impl Drop for Guest {
    fn drop(&mut self) {
        // Idempotent: killing an exited child is an ignorable error, and the
        // wait reaps it either way.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Boot the disk once, log in, assert, and power off. `password` is the
/// password to log in with; the returned pair is the report and the password
/// active after the boot (changed by the forced first login, then stable).
fn boot_once(
    opts: &TryOptions,
    rootfs_partuuid: &str,
    password: &str,
    phase: &str,
    step: &Step,
) -> Result<(BootReport, String), EngineError> {
    let program = qemu_system(opts.build.arch)?;
    let kvm = kvm_usable(opts.build.arch);
    let argv = qemu_argv(
        opts.build.arch,
        opts.fixture,
        &opts.disk,
        &append_line(rootfs_partuuid, &fixture_masks(opts.resolved_image)),
        kvm,
    )?;
    step.log(format!(
        "{phase}: {program} -M virt ({}), disk {}",
        if kvm { "KVM" } else { "TCG emulation" },
        opts.disk.display()
    ));
    let mut child = std::process::Command::new(program)
        .args(&argv)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        // QEMU's own warnings join the guest transcript in the logs rather
        // than the build host's terminal.
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            err(
                "launch QEMU",
                format!("{program}: {e} (see `boot2deb doctor` for the package to install)"),
            )
        })?;
    let stdout = child.stdout.take().expect("piped");
    let stdin = child.stdin.take().expect("piped");
    let stderr = child.stderr.take().expect("piped");
    // Drain stderr so QEMU cannot block on it; keep the tail for errors.
    let stderr_tail = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    {
        let tail = std::sync::Arc::clone(&stderr_tail);
        std::thread::spawn(move || {
            let mut stderr = stderr;
            let mut buf = String::new();
            let _ = stderr.read_to_string(&mut buf);
            *tail.lock().unwrap() = buf;
        });
    }
    let mut guest = Guest { child };
    let mut console = Console::new(stdout, stdin);

    let run = (|| {
        let active = login(&mut console, opts.user, password, opts.boot_timeout, step)?;
        let report = assert_booted(
            &mut console,
            opts.resolved_image,
            &active,
            opts.boot_timeout,
            step,
        )?;
        // Sync before poweroff so the stamp and journal survive even a shutdown
        // the harness ends up killing.
        let _ = run_cmd(&mut console, "sync", COMMAND_TIMEOUT, step)?;
        console.send_line(&poweroff_command(opts.resolved_image.sudo, &active))?;
        Ok((report, active))
    })();

    match run {
        Ok(result) => {
            if !guest.wait_exit(POWEROFF_TIMEOUT) {
                step.log("the guest did not power off in time; killing it (disk already synced)");
            }
            Ok(result)
        }
        Err(e) => {
            drop(guest); // kill before reading the stderr tail
            let tail = stderr_tail.lock().unwrap();
            Err(match tail.trim().is_empty() {
                true => e,
                false => err(phase, format!("{e}\nQEMU stderr:\n{}", tail.trim())),
            })
        }
    }
}

/// The QEMU invocation for the UEFI phase: the firmware with `-bios`, the disk, a
/// monitor socket the CPU state is read through, and no network, so the firmware has
/// nothing to try but the disk. Pure so the shape is testable.
fn qemu_uefi_argv(
    arch: Arch,
    firmware: &Path,
    disk: &Path,
    monitor: &Path,
    kvm: bool,
) -> Result<Vec<String>, EngineError> {
    let _ = qemu_system(arch)?; // arch gate; the program name is the caller's
    let cpu = if kvm { "host" } else { "max" };
    let mut argv: Vec<String> = vec![
        "-M".into(),
        "virt".into(),
        "-cpu".into(),
        cpu.into(),
        "-m".into(),
        "2048".into(),
        "-smp".into(),
        "2".into(),
        "-nographic".into(),
        "-no-reboot".into(),
        "-bios".into(),
        firmware.display().to_string(),
        "-drive".into(),
        format!("file={},format=raw,if=virtio", disk.display()),
        "-nic".into(),
        "none".into(),
        "-monitor".into(),
        format!("unix:{},server=on,wait=off", monitor.display()),
    ];
    if kvm {
        argv.push("-enable-kvm".into());
    }
    Ok(argv)
}

/// What GRUB prints when it cannot boot, and the prompts it falls back to: an error
/// line, its rescue shell (the core image found no `/boot/grub`), and the firmware's
/// own shell (the firmware found no loader at all). GRUB's error lines start the line,
/// which keeps a kernel log line that merely contains the word from matching.
const UEFI_FAILURES: [&str; 3] = ["\nerror:", "grub rescue>", "Shell>"];

/// The kernel is running once the CPU executes from the upper half of the address
/// space, where arm64 Linux maps its own image, or has reached user mode. The firmware,
/// GRUB and the kernel's EFI stub all run identity-mapped, from the lower half.
fn kernel_running(registers: &str) -> Option<String> {
    let pc = registers
        .split_whitespace()
        .find_map(|word| word.strip_prefix("PC="))
        .and_then(|hex| u64::from_str_radix(hex, 16).ok())?;
    let el = registers
        .split_whitespace()
        .find(|word| {
            word.len() == 4 && word.starts_with("EL") && word.as_bytes()[2].is_ascii_digit()
        })
        .unwrap_or("EL?");
    let state = format!("PC={pc:016x} {el}");
    (pc >= 0x8000_0000_0000_0000 || el.starts_with("EL0")).then_some(state)
}

/// Ask the QEMU monitor at `socket` for the boot CPU's registers.
fn read_registers(socket: &Path) -> Result<String, String> {
    use std::os::unix::net::UnixStream;
    let mut stream = UnixStream::connect(socket).map_err(|e| e.to_string())?;
    stream
        .set_read_timeout(Some(Duration::from_secs(5)))
        .map_err(|e| e.to_string())?;
    stream
        .write_all(b"info registers\n")
        .map_err(|e| e.to_string())?;
    let mut out = Vec::new();
    let mut chunk = [0u8; 4096];
    // The reply ends at the monitor's next prompt, after the register dump.
    loop {
        match stream.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                out.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&out);
                if text.contains("PC=") && text.trim_end().ends_with("(qemu)") {
                    break;
                }
            }
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// The longest path a UNIX socket address holds (`sun_path`, less its terminator).
const MAX_SOCKET_PATH: usize = 107;

/// Boot the disk through UEFI firmware and assert the boot path reaches the image's
/// kernel — see the module docs. The guest is stopped as soon as the kernel is seen
/// running, so the disk is left exactly as it was.
///
/// The monitor socket lives in a private directory of its own under the temporary
/// directory rather than beside the disk: a socket path is bounded by `sun_path`, and
/// a work dir is free to be deeper than that.
fn uefi_boot(opts: &TryOptions, firmware: &Path, step: &Step) -> Result<UefiReport, EngineError> {
    let program = qemu_system(opts.build.arch)?;
    let kvm = kvm_usable(opts.build.arch);
    let socket_dir = tempfile::Builder::new()
        .prefix("b2d-try-")
        .tempdir()
        .map_err(|e| {
            err(
                "UEFI boot",
                format!("make the monitor socket's directory: {e}"),
            )
        })?;
    let monitor = socket_dir.path().join("monitor");
    if monitor.as_os_str().len() > MAX_SOCKET_PATH {
        return Err(err(
            "UEFI boot",
            format!(
                "the monitor socket path {} is longer than a UNIX socket address holds \
                 ({MAX_SOCKET_PATH} bytes); point TMPDIR at a shorter directory",
                monitor.display()
            ),
        ));
    }
    let argv = qemu_uefi_argv(opts.build.arch, firmware, &opts.disk, &monitor, kvm)?;
    step.log(format!(
        "UEFI boot: {program} -M virt -bios {} ({}), disk {}",
        firmware.display(),
        if kvm { "KVM" } else { "TCG emulation" },
        opts.disk.display()
    ));
    let mut child = std::process::Command::new(program)
        .args(&argv)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        // QEMU's own complaints, kept for the error rather than the build host's
        // terminal.
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| {
            err(
                "launch QEMU",
                format!("{program}: {e} (see `boot2deb doctor` for the package to install)"),
            )
        })?;
    let stdout = child.stdout.take().expect("piped");
    let stdin = child.stdin.take().expect("piped");
    let mut stderr = child.stderr.take().expect("piped");
    // Read to the end, which comes when QEMU exits, and joined once the guest is
    // killed, so an error contains all of it.
    let stderr_reader = std::thread::spawn(move || {
        let mut buf = String::new();
        let _ = stderr.read_to_string(&mut buf);
        buf
    });
    let mut guest = Guest { child };
    let mut console = Console::new(stdout, stdin);

    let run = (|| {
        let fail = |what: &str, before: &str| {
            err(
                "UEFI boot",
                format!("{what}; console before it:\n{}", before.trim()),
            )
        };
        // Wait for any of `patterns`, a few seconds at a time, so a QEMU that has
        // exited is reported at once rather than when the whole timeout runs out.
        let mut expect = |console: &mut Console,
                          patterns: &[&str],
                          timeout: Duration|
         -> Result<(usize, String), EngineError> {
            let deadline = Instant::now() + timeout;
            loop {
                let slice = deadline
                    .saturating_duration_since(Instant::now())
                    .min(Duration::from_secs(5));
                match console.wait_for(patterns, slice, step) {
                    Ok(hit) => return Ok(hit),
                    Err(e) if Instant::now() >= deadline => return Err(e),
                    Err(_) => {}
                }
                if let Ok(Some(status)) = guest.child.try_wait() {
                    return Err(err(
                        "UEFI boot",
                        format!("QEMU exited ({status}) while waiting for one of {patterns:?}"),
                    ));
                }
            }
        };
        // The firmware found GRUB at the removable path and GRUB reached its menu,
        // which it draws only once the core image has found /boot/grub and read the
        // menu there.
        let mut patterns = vec!["GNU GRUB"];
        patterns.extend(UEFI_FAILURES);
        let (idx, before) = expect(&mut console, &patterns, opts.boot_timeout)?;
        if idx != 0 {
            return Err(fail(
                &format!(
                    "the boot stopped at `{}` before GRUB's menu",
                    patterns[idx].trim()
                ),
                &before,
            ));
        }
        // Boot the default entry now rather than after the menu's timeout.
        console.send_line("")?;
        let mut patterns = vec!["Loading Linux "];
        patterns.extend(UEFI_FAILURES);
        let (idx, before) = expect(&mut console, &patterns, COMMAND_TIMEOUT)?;
        if idx != 0 {
            return Err(fail("GRUB's default entry did not start", &before));
        }
        let (_, kernel) = expect(&mut console, &[" ..."], COMMAND_TIMEOUT)?;
        let mut patterns = vec!["Loading initial ramdisk"];
        patterns.extend(UEFI_FAILURES);
        let (idx, before) = expect(&mut console, &patterns, COMMAND_TIMEOUT)?;
        if idx != 0 {
            return Err(fail("GRUB could not load the kernel", &before));
        }
        // The hand-off: poll the boot CPU until it runs the kernel, and fail on any
        // error GRUB or the EFI stub prints on the way. The CPU is read first, so what
        // the kernel itself prints once it runs is never taken for a hand-off failure.
        let deadline = Instant::now() + opts.boot_timeout;
        loop {
            console.pump(Duration::from_secs(1), step);
            if let Some(cpu) = read_registers(&monitor)
                .ok()
                .as_deref()
                .and_then(kernel_running)
            {
                return Ok(UefiReport {
                    kernel: kernel.trim().to_string(),
                    cpu,
                });
            }
            let tail = &console.buf[console.cursor..];
            if let Some(found) = UEFI_FAILURES
                .iter()
                .chain(&["EFI stub: ERROR"])
                .find(|p| tail.contains(*p))
            {
                return Err(fail(
                    &format!("the hand-off to the kernel failed (`{}`)", found.trim()),
                    tail,
                ));
            }
            if let Ok(Some(status)) = guest.child.try_wait() {
                return Err(fail(
                    &format!("QEMU exited ({status}) before the kernel was seen running"),
                    tail,
                ));
            }
            if Instant::now() >= deadline {
                return Err(fail(
                    &format!(
                        "GRUB loaded Linux {} but the CPU never ran it within {}s",
                        kernel.trim(),
                        opts.boot_timeout.as_secs()
                    ),
                    tail,
                ));
            }
        }
    })();
    drop(guest); // kill, which ends QEMU's stderr
    let stderr = stderr_reader.join().unwrap_or_default();
    run.map_err(|e| match stderr.trim().is_empty() {
        true => e,
        false => err("UEFI boot", format!("{e}\nQEMU stderr:\n{}", stderr.trim())),
    })
}

/// Boot the built image twice under QEMU and assert the userland works — see
/// the module docs for exactly what is and is not covered.
pub fn try_boot(opts: &TryOptions, sink: &dyn EventSink) -> Result<TryReport, EngineError> {
    let step = Step::start(sink, "try");
    if let Some(parent) = opts.disk.parent() {
        std::fs::create_dir_all(parent).map_err(|s| EngineError::io(parent, s))?;
    }
    step.log(format!(
        "copying {} -> {} (the artifact is never booted directly)",
        opts.image.display(),
        opts.disk.display()
    ));
    {
        let mut dest =
            std::fs::File::create(&opts.disk).map_err(|s| EngineError::io(&opts.disk, s))?;
        crate::press::write::stream_image(opts.image, &mut dest, &step)?;
        dest.sync_all()
            .map_err(|s| EngineError::io(&opts.disk, s))?;
    }
    let partuuid = rootfs_partuuid(&opts.disk)?;
    step.log(format!("rootfs PARTUUID {partuuid}"));

    let run = (|| {
        // The firmware boot first, on the disk exactly as built: it reads and writes
        // nothing, and stops as soon as the kernel is running, before the kernel could
        // mount anything.
        let uefi = match opts.resolved_image.loader.as_ref().map(|l| l.loader()) {
            Some(Loader::Efi) => {
                let firmware = opts.fixture.firmware.as_deref().ok_or_else(|| {
                    err(
                        "UEFI boot",
                        "the image's loader is efi, and the fixture holds no UEFI firmware",
                    )
                })?;
                let report = uefi_boot(opts, firmware, &step)?;
                step.log(format!(
                    "UEFI boot passed: firmware -> GRUB -> Linux {} ({})",
                    report.kernel, report.cpu
                ));
                Some(report)
            }
            _ => None,
        };
        let (first, active) = boot_once(opts, &partuuid, opts.password, "first boot", &step)?;
        step.log(format!(
            "first boot passed: {}, selftest: {}",
            first.state, first.selftest
        ));
        // The second boot of the same disk: the check no single-boot smoke test
        // finds, and the login now runs with the changed password.
        let (second, active) = boot_once(opts, &partuuid, &active, "second boot", &step)?;
        if second.stamp != first.stamp {
            return Err(err(
                "second boot",
                format!(
                    "first-boot re-ran on the second boot (stamp {} -> {}) — \
                     the run-once gate is broken",
                    first.stamp, second.stamp
                ),
            ));
        }
        step.log(format!(
            "second boot passed: {}, selftest: {}, first-boot did not re-run",
            second.state, second.selftest
        ));
        Ok(TryReport {
            uefi,
            first,
            second,
            disk_password: active,
        })
    })();

    if !opts.keep_disk {
        let _ = std::fs::remove_file(&opts.disk);
    } else if run.is_ok() {
        step.log(format!(
            "kept the try disk at {} (its account password was changed at first login; \
             the report contains it)",
            opts.disk.display()
        ));
    }
    let report = run?;
    step.finish();
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::os::unix::net::UnixStream;

    fn step_sink() -> impl Fn(crate::event::Event) {
        |_e| {}
    }

    /// What a `turing-rk1/forky` guest actually puts between a newline and the first
    /// character of a command's output: a carriage return, the shell's
    /// bracketed-paste `CSI`, and its OSC command marker. Every one of them made a
    /// line-anchored sentinel unmatchable.
    const OSC_MARKER: &str = concat!(
        "\r",
        "\u{1b}[?2004l",
        "\u{1b}]3008;start=1a64bfa4;user=debian;hostname=turing-rk1;",
        "type=command\u{1b}\\"
    );

    /// The matching end-of-command marker, which precedes the next prompt.
    const OSC_END: &str = "\u{1b}]3008;end=1a64bfa4;exit=success\u{1b}\\";

    #[test]
    fn a_shell_marker_between_the_newline_and_the_sentinel_is_stripped() {
        let mut vt = VtFilter::default();
        let text = vt.push(format!("\r\n{OSC_MARKER}B2D-READY\r\n").as_bytes());
        assert_eq!(text, "\nB2D-READY\n");
        assert!(
            text.contains("\nB2D-READY"),
            "the sentinel is still unmatchable: {text:?}"
        );
    }

    /// A read can end anywhere, including inside a sequence, so the filter has to
    /// keep the partial one across chunks. One byte at a time is the worst case.
    #[test]
    fn a_sequence_split_across_reads_is_still_stripped() {
        let raw = format!("up\r\n{OSC_MARKER}B2D-RC-0\r\n");
        let mut vt = VtFilter::default();
        let mut text = String::new();
        for byte in raw.as_bytes() {
            text.push_str(&vt.push(&[*byte]));
        }
        assert_eq!(text, "up\nB2D-RC-0\n");
        assert!(text.contains("\nB2D-RC-"), "{text:?}");
    }

    /// The families a terminal actually sends, each ending its own way: CSI at a
    /// final byte, a string sequence at `BEL` or at `ST`, and a two-byte escape at
    /// its second byte.
    #[test]
    fn every_sequence_family_is_recognized_and_plain_text_survives() {
        let mut vt = VtFilter::default();
        let text = vt.push(
            concat!(
                "\u{1b}[0;32m",
                "ok",
                "\u{1b}[0m",
                "\u{1b}]0;a title\u{7}",
                "\u{1b}(B",
                "\u{1b}7",
                " done\n",
            )
            .as_bytes(),
        );
        assert_eq!(text, "ok done\n");
    }

    /// Losing the guest's output is worse than showing noise, so an `ESC` that never
    /// terminates is eventually read as the text it evidently is.
    #[test]
    fn an_unterminated_escape_gives_the_output_back_rather_than_eating_it() {
        let mut vt = VtFilter::default();
        let mut raw = String::from("\u{1b}]");
        raw.push_str(&"x".repeat(MAX_SEQUENCE + 8));
        let text = vt.push(raw.as_bytes());
        assert!(
            text.ends_with("xxxx"),
            "the stream was swallowed: {} bytes out",
            text.len()
        );
    }

    /// A spec standing in for a `try` run's, containing only what the provisioner reads.
    fn fixture_spec<'a>(
        archive: &'a crate::Archive,
        components: &'a [String],
        keyring: &'a Path,
        dirs: &'a Path,
    ) -> FixtureSpec<'a> {
        FixtureSpec {
            arch: Arch::Arm64,
            kernel: "linux-image-arm64",
            archive,
            components,
            keyring,
            deb_cache: dirs,
            scratch_dir: dirs,
            uefi: false,
        }
    }

    /// The identity map is the whole reason this root exists rather than a build root,
    /// so it is the one setting worth asserting. The rest is what makes the root
    /// resolve the image's own kernel: its suite, its architecture, and the packages
    /// whose postinst builds the initramfs.
    #[test]
    fn the_fixture_root_is_provisioned_under_the_subordinate_map() {
        let (archive, components) = crate::bootstrap::test_archive("forky");
        let keyring = crate::bootstrap::test_keyring();
        let tmp = tempfile::tempdir().unwrap();
        let spec = fixture_spec(&archive, &components, &keyring, tmp.path());
        let rendered = format!(
            "{:?}",
            fixture_provisioner(&spec, spec.kernel, None).unwrap()
        );
        assert!(!rendered.contains("qemu-efi-aarch64"), "{rendered}");
        assert!(
            rendered.contains("identity_map: Subordinate"),
            "the fixture root was configured under some other map: {rendered}"
        );
        assert!(rendered.contains(r#"suite: "forky""#), "{rendered}");
        assert!(rendered.contains(r#"architecture: "arm64""#), "{rendered}");
        for package in ["linux-image-arm64", "initramfs-tools", "busybox"] {
            assert!(rendered.contains(package), "{package} missing: {rendered}");
        }
        // Nothing layers this root, so it has no base to increment.
        assert!(rendered.contains("base_layer: None"), "{rendered}");

        // An efi image's run harvests the UEFI firmware from the same root.
        let firmware = uefi_firmware(Arch::Arm64).unwrap();
        let rendered = format!(
            "{:?}",
            fixture_provisioner(&spec, spec.kernel, Some(firmware.package)).unwrap()
        );
        assert!(rendered.contains("qemu-efi-aarch64"), "{rendered}");
        assert!(uefi_firmware(Arch::Armv7).is_err());
    }

    /// The UEFI phase gives the firmware the disk and nothing else to boot: no kernel,
    /// no network to try first, and a monitor to read the CPU through.
    #[test]
    fn the_uefi_invocation_boots_the_disk_through_the_firmware_alone() {
        let argv = qemu_uefi_argv(
            Arch::Arm64,
            Path::new("/w/try/fixture/uefi-firmware.fd"),
            Path::new("/w/try/disk.img"),
            Path::new("/w/try/disk.monitor"),
            false,
        )
        .unwrap()
        .join(" ");
        assert!(
            argv.contains("-bios /w/try/fixture/uefi-firmware.fd"),
            "{argv}"
        );
        assert!(
            argv.contains("file=/w/try/disk.img,format=raw,if=virtio"),
            "{argv}"
        );
        assert!(argv.contains("-nic none"), "{argv}");
        assert!(
            argv.contains("-monitor unix:/w/try/disk.monitor,server=on,wait=off"),
            "{argv}"
        );
        assert!(!argv.contains("-kernel"), "{argv}");
        assert!(!argv.contains("-append"), "{argv}");
    }

    /// The firmware, GRUB and the EFI stub run identity-mapped; the kernel runs in the
    /// upper half, or has handed the CPU to user mode.
    #[test]
    fn the_kernel_is_running_once_the_cpu_leaves_the_lower_half() {
        let firmware =
            "PC=00000000bfd4c3a0 X00=0000000000000000\nPSTATE=600003c5 -ZC- EL1h\n(qemu)";
        assert_eq!(kernel_running(firmware), None);
        let kernel = "PC=ffff800080e1d2c4 X00=0000000000000000\nPSTATE=600003c5 -ZC- EL1h\n(qemu)";
        assert_eq!(
            kernel_running(kernel).as_deref(),
            Some("PC=ffff800080e1d2c4 EL1h")
        );
        let user = "PC=0000ffff9e3a1b20 X00=0000000000000000\nPSTATE=80001000 N--- EL0t\n(qemu)";
        assert_eq!(
            kernel_running(user).as_deref(),
            Some("PC=0000ffff9e3a1b20 EL0t")
        );
        assert_eq!(kernel_running("(qemu)"), None);
    }

    /// The fixture kernel boots an efi image's userland with the ESP's mount masked:
    /// its filesystem driver is a module the image cannot provide that kernel.
    #[test]
    fn an_efi_image_boots_its_userland_with_the_esp_mount_masked() {
        // The unit systemd names the mount point's mount after.
        assert_eq!(
            ESP_MOUNT_UNIT,
            format!(
                "{}.mount",
                boot2deb_core::model::ESP_MOUNT_POINT
                    .trim_start_matches('/')
                    .replace('/', "-")
            )
        );
        let line = append_line("0000-1111", &[ESP_MOUNT_UNIT]);
        assert!(line.contains("systemd.mask=boot-efi.mount"), "{line}");
        assert!(!append_line("0000-1111", &[]).contains("boot-efi"));
    }

    #[test]
    fn the_qemu_invocation_names_the_machine_the_root_and_the_mask() {
        let fixture = FixtureKernel {
            kernel: PathBuf::from("/w/try/vmlinuz"),
            initrd: PathBuf::from("/w/try/initrd.img"),
            firmware: None,
        };
        let argv = qemu_argv(
            Arch::Arm64,
            &fixture,
            Path::new("/w/try/disk.img"),
            &append_line("0000-1111", &[]),
            false,
        )
        .unwrap();
        let joined = argv.join(" ");
        assert!(joined.contains("-M virt"), "{joined}");
        assert!(joined.contains("-cpu max"), "{joined}");
        assert!(joined.contains("root=PARTUUID=0000-1111"), "{joined}");
        assert!(
            joined.contains("systemd.mask=systemd-modules-load.service"),
            "the image's modules-load.d names board modules the fixture kernel \
             cannot have:\n{joined}"
        );
        assert!(joined.contains("panic=-1"), "{joined}");
        assert!(!joined.contains("-enable-kvm"), "{joined}");
        assert!(
            joined.contains("file=/w/try/disk.img,format=raw,if=virtio"),
            "{joined}"
        );

        // KVM asks for the host CPU; TCG cannot.
        let kvm = qemu_argv(Arch::Arm64, &fixture, Path::new("/w/d.img"), "x", true)
            .unwrap()
            .join(" ");
        assert!(
            kvm.contains("-enable-kvm") && kvm.contains("-cpu host"),
            "{kvm}"
        );

        assert_eq!(qemu_system(Arch::Armv7).unwrap(), "qemu-system-arm");
        assert!(qemu_system(Arch::Riscv64).is_err());
    }

    /// A raw arm64 `Image`: the magic at 0x38, which every unwrapping ends in.
    fn raw_image() -> Vec<u8> {
        let mut image: Vec<u8> = (0..=255).collect();
        image[0x38..0x3c].copy_from_slice(b"ARM\x64");
        image
    }

    /// An EFI zboot image whose payload is `compressed`, under the scheme `name`.
    fn zboot(name: &str, compressed: &[u8]) -> Vec<u8> {
        let offset = 0x80u32;
        let mut image = vec![0u8; offset as usize];
        image[0..2].copy_from_slice(b"MZ");
        image[4..8].copy_from_slice(b"zimg");
        image[8..12].copy_from_slice(&offset.to_le_bytes());
        image[12..16].copy_from_slice(&(compressed.len() as u32).to_le_bytes());
        image[24..24 + name.len()].copy_from_slice(name.as_bytes());
        image.extend_from_slice(compressed);
        // The kernel appends the uncompressed size after the payload.
        image.extend_from_slice(&256u32.to_le_bytes());
        image
    }

    /// A PE image whose one section, `.linux`, holds `linux`, its raw data padded to
    /// a 512-byte file alignment.
    fn pe_with_linux(linux: &[u8]) -> Vec<u8> {
        let mut image = vec![0u8; 0x200];
        image[0..2].copy_from_slice(b"MZ");
        image[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        image[0x40..0x44].copy_from_slice(b"PE\0\0");
        image[0x44..0x46].copy_from_slice(&0xaa64u16.to_le_bytes());
        image[0x46..0x48].copy_from_slice(&1u16.to_le_bytes());
        // No optional header, so the section table follows the COFF header.
        let entry = 0x40 + 24;
        let raw = linux.len().div_ceil(0x200) * 0x200;
        image[entry..entry + 6].copy_from_slice(b".linux");
        image[entry + 8..entry + 12].copy_from_slice(&(linux.len() as u32).to_le_bytes());
        image[entry + 16..entry + 20].copy_from_slice(&(raw as u32).to_le_bytes());
        image[entry + 20..entry + 24].copy_from_slice(&0x200u32.to_le_bytes());
        image.extend_from_slice(linux);
        image.resize(0x200 + raw, 0);
        image
    }

    fn gzip(data: &[u8]) -> Vec<u8> {
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut gz, data).unwrap();
        gz.finish().unwrap()
    }

    /// Ubuntu's arm64 kernel is a systemd-stub PE whose `.linux` section is a zstd EFI
    /// zboot image. Both wrappers come off, down to the raw `Image`; a gzip zboot image
    /// unpacks the same way. Debian's shapes pass through as they are.
    #[test]
    fn a_wrapped_kernel_unwraps_to_its_raw_image() {
        let raw = raw_image();
        let zstd = ruzstd::encoding::compress_to_vec(
            &raw[..],
            ruzstd::encoding::CompressionLevel::Fastest,
        );
        assert_eq!(
            loadable_kernel(&pe_with_linux(&zboot("zstd", &zstd))).unwrap(),
            raw
        );
        assert_eq!(loadable_kernel(&zboot("zstd", &zstd)).unwrap(), raw);
        assert_eq!(loadable_kernel(&zboot("gzip", &gzip(&raw))).unwrap(), raw);

        // A raw Image, and the gzip one QEMU unpacks itself.
        assert_eq!(loadable_kernel(&raw).unwrap(), raw);
        assert_eq!(loadable_kernel(&gzip(&raw)).unwrap(), gzip(&raw));

        let err = loadable_kernel(&zboot("lz4", b"x")).unwrap_err();
        assert!(err.contains("'lz4'"), "{err}");
        let mut beyond = zboot("zstd", &zstd);
        beyond[12..16].copy_from_slice(&u32::MAX.to_le_bytes());
        let err = loadable_kernel(&beyond).unwrap_err();
        assert!(err.contains("outside the file"), "{err}");
    }

    /// The harvested file is rewritten only when it changes, and an arm64 kernel that
    /// unwraps to anything but an `Image` is refused rather than handed to QEMU, where
    /// it would boot to a silent console.
    #[test]
    fn the_harvested_kernel_is_rewritten_only_to_a_raw_image() {
        let sink = |_e: crate::event::Event| {};
        let step = Step::start(&sink, "test");
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("vmlinuz");
        let zstd = |data: &[u8]| {
            ruzstd::encoding::compress_to_vec(data, ruzstd::encoding::CompressionLevel::Fastest)
        };

        std::fs::write(&path, pe_with_linux(&zboot("zstd", &zstd(&raw_image())))).unwrap();
        unwrap_fixture_kernel(&path, Arch::Arm64, &step).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), raw_image());

        std::fs::write(&path, gzip(&raw_image())).unwrap();
        unwrap_fixture_kernel(&path, Arch::Arm64, &step).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), gzip(&raw_image()));

        let wrapped = zboot("zstd", &zstd(b"not a kernel"));
        std::fs::write(&path, &wrapped).unwrap();
        let err = unwrap_fixture_kernel(&path, Arch::Arm64, &step)
            .unwrap_err()
            .to_string();
        assert!(err.contains("not an arm64 Image"), "{err}");
        assert_eq!(std::fs::read(&path).unwrap(), wrapped);
    }

    /// The fixture kernel is the image's distribution's own, by architecture, from the
    /// shipped distro layers. An architecture a layer names none for is refused before
    /// anything is provisioned, naming the layer.
    #[test]
    fn the_fixture_kernel_is_the_distributions_own() {
        let debian = crate::bootstrap::test_distro("forky");
        let ubuntu = crate::bootstrap::test_distro("resolute");
        assert_eq!(
            fixture_package(&debian, Arch::Arm64).unwrap(),
            "linux-image-arm64"
        );
        assert_eq!(
            fixture_package(&debian, Arch::Armv7).unwrap(),
            "linux-image-armmp"
        );
        assert_eq!(
            fixture_package(&ubuntu, Arch::Arm64).unwrap(),
            "linux-image-virtual"
        );
        for (distro, arch) in [(&debian, Arch::Riscv64), (&ubuntu, Arch::Armv7)] {
            let err = fixture_package(distro, arch).unwrap_err().to_string();
            assert!(err.contains(&distro.name), "{err}");
        }
    }

    /// A scripted guest on the far end of a socketpair: reads what the driver
    /// sends, answers as a Debian serial console would. `expired` ships the
    /// account with the password expired, forcing the change-and-relogin path.
    fn fake_guest(mut sock: UnixStream, password: String, expired: bool) {
        std::thread::spawn(move || {
            let mut reader = BufReader::new(sock.try_clone().unwrap());
            let mut send = |s: &str| {
                let _ = sock.write_all(s.as_bytes());
            };
            let mut read_line = || {
                let mut l = String::new();
                let _ = reader.read_line(&mut l);
                l.trim_end().to_string()
            };
            // Colored status lines and the shell's own markers, as a forky guest
            // sends them. Without them the driver is tested against a stream no
            // guest produces, and a sentinel it can never match reads as passing.
            send("[\u{1b}[0;32m  OK  \u{1b}[0m] Reached target multi-user.target\r\n");
            send("\r\nDebian GNU/Linux forky testhost ttyAMA0\r\n\r\ntesthost login: ");
            let _user = read_line();
            send("Password: ");
            let got = read_line();
            if got != password {
                send("\r\nLogin incorrect\r\ntesthost login: ");
                return;
            }
            let mut active = password.clone();
            if expired {
                send("You are required to change your password immediately (administrator enforced).\r\n");
                send("Changing password for debian.\r\nCurrent password: ");
                let _ = read_line();
                send("New password: ");
                let new1 = read_line();
                send("Retype new password: ");
                let new2 = read_line();
                if new1 != new2 {
                    send("Sorry, passwords do not match.\r\n");
                    return;
                }
                // Debian's login logs the session out after a forced change.
                send("passwd: password updated successfully\r\ntesthost login: ");
                let _user = read_line();
                send("Password: ");
                if read_line() != new1 {
                    send("\r\nLogin incorrect\r\n");
                    return;
                }
                active = new1;
            }
            // `sudo -S` answers to the password in force now, not the generated one.
            let sudo_selftest = selftest_command(SudoPolicy::Password, &active);
            send("Linux testhost 6.12.0 aarch64\r\ndebian@testhost:~$ ");
            // The shell: echo each command line, answer the sentinel probes.
            loop {
                let line = read_line();
                if line.is_empty() {
                    break;
                }
                // tty echo, then the marker the shell writes before the command's
                // own output — which appears between the newline and the first
                // character of every line the driver anchors a sentinel to.
                send(&format!("{line}\r\n{OSC_MARKER}")); // tty echo
                if line.starts_with("PS1=") {
                    send(&format!("B2D-READY\r\n{OSC_END}B2D> "));
                } else if let Some(cmd) = line.strip_suffix("; echo B2D-RC-$?") {
                    match cmd {
                        "systemctl is-system-running --wait" => {
                            send(&format!("running\r\nB2D-RC-0\r\n{OSC_END}B2D> "))
                        }
                        "stat -c %Y /var/lib/boot2deb/first-boot.done" => {
                            send(&format!("1755640000\r\nB2D-RC-0\r\n{OSC_END}B2D> "))
                        }
                        // Echoed twice, as a line typed while the shell was busy is:
                        // once by the tty, once when readline redraws it.
                        "systemctl --failed --no-legend --plain --no-pager" => send(&format!(
                            "B2D> {line}\r\nunit.service loaded failed failed A unit\r\n\
                             B2D-RC-0\r\n{OSC_END}B2D> "
                        )),
                        c if c == "sudo -n /usr/lib/boot2deb/selftest --mode userland"
                            || c == sudo_selftest =>
                        {
                            send(&format!("identity\r\n  ok      kernel-release    7.1.6\r\n\r\n9 ok, 4 not applicable.\r\nB2D-RC-0\r\n{OSC_END}B2D> "))
                        }
                        c if c.contains("| sudo -S") => send(&format!(
                            "sudo: 1 incorrect password attempt\r\nB2D-RC-1\r\n{OSC_END}B2D> "
                        )),
                        "sync" => send(&format!("B2D-RC-0\r\n{OSC_END}B2D> ")),
                        _ => send(&format!("B2D-RC-127\r\n{OSC_END}B2D> ")),
                    }
                } else if line == "sudo -n poweroff" {
                    send("[  OK  ] Reached target poweroff.target\r\n");
                    break;
                }
            }
        });
    }

    fn session(password: &str, expired: bool) -> Console {
        let (ours, theirs) = UnixStream::pair().unwrap();
        fake_guest(theirs, password.to_string(), expired);
        Console::new(ours.try_clone().unwrap(), ours)
    }

    #[test]
    fn a_plain_login_reaches_a_shell_and_commands_report_their_exit_codes() {
        let sink = step_sink();
        let step = Step::start(&sink, "test");
        let mut console = session("s3cr3t-pw", false);
        let active = login(
            &mut console,
            "debian",
            "s3cr3t-pw",
            Duration::from_secs(10),
            &step,
        )
        .unwrap();
        assert_eq!(active, "s3cr3t-pw", "no forced change, no new password");
        let (rc, out) = run_cmd(
            &mut console,
            "systemctl is-system-running --wait",
            Duration::from_secs(10),
            &step,
        )
        .unwrap();
        assert_eq!(rc, 0);
        assert_eq!(out.trim(), "running");
        // The echoed command's literal `B2D-RC-$?` must not satisfy the
        // exit-code scan — only the digit form does.
        let (rc, out) = run_cmd(
            &mut console,
            "stat -c %Y /var/lib/boot2deb/first-boot.done",
            Duration::from_secs(10),
            &step,
        )
        .unwrap();
        assert_eq!((rc, out.trim()), (0, "1755640000"));
        // A command line echoed twice leaves neither echo in the output.
        let (rc, out) = run_cmd(
            &mut console,
            "systemctl --failed --no-legend --plain --no-pager",
            Duration::from_secs(10),
            &step,
        )
        .unwrap();
        assert_eq!(
            (rc, out.trim()),
            (0, "unit.service loaded failed failed A unit")
        );
    }

    #[test]
    fn a_forced_password_change_is_handled_and_the_new_password_reported() {
        let sink = step_sink();
        let step = Step::start(&sink, "test");
        let mut console = session("gen-pw-123", true);
        let active = login(
            &mut console,
            "debian",
            "gen-pw-123",
            Duration::from_secs(10),
            &step,
        )
        .unwrap();
        assert_eq!(
            active,
            changed_password("gen-pw-123"),
            "the driver must report the password it set, or a kept disk is locked out"
        );
        // The shell still works after the relogin.
        let (rc, _) = run_cmd(&mut console, "sync", Duration::from_secs(10), &step).unwrap();
        assert_eq!(rc, 0);
    }

    #[test]
    fn a_wrong_password_is_a_named_authentication_failure() {
        let sink = step_sink();
        let step = Step::start(&sink, "test");
        let mut console = session("right-pw", false);
        let e = login(
            &mut console,
            "debian",
            "wrong-pw",
            Duration::from_secs(10),
            &step,
        )
        .unwrap_err()
        .to_string();
        assert!(e.contains("generated password"), "{e}");
    }

    #[test]
    fn the_full_assertion_pass_runs_over_a_scripted_guest() {
        // assert_booted end to end: settle, stamp, selftest — everything the
        // real boot runs between login and poweroff, minus QEMU. Both sudo policies
        // reach the root selftest; the prompting one after a forced password change,
        // so the password sudo is handed is the changed one.
        for (sudo, expired) in [(SudoPolicy::Nopasswd, false), (SudoPolicy::Password, true)] {
            let sink = step_sink();
            let step = Step::start(&sink, "test");
            let mut console = session("pw", expired);
            let active =
                login(&mut console, "debian", "pw", Duration::from_secs(10), &step).unwrap();

            let build = boot2deb_core::resolve_recipe(
                &crate::test_support::repo_root(),
                "turing-rk1/forky",
                &boot2deb_core::Overrides {
                    sudo: Some(sudo),
                    ..Default::default()
                },
            )
            .unwrap();
            let image = build
                .image
                .as_ref()
                .expect("the fixture recipe builds an image");
            let report =
                assert_booted(&mut console, image, &active, Duration::from_secs(10), &step)
                    .unwrap();
            assert_eq!(report.state, "running", "{sudo:?}");
            assert_eq!(report.stamp, "1755640000", "{sudo:?}");
            assert!(
                report.selftest.contains("9 ok"),
                "{sudo:?}: {}",
                report.selftest
            );
        }
    }

    #[test]
    fn a_prompting_sudo_is_handed_the_password_in_force() {
        // A stale password is a named selftest failure, not a hang at sudo's prompt.
        let sink = step_sink();
        let step = Step::start(&sink, "test");
        let mut console = session("pw", true);
        login(&mut console, "debian", "pw", Duration::from_secs(10), &step).unwrap();
        let build = boot2deb_core::resolve_recipe(
            &crate::test_support::repo_root(),
            "turing-rk1/forky",
            &boot2deb_core::Overrides {
                sudo: Some(SudoPolicy::Password),
                ..Default::default()
            },
        )
        .unwrap();
        let image = build.image.as_ref().unwrap();
        let Err(e) = assert_booted(&mut console, image, "pw", Duration::from_secs(10), &step)
        else {
            panic!("the selftest passed on a password that is no longer in force");
        };
        let e = e.to_string();
        assert!(e.contains("incorrect password"), "{e}");
    }

    #[test]
    fn the_password_reaches_sudo_as_one_shell_word() {
        assert_eq!(
            selftest_command(SudoPolicy::Password, "b2d.0123abcd"),
            "printf '%s\\n' 'b2d.0123abcd' | sudo -S -p '' /usr/lib/boot2deb/selftest --mode userland"
        );
        // A quote in the password closes and reopens the quoting around an escaped one.
        assert!(selftest_command(SudoPolicy::Password, "a'b").contains(r"'a'\''b'"));
        assert_eq!(
            selftest_command(SudoPolicy::Nopasswd, "ignored"),
            "sudo -n /usr/lib/boot2deb/selftest --mode userland"
        );
    }

    /// The guest is powered off as root under either policy. The account's own
    /// `systemctl poweroff` is refused by logind on an image with no polkit, and the
    /// harness then waits out its timeout and kills the guest.
    #[test]
    fn the_guest_is_powered_off_as_root_under_either_policy() {
        assert_eq!(
            poweroff_command(SudoPolicy::Password, "b2d.0123abcd"),
            "printf '%s\\n' 'b2d.0123abcd' | sudo -S -p '' poweroff"
        );
        assert_eq!(
            poweroff_command(SudoPolicy::Nopasswd, "ignored"),
            "sudo -n poweroff"
        );
    }

    #[test]
    fn the_changed_password_is_unlike_the_original() {
        // pam_unix's obscure checks reject similar/reversed/case-changed
        // values; unrelated-by-construction is the property this pins.
        let new = changed_password("Abc123xy");
        assert!(new.starts_with("b2d."));
        assert!(!new.contains("Abc123xy"));
        assert_ne!(changed_password("other"), new);
    }
}
