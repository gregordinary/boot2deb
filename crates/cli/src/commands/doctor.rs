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
    let checks = boot2deb_engine::checks::tool_checks(build.arch, &build.cross_compile);
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
    println!();
    if blocking == 0 {
        println!("result    : all required host tools present");
        Ok(())
    } else {
        Err(format!("{blocking} required host tool(s) missing — install them before building").into())
    }
}
