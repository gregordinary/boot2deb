//! `resolve`: resolve a device or recipe to a complete build and print it.
//!
//! The documented first coherence gate — it does no build work, but it validates the
//! cheap local invariants (geometry, fragments, keyrings) after the printout, so the
//! resolved values sit beside any failure they explain.

use crate::config::{preflight_config, resolve};
use crate::render::print_build;
use boot2deb_core::model::Overrides;
use boot2deb_core::ConfigRoot;

/// Run `resolve <target>`, rendering the resolved build for a human or as one JSON
/// document under `--json`.
pub(crate) fn run(
    root: &ConfigRoot,
    target: &str,
    overrides: Overrides,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let build = resolve(root, target, overrides)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&build)?);
    } else {
        print_build(&build);
    }
    preflight_config(root, &build)?;
    Ok(())
}
