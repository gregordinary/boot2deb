//! What upstream holds beyond a pin — the pure half of the `outdated` survey.
//!
//! Pure and deterministic: given a pin and what its upstream lists, decide whether
//! anything newer exists and what it is. Fetching the listing is the engine's job
//! (`boot2deb_engine::sources`, `boot2deb_engine::netfetch`). Everything here is a
//! comparison, so the whole policy is unit-testable without a network.
//!
//! A lock pins four kinds of thing, and each is compared against the listing its
//! upstream gives:
//!
//! - A git source's `(reference, commit)`, against the refs the remote advertises
//!   ([`compare`]).
//! - A firmware blob's file name, against the files of the upstream tree directory it
//!   came from ([`compare_file`]).
//! - A pool `.deb`'s file name, against the files its pool directory's index lists
//!   ([`compare_deb`]).
//! - A .NET SDK version, against the SDK versions its channel has released
//!   ([`compare_dotnet_sdk`]).
//!
//! This is a different question from durability ([`crate::sources`] and
//! `verify-sources`), which asks whether a pin is still *re-fetchable*. A pin can be
//! perfectly durable and nine releases behind, or ephemeral and current. The two
//! reports read the same ref advertisement and answer separately.
//!
//! The policy in one line: **the pin states its own naming scheme, and only tags
//! spelled the same way are candidates.** No per-axis table of version patterns is
//! needed. [`TagShape`] reads the scheme off the pinned tag, so the kernel axis
//! compares `v7.1.6` against other `vX.Y.Z` tags. The Linux-libre axis compares
//! `sources/v7.1.6-gnu` only against other deblobbed trees, and u-boot's `vYYYY.MM`
//! and FFmpeg's `nX.Y.Z` fall out of the same rule. A release pin is never offered a
//! prerelease. A file pin follows the same rule: its name, read around the version,
//! is the only scheme a candidate is compared under.

use crate::sources::PinForm;
use crate::version::{parse_tag, TagShape};
use semver::Version;
use serde::Serialize;
use std::collections::BTreeMap;

/// One ref as `git ls-remote` advertises it: the full ref name and the object it
/// points at. Borrowed, so the caller's parsed advertisement is not copied.
///
/// `name` is the whole ref (`refs/tags/v7.1.6`, `refs/heads/master`), including the
/// `^{}` suffix on a peeled annotated-tag line. [`compare`] does the peeling, so a
/// caller hands over the advertisement exactly as it was parsed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RemoteRef<'a> {
    /// The full ref name as advertised.
    pub name: &'a str,
    /// The commit (or tag object) sha it points at.
    pub commit: &'a str,
}

/// A release newer than the pin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Newer {
    /// The release as upstream spells it: a tag (`v7.1.9`), a file name
    /// (`rk3588_bl31_v1.56.elf`, `libicu76_76.1-4+b1_arm64.deb`), or a version
    /// (`10.0.402`).
    pub tag: String,
    /// The commit it resolves to. For a tag that is the peeled target of an annotated
    /// one, so it is the value an `update` would write into the lock. For a file read
    /// out of a git tree it is the commit the tree was listed at. `None` where upstream
    /// is not a git repository.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    /// How many comparable releases sit strictly between the pin and this one,
    /// inclusive of it: `1` means this is the very next release.
    pub count: usize,
}

/// What the remote holds beyond one pin.
///
/// Serialized internally-tagged (`{"status": "behind", ...}`) so the `--json` form
/// of a row is one flat object a caller can switch on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "kebab-case")]
pub enum Upgrade {
    /// Nothing newer: the pinned tag is the newest comparable release upstream, or
    /// the branch the pin names is still at the pinned commit.
    Current,
    /// Newer releases exist.
    Behind {
        /// The newest release sharing the pin's `major.minor` — the conservative
        /// bump, which for a kernel is the next stable point release. `None` when the
        /// pin's own line has nothing newer, so every upgrade available is a line
        /// change.
        line: Option<Newer>,
        /// The newest comparable release overall. Equal to
        /// [`line`](Self::Behind::line) when the newest release is in the pin's line.
        latest: Newer,
    },
    /// The pin names a branch whose tip has moved.
    ///
    /// Deliberately not "N commits behind": counting needs the repo's history, and
    /// this survey deliberately buys its whole answer with one `ls-remote` per
    /// remote. The tip's identity is what the advertisement can say honestly.
    TipMoved {
        /// The branch's current tip commit.
        commit: String,
    },
    /// No comparison was possible, with the reason — a bare-commit pin, a ref the
    /// remote no longer advertises, or a ref that names no version.
    Unknown {
        /// Why nothing could be said, phrased for an operator.
        why: String,
    },
}

impl Upgrade {
    /// The short status token: `current` / `behind` / `tip-moved` / `unknown`.
    /// Matches the serialized `status` field, so a table and a JSON document
    /// cannot drift apart.
    pub fn label(&self) -> &'static str {
        match self {
            Upgrade::Current => "current",
            Upgrade::Behind { .. } => "behind",
            Upgrade::TipMoved { .. } => "tip-moved",
            Upgrade::Unknown { .. } => "unknown",
        }
    }

    /// Whether something newer exists upstream — a newer release, or a branch tip
    /// that has moved. False for [`Current`](Upgrade::Current) and for
    /// [`Unknown`](Upgrade::Unknown), which is an absence of evidence, not evidence
    /// of currency.
    pub fn is_behind(&self) -> bool {
        matches!(self, Upgrade::Behind { .. } | Upgrade::TipMoved { .. })
    }
}

/// Compare one pin against what `refs` advertises.
///
/// `reference` and `commit` are the lock's. `refs` is the parsed `ls-remote`
/// advertisement of the pin's configured URL. Total: every input yields a verdict,
/// because a survey across every recipe must not fail on one repo with an odd tag.
///
/// The three pin forms are answered differently, and which one this is comes from
/// the advertisement rather than from guessing at the string:
///
/// - A **tag** pin is compared by version against the tags spelled like it
///   ([`TagShape`]), excluding prereleases unless the pin is itself one.
/// - A **branch** pin is compared by commit against the branch's current tip.
/// - A **bare-commit** pin ([`PinForm::BareCommit`]) has no upstream ref to follow,
///   so it is [`Unknown`](Upgrade::Unknown) — its real problem is durability, which
///   `verify-sources` reports.
pub fn compare(reference: &str, commit: &str, refs: &[RemoteRef]) -> Upgrade {
    if PinForm::classify(reference, commit) == PinForm::BareCommit {
        return Upgrade::Unknown {
            why: "pinned by bare commit — there is no upstream ref to follow".into(),
        };
    }
    let tags = tag_targets(refs);
    if tags.contains_key(reference) {
        return compare_tag(reference, &tags);
    }
    if let Some(tip) = branch_tip(refs, reference) {
        return if tip == commit {
            Upgrade::Current
        } else {
            Upgrade::TipMoved {
                commit: tip.to_string(),
            }
        };
    }
    Upgrade::Unknown {
        why: format!(
            "the remote advertises no tag or branch named '{reference}' — \
             run `boot2deb verify-sources` to see whether the pin is still fetchable"
        ),
    }
}

/// Compare a tag pin against every tag of the same shape.
///
/// The pinned tag is known present in `tags`; a tag whose own version does not parse
/// is [`Unknown`](Upgrade::Unknown), since there is no ordering to place it in.
fn compare_tag(reference: &str, tags: &BTreeMap<String, String>) -> Upgrade {
    let pinned = match parse_tag(reference) {
        Ok(v) => v,
        Err(_) => {
            return Upgrade::Unknown {
                why: format!(
                    "the pinned tag '{reference}' names no version, so nothing orders against it"
                ),
            }
        }
    };
    let shape = TagShape::of(reference);
    // A release pin is never offered a prerelease: an `-rc` is not an upgrade, it is
    // a different question. A pin that is *itself* a prerelease has already answered
    // that question, so its own line stays comparable.
    let want_pre = !pinned.pre.is_empty();
    let newer: Vec<Candidate<Version>> = tags
        .iter()
        .filter(|(tag, _)| shape.matches(tag))
        .filter_map(|(tag, commit)| Some((parse_tag(tag).ok()?, tag, commit)))
        .filter(|(v, _, _)| want_pre || v.pre.is_empty())
        .filter(|(v, _, _)| *v > pinned)
        .map(|(key, tag, commit)| Candidate {
            key,
            name: tag.clone(),
            commit: Some(commit.clone()),
        })
        .collect();
    // The pin's own line: same major.minor, which for a kernel is its stable series.
    verdict(newer, |v| {
        v.major == pinned.major && v.minor == pinned.minor
    })
}

/// One release newer than a pin, ordered by `key`.
struct Candidate<K> {
    /// What orders it against the other candidates.
    key: K,
    /// How upstream spells it.
    name: String,
    /// The commit it resolves to, where upstream is a git repository.
    commit: Option<String>,
}

/// The verdict over every release newer than a pin: [`Upgrade::Current`] when there
/// is none, else the newest overall and the newest that `in_line` keeps.
///
/// Counts are positions in ascending order, the name breaking a tie, so the verdict is
/// deterministic when two spellings order alike. The in-line count is taken within the
/// line, so "3 newer in 7.1.y" counts 7.1 releases and not everything upstream
/// published since.
fn verdict<K: Ord>(mut newer: Vec<Candidate<K>>, in_line: impl Fn(&K) -> bool) -> Upgrade {
    if newer.is_empty() {
        return Upgrade::Current;
    }
    newer.sort_by(|a, b| a.key.cmp(&b.key).then_with(|| a.name.cmp(&b.name)));
    let newest = newer.last().expect("checked non-empty");
    let latest = Newer {
        tag: newest.name.clone(),
        commit: newest.commit.clone(),
        count: newer.len(),
    };
    let line = newer
        .iter()
        .filter(|c| in_line(&c.key))
        .enumerate()
        .map(|(within, c)| Newer {
            tag: c.name.clone(),
            commit: c.commit.clone(),
            count: within + 1,
        })
        .last();
    Upgrade::Behind { line, latest }
}

/// A versioned file name split around its version: `rk3588_bl31_v1.51.elf` is
/// `rk3588_bl31_v`, `[1, 51]` and `.elf`.
///
/// The version is the last run of dot-separated numbers that follows a `v`, which is
/// how the Rockchip firmware files spell theirs. A name with no such run has no version.
fn split_file_version(name: &str) -> Option<(&str, Vec<u64>, &str)> {
    let bytes = name.as_bytes();
    let mut found = None;
    for (i, _) in name.match_indices('v') {
        let mut end = i + 1;
        let mut parts = Vec::new();
        loop {
            let start = end;
            while end < bytes.len() && bytes[end].is_ascii_digit() {
                end += 1;
            }
            if end == start {
                break;
            }
            parts.push(name[start..end].parse::<u64>().ok()?);
            // A dot continues the version only when a digit follows it, so the `.elf`
            // after `1.51` stays in the suffix.
            if end + 1 < bytes.len() && bytes[end] == b'.' && bytes[end + 1].is_ascii_digit() {
                end += 1;
            } else {
                break;
            }
        }
        if !parts.is_empty() {
            found = Some((&name[..=i], parts, &name[end..]));
        }
    }
    found
}

/// Compare a pinned file against the names an upstream directory lists.
///
/// The pinned name states its own scheme, as a pinned tag does: only names with the
/// same text around the version are candidates. So `rk3588_bl31_v1.51.elf` is
/// compared with `rk3588_bl31_v1.56.elf` and never with
/// `rk3588_bl31_primary_v1.60.elf`, a different file that happens to share a family.
/// A file's line is its major version. `listed_at` is the commit the listing was read
/// at, recorded on every candidate.
pub fn compare_file(pinned: &str, listing: &[&str], listed_at: Option<&str>) -> Upgrade {
    let Some((prefix, version, suffix)) = split_file_version(pinned) else {
        return Upgrade::Unknown {
            why: format!(
                "the pinned file '{pinned}' contains no version, so nothing orders against it"
            ),
        };
    };
    let newer = listing
        .iter()
        .filter_map(|name| {
            let (p, v, s) = split_file_version(name)?;
            (p == prefix && s == suffix && v > version).then(|| Candidate {
                key: v,
                name: (*name).to_string(),
                commit: listed_at.map(str::to_owned),
            })
        })
        .collect();
    verdict(newer, |v| v.first() == version.first())
}

/// A Debian version, ordered the way dpkg orders them.
#[derive(PartialEq, Eq)]
struct DebVersion(String);

impl PartialOrd for DebVersion {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for DebVersion {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        crate::debdep::compare_versions(&self.0, &other.0)
    }
}

/// The `(package, version, architecture)` a pool file name `<package>_<version>_<arch>.deb`
/// spells, with any percent-encoding a directory index applied undone.
fn split_deb_name(name: &str) -> Option<(String, String, String)> {
    let name = percent_decode(name);
    let stem = name.strip_suffix(".deb")?;
    let (package, rest) = stem.split_once('_')?;
    let (version, arch) = rest.rsplit_once('_')?;
    Some((package.into(), version.into(), arch.into()))
}

/// The upstream version of a Debian version: no epoch, no revision.
fn upstream_version(version: &str) -> &str {
    let v = version.split_once(':').map_or(version, |(_, v)| v);
    v.rsplit_once('-').map_or(v, |(u, _)| u)
}

/// Compare a pinned pool file against the files its pool directory lists.
///
/// Only the same package for the same architecture is a candidate, ordered as dpkg
/// orders versions. The line is the pin's upstream version. A rebuild or a new Debian
/// revision (`76.1-4` to `76.1-4+b1`) is then the conservative move, and a new
/// upstream release the wider one.
pub fn compare_deb(pinned: &str, listing: &[&str]) -> Upgrade {
    let Some((package, version, arch)) = split_deb_name(pinned) else {
        return Upgrade::Unknown {
            why: format!("'{pinned}' is not a <package>_<version>_<arch>.deb file name"),
        };
    };
    let pinned_version = DebVersion(version.clone());
    let newer = listing
        .iter()
        .filter_map(|name| {
            let (p, v, a) = split_deb_name(name)?;
            let key = DebVersion(v);
            (p == package && a == arch && key > pinned_version).then(|| Candidate {
                key,
                name: percent_decode(name),
                commit: None,
            })
        })
        .collect();
    verdict(newer, |v| {
        upstream_version(&v.0) == upstream_version(&version)
    })
}

/// Compare a pinned .NET SDK version against the SDK versions its channel has
/// released.
///
/// The line is the pin's feature band (`10.0.4xx`), the band the SDK's own servicing
/// releases stay in. A prerelease is offered only to a prerelease pin.
pub fn compare_dotnet_sdk(pinned: &str, released: &[&str]) -> Upgrade {
    let Ok(pin) = Version::parse(pinned) else {
        return Upgrade::Unknown {
            why: format!("the pinned SDK '{pinned}' is not a version"),
        };
    };
    let want_pre = !pin.pre.is_empty();
    let band = |v: &Version| (v.major, v.minor, v.patch / 100);
    let newer = released
        .iter()
        .filter_map(|r| Some((Version::parse(r).ok()?, *r)))
        .filter(|(v, _)| (want_pre || v.pre.is_empty()) && *v > pin)
        .map(|(key, name)| Candidate {
            key,
            name: name.to_string(),
            commit: None,
        })
        .collect();
    verdict(newer, |v| band(v) == band(&pin))
}

/// The channel a .NET SDK version belongs to (`10.0.401` is in `10.0`), which names
/// the release metadata that lists its siblings.
pub fn dotnet_channel(version: &str) -> Option<String> {
    let v = Version::parse(version).ok()?;
    Some(format!("{}.{}", v.major, v.minor))
}

/// Every SDK version a channel's `releases.json` lists, in document order.
///
/// # Errors
///
/// A document that is not JSON, or has no `releases` array.
pub fn dotnet_sdk_versions(releases_json: &str) -> Result<Vec<String>, String> {
    let doc: serde_json::Value =
        serde_json::from_str(releases_json).map_err(|e| format!("not JSON: {e}"))?;
    let releases = doc
        .get("releases")
        .and_then(|r| r.as_array())
        .ok_or("no releases array")?;
    let mut out = Vec::new();
    for release in releases {
        let listed = release
            .get("sdks")
            .and_then(|s| s.as_array())
            .into_iter()
            .flatten()
            .chain(release.get("sdk"));
        for sdk in listed {
            if let Some(v) = sdk.get("version").and_then(|v| v.as_str()) {
                if !out.iter().any(|o| o == v) {
                    out.push(v.to_string());
                }
            }
        }
    }
    Ok(out)
}

/// The file names an HTTP directory index links to: every `href` that names a plain
/// entry of the directory itself, percent-decoded. A link with a `/`, `?` or `#` is a
/// subdirectory, a sort control or an anchor, and is not one.
pub fn index_names(html: &str) -> Vec<String> {
    html.split("href=\"")
        .skip(1)
        .filter_map(|rest| rest.split('"').next())
        .filter(|href| !href.is_empty() && !href.contains(['/', '?', '#']))
        .map(percent_decode)
        .collect()
}

/// Undo URL percent-encoding. A `%` not followed by two hex digits stays as it is.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        let hex = |b: u8| (b as char).to_digit(16);
        match (
            bytes[i],
            bytes.get(i + 1).copied().and_then(hex),
            bytes.get(i + 2).copied().and_then(hex),
        ) {
            (b'%', Some(hi), Some(lo)) => {
                out.push((hi * 16 + lo) as u8);
                i += 3;
            }
            (b, _, _) => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The commit each advertised tag resolves to, keyed by bare tag name.
///
/// An annotated tag advertises twice — the tag object, then the peeled `^{}` line
/// containing the commit — and the peeled line is the one an `update` would pin, so it
/// wins wherever both are present.
fn tag_targets<'a>(refs: &[RemoteRef<'a>]) -> BTreeMap<String, String> {
    let mut peeled: BTreeMap<String, String> = BTreeMap::new();
    let mut plain: BTreeMap<String, String> = BTreeMap::new();
    for r in refs {
        let Some(tag) = r.name.strip_prefix("refs/tags/") else {
            continue;
        };
        match tag.strip_suffix("^{}") {
            Some(base) => {
                peeled.insert(base.to_string(), r.commit.to_string());
            }
            None => {
                plain.insert(tag.to_string(), r.commit.to_string());
            }
        }
    }
    plain.extend(peeled);
    plain
}

/// The current tip of the branch named `reference`, if the remote advertises one.
fn branch_tip<'a>(refs: &[RemoteRef<'a>], reference: &str) -> Option<&'a str> {
    let want = format!("refs/heads/{reference}");
    refs.iter().find(|r| r.name == want).map(|r| r.commit)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A sha of `n` repeated, so a fixture's commits are distinguishable at a glance.
    fn sha(n: char) -> String {
        std::iter::repeat_n(n, 40).collect()
    }

    /// Build an advertisement from `(name, commit)` pairs.
    fn advert<'a>(pairs: &'a [(String, String)]) -> Vec<RemoteRef<'a>> {
        pairs
            .iter()
            .map(|(name, commit)| RemoteRef { name, commit })
            .collect()
    }

    /// The linux-stable shape: a pinned 7.1.6, later 7.1 point releases, a newer
    /// stable line, and a prerelease of the line after that.
    fn kernel_refs() -> Vec<(String, String)> {
        vec![
            ("refs/tags/v7.1.6".into(), sha('a')),
            ("refs/tags/v7.1.7".into(), sha('b')),
            ("refs/tags/v7.1.9".into(), sha('c')),
            ("refs/tags/v7.2".into(), sha('d')),
            ("refs/tags/v7.2.1".into(), sha('e')),
            ("refs/tags/v7.3-rc1".into(), sha('f')),
            ("refs/heads/master".into(), sha('0')),
        ]
    }

    #[test]
    fn a_stable_pin_reports_its_own_line_and_the_newest_release() {
        // The two answers an operator wants are different: the conservative bump
        // stays in 7.1.y, and the newest upstream release is a line change that
        // needs the patch series re-checked. Reporting only one of them either
        // hides the safe move or hides the real state of upstream.
        let refs = kernel_refs();
        let up = compare("v7.1.6", &sha('a'), &advert(&refs));
        let Upgrade::Behind { line, latest } = &up else {
            panic!("expected behind, got {up:?}");
        };
        let line = line.as_ref().expect("7.1 has newer point releases");
        assert_eq!(line.tag, "v7.1.9");
        assert_eq!(line.commit, Some(sha('c')));
        assert_eq!(line.count, 2, "7.1.7 and 7.1.9 are the in-line releases");
        assert_eq!(latest.tag, "v7.2.1");
        assert_eq!(latest.count, 4, "7.1.7, 7.1.9, 7.2 and 7.2.1");
        assert!(up.is_behind());
        assert_eq!(up.label(), "behind");
    }

    #[test]
    fn a_release_pin_is_never_offered_a_prerelease() {
        // The plan's sharpest requirement: a stable-series pin must not report an
        // `-rc` as an upgrade. 7.3-rc1 is the newest tag in the fixture by tag name
        // and must not be the answer.
        let refs = kernel_refs();
        let Upgrade::Behind { latest, .. } = compare("v7.1.6", &sha('a'), &advert(&refs)) else {
            panic!("expected behind");
        };
        assert_eq!(latest.tag, "v7.2.1");

        // Pinned at the newest release, with only a prerelease above it: current.
        assert_eq!(
            compare("v7.2.1", &sha('e'), &advert(&refs)),
            Upgrade::Current
        );

        // A pin that is *itself* a prerelease has already taken that decision, so its
        // own line stays comparable — otherwise an rc pin could never be told that a
        // later rc, or the release, exists.
        let rcs = vec![
            ("refs/tags/v7.3-rc1".into(), sha('f')),
            ("refs/tags/v7.3-rc2".into(), sha('g')),
        ];
        let Upgrade::Behind { latest, .. } = compare("v7.3-rc1", &sha('f'), &advert(&rcs)) else {
            panic!("an rc pin must still see a later rc");
        };
        assert_eq!(latest.tag, "v7.3-rc2");
    }

    #[test]
    fn only_tags_spelled_like_the_pin_are_candidates() {
        // Both schemes live in one repo and their versions interleave, so comparing
        // every parseable tag would offer a deblobbed tree to a build that pinned an
        // ordinary kernel — an "upgrade" that swaps out the whole firmware posture.
        let refs = vec![
            ("refs/tags/v7.1.6".into(), sha('a')),
            ("refs/tags/v7.1.9".into(), sha('b')),
            ("refs/tags/sources/v7.1.6-gnu".into(), sha('c')),
            ("refs/tags/sources/v7.2-gnu".into(), sha('d')),
        ];
        let Upgrade::Behind { latest, .. } = compare("v7.1.6", &sha('a'), &advert(&refs)) else {
            panic!("expected behind");
        };
        assert_eq!(
            latest.tag, "v7.1.9",
            "the -gnu trees are a different scheme"
        );

        let Upgrade::Behind { latest, .. } =
            compare("sources/v7.1.6-gnu", &sha('c'), &advert(&refs))
        else {
            panic!("expected behind");
        };
        assert_eq!(latest.tag, "sources/v7.2-gnu");
    }

    #[test]
    fn an_ffmpeg_pin_reports_its_point_release_line() {
        // FFmpeg spells releases `nX.Y.Z`, has `-dev` tags on master, and keeps
        // the request-API fork's branches in other repos. The line answer is the
        // point release a graft written against 8.1 can take; the newest release
        // is a generation change it cannot take unreviewed.
        let refs = vec![
            ("refs/tags/n8.1.3".into(), sha('a')),
            ("refs/tags/n8.1.3^{}".into(), sha('b')),
            ("refs/tags/n8.1.4".into(), sha('c')),
            ("refs/tags/n9.0.2".into(), sha('d')),
            ("refs/tags/n9.1-dev".into(), sha('e')),
            ("refs/heads/release/8.1".into(), sha('c')),
        ];
        let up = compare("n8.1.3", &sha('b'), &advert(&refs));
        let Upgrade::Behind { line, latest } = &up else {
            panic!("expected behind, got {up:?}");
        };
        assert_eq!(
            line.as_ref().expect("8.1 has a newer point release").tag,
            "n8.1.4"
        );
        assert_eq!(latest.tag, "n9.0.2", "the -dev tag is a prerelease");
    }

    #[test]
    fn a_uboot_pin_compares_across_zero_padded_release_months() {
        // u-boot's `vYYYY.MM` is not semver — the padded month is rejected outright
        // until it is normalized — and every release is its own major.minor, so the
        // pin's line is empty and only the newest release is offered.
        let refs = vec![
            ("refs/tags/v2026.04".into(), sha('a')),
            ("refs/tags/v2026.07".into(), sha('b')),
            ("refs/tags/v2026.10".into(), sha('c')),
            ("refs/tags/v2027.01-rc1".into(), sha('d')),
        ];
        let up = compare("v2026.04", &sha('a'), &advert(&refs));
        let Upgrade::Behind { line, latest } = &up else {
            panic!("expected behind, got {up:?}");
        };
        assert_eq!(line, &None, "each u-boot release is its own line");
        assert_eq!(latest.tag, "v2026.10");
        assert_eq!(latest.count, 2);

        // A point release on the pinned month *is* in line, and is then the
        // conservative bump.
        let refs = vec![
            ("refs/tags/v2026.04".into(), sha('a')),
            ("refs/tags/v2026.04.1".into(), sha('b')),
            ("refs/tags/v2026.10".into(), sha('c')),
        ];
        let Upgrade::Behind { line, latest } = compare("v2026.04", &sha('a'), &advert(&refs))
        else {
            panic!("expected behind");
        };
        assert_eq!(line.unwrap().tag, "v2026.04.1");
        assert_eq!(latest.tag, "v2026.10");
    }

    #[test]
    fn an_annotated_tags_peeled_target_is_the_commit_reported() {
        // The unpeeled line contains the tag object's sha, not the commit — reporting
        // it would hand an operator a value no checkout resolves to.
        let refs = vec![
            ("refs/tags/v1.0".into(), sha('a')),
            ("refs/tags/v1.1".into(), sha('b')),
            ("refs/tags/v1.1^{}".into(), sha('c')),
        ];
        let Upgrade::Behind { latest, .. } = compare("v1.0", &sha('a'), &advert(&refs)) else {
            panic!("expected behind");
        };
        assert_eq!(latest.tag, "v1.1");
        assert_eq!(
            latest.commit,
            Some(sha('c')),
            "the peeled target, not the tag object"
        );
    }

    #[test]
    fn a_branch_pin_is_answered_by_its_tip() {
        let refs = vec![
            ("refs/heads/master".into(), sha('a')),
            ("refs/heads/develop".into(), sha('b')),
            ("refs/tags/v9.9".into(), sha('c')),
        ];
        // At the tip: current, and the repo's tags are irrelevant to a branch pin.
        assert_eq!(
            compare("master", &sha('a'), &advert(&refs)),
            Upgrade::Current
        );
        // Moved: the tip's identity is reported, and no distance is claimed — that
        // would need the history this survey does not fetch.
        assert_eq!(
            compare("master", &sha('z'), &advert(&refs)),
            Upgrade::TipMoved { commit: sha('a') }
        );
    }

    #[test]
    fn a_bare_commit_pin_and_a_vanished_ref_say_why_they_cannot_answer() {
        // Neither is "up to date", and calling either one current would be the worst
        // possible answer: it asserts currency from an absence of evidence.
        let refs = vec![("refs/heads/master".into(), sha('a'))];
        let bare = compare(&sha('d'), &sha('d'), &advert(&refs));
        assert_eq!(bare.label(), "unknown");
        assert!(!bare.is_behind());
        let Upgrade::Unknown { why } = &bare else {
            panic!("expected unknown")
        };
        assert!(why.contains("bare commit"), "{why}");

        let Upgrade::Unknown { why } = compare("v1.0", &sha('a'), &advert(&refs)) else {
            panic!("a ref the remote no longer advertises cannot be compared");
        };
        assert!(why.contains("verify-sources"), "{why}");
    }

    #[test]
    fn a_pinned_tag_naming_no_version_is_unknown_rather_than_current() {
        // The mpp anti-pattern's cousin: a tag that exists but encodes no version.
        // There is no ordering to place it in, so no upgrade can be derived — and
        // saying "current" would claim one.
        let refs = vec![
            ("refs/tags/nightly".into(), sha('a')),
            ("refs/tags/v2.0".into(), sha('b')),
        ];
        let Upgrade::Unknown { why } = compare("nightly", &sha('a'), &advert(&refs)) else {
            panic!("a versionless tag must not be ordered");
        };
        assert!(why.contains("nightly"), "{why}");
    }

    #[test]
    fn a_file_is_compared_only_with_files_spelled_like_it() {
        // The rkbin tree as it lists `bin/rk35`: the pinned version is gone, newer
        // ones of the same file are there, and so are other files of the same family.
        let listing = [
            "rk3588_bl31_v1.56.elf",
            "rk3588_bl31_v1.53.elf",
            "rk3588_bl31_primary_v1.60.elf",
            "rk3588_ddr_lp4_2112MHz_lp5_2400MHz_v1.24.bin",
            "rk3588_ddr_lp4_2112MHz_lp5_2400MHz_eyescan_v1.24.bin",
            "rk3588_bl31_v2.00.elf",
        ];
        let Upgrade::Behind { line, latest } =
            compare_file("rk3588_bl31_v1.51.elf", &listing, Some(&sha('e')))
        else {
            panic!("expected behind");
        };
        assert_eq!(latest.tag, "rk3588_bl31_v2.00.elf");
        assert_eq!(
            latest.count, 3,
            "v1.53, v1.56, v2.00 — never the primary_ file"
        );
        assert_eq!(
            latest.commit,
            Some(sha('e')),
            "the commit the tree was listed at"
        );
        let line = line.unwrap();
        assert_eq!(
            line.tag, "rk3588_bl31_v1.56.elf",
            "the line is the major version"
        );
        assert_eq!(line.count, 2);

        // The TPL's long name: the version is the last `v<numbers>` run, and the
        // `eyescan_` variant is a different file.
        let Upgrade::Behind { latest, .. } = compare_file(
            "rk3588_ddr_lp4_2112MHz_lp5_2400MHz_v1.19.bin",
            &listing,
            None,
        ) else {
            panic!("expected behind");
        };
        assert_eq!(latest.tag, "rk3588_ddr_lp4_2112MHz_lp5_2400MHz_v1.24.bin");
        assert_eq!(latest.count, 1);

        assert_eq!(
            compare_file("rk3588_bl31_v2.00.elf", &listing, None),
            Upgrade::Current
        );
        assert_eq!(
            compare_file("idbloader.img", &listing, None).label(),
            "unknown"
        );
    }

    #[test]
    fn a_pool_deb_is_compared_by_dpkg_order_within_its_package_and_arch() {
        let listing = [
            "libicu76_76.1-4_arm64.deb",
            "libicu76_76.1-4%2Bb1_arm64.deb",
            "libicu76_76.1-4+b1_amd64.deb",
            "libicu76_76.2-1_arm64.deb",
            "libicu78_78.1-1_arm64.deb",
            "libicu-dev_76.1-4_arm64.deb",
        ];
        let Upgrade::Behind { line, latest } = compare_deb("libicu76_76.1-4_arm64.deb", &listing)
        else {
            panic!("expected behind");
        };
        // A binNMU keeps the upstream version, so it is the conservative move.
        let line = line.unwrap();
        assert_eq!(line.tag, "libicu76_76.1-4+b1_arm64.deb", "percent-decoded");
        assert_eq!(latest.tag, "libicu76_76.2-1_arm64.deb");
        assert_eq!(latest.count, 2, "not the other arch, package or soname");
        assert_eq!(latest.commit, None);
        assert_eq!(
            compare_deb("libicu76_76.2-1_arm64.deb", &listing),
            Upgrade::Current
        );
    }

    #[test]
    fn an_sdk_is_offered_its_feature_band_before_a_newer_band() {
        let released = [
            "10.0.402",
            "10.0.500",
            "10.0.112",
            "10.0.401",
            "10.0.600-rc.1",
        ];
        let Upgrade::Behind { line, latest } = compare_dotnet_sdk("10.0.401", &released) else {
            panic!("expected behind");
        };
        assert_eq!(line.unwrap().tag, "10.0.402");
        assert_eq!(
            latest.tag, "10.0.500",
            "a release pin is offered no prerelease"
        );
        assert_eq!(compare_dotnet_sdk("10.0.500", &released), Upgrade::Current);
        assert_eq!(dotnet_channel("10.0.401").as_deref(), Some("10.0"));
    }

    #[test]
    fn listings_parse_out_of_their_documents() {
        let html = r#"<a href="?C=N;O=D">Name</a> <a href="/debian/pool/main/i/">Parent</a>
            <a href="libicu76_76.1-4_arm64.deb">x</a> <a href="libicu76_76.1-4%2Bb1_arm64.deb">y</a>"#;
        assert_eq!(
            index_names(html),
            ["libicu76_76.1-4_arm64.deb", "libicu76_76.1-4+b1_arm64.deb"]
        );
        let json = r#"{"releases": [
            {"sdk": {"version": "10.0.401"}, "sdks": [{"version": "10.0.401"}, {"version": "10.0.112"}]},
            {"sdk": {"version": "10.0.400"}}
        ]}"#;
        assert_eq!(
            dotnet_sdk_versions(json).unwrap(),
            ["10.0.401", "10.0.112", "10.0.400"]
        );
        assert!(dotnet_sdk_versions("{}").is_err());
    }

    #[test]
    fn the_json_form_is_one_flat_object_tagged_by_status() {
        // The `--json` row is switched on by `status`, and the token must be the same
        // one the table prints, or a caller and a reader disagree about the verdict.
        let up = Upgrade::Behind {
            line: None,
            latest: Newer {
                tag: "v2026.10".into(),
                commit: Some(sha('c')),
                count: 2,
            },
        };
        let json = serde_json::to_value(&up).unwrap();
        assert_eq!(json["status"], "behind");
        assert_eq!(json["status"], up.label());
        assert_eq!(json["latest"]["tag"], "v2026.10");
        assert_eq!(json["line"], serde_json::Value::Null);
        assert_eq!(
            serde_json::to_value(Upgrade::Current).unwrap()["status"],
            "current"
        );
        assert_eq!(
            serde_json::to_value(Upgrade::TipMoved { commit: sha('a') }).unwrap()["status"],
            "tip-moved"
        );
    }
}
