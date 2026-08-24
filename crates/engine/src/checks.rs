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
//!   `bc`/`flex`/`bison`/openssl build deps, and unprivileged user namespaces. Both
//!   the OS rootfs and the target-arch *build sandbox* — where the media-accel
//!   `.deb`s compile — are bootstrapped and entered in-process through the
//!   ferroday-cage library, so neither adds a tool beyond the user namespaces
//!   already listed here.
//! - **Cross-arch only** (host arch ≠ target arch): a `qemu-<arch>` interpreter and
//!   a registered+enabled binfmt handler for the target. A same-arch host runs the
//!   target's binaries directly and skips these entirely.
//! - **Image path:** none required — the rootfs ext4 is formatted and every metadata
//!   checksum verified in-process by the pure-Rust `ferrosys` formatter. `e2fsck`, when
//!   present, runs as an optional cross-check.
//!
//! Detection is a side effect (PATH scan, `/proc` + `/etc/os-release` reads,
//! `pkg-config` shell-out), so it lives in the engine, not `core`.

use std::path::{Path, PathBuf};
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
            Pkg::E2fsprogs => "e2fsprogs".into(),
            Pkg::Fakeroot => "fakeroot".into(),
            // Kernel `bindeb-pkg` deps. rsync/cpio/kmod share the name across managers;
            // debhelper is Debian's, and elfutils' dev libs split by distro.
            Pkg::Rsync => "rsync".into(),
            Pkg::Cpio => "cpio".into(),
            Pkg::Kmod => "kmod".into(),
            // u-boot pylibfdt/binman deps. swig shares its name; the Python packages
            // are Debian's `python3-*` and split by manager elsewhere.
            Pkg::Swig => "swig".into(),
            Pkg::Python3Dev => match self {
                Dnf => "python3-devel".into(),
                Pacman | Brew => "python".into(),
                _ => "python3-dev".into(),
            },
            Pkg::Python3Setuptools => match self {
                Dnf => "python3-setuptools".into(),
                Pacman => "python-setuptools".into(),
                Brew => "python-setuptools".into(),
                _ => "python3-setuptools".into(),
            },
            Pkg::Pyelftools => match self {
                Dnf => "python3-pyelftools".into(),
                Pacman => "python-pyelftools".into(),
                Brew => "python-pyelftools".into(),
                _ => "python3-pyelftools".into(),
            },
            Pkg::Debhelper => match self {
                Pacman => "debhelper (AUR)".into(),
                _ => "debhelper".into(),
            },
            Pkg::Libelf => match self {
                Dnf => "elfutils-libelf-devel".into(),
                Pacman | Brew => "elfutils".into(),
                _ => "libelf-dev".into(),
            },
            Pkg::Libdw => match self {
                Dnf => "elfutils-devel".into(),
                Pacman | Brew => "elfutils".into(),
                _ => "libdw-dev".into(),
            },
            // dpkg is Debian's own tool; on non-Debian hosts it comes from the
            // distro's dpkg port (or AUR).
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
    QemuUser,
    E2fsprogs,
    /// `dpkg-deb` — host-side `.deb` packaging (the u-boot and kmod debs).
    Dpkg,
    /// `fakeroot` — the root-owned staging `dpkg-deb` packages those debs from.
    Fakeroot,
    /// `debhelper` (`dh`) — the kernel `make bindeb-pkg` `dpkg-buildpackage` dep.
    Debhelper,
    /// `libelf-dev` — kernel objtool/BTF build dep (`pkg-config libelf`).
    Libelf,
    /// `libdw-dev` — kernel objtool build dep (`pkg-config libdw`).
    Libdw,
    /// `rsync` — kernel `bindeb-pkg` staging dep.
    Rsync,
    /// `cpio` — kernel `bindeb-pkg` dep.
    Cpio,
    /// `kmod` (`depmod`) — kernel module tooling.
    Kmod,
    /// `swig` — u-boot `pylibfdt` binding generator.
    Swig,
    /// `python3-dev` — Python headers for the `pylibfdt` C extension.
    Python3Dev,
    /// `python3-setuptools` — u-boot `pylibfdt` `setup.py`.
    Python3Setuptools,
    /// `python3-pyelftools` — u-boot `binman` ELF parsing.
    Pyelftools,
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

/// What a *particular build* needs from the host — the input to [`tool_checks`].
///
/// Not every build needs every tool, and asking for tools a build will never invoke
/// is not harmless: it turns `doctor` from "here is what you are missing" into a
/// checklist with items on it that do not apply, which is how a real missing tool gets
/// lost in the noise. A board that installs Debian's kernel and boots its own firmware
/// compiles nothing at all, and should not be told to install a cross compiler.
#[derive(Debug, Clone)]
pub struct ToolNeeds {
    /// The target architecture.
    pub target: Arch,
    /// `CROSS_COMPILE` prefix for the target (e.g. `aarch64-linux-gnu-`), used only
    /// when the host arch differs.
    pub cross_compile: String,
    /// The build compiles a kernel and/or a bootloader from source: it needs a C
    /// toolchain for the target, the kernel's build-time helpers, and `git` to fetch
    /// the trees and apply the patch series.
    pub compiles_sources: bool,
    /// The build compiles a kernel specifically (a subset of `compiles_sources`).
    /// The kernel's `make bindeb-pkg` runs `dpkg-buildpackage`, which enforces the
    /// generated `debian/control` build-deps — `debhelper`, `libelf-dev`/`libdw-dev`
    /// (objtool/BTF), plus `rsync`/`cpio`/`kmod` — none of which the plain compile
    /// toolchain above pulls in. A bootloader-only build does not package a kernel and
    /// needs none of them.
    pub compiles_kernel: bool,
    /// The build compiles u-boot from source (a `rockchip-rkbin` boot method). u-boot's
    /// build generates its device-tree Python bindings (`pylibfdt`) and runs `binman`,
    /// which need `swig`, the Python dev headers, `setuptools`, and `pyelftools` — a
    /// distinct dep set from the kernel's. A board that boots its own firmware compiles
    /// no u-boot and needs none of them.
    pub builds_uboot: bool,
    /// Where this build's disposable build roots put their overlay upper layers
    /// ([`build_root_uppers`](crate::sandbox::build_root_uppers)), for the host's
    /// overlay capability to be probed on the filesystem that will actually carry
    /// them. `None` for a build that stands up no build root — one that compiles no
    /// packages in the target-arch sandbox — which needs no overlay at all.
    pub build_root_uppers: Option<PathBuf>,
}

/// Run every host preflight check a build actually needs, in report order.
pub fn tool_checks(needs: &ToolNeeds) -> Vec<Check> {
    let target = needs.target;
    let host = HostInfo::detect();
    let cross = host.is_cross_for(target);
    let pm = PkgManager::detect(&host);
    let mut checks = Vec::new();

    // The compile toolchain, only where something is compiled. A distro-package kernel
    // on a board with no bootloader of its own needs none of this.
    if needs.compiles_sources {
        checks.extend([
            exe(pm, target, "git", &["git"], "fetch pinned sources + git am the patch series", true, Pkg::Git),
            exe(pm, target, "make", &["make"], "kernel/u-boot compile", true, Pkg::Make),
            exe(pm, target, "bc", &["bc"], "kernel build dependency", true, Pkg::Bc),
            exe(pm, target, "flex", &["flex"], "kernel build dependency", true, Pkg::Flex),
            exe(pm, target, "bison", &["bison"], "kernel build dependency", true, Pkg::Bison),
            openssl_check(pm, target),
        ]);
        // Target C toolchain: native cc when host arch = target, else the cross gcc.
        if cross {
            let cc = format!("{}gcc", needs.cross_compile);
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
    }

    // Kernel `.deb` packaging: `make bindeb-pkg` shells out to `dpkg-buildpackage`,
    // which hard-fails on the generated `debian/control`'s build-deps before it
    // compiles a thing. These are separate from the compile toolchain above and are
    // easy to be missing on a fresh host — a bare `doctor` that skipped them would pass
    // and then the build would die minutes in on `dpkg-checkbuilddeps`. Gated on
    // `compiles_kernel`: a bootloader-only build packages no kernel and needs none.
    if needs.compiles_kernel {
        checks.push(exe(pm, target, "dh", &["dh"], "kernel .deb build (debhelper)", true, Pkg::Debhelper));
        checks.push(pkgconfig_check(pm, target, "libelf", "libelf", "objtool/BTF — kernel .deb build", Pkg::Libelf));
        checks.push(pkgconfig_check(pm, target, "libdw", "libdw", "objtool — kernel .deb build", Pkg::Libdw));
        checks.push(exe(pm, target, "rsync", &["rsync"], "kernel .deb build dependency", true, Pkg::Rsync));
        checks.push(exe(pm, target, "cpio", &["cpio"], "kernel .deb build dependency", true, Pkg::Cpio));
        checks.push(exe(pm, target, "depmod", &["depmod"], "kernel module tooling (kmod)", true, Pkg::Kmod));
    }

    // u-boot's build generates its `pylibfdt` device-tree bindings and runs `binman`.
    // That needs `swig` + the Python dev headers to compile the extension, plus the
    // `setuptools`/`pyelftools` modules — a fresh host has none by default, and the
    // failure is a mid-build Python traceback rather than a clear "missing dep".
    if needs.builds_uboot {
        checks.push(exe(pm, target, "swig", &["swig"], "u-boot pylibfdt bindings", true, Pkg::Swig));
        checks.push(pkgconfig_check(pm, target, "python3", "python3-dev", "u-boot pylibfdt extension headers", Pkg::Python3Dev));
        checks.push(python_module_check(pm, target, "setuptools", "python3-setuptools", "u-boot pylibfdt build", Pkg::Python3Setuptools));
        checks.push(python_module_check(pm, target, "elftools", "python3-pyelftools", "u-boot binman image assembly", Pkg::Pyelftools));
    }

    // Rootless namespaces — needed on every build. Both bootstraps (the *OS* rootfs
    // that becomes the image, [`crate::rootfs`], and the target-arch *build sandbox*)
    // run in-process through ferroday-cage, so neither needs a binary of its own;
    // what they do need, along with the package builds, is unprivileged user
    // namespaces ([`userns_check`]).
    checks.push(userns_check());

    // A build that compiles packages in the target-arch sandbox roots each component's
    // build on an unprivileged overlay, which not every host can establish — so the
    // capability is preflighted like a tool. A build that compiles no such packages
    // stands up no build root and is unaffected.
    if let Some(uppers) = &needs.build_root_uppers {
        checks.push(overlay_check(uppers));
    }

    // Host-side `.deb` packaging: the u-boot and kmod `.deb`s are assembled with
    // `fakeroot dpkg-deb`. Both halves are checked, because the failure modes differ
    // and only one of them is loud — a missing `dpkg-deb` fails to spawn, while
    // `fakeroot` is what makes the staged tree package as `root:root`, so without it
    // the debs would carry the build user's ownership. Missing either, `doctor` would
    // pass and the build then die mid-package on a non-Debian host.
    checks.push(exe(pm, target, "fakeroot", &["fakeroot"], "root-owned staging for the u-boot and kmod .debs", true, Pkg::Fakeroot));
    checks.push(exe(pm, target, "dpkg-deb", &["dpkg-deb"], "assemble the u-boot and kmod .debs", true, Pkg::Dpkg));

    // Cross-arch: the target's maintainer scripts and compiles run under the host's
    // qemu-user binfmt handler — during the rootfs bootstrap whatever else the build
    // does, so this is needed even by a board that compiles nothing. A host whose arch
    // already matches the target runs them directly and needs no qemu at all.
    if cross {
        let qa = qemu_arch(target);
        let qnames = [format!("qemu-{qa}-static"), format!("qemu-{qa}")];
        let qrefs: Vec<&str> = qnames.iter().map(String::as_str).collect();
        checks.push(exe(
            pm, target, &format!("qemu-{qa}-static"), &qrefs,
            "run target binaries under binfmt", true, Pkg::QemuUser,
        ));
        checks.push(binfmt_check(pm, target, qa));
    }

    // Image assembly is pure Rust (ferrosys): the rootfs ext4 is formatted and every
    // metadata checksum verified in-process, so no `mke2fs`/`e2fsprogs` is required.
    // `e2fsck` is an optional `-fn` cross-check the image stage runs only when present.
    checks.push(exe(pm, target, "e2fsck", &["e2fsck"], "optional cross-check of the formatted rootfs ext4 image", false, Pkg::E2fsprogs));

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
    pkgconfig_check(
        pm, target, "openssl", "libssl (openssl)",
        "kernel/u-boot certificate + TLS build dep", Pkg::Openssl,
    )
}

/// A `-dev` library the build probes with `pkg-config --exists <module>` — present
/// only when its development package is installed. `name` is the display label and
/// `purpose` its role; a miss maps `pkg` to the host's package name. The same shape as
/// [`exe`] but for a library rather than an executable. If `pkg-config` itself is
/// absent the module reads as missing (its remedy still names the `-dev` package —
/// `pkg-config` rides in as a dependency of the toolchain check).
fn pkgconfig_check(
    pm: PkgManager,
    target: Arch,
    module: &str,
    name: &str,
    purpose: &'static str,
    pkg: Pkg,
) -> Check {
    let present = which("pkg-config").is_some()
        && Command::new("pkg-config")
            .args(["--exists", module])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    let status = if present {
        CheckStatus::Present(format!("pkg-config {module}"))
    } else {
        CheckStatus::Missing(pm.remedy(pkg, target))
    };
    Check {
        name: name.to_string(),
        purpose,
        required: true,
        status,
    }
}

/// A Python module the build imports at runtime — probed with `python3 -c "import
/// <module>"`, so it catches a package installed under the wrong interpreter, which a
/// dpkg presence test would miss. Named after the *distro package* (`name`) since that
/// is what a miss tells the user to install. When `python3` itself is absent the module
/// reads as missing.
fn python_module_check(
    pm: PkgManager,
    target: Arch,
    module: &str,
    name: &str,
    purpose: &'static str,
    pkg: Pkg,
) -> Check {
    let present = which("python3").is_some()
        && Command::new("python3")
            .args(["-c", &format!("import {module}")])
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
    let status = if present {
        CheckStatus::Present(format!("python3 import {module}"))
    } else {
        CheckStatus::Missing(pm.remedy(pkg, target))
    };
    Check {
        name: name.to_string(),
        purpose,
        required: true,
        status,
    }
}

/// Unprivileged user namespaces with subuid/subgid ranges — the OS-rootfs
/// bootstrap (its subordinate-mapped provision + `export_tar`, which need a
/// `newuidmap`/subuid range), the in-process build sandbox (its dpkg-configure
/// waves), and the package builds all depend on them.
///
/// Probed **functionally**: actually create one with `unshare --map-root-user
/// --map-auto true` — the exact invocation the rootless bootstrap uses. A
/// single-sysctl read (`unprivileged_userns_clone`) misses the other ways a
/// host forbids namespaces — Ubuntu 24.04's
/// `apparmor_restrict_unprivileged_userns=1` and `user.max_user_namespaces=0` —
/// and a plain `--map-root-user` probe misses absent `/etc/subuid` ranges, so
/// the actual syscall + mapping is the authoritative check.
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

/// Whether an unprivileged overlay can be established with its upper layer on the
/// filesystem hosting `uppers` — what a build root is rooted on.
///
/// Two host properties gate it and the probe reports the first that fails: the
/// filesystem must hold `user.*` extended attributes, which is where an unprivileged
/// overlay records its whiteouts, and the kernel must accept the mount from inside a
/// user namespace.
///
/// **The filesystem is the subject, not the directory.** Which one the uppers land on
/// is decided by the work dir, and `/tmp` may well answer differently — a tmpfs `/tmp`
/// on a pre-6.6 kernel cannot hold the xattrs while the ext4 work dir beside it can. So
/// the probe is pointed at the real location, and since that directory does not exist
/// until the first build creates it, it walks up to the nearest existing ancestor:
/// capability is a property of the filesystem, which an ancestor shares.
pub fn overlay_check(uppers: &Path) -> Check {
    let status = match nearest_existing(uppers) {
        None => CheckStatus::Missing(format!(
            "no existing directory above {} to probe; create the work dir's parent",
            uppers.display()
        )),
        Some(dir) => match ferroday_cage::host::overlay_blocker(&dir) {
            None => CheckStatus::Present(format!("upper on {} works", dir.display())),
            // The blocker names its own remedy; the probed directory is what tells the
            // operator which filesystem to move or upgrade.
            Some(blocker) => CheckStatus::Missing(format!("{blocker} (probed {})", dir.display())),
        },
    };
    Check {
        name: "unprivileged overlay".into(),
        purpose: "disposable per-component build roots",
        required: true,
        status,
    }
}

/// The nearest ancestor of `path` — `path` itself first — that exists as a directory.
///
/// `None` only when nothing up to the root is one, which on an absolute path means the
/// filesystem cannot be reached at all.
fn nearest_existing(path: &Path) -> Option<PathBuf> {
    path.ancestors().find(|p| p.is_dir()).map(Path::to_path_buf)
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

    /// The overlay check answers about a directory the first build has not created yet,
    /// and its report has to name the directory it actually asked about — an operator
    /// whose work dir is on another volume has no other way to see that the probe went
    /// somewhere else.
    #[test]
    fn the_overlay_check_probes_the_nearest_existing_ancestor() {
        let tmp = tempfile::tempdir().unwrap();
        let uppers = tmp.path().join("sandbox").join("layers");

        // Nothing below the scratch root exists, so the scratch root answers for it:
        // overlay capability belongs to the filesystem, which the ancestor shares.
        assert_eq!(nearest_existing(&uppers).as_deref(), Some(tmp.path()));
        // Once the directory itself exists it is its own answer.
        std::fs::create_dir_all(&uppers).unwrap();
        assert_eq!(nearest_existing(&uppers).as_deref(), Some(uppers.as_path()));

        // Either verdict names the probed directory. Which verdict it is depends on the
        // host, which is the point of the check — so the assertion is on the report.
        let check = overlay_check(&uppers);
        assert!(check.required, "a build root cannot be established without it");
        let detail = match &check.status {
            CheckStatus::Present(d) | CheckStatus::Missing(d) => d,
        };
        assert!(detail.contains(&uppers.display().to_string()), "{detail}");
    }

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
        // Kernel bindeb-pkg deps: the exact package a user installs on a miss.
        assert_eq!(PkgManager::Apt.remedy(Pkg::Debhelper, Arch::Arm64), "sudo apt install debhelper");
        assert_eq!(PkgManager::Apt.remedy(Pkg::Libelf, Arch::Arm64), "sudo apt install libelf-dev");
        assert_eq!(PkgManager::Apt.remedy(Pkg::Libdw, Arch::Arm64), "sudo apt install libdw-dev");
        assert_eq!(PkgManager::Dnf.remedy(Pkg::Libelf, Arch::Arm64), "sudo dnf install elfutils-libelf-devel");
        // u-boot pylibfdt/binman deps.
        assert_eq!(PkgManager::Apt.remedy(Pkg::Swig, Arch::Arm64), "sudo apt install swig");
        assert_eq!(PkgManager::Apt.remedy(Pkg::Python3Setuptools, Arch::Arm64), "sudo apt install python3-setuptools");
        assert_eq!(PkgManager::Apt.remedy(Pkg::Pyelftools, Arch::Arm64), "sudo apt install python3-pyelftools");
    }

    /// The RK1 shape: compiles a kernel and a bootloader, and builds the media-accel
    /// stack in the sandbox.
    fn compiling_build() -> ToolNeeds {
        ToolNeeds {
            target: Arch::Arm64,
            cross_compile: "aarch64-linux-gnu-".into(),
            compiles_sources: true,
            compiles_kernel: true,
            builds_uboot: true,
            build_root_uppers: Some(std::env::temp_dir()),
        }
    }

    /// The C201 shape: Debian's kernel, the board's own firmware, no accel stack — so
    /// nothing is compiled from source at all.
    fn assembling_build() -> ToolNeeds {
        ToolNeeds {
            target: Arch::Armv7,
            cross_compile: "arm-linux-gnueabihf-".into(),
            compiles_sources: false,
            compiles_kernel: false,
            builds_uboot: false,
            build_root_uppers: None,
        }
    }

    #[test]
    fn a_compiling_build_asks_for_the_toolchain() {
        let checks = tool_checks(&compiling_build());
        for needed in ["git", "make", "bc", "flex", "bison", "fakeroot", "dpkg-deb"] {
            assert!(checks.iter().any(|c| c.name == needed), "missing {needed}");
        }
        // Image assembly needs no host filesystem tool now; `e2fsck` rides along as the
        // sole optional cross-check, and every other tool is a hard requirement.
        assert!(
            checks.iter().filter(|c| c.name != "e2fsck").all(|c| c.required),
            "only e2fsck is optional"
        );
        let e2fsck = checks.iter().find(|c| c.name == "e2fsck").expect("e2fsck check present");
        assert!(!e2fsck.required, "e2fsck is an optional cross-check");
    }

    #[test]
    fn a_kernel_build_asks_for_the_bindeb_pkg_deps() {
        // `make bindeb-pkg` runs `dpkg-buildpackage`, which enforces the generated
        // control's build-deps before compiling. doctor must list them or it passes and
        // the build dies on `dpkg-checkbuilddeps` minutes in (the fresh-host trap).
        let checks = tool_checks(&compiling_build());
        for needed in ["dh", "libelf", "libdw", "rsync", "cpio", "depmod"] {
            assert!(
                checks.iter().any(|c| c.name == needed && c.required),
                "a kernel build must require {needed}"
            );
        }
        // A kernel-less build packages no kernel, so it must NOT ask for the deb deps.
        let no_kernel = ToolNeeds { compiles_kernel: false, ..compiling_build() };
        for absent in ["dh", "libelf", "libdw", "depmod"] {
            assert!(
                !tool_checks(&no_kernel).iter().any(|c| c.name == absent),
                "{absent} is a kernel-deb dep; a kernel-less build should not ask for it"
            );
        }
    }

    #[test]
    fn a_uboot_build_asks_for_the_pylibfdt_deps() {
        // u-boot compiles its pylibfdt bindings + runs binman; a fresh host has none of
        // this and fails on a mid-build Python traceback, not a clear missing-dep error.
        let checks = tool_checks(&compiling_build());
        for needed in ["swig", "python3-dev", "python3-setuptools", "python3-pyelftools"] {
            assert!(
                checks.iter().any(|c| c.name == needed && c.required),
                "a u-boot build must require {needed}"
            );
        }
        // A board that boots its own firmware compiles no u-boot and skips them.
        let no_uboot = ToolNeeds { builds_uboot: false, ..compiling_build() };
        for absent in ["swig", "python3-dev", "python3-setuptools", "python3-pyelftools"] {
            assert!(
                !tool_checks(&no_uboot).iter().any(|c| c.name == absent),
                "{absent} is a u-boot dep; a firmware-boot board should not ask for it"
            );
        }
    }

    #[test]
    fn the_sandbox_adds_no_external_tool_and_qemu_stays_cross_only() {
        // The package stages enter their target-arch sandbox through the in-process
        // ferroday-cage library, not an external sandbox binary, so a sandbox build
        // asks for no `bwrap`/`bubblewrap` — only the unprivileged user namespaces
        // that every build already requires.
        let checks = tool_checks(&compiling_build());
        assert!(
            !checks.iter().any(|c| c.name == "bwrap" || c.name == "bubblewrap"),
            "the in-process sandbox needs no external sandbox binary"
        );
        assert!(
            checks.iter().any(|c| c.name.contains("user namespace") && c.required),
            "every build requires unprivileged user namespaces"
        );
        // The overlay behind each component's build root is the sandbox's one host
        // requirement beyond those namespaces, and it is asked for only by a build that
        // compiles packages in there.
        assert!(
            checks.iter().any(|c| c.name == "unprivileged overlay" && c.required),
            "a sandbox build requires an unprivileged overlay"
        );
        assert!(
            !tool_checks(&assembling_build()).iter().any(|c| c.name == "unprivileged overlay"),
            "a build that compiles no packages stands up no build root"
        );
        // qemu-user is the genuinely cross-only half: a matching-arch host runs the
        // target's binaries directly and never consults a binfmt handler.
        let host = HostInfo::detect();
        assert_eq!(
            checks.iter().any(|c| c.name == "qemu-aarch64-static"),
            host.is_cross_for(Arch::Arm64),
            "qemu-user is needed exactly when the host cannot run the target's binaries"
        );
    }

    #[test]
    fn a_build_that_compiles_nothing_asks_for_no_compiler() {
        // The payoff of a needs-driven list: this board installs Debian's kernel and
        // boots its own firmware, so a cross compiler is not merely unused — telling
        // the operator to install one is noise a genuinely missing tool could hide in.
        let checks = tool_checks(&assembling_build());
        for absent in ["git", "make", "bc", "flex", "bison", "openssl"] {
            assert!(
                !checks.iter().any(|c| c.name.contains(absent)),
                "{absent} is not needed by a build that compiles nothing"
            );
        }
        assert!(!checks.iter().any(|c| c.name.ends_with("gcc")));

        // What it *does* still need: the OS rootfs is bootstrapped and content-pinned
        // the same way, and its armhf maintainer scripts still run under qemu — so a
        // missing binfmt handler is still a blocking failure, and it is the *arm* one,
        // not aarch64.
        for needed in ["fakeroot", "dpkg-deb"] {
            assert!(checks.iter().any(|c| c.name == needed), "missing {needed}");
        }
        let host = HostInfo::detect();
        if host.is_cross_for(Arch::Armv7) {
            assert!(checks.iter().any(|c| c.name == "qemu-arm-static"));
            assert!(checks.iter().any(|c| c.name.contains("arm binfmt")));
        }
    }

    #[test]
    fn which_finds_a_known_tool_and_misses_a_bogus_one() {
        // `sh` exists on every unix test host; a random name does not.
        assert!(which("sh").is_some());
        assert!(which("boot2deb-definitely-not-a-real-binary").is_none());
    }
}
