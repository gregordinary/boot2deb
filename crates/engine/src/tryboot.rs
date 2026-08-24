//! `boot2deb try` — boot the built image under QEMU system emulation and assert
//! the userland works, before anything is flashed.
//!
//! This tests the **userland**, not the board. The guest machine is `-M virt`,
//! so no board device exists in it; what it proves is that systemd reaches
//! `multi-user.target` with no failed unit, that the generated first-boot
//! password authenticates, that `first-boot` ran to completion — and, because
//! it boots the same disk **twice**, that the image survives its second boot,
//! the failure class no single-boot smoke test finds. The on-image selftest
//! runs inside the guest in userland mode, so the disk-content half of the
//! board's expectations is checked too.
//!
//! The board kernel is deliberately not booted: it is configured from the
//! board's fragments and has no reason to carry virtio drivers, and adding them
//! would change the shipped kernel to serve the test. The guest instead boots
//! the suite's own generic kernel ([`fixture_kernel`]) — the kernel is a
//! fixture, the userland is what is under test. No bootloader is in the loop
//! either: QEMU loads the kernel directly, because `try` is not testing the
//! boot path.
//!
//! Everything in the guest is driven over the serial console: `try` logs in as
//! the image's account with the password the build generated (handling the
//! forced first-login change), runs the assertions as shell commands, and
//! powers the guest off. Driving the real login path is the point — it is the
//! assertion that the account works, not a side channel around it.
//!
//! Runtime is minutes, not seconds, under TCG on an x86 host: this replaces a
//! flash-plus-serial-console cycle, not a unit test. With KVM on a matching
//! host it is fast, and `try` uses KVM when `/dev/kvm` is usable.

use crate::error::EngineError;
use crate::event::{EventSink, Step, Stream};
use crate::sandbox::{BuildRootSpec, BuildSandbox, SandboxRun};
use boot2deb_core::model::{Arch, ResolvedBuild, ResolvedImage, SudoPolicy};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Where a recipe's fixture kernel is cached under its work dir, and the
/// build-root stage name the harvest layers under.
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

/// The fixture kernel pair a guest boots with: the suite's generic kernel and
/// an initramfs that can find a virtio root, both harvested from the archive's
/// own `.deb` ([`fixture_kernel`]).
pub struct FixtureKernel {
    /// The kernel image (`vmlinuz`).
    pub kernel: PathBuf,
    /// The matching initramfs, built by `initramfs-tools` inside a target-arch
    /// root at harvest time, with its `MODULES=most` default — which is what
    /// puts the virtio drivers in it.
    pub initrd: PathBuf,
}

/// Obtain the fixture kernel for `arch`, cached under `dir`.
///
/// The suite's `linux-image-*` metapackage and `initramfs-tools` are layered
/// over the target-arch build sandbox's base ([`BuildSandbox::build_root`]), so
/// the kernel's own postinst builds the initramfs inside a real userland of the
/// pinned suite — the same machinery every package stage uses, and the reason
/// this needs no host `dpkg`. The pair is copied out and reused on later runs;
/// `refresh` discards the cached pair and harvests again (how a new point
/// release of the suite kernel is picked up).
pub fn fixture_kernel(
    sandbox: &dyn BuildSandbox,
    arch: Arch,
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
    let package = fixture_package(arch)?;
    step.log(format!(
        "harvesting the fixture kernel: {package} + initramfs-tools in a {} build root",
        sandbox.describe()
    ));
    sandbox.ensure_ready(step)?;
    let root = sandbox.build_root(
        &BuildRootSpec {
            // busybox is named outright: initramfs-tools only Recommends it,
            // and an initramfs built without it has no rescue shell.
            packages: &[package, "initramfs-tools", "busybox"],
            pool: None,
            stage: FIXTURE_STAGE,
        },
        step,
    )?;
    // One kernel exists in a fresh increment, so the globs are unambiguous.
    let script = format!(
        "set -e; cp /boot/vmlinuz-* '{dir}/vmlinuz'; cp /boot/initrd.img-* '{dir}/initrd.img'; \
         chmod 0644 '{dir}/vmlinuz' '{dir}/initrd.img'",
        dir = dir.display()
    );
    root.run(
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
    /// How long one boot may take to reach a login prompt (and how long
    /// `systemctl is-system-running --wait` may take after it). Under TCG this
    /// is minutes.
    pub boot_timeout: Duration,
    /// Keep the disk copy after the run — for a post-mortem, or to boot it by
    /// hand. Note the first login was forced to change the account password;
    /// the report carries the one that is now set.
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
    /// [`TryOptions::keep_disk`]; the built image is untouched.
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

/// The guest's serial console: bytes in from a reader thread, lines out through
/// a writer, and an accumulated transcript the expect calls scan.
///
/// Generic over the transport so the login and command drivers are tested
/// against a scripted fake guest (a `UnixStream` pair) — the QEMU integration
/// contributes only the transport.
struct Console {
    rx: std::sync::mpsc::Receiver<Vec<u8>>,
    writer: Box<dyn Write + Send>,
    /// The transcript so far, lossy UTF-8.
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
            let line = self.buf[self.line_start..end].trim_end_matches('\r');
            step.relay(Stream::Stdout, line.to_string());
            self.line_start = end + 1;
        }
    }

    /// Pull whatever the guest has produced, without blocking longer than
    /// `wait`.
    fn pump(&mut self, wait: Duration, step: &Step) {
        if let Ok(chunk) = self.rx.recv_timeout(wait) {
            self.buf.push_str(&String::from_utf8_lossy(&chunk));
            // Drain anything else already queued.
            while let Ok(more) = self.rx.try_recv() {
                self.buf.push_str(&String::from_utf8_lossy(&more));
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
            send("[  OK  ] Reached target multi-user.target\r\n");
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
                send(&format!("{line}\r\n")); // tty echo
                if line.starts_with("PS1=") {
                    send("B2D-READY\r\nB2D> ");
                } else if let Some(cmd) = line.strip_suffix("; echo B2D-RC-$?") {
                    match cmd {
                        "systemctl is-system-running --wait" => {
                            send("running\r\nB2D-RC-0\r\nB2D> ")
                        }
                        "stat -c %Y /var/lib/boot2deb/first-boot.done" => {
                            send("1755640000\r\nB2D-RC-0\r\nB2D> ")
                        }
                        "sudo -n /usr/lib/boot2deb/selftest --mode userland" => {
                            send("identity\r\n  ok      kernel-release    7.1.6\r\n\r\n9 ok, 4 not applicable.\r\nB2D-RC-0\r\nB2D> ")
                        }
                        "sync" => send("B2D-RC-0\r\nB2D> "),
                        _ => send("B2D-RC-127\r\nB2D> "),
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
