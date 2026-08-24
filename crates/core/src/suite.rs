//! Debian suite facts: which archive pockets a suite actually publishes.
//!
//! Pure and data-only, so the rootfs generator can ask rather than assume. The one
//! thing it knows is that the pocket set is a *property of the suite*, not a naming
//! convention every suite obeys: a released suite carries `-security` and `-updates`
//! alongside its base, and unstable carries neither.
//!
//! Getting this wrong is quiet — the image boots and every `apt update` on it errors
//! on sources the archive has never served.

/// One `/etc/apt/sources.list` entry: which archive to fetch from, and the suite
/// string on that line.
///
/// The archive is part of the answer because the pockets do not share one: the
/// security pocket lives on `debian-security`, everything else on `debian`. A caller
/// that only got suite suffixes back would have to re-derive that mapping.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pocket {
    /// Archive path component under the Debian mirror host — `debian` or
    /// `debian-security`.
    pub archive: &'static str,
    /// The suite this line names (`forky`, `forky-security`, `forky-updates`).
    pub suite: String,
}

/// The pockets `suite` publishes, in the order a `sources.list` lists them: the base
/// suite, then `-security`, then `-updates`.
///
/// Debian publishes no security or updates pocket for **unstable** — by design, since
/// unstable is where fixes land directly, so there is nowhere for a separate stream to
/// come from. `experimental` likewise has neither. Emitting them anyway ships an image
/// whose every `apt update` reports two 404s.
///
/// Any other suite is treated as released (or as testing, which does publish both).
/// That is the right default: a codename this does not recognise is far more likely to
/// be a current or future stable release than a second unstable-like suite, and the
/// failure modes are asymmetric — a missing pocket costs security updates, a spurious
/// one costs a visible error on every `apt update`.
pub fn pockets(suite: &str) -> Vec<Pocket> {
    let base = Pocket {
        archive: "debian",
        suite: suite.to_string(),
    };
    if is_rolling(suite) {
        return vec![base];
    }
    vec![
        base,
        Pocket {
            archive: "debian-security",
            suite: format!("{suite}-security"),
        },
        Pocket {
            archive: "debian",
            suite: format!("{suite}-updates"),
        },
    ]
}

/// True for the suites that never gain a `-security`/`-updates` pocket: unstable
/// (under either spelling) and `experimental`.
fn is_rolling(suite: &str) -> bool {
    matches!(suite, "sid" | "unstable" | "experimental")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_released_suite_publishes_all_three_pockets() {
        let p = pockets("forky");
        assert_eq!(
            p.iter().map(|p| p.suite.as_str()).collect::<Vec<_>>(),
            ["forky", "forky-security", "forky-updates"]
        );
        // The security pocket is served by its own archive; the other two are not.
        assert_eq!(
            p.iter().map(|p| p.archive).collect::<Vec<_>>(),
            ["debian", "debian-security", "debian"]
        );
        assert_eq!(pockets("trixie").len(), 3);
    }

    #[test]
    fn unstable_publishes_only_its_base_suite() {
        // Probed against the live archive: `sid` is 200, `sid-security` and
        // `sid-updates` are both 404. Emitting them shipped an image that errored on
        // two sources every time apt ran.
        for rolling in ["sid", "unstable", "experimental"] {
            assert_eq!(
                pockets(rolling),
                vec![Pocket {
                    archive: "debian",
                    suite: rolling.to_string()
                }],
                "{rolling} publishes no security or updates pocket"
            );
        }
    }
}
