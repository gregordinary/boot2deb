//! `doctor`: host preflight — arch/OS facts, whether a target build is cross-arch,
//! and the presence of every host tool the build needs.
//!
//! A bare `doctor` reports host facts only and reads no config root; with a target it
//! resolves the build to know which toolchain the checks apply to. Missing *required*
//! tools are a non-zero exit, so it doubles as a CI gate.

use crate::config::resolve;
use boot2deb_core::model::Overrides;
use boot2deb_core::ConfigRoot;
use boot2deb_engine::checks::CheckStatus;

/// Run `doctor [target]`.
pub(crate) fn run(
    root: &ConfigRoot,
    target: Option<String>,
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
    if pf.cross {
        println!(
            "cross     : yes — needs qemu-user binfmt for {} maintainer scripts/compiles",
            build.arch
        );
    } else {
        println!("cross     : no — native {} build, no qemu-user needed", build.arch);
    }

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
        sandbox_builds: build.userspace.is_some(),
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
    // Trust anchors: every keyring this build bootstraps against, and the vetted keys
    // each one carries. Printed in full rather than summarized — the point of the
    // fingerprint manifests is that whose keys you trust is something you can *see*,
    // and a preflight that only says "ok" would put that back behind a binary blob.
    println!();
    println!("trust anchors (apt keyrings verified against blobs/keyrings/*.fingerprints):");
    let mut anchors: Vec<std::path::PathBuf> = Vec::new();
    if let Some(archive) = root.find_trust_anchor("blobs/keyrings/debian-archive-keyring.gpg", false)? {
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
        Err(format!("{blocking} required host tool(s) missing — install them before building").into())
    }
}
