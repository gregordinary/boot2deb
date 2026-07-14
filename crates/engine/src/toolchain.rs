//! Host toolchain identity for the Tier-2 output signature.
//!
//! The kernel and u-boot nodes cross-compile with the host's `<cross>gcc` and link
//! with its binutils, so those tools' identities are inputs that determine the
//! output `.deb`s. Folding them into the [artifact store](crate::artstore) key
//! keeps a build on one toolchain from restoring an artifact another produced —
//! "bias toward hashing more, not less". This is a build-time host probe, so it
//! lives in the engine, not in the pure lock-only `why-rebuild` plan.

use std::process::Command;

/// A stable identity string for the host toolchain used to cross-compile the
/// kernel and u-boot: the version lines of `<cross>gcc`, `<cross>as`, and
/// `<cross>ld`, joined.
///
/// Binutils is probed alongside the compiler: the produced bytes come
/// from the assembler and linker as much as from `gcc`, so a binutils upgrade
/// must invalidate a cached kernel/u-boot artifact rather than restore one built
/// by the old tools. A tool that cannot be run contributes a fallback naming it,
/// so distinct cross prefixes still yield distinct identities. `cross_compile`
/// is the `CROSS_COMPILE` prefix (e.g. `aarch64-linux-gnu-`) on a cross build,
/// `None` on a native build.
pub fn host_cc_identity(cross_compile: Option<&str>) -> String {
    let prefix = cross_compile.unwrap_or("");
    ["gcc", "as", "ld"]
        .map(|tool| tool_version_line(&format!("{prefix}{tool}")))
        .join(" | ")
}

/// The first line of `<tool> --version` (which carries the version), or a
/// fallback naming the tool when it cannot be run.
fn tool_version_line(tool: &str) -> String {
    match Command::new(tool).arg("--version").output() {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
            .lines()
            .next()
            .unwrap_or("")
            .trim()
            .to_string(),
        _ => format!("unknown-tool:{tool}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_identity_folds_all_three_tools_and_is_stable() {
        // The host has a native toolchain (a build prerequisite); the identity is
        // stable across calls and carries one segment per tool, so a
        // binutils change alone re-keys the cache.
        let a = host_cc_identity(None);
        let b = host_cc_identity(None);
        assert_eq!(a, b);
        assert_eq!(a.split(" | ").count(), 3, "{a}");
        assert!(!a.split(" | ").any(|seg| seg.is_empty()), "{a}");
    }

    #[test]
    fn a_missing_cross_toolchain_yields_a_prefix_specific_fallback() {
        // An implausible prefix cannot be run, so the fallback still distinguishes
        // it, per tool.
        let id = host_cc_identity(Some("boot2deb-no-such-triple-"));
        assert_eq!(
            id,
            "unknown-tool:boot2deb-no-such-triple-gcc | \
             unknown-tool:boot2deb-no-such-triple-as | \
             unknown-tool:boot2deb-no-such-triple-ld"
        );
    }
}
