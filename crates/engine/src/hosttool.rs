//! One contract for "is this host tool here, and which version?".
//!
//! Two things the engine and its tests both need, with exactly one implementation of
//! each so they cannot drift:
//!
//! - **Presence** ([`have`]) is whether the probe **spawns**. A tool that rejects the
//!   version flag and exits non-zero is still present — `e2fsck --version` exits 16
//!   and is the reason this rule is written down rather than assumed. Keying presence
//!   on exit status silently reports such a tool absent, which downgrades a real check
//!   into a skipped one.
//! - **Version** ([`version`]) is the first line the tool prints, from stdout *or*
//!   stderr, since which stream carries it is per-tool (`e2fsck -V` writes to stderr).
//!   Only for a caller that must gate on a version floor; presence never needs it.
//!
//! Side-effecting (it spawns processes), so it lives in the engine rather than `core`.

use std::process::Command;

/// True when `tool` is runnable on this host.
///
/// Presence is whether the probe **spawns**: a missing binary fails to exec with
/// `ENOENT`, while a present one that rejects `--version` merely exits non-zero. The
/// distinction is load-bearing — see the module docs.
pub fn have(tool: &str) -> bool {
    Command::new(tool).arg("--version").output().is_ok()
}

/// The first non-empty line `tool <flag>` prints, from stdout or stderr, or `None`
/// when the tool could not be spawned or printed nothing.
///
/// `flag` is per-tool because the conventional spelling is not universal: GNU tools
/// take `--version`, `e2fsck` takes `-V`. The exit status is ignored for the same
/// reason [`have`] ignores it.
pub fn version(tool: &str, flag: &str) -> Option<String> {
    let out = Command::new(tool).arg(flag).output().ok()?;
    // stdout first: a tool that writes to both (a banner plus a warning) should be
    // read by the stream carrying the banner.
    [out.stdout, out.stderr].iter().find_map(|stream| {
        String::from_utf8_lossy(stream)
            .lines()
            .find(|l| !l.trim().is_empty())
            .map(str::to_string)
    })
}

/// Parse a `MAJOR.MINOR` pair out of a version line, from the first token that looks
/// like a dotted number (`"e2fsck 1.47.0 (5-Feb-2023)"` → `(1, 47)`).
///
/// Pure, so a version-floor gate is unit-testable against real banner text rather than
/// against whatever the test host happens to carry.
pub fn major_minor(line: &str) -> Option<(u32, u32)> {
    line.split_whitespace().find_map(|token| {
        // Trim the punctuation a banner wraps a version in, then require the first two
        // dot-separated components to be numeric. A token like `(5-Feb-2023)` has no
        // second numeric component and is skipped.
        let token = token.trim_matches(|c: char| !c.is_ascii_digit() && c != '.');
        let mut parts = token.split('.');
        let major = parts.next()?.parse().ok()?;
        let minor = parts.next()?.parse().ok()?;
        Some((major, minor))
    })
}

/// Every self-skipping test's one gate: `true` when all of `tools` are present.
///
/// When something is missing the behaviour depends on `BOOT2DEB_REQUIRE_HOST_TOOLS`:
/// a CI job that guarantees the tools sets it, and a miss then **panics**, so the
/// assertions those tests carry cannot silently drop out of the run. Unset (a
/// tool-minimal dev host), the caller skips with a printed note.
///
/// One implementation, because a test that skips quietly is a coverage regression that
/// looks exactly like a pass — and it has happened here before.
#[cfg(test)]
pub(crate) fn require(tools: &[&str]) -> bool {
    let missing: Vec<&str> = tools.iter().copied().filter(|t| !have(t)).collect();
    if missing.is_empty() {
        return true;
    }
    assert!(
        std::env::var_os("BOOT2DEB_REQUIRE_HOST_TOOLS").is_none(),
        "BOOT2DEB_REQUIRE_HOST_TOOLS is set but required host tools are missing: \
         {missing:?} — this CI job must provide them so these assertions do not skip"
    );
    eprintln!("skipping: required host tools unavailable: {missing:?}");
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn presence_is_whether_the_probe_spawns_not_its_exit_status() {
        assert!(have("sh"));
        assert!(!have("boot2deb-definitely-not-a-real-binary"));
        // The case the rule exists for: `e2fsck --version` is an invalid option and
        // exits 16. An exit-status probe would call a present e2fsck absent and quietly
        // drop the image cross-check.
        if have("e2fsck") {
            let status = std::process::Command::new("e2fsck")
                .arg("--version")
                .output()
                .unwrap()
                .status;
            assert!(
                !status.success(),
                "e2fsck --version is expected to fail; if it now succeeds this test has \
                 stopped covering the exit-status trap"
            );
        }
    }

    #[test]
    fn major_minor_reads_real_version_banners() {
        assert_eq!(major_minor("e2fsck 1.47.0 (5-Feb-2023)"), Some((1, 47)));
        assert_eq!(major_minor("tar (GNU tar) 1.35"), Some((1, 35)));
        assert_eq!(
            major_minor("Debian 'dpkg-deb' package archive backend version 1.22.6 (amd64)."),
            Some((1, 22))
        );
        // A two-component version is enough; a bare word or a date-shaped token is not.
        assert_eq!(major_minor("something 2.5"), Some((2, 5)));
        assert_eq!(major_minor("no version here"), None);
        assert_eq!(major_minor("built (5-Feb-2023)"), None);
    }

    #[test]
    fn version_reads_the_line_whichever_stream_carries_it() {
        // `tar --version` writes to stdout...
        if have("tar") {
            let line = version("tar", "--version").expect("tar prints a version");
            assert!(line.contains("tar"), "{line}");
        }
        // ...and `e2fsck -V` writes to stderr. One helper has to read both.
        if have("e2fsck") {
            let line = version("e2fsck", "-V").expect("e2fsck prints a version");
            assert!(line.starts_with("e2fsck "), "{line}");
            assert!(major_minor(&line).is_some(), "{line}");
        }
        assert_eq!(
            version("boot2deb-definitely-not-a-real-binary", "--version"),
            None
        );
    }
}
