//! boot2deb core — typed config model, layer resolution, and lock format.
//!
//! Pure and deterministic: no build side effects (those live in the engine), so
//! everything here is unit-testable without a Linux host. The public surface is
//! the [`model`] types, the [`ConfigRoot`] loader, the [`resolve_device`] /
//! [`resolve_recipe`] entry points, the [`feature`], [`series`], [`lock`], and
//! [`kconfig`] formats, patch normalization for `patch import` ([`mbox`]),
//! device/recipe generation for `new-device` ([`scaffold`]), [`size`] parsing,
//! source-pin durability form ([`sources`]), Debian suite facts ([`suite`]), and
//! [`host`] detection.
//!
//! `missing_docs` is a warning here to keep the config surface documented as it
//! grows.
#![warn(missing_docs)]
#![warn(rustdoc::broken_intra_doc_links)]

pub mod buildpoint;
pub mod chromeos;
pub mod error;
pub mod feature;
pub mod host;
pub mod kconfig;
pub mod loader;
pub mod lock;
pub mod mbox;
pub mod model;
pub mod provenance;
pub mod resolve;
pub mod scaffold;
pub mod series;
pub mod size;
pub mod sources;
pub mod suite;
pub mod support;

pub use buildpoint::BuildPoint;
pub use error::ConfigError;
pub use feature::Feature;
pub use host::HostInfo;
pub use kconfig::KernelConfig;
pub use loader::ConfigRoot;
pub use model::*;
pub use resolve::{resolve_device, resolve_recipe};
pub use series::{load_series, PatchEntry, PatchSeries, RangeMatch};
