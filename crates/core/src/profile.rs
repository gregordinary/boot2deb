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

/// The `patch_profile` a kernel definition authors when it applies no patch series
/// at all — a stock mainline kernel whose SoC is fully upstream, or a vendor kernel
/// that already ships its patches. Such a build never reads the `patches` repo, so
/// its lock records no `[patches]` table.
///
/// The spelling is config-facing; [`patch_profile`] maps it to the `None` the rest of
/// the code reasons about, so no other module compares against this string.
pub const NO_PATCH_PROFILE: &str = "none";

/// Interpret a kernel definition's authored `patch_profile`: `None` for the
/// [`NO_PATCH_PROFILE`] sentinel, `Some(name)` for a real profile in the `patches`
/// repo. Resolution calls this once, so an absent profile flows through
/// [`ResolvedKernel`](crate::model::ResolvedKernel) and the lock as a typed absence
/// rather than a magic string.
pub fn patch_profile(authored: &str) -> Option<&str> {
    (authored != NO_PATCH_PROFILE).then_some(authored)
}

/// How a kernel version is matched against a declared range.
///
/// The distinction exists only for prereleases. By semver's rule a prerelease
/// never satisfies a range whose bounds carry none, so `7.2.0-rc3` does not match
/// `">=7.0, <7.2"` — and separately does not match `">=7.2"` either, leaving an RC
/// matched by nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeMatch {
    /// Release-strict: a prerelease matches only a range that names one. This is
    /// what a **build** uses — a profile's declared envelope is a claim about
    /// released kernels, and quietly building an RC against it would overstate it.
    Release,
    /// Candidate: a prerelease is matched as its base release (`7.2.0-rc3` is read
    /// as `7.2.0`). This is what **candidate verification** uses, where an RC is
    /// precisely the tree the question is about.
    Candidate,
}

/// One entry in a profile's ordered scope list.
///
/// Bare string or table, so the version-insensitive majority stay one-liners and
/// only the volatile few carry a range:
///
/// ```toml
/// kernel = [
///   "media-accel/kernel/040-vdpu381-multicore-v1-curated.patch",
///   { path = "rocket/084-rocket-drv-fix-bo-mm-uaf.patch", kernels = "<7.3" },
/// ]
/// ```
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(untagged)]
pub enum PatchEntry {
    /// A bare path: applies to every kernel the profile's envelope admits.
    Always(String),
    /// A path narrowed to its own kernel range, intersected with the envelope.
    Ranged(RangedPatch),
}

/// A [`PatchEntry`] carrying its own kernel range.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RangedPatch {
    /// Patches-repo-relative path, as a bare entry would spell it.
    pub path: String,
    /// Semver requirement narrowing this patch within the profile's envelope
    /// (e.g. `"<7.3"` for one upstreamed at 7.3, `">=7.2"` for a successor).
    pub kernels: String,
}

impl PatchEntry {
    /// The patches-repo-relative path, however the entry was spelled.
    pub fn path(&self) -> &str {
        match self {
            PatchEntry::Always(p) => p,
            PatchEntry::Ranged(r) => &r.path,
        }
    }

    /// This entry's own kernel range, or `None` for a bare path ("always").
    pub fn kernels(&self) -> Option<&str> {
        match self {
            PatchEntry::Always(_) => None,
            PatchEntry::Ranged(r) => Some(&r.kernels),
        }
    }

    /// True when this entry is selected for `kernel_version`. A bare entry always
    /// is; a ranged one is when its range matches under `mode`.
    ///
    /// `profile` names the owner for the error message only.
    pub fn selected(
        &self,
        profile: &str,
        kernel_version: &str,
        mode: RangeMatch,
    ) -> Result<bool, ConfigError> {
        match self.kernels() {
            None => Ok(true),
            Some(range) => matches_range(profile, range, kernel_version, mode),
        }
    }
}

/// A patch profile manifest (`profiles/<name>/profile.toml`).
///
/// Each scope list is an ordered sequence of [`PatchEntry`], and the list — not the
/// filename prefixes — is the authoritative apply order. A single tree's list may
/// span scopes: the `kernel` list interleaves `media-accel/kernel/*` and `rocket/*`
/// patches in one apply sequence, so a `rocket` patch can fall between two
/// `media-accel` patches. The engine applies each list to its corresponding source
/// tree via `git am --3way`.
///
/// Two ranges gate a build: [`applies_to_kernel`](Self::applies_to_kernel) is the
/// profile's overall envelope, and each entry may narrow itself further within it.
/// Both express *declared intent*; the engine's `git am` pass is the enforcement.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PatchProfile {
    /// Version range the kernel-family scopes (`kernel`/`ffmpeg`/`userspace`) target,
    /// as a semver requirement (e.g. `">=7.0, <7.2"`), matched against the resolved
    /// kernel's release version. `None` (omitted) means those scopes apply to any
    /// kernel — the shape a profile that patches only u-boot takes, since it makes no
    /// kernel claim. Gated per build by [`ensure_applies`](Self::ensure_applies).
    #[serde(default)]
    pub applies_to_kernel: Option<String>,
    /// Version range the `uboot` scope targets, matched against the resolved u-boot's
    /// version (the boot method's `uboot_ref`, e.g. `v2026.04`) rather than the
    /// kernel's — u-boot is its own axis. `None` means the u-boot series applies to
    /// any u-boot this profile is built against. Gated by
    /// [`ensure_applies_uboot`](Self::ensure_applies_uboot).
    #[serde(default)]
    pub applies_to_uboot: Option<String>,
    /// Kernel-tree patches, in apply order (may span the `media-accel` and
    /// `rocket` scopes).
    #[serde(default)]
    pub kernel: Vec<PatchEntry>,
    /// ffmpeg-tree patches, in apply order.
    #[serde(default)]
    pub ffmpeg: Vec<PatchEntry>,
    /// Userspace-tree (MPP/RGA) patches, in apply order.
    #[serde(default)]
    pub userspace: Vec<PatchEntry>,
    /// u-boot-tree patches, in apply order (empty for boards that patch no
    /// u-boot, e.g. the RK1's pristine `v2026.04`).
    #[serde(default)]
    pub uboot: Vec<PatchEntry>,
}

impl PatchProfile {
    /// The version-range envelope gating `scope`, if the profile declares one: the
    /// [`applies_to_kernel`](Self::applies_to_kernel) range for the kernel-family
    /// scopes, the [`applies_to_uboot`](Self::applies_to_uboot) range for `uboot`.
    fn envelope(&self, scope: Scope) -> Option<&str> {
        match scope {
            Scope::Uboot => self.applies_to_uboot.as_deref(),
            _ => self.applies_to_kernel.as_deref(),
        }
    }

    /// Parse `scope`'s declared envelope into a [`VersionReq`], or `None` when the
    /// profile declares none for it (that scope then applies to any version).
    ///
    /// `profile` names the owner for the error message only.
    pub fn version_req(
        &self,
        profile: &str,
        scope: Scope,
    ) -> Result<Option<VersionReq>, ConfigError> {
        self.envelope(scope)
            .map(|req| {
                VersionReq::parse(req).map_err(|source| ConfigError::InvalidVersionReq {
                    profile: profile.to_string(),
                    value: req.to_string(),
                    source,
                })
            })
            .transpose()
    }

    /// True when `kernel_version` falls in this profile's kernel envelope, matched
    /// release-strict ([`RangeMatch::Release`]); always true when the profile
    /// declares no kernel envelope.
    ///
    /// `kernel_version` may be `v`-prefixed (`v7.1.1`) and may omit the patch
    /// component (`7.1` is read as `7.1.0`).
    pub fn applies_to(&self, profile: &str, kernel_version: &str) -> Result<bool, ConfigError> {
        self.applies_to_under(profile, kernel_version, RangeMatch::Release)
    }

    /// [`applies_to`](Self::applies_to) under an explicit [`RangeMatch`], so
    /// candidate verification can ask about an `-rc` kernel that the release-strict
    /// build path would refuse.
    pub fn applies_to_under(
        &self,
        profile: &str,
        kernel_version: &str,
        mode: RangeMatch,
    ) -> Result<bool, ConfigError> {
        match self.applies_to_kernel.as_deref() {
            None => Ok(true),
            Some(req) => matches_range(profile, req, kernel_version, mode),
        }
    }

    /// The ordered paths of one [`Scope`] that apply to `kernel_version` — the
    /// series the engine actually feeds to `git am`.
    ///
    /// Entries whose own range excludes this kernel are filtered out; order among
    /// the survivors is preserved. The envelope is *not* re-checked here (the
    /// kernel node gates it once per build via [`ensure_applies`](Self::ensure_applies)),
    /// so this is purely the per-entry narrowing.
    pub fn series_for(
        &self,
        scope: Scope,
        profile: &str,
        kernel_version: &str,
        mode: RangeMatch,
    ) -> Result<Vec<&str>, ConfigError> {
        self.scope(scope)
            .iter()
            .filter_map(|e| match e.selected(profile, kernel_version, mode) {
                Ok(true) => Some(Ok(e.path())),
                Ok(false) => None,
                Err(e) => Some(Err(e)),
            })
            .collect()
    }

    /// Entries the profile can never select, as `(scope, entry)` pairs.
    ///
    /// An entry is unreachable when its own range shares no version with its scope's
    /// envelope: no version the profile admits for that scope can select it, so it is
    /// dead by construction rather than by judgement. Envelope `">=7.8, <8.0"` with an
    /// entry pinned `"<7.2"` is the shape this catches — typically a patch that was
    /// upstreamed long enough ago that the envelope has moved past its cap. A scope
    /// with no declared envelope admits every version, so none of its entries is
    /// unreachable.
    ///
    /// Deleting a reported entry (and its file) is safe for the same reason the
    /// commit pin exists: an old lock names an old `patches` commit whose tree still
    /// contains both.
    ///
    /// Conservative: an entry whose range or envelope uses an operator this cannot
    /// bound (`^`, `~`, `*`) is never reported, so a finding is always a real one.
    pub fn unreachable(&self, profile: &str) -> Result<Vec<(Scope, &PatchEntry)>, ConfigError> {
        let mut dead = Vec::new();
        for scope in Scope::ALL {
            let Some(envelope) = self.version_req(profile, scope)? else {
                continue;
            };
            let Some(env) = Interval::of(&envelope) else {
                continue;
            };
            for entry in self.scope(scope) {
                let Some(range) = entry.kernels() else {
                    continue;
                };
                let req = parse_req(profile, range)?;
                if Interval::of(&req).is_some_and(|i| !env.intersects(&i)) {
                    dead.push((scope, entry));
                }
            }
        }
        Ok(dead)
    }

    /// [`applies_to`](PatchProfile::applies_to) as a hard gate on the kernel-family
    /// scopes: returns [`ConfigError::KernelOutsideProfileRange`] when the kernel is
    /// out of range, so a mismatched `(kernel, profile)` fails before any patch is
    /// fetched. A no-op when the profile declares no kernel envelope.
    pub fn ensure_applies(&self, profile: &str, kernel_version: &str) -> Result<(), ConfigError> {
        match &self.applies_to_kernel {
            Some(range) if !self.applies_to(profile, kernel_version)? => {
                Err(ConfigError::KernelOutsideProfileRange {
                    profile: profile.to_string(),
                    kernel_version: kernel_version.to_string(),
                    applies_to: range.clone(),
                })
            }
            _ => Ok(()),
        }
    }

    /// The `uboot` scope's declared-intent gate: returns
    /// [`ConfigError::UbootOutsideProfileRange`] when `uboot_version` is outside the
    /// profile's [`applies_to_uboot`](Self::applies_to_uboot) envelope. A no-op when
    /// the profile declares no u-boot envelope, which is the common case — a u-boot
    /// series is usually written for the one u-boot generation the board runs.
    pub fn ensure_applies_uboot(
        &self,
        profile: &str,
        uboot_version: &str,
    ) -> Result<(), ConfigError> {
        match &self.applies_to_uboot {
            Some(range) if !matches_range(profile, range, uboot_version, RangeMatch::Release)? => {
                Err(ConfigError::UbootOutsideProfileRange {
                    profile: profile.to_string(),
                    uboot_version: uboot_version.to_string(),
                    applies_to: range.clone(),
                })
            }
            _ => Ok(()),
        }
    }

    /// The ordered entry list for one [`Scope`], unfiltered — the tree
    /// `patch import` slots a new patch into. Use
    /// [`series_for`](Self::series_for) to get the paths a given kernel selects.
    pub fn scope(&self, scope: Scope) -> &[PatchEntry] {
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
    /// Every scope, in profile-declaration order — so a check that must cover the
    /// whole manifest cannot silently miss one when a scope is added.
    pub const ALL: [Scope; 4] = [Scope::Kernel, Scope::Ffmpeg, Scope::Userspace, Scope::Uboot];

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
pub fn derive_prefix(list: &[&str], index: usize) -> Result<String, ConfigError> {
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
fn prefix_width(list: &[&str]) -> usize {
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
fn next_suffix(list: &[&str], value: u32) -> char {
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

/// Parse `range` and test `kernel_version` against it under `mode`.
///
/// Under [`RangeMatch::Candidate`] the version's prerelease is dropped before
/// matching, which is what makes an `-rc` tree answerable: semver would otherwise
/// exclude it from every range whose bounds name no prerelease.
fn matches_range(
    profile: &str,
    range: &str,
    kernel_version: &str,
    mode: RangeMatch,
) -> Result<bool, ConfigError> {
    let req = parse_req(profile, range)?;
    let mut ver = parse_kernel_version(kernel_version)?;
    if mode == RangeMatch::Candidate {
        ver.pre = semver::Prerelease::EMPTY;
    }
    Ok(req.matches(&ver))
}

/// Parse a semver requirement, attributing a failure to `profile`.
fn parse_req(profile: &str, range: &str) -> Result<VersionReq, ConfigError> {
    VersionReq::parse(range).map_err(|source| ConfigError::InvalidVersionReq {
        profile: profile.to_string(),
        value: range.to_string(),
        source,
    })
}

/// The half-open version span a [`VersionReq`] admits, used to decide whether two
/// ranges can share a version.
///
/// Only the bounding operators (`=`, `>`, `>=`, `<`, `<=`) are modelled;
/// [`of`](Interval::of) returns `None` for `^`, `~`, or `*` rather than guess. That
/// keeps the unreachable lint one-sided: it may miss a dead entry, but it never
/// reports a live one.
#[derive(Debug)]
struct Interval {
    /// Inclusive lower bound; `None` is unbounded below.
    lo: Option<Version>,
    /// Exclusive upper bound; `None` is unbounded above.
    hi: Option<Version>,
}

impl Interval {
    /// Derive the interval, or `None` if any comparator uses an unmodelled operator.
    ///
    /// A comparator's missing components read as zero (`<7.2` bounds at `7.2.0`),
    /// matching how the profiles spell their ranges. `<=`/`=` are widened to the
    /// next patch release so the upper bound stays exclusive throughout.
    fn of(req: &VersionReq) -> Option<Self> {
        use semver::Op;
        let mut lo: Option<Version> = None;
        let mut hi: Option<Version> = None;
        for c in &req.comparators {
            let at = Version::new(c.major, c.minor.unwrap_or(0), c.patch.unwrap_or(0));
            let next = Version::new(at.major, at.minor, at.patch + 1);
            let (l, h) = match c.op {
                Op::GreaterEq => (Some(at), None),
                Op::Greater => (Some(next), None),
                Op::Less => (None, Some(at)),
                Op::LessEq => (None, Some(next)),
                Op::Exact => (Some(at.clone()), Some(next)),
                _ => return None,
            };
            // Several comparators in one requirement conjoin: keep the tightest.
            if let Some(l) = l {
                lo = Some(lo.map_or(l.clone(), |cur| cur.max(l)));
            }
            if let Some(h) = h {
                hi = Some(hi.map_or(h.clone(), |cur| cur.min(h)));
            }
        }
        Some(Interval { lo, hi })
    }

    /// True when the two spans share at least one version.
    fn intersects(&self, other: &Interval) -> bool {
        let below = |lo: &Option<Version>, hi: &Option<Version>| match (lo, hi) {
            (Some(lo), Some(hi)) => lo < hi,
            // An unbounded side cannot close the gap on its own.
            _ => true,
        };
        below(&self.lo, &other.hi) && below(&other.lo, &self.hi)
    }
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

    /// A bare (always-applies) entry, for tests that do not exercise ranges.
    fn always(path: &str) -> PatchEntry {
        PatchEntry::Always(path.into())
    }

    /// A range-narrowed entry.
    fn ranged(path: &str, kernels: &str) -> PatchEntry {
        PatchEntry::Ranged(RangedPatch {
            path: path.into(),
            kernels: kernels.into(),
        })
    }

    fn profile() -> PatchProfile {
        PatchProfile {
            applies_to_kernel: Some(">=7.0, <7.2".into()),
            applies_to_uboot: None,
            kernel: vec![
                always("media-accel/kernel/040-vdpu381-multicore-v1-curated.patch"),
                always("rocket/081-rocket-drv-npu-clk.patch"),
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
        assert_eq!(p.ffmpeg, vec![always("media-accel/ffmpeg/0001-z.patch")]);
        assert!(p.uboot.is_empty());
    }

    #[test]
    fn parses_bare_and_ranged_entries_in_one_list() {
        // The volatile few carry a range; the rest stay one-liners.
        let text = r#"
            applies_to_kernel = ">=7.0, <7.4"
            kernel = [
              "media-accel/kernel/040-x.patch",
              { path = "media-accel/kernel/050-av1-v14.patch", kernels = "<7.2" },
              { path = "media-accel/kernel/050-av1-v15.patch", kernels = ">=7.2" },
            ]
        "#;
        let p: PatchProfile = toml::from_str(text).unwrap();
        assert_eq!(p.kernel[0], always("media-accel/kernel/040-x.patch"));
        assert_eq!(p.kernel[1].path(), "media-accel/kernel/050-av1-v14.patch");
        assert_eq!(p.kernel[1].kernels(), Some("<7.2"));
        assert_eq!(p.kernel[0].kernels(), None);
    }

    #[test]
    fn a_ranged_entry_with_an_unknown_key_is_rejected() {
        // Guards the untagged enum: a typo must not silently fall through to some
        // other variant, it must fail the parse.
        let text = r#"
            applies_to_kernel = ">=7.0"
            kernel = [{ path = "k/010-x.patch", kernel = "<7.2" }]
        "#;
        assert!(toml::from_str::<PatchProfile>(text).is_err());
    }

    #[test]
    fn series_for_selects_the_entries_the_kernel_admits() {
        let p = PatchProfile {
            applies_to_kernel: Some(">=7.0, <7.4".into()),
            applies_to_uboot: None,
            kernel: vec![
                always("k/040-always.patch"),
                ranged("k/050-v14.patch", "<7.2"),
                ranged("k/050-v15.patch", ">=7.2"),
                ranged("k/084-upstreamed.patch", "<7.3"),
            ],
            ffmpeg: vec![],
            userspace: vec![],
            uboot: vec![],
        };
        let at = |v| {
            p.series_for(Scope::Kernel, "t", v, RangeMatch::Release)
                .unwrap()
        };

        // 7.1: the pre-rework AV1 patch and the not-yet-upstreamed fix.
        assert_eq!(
            at("7.1.1"),
            [
                "k/040-always.patch",
                "k/050-v14.patch",
                "k/084-upstreamed.patch"
            ]
        );
        // 7.2: the AV1 successor takes over; the fix is still needed.
        assert_eq!(
            at("7.2.0"),
            [
                "k/040-always.patch",
                "k/050-v15.patch",
                "k/084-upstreamed.patch"
            ]
        );
        // 7.3: mainline absorbed 084, so it drops out by its own upper bound.
        assert_eq!(at("7.3.0"), ["k/040-always.patch", "k/050-v15.patch"]);
    }

    #[test]
    fn a_release_candidate_is_answerable_only_on_the_candidate_path() {
        let p = profile(); // envelope ">=7.0, <7.2"

        // Release-strict: semver excludes a prerelease from a release-only range,
        // which is correct for a build -- the envelope claims released kernels.
        assert!(!p.applies_to("t", "v7.1.0-rc3").unwrap());

        // Candidate: the RC is read as its base release, so the gate can answer
        // "would this series survive 7.1?" against the actual RC tree.
        assert!(p
            .applies_to_under("t", "v7.1.0-rc3", RangeMatch::Candidate)
            .unwrap());
        // Still bounded -- an RC past the envelope stays out.
        assert!(!p
            .applies_to_under("t", "v7.2.0-rc1", RangeMatch::Candidate)
            .unwrap());
    }

    #[test]
    fn unreachable_reports_only_entries_the_envelope_cannot_select() {
        let p = PatchProfile {
            applies_to_kernel: Some(">=7.8, <8.0".into()),
            applies_to_uboot: None,
            kernel: vec![
                always("k/040-always.patch"),          // bare: never unreachable
                ranged("k/084-old.patch", "<7.2"),     // dead: caps below the envelope
                ranged("k/090-live.patch", ">=7.9"),   // overlaps 7.9..8.0
                ranged("k/091-future.patch", ">=8.2"), // dead: starts above the envelope
            ],
            ffmpeg: vec![],
            userspace: vec![],
            uboot: vec![],
        };
        let dead: Vec<&str> = p
            .unreachable("t")
            .unwrap()
            .iter()
            .map(|(_, e)| e.path())
            .collect();
        assert_eq!(dead, ["k/084-old.patch", "k/091-future.patch"]);
    }

    #[test]
    fn unreachable_declines_to_judge_an_unmodelled_operator() {
        // `^` is not bounded by Interval::of, so the lint stays silent rather than
        // report a live entry. One-sided by design.
        let p = PatchProfile {
            applies_to_kernel: Some(">=7.8, <8.0".into()),
            applies_to_uboot: None,
            kernel: vec![ranged("k/010-x.patch", "^6.1")],
            ffmpeg: vec![],
            userspace: vec![],
            uboot: vec![],
        };
        assert!(p.unreachable("t").unwrap().is_empty());
    }

    #[test]
    fn unreachable_covers_every_scope() {
        // Scope::ALL is what keeps a newly added scope from escaping the lint. Each
        // scope is judged against its own envelope, so the u-boot scope gets one too.
        let p = PatchProfile {
            applies_to_kernel: Some(">=7.8, <8.0".into()),
            applies_to_uboot: Some(">=2026, <2027".into()),
            kernel: vec![],
            ffmpeg: vec![ranged("f/010-x.patch", "<7.0")],
            userspace: vec![ranged("u/010-y.patch", "<7.0")],
            uboot: vec![ranged("b/010-z.patch", "<2020")],
        };
        assert_eq!(p.unreachable("t").unwrap().len(), 3);
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
        assert!(matches!(err, ConfigError::KernelOutsideProfileRange { .. }));
    }

    #[test]
    fn invalid_range_is_typed_error() {
        let mut p = profile();
        p.applies_to_kernel = Some("not a range".into());
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
        let list = [
            "media-accel/kernel/040-a.patch",
            "media-accel/kernel/050-b.patch",
            "rocket/081-c.patch",
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
        let list = ["k/070-a.patch", "k/071-b.patch"];
        // Consecutive 070/071 leave no whole-number gap: fall back to a lettered
        // sub-prefix on the lower neighbor, which sorts between them.
        assert_eq!(derive_prefix(&list, 1).unwrap(), "070a");
        assert!("070-a.patch" < "070a-x.patch" && "070a-x.patch" < "071-b.patch");
    }

    #[test]
    fn derive_prefix_advances_the_suffix_letter() {
        // A second insert at the same slot skips the taken `a` and uses `b`.
        let list = ["k/070-a.patch", "k/070a-x.patch", "k/071-b.patch"];
        assert_eq!(derive_prefix(&list, 1).unwrap(), "070b");
    }

    #[test]
    fn derive_prefix_prepends_before_a_low_first_entry() {
        // Before `001` there is integer room (`000`); before `000` there is none.
        assert_eq!(derive_prefix(&["k/001-a.patch"], 0).unwrap(), "000");
        let err = derive_prefix(&["k/000-a.patch"], 0).unwrap_err();
        assert!(matches!(err, ConfigError::PatchPrefixNoGap { after: 0 }));
    }
}
