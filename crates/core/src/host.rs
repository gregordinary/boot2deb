//! Host detection for preflight (`doctor`). The build host may be x86_64
//! or arm64 Linux (or a non-Linux client); cross-arch builds need qemu-user.

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
    pub fn as_target_arch(&self) -> Option<Arch> {
        match self.arch {
            "aarch64" => Some(Arch::Arm64),
            "arm" | "armv7" => Some(Arch::Armv7),
            "riscv64" => Some(Arch::Riscv64),
            _ => None,
        }
    }

    /// True when building `target` on this host needs qemu-user binfmt:
    /// the host arch differs from (or can't run) the target arch.
    pub fn is_cross_for(&self, target: Arch) -> bool {
        self.as_target_arch() != Some(target)
    }

    /// Builds require a Linux host (privileged loop/bootstrap/qemu work).
    pub fn is_linux(&self) -> bool {
        self.os == "linux"
    }
}
