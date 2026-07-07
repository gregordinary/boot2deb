//! boot2deb core — typed config model, layer resolution, and lock format.
//!
//! Pure and deterministic: no build side effects (those live in the engine), so
//! everything here is unit-testable without a Linux host. The public surface is
//! the [`model`] types, the [`ConfigRoot`] loader, the [`resolve_device`] /
//! [`resolve_recipe`] entry points, the [`feature`], [`profile`], [`lock`], and
//! [`kconfig`] formats, patch normalization for `patch import` ([`mbox`]),
//! [`size`] parsing, source-pin durability form ([`sources`]), and [`host`]
//! detection.
//!
//! `missing_docs` is a warning here to keep the config surface documented as it
//! grows.
#![warn(missing_docs)]
#![warn(rustdoc::broken_intra_doc_links)]

pub mod error;
pub mod feature;
pub mod host;
pub mod kconfig;
pub mod loader;
pub mod lock;
pub mod mbox;
pub mod model;
pub mod profile;
pub mod provenance;
pub mod resolve;
pub mod size;
pub mod sources;

pub use error::ConfigError;
pub use feature::Feature;
pub use host::HostInfo;
pub use kconfig::KernelConfig;
pub use loader::ConfigRoot;
pub use model::*;
pub use profile::{load_profile, PatchProfile};
pub use resolve::{resolve_device, resolve_recipe};
