//! Host toolchain identity for the Tier-2 output signature.
//!
//! The kernel and u-boot nodes cross-compile with the host's `<cross>gcc`, so that
//! compiler's identity is one of the inputs that determines their output `.deb`s.
//! Folding it into the [artifact store](crate::artstore) key keeps a build on one
//! compiler from restoring an artifact another compiler produced — "bias toward
//! hashing more, not less". This is a build-time host probe, so it lives in
//! the engine, not in the pure lock-only `why-rebuild` plan.

use std::process::Command;

/// A stable identity string for the host C toolchain used to cross-compile the
/// kernel and u-boot.
///
/// It is the first line of `<cross>gcc --version` (which carries the compiler
/// version), or — when that command cannot be run — a fallback naming the compiler,
/// so distinct cross prefixes still yield distinct identities. `cross_compile` is the
/// `CROSS_COMPILE` prefix (e.g. `aarch64-linux-gnu-`) on a cross build, `None` on a
/// native build.
pub fn host_cc_identity(cross_compile: Option<&str>) -> String {
    let cc = format!("{}gcc", cross_compile.unwrap_or(""));
    match Command::new(&cc).arg("--version").output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string(),
        _ => format!("unknown-cc:{cc}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_identity_is_nonempty_and_stable() {
        // The host has a native `gcc` (a build prerequisite); its identity is stable
        // across calls and non-empty.
        let a = host_cc_identity(None);
        let b = host_cc_identity(None);
        assert_eq!(a, b);
        assert!(!a.is_empty());
    }

    #[test]
    fn a_missing_cross_toolchain_yields_a_prefix_specific_fallback() {
        // An implausible prefix cannot be run, so the fallback still distinguishes it.
        let id = host_cc_identity(Some("boot2deb-no-such-triple-"));
        assert_eq!(id, "unknown-cc:boot2deb-no-such-triple-gcc");
    }
}
