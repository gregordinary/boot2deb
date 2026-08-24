//! `shell`: open an interactive session in the root a build stage compiles in.
//!
//! A thin client over [`boot2deb_engine::shell`], like every other command here. What
//! it owns is the part that is config: which build point's roots these are, which of
//! them the requested stage has, what outside the work dir the session should see, and
//! the one host value the sandbox environment deliberately does not carry — `TERM`.
//!
//! The roots come from [`crate::sandboxes`], the same construction `build` uses, so a
//! session enters the tree a build made rather than a second one keyed slightly
//! differently.

use crate::args::ShellArgs;
use crate::config::{device_dts_paths, fragment_paths};
use crate::fsutil::absolutize;
use crate::render::{print_event_at, Verbosity};
use crate::sandboxes::{host_deb_arch, keyring, roots, RootInputs};
use crate::workdir::mark_work_dir;
use boot2deb_core::lock::SnapshotMode;
use boot2deb_core::model::{Overrides, ResolvedBuild};
use boot2deb_core::{resolve_recipe, ConfigRoot};
use boot2deb_engine::event::Event;
use boot2deb_engine::shell::{ShellOptions, ShellRoots, ShellStage};
use std::path::PathBuf;

/// A session that ended non-zero, carried out of `main` as the process's own status.
///
/// `shell` is the one command whose result is a *command's* exit status rather than a
/// verdict of its own, and a script that wraps it reads that status. Carried as an
/// error so it travels the ordinary `Result` path, and recognized in
/// [`main`](crate::main) so it does not print as one: a shell exiting non-zero is what
/// the last command in it did, not a boot2deb failure.
#[derive(Debug)]
pub(crate) struct SessionExit(pub(crate) u8);

impl std::fmt::Display for SessionExit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the shell exited with status {}", self.0)
    }
}

impl std::error::Error for SessionExit {}

/// Run `shell <recipe> --stage <stage> [-- command...]`.
pub(crate) fn run(
    root: &ConfigRoot,
    recipe: &str,
    args: ShellArgs,
    verbosity: Verbosity,
) -> Result<(), Box<dyn std::error::Error>> {
    let point = crate::config::build_point(recipe, args.features.clone())?;
    let reference = point.reference();
    let recipe = reference.as_str();
    // A session stamps nothing, so a builder that predates the checkout is worth saying
    // and not worth refusing over — unlike a build, which would write that commit into
    // an image's provenance.
    if let Some(line) = crate::builder::freshness(root).note() {
        eprintln!("warning: {line}");
    }

    let lock = root.lock(recipe)?;
    let resolved = resolve_recipe(root, recipe, &Overrides::default())?;
    let stage = ShellStage::from(args.stage);
    ensure_build_has(&resolved, stage, recipe)?;

    // The mirror list keys every provisioned root, so it is resolved the way `build`
    // resolves it — the flag, else the lock's captured mode. A session opened under a
    // different mode would enter a different tree.
    let snapshot_mode = args
        .snapshot
        .or(lock.snapshot.as_ref().map(|s| s.mode))
        .unwrap_or(SnapshotMode::Off);
    let mirrors = boot2deb_engine::snapshot::resolve_mirrors(
        boot2deb_engine::DEFAULT_MIRROR,
        lock.snapshot.as_ref(),
        snapshot_mode,
    )?;

    let work_dir = crate::workdir::work_dir_for(root, recipe, args.work_dir);
    // The session's working directory and its bind, so it exists and is stamped before
    // anything enters it — a session opened before this recipe's first build is a valid
    // thing to want, and it starts in an empty work dir rather than failing.
    mark_work_dir(&work_dir)?;
    let out_dir = absolutize(args.out_dir.unwrap_or_else(|| work_dir.join("artifacts")));

    let pf = boot2deb_engine::preflight(resolved.arch);
    pf.ensure_can_build()?;
    let host_deb_arch = host_deb_arch(&pf)?;
    let inputs = RootInputs {
        work_dir: &work_dir,
        host_deb_arch,
        mirrors: &mirrors,
        keyring: keyring(root, args.keyring, false)?,
        deb_cache: work_dir.join("cache").join("provisioner-debs"),
    };
    let provisioned = roots(&resolved, &inputs);

    // What lives outside the work dir and a compile in this root still reads by absolute
    // path: the kernel's config fragments and the board's own device-tree sources, both
    // in the config root. Bound exactly as the kernel stage binds them — see
    // `ShellOptions::binds`.
    let mut binds = fragment_paths(root, &resolved)?;
    binds.extend(device_dts_paths(root, &resolved)?);
    let binds: Vec<PathBuf> = binds
        .into_iter()
        .map(|path| std::path::absolute(&path).unwrap_or(path))
        .collect();

    // The one host value the declared sandbox environment has no reason to carry: a
    // build has no terminal to describe, and a session does. Passed here rather than in
    // the shared profile, so the environment a compile runs in — and that the image's
    // provenance records — does not move.
    let env: Vec<(String, String)> = std::env::var("TERM")
        .ok()
        .filter(|term| !term.is_empty())
        .map(|term| vec![("TERM".to_string(), term)])
        .into_iter()
        .flatten()
        .collect();

    // The same narrowing `build` applies, so a session lands in the root that build
    // compiled in rather than one that resembles it.
    let userspace = crate::config::enabled_userspace(
        resolved
            .image
            .as_ref()
            .map(|i| i.userspace.as_slice())
            .unwrap_or(&[]),
        &args.userspace,
    )?;
    let sink = move |e: Event| print_event_at(verbosity, &e);
    let code = boot2deb_engine::shell::open(
        &resolved,
        &lock,
        &ShellOptions {
            stage,
            work_dir: &work_dir,
            out_dir: &out_dir,
            binds: &binds,
            argv: &args.command,
            env: &env,
            userspace: &userspace,
            // The cross root emits the target's objects → the compile is passed a
            // prefix; it *is* the target's architecture → there is none to pass. The
            // same question `build` answers, answered the same way.
            cross_compile: (host_deb_arch != resolved.arch.debian_arch())
                .then_some(resolved.cross_compile.as_str()),
        },
        &ShellRoots {
            target: provisioned.target.as_deref(),
            cross: &provisioned.cross,
            packaging: &provisioned.packaging,
        },
        &sink,
    )?;

    if code == 0 {
        Ok(())
    } else {
        Err(Box::new(SessionExit(code)))
    }
}

/// Refuse a stage this build does not have, naming why — the same gate `build` applies
/// to `--stage`, and for the same reason: a session in a root whose stage never runs
/// would provision a tree to look at nothing.
///
/// The packaging root is not gated: every deliverable archives something.
fn ensure_build_has(
    build: &ResolvedBuild,
    stage: ShellStage,
    recipe: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let refuse = |why: &str| -> Box<dyn std::error::Error> {
        format!(
            "recipe '{recipe}' has no {} stage: {why}. Its roots are the ones its \
             stages compile in, so there is none to enter.",
            stage.as_str()
        )
        .into()
    };
    match stage {
        ShellStage::Kernel if !build.compiles_kernel() => Err(refuse(
            "it installs Debian's kernel from the mirror rather than compiling one",
        )),
        ShellStage::Uboot if build.rkbin_boot().is_none() => Err(refuse(
            "this board's boot method builds no bootloader — its firmware is its own",
        )),
        ShellStage::Kmod
            if build
                .image
                .as_ref()
                .is_none_or(|i| i.device_kmods.is_empty()) =>
        {
            Err(refuse("this board declares no out-of-tree kernel modules"))
        }
        ShellStage::Userspace | ShellStage::Ffmpeg
            if build.image.as_ref().is_none_or(|i| i.userspace.is_empty()) =>
        {
            Err(refuse(
                "no selected feature requires the media-accel stack, so it compiles no \
                 userspace or ffmpeg packages",
            ))
        }
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::args::ShellStageArg;
    use crate::testsupport::repo_root;

    fn resolved(recipe: &str) -> ResolvedBuild {
        resolve_recipe(&repo_root(), recipe, &Overrides::default()).unwrap()
    }

    /// A session is refused for a stage the recipe has no root for, and the refusal
    /// names why rather than reporting a tree that could not be found. Asserted against
    /// two shipped recipes that differ in exactly this: the base RK1 recipe compiles no
    /// media-accel stack, the media-accel one does.
    #[test]
    fn a_stage_this_recipe_does_not_have_is_refused_with_the_reason() {
        let base = resolved("turing-rk1/forky");
        let media = resolved("turing-rk1/media-accel-forky");

        let refused = ensure_build_has(&base, ShellStage::Ffmpeg, "turing-rk1/forky")
            .expect_err("a base recipe compiles no ffmpeg");
        assert!(
            refused.to_string().contains("media-accel"),
            "the refusal names why: {refused}"
        );
        assert!(ensure_build_has(&base, ShellStage::Userspace, "r").is_err());
        assert!(ensure_build_has(&media, ShellStage::Userspace, "r").is_ok());
        assert!(ensure_build_has(&media, ShellStage::Ffmpeg, "r").is_ok());

        // Both compile a kernel and a bootloader, and every deliverable packages
        // something — so those roots are enterable on either.
        for build in [&base, &media] {
            assert!(ensure_build_has(build, ShellStage::Kernel, "r").is_ok());
            assert!(ensure_build_has(build, ShellStage::Uboot, "r").is_ok());
            assert!(ensure_build_has(build, ShellStage::Packaging, "r").is_ok());
        }
        // Neither declares an out-of-tree module, so neither has a kmod root.
        assert!(ensure_build_has(&base, ShellStage::Kmod, "r").is_err());
    }

    /// Every value of the flag names a distinct engine stage, so no two `--stage`
    /// choices land in one root by a mapping slip.
    #[test]
    fn every_stage_flag_names_a_distinct_root() {
        let stages = [
            ShellStageArg::Kernel,
            ShellStageArg::Uboot,
            ShellStageArg::Kmod,
            ShellStageArg::Userspace,
            ShellStageArg::Ffmpeg,
            ShellStageArg::Packaging,
        ];
        let mut named: Vec<&str> = stages
            .iter()
            .map(|s| ShellStage::from(*s).as_str())
            .collect();
        named.sort_unstable();
        named.dedup();
        assert_eq!(named.len(), stages.len());
    }
}
