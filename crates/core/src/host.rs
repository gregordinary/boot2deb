//! Host detection for preflight (`doctor`) and the build's cross decisions. The
//! build host may be x86_64 or arm64 Linux (or a non-Linux client).
//!
//! Two questions the host answers, and they are not the same question:
//! whether a **cross toolchain** is needed to *produce* target binaries, and whether a
//! qemu-user **interpreter** is needed to *run* them. See
//! [`needs_cross_toolchain`](HostInfo::needs_cross_toolchain) and
//! [`needs_interpreter`](HostInfo::needs_interpreter).

use crate::model::Arch;

/// Identity of the machine the process is running on, as compiled-in constants.
/// Used to decide whether a build is cross-arch and whether it can run here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HostInfo {
    /// `std::env::consts::ARCH` — e.g. "x86_64", "aarch64".
    pub arch: &'static str,
    /// `std::env::consts::OS` — e.g. "linux", "macos".
    pub os: &'static str,
}

impl HostInfo {
    /// Read the current host's arch and OS.
    pub fn detect() -> Self {
        Self {
            arch: std::env::consts::ARCH,
            os: std::env::consts::OS,
        }
    }

    /// The host arch expressed as one of our target [`Arch`]es, if it maps.
    ///
    /// 32-bit ARM Linux reports `"arm"`; `std::env::consts::ARCH` has no `"armv7"`
    /// value, so matching one would claim support for a string that cannot arrive.
    pub fn as_target_arch(&self) -> Option<Arch> {
        match self.arch {
            "aarch64" => Some(Arch::Arm64),
            "arm" => Some(Arch::Armv7),
            "riscv64" => Some(Arch::Riscv64),
            _ => None,
        }
    }

    /// True when producing `target` binaries needs a cross toolchain — a compiler built
    /// for this host's architecture cannot emit them, so the compile root is given
    /// `crossbuild-essential-<target>` and the compile is passed `CROSS_COMPILE`.
    ///
    /// About the *compile*, not about the host's own tooling: no build invokes a compiler
    /// from the host at all, so this decides which package a provisioned root carries
    /// rather than which package the operator installs.
    ///
    /// Strict arch equality: an aarch64 compiler cannot emit armhf even though the same
    /// CPU executes armhf fine.
    pub fn needs_cross_toolchain(&self, target: Arch) -> bool {
        self.as_target_arch() != Some(target)
    }

    /// True when *running* a `target` binary on this host needs a qemu-user
    /// interpreter and a registered binfmt handler.
    ///
    /// This is the *execute* half, and it is strictly weaker than
    /// [`needs_cross_toolchain`](Self::needs_cross_toolchain): an arm64 host runs
    /// armhf binaries natively, because every Debian arm64 kernel is built with
    /// `CONFIG_COMPAT=y`. So an arm64 host building an armhf (RK3288/Veyron) image
    /// genuinely compiles through `arm-linux-gnueabihf-` and genuinely needs no
    /// `qemu-arm-static` — the one case where the two answers differ.
    ///
    /// Conflating them made `doctor` report qemu-user and the arm binfmt handler as
    /// *blocking* on such a host, for tooling the build never invokes. Necessary but not
    /// sufficient on its own: an interpreter is a requirement only of a build that
    /// actually enters a target-arch root, which is the image path.
    pub fn needs_interpreter(&self, target: Arch) -> bool {
        match self.as_target_arch() {
            Some(host) => !runs_natively(host, target),
            // A host arch we do not model (or a non-Linux one) is assumed unable to
            // execute the target: the fail-safe direction, since a spurious qemu
            // requirement is a visible complaint while a missing one is a build that
            // dies inside the sandbox.
            None => true,
        }
    }

    /// The Debian architecture name of the machine itself, or `None` for a host arch
    /// this does not model.
    ///
    /// Distinct from [`as_target_arch`](Self::as_target_arch), which asks whether the
    /// host is a *target* boot2deb builds images for. It is not the same question: the
    /// overwhelmingly common build host is `x86_64`, which is no board's architecture
    /// and yet is a perfectly good `amd64` for the packaging root
    /// ([`PackagingSandbox`](../../boot2deb_engine/sandbox/struct.PackagingSandbox.html))
    /// to be provisioned at, since archiving a `.deb` is arch-independent work.
    ///
    /// `None` rather than a guess: the name reaches an archive as the architecture a
    /// root is provisioned for, and a wrong one there resolves the wrong packages
    /// rather than failing.
    pub fn debian_arch(&self) -> Option<&'static str> {
        match self.arch {
            "x86_64" => Some("amd64"),
            "aarch64" => Some("arm64"),
            // 32-bit ARM Linux reports `arm`; Debian's hard-float port is `armhf`,
            // which is the only 32-bit ARM port in the archive that matters here.
            "arm" => Some("armhf"),
            "riscv64" => Some("riscv64"),
            _ => None,
        }
    }

    /// Builds require a Linux host (privileged loop/bootstrap/qemu work).
    pub fn is_linux(&self) -> bool {
        self.os == "linux"
    }
}

/// True when a `host`-arch CPU executes `target`-arch userspace with no interpreter.
///
/// Beyond the identity case there is exactly one: arm64 executing armhf, via the
/// AArch32 execution state that `CONFIG_COMPAT=y` exposes. x86_64 does *not* get an
/// entry for i386 — no target uses it, and adding an unexercised one would be a claim
/// nothing here can check.
fn runs_natively(host: Arch, target: Arch) -> bool {
    host == target || (host == Arch::Arm64 && target == Arch::Armv7)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A host at an arbitrary arch, for the (host, target) matrix.
    fn host(arch: &'static str) -> HostInfo {
        HostInfo { arch, os: "linux" }
    }

    #[test]
    fn the_two_predicates_agree_except_where_arm64_runs_armhf() {
        // The whole matrix, stated once: (host arch, target, toolchain?, interpreter?).
        let matrix = [
            ("x86_64", Arch::Arm64, true, true),
            ("x86_64", Arch::Armv7, true, true),
            ("x86_64", Arch::Riscv64, true, true),
            ("aarch64", Arch::Arm64, false, false),
            // The case the two answers come apart on: an aarch64 gcc cannot emit
            // armhf, but an arm64 kernel with CONFIG_COMPAT=y runs armhf binaries.
            ("aarch64", Arch::Armv7, true, false),
            ("aarch64", Arch::Riscv64, true, true),
            ("arm", Arch::Armv7, false, false),
            ("arm", Arch::Arm64, true, true),
            ("riscv64", Arch::Riscv64, false, false),
        ];
        for (arch, target, toolchain, interpreter) in matrix {
            let h = host(arch);
            assert_eq!(
                h.needs_cross_toolchain(target),
                toolchain,
                "{arch} -> {target:?} toolchain"
            );
            assert_eq!(
                h.needs_interpreter(target),
                interpreter,
                "{arch} -> {target:?} interpreter"
            );
        }
    }

    #[test]
    fn an_unmodelled_host_arch_is_assumed_to_need_both() {
        // Fail-safe: a spurious qemu requirement is a visible complaint, a missing one
        // is a build that dies inside the sandbox.
        let h = host("powerpc64");
        assert_eq!(h.as_target_arch(), None);
        for target in [Arch::Arm64, Arch::Armv7, Arch::Riscv64] {
            assert!(h.needs_cross_toolchain(target));
            assert!(h.needs_interpreter(target));
        }
    }

    #[test]
    fn thirty_two_bit_arm_linux_is_spelled_arm() {
        // `std::env::consts::ARCH` has no "armv7" value, so matching one would claim
        // support for a string that cannot arrive.
        assert_eq!(host("arm").as_target_arch(), Some(Arch::Armv7));
        assert_eq!(host("armv7").as_target_arch(), None);
    }

    /// The host's own Debian architecture is a different question from whether the host
    /// is a target: the usual build host is `amd64`, which is no board's arch.
    #[test]
    fn a_host_names_its_own_debian_architecture_target_or_not() {
        let amd64 = host("x86_64");
        assert_eq!(amd64.debian_arch(), Some("amd64"));
        assert_eq!(
            amd64.as_target_arch(),
            None,
            "no board is amd64, and it still has a Debian arch"
        );

        // Where the host *is* a target, the two agree on the archive's spelling —
        // including 32-bit ARM, whose Debian port is `armhf` rather than the `arm` the
        // host reports or the `armv7` the ISA is called.
        for (reported, deb, target) in [
            ("aarch64", "arm64", Arch::Arm64),
            ("arm", "armhf", Arch::Armv7),
            ("riscv64", "riscv64", Arch::Riscv64),
        ] {
            let h = host(reported);
            assert_eq!(h.debian_arch(), Some(deb));
            assert_eq!(h.as_target_arch().map(|a| a.debian_arch()), Some(deb));
            assert_eq!(h.as_target_arch(), Some(target));
        }

        // Unmodelled: no guess. The name reaches an archive as the architecture a root
        // is provisioned for, and a wrong one resolves rather than fails.
        assert_eq!(host("powerpc64").debian_arch(), None);
    }
}
