//! boot2deb CLI — a thin client over the config core and the engine.
//!
//! Subcommands: `list-devices`, `list-recipes`, `list-kernels`, `list-features`,
//! `list-kmods`, `resolve`, and `doctor` (config inspection + host preflight);
//! `support-matrix` (each shipped recipe's support claim joined to its lock's pins);
//! `new-device` (scaffold a new device + recipe from the typed model); `update`
//! (resolve upstream refs into the lock); `outdated` (survey, read-only, what has
//! moved upstream since); `verify-patches`, `verify-config`, and `verify-sources`
//! (the patch, kernel-config, and source-durability gates); `patch import` (fetch +
//! normalize + slot a patch into a series); `build` (drive the compile / rootfs /
//! image pipeline from the lock); `diff` (compare two build points, offline, from the
//! documents a build already wrote); `sbom` (export an image's bill of materials as
//! SPDX or CycloneDX); `size` (break down what an image's package set weighs, from the
//! published plan); `why-rebuild` (explain, offline, which compile nodes the next
//! build reuses vs. rebuilds); `shell` (open an interactive session in the root a build
//! stage compiles in); `try` (boot the built image under QEMU and assert the
//! userland works before flashing); and `clean` (remove a recipe's build scratch).
//!
//! This module is the entry point only: it parses the argument tree ([`crate::args`]),
//! composes the config root, and dispatches to the handler in [`crate::commands`] that
//! owns each subcommand. Every error surfaces here once, as the process's exit code —
//! with one exception, `shell`, whose result is a command's own exit status.

mod args;
mod artifacts;
mod builder;
mod commands;
mod config;
mod fsutil;
mod nextstep;
mod prompt;
mod render;
mod sandboxes;
#[cfg(test)]
mod testsupport;
mod timing;
mod workdir;

use args::{Cli, Command, PatchAction};
use boot2deb_core::ConfigRoot;
use clap::Parser;
use config::ensure_config_root;
use render::Verbosity;
use std::process::ExitCode;

fn main() -> ExitCode {
    // Before anything creates a file: the umask is a process attribute, so it is the
    // one build-host setting that reaches the image through no environment variable at
    // all. Declared here rather than inherited.
    boot2deb_engine::build::declare_umask();
    let cli = Cli::parse();
    // Every overlay must name an existing directory; a bad `--overlay` fails here
    // rather than silently composing a search path the operator did not intend.
    let root = match ConfigRoot::with_overlays(cli.root, cli.overlay) {
        Ok(root) => root,
        Err(e) => {
            eprintln!("error: {e}");
            return ExitCode::FAILURE;
        }
    };
    let verbosity = Verbosity::from_flags(cli.quiet, cli.verbose);
    match run(&root, cli.command, cli.json, verbosity) {
        Ok(()) => ExitCode::SUCCESS,
        // `shell` is the one command whose result is a *command's* exit status rather
        // than a verdict of its own, so its status is adopted as the process's and is
        // not announced as an error. A shell that exits 2 because the last thing run in
        // it exited 2 has not failed at anything boot2deb did.
        Err(e) => match e.downcast::<commands::shell::SessionExit>() {
            Ok(session) => ExitCode::from(session.0),
            Err(e) => {
                eprintln!("error: {e}");
                ExitCode::FAILURE
            }
        },
    }
}

/// Dispatch one parsed command against the composed config root.
///
/// `json` and `verbosity` reach only the commands whose output they change: `json`
/// the ones with a machine form ([`supports_json`]), `verbosity` the ones that stream
/// build events.
fn run(
    root: &ConfigRoot,
    command: Command,
    json: bool,
    verbosity: Verbosity,
) -> Result<(), Box<dyn std::error::Error>> {
    // `--json` is global, so clap accepts it everywhere — but it only *means*
    // something where a command has a machine form. A flag that silently does nothing
    // is a trap for the scripted caller it exists for, so an unsupported pairing is an
    // error naming what to parse instead.
    if json && !supports_json(&command) {
        return Err(format!(
            "--json is not supported by `{}`: it has no machine-readable form. \
             The commands that do are list-*, resolve, doctor, verify-*, and build.{}",
            command_name(&command),
            json_alternative(&command),
        )
        .into());
    }
    // `--root` defaults to `.`, so running from any other directory used to
    // cascade per-layer "not found" errors that never named the real cause. One
    // structural check up front replaces that cascade. `patch import` is exempt
    // because it does not read the config root: it operates on the patches repo, and
    // its recipe hint degrades gracefully, and `cli-reference`/`completions`/`man`
    // render the command tree, which is compiled in. A bare `doctor` is *not* exempt — it verifies the
    // vendored trust anchors, so a missing root is a real answer it cannot give
    // rather than a check it can skip.
    if !matches!(
        command,
        Command::Patch { .. }
            | Command::CliReference { .. }
            | Command::Completions { .. }
            | Command::Man
    ) {
        ensure_config_root(root)?;
    }
    match command {
        Command::ListDevices => commands::list::devices(root, json),
        Command::ListRecipes => commands::list::recipes(root, json),
        Command::ListKernels => commands::list::kernels(root, json),
        Command::ListFeatures => commands::list::features(root, json),
        Command::ListKmods => commands::list::kmods(root, json),
        Command::SupportMatrix { markdown } => commands::support_matrix::run(root, markdown),
        Command::CliReference { markdown } => commands::cli_reference::run(markdown),
        Command::Completions { shell } => commands::shellenv::completions(shell),
        Command::Man => commands::shellenv::man(),
        Command::NewDevice { name, args } => commands::new_device::run(root, &name, args),
        Command::Resolve { target, overrides } => {
            commands::resolve::run(root, &target, overrides.into(), json)
        }
        Command::Doctor {
            target,
            work_dir,
            overrides,
        } => commands::doctor::run(root, target, work_dir, overrides.into(), json),
        Command::Update { recipe, args } => commands::update::run(root, &recipe, args, verbosity),
        Command::VerifyPatches { recipe, args } => {
            commands::verify_patches::run(root, &recipe, args, json, verbosity)
        }
        Command::VerifyConfig { recipe, args } => {
            commands::verify_config::run(root, &recipe, args, json, verbosity)
        }
        Command::VerifyPackages { recipe } => commands::verify_packages::run(root, &recipe, json),
        Command::VerifyImage { recipe, out_dir } => {
            commands::verify_image::run(root, &recipe, out_dir, json)
        }
        Command::VerifySources { recipe } => commands::verify_sources::run(root, &recipe, json),
        Command::Patch { action } => match action {
            PatchAction::Import { source, args } => commands::patch::import(root, &source, args),
        },
        Command::Build { recipe, args } => {
            commands::build::run(root, &recipe, args, None, json, verbosity)
        }
        Command::Reproduce { recipe, from, args } => {
            commands::reproduce::run(root, &recipe, from, args, json, verbosity)
        }
        Command::Diff {
            left,
            right,
            sections,
            patches_path,
        } => commands::diff::run(
            root,
            &left,
            &right,
            &sections,
            patches_path.as_deref(),
            json,
        ),
        Command::Sbom {
            target,
            format,
            out,
            features,
        } => commands::sbom::run(root, &target, format, out, features),
        Command::Size {
            target,
            by,
            top,
            features,
        } => commands::size::run(
            root,
            &target,
            by,
            // `--top 0` is "every row", which is what a `0` limit means to a reader;
            // passing it through as a limit of zero would print a header and nothing.
            (top > 0).then_some(top),
            features,
            json,
        ),
        Command::Outdated { recipes } => commands::outdated::run(root, &recipes, json),
        Command::WhyRebuild { recipe, args } => commands::why_rebuild::run(root, &recipe, args),
        Command::Shell { recipe, args } => commands::shell::run(root, &recipe, args, verbosity),
        Command::Clean { recipe, args } => commands::clean::run(root, recipe.as_deref(), args),
        Command::Press {
            recipe,
            output,
            args,
        } => commands::press::run(root, &recipe, output, args, verbosity),
        Command::Seed { image, args } => commands::seed::run(&image, args, verbosity),
        Command::Try { recipe, args } => commands::try_boot::run(root, &recipe, args, verbosity),
    }
}

/// Whether `--json` changes this command's output.
///
/// The set is deliberately the commands a *script* consumes: the inventories, the
/// resolution, the two gates a CI job asserts on, and the build stream. The rest
/// either produce a file (`support-matrix --markdown`, `new-device`) or are read by a
/// person deciding what to do next, and giving those a machine form would be inventing
/// a schema nothing asked for.
///
/// Exhaustive, like [`command_name`], so a new subcommand is a compile error here
/// rather than a silent `false` — which the `--json` guard would then reject, with
/// nothing to say the omission was an oversight.
fn supports_json(command: &Command) -> bool {
    match command {
        Command::ListDevices
        | Command::ListRecipes
        | Command::ListKernels
        | Command::ListFeatures
        | Command::ListKmods
        | Command::Resolve { .. }
        | Command::Doctor { .. }
        | Command::VerifyPatches { .. }
        | Command::VerifyConfig { .. }
        | Command::VerifyImage { .. }
        | Command::VerifyPackages { .. }
        | Command::VerifySources { .. }
        | Command::Build { .. }
        | Command::Reproduce { .. }
        | Command::Diff { .. }
        | Command::Size { .. }
        | Command::Outdated { .. } => true,
        Command::SupportMatrix { .. }
        | Command::CliReference { .. }
        | Command::Completions { .. }
        | Command::Man
        | Command::NewDevice { .. }
        | Command::Update { .. }
        | Command::Patch { .. }
        | Command::Sbom { .. }
        | Command::WhyRebuild { .. }
        | Command::Shell { .. }
        | Command::Clean { .. }
        | Command::Press { .. }
        | Command::Seed { .. }
        | Command::Try { .. } => false,
    }
}

/// The subcommand's name as typed, for the `--json` rejection message.
fn command_name(command: &Command) -> &'static str {
    match command {
        Command::ListDevices => "list-devices",
        Command::ListRecipes => "list-recipes",
        Command::ListKernels => "list-kernels",
        Command::ListFeatures => "list-features",
        Command::ListKmods => "list-kmods",
        Command::SupportMatrix { .. } => "support-matrix",
        Command::CliReference { .. } => "cli-reference",
        Command::Completions { .. } => "completions",
        Command::Man => "man",
        Command::NewDevice { .. } => "new-device",
        Command::Resolve { .. } => "resolve",
        Command::Doctor { .. } => "doctor",
        Command::Update { .. } => "update",
        Command::VerifyPatches { .. } => "verify-patches",
        Command::VerifyConfig { .. } => "verify-config",
        Command::VerifyImage { .. } => "verify-image",
        Command::VerifyPackages { .. } => "verify-packages",
        Command::VerifySources { .. } => "verify-sources",
        Command::Patch { .. } => "patch import",
        Command::Build { .. } => "build",
        Command::Reproduce { .. } => "reproduce",
        Command::Diff { .. } => "diff",
        Command::Sbom { .. } => "sbom",
        Command::Size { .. } => "size",
        Command::Outdated { .. } => "outdated",
        Command::WhyRebuild { .. } => "why-rebuild",
        Command::Shell { .. } => "shell",
        Command::Clean { .. } => "clean",
        Command::Press { .. } => "press",
        Command::Seed { .. } => "seed",
        Command::Try { .. } => "try",
    }
}

/// The structured route to the same information, where one exists — so the rejection
/// above redirects instead of merely refusing.
fn json_alternative(command: &Command) -> &'static str {
    match command {
        Command::SupportMatrix { .. } => {
            " For the matrix as a document, use `support-matrix --markdown`."
        }
        Command::CliReference { .. } => {
            " For the flag reference as a document, use `cli-reference --markdown`."
        }
        Command::Update { .. } => {
            " The lock `update` writes is the machine-readable result; read that."
        }
        Command::Sbom { .. } => " `sbom` writes JSON already; select the schema with --format.",
        Command::Clean { .. } => " For what would be removed, use `clean --dry-run`.",
        _ => "",
    }
}
