//! One module per subcommand: each owns its handler and the helpers only it uses.
//! [`crate::main`] parses and dispatches; the shared machinery lives in
//! [`crate::config`], [`crate::render`], and their siblings.

pub(crate) mod build;
pub(crate) mod clean;
pub(crate) mod cli_reference;
pub(crate) mod diff;
pub(crate) mod doctor;
pub(crate) mod list;
pub(crate) mod new_device;
pub(crate) mod outdated;
pub(crate) mod patch;
pub(crate) mod reproduce;
pub(crate) mod resolve;
pub(crate) mod sbom;
pub(crate) mod shellenv;
pub(crate) mod support_matrix;
pub(crate) mod update;
pub(crate) mod verify_config;
pub(crate) mod verify_packages;
pub(crate) mod verify_patches;
pub(crate) mod verify_sources;
pub(crate) mod why_rebuild;
