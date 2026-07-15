//! boot2deb engine — executes on Linux, owns build side effects, and emits the
//! structured event stream.
//!
//! The lock-driven stages: lock resolution ([`pins`]), the patch verify-applies
//! gate ([`patches`]), and kernel-config generation + the parity check
//! ([`kconfig`]). Curating the series is [`patchimport`] (`patch import`): it fetches
//! a patch, normalizes it to canonical mbox (via [`boot2deb_core::mbox`]), and slots
//! it into a profile's ordered scope list. The compile steps run as subprocess stages — the [`build`] graph
//! nodes ([`build::kernel`], [`build::uboot`], [`build::userspace`],
//! [`build::ffmpeg`]) — reading the resolved lock and emitting the structured
//! [`event`] stream. Reuse of a cloned+patched kernel/u-boot tree is gated
//! by a Tier-1 [`signature`] stamp rather than bare directory existence, so a lock
//! bump rebuilds instead of silently building on a stale checkout; the same
//! stamps let [`plan`] (`why-rebuild`) explain, offline, why each compile node will
//! reuse or rebuild its tree. The userspace and ffmpeg `.deb`s cross-build inside a
//! [`sandbox`]: an arm64 userland bootstrapped and entered without root. All
//! of this is built on the shared [`git`] shell-outs, `make`/`merge_config.sh`,
//! and blob verification ([`blobs`]). The [`image`] node assembles the
//! bootable disk image without root — GPT and `.xz` in pure Rust, the ext4
//! rootfs via host `mke2fs -d` from a userns-staged tree. The [`repo`] module assembles the build's
//! `.deb`s into a local apt repo — including the pre-built `extra_debs` a
//! layer or feature pulls from outside the mirror, which [`extradebs`] materializes
//! into a content-addressed [`debstore`] and verifies against their sha256 pins
//! — and the [`rootfs`] node bootstraps the
//! device userland from it into the tarball the image node formats — with a unique
//! per-image first-boot password ([`secret`]). A cheap `mmdebstrap --simulate`
//! solve lets [`rootcache`] skip that bootstrap on an unchanged *solved* package set
//! (early cutoff) without ever reusing a stale solve. The rootfs bootstrap fetches
//! from the mirror list [`snapshot`] resolves (the live mirror, plus a
//! `snapshot.debian.org` mirror when a captured snapshot is activated), and its
//! solved package manifest is verified against the committed reproducibility pin by
//! [`manifest`]. Host preflight
//! for `doctor` — identity/cross status ([`preflight`]) plus tool-presence checks
//! with remediation ([`checks`]) — is also here.
#![warn(missing_docs)]

pub mod artstore;
pub mod blobs;
mod bootstrap;
pub mod build;
pub mod checks;
pub mod debstore;
pub mod error;
pub mod event;
pub mod extradebs;
pub mod gc;
pub mod git;
pub mod image;
pub mod kconfig;
pub mod keyring;
pub mod manifest;
pub mod netfetch;
pub mod patches;
pub mod patchfetch;
pub mod patchimport;
pub mod pins;
pub mod plan;
pub mod repo;
pub mod rootcache;
pub mod rootfs;
pub mod sandbox;
pub mod secret;
pub mod signature;
pub mod snapshot;
pub mod sources;
pub mod srcfetch;
pub mod toolchain;

pub use bootstrap::DEFAULT_MIRROR;
pub use error::EngineError;
pub use event::{Event, EventSink, Step, Stream};

/// Shared fixtures for the stage tests, so the resolved-RK1 build is defined once
/// rather than copied into each stage module.
#[cfg(test)]
pub(crate) mod test_support {
    use boot2deb_core::model::ResolvedBuild;
    use boot2deb_core::{resolve_recipe, ConfigRoot, Overrides};
    use std::path::PathBuf;

    /// The boot2deb repo root — two levels up from the engine crate manifest
    /// (`crates/engine` → `crates` → repo root), where the config layers live.
    pub(crate) fn repo_root() -> ConfigRoot {
        ConfigRoot::new(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(2)
                .unwrap()
                .to_path_buf(),
        )
    }

    /// The resolved `turing-rk1-media-accel-forky` build, for stage tests that need
    /// real device / offset / soc values *and* the media userspace + ffmpeg pins.
    pub(crate) fn rk1_build() -> ResolvedBuild {
        resolve_recipe(&repo_root(), "turing-rk1-media-accel-forky", &Overrides::default()).unwrap()
    }
}

use boot2deb_core::host::HostInfo;
use boot2deb_core::model::Arch;

/// Host identity + cross-arch status for a given target arch.
///
/// This is the coarse "can this host build this target at all" summary. The
/// concrete tool/capability checks (git/make/toolchain/qemu-user binfmt/
/// e2fsprogs, with remediation) are [`checks::tool_checks`].
#[derive(Debug, Clone)]
pub struct Preflight {
    /// Detected build host.
    pub host: HostInfo,
    /// The target architecture being built.
    pub target_arch: Arch,
    /// Cross-arch build → needs qemu-user binfmt on the host.
    pub cross: bool,
    /// Builds require a Linux host.
    pub host_is_linux: bool,
}

/// Detect the host and determine whether building `target_arch` is cross-arch.
pub fn preflight(target_arch: Arch) -> Preflight {
    let host = HostInfo::detect();
    Preflight {
        cross: host.is_cross_for(target_arch),
        host_is_linux: host.is_linux(),
        host,
        target_arch,
    }
}
