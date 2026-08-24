//! Host tool-presence preflight for `doctor`.
//!
//! Host identity and cross-arch status come from [`crate::preflight`]; this module
//! adds the concrete tool/capability checks the build needs, with per-platform
//! remediation. It reports exactly what is present or missing *before* any build
//! work starts — the same "typed error before any work starts" contract as config
//! validation.
//!
//! The list is short, and its shortness is the design rather than an omission. Every
//! compiler, packaging tool and build-dependency a build runs is a *package of a
//! provisioned Debian root* ([`crate::sandbox`]): resolved from the build's own mirror
//! list and sha256-pinned in that root's manifest, so it is an input the lock names
//! rather than a fact about the machine. What is asked of the host is what no root can
//! carry:
//!
//! - **Every build:** unprivileged user namespaces. Each root a build provisions — the
//!   OS rootfs, the target-arch build sandbox, the host-arch cross root, the packaging
//!   root — is bootstrapped and entered in-process through the ferroday-cage library,
//!   which needs the capability and no binary at all. No `dpkg`, no `fakeroot`, no
//!   external sandbox helper.
//! - **A build that compiles**
//!   ([`compiles_from_source`](boot2deb_core::model::ResolvedBuild::compiles_from_source)):
//!   host `git`, which clones the pinned trees and applies the patch series before any
//!   root sees them, and an unprivileged **overlay**, which is how a compile root layers
//!   a stage's build-dependencies over its base.
//! - **A build that enters a target-arch root** on a host that cannot execute the
//!   target's binaries ([`needs_interpreter`](HostInfo::needs_interpreter)): a
//!   `qemu-<arch>` interpreter and a registered+enabled binfmt handler. That is the
//!   image path — the OS rootfs runs the target's maintainer scripts, and the media-accel
//!   `.deb`s compile in a target-arch sandbox. The cross root and the packaging root are
//!   the *host's* architecture and interpret nothing, so a bootloader-only deliverable
//!   needs no qemu even when it builds for a foreign target.
//! - **Image path:** `tar` and `cp`, the two POSIX tools the rootfs and image stages
//!   invoke directly. No filesystem tooling — the rootfs ext4 is formatted and then
//!   scanned back in-process by the pure-Rust `ferrosys` formatter; `e2fsck`, when
//!   present, runs as an optional independent cross-check.
//!
//! Detection is a side effect (PATH scan, `/proc` + `/etc/os-release` reads, an `unshare`
//! probe), so it lives in the engine, not `core`.

use std::path::{Path, PathBuf};
use std::process::Command;

use boot2deb_core::host::HostInfo;
use boot2deb_core::model::Arch;
use ferroday_cage::provision::debian::foreign_interpreter;

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
    fn package(self, pkg: Pkg) -> String {
        match pkg {
            // Same name across managers.
            Pkg::Git => "git".into(),
            Pkg::E2fsprogs => "e2fsprogs".into(),
            Pkg::Tar => "tar".into(),
            Pkg::Coreutils => "coreutils".into(),
            Pkg::QemuUser => match self {
                PkgManager::Pacman => "qemu-user-static-binfmt (AUR)".into(),
                _ => "qemu-user-static".into(),
            },
        }
    }

    /// A one-line remediation for a missing [`Pkg`].
    fn remedy(self, pkg: Pkg) -> String {
        format!("{} {}", self.install_cmd(), self.package(pkg))
    }
}

/// Canonical host packages the build depends on, mapped to per-manager names.
///
/// Short, and every entry earns its place by being something no provisioned root can
/// supply — see the [module docs](self). A compiler or a build-dependency does not
/// belong here: it belongs in a root's package set, where the lock pins its version.
#[derive(Debug, Clone, Copy)]
enum Pkg {
    /// `git` — clones the pinned kernel/u-boot/userspace trees and applies the patch
    /// series with `git am --3way`, both on the host and before any root sees the tree.
    Git,
    /// `qemu-user-static` — interprets the target's binaries where the host cannot
    /// execute them, under the host kernel's binfmt handler.
    QemuUser,
    /// `e2fsprogs` (`e2fsck`) — the optional `-fn` cross-check of the formatted rootfs.
    E2fsprogs,
    /// `tar` — unpacking and verifying the rootfs tarball and a signed kernel partition.
    Tar,
    /// `coreutils` (`cp`) — merging the layer overlay trees into the build's staging
    /// tree. Laying that tree into the provisioned rootfs is in-process, through the
    /// identity map the rootfs is owned under, so it needs no host tool.
    Coreutils,
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
    /// The target architecture, which decides *which* `qemu-<arch>` interpreter and
    /// binfmt handler are asked for on a host that cannot execute the target's binaries.
    pub target: Arch,
    /// Where this build's compile roots put their overlay upper layers
    /// ([`build_root_uppers`](crate::sandbox::build_root_uppers)), or `None` for a build
    /// that compiles nothing at all
    /// ([`compiles_from_source`](boot2deb_core::model::ResolvedBuild::compiles_from_source)).
    ///
    /// One field, because the two things compiling asks of a host are asked together:
    /// host `git`, which fetches the pinned trees and applies the patch series, and an
    /// unprivileged overlay, which is how every compile root layers a stage's
    /// build-dependencies over its base. A board that installs Debian's kernel and boots
    /// its own firmware needs neither, and telling its operator to install a compiler
    /// would be noise a genuinely missing tool could hide in.
    ///
    /// The path is the build's own work dir rather than the host temp dir, so the
    /// capability is probed on the filesystem that will actually carry the uppers — an
    /// operator whose work dir is on another volume can get a different answer there.
    pub compiles: Option<PathBuf>,
    /// The build assembles a rootfs and a disk image.
    ///
    /// Two consequences, both keyed on this one fact. It shells out to the two POSIX
    /// tools the image path uses directly: `cp` merges the layer overlay trees into the
    /// build's staging tree, and `tar` verifies the rootfs tarball (and, on a
    /// depthcharge board, extracts the signed kernel partition). And
    /// it is the only path that enters a **target-arch** root — the rootfs runs the
    /// target's maintainer scripts, and the media-accel `.deb`s compile in a target-arch
    /// sandbox — so it is what makes a `qemu-user` interpreter a requirement on a host
    /// that cannot execute those binaries. A bootloader-only deliverable assembles
    /// nothing and compiles in a host-arch cross root, so it needs none of the three.
    pub assembles_image: bool,
}

/// Run every host preflight check a build actually needs, in report order.
pub fn tool_checks(needs: &ToolNeeds) -> Vec<Check> {
    tool_checks_on(HostInfo::detect(), needs)
}

/// The requirements **every** build has, whatever board it targets — the answerable
/// half of a `doctor` with no target named.
///
/// Membership is decided by one question: does the answer change with the recipe? Host
/// `git` does (a board installing Debian's kernel and booting its own firmware clones
/// nothing), so it is not here. Unprivileged user namespaces do not: every build
/// provisions at least one Debian root through them — the userland its `.deb`s are
/// compiled in, the one whose `dpkg` archives them, or the one that becomes the image —
/// and none of that needs a binary on the host. This is the whole of what boot2deb asks
/// of a host it does not resolve a recipe for.
///
/// [`tool_checks_on`] emits exactly this set unconditionally, by calling it, so a bare
/// `doctor` and a targeted one can never report a different verdict for the same
/// requirement.
pub fn host_checks() -> Vec<Check> {
    host_checks_on(HostInfo::detect())
}

/// [`host_checks`] against an explicit host.
///
/// The shared set happens not to consult the host today — every check it holds is a
/// kernel capability, and kernels do not have package managers. The parameter stays
/// because the seam is what [`tool_checks_on`] composes this through, and because a
/// shared check that *did* need a per-manager install hint would otherwise have to
/// change this signature and every caller at once.
pub fn host_checks_on(_host: HostInfo) -> Vec<Check> {
    // Every provisioned root is bootstrapped and entered in-process through
    // ferroday-cage — the *OS* rootfs that becomes the image ([`crate::rootfs`]), the
    // target-arch *build sandbox*, the host-arch *cross root* and the *packaging root*
    // ([`crate::sandbox`]) — so none of them needs a binary of its own. What all four
    // need is unprivileged user namespaces, and that is the one thing every build
    // requires of every host.
    vec![userns_check()]
}

/// [`tool_checks`] against an explicit host.
///
/// The host decides one thing here — whether an interpreter is asked for — and the
/// interesting case is the host that provisions a cross root and still needs none: an
/// arm64 one building armhf, where `CONFIG_COMPAT=y` runs the result natively. Taking the
/// host as a parameter is what makes that assertable from a CI machine that is not one,
/// and it is why the tests below fix the host rather than detecting it.
pub fn tool_checks_on(host: HostInfo, needs: &ToolNeeds) -> Vec<Check> {
    let target = needs.target;
    let interpreter = host.needs_interpreter(target);
    let pm = PkgManager::detect(&host);
    let mut checks = Vec::new();

    // The one host binary a compile still reaches for. Everything else it needs — the
    // toolchain, `make`, the kernel's and u-boot's build-deps — is resolved into a
    // provisioned root from the build's own mirror list, so it is pinned by the lock
    // rather than installed on the machine. `git` is not, and cannot be: it fetches the
    // pinned trees over the network and applies the patch series *before* there is a
    // root for them to enter.
    if needs.compiles.is_some() {
        checks.push(exe(
            pm,
            "git",
            &["git"],
            "fetch pinned sources + git am the patch series",
            true,
            Pkg::Git,
        ));
    }

    // The requirements no board can opt out of, in one shared block so a bare
    // `doctor` and a targeted one can never disagree about them.
    checks.extend(host_checks_on(host));

    // Every compile root layers its stage's build-dependencies over a shared base, and
    // an unprivileged overlay is what makes that layer disposable — so the capability is
    // preflighted like a tool. Not every host can establish one, and a build that
    // compiles nothing never asks it to.
    if let Some(uppers) = &needs.compiles {
        checks.push(overlay_check(uppers));
    }

    // The two POSIX tools the image path invokes directly. Both are effectively
    // universal, which is exactly why they are easy to leave off a list built from
    // "what did I have to install" — but `doctor`'s contract is what this build will
    // invoke, not what is usually there, and a container image trimmed past coreutils
    // would otherwise fail mid-rootfs rather than here.
    if needs.assembles_image {
        checks.push(exe(
            pm,
            "tar",
            &["tar"],
            "verify the rootfs tarball / extract the signed kernel partition",
            true,
            Pkg::Tar,
        ));
        checks.push(exe(
            pm,
            "cp",
            &["cp"],
            "stage the layer overlay trees into the rootfs",
            true,
            Pkg::Coreutils,
        ));
    }

    // Emulated execution, on the image path alone: the OS rootfs runs the target's
    // maintainer scripts, and the media-accel `.deb`s compile in a target-arch sandbox,
    // both under the host's qemu-user binfmt handler. Every other root a build
    // provisions — the cross root that compiles the kernel, u-boot and the modules, and
    // the packaging root that archives them — is the *host's* architecture and
    // interprets nothing, so a bootloader-only deliverable consults no interpreter even
    // when it builds for a foreign target.
    //
    // Keyed additionally on the *interpreter* question, which is weaker than "is this
    // build cross": a host that executes the target's binaries directly needs no qemu at
    // all, and that includes an arm64 host building armhf, where CONFIG_COMPAT=y runs
    // those binaries natively. Either over-ask would report a blocking requirement for
    // tooling the build never invokes — the exact noise ToolNeeds exists to eliminate.
    if interpreter && needs.assembles_image {
        let qa = target.qemu_arch();
        let qnames = [format!("qemu-{qa}-static"), format!("qemu-{qa}")];
        let qrefs: Vec<&str> = qnames.iter().map(String::as_str).collect();
        checks.push(exe(
            pm,
            &format!("qemu-{qa}-static"),
            &qrefs,
            "run target binaries under binfmt",
            true,
            Pkg::QemuUser,
        ));
        checks.push(binfmt_check(pm, target.debian_arch(), qa));
    }

    // Image assembly is pure Rust (ferrosys): the rootfs ext4 is formatted and scanned
    // back in-process, so no `mke2fs`/`e2fsprogs` is required. `e2fsck` is an optional
    // `-fn` cross-check the image stage runs only when present — valuable because it is
    // an independent implementation, not because it checks more.
    checks.push(exe(
        pm,
        "e2fsck",
        &["e2fsck"],
        "optional cross-check of the formatted rootfs ext4 image",
        false,
        Pkg::E2fsprogs,
    ));

    checks
}

/// Check for an executable by scanning `PATH` for any of `candidates`, mapping a miss
/// to `pkg`'s name on this host's package manager.
fn exe(
    pm: PkgManager,
    name: &str,
    candidates: &[&str],
    purpose: &'static str,
    required: bool,
    pkg: Pkg,
) -> Check {
    let status = match candidates.iter().find_map(|c| which(c)) {
        Some(path) => CheckStatus::Present(path.display().to_string()),
        None => CheckStatus::Missing(pm.remedy(pkg)),
    };
    Check {
        name: name.to_string(),
        purpose,
        required,
        status,
    }
}

/// Unprivileged user namespaces with subuid/subgid ranges — the OS-rootfs
/// bootstrap (its subordinate-mapped provision + `export_tar`, which need a
/// `newuidmap`/subuid range), the in-process build sandbox (its dpkg-configure
/// waves), and the package builds all depend on them.
///
/// Probed **functionally**: actually create the namespaces with `unshare
/// --map-root-user --map-auto unshare --user true`. A single-sysctl read
/// (`unprivileged_userns_clone`) misses the other ways a host forbids namespaces —
/// Ubuntu 24.04's `apparmor_restrict_unprivileged_userns=1` and
/// `user.max_user_namespaces=0` — and a plain `--map-root-user` probe misses absent
/// `/etc/subuid` ranges, so the actual syscall + mapping is the authoritative check.
///
/// **Two namespaces, not one**, because that is what a launch holds: the sandbox's own,
/// and the nested one its command enters so the kernel locks the sandbox's mount flags.
/// They are charged against `user.max_user_namespaces` at every level, so a host with a
/// ceiling of 1 satisfies a single-namespace probe and then fails every build at launch.
/// The probe's whole premise is that it performs the sequence a build performs.
///
/// The probe answers *whether*, and [`userns_blocker_detail`] answers *why* — but a
/// blocker is consulted even when the probe passes, because the probe is necessary and
/// not sufficient. A ceiling this process fits under is not one every build fits under:
/// a host allowing four namespaces with three already live passes here and exhausts its
/// budget at launch. Where the library names a condition, that condition is the answer
/// regardless of what one successful `unshare` proved.
fn userns_check() -> Check {
    let status = match Command::new("unshare")
        .args(["--map-root-user", "--map-auto", "unshare", "--user", "true"])
        .output()
    {
        Ok(out) if out.status.success() => match ferroday_cage::host::userns_blocker() {
            Some(blocker) => CheckStatus::Missing(blocker.to_string()),
            None => CheckStatus::Present(
                "unshare --map-root-user --map-auto unshare --user works (two namespaces, as a \
                 launch holds)"
                    .into(),
            ),
        },
        Ok(_) => CheckStatus::Missing(userns_blocker_detail()),
        // `unshare` is util-linux, so its absence is a different problem from a host
        // that forbids namespaces — and one no blocker classifier will explain.
        Err(_) => CheckStatus::Missing(
            "could not run `unshare` to probe user namespaces — install util-linux".into(),
        ),
    };
    Check {
        name: "unprivileged user namespaces".into(),
        purpose:
            "every provisioned root (rootfs, build sandbox, packaging root) + ext4 image staging",
        required: true,
        status,
    }
}

/// Why the user-namespace probe failed, asked of the sandbox library rather than
/// guessed — the same delegation [`overlay_check`] makes, and for the same reason: the
/// library owns these conditions, so only it can keep an answer current.
///
/// Two independent things must hold, and they fail for unrelated reasons, so both are
/// consulted in the order the kernel needs them:
///
///  1. **The namespaces can be created at all** — three sysctls, one of them
///     Ubuntu-specific, and the ceiling read against the *two* a launch holds rather
///     than against zero ([`userns_blocker`](ferroday_cage::host::userns_blocker)).
///  2. **A subordinate *range* can be mapped into it**
///     ([`range_map_blocker`](ferroday_cage::host::range_map_blocker)). An
///     unprivileged process may write exactly one uid_map entry — itself to root —
///     so the range the rootfs's real ownership needs comes from the shadow suite's
///     setuid `newuidmap`/`newgidmap`. A host can therefore have a perfectly good
///     `/etc/subuid` allocation and still fail here because those helpers are not
///     installed, which is what a Debian image without the `uidmap` package looks
///     like — and a remedy that only talked about sysctls and `usermod --add-subuids`
///     would send the operator to fix four things that are already correct.
///
/// A blocker states its own remedy, including the per-distro package name, so this
/// adds none of its own. The fallback covers a refusal neither classifier recognizes
/// — a seccomp filter or an LSM policy can deny the syscall with nothing in the host
/// configuration to read.
fn userns_blocker_detail() -> String {
    if let Some(blocker) = ferroday_cage::host::userns_blocker() {
        return blocker.to_string();
    }
    if let Some(blocker) = ferroday_cage::host::range_map_blocker() {
        return blocker.to_string();
    }
    "cannot create the two nested unprivileged user namespaces a launch holds, with a \
     subordinate id map, and no known host condition explains it — a seccomp filter or \
     an LSM policy may be denying the syscall; run `unshare --map-root-user --map-auto \
     unshare --user true` to see the kernel's own error"
        .to_string()
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

/// A registered *and enabled* `qemu-<arch>` binfmt handler for the Debian architecture
/// `arch`, reported with the interpreter it will actually run.
///
/// Reads the registration through
/// [`foreign_interpreter`](ferroday_cage::provision::debian::foreign_interpreter) — the
/// same reader [`HostToolchain`](crate::toolchain::HostToolchain) identifies the
/// interpreter from, and the same one the rootfs provisioner's own preflight uses. One
/// reader of one `/proc` file, so what `doctor` reports, what a build folds into its
/// cache key, and what the bootstrap refuses on cannot disagree.
///
/// The interpreter is worth printing rather than assumed: it is generally a wrapper
/// under `/usr/libexec` rather than the `qemu-<arch>-static` on `PATH`, and an operator
/// diagnosing an emulation problem is otherwise looking at the wrong binary. Where the
/// wrapper resolves elsewhere, both paths are printed — repointing that symlink swaps
/// the interpreter with the registration unchanged.
///
/// A registered-but-*disabled* handler is reported as present and not enabled rather
/// than as absent, because the remedies differ: one is "turn it on", the other "install
/// it".
fn binfmt_check(pm: PkgManager, arch: &str, qemu_arch: &str) -> Check {
    let name = format!("{qemu_arch} binfmt handler");
    let status = match foreign_interpreter(arch) {
        Some(interpreter) if interpreter.enabled => {
            let mut detail = format!("registered, enabled (flags: {})", interpreter.flags);
            detail.push_str(&format!(" — runs {}", interpreter.path.display()));
            match &interpreter.resolved {
                Some(resolved) if resolved != &interpreter.path => {
                    detail.push_str(&format!(" -> {}", resolved.display()));
                }
                Some(_) => {}
                None => detail.push_str(" — WARNING: that path does not resolve"),
            }
            if !interpreter.flags.contains('F') {
                detail.push_str(" — WARNING: no F flag; the sandbox needs fix-binary");
            }
            CheckStatus::Present(detail)
        }
        Some(_) => CheckStatus::Missing(format!(
            "handler present but disabled — run: {}",
            pm.remedy(Pkg::QemuUser)
        )),
        None => CheckStatus::Missing(format!(
            "not registered — install {} and register binfmt (needs root, one-time)",
            pm.package(Pkg::QemuUser)
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
        assert!(
            check.required,
            "a build root cannot be established without it"
        );
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
        assert_eq!(
            PkgManager::from_os_release("ID=void\n"),
            PkgManager::Unknown
        );
    }

    #[test]
    fn remedy_is_manager_specific() {
        // The whole table, because it is now small enough to assert whole — and a
        // package that reappears in it is a host requirement that came back unannounced.
        assert_eq!(PkgManager::Apt.remedy(Pkg::Git), "sudo apt install git");
        assert_eq!(PkgManager::Dnf.remedy(Pkg::Tar), "sudo dnf install tar");
        assert_eq!(
            PkgManager::Pacman.remedy(Pkg::Coreutils),
            "sudo pacman -S coreutils"
        );
        assert_eq!(
            PkgManager::Brew.remedy(Pkg::E2fsprogs),
            "brew install e2fsprogs"
        );
        // The one entry that is not spelled the same everywhere.
        assert_eq!(
            PkgManager::Apt.remedy(Pkg::QemuUser),
            "sudo apt install qemu-user-static"
        );
        assert_eq!(
            PkgManager::Pacman.remedy(Pkg::QemuUser),
            "sudo pacman -S qemu-user-static-binfmt (AUR)"
        );
    }

    /// The user-namespace remedy is a diagnosis, not a list of guesses.
    ///
    /// It used to be a constant naming three sysctls and `usermod --add-subuids`. On a
    /// Debian host with a perfectly good `/etc/subuid` allocation but no `uidmap`
    /// package, every one of those four was already correct and the one missing thing
    /// went unnamed — so the operator fixed nothing and the check still failed. The
    /// text must now come from whichever classifier recognizes the host, and this
    /// asserts the two ends of that: a host that *can* map says so and does not block,
    /// and a host that cannot gets a non-empty explanation rather than a blank line.
    ///
    /// Host-dependent by nature, so it asserts the invariant on whichever branch this
    /// machine falls into — both are exercised across the project's build hosts.
    #[test]
    fn the_userns_remedy_explains_this_host() {
        assert!(
            !userns_blocker_detail().trim().is_empty(),
            "an unexplained refusal must still produce an actionable line"
        );
        let check = userns_check();
        match &check.status {
            CheckStatus::Present(detail) => {
                assert!(!check.is_blocking(), "a host that can map must not block");
                assert!(detail.contains("unshare"), "names the probe: {detail}");
            }
            CheckStatus::Missing(remedy) => {
                assert!(check.is_blocking(), "a host that cannot map must block");
                assert!(!remedy.trim().is_empty(), "a bare refusal helps nobody");
            }
        }
    }

    /// The RK1 shape: compiles a kernel, a bootloader and the media-accel stack, and
    /// assembles an image.
    fn compiling_build() -> ToolNeeds {
        ToolNeeds {
            target: Arch::Arm64,
            compiles: Some(std::env::temp_dir()),
            assembles_image: true,
        }
    }

    /// The C201 shape: Debian's kernel, the board's own firmware, no accel stack — so
    /// nothing is compiled from source at all.
    fn assembling_build() -> ToolNeeds {
        ToolNeeds {
            target: Arch::Armv7,
            compiles: None,
            assembles_image: true,
        }
    }

    /// The `deliverable = uboot` shape: compiles a bootloader and assembles no image.
    fn bootloader_build() -> ToolNeeds {
        ToolNeeds {
            target: Arch::Arm64,
            compiles: Some(std::env::temp_dir()),
            assembles_image: false,
        }
    }

    /// An x86_64 host, which executes neither arm64 nor armhf binaries — so every
    /// interpreter-gated check is emitted and the fixtures differ only in what they
    /// build. Fixed rather than detected, because a native-arm64 host answers
    /// differently and the assertions below are about the *needs*, not about the machine
    /// running the test.
    const X86: HostInfo = HostInfo {
        arch: "x86_64",
        os: "linux",
    };

    /// The names of the checks `needs` emits on [`X86`], in report order, with the two
    /// prose-worded checks reduced to a stable token.
    ///
    /// The user-namespace and binfmt checks name themselves in a sentence; an assertion
    /// that transcribed the sentence would be about the wording rather than about which
    /// requirements exist.
    fn check_names(needs: &ToolNeeds) -> Vec<String> {
        tool_checks_on(X86, needs)
            .into_iter()
            .map(|c| {
                if c.name.contains("user namespace") {
                    "user namespaces".to_string()
                } else if c.name.contains("binfmt") {
                    "binfmt".to_string()
                } else {
                    c.name
                }
            })
            .collect()
    }

    /// The whole of what a build that compiles asks of its host, asserted as a set.
    ///
    /// A membership test would pass while a retired requirement quietly came back, and
    /// the set is small enough for the exact list to be the assertion — so this names
    /// every check the most demanding build emits, and a new one has to be added here
    /// deliberately.
    #[test]
    fn a_compiling_build_asks_for_git_a_namespace_and_an_overlay() {
        assert_eq!(
            check_names(&compiling_build()),
            [
                "git",
                "user namespaces",
                "unprivileged overlay",
                "tar",
                "cp",
                "qemu-aarch64-static",
                "binfmt",
                "e2fsck",
            ],
            "the host requirement list is a documented claim; a new entry belongs in \
             docs/src/getting-started.md too"
        );
    }

    #[test]
    fn no_build_asks_for_a_compiler_or_a_packaging_tool() {
        // "Your host supplies no compiler and no packaging tool" is a claim about an
        // absence, so it is asserted as one: every compiler, packaging tool and
        // build-dependency is a package of a provisioned root, resolved from the build's
        // own mirror list and sha256-pinned in that root's manifest.
        //
        // Worth a test of its own because the failure mode is silent. A host requirement
        // can return without anyone adding a check for it — a tool the build invokes
        // execs another, and the second one is only reached at the end of a 30-minute
        // compile. `make bindeb-pkg` ending in `dh_builddeb` execing `dpkg-deb` is the
        // shape of it. Naming each absent tool is what makes that visible here rather
        // than on a fresh host.
        //
        // Asserted against the build that compiles the most: a kernel, u-boot,
        // out-of-tree modules and the media-accel stack.
        let checks = tool_checks(&compiling_build());
        // The compile toolchain and the kernel's and u-boot's generators; then `make
        // bindeb-pkg`'s chain at both the depths it can fail at; then u-boot's
        // pylibfdt/binman set; and last the wrapper that never did anything.
        for gone in [
            "cc",
            "gcc",
            "make",
            "bc",
            "flex",
            "bison",
            "libssl",
            "dpkg-buildpackage",
            "dpkg-deb",
            "dh",
            "libelf",
            "libdw",
            "rsync",
            "cpio",
            "depmod",
            "swig",
            "python3",
            "setuptools",
            "pyelftools",
            "fakeroot",
        ] {
            assert!(
                !checks.iter().any(|c| c.name.contains(gone)),
                "{gone} is a package of a provisioned root, not a host requirement"
            );
        }
        // Every remaining check is a hard requirement but one: `e2fsck` is the optional
        // cross-check of a filesystem ferrosys already scanned in-process.
        assert!(
            checks
                .iter()
                .filter(|c| c.name != "e2fsck")
                .all(|c| c.required),
            "only e2fsck is optional"
        );
        let e2fsck = checks
            .iter()
            .find(|c| c.name == "e2fsck")
            .expect("e2fsck check present");
        assert!(!e2fsck.required, "e2fsck is an optional cross-check");
    }

    #[test]
    fn an_image_build_asks_for_the_two_tools_the_image_path_shells_out_to() {
        // `doctor`'s contract is what *this build* will invoke, not what a Linux host
        // usually has. Both are effectively universal, which is why they are the two
        // easiest to omit — and a trimmed container image without coreutils would
        // otherwise pass preflight and fail mid-rootfs.
        let checks = tool_checks(&assembling_build());
        for needed in ["tar", "cp"] {
            assert!(
                checks.iter().any(|c| c.name == needed && c.required),
                "an image build must require {needed}"
            );
        }
        // A u-boot-only deliverable assembles no rootfs and stages no overlay tree.
        for absent in ["tar", "cp"] {
            assert!(
                !tool_checks(&bootloader_build())
                    .iter()
                    .any(|c| c.name == absent),
                "{absent} is an image-path tool; a u-boot-only build should not ask for it"
            );
        }
    }

    #[test]
    fn the_provisioned_roots_add_no_external_tool() {
        // Every root a build stands up is bootstrapped and entered through the
        // in-process ferroday-cage library rather than an external sandbox binary, so
        // none of them asks for `bwrap`/`bubblewrap` — only the unprivileged user
        // namespaces every build already requires.
        let checks = tool_checks(&compiling_build());
        assert!(
            !checks
                .iter()
                .any(|c| c.name == "bwrap" || c.name == "bubblewrap"),
            "the in-process cage needs no external sandbox binary"
        );
        assert!(
            checks
                .iter()
                .any(|c| c.name.contains("user namespace") && c.required),
            "every build requires unprivileged user namespaces"
        );
        // The overlay each compile root layers its stage's build-deps on is the one host
        // capability beyond those namespaces, and it belongs to compiling rather than to
        // the sandbox: a bootloader-only build layers u-boot's deps and needs it too.
        for compiles in [compiling_build(), bootloader_build()] {
            assert!(
                tool_checks(&compiles)
                    .iter()
                    .any(|c| c.name == "unprivileged overlay" && c.required),
                "a build that compiles layers a build root over an overlay"
            );
        }
        assert!(
            !tool_checks(&assembling_build())
                .iter()
                .any(|c| c.name == "unprivileged overlay"),
            "a build that compiles nothing stands up no build root"
        );
    }

    #[test]
    fn an_interpreter_is_a_requirement_of_the_image_path_alone() {
        // qemu-user answers two questions at once, and both have to hold. *Can* the host
        // execute the target's binaries, and *does* this build ask it to — which only
        // the image path does: the OS rootfs runs the target's maintainer scripts and
        // the media-accel `.deb`s compile in a target-arch sandbox. The cross root and
        // the packaging root are the host's own architecture.
        let image = check_names(&compiling_build());
        assert!(image.iter().any(|n| n == "qemu-aarch64-static"));
        assert!(image.iter().any(|n| n == "binfmt"));

        // Same host, same foreign target, no image: nothing arm64 is ever executed.
        let loader = check_names(&bootloader_build());
        assert!(
            !loader.iter().any(|n| n.contains("qemu") || n == "binfmt"),
            "a bootloader-only build compiles in a host-arch root and runs nothing \
             foreign: {loader:?}"
        );

        // The one host/target pair where "cross" and "interpreted" disagree, and the
        // reason the predicate is `needs_interpreter` rather than "is this build cross".
        // An arm64 kernel with CONFIG_COMPAT=y runs armhf binaries natively, so an arm64
        // host assembling an armhf image consults no binfmt handler at all.
        //
        // Asserted against an explicit host rather than the running one: no CI machine
        // here is arm64, and reporting a blocking qemu requirement on the user's only
        // native-arm64 box is exactly the noise ToolNeeds exists to eliminate.
        let arm64 = HostInfo {
            arch: "aarch64",
            os: "linux",
        };
        let armhf = tool_checks_on(arm64, &assembling_build());
        assert!(
            !armhf
                .iter()
                .any(|c| c.name.contains("qemu") || c.name.contains("binfmt")),
            "an arm64 host runs armhf binaries directly: {:?}",
            armhf.iter().map(|c| &c.name).collect::<Vec<_>>()
        );
    }

    #[test]
    fn a_build_that_compiles_nothing_asks_for_neither_git_nor_an_overlay() {
        // The payoff of a needs-driven list: this board installs Debian's kernel and
        // boots its own firmware, so it clones nothing and layers nothing. Naming those
        // requirements anyway would be noise a genuinely missing tool could hide in.
        //
        // It is also the case that makes "boot2deb needs no dpkg-family host" a plain
        // claim rather than a technicality: for this recipe the whole host list is
        // `tar`, `cp`, user namespaces, and — on a host that cannot run armhf — qemu.
        assert_eq!(
            check_names(&assembling_build()),
            [
                "user namespaces",
                "tar",
                "cp",
                "qemu-arm-static",
                "binfmt",
                "e2fsck",
            ]
        );
    }

    #[test]
    fn which_finds_a_known_tool_and_misses_a_bogus_one() {
        // `sh` exists on every unix test host; a random name does not.
        assert!(which("sh").is_some());
        assert!(which("boot2deb-definitely-not-a-real-binary").is_none());
    }
}
