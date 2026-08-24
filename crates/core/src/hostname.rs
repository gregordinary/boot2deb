//! The shape of a host name.
//!
//! Its own module because two layers enforce it and they must not drift: a device's
//! `hostname`, which reaches the image, and the device **slug**, which is the default
//! that `hostname` falls back to. A slug held to a looser rule than the name derived
//! from it would put the generator's own default outside what the image accepts.
//!
//! Pure and host-independent; nothing here touches the filesystem.

/// Longest host name a Linux kernel will hold. The `utsname` node field is
/// `__NEW_UTS_LEN` + 1 bytes, so a longer name is one `sethostname` refuses and the
/// booted board silently does not have.
pub const MAX_HOSTNAME_LEN: usize = 64;

/// Longest single DNS label (RFC 1035). Smaller than [`MAX_HOSTNAME_LEN`], so a
/// dotless name is bounded by this rather than by the kernel's limit.
pub const MAX_LABEL_LEN: usize = 63;

/// Check `name` against the RFC 1123 host-name shape: one or more labels of
/// `[A-Za-z0-9-]`, each non-empty, at most [`MAX_LABEL_LEN`] characters, and neither
/// starting nor ending with `-`, joined by `.` into at most [`MAX_HOSTNAME_LEN`]
/// characters. Dots are allowed so a board may carry a fully qualified name.
///
/// A shape that fails returns *why* — a terse clause naming the offending property,
/// for the caller to wrap in the typed error that suits where the value was authored:
/// [`ConfigError::InvalidField`] with `what = "hostname"` for a device's `hostname`,
/// [`ConfigError::InvalidDeviceName`] for the slug. The clause says what is wrong, not
/// what goes wrong if it is ignored; that is this documentation's job, and repeating it
/// in every message would bury the part the author has to act on.
///
/// [`ConfigError::InvalidField`]: crate::ConfigError::InvalidField
/// [`ConfigError::InvalidDeviceName`]: crate::ConfigError::InvalidDeviceName
///
/// **Rejected, not repaired.** Debian's `hostname(5)` says systemd *filters* invalid
/// characters out of `/etc/hostname` when it sets the name, so an unchecked value does
/// not fail — the board boots under a different name than the one written. `/etc/hosts`
/// is generated from the authored value, so its `127.0.1.1` entry then maps a name the
/// running system does not have, and every lookup of the machine's own name misses.
/// Refusing the value is what keeps the two files describing one host.
pub fn check(name: &str) -> Result<(), &'static str> {
    if name.is_empty() {
        return Err("empty");
    }
    if name.len() > MAX_HOSTNAME_LEN {
        return Err("longer than 64 characters, which the kernel cannot hold");
    }
    for label in name.split('.') {
        if label.is_empty() {
            return Err("has an empty label — a leading, trailing, or doubled '.'");
        }
        if label.len() > MAX_LABEL_LEN {
            return Err("has a label longer than 63 characters, which DNS cannot carry");
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err("has a label starting or ending with '-'");
        }
        if !label.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err("labels are [A-Za-z0-9-]");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_the_shapes_a_board_may_carry() {
        for ok in [
            "turing-rk1",
            "h96-max-m9",
            "rk1",
            "a",
            "b0ard-1",
            "board.lan",
            "b0ard-1.example.com",
            &"a".repeat(63),
            &format!("{}.{}", "a".repeat(31), "b".repeat(32)),
        ] {
            check(ok).unwrap_or_else(|why| panic!("rejected {ok:?}: {why}"));
        }
    }

    #[test]
    fn rejects_what_etc_hosts_would_read_as_structure() {
        // A newline is the sharpest case: `/etc/hosts` is one entry per line, so
        // everything past it is a further mapping nobody wrote. Whitespace is the same
        // problem one field down, where the tail becomes an alias of 127.0.1.1.
        for bad in [
            "rk1\n10.0.0.1 deb.debian.org",
            "rk1 deb.debian.org",
            "rk1\tdeb.debian.org",
            "rk1\r",
            "rk1#comment",
        ] {
            assert!(check(bad).is_err(), "accepted {bad:?}");
        }
    }

    #[test]
    fn rejects_what_dns_and_the_kernel_cannot_carry() {
        for bad in [
            "",
            "-rk1",
            "rk1-",
            ".rk1",
            "rk1.",
            "board..lan",
            "rk_1",
            "a/b",
            "..",
            ".",
            &"a".repeat(65), // past the kernel's whole-name limit
            &"a".repeat(64), // one label past the DNS label limit
            &format!("{}.b", "a".repeat(64)),
        ] {
            assert!(check(bad).is_err(), "accepted {bad:?}");
        }
    }

    /// Every valid host name is also a bare identifier, which is what lets a device
    /// slug be held to this rule alone: the tighter shape already implies the
    /// filesystem-safety one the other config layers are checked against.
    #[test]
    fn a_valid_host_name_is_always_a_bare_identifier() {
        for ok in ["turing-rk1", "board.lan", "a", "b0ard-1.example.com"] {
            check(ok).unwrap();
            assert!(!ok.is_empty() && !ok.starts_with('.'));
            assert!(ok
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')));
        }
    }
}
