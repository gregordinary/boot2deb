//! `doctor`: host preflight — arch/OS facts, whether a target build is cross-arch,
//! and the presence of every host tool the build needs.
//!
//! A bare `doctor` reports host facts only and reads no config root; with a target it
//! resolves the build to know which toolchain the checks apply to. Missing *required*
//! tools are a non-zero exit, so it doubles as a CI gate.

use crate::config::resolve;
use crate::workdir::work_dir_for;
use boot2deb_core::model::Overrides;
use boot2deb_core::ConfigRoot;
use boot2deb_engine::checks::CheckStatus;

/// Run `doctor [target]`.
pub(crate) fn run(
    root: &ConfigRoot,
    target: Option<String>,
    work_dir: Option<std::path::PathBuf>,
    overrides: Overrides,
) -> Result<(), Box<dyn std::error::Error>> {
    let host = boot2deb_core::HostInfo::detect();
    println!("host arch : {}", host.arch);
    println!("host os   : {}", host.os);
    if !host.is_linux() {
        println!("note      : builds require a Linux host; this is a client-only platform");
    }
    let Some(target) = target else {
        return Ok(());
    };
    let build = resolve(root, &target, overrides)?;
    let pf = boot2deb_engine::preflight(build.arch);
    println!("target    : {target} (arch {})", build.arch);
    // Two lines, not one, because the two answers come apart: an arm64 host building
    // armhf needs the cross toolchain and needs no qemu at all (CONFIG_COMPAT=y runs
    // those binaries natively). Reporting them as one "cross: yes" told that host to
    // install an interpreter its build never invokes.
    println!(
        "toolchain : {}",
        if pf.cross_toolchain {
            format!(
                "cross — {} cannot be emitted by this host's native cc",
                build.arch
            )
        } else {
            format!("native — this host's cc emits {}", build.arch)
        }
    );
    println!(
        "execution : {}",
        if pf.interpreter {
            format!(
                "emulated — needs qemu-user binfmt for {} maintainer scripts/compiles",
                build.arch
            )
        } else {
            format!("native — this host runs {} binaries directly", build.arch)
        }
    );

    // Tool-presence preflight: report each requirement with its path or a
    // host-specific install hint, then fail if any required tool is missing.
    println!();
    // Ask only for what this build will actually invoke. A board that installs
    // Debian's kernel and boots its own firmware compiles nothing, so listing a cross
    // compiler among its requirements would be noise a real missing tool could hide in.
    let needs = boot2deb_engine::checks::ToolNeeds {
        target: build.arch,
        cross_compile: build.cross_compile.clone(),
        compiles_sources: build.compiles_kernel() || build.rkbin_boot().is_some(),
        compiles_kernel: build.compiles_kernel(),
        builds_uboot: build.rkbin_boot().is_some(),
        // The media-accel `.deb`s are the only packages compiled in the target-arch
        // sandbox, so they are the only stages that acquire a build root. Probe the
        // filesystem the build would actually put its overlay uppers on, which is the
        // work dir's — not `/tmp`'s, which can answer differently.
        // `cp` and `tar` are only invoked on the image path — the overlay staging and
        // the rootfs-tar verification. A u-boot-only deliverable emits payloads and
        // asks for neither.
        assembles_image: build.produces_image(),
        build_root_uppers: build.userspace.is_some().then(|| {
            boot2deb_engine::sandbox::build_root_uppers(&work_dir_for(root, &target, work_dir))
        }),
    };
    let checks = boot2deb_engine::checks::tool_checks(&needs);
    let mut blocking = 0usize;
    for c in &checks {
        match &c.status {
            CheckStatus::Present(detail) => {
                println!("  ok      {:<28} {}", c.name, detail);
            }
            CheckStatus::Missing(remedy) => {
                let tag = if c.required { "MISSING " } else { "absent  " };
                println!("  {tag}{:<28} {} — {}", c.name, c.purpose, remedy);
                if c.is_blocking() {
                    blocking += 1;
                }
            }
        }
    }
    // A native kernel build carries one prerequisite these checks cannot see. `make
    // bindeb-pkg` runs `dpkg-checkbuilddeps` against the kernel's generated
    // debian/control, and that consults the *dpkg database* — while everything above
    // is a PATH scan and `pkg-config`. Those are different oracles: a host where
    // `libelf-dev` or `debhelper` is present but not dpkg-registered passes every
    // check here and then fails the build minutes in. Cross builds pass `DPKG_FLAGS=-d`
    // and skip the gate entirely, so this is the native path's alone.
    //
    // Stated rather than probed: reproducing dpkg's own dependency solve would be a
    // second implementation of it, and the answer only matters on a host that installed
    // its build deps outside dpkg.
    if build.compiles_kernel() && !pf.cross_toolchain {
        println!();
        println!("note      : native kernel build — `make bindeb-pkg` runs");
        println!("            dpkg-checkbuilddeps against the dpkg database, which the");
        println!("            PATH and pkg-config probes above cannot speak for. On a");
        println!("            Debian/Ubuntu host that installed its build deps with apt");
        println!("            this is already satisfied; on one that installed them from");
        println!("            source, install the matching -dev packages so dpkg knows");
        println!("            about them too. Cross builds skip this check entirely.");
    }

    // Trust anchors: every keyring this build bootstraps against, and the vetted keys
    // each one carries. Printed in full rather than summarized — the point of the
    // fingerprint manifests is that whose keys you trust is something you can *see*,
    // and a preflight that only says "ok" would put that back behind a binary blob.
    println!();
    println!("trust anchors (apt keyrings verified against blobs/keyrings/*.fingerprints):");
    let mut anchors: Vec<std::path::PathBuf> = Vec::new();
    if let Some(archive) =
        root.find_trust_anchor("blobs/keyrings/debian-archive-keyring.gpg", false)?
    {
        anchors.push(archive);
    }
    for source in &build.apt_sources {
        if let Some(path) = root.find_asset(format!("blobs/keyrings/{}", source.signed_by)) {
            anchors.push(path);
        }
    }
    if anchors.is_empty() {
        println!("  none vendored — bootstrapping against the host's apt trust store");
    }
    for anchor in &anchors {
        let name = anchor.file_name().unwrap_or_default().to_string_lossy();
        // A keyring that fails its manifest is a blocking finding, not a printed
        // warning: doctor doubles as a CI gate, and an unvetted trust anchor is
        // exactly the thing that must not slip through one.
        let keys = boot2deb_engine::keyring::verify(anchor)?;
        println!("  ok      {name} — {} vetted key(s)", keys.len());
        for key in &keys {
            println!("            {key}");
        }
    }

    println!();
    if blocking == 0 {
        println!("result    : all required host tools present");
        Ok(())
    } else {
        Err(
            format!("{blocking} required host tool(s) missing — install them before building")
                .into(),
        )
    }
}
