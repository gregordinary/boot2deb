//! Re-pin ref selection — which git ref an `update` pins for one source axis,
//! given the caller's flag, the previous lock, and the config's declared ref.
//!
//! Pure: a three-way choice over strings, no network and no I/O. Resolving the
//! chosen ref to a commit is the engine's job.
//!
//! The config layers declare a *constraint* (`uboot_ref = "v2026.07"`,
//! `[userspace.mpp] ref = "v1.5.0-…"`) and the lock records the *exact pin*. Editing
//! a constraint is an authored decision that must reach every recipe on the next
//! `update`, or a bump committed to `boot-methods/` leaves the boards that already
//! have locks silently behind. So the config's ref is what an omitted flag takes.
//!
//! The exception is a lock pinned to a bare commit sha. No config layer can author
//! one — a 40-hex ref only ever arrives through an explicit `--<tree>-ref`, so it is
//! a deliberate hand-pin and re-reading the constraint over it would discard the
//! operator's choice and float the tree back to a branch tip. Those pins are left
//! alone; [`PinForm`](crate::sources::PinForm) draws the same named-ref/bare-commit
//! line for durability.
//!
//! An axis with no declared constraint at all (the kernel, whose config carries a
//! `track` rather than a concrete ref) does not come through here; its ref is
//! inherited from the lock directly.

use crate::sources::is_full_sha;

/// Choose the ref to pin for one source axis.
///
/// Precedence:
/// 1. `flag` — an explicit `--<tree>-ref`, which always wins.
/// 2. `locked`, when it is a bare commit sha — a hand-pin no config can have
///    authored, so a constraint change does not override it.
/// 3. `configured` — the config layer's declared ref, so an authored bump
///    propagates. Identical to `locked` whenever the constraint has not moved.
/// 4. `locked`, when `configured` is empty — the axis declares no constraint, so
///    there is nothing to prefer over the existing pin.
///
/// Returns the empty string when nothing declares the axis and no lock holds it,
/// which is how the caller spells "this build has no such tree"; the engine never
/// reads a ref for a tree the build does not carry.
///
/// `flag` is taken by value because it is moved straight into the engine's ref set;
/// the other two are borrows of config and lock.
pub fn pick_ref(flag: Option<String>, locked: Option<&str>, configured: &str) -> String {
    if let Some(f) = flag {
        return f;
    }
    match locked {
        Some(l) if is_full_sha(l) || configured.is_empty() => l.to_string(),
        _ => configured.to_string(),
    }
}

/// Whether choosing `chosen` over the existing `locked` pin is a *constraint*
/// bump — the config moved and the omitted-flag path followed it — as opposed to a
/// no-op re-pin or an explicit flag.
///
/// `update` reports these so a propagated bump is visible at pin time rather than
/// being discovered later as an unexplained ref change in the lock diff.
pub fn is_config_bump(locked: Option<&str>, chosen: &str, configured: &str) -> bool {
    matches!(locked, Some(l) if l != chosen && chosen == configured)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SHA: &str = "2cffdf6f332c3ddb93eb087841d78e8b487db2a3";

    #[test]
    fn an_explicit_flag_beats_both_the_lock_and_the_config() {
        assert_eq!(
            pick_ref(Some("v2026.10".into()), Some("v2026.04"), "v2026.07"),
            "v2026.10"
        );
        // Including over a hand-pinned bare commit: a later flag re-pins it.
        assert_eq!(
            pick_ref(Some("master".into()), Some(SHA), "master"),
            "master"
        );
    }

    #[test]
    fn an_authored_constraint_bump_propagates_to_an_existing_lock() {
        // The reported case: `boot-methods/rockchip-rkbin.toml` moves to v2026.07 and a
        // plain `update` carries every already-locked board with it.
        assert_eq!(pick_ref(None, Some("v2026.04"), "v2026.07"), "v2026.07");
        assert!(is_config_bump(Some("v2026.04"), "v2026.07", "v2026.07"));
    }

    #[test]
    fn an_unmoved_constraint_re_pins_the_same_ref() {
        assert_eq!(pick_ref(None, Some("master"), "master"), "master");
        assert_eq!(
            pick_ref(None, Some("v4l2-request-n8.1"), "v4l2-request-n8.1"),
            "v4l2-request-n8.1"
        );
        assert!(!is_config_bump(Some("master"), "master", "master"));
    }

    #[test]
    fn a_hand_pinned_bare_commit_survives_a_constraint_it_differs_from() {
        // librga/libmali/ffmpeg carry deliberate sha pins against a `master`
        // constraint. Re-reading the config over them would float the tree.
        assert_eq!(pick_ref(None, Some(SHA), "master"), SHA);
        assert_eq!(pick_ref(None, Some(SHA), "v4l2-request-n8.1"), SHA);
        assert!(!is_config_bump(Some(SHA), SHA, "master"));
    }

    #[test]
    fn an_uppercase_sha_is_still_recognized_as_a_hand_pin() {
        // `normalize_ref` lowercases on the way into a lock, but the shape test is
        // case-insensitive so a hand-edited lock is read the same way.
        let upper = SHA.to_ascii_uppercase();
        assert_eq!(pick_ref(None, Some(&upper), "master"), upper);
    }

    #[test]
    fn a_first_update_takes_the_config_constraint() {
        assert_eq!(pick_ref(None, None, "v2026.07"), "v2026.07");
        // …and reports nothing, having moved no existing pin.
        assert!(!is_config_bump(None, "v2026.07", "v2026.07"));
    }

    #[test]
    fn an_undeclared_axis_keeps_its_lock_and_otherwise_reads_empty() {
        assert_eq!(pick_ref(None, Some("v2026.04"), ""), "v2026.04");
        assert_eq!(pick_ref(None, None, ""), "");
    }
}
