//! What a remote holds beyond a pin — the pure half of the `outdated` survey.
//!
//! Pure and deterministic: given a lock's `(reference, commit)` and the refs a
//! remote advertises, decide whether anything newer exists and what it is. The
//! `git ls-remote` that produces those refs is the engine's job
//! (`boot2deb_engine::sources`); everything here is a comparison, so the whole
//! policy is unit-testable without a network.
//!
//! This is a different question from durability ([`crate::sources`] and
//! `verify-sources`), which asks whether a pin is still *re-fetchable*. A pin can be
//! perfectly durable and nine releases behind, or ephemeral and current. The two
//! reports read the same ref advertisement and answer separately.
//!
//! The policy in one line: **the pin states its own naming scheme, and only tags
//! spelled the same way are candidates.** No per-axis table of version patterns is
//! needed — [`TagShape`] reads the scheme off the pinned tag, so the kernel axis
//! compares `v7.1.6` against other `vX.Y.Z` tags, the Linux-libre axis compares
//! `sources/v7.1.6-gnu` only against other deblobbed trees, and u-boot's `vYYYY.MM`
//! falls out of the same rule. A release pin is never offered a prerelease.

use crate::sources::PinForm;
use crate::version::{parse_tag, TagShape};
use semver::Version;
use serde::Serialize;
use std::collections::BTreeMap;

/// One ref as `git ls-remote` advertises it: the full ref name and the object it
/// points at. Borrowed, so the caller's parsed advertisement is not copied.
///
/// `name` is the whole ref (`refs/tags/v7.1.6`, `refs/heads/master`), including the
/// `^{}` suffix on a peeled annotated-tag line — [`compare`] does the peeling, so a
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
    /// The tag, spelled as the remote advertises it (`v7.1.9`).
    pub tag: String,
    /// The commit it resolves to — the peeled target for an annotated tag, so it is
    /// the value an `update` would write into the lock.
    pub commit: String,
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
/// `reference` and `commit` are the lock's; `refs` is the parsed `ls-remote`
/// advertisement of the pin's configured URL. Total — every input yields a verdict,
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
    let mut newer: Vec<(Version, &String, &String)> = tags
        .iter()
        .filter(|(tag, _)| shape.matches(tag))
        .filter_map(|(tag, commit)| Some((parse_tag(tag).ok()?, tag, commit)))
        .filter(|(v, _, _)| want_pre || v.pre.is_empty())
        .filter(|(v, _, _)| *v > pinned)
        .collect();
    if newer.is_empty() {
        return Upgrade::Current;
    }
    // Ascending, tag name breaking a tie, so `count` is a position and the whole
    // verdict is deterministic when two spellings parse to one version.
    newer.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(b.1)));
    let at = |i: usize| Newer {
        tag: newer[i].1.clone(),
        commit: newer[i].2.clone(),
        count: i + 1,
    };
    let latest = at(newer.len() - 1);
    // The pin's own line: same major.minor, which for a kernel is its stable series.
    // Counted within the line, so "3 newer in 7.1.y" is a count of 7.1 releases and
    // not of everything upstream published since.
    let line = newer
        .iter()
        .enumerate()
        .filter(|(_, (v, _, _))| v.major == pinned.major && v.minor == pinned.minor)
        .enumerate()
        .map(|(within, (_, (_, tag, commit)))| Newer {
            tag: (*tag).clone(),
            commit: (*commit).clone(),
            count: within + 1,
        })
        .last();
    Upgrade::Behind { line, latest }
}

/// The commit each advertised tag resolves to, keyed by bare tag name.
///
/// An annotated tag advertises twice — the tag object, then the peeled `^{}` line
/// carrying the commit — and the peeled line is the one an `update` would pin, so it
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
        assert_eq!(line.commit, sha('c'));
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
        // The unpeeled line carries the tag object's sha, not the commit — reporting
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
            sha('c'),
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
    fn the_json_form_is_one_flat_object_tagged_by_status() {
        // The `--json` row is switched on by `status`, and the token must be the same
        // one the table prints, or a caller and a reader disagree about the verdict.
        let up = Upgrade::Behind {
            line: None,
            latest: Newer {
                tag: "v2026.10".into(),
                commit: sha('c'),
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
