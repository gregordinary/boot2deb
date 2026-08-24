//! `doctor`: host preflight — arch/OS facts, whether a target build is cross-arch,
//! and the presence of every host tool the build needs.
//!
//! With a target it resolves the build to know which toolchain the checks apply to.
//! Bare, it runs the requirements no board can opt out of
//! ([`host_checks`](boot2deb_engine::checks::host_checks)) and says which answers are
//! waiting on a target, so the first command after a clone is useful with nothing else
//! typed. Missing *required* tools are a non-zero exit either way, so it doubles as a
//! CI gate — and `--json` renders the same verdict as one document for one to parse.

use crate::config::resolve;
use crate::workdir::work_dir_for;
use boot2deb_core::model::Overrides;
use boot2deb_core::ConfigRoot;
use boot2deb_engine::checks::{Check, CheckStatus};
use serde_json::json;

/// Run `doctor [target]`.
pub(crate) fn run(
    root: &ConfigRoot,
    target: Option<String>,
    work_dir: Option<std::path::PathBuf>,
    overrides: Overrides,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let host = boot2deb_core::HostInfo::detect();
    // The builder is a host fact too — which boot2deb is about to run, and whether it
    // still matches the checkout it would stamp images with. It belongs in the command
    // people type before a build, so the mismatch is something you read rather than
    // something you learn from a refused build.
    let freshness = crate::builder::freshness(root);
    let mut doc = json!({
        "host": { "arch": host.arch.to_string(), "os": host.os.to_string() },
        "builder": {
            "version": crate::builder::version(),
            "commit": crate::builder::commit(),
            "dirty": crate::builder::dirty(),
            "matches_checkout": freshness.note().is_none(),
        },
    });
    if !json {
        println!("host arch : {}", host.arch);
        println!("host os   : {}", host.os);
        println!("builder   : {}", crate::builder::identity());
        if let Some(line) = freshness.note() {
            println!("          ! {line}");
        }
        if !host.is_linux() {
            println!("note      : builds require a Linux host; this is a client-only platform");
        }
    }

    // No target: run what is answerable without one rather than stopping at two host
    // facts. Every build needs these, so a failure here is a failure for every board —
    // which is exactly what someone types `doctor` after a clone to find out.
    let Some(target) = target else {
        let checks = boot2deb_engine::checks::host_checks();
        let anchors = trust_anchors(root, &[])?;
        let blocking = report(&checks, &anchors, json, &mut doc)?;
        if !json {
            println!();
            println!("note      : these are the requirements every board shares. Host `git`,");
            println!("            the build-root overlay, the cross emulation and the image");
            println!("            path's tools depend on what you are building, so name a");
            println!("            target to have them checked too:");
            println!("              boot2deb doctor turing-rk1/forky");
            println!("            (`boot2deb list-recipes` shows what is available.)");
        }
        return finish(blocking, json, doc, "all shared host requirements present");
    };

    let build = resolve(root, &target, overrides)?;
    let pf = boot2deb_engine::preflight(build.arch);
    doc["target"] = json!({
        "reference": target,
        "arch": build.arch.to_string(),
        "cross_toolchain": pf.cross_toolchain,
        "interpreter": pf.interpreter,
    });
    if !json {
        println!("target    : {target} (arch {})", build.arch);
        // Two lines, not one, because the two answers come apart: an arm64 host building
        // armhf provisions a cross root and needs no qemu at all (CONFIG_COMPAT=y runs
        // those binaries natively). Reporting them as one "cross: yes" told that host to
        // install an interpreter its build never invokes.
        //
        // Neither line is a host *requirement* — both describe what the build will
        // provision. The toolchain is a package of the cross root either way; what
        // differs is which package.
        println!(
            "toolchain : {}",
            if pf.cross_toolchain {
                format!(
                    "cross — the build root carries a toolchain emitting {}",
                    build.arch
                )
            } else {
                format!("native — the build root compiles {} directly", build.arch)
            }
        );
        // Three answers, not two, because "can this host run the target's binaries" and
        // "does this build ask it to" are different questions and a bootloader-only
        // deliverable answers no to the second whatever the first says: it compiles in a
        // host-arch cross root and archives in a host-arch packaging root, so nothing
        // foreign is ever executed. Saying "emulated" there would name a requirement
        // this build does not have.
        println!(
            "execution : {}",
            match (pf.interpreter, build.produces_image()) {
                (true, true) => format!(
                    "emulated — needs qemu-user binfmt for {} maintainer scripts and \
                     sandbox compiles",
                    build.arch
                ),
                (true, false) => format!(
                    "none — this host cannot run {} binaries, and this build runs none: \
                     it compiles and archives in host-arch roots",
                    build.arch
                ),
                (false, _) => format!("native — this host runs {} binaries directly", build.arch),
            }
        );
    }

    // Ask only for what this build will actually invoke. A board that installs
    // Debian's kernel and boots its own firmware compiles nothing, so listing `git`
    // among its requirements would be noise a real missing tool could hide in.
    let needs = boot2deb_engine::checks::ToolNeeds {
        target: build.arch,
        // Every stage that compiles layers its build-deps over a provisioned root, so
        // this is one question for every compile node rather than one per node. Probe
        // the filesystem the build would actually put its overlay uppers on, which is
        // the work dir's — not `/tmp`'s, which can answer differently.
        compiles: build.compiles_from_source().then(|| {
            boot2deb_engine::sandbox::build_root_uppers(&work_dir_for(root, &target, work_dir))
        }),
        // `cp` and `tar` are only invoked on the image path — the overlay staging and
        // the rootfs-tar verification — and it is also the only path that enters a
        // target-arch root, which is what makes qemu a requirement. A u-boot-only
        // deliverable emits payloads and asks for none of the three.
        assembles_image: build.produces_image(),
    };
    let checks = boot2deb_engine::checks::tool_checks(&needs);
    let anchors = trust_anchors(root, &build.apt_sources)?;
    let blocking = report(&checks, &anchors, json, &mut doc)?;

    // Every check above is a probe of the host, and that is the whole list: a build
    // dependency is not on it, because it is resolved into a provisioned root from the
    // build's own mirror list. One the lock cannot satisfy fails when the provisioner
    // cannot resolve it — before `make` starts, with the package named — rather than
    // needing a preflight here to guess at it.
    finish(blocking, json, doc, "all required host tools present")
}

/// One verified trust anchor: the keyring file's name and the vetted keys it carries.
type Anchor = (String, Vec<boot2deb_engine::keyring::Key>);

/// Verify every apt keyring this run bootstraps against — the Debian archive keyring
/// plus one per third-party source — and return each with its vetted keys.
///
/// A keyring that fails its fingerprint manifest, or that resolves outside the
/// vendored keyring directory, is an error, not a printed warning: `doctor` doubles
/// as a CI gate, and an unvetted trust anchor is exactly the thing that must not slip
/// through one. A keyring that is simply *absent* is skipped instead, because it is
/// not a trust anchor at all — `preflight_config` fails the build on it, naming the
/// source that wanted it.
fn trust_anchors(
    root: &ConfigRoot,
    apt_sources: &[boot2deb_core::model::AptSource],
) -> Result<Vec<Anchor>, Box<dyn std::error::Error>> {
    let mut paths: Vec<std::path::PathBuf> = Vec::new();
    if let Some(archive) =
        root.find_trust_anchor("blobs/keyrings/debian-archive-keyring.gpg", false)?
    {
        paths.push(archive);
    }
    for source in apt_sources {
        if let Some(path) = crate::config::apt_source_keyring(root, &source.signed_by)? {
            paths.push(path);
        }
    }
    let mut out = Vec::with_capacity(paths.len());
    for path in paths {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        out.push((name, boot2deb_engine::keyring::verify(&path)?));
    }
    Ok(out)
}

/// Render the checks and trust anchors — to the terminal, or into `doc` under
/// `--json` — and return how many required checks are unsatisfied.
///
/// The anchors are printed in full rather than summarized: the point of the
/// fingerprint manifests is that *whose keys you trust* is something you can see, and
/// a preflight that only said "ok" would put that back behind a binary blob.
fn report(
    checks: &[Check],
    anchors: &[Anchor],
    json: bool,
    doc: &mut serde_json::Value,
) -> Result<usize, Box<dyn std::error::Error>> {
    let blocking = checks.iter().filter(|c| c.is_blocking()).count();
    if json {
        doc["checks"] = checks
            .iter()
            .map(|c| match &c.status {
                CheckStatus::Present(detail) => json!({
                    "name": c.name, "purpose": c.purpose, "required": c.required,
                    "status": "present", "detail": detail,
                }),
                CheckStatus::Missing(remedy) => json!({
                    "name": c.name, "purpose": c.purpose, "required": c.required,
                    "status": "missing", "remedy": remedy,
                }),
            })
            .collect();
        doc["trust_anchors"] = anchors
            .iter()
            .map(|(name, keys)| {
                json!({
                    "keyring": name,
                    "keys": keys.iter().map(|k| json!({
                        "fingerprint": k.fingerprint, "label": k.label,
                    })).collect::<Vec<_>>(),
                })
            })
            .collect();
        return Ok(blocking);
    }
    println!();
    for c in checks {
        match &c.status {
            CheckStatus::Present(detail) => println!("  ok      {:<28} {}", c.name, detail),
            CheckStatus::Missing(remedy) => {
                let tag = if c.required { "MISSING " } else { "absent  " };
                println!("  {tag}{:<28} {} — {}", c.name, c.purpose, remedy);
            }
        }
    }
    println!();
    println!("trust anchors (apt keyrings verified against blobs/keyrings/*.fingerprints):");
    if anchors.is_empty() {
        println!("  none vendored — bootstrapping against the host's apt trust store");
    }
    for (name, keys) in anchors {
        println!("  ok      {name} — {} vetted key(s)", keys.len());
        for key in keys {
            println!("            {key}");
        }
    }
    Ok(blocking)
}

/// Emit the verdict and turn it into the process result: a missing required tool is a
/// non-zero exit whichever renderer ran, so a CI gate reads the same either way.
fn finish(
    blocking: usize,
    json: bool,
    mut doc: serde_json::Value,
    pass_line: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    doc["blocking"] = json!(blocking);
    doc["result"] = json!(if blocking == 0 { "pass" } else { "fail" });
    if json {
        println!("{}", serde_json::to_string_pretty(&doc)?);
    } else {
        println!();
        if blocking == 0 {
            println!("result    : {pass_line}");
        }
    }
    if blocking == 0 {
        Ok(())
    } else {
        Err(
            format!("{blocking} required host tool(s) missing — install them before building")
                .into(),
        )
    }
}
