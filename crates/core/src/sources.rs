//! Source-pin durability *form* — the offline classification of a git
//! source pin from the lock's `(reference, commit)` alone.
//!
//! Pure: no network. A pin is re-fetchable from its URL only if the remote still
//! holds the commit, and the lock records both the `reference` a pin resolved from
//! and the exact `commit`. Comparing them tells the pin's *form* — a named ref
//! (tag/branch) versus a bare commit — which is the offline half of durability: a
//! bare-commit pin is undurable by construction (nothing but the commit anchors
//! it), while a named ref *may* be durable if it is a tag. Confirming which needs
//! the network, so the authoritative check is the engine's `verify-sources` probe;
//! this form is what the provenance manifest ([`crate::provenance`]) records so
//! durability is visible without a round-trip.

/// The offline durability *form* of a git source pin, derived from whether
/// its lock `reference` is a named ref or the bare commit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinForm {
    /// Pinned by a named ref — `reference` differs from `commit`, so a tag or
    /// branch was resolved. Durable *iff* that ref is a tag (immutable,
    /// shallow-fetchable forever); an ephemeral branch tip is not. Which one it is
    /// needs the network probe (`verify-sources`).
    NamedRef,
    /// Pinned by the bare commit — `reference` is the 40-hex commit itself, so no
    /// tag or branch anchors it. Undurable: re-fetchable only while some ref still
    /// reaches the commit, and unfetchable at all when it exists only in a local
    /// checkout (the mpp anti-pattern). This is the form the durable-base
    /// pattern exists to avoid.
    BareCommit,
}

impl PinForm {
    /// Classify a pin from its lock `reference` and exact `commit`. A pin
    /// whose `reference` is the full commit sha is a [`BareCommit`](PinForm::BareCommit);
    /// anything else resolved from a tag or branch, so it is a
    /// [`NamedRef`](PinForm::NamedRef). Pins are stored canonically lowercase
    /// ([`normalize_ref`]), so within a lock `reference == commit` holds byte-for-byte
    /// for a bare-commit pin.
    pub fn classify(reference: &str, commit: &str) -> PinForm {
        if reference == commit && is_full_sha(reference) {
            PinForm::BareCommit
        } else {
            PinForm::NamedRef
        }
    }

    /// A short stable token for the form, used in the provenance manifest:
    /// `"named-ref"` or `"bare-commit"`.
    pub fn as_str(self) -> &'static str {
        match self {
            PinForm::NamedRef => "named-ref",
            PinForm::BareCommit => "bare-commit",
        }
    }

    /// Whether this form is *definitely* undurable offline. A bare commit is; a
    /// named ref is only conditionally durable (tag yes, branch no), so it is not
    /// flagged here — `verify-sources` makes that call.
    pub fn is_undurable(self) -> bool {
        matches!(self, PinForm::BareCommit)
    }
}

/// True for a full 40-character hex sha1 commit id, in either case — the one
/// syntactic sha test shared across the workspace (the engine's git helpers use it
/// too). This is a *shape* check; canonicalization is separate ([`normalize_ref`]),
/// so a caller comparing against git's lowercase output normalizes first.
pub fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.bytes().all(|b| b.is_ascii_hexdigit())
}

/// True for a 64-character *lowercase* hex sha256 digest — the shape every
/// content pin (blob pins, `extra_debs` hashes, the manifest digest) is written
/// with, enforced where those pins are parsed. Lowercase-strict
/// because the generators (`sha256_hex`) emit lowercase and the pins are
/// compared as bytes.
pub fn is_sha256_hex(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Canonicalize a git reference for a pin: a full 40-hex sha is lowercased to git's
/// own output form, so a later byte-for-byte `HEAD == pinned` check holds; a tag or
/// branch name is returned unchanged. Applied where a user-supplied ref is ingested
/// into a pin, so a lock only ever records canonical commit ids (an uppercase sha a
/// user passes to `update` never survives into the lock to fail verification later).
pub fn normalize_ref(s: &str) -> String {
    if is_full_sha(s) {
        s.to_ascii_lowercase()
    } else {
        s.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_commit_when_reference_equals_full_sha() {
        let sha = "2cffdf6f332c3ddb93eb087841d78e8b487db2a3";
        assert_eq!(PinForm::classify(sha, sha), PinForm::BareCommit);
        assert!(PinForm::classify(sha, sha).is_undurable());
        assert_eq!(PinForm::classify(sha, sha).as_str(), "bare-commit");
    }

    #[test]
    fn named_ref_when_reference_is_a_tag_or_branch() {
        // A tag pin: ref differs from the commit it peels to.
        let form = PinForm::classify("v7.1.1", "c9acdc466e9aa96352f658b9276aa8a45b8e817d");
        assert_eq!(form, PinForm::NamedRef);
        assert!(!form.is_undurable());
        assert_eq!(form.as_str(), "named-ref");
        // A branch pin: same shape (a name, not the commit).
        assert_eq!(
            PinForm::classify("mainline-cma-fix", "95a6c48816d39b190be4b7333ad6fc249c08590c"),
            PinForm::NamedRef
        );
        // The durable mpp tag is a named ref, not a bare commit.
        assert_eq!(
            PinForm::classify(
                "v1.5.0-1-20260121-750e76e",
                "750e76ec2d9287babfaf08c8bf395ebc5e8778ea"
            ),
            PinForm::NamedRef
        );
    }

    #[test]
    fn a_short_hash_is_not_a_bare_commit() {
        // A ref that merely looks hex-ish but is not a full sha is a name.
        assert_eq!(PinForm::classify("95a6c488", "95a6c488"), PinForm::NamedRef);
    }

    #[test]
    fn normalize_ref_lowercases_full_shas_only() {
        let upper = "C9ACDC466E9AA96352F658B9276AA8A45B8E817D";
        let lower = "c9acdc466e9aa96352f658b9276aa8a45b8e817d";
        // A full sha canonicalizes to git's lowercase output form.
        assert_eq!(normalize_ref(upper), lower);
        assert_eq!(normalize_ref(lower), lower);
        // A normalized uppercase bare commit then classifies as a bare commit.
        let norm = normalize_ref(upper);
        assert_eq!(PinForm::classify(&norm, &norm), PinForm::BareCommit);
        // Tags and branches are untouched (case is meaningful in a ref name).
        assert_eq!(normalize_ref("v7.1.1"), "v7.1.1");
        assert_eq!(normalize_ref("Feature-Branch"), "Feature-Branch");
        // A short hex-ish string is a name, not a sha, so it is left as-is.
        assert_eq!(normalize_ref("95a6c488"), "95a6c488");
    }
}
