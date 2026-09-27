//! boot2deb core — typed config model, layer resolution, and lock format.
//!
//! Pure and deterministic: no build side effects (those live in the engine), so
//! everything here is unit-testable without a Linux host.
//!
//! The public surface is:
//!
//! - The [`model`] types and the [`ConfigRoot`] loader.
//! - The [`resolve_device`] and [`resolve_recipe`] entry points.
//! - The [`feature`], [`series`], [`lock`], [`manifest`], and [`kconfig`] formats.
//! - Comparison of two build points ([`diff`]), and of a build with its reproduction
//!   ([`outputs`]).
//! - Patch normalization for `patch import` ([`mbox`]).
//! - Device and recipe generation for `new-device` ([`scaffold`]).
//! - Selftest expectations ([`expect`]), and [`size`] parsing.
//! - Initramfs module coverage ([`initramfs`]), and image file manifests ([`files`]).
//! - Source-pin durability form ([`sources`]), and re-pin ref selection ([`repin`]).
//! - The upgrade survey's comparison ([`outdated`]), over the tag spellings
//!   [`version`] parses.
//! - Debian suite facts ([`suite`]), `authorized_keys` entry shape ([`authkeys`]),
//!   and [`host`] detection.
//!
//! `missing_docs` is a warning here to keep the config surface documented as it
//! grows.
#![warn(missing_docs)]
#![warn(rustdoc::broken_intra_doc_links)]

pub mod authkeys;
pub mod base64;
pub mod buildpoint;
pub mod chromeos;
pub mod datavolume;
pub mod datetime;
pub mod debdep;
pub mod diff;
pub mod error;
pub mod expect;
pub mod feature;
pub mod files;
pub mod host;
pub mod hostname;
pub mod initramfs;
pub mod kconfig;
pub mod loader;
pub mod lock;
pub mod manifest;
pub mod mbox;
pub mod model;
pub mod outdated;
pub mod outputs;
pub mod press;
pub mod provenance;
pub mod repin;
pub mod resolve;
pub mod sbom;
pub mod scaffold;
pub mod series;
pub mod size;
pub mod sources;
pub mod staticip;
pub mod suite;
pub mod support;
pub mod version;
pub mod weight;
pub mod wifi;

pub use buildpoint::BuildPoint;
pub use error::ConfigError;
pub use feature::Feature;
pub use host::HostInfo;
pub use kconfig::KernelConfig;
pub use loader::ConfigRoot;
pub use model::*;
pub use resolve::{resolve_device, resolve_recipe};
pub use series::{load_series, PatchEntry, PatchSeries, RangeMatch};
