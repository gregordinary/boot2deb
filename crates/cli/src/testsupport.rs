//! Shared fixtures for the CLI's unit tests.

use boot2deb_core::ConfigRoot;
use std::path::PathBuf;

/// The boot2deb repo root (two levels up from this crate's manifest), for tests
/// that resolve the shipped config.
pub(crate) fn repo_root() -> ConfigRoot {
    ConfigRoot::new(repo_root_path())
}

/// The repo root as a path, for tests that compose it with their own overlay.
pub(crate) fn repo_root_path() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the crate manifest sits two levels below the repo root")
        .to_path_buf()
}
