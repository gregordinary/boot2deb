//! Host tool-presence preflight for `doctor`.
//!
//! Host identity and cross-arch status come from [`crate::preflight`]; this module
//! adds the concrete tool/capability checks the build needs, with per-platform
//! remediation. It reports exactly what is present or missing *before* any build
//! work starts — the same "typed error before any work starts" contract as config
//! validation. What is checked depends on the target and whether the build is
//! cross-arch:
//!
//! - **Always:** host `git` (`git am --3way`), `make` + the target C
//!   toolchain (native `cc`, else the `<triple>gcc` cross toolchain), the kernel's
//!   `bc`/`flex`/`bison`/openssl build deps, `mmdebstrap` (rootfs bootstrap),
//!   and unprivileged user namespaces (the rootless bootstrap + sandbox).
//! - **Cross-arch only** (host arch ≠ target arch): `bwrap` (rootless sandbox
//!   entry), a `qemu-<arch>` interpreter, and a registered+enabled binfmt
//!   handler for the target. Same-arch builds skip these entirely.
//! - **Image path:** `mke2fs` (formats the rootfs ext4 from a userns-staged
//!   tree) and `e2fsck` (the clean-verify gate); the staging itself rides on the
//!   same unprivileged-userns capability the rootless bootstrap needs.
//!
//! Detection is a side effect (PATH scan, `/proc` + `/etc/os-release` reads,
//! `pkg-config` shell-out), so it lives in the engine, not `core`.

use std::path::PathBuf;
use std::process::Command;

use boot2deb_core::host::HostInfo;
use boot2deb_core::model::Arch;

/// One host requirement and whether it is satisfied.
#[derive(Debug, Clone)]
pub struct Check {
    /// Human name of the requirement (a tool name or a capability).
    pub name: String,
    /// What the build needs it for, with a plan section reference.
    pub purpose: &'static str,
    /// A hard requirement (`true`) vs. a fallback-only convenience (`false`).
    /// A missing required check fails preflight; a missing fallback is a note.
    pub required: bool,
    /// The result of probing for it.
    pub status: CheckStatus,
}

/// The outcome of probing for a [`Check`].
#[derive(Debug, Clone)]
pub enum CheckStatus {
    /// Found. Carries a detail — a resolved path, or `"registered (flags …)"`.
    Present(String),
    /// Absent. Carries a host-specific install hint (e.g. `sudo apt install …`).
    Missing(String),
}

impl Check {
    /// True when this check is a required one that was not satisfied.
    pub fn is_blocking(&self) -> bool {
        self.required && matches!(self.status, CheckStatus::Missing(_))
    }
}

/// The host's package manager, used to phrase remediation hints. Detected from
/// `/etc/os-release` (`ID`/`ID_LIKE`) on Linux, or the host OS on macOS.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PkgManager {
    /// Debian/Ubuntu family (`apt`).
    Apt,
    /// Fedora/RHEL family (`dnf`).
    Dnf,
    /// Arch family (`pacman`).
    Pacman,
    /// macOS (`brew`).
    Brew,
    /// Unrecognized host — remediation names the package generically.
    Unknown,
}

impl PkgManager {
    /// Detect the host package manager from `/etc/os-release` / the host OS.
    pub fn detect(host: &HostInfo) -> Self {
        if host.os == "macos" {
            return PkgManager::Brew;
        }
        let os_release = std::fs::read_to_string("/etc/os-release").unwrap_or_default();
        Self::from_os_release(&os_release)
    }

    /// Classify from `/etc/os-release` contents (pure — unit-testable).
    fn from_os_release(contents: &str) -> Self {
        let field = |key: &str| -> String {
            contents
                .lines()
                .find_map(|l| l.strip_prefix(key))
                .map(|v| v.trim_matches(['=', '"', ' ']).to_ascii_lowercase())
                .unwrap_or_default()
        };
        let ids = format!("{} {}", field("ID="), field("ID_LIKE="));
        if ids.contains("debian") || ids.contains("ubuntu") {
            PkgManager::Apt
        } else if ids.contains("fedora") || ids.contains("rhel") || ids.contains("centos") {
            PkgManager::Dnf
        } else if ids.contains("arch") {
            PkgManager::Pacman
        } else {
            PkgManager::Unknown
        }
    }

    /// The install command prefix (e.g. `sudo apt install`).
    fn install_cmd(self) -> &'static str {
        match self {
            PkgManager::Apt => "sudo apt install",
            PkgManager::Dnf => "sudo dnf install",
            PkgManager::Pacman => "sudo pacman -S",
            PkgManager::Brew => "brew install",
            PkgManager::Unknown => "install",
        }
    }

    /// Concrete package name for a canonical [`Pkg`] on this manager.
    fn package(self, pkg: Pkg, target: Arch) -> String {
        use PkgManager::*;
        match pkg {
            // Same name across managers.
            Pkg::Git => "git".into(),
            Pkg::Make => "make".into(),
            Pkg::Bc => "bc".into(),
            Pkg::Flex => "flex".into(),
            Pkg::Bison => "bison".into(),
            Pkg::Mmdebstrap => "mmdebstrap".into(),
            Pkg::E2fsprogs => "e2fsprogs".into(),
            Pkg::Bubblewrap => "bubblewrap".into(),
            Pkg::Coreutils => "coreutils".into(),
            // dpkg / dpkg-dev / apt-utils are Debian's own tools; on non-Debian
            // hosts they come from the distro's dpkg/apt ports (or AUR).
            Pkg::DpkgDev => match self {
                Pacman => "dpkg (AUR)".into(),
                _ => "dpkg-dev".into(),
            },
            Pkg::AptUtils => match self {
                Apt => "apt-utils".into(),
                Pacman => "apt (AUR)".into(),
                _ => "apt".into(),
            },
            Pkg::Dpkg => match self {
                Pacman => "dpkg (AUR)".into(),
                _ => "dpkg".into(),
            },
            Pkg::Openssl => match self {
                Dnf => "openssl-devel".into(),
                Pacman | Brew => "openssl".into(),
                _ => "libssl-dev".into(),
            },
            Pkg::NativeToolchain => match self {
                Apt => "build-essential".into(),
                Dnf => "gcc make".into(),
                Pacman => "base-devel".into(),
                Brew => "gcc".into(),
                Unknown => "gcc".into(),
            },
            Pkg::CrossToolchain => {
                let triple = cross_triple(target);
                match self {
                    Apt => format!("gcc-{triple}"),
                    Dnf => format!("gcc-{}-linux-gnu", target_gnu(target)),
                    Pacman => format!("{triple}-gcc (AUR)"),
                    _ => format!("gcc-{triple}"),
                }
            }
            Pkg::QemuUser => match self {
                Pacman => "qemu-user-static-binfmt (AUR)".into(),
                _ => "qemu-user-static".into(),
            },
        }
    }

    /// A one-line remediation for a missing [`Pkg`].
    fn remedy(self, pkg: Pkg, target: Arch) -> String {
        format!("{} {}", self.install_cmd(), self.package(pkg, target))
    }
}

/// Canonical host packages the build depends on, mapped to per-manager names.
#[derive(Debug, Clone, Copy)]
enum Pkg {
    Git,
    Make,
    Bc,
    Flex,
    Bison,
    Openssl,
    NativeToolchain,
    CrossToolchain,
    Mmdebstrap,
    Bubblewrap,
    QemuUser,
    E2fsprogs,
    /// `dpkg-scanpackages` — the local apt repo's `Packages` index.
    DpkgDev,
    /// `apt-ftparchive` — the local apt repo's `Release`.
    AptUtils,
    /// `dpkg-deb` — host-side `.deb` packaging (u-boot/ffmpeg debs).
    Dpkg,
    /// `sha256sum` — the rootfs `.deb` content-hash capture hook.
    Coreutils,
}

/// GNU triple for a cross toolchain targeting `arch` (the `<triple>gcc` prefix).
fn cross_triple(arch: Arch) -> &'static str {
    match arch {
        Arch::Arm64 => "aarch64-linux-gnu",
        Arch::Armv7 => "arm-linux-gnueabihf",
        Arch::Riscv64 => "riscv64-linux-gnu",
    }
}

/// The arch token Fedora uses in its cross-gcc package names.
fn target_gnu(arch: Arch) -> &'static str {
    match arch {
        Arch::Arm64 => "aarch64",
        Arch::Armv7 => "arm",
        Arch::Riscv64 => "riscv64",
    }
}

/// The qemu-user interpreter arch token for a target (`qemu-<token>`).
fn qemu_arch(arch: Arch) -> &'static str {
    match arch {
        Arch::Arm64 => "aarch64",
        Arch::Armv7 => "arm",
        Arch::Riscv64 => "riscv64",
    }
}

/// Run every host preflight check for building `target` with the given
/// `cross_compile` prefix (e.g. `aarch64-linux-gnu-`), in report order.
pub fn tool_checks(target: Arch, cross_compile: &str) -> Vec<Check> {
    let host = HostInfo::detect();
    let cross = host.is_cross_for(target);
    let pm = PkgManager::detect(&host);
    // Always-required host tooling.
    let mut checks = vec![
        exe(pm, target, "git", &["git"], "git am --3way patch apply", true, Pkg::Git),
        exe(pm, target, "make", &["make"], "kernel/u-boot/userspace compile", true, Pkg::Make),
        exe(pm, target, "bc", &["bc"], "kernel build dependency", true, Pkg::Bc),
        exe(pm, target, "flex", &["flex"], "kernel build dependency", true, Pkg::Flex),
        exe(pm, target, "bison", &["bison"], "kernel build dependency", true, Pkg::Bison),
        openssl_check(pm, target),
    ];

    // Target C toolchain: native cc when host arch = target, else the cross gcc.
    if cross {
        let cc = format!("{cross_compile}gcc");
        checks.push(exe(
            pm, target, &cc, &[&cc],
            "cross C toolchain for the target", true, Pkg::CrossToolchain,
        ));
    } else {
        checks.push(exe(
            pm, target, "cc", &["cc", "gcc"],
            "native C toolchain for the target", true, Pkg::NativeToolchain,
        ));
    }

    // Rootfs bootstrap + rootless namespaces — needed on every build.
    checks.push(exe(pm, target, "mmdebstrap", &["mmdebstrap"], "rootfs bootstrap", true, Pkg::Mmdebstrap));
    checks.push(userns_check());

    // Local apt repo + packaging + content-hash tools the rootfs/package stages
    // shell out to. Missing, `doctor` used to pass and the build then
    // died mid-rootfs on a non-Debian host (DR-1).
    checks.push(exe(pm, target, "dpkg-deb", &["dpkg-deb"], "host .deb packaging", true, Pkg::Dpkg));
    checks.push(exe(pm, target, "dpkg-scanpackages", &["dpkg-scanpackages"], "local apt repo Packages index", true, Pkg::DpkgDev));
    checks.push(exe(pm, target, "apt-ftparchive", &["apt-ftparchive"], "local apt repo Release", true, Pkg::AptUtils));
    checks.push(exe(pm, target, "sha256sum", &["sha256sum"], "rootfs .deb content-hash capture", true, Pkg::Coreutils));

    // Cross-arch only: the rootless sandbox + qemu-user binfmt.
    if cross {
        checks.push(exe(pm, target, "bwrap", &["bwrap"], "rootless sandbox entry", true, Pkg::Bubblewrap));
        let qa = qemu_arch(target);
        let qnames = [format!("qemu-{qa}-static"), format!("qemu-{qa}")];
        let qrefs: Vec<&str> = qnames.iter().map(String::as_str).collect();
        checks.push(exe(
            pm, target, &format!("qemu-{qa}-static"), &qrefs,
            "run target binaries under binfmt", true, Pkg::QemuUser,
        ));
        checks.push(binfmt_check(pm, target, qa));
    }

    // Image assembly path: `mke2fs -d` formats the rootfs ext4 from a
    // userns-staged tree and `e2fsck -fn` verifies it clean. Both ship in
    // e2fsprogs, so the `mke2fs` probe already guarantees the pair; the userns
    // capability itself is covered by the unprivileged-userns check above.
    checks.push(exe(pm, target, "mke2fs", &["mke2fs"], "format the rootfs ext4 image", true, Pkg::E2fsprogs));

    checks
}

/// Check for an executable by scanning `PATH` for any of `candidates`.
fn exe(
    pm: PkgManager,
    target: Arch,
    name: &str,
    candidates: &[&str],
    purpose: &'static str,
    required: bool,
    pkg: Pkg,
) -> Check {
    let status = match candidates.iter().find_map(|c| which(c)) {
        Some(path) => CheckStatus::Present(path.display().to_string()),
        None => CheckStatus::Missing(pm.remedy(pkg, target)),
    };
    Check { name: name.to_string(), purpose, required, status }
}

/// openssl headers, probed via `pkg-config` (a dev lib, not an executable).
fn openssl_check(pm: PkgManager, target: Arch) -> Check {
    let present = which("pkg-config").is_some()
        && Command::new("pkg-config")
            .args(["--exists", "openssl"])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    let status = if present {
        CheckStatus::Present("pkg-config openssl".into())
    } else {
        CheckStatus::Missing(pm.remedy(Pkg::Openssl, target))
    };
    Check {
        name: "libssl (openssl)".into(),
        purpose: "kernel/u-boot certificate + TLS build dep",
        required: true,
        status,
    }
}

/// Unprivileged user namespaces with subuid/subgid ranges — the rootless
/// `mmdebstrap --mode=unshare` bootstrap, the `bwrap` sandbox, and the ext4
/// image staging (multi-uid ownership under `unshare --map-auto`) all depend
/// on them.
///
/// Probed **functionally**: actually create one with `unshare --map-root-user
/// --map-auto true` — the exact invocation the ext4 staging uses. A
/// single-sysctl read (`unprivileged_userns_clone`) misses the other ways a
/// host forbids namespaces — Ubuntu 24.04's
/// `apparmor_restrict_unprivileged_userns=1` and `user.max_user_namespaces=0` —
/// and a plain `--map-root-user` probe misses absent `/etc/subuid` ranges, so
/// the actual syscall + mapping is the authoritative check (DR-2).
fn userns_check() -> Check {
    let works = Command::new("unshare")
        .args(["--map-root-user", "--map-auto", "true"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);
    let status = if works {
        CheckStatus::Present("unshare --map-root-user --map-auto works".into())
    } else {
        CheckStatus::Missing(
            "cannot create an unprivileged user namespace with subuid mapping; enable \
             namespaces — Debian: `sudo sysctl -w kernel.unprivileged_userns_clone=1`; \
             Ubuntu 24.04+: `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`; \
             ensure `sysctl kernel.max_user_namespaces` (or `user.max_user_namespaces`) > 0; \
             and give this user a subuid/subgid range (`sudo usermod --add-subuids \
             100000-165535 --add-subgids 100000-165535 $USER`)"
                .into(),
        )
    };
    Check {
        name: "unprivileged user namespaces".into(),
        purpose: "rootless rootfs bootstrap + sandbox + ext4 image staging",
        required: true,
        status,
    }
}

/// A registered *and enabled* `qemu-<arch>` binfmt handler. Reads
/// `/proc/sys/fs/binfmt_misc/qemu-<arch>`; reports the handler flags and notes
/// when the `F` (fix-binary) flag the rootless sandbox relies on is absent.
fn binfmt_check(pm: PkgManager, target: Arch, qemu_arch: &str) -> Check {
    let path = format!("/proc/sys/fs/binfmt_misc/qemu-{qemu_arch}");
    let name = format!("{qemu_arch} binfmt handler");
    let status = match std::fs::read_to_string(&path) {
        Ok(body) if body.lines().next() == Some("enabled") => {
            let flags = body
                .lines()
                .find_map(|l| l.strip_prefix("flags:"))
                .map(|f| f.trim())
                .unwrap_or("");
            let mut detail = format!("registered, enabled (flags: {flags})");
            if !flags.contains('F') {
                detail.push_str(" — WARNING: no F flag; the sandbox needs fix-binary");
            }
            CheckStatus::Present(detail)
        }
        Ok(_) => CheckStatus::Missing(format!(
            "handler present but disabled — run: {}",
            pm.remedy(Pkg::QemuUser, target)
        )),
        Err(_) => CheckStatus::Missing(format!(
            "not registered — install {} and register binfmt (needs root, one-time)",
            pm.package(Pkg::QemuUser, target)
        )),
    };
    Check {
        name,
        purpose: "run target maintainer scripts/compiles under qemu",
        required: true,
        status,
    }
}

/// Resolve an executable name against `PATH` (like `which`), returning the first
/// hit. Scans `PATH` directly rather than shelling out, so it is fast and needs
/// no host `which`.
fn which(name: &str) -> Option<PathBuf> {
    // An explicit path (contains a separator) is checked as-is.
    if name.contains('/') {
        let p = PathBuf::from(name);
        return is_executable(&p).then_some(p);
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path).find_map(|dir| {
        let candidate = dir.join(name);
        is_executable(&candidate).then_some(candidate)
    })
}

/// True if `p` is a regular file with any execute bit set.
fn is_executable(p: &std::path::Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_release_maps_to_package_manager() {
        assert_eq!(
            PkgManager::from_os_release("ID=pop\nID_LIKE=\"ubuntu debian\"\n"),
            PkgManager::Apt
        );
        assert_eq!(PkgManager::from_os_release("ID=debian\n"), PkgManager::Apt);
        assert_eq!(PkgManager::from_os_release("ID=fedora\n"), PkgManager::Dnf);
        assert_eq!(
            PkgManager::from_os_release("ID=rocky\nID_LIKE=\"rhel centos fedora\"\n"),
            PkgManager::Dnf
        );
        assert_eq!(PkgManager::from_os_release("ID=arch\n"), PkgManager::Pacman);
        assert_eq!(PkgManager::from_os_release("ID=void\n"), PkgManager::Unknown);
    }

    #[test]
    fn remedy_is_manager_and_arch_specific() {
        assert_eq!(
            PkgManager::Apt.remedy(Pkg::Openssl, Arch::Arm64),
            "sudo apt install libssl-dev"
        );
        assert_eq!(
            PkgManager::Dnf.remedy(Pkg::Openssl, Arch::Arm64),
            "sudo dnf install openssl-devel"
        );
        assert_eq!(
            PkgManager::Apt.remedy(Pkg::CrossToolchain, Arch::Arm64),
            "sudo apt install gcc-aarch64-linux-gnu"
        );
        assert_eq!(
            PkgManager::Dnf.remedy(Pkg::CrossToolchain, Arch::Arm64),
            "sudo dnf install gcc-aarch64-linux-gnu"
        );
        assert_eq!(
            PkgManager::Pacman.remedy(Pkg::NativeToolchain, Arch::Arm64),
            "sudo pacman -S base-devel"
        );
    }

    #[test]
    fn cross_target_yields_qemu_and_sandbox_checks() {
        // Building arm64 from this test's host: if the host is not arm64, the
        // list must include the qemu binfmt + bwrap checks; the cross toolchain
        // check names the <triple>gcc. Assert the invariants that hold either way.
        let checks = tool_checks(Arch::Arm64, "aarch64-linux-gnu-");
        // git/make/mmdebstrap/mke2fs + the DR-1 packaging tools are in the list.
        for needed in ["git", "make", "mmdebstrap", "mke2fs", "dpkg-deb", "sha256sum"] {
            assert!(checks.iter().any(|c| c.name == needed), "missing {needed}");
        }
        // Every check is a hard requirement — there are no fallback-only tools.
        assert!(checks.iter().all(|c| c.required));
    }

    #[test]
    fn which_finds_a_known_tool_and_misses_a_bogus_one() {
        // `sh` exists on every unix test host; a random name does not.
        assert!(which("sh").is_some());
        assert!(which("boot2deb-definitely-not-a-real-binary").is_none());
    }
}
