//! The provisioned roots a run stands up, and the trust anchor they bootstrap under.
//!
//! Two commands enter these roots — `build`, which compiles in them, and `shell`, which
//! opens a session in one — and they must land on the *same trees*. A root's directory
//! is keyed by its role, architecture, suite, mirror list and package set, and
//! `ensure_ready` reuses an existing directory without re-checking any of it. So a
//! second site that composed the key from, say, the image suite where this one composes
//! it from the packaging suite would not fail: it would quietly bootstrap a second tree
//! and open a session in a root the build never used.
//!
//! One definition of the three, here, is what makes that impossible.

use boot2deb_core::model::ResolvedBuild;
use boot2deb_core::ConfigRoot;
use boot2deb_engine::sandbox::{
    build_root_uppers, build_sandbox_dir, packaging_root_dir, BuildSandbox, PackagingSandbox,
    RootlessSandbox, SandboxRole, SandboxSpec,
};
use std::path::PathBuf;

/// The three roots a run can provision, standing by. None is bootstrapped here:
/// constructing one costs nothing, and each pays for its tree the first time something
/// calls `ensure_ready` on it.
pub(crate) struct Roots {
    /// The **target-arch** root the userspace and ffmpeg stages compile in — never the
    /// host, even where the architectures match. Those `.deb`s are packaged for the
    /// target suite and `dpkg-shlibdeps` derives each one's runtime `Depends` from the
    /// libraries present at build time, so compiling on the host would stamp the host's
    /// package names and versions into a `.deb` that then does not install on the board.
    ///
    /// `None` for a build that resolves no image suite and so has no suite to bootstrap
    /// one for — a `deliverable = uboot` build, whose stages never ask for it.
    pub(crate) target: Option<Box<dyn BuildSandbox>>,
    /// The **host-arch** root the kernel, u-boot and kmod stages compile in, carrying a
    /// cross toolchain that emits the target's objects — so a multi-minute kernel build
    /// runs natively and never passes through `qemu-user`.
    ///
    /// Unconditional: every deliverable compiles something. It reads `packaging_suite`
    /// rather than the image suite, so a bootloader-only build shares that board's tree
    /// with its image builds instead of standing up a second one.
    pub(crate) cross: RootlessSandbox,
    /// The **host-arch** root a staged tree becomes a `.deb` in. Unconditional for the
    /// same reason, and host-arch for a different one: `dpkg-deb` does not care what
    /// architecture the payload targets, so archiving runs natively.
    pub(crate) packaging: PackagingSandbox,
}

/// What the three roots are provisioned from — the values that key their trees, plus
/// the caches they share.
///
/// Passed as one value because they are read together and must be the same values at
/// every site: the mirror list above all, which is in each tree's key precisely because
/// `--snapshot pin` must move the *compiler* and not only the image's userland.
pub(crate) struct RootInputs<'a> {
    /// The build's scratch tree; every root lives under `<work_dir>/sandbox`.
    pub(crate) work_dir: &'a std::path::Path,
    /// The **host's** Debian architecture, which the cross and packaging roots are
    /// provisioned at so their commands run natively.
    pub(crate) host_deb_arch: &'a str,
    /// The build's own resolved mirror list, in order.
    pub(crate) mirrors: &'a [String],
    /// The Debian archive keyring verifying each bootstrap's `Release` signature, from
    /// [`keyring`]. `None` falls back to the host apt trust store.
    pub(crate) keyring: Option<PathBuf>,
    /// Where downloaded `.deb`s are cached, shared with the rootfs node's provisioner.
    pub(crate) deb_cache: PathBuf,
}

/// Construct the three roots for `build`.
pub(crate) fn roots(build: &ResolvedBuild, inputs: &RootInputs) -> Roots {
    let target = build.image.as_ref().map(|image| {
        let suite = &image.suite;
        let rootfs = build_sandbox_dir(
            inputs.work_dir,
            SandboxRole::Target,
            build.arch.debian_arch(),
            suite,
            inputs.mirrors,
        );
        Box::new(RootlessSandbox::new(
            SandboxRole::Target,
            SandboxSpec {
                rootfs,
                suite: suite.clone(),
                arch: build.arch.debian_arch().to_string(),
                mirrors: inputs.mirrors.to_vec(),
                keyring: inputs.keyring.clone(),
                cache_dir: Some(inputs.deb_cache.clone()),
            },
            // The same directory `doctor`'s overlay check probes, so a build root is
            // established where the host was cleared to establish one.
            build_root_uppers(inputs.work_dir),
        )) as Box<dyn BuildSandbox>
    });

    let cross_role = SandboxRole::Cross {
        target: build.arch.debian_arch(),
    };
    let cross = RootlessSandbox::new(
        cross_role,
        SandboxSpec {
            rootfs: build_sandbox_dir(
                inputs.work_dir,
                cross_role,
                inputs.host_deb_arch,
                &build.packaging_suite,
                inputs.mirrors,
            ),
            suite: build.packaging_suite.clone(),
            arch: inputs.host_deb_arch.to_string(),
            mirrors: inputs.mirrors.to_vec(),
            keyring: inputs.keyring.clone(),
            cache_dir: Some(inputs.deb_cache.clone()),
        },
        build_root_uppers(inputs.work_dir),
    );

    let packaging = PackagingSandbox::new(SandboxSpec {
        rootfs: packaging_root_dir(
            inputs.work_dir,
            inputs.host_deb_arch,
            &build.packaging_suite,
            inputs.mirrors,
        ),
        suite: build.packaging_suite.clone(),
        arch: inputs.host_deb_arch.to_string(),
        mirrors: inputs.mirrors.to_vec(),
        keyring: inputs.keyring.clone(),
        cache_dir: Some(inputs.deb_cache.clone()),
    });

    Roots {
        target,
        cross,
        packaging,
    }
}

/// The Debian archive keyring every bootstrap verifies its `Release` signature against:
/// the explicit `--keyring`, else the vendored anchor, else `None` (the host apt trust
/// store, which only exists on a Debian host).
///
/// A vendored keyring is additionally held to its fingerprint manifest. It decides whose
/// signatures a bootstrap accepts, and as a binary blob it is the one vendored file a
/// reviewer cannot read. An explicit `--keyring` is the operator's own anchor, chosen
/// deliberately, and is used as given.
///
/// `unsafe_overlay` allows an overlay to supply it; without that the anchor is resolved
/// from the primary root only, since an overlay copy would be a fail-closed swap.
pub(crate) fn keyring(
    root: &ConfigRoot,
    explicit: Option<PathBuf>,
    unsafe_overlay: bool,
) -> Result<Option<PathBuf>, Box<dyn std::error::Error>> {
    if let Some(explicit) = explicit {
        return Ok(Some(explicit));
    }
    let vendored =
        root.find_trust_anchor("blobs/keyrings/debian-archive-keyring.gpg", unsafe_overlay)?;
    if let Some(path) = &vendored {
        boot2deb_engine::keyring::verify(path)?;
    }
    Ok(vendored)
}

/// The host's Debian architecture, or a refusal naming why a build needs one.
///
/// Every root that is not the target's is provisioned for the host's architecture so
/// its commands run natively. A host this cannot name has no architecture to provision
/// one at, and every deliverable both compiles and packages — so the answer is owed up
/// front rather than at the first stage that needs a root.
pub(crate) fn host_deb_arch(
    preflight: &boot2deb_engine::Preflight,
) -> Result<&'static str, Box<dyn std::error::Error>> {
    preflight.host.debian_arch().ok_or_else(|| {
        format!(
            "cannot name a Debian architecture for this host ({}) — boot2deb \
             provisions host-arch roots to compile and archive in, and has no name \
             to provision one under",
            preflight.host.arch
        )
        .into()
    })
}
