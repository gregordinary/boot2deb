//! Patch-profile model: a kernel-version-scoped manifest —
//! `profiles/<name>/profile.toml` in the `patches` repo — declaring the kernel
//! range a series targets plus ordered per-tree patch lists.
//!
//! A profile belongs to a *kernel definition*, not a device: a series that
//! applies to one kernel version will not apply to another, so the profile lives
//! with the kernel that owns it. Supporting a new kernel version means
//! authoring a new profile; old profiles stay so old kernels keep building.
//!
//! Pure: parsing plus version-range matching only. Fetching the patches repo and
//! running `git am` are engine side effects. The version match here is the
//! *declared intent* (`applies_to_kernel`); the engine's verify-applies gate is
//! the *enforcement*.

use crate::error::ConfigError;
use semver::{Version, VersionReq};
use serde::Deserialize;
use std::path::Path;
use std::str::FromStr;

/// A patch profile manifest (`profiles/<name>/profile.toml`).
///
/// Each scope list is an ordered sequence of patches-repo-relative paths, and
/// the list — not the filename prefixes — is the authoritative apply order. A
/// single tree's list may span scopes: the `kernel` list interleaves
/// `media-accel/kernel/*` and `rocket/*` patches in one apply sequence, so a
/// `rocket` patch can fall between two `media-accel` patches. The engine
/// applies each list to its corresponding source tree via `git am --3way`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchProfile {
    /// Kernel version range the series targets, as a semver requirement
    /// (e.g. `">=7.0, <7.2"`). Matched against the resolved kernel's release
    /// version by [`applies_to`](PatchProfile::applies_to).
    pub applies_to_kernel: String,
    /// Kernel-tree patches, in apply order (may span the `media-accel` and
    /// `rocket` scopes).
    #[serde(default)]
    pub kernel: Vec<String>,
    /// ffmpeg-tree patches, in apply order.
    #[serde(default)]
    pub ffmpeg: Vec<String>,
    /// Userspace-tree (MPP/RGA) patches, in apply order.
    #[serde(default)]
    pub userspace: Vec<String>,
    /// u-boot-tree patches, in apply order (empty for boards that patch no
    /// u-boot, e.g. the RK1's pristine `v2026.04`).
    #[serde(default)]
    pub uboot: Vec<String>,
}

impl PatchProfile {
    /// Parse `applies_to_kernel` into a [`VersionReq`].
    ///
    /// `profile` names the owner for the error message only.
    pub fn version_req(&self, profile: &str) -> Result<VersionReq, ConfigError> {
        VersionReq::parse(&self.applies_to_kernel).map_err(|source| {
            ConfigError::InvalidVersionReq {
                profile: profile.to_string(),
                value: self.applies_to_kernel.clone(),
                source,
            }
        })
    }

    /// True when `kernel_version` falls in this profile's declared range.
    ///
    /// `kernel_version` may be `v`-prefixed (`v7.1.1`) and may omit the patch
    /// component (`7.1` is read as `7.1.0`). Matching targets *release* versions:
    /// an `-rc` / prerelease tag is not matched by a release-only range, which is
    /// intentional — profiles pin against release kernels.
    pub fn applies_to(&self, profile: &str, kernel_version: &str) -> Result<bool, ConfigError> {
        let req = self.version_req(profile)?;
        let ver = parse_kernel_version(kernel_version)?;
        Ok(req.matches(&ver))
    }

    /// [`applies_to`](PatchProfile::applies_to) as a hard gate: returns
    /// [`ConfigError::KernelOutsideProfileRange`] when the kernel is out of
    /// range, so a mismatched `(kernel, profile)` fails before any patch is
    /// fetched.
    pub fn ensure_applies(&self, profile: &str, kernel_version: &str) -> Result<(), ConfigError> {
        if self.applies_to(profile, kernel_version)? {
            Ok(())
        } else {
            Err(ConfigError::KernelOutsideProfileRange {
                profile: profile.to_string(),
                kernel_version: kernel_version.to_string(),
                applies_to: self.applies_to_kernel.clone(),
            })
        }
    }

    /// The ordered patch list for one [`Scope`] — the tree `patch import` slots a
    /// new patch into.
    pub fn scope(&self, scope: Scope) -> &[String] {
        match scope {
            Scope::Kernel => &self.kernel,
            Scope::Ffmpeg => &self.ffmpeg,
            Scope::Userspace => &self.userspace,
            Scope::Uboot => &self.uboot,
        }
    }
}

/// One of the four source trees a profile orders independently. The variant
/// name matches the profile's TOML array key, so it doubles as the key to edit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// The kernel tree (spans the `media-accel` and `rocket` scopes).
    Kernel,
    /// The ffmpeg tree.
    Ffmpeg,
    /// The userspace (MPP/RGA) tree.
    Userspace,
    /// The u-boot tree.
    Uboot,
}

impl Scope {
    /// The profile TOML array key for this scope (`"kernel"`, `"ffmpeg"`, …).
    pub fn as_str(self) -> &'static str {
        match self {
            Scope::Kernel => "kernel",
            Scope::Ffmpeg => "ffmpeg",
            Scope::Userspace => "userspace",
            Scope::Uboot => "uboot",
        }
    }
}

impl FromStr for Scope {
    type Err = String;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "kernel" => Ok(Scope::Kernel),
            "ffmpeg" => Ok(Scope::Ffmpeg),
            "userspace" => Ok(Scope::Userspace),
            "uboot" => Ok(Scope::Uboot),
            other => Err(format!(
                "unknown scope '{other}' (expected kernel|ffmpeg|userspace|uboot)"
            )),
        }
    }
}

/// The leading numeric prefix of a patch label's filename, e.g.
/// `"media-accel/kernel/045-fix-foo.patch"` → `Some(45)`. `None` when the basename
/// does not begin with digits.
pub fn patch_prefix(label: &str) -> Option<u32> {
    let base = label.rsplit('/').next().unwrap_or(label);
    let digits: String = base.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// Choose a zero-padded filename prefix that sorts a new patch at list index
/// `index` among the profile scope's ordered entries.
///
/// The prefix mirrors the list order — a reading aid, not load-bearing (the list
/// is authoritative). It is the integer midpoint between the numeric prefixes of
/// the neighbors on either side of `index` (`before + 10` when appending, half the
/// first when prepending, `010` into an empty list). Padding width matches the
/// widest existing prefix (minimum 3).
///
/// When two integer neighbors leave no whole-number gap (`070`/`071`), the import
/// does not dead-end: it appends the next free lowercase-letter suffix to the lower
/// neighbor (`070` → `070a` → `070b` → …), which lexically sorts after `070` and
/// before `071`, so a patch slots between consecutive entries without renumbering
/// the committed series. Because the list — not the filename — is the authoritative
/// order, the suffix only needs to read *near* its neighbors, not fall exactly
/// between them.
///
/// The one case with no automatic room is prepending before a `000`-prefixed first
/// entry (nothing sorts below it): that is [`ConfigError::PatchPrefixNoGap`], so the
/// caller supplies an explicit `--as` label.
pub fn derive_prefix(list: &[String], index: usize) -> Result<String, ConfigError> {
    let before = index
        .checked_sub(1)
        .and_then(|i| list.get(i))
        .and_then(|l| patch_prefix(l));
    let after = list.get(index).and_then(|l| patch_prefix(l));
    let width = prefix_width(list);

    let value = match (before, after) {
        (Some(b), Some(a)) if a > b + 1 => b + (a - b) / 2,
        // Consecutive (or duplicate) integer neighbors: no whole-number gap, so fall
        // back to a lettered sub-prefix on the lower neighbor.
        (Some(b), Some(_)) => {
            let suffix = next_suffix(list, b);
            return Ok(format!("{b:0width$}{suffix}"));
        }
        (Some(b), None) => b + 10,
        (None, Some(a)) if a >= 1 => a / 2,
        // Prepending before a `000` first entry: nothing sorts below it.
        (None, Some(a)) => return Err(ConfigError::PatchPrefixNoGap { after: a }),
        (None, None) => 10,
    };

    Ok(format!("{value:0width$}"))
}

/// The zero-padding width for a derived prefix: the widest numeric prefix among the
/// scope's existing filenames, floored at 3, so a new prefix lines up with them.
fn prefix_width(list: &[String]) -> usize {
    list.iter()
        .filter_map(|l| l.rsplit('/').next())
        .map(|b| b.chars().take_while(|c| c.is_ascii_digit()).count())
        .max()
        .unwrap_or(0)
        .max(3)
}

/// The next free lowercase-letter suffix at numeric prefix `value` — `a` when none
/// is taken, else the letter after the highest one already used by an entry whose
/// numeric prefix is `value` (so `070` + existing `070a` yields `b`). Falls back to
/// `z` in the absurd case that all 26 are taken; the prefix is advisory, so a
/// collision there only affects display ordering.
fn next_suffix(list: &[String], value: u32) -> char {
    let used: std::collections::BTreeSet<char> = list
        .iter()
        .filter_map(|l| l.rsplit('/').next())
        .filter_map(|base| {
            let digits: String = base.chars().take_while(|c| c.is_ascii_digit()).collect();
            (digits.parse::<u32>().ok() == Some(value))
                .then(|| base.chars().nth(digits.len()))
                .flatten()
                .filter(|c| c.is_ascii_lowercase())
        })
        .collect();
    ('a'..='z').find(|c| !used.contains(c)).unwrap_or('z')
}

/// Load `profiles/<name>/profile.toml` from a patches-repo root.
///
/// `patches_root` is a checkout of the `patches` repo (fetched at the
/// lock-pinned commit, or a `--patches-path` dev override). A missing file is
/// [`ConfigError::NotFound`] with `kind = "profile"`.
pub fn load_profile(patches_root: &Path, name: &str) -> Result<PatchProfile, ConfigError> {
    let path = patches_root
        .join("profiles")
        .join(name)
        .join("profile.toml");
    let text = std::fs::read_to_string(&path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            ConfigError::NotFound {
                kind: "profile",
                name: name.to_string(),
                path: path.display().to_string(),
            }
        } else {
            ConfigError::Io {
                path: path.display().to_string(),
                source,
            }
        }
    })?;
    toml::from_str(&text).map_err(|source| ConfigError::Parse {
        path: path.display().to_string(),
        source,
    })
}

/// Parse a kernel version tag into a [`Version`], tolerating a leading `v` and a
/// missing patch component (`v7.1` → `7.1.0`). Prerelease suffixes (`-rc2`) are
/// preserved as semver prereleases.
fn parse_kernel_version(s: &str) -> Result<Version, ConfigError> {
    let stripped = s.strip_prefix('v').unwrap_or(s);
    let normalized = pad_to_three_components(stripped);
    Version::parse(&normalized).map_err(|source| ConfigError::InvalidKernelVersion {
        value: s.to_string(),
        source,
    })
}

/// Pad a `MAJOR.MINOR` core to `MAJOR.MINOR.0` so two-component kernel tags parse
/// as semver, leaving any `-prerelease` / `+build` suffix and already-three-part
/// cores untouched.
fn pad_to_three_components(s: &str) -> String {
    // Split off the first prerelease/build delimiter; only the numeric core needs
    // padding.
    let (core, rest) = match s.find(['-', '+']) {
        Some(i) => (&s[..i], &s[i..]),
        None => (s, ""),
    };
    if core.split('.').count() == 2 {
        format!("{core}.0{rest}")
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn profile() -> PatchProfile {
        PatchProfile {
            applies_to_kernel: ">=7.0, <7.2".into(),
            kernel: vec![
                "media-accel/kernel/040-vdpu381-multicore-v1-curated.patch".into(),
                "rocket/081-rocket-drv-npu-clk.patch".into(),
            ],
            ffmpeg: vec![],
            userspace: vec![],
            uboot: vec![],
        }
    }

    #[test]
    fn parses_manifest_toml() {
        let text = r#"
            applies_to_kernel = ">=7.0, <7.2"
            kernel    = ["media-accel/kernel/040-x.patch", "rocket/081-y.patch"]
            ffmpeg    = ["media-accel/ffmpeg/0001-z.patch"]
            userspace = ["media-accel/userspace/001-w.patch"]
        "#;
        let p: PatchProfile = toml::from_str(text).unwrap();
        assert_eq!(p.kernel.len(), 2);
        assert_eq!(p.ffmpeg, vec!["media-accel/ffmpeg/0001-z.patch"]);
        assert!(p.uboot.is_empty());
    }

    #[test]
    fn unknown_field_is_rejected() {
        let text = "applies_to_kernel = \">=7.0\"\nbogus = []\n";
        assert!(toml::from_str::<PatchProfile>(text).is_err());
    }

    #[test]
    fn version_in_range_applies() {
        let p = profile();
        // The RK1's kernel version.
        assert!(p.applies_to("rk3588-accel", "v7.1.1").unwrap());
        assert!(p.applies_to("rk3588-accel", "7.1.1").unwrap());
        // Lower bound inclusive; a bare MAJOR.MINOR reads as .0.
        assert!(p.applies_to("rk3588-accel", "7.0").unwrap());
    }

    #[test]
    fn version_out_of_range_does_not_apply() {
        let p = profile();
        assert!(!p.applies_to("rk3588-accel", "6.12.0").unwrap());
        // Upper bound exclusive.
        assert!(!p.applies_to("rk3588-accel", "7.2.0").unwrap());
    }

    #[test]
    fn ensure_applies_hard_errors_out_of_range() {
        let p = profile();
        let err = p.ensure_applies("rk3588-accel", "6.12.0").unwrap_err();
        assert!(matches!(
            err,
            ConfigError::KernelOutsideProfileRange { .. }
        ));
    }

    #[test]
    fn invalid_range_is_typed_error() {
        let mut p = profile();
        p.applies_to_kernel = "not a range".into();
        let err = p.applies_to("rk3588-accel", "7.1.1").unwrap_err();
        assert!(matches!(err, ConfigError::InvalidVersionReq { .. }));
    }

    #[test]
    fn scope_parses_and_indexes_the_right_list() {
        assert_eq!("kernel".parse::<Scope>().unwrap(), Scope::Kernel);
        assert_eq!("uboot".parse::<Scope>().unwrap(), Scope::Uboot);
        assert!("bogus".parse::<Scope>().is_err());
        let p = profile();
        assert_eq!(p.scope(Scope::Kernel).len(), 2);
        assert!(p.scope(Scope::Ffmpeg).is_empty());
        assert_eq!(Scope::Userspace.as_str(), "userspace");
    }

    #[test]
    fn patch_prefix_reads_basename_digits() {
        assert_eq!(patch_prefix("media-accel/kernel/045-fix.patch"), Some(45));
        assert_eq!(patch_prefix("rocket/081-npu.patch"), Some(81));
        assert_eq!(patch_prefix("no-number.patch"), None);
    }

    #[test]
    fn derive_prefix_appends_midpoints_and_pads() {
        let list = vec![
            "media-accel/kernel/040-a.patch".to_string(),
            "media-accel/kernel/050-b.patch".to_string(),
            "rocket/081-c.patch".to_string(),
        ];
        // Append past the end: last + 10.
        assert_eq!(derive_prefix(&list, 3).unwrap(), "091");
        // Insert between 040 and 050: midpoint 045.
        assert_eq!(derive_prefix(&list, 1).unwrap(), "045");
        // Insert between 050 and 081: midpoint 065.
        assert_eq!(derive_prefix(&list, 2).unwrap(), "065");
        // Prepend before 040: half.
        assert_eq!(derive_prefix(&list, 0).unwrap(), "020");
        // Empty list starts at 010.
        assert_eq!(derive_prefix(&[], 0).unwrap(), "010");
    }

    #[test]
    fn derive_prefix_suffixes_when_no_integer_gap() {
        let list = vec![
            "k/070-a.patch".to_string(),
            "k/071-b.patch".to_string(),
        ];
        // Consecutive 070/071 leave no whole-number gap: fall back to a lettered
        // sub-prefix on the lower neighbor, which sorts between them.
        assert_eq!(derive_prefix(&list, 1).unwrap(), "070a");
        assert!("070-a.patch" < "070a-x.patch" && "070a-x.patch" < "071-b.patch");
    }

    #[test]
    fn derive_prefix_advances_the_suffix_letter() {
        // A second insert at the same slot skips the taken `a` and uses `b`.
        let list = vec![
            "k/070-a.patch".to_string(),
            "k/070a-x.patch".to_string(),
            "k/071-b.patch".to_string(),
        ];
        assert_eq!(derive_prefix(&list, 1).unwrap(), "070b");
    }

    #[test]
    fn derive_prefix_prepends_before_a_low_first_entry() {
        // Before `001` there is integer room (`000`); before `000` there is none.
        assert_eq!(derive_prefix(&["k/001-a.patch".to_string()], 0).unwrap(), "000");
        let err = derive_prefix(&["k/000-a.patch".to_string()], 0).unwrap_err();
        assert!(matches!(err, ConfigError::PatchPrefixNoGap { after: 0 }));
    }
}
