//! Host toolchain identity: the versions of the host tools that produce a build's
//! compiled bytes, for the Tier-2 output signature and the provenance manifest.
//!
//! Two distinct toolchains produce a build's artifacts, and they are probed together
//! because an image carries output from both:
//!
//!  - The **host cross toolchain** compiles the kernel, u-boot, and the out-of-tree
//!    modules directly on the build host, so `<cross>gcc`, `<cross>as`, and
//!    `<cross>ld` determine those `.deb`s byte for byte.
//!  - The **`qemu-user` interpreter** executes the target-arch compiler inside the
//!    build sandbox on a cross host, so on such a host every userspace/ffmpeg `.deb`
//!    passes through it.
//!
//! Folding these into the [artifact store](crate::artstore) key keeps a build on one
//! toolchain from restoring an artifact another produced — "bias toward hashing more,
//! not less" — and recording them in the manifest is what lets an image's provenance
//! answer "which compiler built this kernel", which is the single largest host input
//! to the kernel bytes. This is a build-time host probe, so it lives in the engine, not
//! in the pure lock-only `why-rebuild` plan.

use std::process::Command;

/// One probed tool: the binary that was asked, and the version line it reported.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Tool {
    /// The binary invoked (e.g. `aarch64-linux-gnu-gcc`).
    name: String,
    /// First line of its `--version` output, or `None` when it could not be run.
    version: Option<String>,
}

impl Tool {
    /// Probe `name` for its version line. A tool that cannot be run is not an error
    /// here: a build that compiles nothing needs no compiler, and reporting the
    /// absence is more useful than refusing to describe the host.
    fn probe(name: String) -> Self {
        let version = match Command::new(&name).arg("--version").output() {
            Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout)
                .lines()
                .next()
                .map(|l| l.trim().to_string())
                .filter(|l| !l.is_empty()),
            _ => None,
        };
        Tool { name, version }
    }

    /// The signature form: total, because a cache key cannot have a hole, and
    /// distinct per binary even when the binary cannot be run — so two cross prefixes
    /// that are both absent still yield different identities rather than colliding.
    fn identity(&self) -> String {
        self.version
            .clone()
            .unwrap_or_else(|| format!("unknown-tool:{}", self.name))
    }
}

/// The build host's compilers and interpreter, probed once per build.
///
/// Held by [`BuildEnv`](crate::build::BuildEnv) and read from two directions: the
/// stages fold [`compiler_identity`](Self::compiler_identity) and
/// [`qemu_identity`](Self::qemu_identity) into their output signatures, and the CLI
/// reads the per-tool accessors into the provenance manifest's `[toolchain]` section.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HostToolchain {
    /// `<cross>gcc`.
    cc: Tool,
    /// `<cross>as` — the assembler.
    assembler: Tool,
    /// `<cross>ld` — the linker.
    linker: Tool,
    /// `qemu-<arch>-static`, present only on a cross build (a native build runs
    /// target binaries directly and needs no interpreter).
    qemu: Option<Tool>,
}

impl HostToolchain {
    /// Probe the host. `cross_compile` is the `CROSS_COMPILE` prefix (e.g.
    /// `aarch64-linux-gnu-`) on a cross build and `None` on a native one;
    /// `qemu_arch` is the target's [`qemu` token](boot2deb_core::model::Arch::qemu_arch),
    /// `None` on a native build where nothing is interpreted.
    ///
    /// Binutils is probed alongside the compiler because the produced bytes come from
    /// the assembler and linker as much as from `gcc`, so a binutils upgrade must
    /// invalidate a cached kernel/u-boot artifact rather than restore one built by the
    /// old tools.
    pub fn probe(cross_compile: Option<&str>, qemu_arch: Option<&str>) -> Self {
        let prefix = cross_compile.unwrap_or("");
        HostToolchain {
            cc: Tool::probe(format!("{prefix}gcc")),
            assembler: Tool::probe(format!("{prefix}as")),
            linker: Tool::probe(format!("{prefix}ld")),
            // The `-static` build is the one the binfmt handler registers `F`-flagged,
            // which is what lets the sandbox run target binaries with no interpreter
            // copied into the rootfs — so it is the binary that actually executes.
            qemu: qemu_arch.map(|a| Tool::probe(format!("qemu-{a}-static"))),
        }
    }

    /// The identity of the tools that compile on the host, folded into the kernel,
    /// u-boot, and kmod Tier-2 output signatures: the three version lines, joined.
    pub fn compiler_identity(&self) -> String {
        [&self.cc, &self.assembler, &self.linker]
            .map(Tool::identity)
            .join(" | ")
    }

    /// The identity of the interpreter that executes every sandbox compile, folded
    /// into the userspace/ffmpeg output signatures. `None` on a native build, where
    /// target binaries run directly and no interpreter is an input at all.
    pub fn qemu_identity(&self) -> Option<String> {
        self.qemu.as_ref().map(Tool::identity)
    }

    /// `<cross>gcc`'s version line, or `None` when it could not be run — a build that
    /// compiles nothing from source, or a host without that cross toolchain.
    pub fn cc(&self) -> Option<&str> {
        self.cc.version.as_deref()
    }

    /// `<cross>as`'s version line, `None` as for [`cc`](Self::cc).
    pub fn assembler(&self) -> Option<&str> {
        self.assembler.version.as_deref()
    }

    /// `<cross>ld`'s version line, `None` as for [`cc`](Self::cc).
    pub fn linker(&self) -> Option<&str> {
        self.linker.version.as_deref()
    }

    /// The `qemu-user` version line. `None` on a native build (nothing is
    /// interpreted) and on a cross host where the interpreter could not be run.
    pub fn qemu(&self) -> Option<&str> {
        self.qemu.as_ref().and_then(|t| t.version.as_deref())
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
        let a = HostToolchain::probe(None, None).compiler_identity();
        let b = HostToolchain::probe(None, None).compiler_identity();
        assert_eq!(a, b);
        assert_eq!(a.split(" | ").count(), 3, "{a}");
        assert!(!a.split(" | ").any(|seg| seg.is_empty()), "{a}");
    }

    #[test]
    fn a_missing_cross_toolchain_yields_a_prefix_specific_fallback() {
        // An implausible prefix cannot be run, so the fallback still distinguishes
        // it, per tool — while the provenance accessors report the honest absence
        // rather than the fallback string.
        let tc = HostToolchain::probe(Some("boot2deb-no-such-triple-"), None);
        assert_eq!(
            tc.compiler_identity(),
            "unknown-tool:boot2deb-no-such-triple-gcc | \
             unknown-tool:boot2deb-no-such-triple-as | \
             unknown-tool:boot2deb-no-such-triple-ld"
        );
        assert_eq!(tc.cc(), None);
        assert_eq!(tc.assembler(), None);
        assert_eq!(tc.linker(), None);
    }

    #[test]
    fn a_native_build_has_no_interpreter_to_fold() {
        // Nothing is interpreted on a native build, so qemu is not an input to any
        // signature there — distinct from a cross build whose interpreter is missing,
        // which still folds a fallback so the two never key alike.
        assert_eq!(HostToolchain::probe(None, None).qemu_identity(), None);
        assert_eq!(
            HostToolchain::probe(None, Some("boot2deb-no-such-arch"))
                .qemu_identity()
                .as_deref(),
            Some("unknown-tool:qemu-boot2deb-no-such-arch-static")
        );
    }
}
