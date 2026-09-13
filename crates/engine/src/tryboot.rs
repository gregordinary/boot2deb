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
//! The board kernel is deliberately not booted. It is configured from the board's
//! fragments and has no reason to carry virtio drivers. Adding them would change the
//! shipped kernel to serve the test. The guest instead boots the suite's own generic
//! kernel ([`fixture_kernel`]) — the kernel is a fixture, the userland is what is
//! under test. No bootloader is in the loop either: QEMU loads the kernel directly,
//! because `try` is not testing the boot path.
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

use crate::error::EngineError;
use crate::event::{EventSink, Step, Stream};
use crate::rootfs::{provisioned_dir, sweep_provisioned, ProvisionedRoot};
use crate::sandbox::{forward_bootstrap_event, SandboxRun};
use boot2deb_core::model::{Arch, ResolvedBuild, ResolvedImage, SudoPolicy};
use ferroday_cage::provision;
use ferroday_cage::provision::debian::{Debian, DebianEvent};
use ferroday_cage::IdentityMap;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Names the fixture harvest: the step a `try` run reports it under, and the label
/// its transient root carries in the work dir.
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

/// The suite's generic kernel this architecture boots under `-M virt` — the
/// fixture. A distro-package kernel build coincides with it, which `try`'s
/// report says out loud.
fn fixture_package(arch: Arch) -> Result<&'static str, EngineError> {
    match arch {
        Arch::Arm64 => Ok("linux-image-arm64"),
        Arch::Armv7 => Ok("linux-image-armmp"),
        other => Err(EngineError::TryBoot {
            context: "select the fixture kernel".into(),
            message: format!(
                "no generic Debian kernel is wired up for {other} guests yet; \
                 try covers arm64 and armv7"
            ),
        }),
    }
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

/// The fixture kernel pair a guest boots with: the suite's generic kernel, and an
/// initramfs that can find a virtio root. Both are harvested from the archive's own
/// `.deb` ([`fixture_kernel`]).
pub struct FixtureKernel {
    /// The kernel image (`vmlinuz`).
    pub kernel: PathBuf,
    /// The matching initramfs, built by `initramfs-tools` inside a target-arch
    /// root at harvest time, with its `MODULES=most` default. That default is what
    /// puts the virtio drivers in it.
    pub initrd: PathBuf,
}

/// What the fixture root is provisioned from: a suite, an architecture, an archive to
/// resolve it against, and the two directories a transient tree needs.
///
/// `try` stands this root up for itself rather than borrowing the build's.
///
/// A `systemd` postinst cannot be configured under the **single-identity** map, which
/// is the map every [`BuildSandbox`](crate::sandbox::BuildSandbox) root carries. The
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
    /// The guest architecture, which names both the kernel package and the emulator.
    pub arch: Arch,
    /// The Debian suite the fixture kernel comes from — the image's, so the guest
    /// boots the generic kernel of the release it runs.
    pub suite: &'a str,
    /// Ordered mirror list, primary first. Non-empty.
    pub mirrors: &'a [String],
    /// Debian archive keyring verifying the suite's `Release` signature. `None` falls
    /// back to the host apt trust store, as everywhere else.
    pub keyring: Option<&'a Path>,
    /// Where downloaded `.deb`s are cached, shared with the build's own provisioner
    /// runs so a `try` after a `build` refetches nothing it already holds.
    pub deb_cache: &'a Path,
    /// The work dir the transient root is provisioned under. Not `TMPDIR`: the tree is
    /// a Debian userland with real ownership, and `clean` reclaims it from here.
    pub scratch_dir: &'a Path,
}

/// Obtain the fixture kernel for `spec`, cached under `dir`.
///
/// The suite's `linux-image-*` metapackage and `initramfs-tools` are installed into a
/// freshly provisioned target-arch root. The kernel's own postinst therefore builds the
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
    if !refresh && kernel.is_file() && initrd.is_file() {
        step.log(format!(
            "reusing the fixture kernel at {} (--refresh-fixture re-harvests)",
            dir.display()
        ));
        return Ok(FixtureKernel { kernel, initrd });
    }
    std::fs::create_dir_all(dir).map_err(|s| EngineError::io(dir, s))?;
    let package = fixture_package(spec.arch)?;
    let arch = spec.arch.debian_arch();
    step.log(format!(
        "harvesting the fixture kernel: {package} + initramfs-tools in a {arch} {} root",
        spec.suite
    ));

    std::fs::create_dir_all(spec.scratch_dir).map_err(|s| EngineError::io(spec.scratch_dir, s))?;
    sweep_provisioned(spec.scratch_dir);
    let root = provisioned_dir(spec.scratch_dir, "fixture");
    let mut debian = fixture_provisioner(spec, package)?;
    let mut sink = |event: DebianEvent<'_>| forward_bootstrap_event(step, event);
    provision::ensure(&root, &mut debian.observe(&mut sink)).map_err(|source| {
        EngineError::Bootstrap {
            context: format!("provision the {arch} {} fixture root", spec.suite),
            message: source.to_string(),
        }
    })?;
    let _provisioned = ProvisionedRoot::new(root.clone());

    // One kernel exists in a fresh root, so the globs are unambiguous. The copy runs
    // inside the root because `initramfs-tools` writes its initrd `0600 root:root`,
    // which the calling user cannot read from outside; inside the subordinate map the
    // command is that root, and `dir` is bound at its own path so the pair lands on the
    // host. The map's inside-root is the calling user, so the harvested pair belongs to
    // the caller rather than to a subuid.
    let script = format!(
        "set -e; cp /boot/vmlinuz-* '{dir}/vmlinuz'; cp /boot/initrd.img-* '{dir}/initrd.img'; \
         chmod 0644 '{dir}/vmlinuz' '{dir}/initrd.img'",
        dir = dir.display()
    );
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
    Ok(FixtureKernel { kernel, initrd })
}

/// The provisioner the fixture root is bootstrapped with: `spec`'s archive, the
/// subordinate identity map, and the kernel packages as the only includes over a
/// `required`-priority base.
///
/// `busybox` is named outright because `initramfs-tools` only Recommends it, and
/// Recommends are never installed — an initramfs built without it has no rescue shell.
/// The base stays at the library's `required` floor: this root exists to run one
/// postinst, not to be a userland anyone works in.
fn fixture_provisioner(spec: &FixtureSpec, package: &str) -> Result<Debian<'static>, EngineError> {
    let (primary, fallbacks) = spec
        .mirrors
        .split_first()
        .ok_or_else(|| EngineError::TryBoot {
            context: "provision the fixture root".into(),
            message: "no mirror to resolve the fixture kernel from".into(),
        })?;
    let mut b = Debian::builder(spec.suite)
        .architecture(spec.arch.debian_arch())
        .components(crate::bootstrap::COMPONENTS.split(','))
        .identity_map(IdentityMap::Subordinate)
        .cache_dir(spec.deb_cache)
        .mirror(primary)
        .include([package, "initramfs-tools", "busybox"]);
    for fallback in fallbacks {
        b = b.mirror_fallback(fallback);
    }
    // A point-in-time archive's release is expired by design, as everywhere else.
    if crate::snapshot::has_snapshot(spec.mirrors) {
        b = b.allow_stale_release(true);
    }
    if let Some(keyring) = spec.keyring {
        b = b.keyring(keyring);
    }
    b.build().map_err(|source| EngineError::Bootstrap {
        context: format!(
            "configure the {} {} fixture bootstrap",
            spec.arch.debian_arch(),
            spec.suite
        ),
        message: source.to_string(),
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
    /// The generated first-boot password, from the build's provenance manifest.
    pub password: &'a str,
    /// How long one boot is allowed to take to reach a login prompt (and how long
    /// `systemctl is-system-running --wait` is allowed to take after it). Under TCG
    /// this is minutes.
    pub boot_timeout: Duration,
    /// Keep the disk copy after the run — for a post-mortem, or to boot it by
    /// hand. Note the first login was forced to change the account password. The
    /// report carries the one that is now set.
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

/// The whole run: both boots passed everything they assert.
pub struct TryReport {
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
/// masking it here keeps that a property of the try boot, not of the image.
/// `panic=-1` plus QEMU's `-no-reboot` turns a kernel panic into a prompt QEMU
/// exit instead of a hung run.
fn append_line(rootfs_partuuid: &str) -> String {
    format!(
        "root=PARTUUID={rootfs_partuuid} rw rootwait console=ttyAMA0 panic=-1 \
         systemd.mask=systemd-modules-load.service"
    )
}

/// The QEMU invocation for one boot. Pure so the shape is testable; `kvm` is a
/// host fact the caller supplies ([`kvm_usable`]).
fn qemu_argv(
    arch: Arch,
    fixture: &FixtureKernel,
    disk: &Path,
    rootfs_partuuid: &str,
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
        append_line(rootfs_partuuid),
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
/// no assumption about which index the rootfs landed at.
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

/// Reduces the guest's console bytes to the text they carry: control sequences
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
    /// Feed `chunk`; return the printable text it carried.
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
/// follow — the echo carries the unexpanded `$?`, which fails the digit check.
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
            let output = match output.find("B2D-RC-$?") {
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

/// One boot's assertions, already logged in: system state, first-boot stamp,
/// and the selftest.
fn assert_booted(
    console: &mut Console,
    image: &ResolvedImage,
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
        let (_, failed) = run_cmd(
            console,
            "systemctl --failed --no-legend --plain",
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
    // The selftest, in the mode built for a guest that is not the board. Root
    // where sudo is free (dmesg and the initramfs are then readable); as the
    // user otherwise — the runner reports what it had to skip.
    let selftest_cmd = match image.sudo {
        SudoPolicy::Nopasswd => "sudo -n /usr/lib/boot2deb/selftest --mode userland",
        SudoPolicy::Password => "/usr/lib/boot2deb/selftest --mode userland",
    };
    let (rc, selftest_out) = run_cmd(console, selftest_cmd, COMMAND_TIMEOUT, step)?;
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
        rootfs_partuuid,
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
        let report = assert_booted(&mut console, opts.resolved_image, opts.boot_timeout, step)?;
        // Sync before poweroff so the stamp and journal survive even a shutdown
        // the harness ends up killing.
        let _ = run_cmd(&mut console, "sync", COMMAND_TIMEOUT, step)?;
        let poweroff = match opts.resolved_image.sudo {
            SudoPolicy::Nopasswd => "sudo -n poweroff",
            SudoPolicy::Password => "systemctl poweroff",
        };
        console.send_line(poweroff)?;
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
             the report carries it)",
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
    /// carry the partial one across chunks. One byte at a time is the worst case.
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

    /// A spec standing in for a `try` run's, carrying only what the provisioner reads.
    fn fixture_spec<'a>(mirrors: &'a [String], dirs: &'a Path) -> FixtureSpec<'a> {
        FixtureSpec {
            arch: Arch::Arm64,
            suite: "forky",
            mirrors,
            keyring: None,
            deb_cache: dirs,
            scratch_dir: dirs,
        }
    }

    /// The identity map is the whole reason this root exists rather than a build root,
    /// so it is the one setting worth asserting. The rest is what makes the root
    /// resolve the image's own kernel: its suite, its architecture, and the packages
    /// whose postinst builds the initramfs.
    #[test]
    fn the_fixture_root_is_provisioned_under_the_subordinate_map() {
        let mirrors = vec!["http://deb.debian.org/debian".to_string()];
        let tmp = tempfile::tempdir().unwrap();
        let spec = fixture_spec(&mirrors, tmp.path());
        let rendered = format!(
            "{:?}",
            fixture_provisioner(&spec, fixture_package(Arch::Arm64).unwrap()).unwrap()
        );
        assert!(
            rendered.contains("identity_map: Subordinate"),
            "the fixture root was configured under some other map: {rendered}"
        );
        assert!(rendered.contains(r#"suite: "forky""#), "{rendered}");
        assert!(rendered.contains(r#"architecture: "arm64""#), "{rendered}");
        for package in ["linux-image-arm64", "initramfs-tools", "busybox"] {
            assert!(rendered.contains(package), "{package} missing: {rendered}");
        }
        // Nothing layers this root, so it carries no base to increment.
        assert!(rendered.contains("base_layer: None"), "{rendered}");
    }

    /// An empty mirror list is refused where it is read, rather than reaching the
    /// provisioner as a bootstrap with nowhere to resolve from.
    #[test]
    fn a_fixture_root_with_no_mirror_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let err = fixture_provisioner(&fixture_spec(&[], tmp.path()), "linux-image-arm64")
            .expect_err("a fixture root resolved from nothing");
        assert!(err.to_string().contains("no mirror"), "{err}");
    }

    #[test]
    fn the_qemu_invocation_names_the_machine_the_root_and_the_mask() {
        let fixture = FixtureKernel {
            kernel: PathBuf::from("/w/try/vmlinuz"),
            initrd: PathBuf::from("/w/try/initrd.img"),
        };
        let argv = qemu_argv(
            Arch::Arm64,
            &fixture,
            Path::new("/w/try/disk.img"),
            "0000-1111",
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
        assert!(fixture_package(Arch::Riscv64).is_err());
        assert_eq!(fixture_package(Arch::Armv7).unwrap(), "linux-image-armmp");
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
            // Coloured status lines and the shell's own markers, as a forky guest
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
            }
            send("Linux testhost 6.12.0 aarch64\r\ndebian@testhost:~$ ");
            // The shell: echo each command line, answer the sentinel probes.
            loop {
                let line = read_line();
                if line.is_empty() {
                    break;
                }
                // tty echo, then the marker the shell writes before the command's
                // own output — which lands between the newline and the first
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
                        "sudo -n /usr/lib/boot2deb/selftest --mode userland" => {
                            send(&format!("identity\r\n  ok      kernel-release    7.1.6\r\n\r\n9 ok, 4 not applicable.\r\nB2D-RC-0\r\n{OSC_END}B2D> "))
                        }
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
        // real boot runs between login and poweroff, minus QEMU.
        let sink = step_sink();
        let step = Step::start(&sink, "test");
        let mut console = session("pw", false);
        login(&mut console, "debian", "pw", Duration::from_secs(10), &step).unwrap();

        let build = boot2deb_core::resolve_recipe(
            &crate::test_support::repo_root(),
            "turing-rk1/forky",
            &boot2deb_core::Overrides::default(),
        )
        .unwrap();
        let image = build
            .image
            .as_ref()
            .expect("the fixture recipe builds an image");
        let report = assert_booted(&mut console, image, Duration::from_secs(10), &step).unwrap();
        assert_eq!(report.state, "running");
        assert_eq!(report.stamp, "1755640000");
        assert!(report.selftest.contains("9 ok"), "{}", report.selftest);
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
