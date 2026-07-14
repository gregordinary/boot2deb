//! Kernel-config generation and the config-parity check.
//!
//! Generating a `.config` means shelling out to the pinned kernel tree: `make
//! <base_defconfig>` for the base, then the tree's own
//! `scripts/kconfig/merge_config.sh` to layer the fragments (it runs
//! `make KCONFIG_ALLCONFIG=<merged> alldefconfig` to dependency-resolve). We reuse
//! the kernel's Kconfig machinery rather than reimplementing dependency resolution;
//! the pure value comparison lives in [`boot2deb_core::kconfig`].
//!
//! The parity check ([`check_parity`]) compares our fragment-merged config against
//! a reference config. It generates our fragment-merged `.config` and, separately,
//! `olddefconfig(<reference>)` on the same patched tree with the same toolchain,
//! then diffs them over the normalized `CONFIG_*` set. Comparing against the
//! olddefconfig'd reference (not the raw file) is deliberate: a raw reference
//! carries toolchain-probed symbols (MTE/RELR/`CC_*`) and no-prompt `default y`
//! entries that the kernel's own `olddefconfig` overrides, so it is not the config
//! the reference build actually compiles. Generating both sides on one tree +
//! toolchain makes those probed symbols cancel, leaving only fragment-authored
//! differences.
//!
//! The tree must already have the patch series applied: the accel fragment's
//! symbols (`ROCKCHIP_MULTI_RGA`, `DRM_ACCEL_ROCKET`, …) exist only once the
//! patches add their Kconfig entries.

use crate::error::EngineError;
use crate::event::Step;
use boot2deb_core::kconfig::{self, KernelConfig};
use std::path::{Path, PathBuf};
use std::process::Command;

/// The kernel tree and the fragment set to merge onto its base defconfig.
pub struct ConfigInputs<'a> {
    /// A checkout of the pinned kernel at the locked ref, with the patch series
    /// already applied. Out-of-tree builds (`O=`) leave it untouched.
    pub tree: &'a Path,
    /// `ARCH=` for kbuild (e.g. `arm64`).
    pub arch: &'a str,
    /// `CROSS_COMPILE=` prefix, so the config is resolved in the **same toolchain
    /// context the kernel compiles in**; `None` on a native build. This is
    /// load-bearing for a cross build: toolchain-probed symbols (`ARM64_BTI`,
    /// `ARM64_E0PD`, the ARMv8.5 block, …) are gated on `cc-option` probes, so they
    /// resolve per the *target* compiler. Generating with the host compiler instead
    /// leaves them unresolved, and `make bindeb-pkg` under the cross toolchain then
    /// finds them as new symbols and drops into an interactive `oldconfig`. Matching
    /// the toolchains makes the fragment-merged `.config` complete for the build.
    pub cross_compile: Option<&'a str>,
    /// In-tree base defconfig target the fragments merge onto (e.g. `defconfig`).
    pub base_defconfig: &'a str,
    /// Fragment files in merge order (base → soc → accel → device); later files
    /// win, per Kconfig last-wins.
    pub fragments: &'a [PathBuf],
}

/// The result of merging fragments onto a base defconfig.
pub struct Generated {
    /// The dependency-resolved `.config`.
    pub config: KernelConfig,
    /// Fragment-requested symbols that did not survive into the final `.config`
    /// — `merge_config.sh`'s "not in final .config" warnings. Empty is the
    /// clean-merge signal a non-reference kernel gates on.
    pub unmet: Vec<String>,
}

/// Outcome of the config-parity check.
pub struct ParityReport {
    /// Symbols where our generated config differs from the effective reference
    /// config. Empty ⇒ byte-identical `CONFIG_*` parity.
    pub differences: Vec<kconfig::Diff>,
    /// Fragment symbols `merge_config.sh` reported as unmet (see [`Generated`]).
    pub unmet: Vec<String>,
    /// Symbol count of the generated config (for the report line).
    pub generated_symbols: usize,
    /// Symbol count of the effective reference config.
    pub reference_symbols: usize,
}

impl ParityReport {
    /// True when the fragments reproduce the effective reference config exactly.
    pub fn is_match(&self) -> bool {
        self.differences.is_empty()
    }
}

/// Generate a `.config` from the base defconfig + fragments into `out_dir` (an
/// out-of-tree `O=` build, so the source tree is not modified). Returns the
/// resolved config and any unmet fragment symbols.
pub fn generate(inputs: &ConfigInputs, out_dir: &Path, step: &Step) -> Result<Generated, EngineError> {
    let out = prepare_out(out_dir)?;
    // Base: `make <base_defconfig>` writes out/.config.
    make_target(inputs.tree, &out, inputs.arch, inputs.cross_compile, inputs.base_defconfig, step)?;
    let dot_config = out.join(".config");
    // Layer fragments with the tree's merge_config.sh (runs alldefconfig).
    run_merge_config(inputs.tree, &out, inputs.arch, inputs.cross_compile, &dot_config, inputs.fragments, step)?;
    let config = KernelConfig::parse(&read_config(&dot_config)?);
    // Clean-merge check computed against our fragments directly (not scraped from
    // merge_config, whose own check also flags base-defconfig toolchain symbols).
    let requested = read_fragments(inputs.fragments)?;
    let unmet = unmet_symbols(&requested, &config);
    Ok(Generated { config, unmet })
}

/// Produce the *effective* reference config: copy `reference_config` to
/// `out_dir/.config` and run `make olddefconfig`, yielding exactly the `.config`
/// the reference build compiles. Run on the same tree + toolchain as [`generate`]
/// so toolchain-probed symbols match.
pub fn effective_reference(
    tree: &Path,
    arch: &str,
    cross_compile: Option<&str>,
    reference_config: &Path,
    out_dir: &Path,
    step: &Step,
) -> Result<KernelConfig, EngineError> {
    let out = prepare_out(out_dir)?;
    let dot_config = out.join(".config");
    std::fs::copy(reference_config, &dot_config).map_err(|source| EngineError::Io {
        path: format!("{} -> {}", reference_config.display(), dot_config.display()),
        source,
    })?;
    make_target(tree, &out, arch, cross_compile, "olddefconfig", step)?;
    let text = read_config(&dot_config)?;
    Ok(KernelConfig::parse(&text))
}

/// The config-parity check: generate our fragment-merged config and the
/// effective reference config on the same tree, and diff them over the normalized
/// `CONFIG_*` set. `work_dir` holds the two out-of-tree builds (`gen/`,
/// `reference/`), left in place so a failing check can be inspected.
pub fn check_parity(
    inputs: &ConfigInputs,
    reference_config: &Path,
    work_dir: &Path,
    step: &Step,
) -> Result<ParityReport, EngineError> {
    let generated = generate(inputs, &work_dir.join("gen"), step)?;
    let reference = effective_reference(
        inputs.tree,
        inputs.arch,
        inputs.cross_compile,
        reference_config,
        &work_dir.join("reference"),
        step,
    )?;
    Ok(ParityReport {
        differences: kconfig::diff(&generated.config, &reference),
        unmet: generated.unmet,
        generated_symbols: generated.config.len(),
        reference_symbols: reference.len(),
    })
}

/// Create `dir` if needed and return its absolute path, so `make O=` is
/// unambiguous regardless of the caller's CWD.
fn prepare_out(dir: &Path) -> Result<PathBuf, EngineError> {
    std::fs::create_dir_all(dir).map_err(|source| EngineError::io(dir, source))?;
    std::fs::canonicalize(dir).map_err(|source| EngineError::io(dir, source))
}

/// Run `make -C <tree> O=<out> ARCH=<arch> [CROSS_COMPILE=<prefix>] -- <target>` for
/// a config target, streaming its output to `step` like every other subprocess stage.
/// `CROSS_COMPILE` is passed as a `make` variable (matching the compile
/// stage), so the config's `cc-option` probes see the target toolchain. The target
/// is validated and passed after `--` so a config-derived defconfig cannot be read
/// as an option or a variable assignment.
fn make_target(
    tree: &Path,
    out: &Path,
    arch: &str,
    cross_compile: Option<&str>,
    target: &str,
    step: &Step,
) -> Result<(), EngineError> {
    crate::build::reject_unsafe_make_target("make target", target)?;
    let context = format!("make {target} for {}", tree.display());
    let mut cmd = Command::new("make");
    cmd.arg("-C")
        .arg(tree)
        .arg(format!("O={}", out.display()))
        .arg(format!("ARCH={arch}"));
    if let Some(prefix) = cross_compile {
        cmd.arg(format!("CROSS_COMPILE={prefix}"));
    }
    cmd.arg("--").arg(target);
    crate::build::run(cmd, "make", &context, step)
}

/// Run the tree's `scripts/kconfig/merge_config.sh -O <out> <base> <frags…>` to
/// layer the fragments onto the base and dependency-resolve (via
/// `make KCONFIG_ALLCONFIG=<merged> alldefconfig`, which the script invokes),
/// streaming its output to `step`.
fn run_merge_config(
    tree: &Path,
    out: &Path,
    arch: &str,
    cross_compile: Option<&str>,
    base_config: &Path,
    fragments: &[PathBuf],
    step: &Step,
) -> Result<(), EngineError> {
    let script = tree.join("scripts/kconfig/merge_config.sh");
    // `sh` opens the script argument only after `current_dir(tree)` takes effect, so
    // a tree-derived script path must be absolute or it re-resolves *inside* the
    // tree and dangles whenever `tree` itself is relative (e.g. the verify cache
    // under the default relative `--root`). Canonicalizing keeps the invocation
    // CWD-independent and fails here, naming the script, when the tree carries no
    // merge_config.sh at all.
    let script = std::fs::canonicalize(&script).map_err(|source| EngineError::io(&script, source))?;
    let context = format!("merge_config.sh for {}", tree.display());
    let mut cmd = Command::new("sh");
    cmd.arg(&script)
        .current_dir(tree)
        .env("ARCH", arch)
        .arg("-O")
        .arg(out)
        .arg(base_config);
    // merge_config.sh runs `make … alldefconfig` internally; pass CROSS_COMPILE via
    // the environment so that inner make resolves toolchain-probed symbols against
    // the target compiler, matching the base and the compile stage.
    if let Some(prefix) = cross_compile {
        cmd.env("CROSS_COMPILE", prefix);
    }
    // merge_config.sh runs with the tree as its CWD, so fragment paths must be
    // absolute or they would resolve against the tree, not the config root.
    for frag in fragments {
        let abs = std::fs::canonicalize(frag).map_err(|source| EngineError::io(frag, source))?;
        cmd.arg(abs);
    }
    crate::build::run(cmd, "merge_config.sh", &context, step)
}

/// Concatenate and parse the fragment files into one requested-value map
/// (last-wins across files, matching the merge order).
fn read_fragments(fragments: &[PathBuf]) -> Result<KernelConfig, EngineError> {
    let mut text = String::new();
    for frag in fragments {
        text.push_str(&read_config(frag)?);
        text.push('\n');
    }
    Ok(KernelConfig::parse(&text))
}

/// Fragment-requested symbols whose value did not survive dependency resolution
/// into `final_config` — the precise clean-merge signal. Unlike scraping
/// `merge_config.sh`, this checks *only* symbols we authored, so a base-defconfig
/// toolchain probe that `olddefconfig` drops is not misreported. Pure.
fn unmet_symbols(requested: &KernelConfig, final_config: &KernelConfig) -> Vec<String> {
    requested
        .iter()
        .filter(|(sym, val)| final_config.get(sym) != **val)
        .map(|(sym, _)| sym.to_string())
        .collect()
}

/// Read a `.config` / fragment file to a string.
fn read_config(path: &Path) -> Result<String, EngineError> {
    std::fs::read_to_string(path).map_err(|source| EngineError::io(path, source))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unmet_flags_only_fragment_values_that_did_not_take() {
        // Fragment asked for A=y, B=m, and C off.
        let requested = KernelConfig::parse("CONFIG_A=y\nCONFIG_B=m\n# CONFIG_C is not set\n");
        // Final: A took, B got forced to y (dependency), C leaked back on.
        let final_config =
            KernelConfig::parse("CONFIG_A=y\nCONFIG_B=y\nCONFIG_C=y\nCONFIG_PAHOLE_X=y\n");
        // B and C are unmet; A took; the base symbol PAHOLE_X is not our concern.
        assert_eq!(unmet_symbols(&requested, &final_config), vec!["CONFIG_B", "CONFIG_C"]);
    }

    #[test]
    fn unmet_empty_when_all_fragment_values_survive() {
        let requested = KernelConfig::parse("CONFIG_A=y\n# CONFIG_B is not set\n");
        // Final matches the requested values; B absent == not set.
        let final_config = KernelConfig::parse("CONFIG_A=y\nCONFIG_UNRELATED=m\n");
        assert!(unmet_symbols(&requested, &final_config).is_empty());
    }

    #[test]
    fn merge_config_script_resolves_from_a_relative_tree_path() {
        use crate::event::Event;
        // The child `sh` chdirs into the tree before opening the script argument, so
        // the script path must not depend on the parent CWD. Reproduce with
        // a *relative* tree path, which requires a known CWD: cargo runs unit tests
        // from the crate root, with the workspace target dir at ../../target. Skip
        // (rather than create stray dirs) if invoked from anywhere else.
        let cwd = std::env::current_dir().unwrap();
        if cwd.canonicalize().ok() != Path::new(env!("CARGO_MANIFEST_DIR")).canonicalize().ok() {
            eprintln!("skipping: test CWD is not the crate root");
            return;
        }
        let rel_root = Path::new("../../target/kconfig-relative-tree-test");
        let tree = rel_root.join("tree");
        let scripts = tree.join("scripts/kconfig");
        std::fs::create_dir_all(&scripts).unwrap();
        // A stub merge_config.sh: the test asserts only that `sh` can open it.
        std::fs::write(scripts.join("merge_config.sh"), "#!/bin/sh\nexit 0\n").unwrap();
        let out = rel_root.join("out");
        std::fs::create_dir_all(&out).unwrap();

        let sink = |_e: Event| {};
        let step = Step::start(&sink, "test");
        // The stub ignores its arguments, so any existing file serves as the base.
        let base = scripts.join("merge_config.sh");
        let result = run_merge_config(&tree, &out, "arm64", None, &base, &[], &step);
        std::fs::remove_dir_all(rel_root).ok();
        result.expect("merge_config.sh must be invocable when the tree path is relative");
    }
}
