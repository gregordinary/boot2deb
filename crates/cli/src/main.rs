//! boot2deb CLI — a thin client over the config core and the engine.
//!
//! Subcommands: `list-devices`, `list-recipes`, `list-kernels`, `list-features`,
//! `resolve`, and `doctor` (config inspection + host preflight); `new-device`
//! (scaffold a new device + recipe from the typed model); `update` (resolve upstream
//! refs into the lock); `verify-patches`, `verify-config`, and `verify-sources` (the
//! patch, kernel-config, and source-durability gates); `patch import` (fetch +
//! normalize + slot a patch into a profile); `build` (drive the compile / rootfs /
//! image pipeline from the lock); `why-rebuild` (explain, offline, which compile nodes
//! the next build reuses vs. rebuilds); and `clean` (remove a recipe's build scratch).
//!
//! This module is the entry point only: it parses the argument tree ([`crate::args`]),
//! composes the config root, and dispatches to the handler in [`crate::commands`] that
//! owns each subcommand. Every error surfaces here once, as the process's exit code.

mod args;
mod artifacts;
mod commands;
mod config;
mod fsutil;
mod prompt;
mod render;
#[cfg(test)]
mod testsupport;
mod workdir;

use args::{Cli, Command, PatchAction};
use boot2deb_core::ConfigRoot;
use clap::Parser;
use config::ensure_config_root;
use std::process::ExitCode;

fn main() -> ExitCode {
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
    match run(&root, cli.command, cli.json) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("error: {e}");
            ExitCode::FAILURE
        }
    }
}

/// Dispatch one parsed command against the composed config root. The `json` flag is
/// passed only to the commands whose output it changes (`list-*`, `resolve`, `build`).
fn run(root: &ConfigRoot, command: Command, json: bool) -> Result<(), Box<dyn std::error::Error>> {
    // `--root` defaults to `.`, so running from any other directory used to
    // cascade per-layer "not found" errors that never named the real cause. One
    // structural check up front replaces that cascade. Two commands are exempt
    // because they do not read the config root: `patch import` operates on the
    // patches repo (its recipe hint degrades gracefully), and a bare `doctor`
    // reports host facts only.
    let needs_root = !matches!(
        command,
        Command::Patch { .. } | Command::Doctor { target: None, .. }
    );
    if needs_root {
        ensure_config_root(root)?;
    }
    match command {
        Command::ListDevices => commands::list::devices(root, json),
        Command::ListRecipes => commands::list::recipes(root, json),
        Command::ListKernels => commands::list::kernels(root, json),
        Command::ListFeatures => commands::list::features(root, json),
        Command::NewDevice { name, args } => commands::new_device::run(root, &name, args),
        Command::Resolve { target, overrides } => {
            commands::resolve::run(root, &target, overrides.into(), json)
        }
        Command::Doctor { target, overrides } => {
            commands::doctor::run(root, target, overrides.into())
        }
        Command::Update { recipe, args } => commands::update::run(root, &recipe, args),
        Command::VerifyPatches { recipe, args } => {
            commands::verify_patches::run(root, &recipe, args)
        }
        Command::VerifyConfig { recipe, args } => commands::verify_config::run(root, &recipe, args),
        Command::VerifySources { recipe } => commands::verify_sources::run(root, &recipe),
        Command::Patch { action } => match action {
            PatchAction::Import { source, args } => commands::patch::import(root, &source, args),
        },
        Command::Build { recipe, args } => commands::build::run(root, &recipe, args, json),
        Command::WhyRebuild { recipe, args } => commands::why_rebuild::run(root, &recipe, args),
        Command::Clean { recipe, args } => commands::clean::run(root, &recipe, args),
    }
}
