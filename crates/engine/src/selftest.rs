//! The on-image selftest: what the engine knows about the runner the base
//! overlay ships.
//!
//! The runner itself is POSIX sh (`base/overlay/usr/lib/boot2deb/selftest`) —
//! it executes on the device, where no Rust of ours runs. This module holds the
//! paths the engine and `boot2deb try` address it by, and the harness tests
//! that drive every check kind through a fixture root, so the shell's contract
//! is pinned by `cargo test` like any other stage's.
//!
//! The split of responsibilities is the reason the runner can stay parser-free:
//! `core::expect` validates and renders the checks at build time, the rootfs
//! stage writes them into `/etc/boot2deb/selftest.d/`, and the runner only
//! reads lines and looks at the system. See the manual's self-test reference
//! for the check-kind semantics.

/// Where the runner lives on the image (and in the base overlay). `boot2deb
/// try` invokes this path in the guest rather than the `boot2deb-selftest`
/// wrapper so a broken `/usr/bin` symlink farm cannot mask a broken image.
pub const RUNNER_IMAGE_PATH: &str = "/usr/lib/boot2deb/selftest";

/// The PATH-visible wrapper an operator types on the board.
pub const WRAPPER_IMAGE_PATH: &str = "/usr/bin/boot2deb-selftest";

/// Image-relative directory the generated `.checks` files live in, re-exported
/// beside the paths above so a caller addressing the selftest needs one import.
pub const CHECKS_DIR: &str = boot2deb_core::expect::CHECKS_DIR;

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::process::Command;

    /// The shipped runner, straight from the base overlay in this checkout —
    /// the tests drive the exact bytes an image will carry.
    fn runner() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap()
            .join("base/overlay")
            .join(super::RUNNER_IMAGE_PATH.trim_start_matches('/'))
    }

    /// Run the runner against `root` with extra args; return (exit code, stdout).
    fn run(root: &Path, args: &[&str]) -> (i32, String) {
        let out = Command::new("sh")
            .arg(runner())
            .arg("--root")
            .arg(root)
            .args(args)
            .output()
            .expect("sh is a build prerequisite");
        (
            out.status.code().unwrap_or(-1),
            String::from_utf8_lossy(&out.stdout).into_owned(),
        )
    }

    fn write(root: &Path, rel: &str, content: &str) {
        let path = root.join(rel);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, content).unwrap();
    }

    /// A fixture root shaped like a booted RK1-ish image: kernel + initrd +
    /// extlinux + dtb on /boot, a bound GPU driver in /sys, a render node, a
    /// sound card, firmware, and a builtin module list.
    fn fixture() -> tempfile::TempDir {
        let tmp = tempfile::tempdir().unwrap();
        let r = tmp.path();
        write(r, "boot/vmlinuz-7.1.6-1-arm64", "");
        write(r, "boot/initrd.img-7.1.6-1-arm64", "");
        write(r, "boot/extlinux/extlinux.conf", "label l\n");
        write(r, "boot/rk3588-turing-rk1.dtb-7.1.6-1-arm64", "");
        write(r, "sys/bus/platform/drivers/panthor/fb000000.gpu", "");
        write(r, "dev/dri/renderD128", "");
        write(r, "proc/asound/cards", " 0 [Analog]: H96 Analog\n");
        write(r, "usr/lib/firmware/arm/mali/arch10.8/mali_csffw.bin", "");
        // The builtin list covers the initramfs-module pass case without
        // depending on a host lsinitramfs; note the `-`/`_` normalization
        // (the check asks for dw_mmc-rockchip, the list spells it the same
        // way the kernel tree does).
        write(
            r,
            "lib/modules/7.1.6-1-arm64/modules.builtin",
            "kernel/drivers/mmc/host/dw_mmc-rockchip.ko\n",
        );
        write(
            r,
            "etc/boot2deb/image.toml",
            "device = \"turing-rk1\"\nsuite = \"forky\"\nid = \"rk3588-mainline-7.1\"\n",
        );
        tmp
    }

    fn checks(root: &Path, name: &str, lines: &str) {
        write(
            root,
            &format!("etc/boot2deb/selftest.d/{name}.checks"),
            lines,
        );
    }

    const UNAME: &[&str] = &["--uname", "7.1.6-1-arm64"];

    /// Every check kind's pass case, in one run: the fixture satisfies all of
    /// them and the runner exits 0 with each reported `ok`.
    #[test]
    fn every_check_kind_passes_on_a_satisfying_root() {
        if !crate::hosttool::require(&["sh"]) {
            return;
        }
        let tmp = fixture();
        let dmesg = tmp.path().join("dmesg.txt");
        std::fs::write(&dmesg, "usb 1-1: new high-speed USB device\n").unwrap();
        checks(
            tmp.path(),
            "identity",
            "kernel-release    7.1.6\nkernel-flavor     arm64\ndtb               rockchip/rk3588-turing-rk1.dtb\n",
        );
        checks(
            tmp.path(),
            "soc-rk3588",
            "file              /boot/vmlinuz-*\n\
             file              /boot/extlinux/extlinux.conf\n\
             firmware          arm/mali/arch10.8/mali_csffw.bin\n\
             initramfs-module  dw_mmc_rockchip\n\
             driver-bound      fb000000.gpu panthor\n\
             devnode           /dev/dri/renderD128\n\
             sound-card        H96 Analog\n\
             no-dmesg-match    SError|Synchronous External Abort\n",
        );
        let (code, out) = run(
            tmp.path(),
            &[UNAME, &["--dmesg-file", dmesg.to_str().unwrap()]].concat(),
        );
        assert_eq!(code, 0, "{out}");
        assert!(
            out.contains("turing-rk1 / forky / rk3588-mainline-7.1"),
            "{out}"
        );
        assert_eq!(out.matches("\n  ok      ").count(), 11, "{out}");
        assert!(out.contains("11 ok."), "{out}");
    }

    /// The one-kernel invariant: the fixture carries one and passes; a second
    /// kernel and a kernel whose module tree is for another version both fail,
    /// each naming the versions it found rather than just the count.
    #[test]
    fn single_kernel_passes_on_one_and_names_what_it_finds_otherwise() {
        if !crate::hosttool::require(&["sh"]) {
            return;
        }
        let line = "single-kernel\n";

        let tmp = fixture();
        checks(tmp.path(), "identity", line);
        let (code, out) = run(tmp.path(), UNAME);
        assert_eq!(code, 0, "{out}");
        assert!(
            out.contains("ok      single-kernel     7.1.6-1-arm64"),
            "{out}"
        );

        // A second kernel: what a mid-build dist-upgrade or a --deb addition
        // would leave behind. Both versions are named, because which one arrived
        // is the question a failure sends you to answer.
        let tmp = fixture();
        checks(tmp.path(), "identity", line);
        write(tmp.path(), "boot/vmlinuz-7.1.7-1-arm64", "");
        let (code, out) = run(tmp.path(), UNAME);
        assert_eq!(code, 1, "{out}");
        assert!(out.contains("2 kernels on /boot"), "{out}");
        assert!(out.contains("7.1.6-1-arm64 7.1.7-1-arm64"), "{out}");

        // One kernel, but its modules are for another — the shape a half-swapped
        // kernel leaves, which a bare count would call healthy.
        let tmp = fixture();
        checks(tmp.path(), "identity", line);
        std::fs::rename(
            tmp.path().join("lib/modules/7.1.6-1-arm64"),
            tmp.path().join("lib/modules/7.1.7-1-arm64"),
        )
        .unwrap();
        let (code, out) = run(tmp.path(), UNAME);
        assert_eq!(code, 1, "{out}");
        assert!(out.contains("its modules are for 7.1.7-1-arm64"), "{out}");

        // A merged-usr root: /lib/modules and /usr/lib/modules are one tree, and
        // counting both would call every image two-kernelled.
        let tmp = fixture();
        checks(tmp.path(), "identity", line);
        write(
            tmp.path(),
            "usr/lib/modules/7.1.6-1-arm64/modules.builtin",
            "",
        );
        let (code, out) = run(tmp.path(), UNAME);
        assert_eq!(code, 0, "{out}");
    }

    /// Every check kind's fail case: nothing in the empty root satisfies them,
    /// and each failure carries a reason. One run, exit 1.
    #[test]
    fn every_check_kind_fails_on_an_empty_root() {
        if !crate::hosttool::require(&["sh"]) {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let dmesg = tmp.path().join("dmesg.txt");
        std::fs::write(&dmesg, "SError Interrupt on CPU6\n").unwrap();
        // An empty /boot means the disk-content checks name what is absent; a
        // wrong uname fails the identity pair; the dmesg fixture carries the
        // pattern that must not be there.
        checks(
            tmp.path(),
            "all",
            "kernel-release    7.1.6\n\
             kernel-flavor     arm64\n\
             file              /boot/vmlinuz-*\n\
             dtb               rockchip/rk3588-turing-rk1.dtb\n\
             firmware          arm/mali/arch10.8/mali_csffw.bin\n\
             initramfs-module  dw_mmc_rockchip\n\
             driver-bound      fb000000.gpu panthor\n\
             devnode           /dev/dri/renderD128\n\
             sound-card        H96 Analog\n\
             no-dmesg-match    SError\n",
        );
        let (code, out) = run(
            tmp.path(),
            &[
                "--uname",
                "7.1.7-1-rt-arm64",
                "--dmesg-file",
                dmesg.to_str().unwrap(),
            ],
        );
        assert_eq!(code, 1, "{out}");
        assert_eq!(out.matches("\n  FAILED  ").count(), 10, "{out}");
        assert!(out.contains("running 7.1.7-1-rt-arm64"), "{out}");
        assert!(out.contains("running an -rt kernel"), "{out}");
        assert!(out.contains("pattern matched the kernel log"), "{out}");
        assert!(
            out.contains("expected to have and does not"),
            "the summary says what a failure means:\n{out}"
        );
    }

    /// The PREEMPT_RT trap specifically: `-rt-arm64` ends in `-arm64`, so a
    /// naive suffix match would pass it. It must fail the flavor check while
    /// the same uname still passes its release check.
    #[test]
    fn an_rt_kernel_fails_the_flavor_check_but_not_the_release_check() {
        if !crate::hosttool::require(&["sh"]) {
            return;
        }
        let tmp = fixture();
        checks(
            tmp.path(),
            "identity",
            "kernel-release    7.1.6\nkernel-flavor     arm64\n",
        );
        let (code, out) = run(tmp.path(), &["--uname", "7.1.6-1-rt-arm64"]);
        assert_eq!(code, 1, "{out}");
        assert!(out.contains("  ok      kernel-release"), "{out}");
        assert!(out.contains("  FAILED  kernel-flavor"), "{out}");
    }

    /// Userland mode (`boot2deb try` under `-M virt`): the hardware checks are
    /// not-applicable rather than failed, the running kernel is a fixture so
    /// the kernel checks look at /boot instead of uname, and the disk-content
    /// checks still run — an empty /boot would still fail.
    #[test]
    fn userland_mode_marks_hardware_checks_not_applicable() {
        if !crate::hosttool::require(&["sh"]) {
            return;
        }
        let tmp = fixture();
        checks(
            tmp.path(),
            "all",
            "kernel-release    7.1.6\n\
             kernel-flavor     arm64\n\
             file              /boot/vmlinuz-*\n\
             firmware          arm/mali/arch10.8/mali_csffw.bin\n\
             initramfs-module  dw_mmc-rockchip\n\
             driver-bound      fb000000.gpu panthor\n\
             devnode           /dev/dri/renderD128\n\
             sound-card        H96 Analog\n\
             no-dmesg-match    SError\n",
        );
        // The emulator's uname: nothing like the board kernel. Must not fail
        // anything in this mode.
        let (code, out) = run(
            tmp.path(),
            &["--mode", "userland", "--uname", "6.12.0-virt"],
        );
        assert_eq!(code, 0, "{out}");
        assert_eq!(out.matches("\n  n/a     ").count(), 4, "{out}");
        assert_eq!(out.matches("\n  ok      ").count(), 5, "{out}");
        assert!(out.contains("4 not applicable"), "{out}");
    }

    /// An unknown check kind — a config tree newer than the boot2deb that
    /// built the image — is reported skipped, never failed.
    #[test]
    fn an_unknown_kind_skips_and_a_skip_does_not_fail_the_run() {
        if !crate::hosttool::require(&["sh"]) {
            return;
        }
        let tmp = fixture();
        checks(
            tmp.path(),
            "device-future",
            "gpio-line          gpiochip0 17 high\nfile              /boot/vmlinuz-*\n",
        );
        let (code, out) = run(tmp.path(), UNAME);
        assert_eq!(code, 0, "{out}");
        assert!(out.contains("  skipped gpio-line"), "{out}");
        assert!(out.contains("unknown check kind"), "{out}");
        assert!(out.contains("1 skipped."), "{out}");
    }

    /// No installed checks is its own loud outcome (exit 2), distinct from
    /// both success and failure — an image missing the whole directory was
    /// built wrong, not validated.
    #[test]
    fn a_root_with_no_checks_exits_2() {
        if !crate::hosttool::require(&["sh"]) {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let (code, _) = run(tmp.path(), UNAME);
        assert_eq!(code, 2);
    }

    /// The shipped wrapper and runner carry the executable bit in the
    /// checkout, like the first-boot hooks — the guard against shipping a
    /// selftest nothing can run.
    #[test]
    fn the_shipped_runner_and_wrapper_are_executable() {
        use std::os::unix::fs::PermissionsExt;
        for rel in [super::RUNNER_IMAGE_PATH, super::WRAPPER_IMAGE_PATH] {
            let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(2)
                .unwrap()
                .join("base/overlay")
                .join(rel.trim_start_matches('/'));
            let mode = path
                .metadata()
                .unwrap_or_else(|_| panic!("{} is shipped", path.display()))
                .permissions()
                .mode();
            assert!(mode & 0o111 != 0, "{} must be executable", path.display());
        }
    }
}
