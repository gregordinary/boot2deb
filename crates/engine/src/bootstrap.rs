//! Archive parameters shared by the cross sandbox ([`crate::sandbox`]) and the
//! rootfs node ([`crate::rootfs`]).
//!
//! Both stand up a Debian userland with ferroday-cage's provisioner, so both draw
//! from the same mirror and the same component set. The values live here rather
//! than in either caller because a build in which the sandbox and the image
//! resolved against different archives would be silently inconsistent.

/// Default Debian mirror both bootstraps pull from — also the base mirror the
/// rootfs node's snapshot resolution ([`crate::snapshot`]) layers a snapshot
/// mirror onto, re-exported at the crate root as [`crate::DEFAULT_MIRROR`].
///
/// Plain `http://` is standard Debian practice: integrity comes from
/// `Release`-signature verification against the vendored archive keyring, not
/// the transport, so a tampering mirror or on-path attacker can at worst
/// observe which packages are fetched or deny service — never alter what is
/// installed.
pub const DEFAULT_MIRROR: &str = "http://deb.debian.org/debian";

/// Debian archive components enabled in the cross sandbox: `non-free`/
/// `non-free-firmware` carry the codecs (`libfdk-aac-dev`) and firmware the accel
/// stack and NICs need; `contrib` rounds out the standard device set. The
/// provisioner enables only `main` unless told otherwise, so this is passed
/// explicitly.
///
/// Also the image's set on every build but a libre one — see [`components`].
pub(crate) const COMPONENTS: &str = "main,contrib,non-free,non-free-firmware";

/// The components a [`libre`](boot2deb_core::ResolvedBuild::libre) image is
/// provisioned from and boots with — Debian's free archive, and nothing else.
pub(crate) const FREE_COMPONENTS: &str = "main";

/// The archive components *this image* resolves against: [`FREE_COMPONENTS`] for a
/// [`libre`](boot2deb_core::ResolvedBuild::libre) build, [`COMPONENTS`] otherwise.
///
/// It is one answer for two questions that must not diverge — what the rootfs solve
/// may install, and what the booted image's `sources.list` offers — so a package
/// `apt` could not have pulled at build time is also one it will not offer to pull
/// later. The **cross sandbox** deliberately does not consult this: it is a build
/// environment that is never shipped, and narrowing the toolchain it draws from
/// would change what a libre image is *compiled by* without changing a byte of what
/// is in it.
pub(crate) fn components(build: &boot2deb_core::ResolvedBuild) -> &'static str {
    if build.libre {
        FREE_COMPONENTS
    } else {
        COMPONENTS
    }
}
