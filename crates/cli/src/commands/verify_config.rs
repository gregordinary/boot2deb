//! `verify-config`: the kernel-config gate.
//!
//! Generates the kernel `.config` (base defconfig + fragments via `merge_config.sh`)
//! on a patched kernel tree and, with a reference config, checks byte-identical
//! `CONFIG_*` parity against it. The tree is an explicit `--kernel-path` (assumed
//! already at the locked ref with the series applied) or the locked kernel
//! auto-fetched and patched for the caller, so the gate works on a fresh clone.

use crate::args::ConfigArgs;
use crate::config::{fetch_verify_tree, fragment_paths, resolve_patches_source, verify_trees_cache};
use crate::render::print_event;
use boot2deb_core::model::Overrides;
use boot2deb_core::{load_profile, resolve_recipe, ConfigRoot};
use boot2deb_engine::event::{Event, Step};
use boot2deb_engine::{kconfig, pins, EventSink};
use std::path::{Path, PathBuf};

/// Run `verify-config <recipe>`.
pub(crate) fn run(
    root: &ConfigRoot,
    recipe: &str,
    args: ConfigArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let build = resolve_recipe(root, recipe, &Overrides::default())?;
    // There is a kernel config to verify only where a kernel is configured. A distro
    // kernel arrives pre-built from the mirror: Debian owns its `.config`, so there
    // are no fragments to merge and nothing this gate could compare.
    let kernel = build.kernel.compiled().ok_or_else(|| {
        format!(
            "recipe '{recipe}' uses kernel '{}', a distro package built by Debian — its \
             kernel config is not ours to generate, so there is nothing to verify",
            build.kernel.id()
        )
    })?;
    // Fragment names resolve to fragments/<name>.config along the config search
    // path (overlay-aware), erroring if any is missing.
    let fragments = fragment_paths(root, &build)?;
    // Resolve the config in the same toolchain context the kernel build uses, so the
    // gate validates the config the build actually ships (cross-toolchain-probed
    // symbols included), not a host-probed variant.
    let pf = boot2deb_engine::preflight(build.arch);
    let cross = pf.cross.then(|| build.cross_compile.clone());
    let sink = |e: Event| print_event(&e);

    // Resolve the kernel tree to configure. An explicit `--kernel-path` is used as-is
    // (assumed at the locked ref with the patch series applied). Otherwise the locked
    // kernel is auto-fetched clean and its kernel series applied for us — the config
    // gate then runs out-of-tree, and `restore` returns the shared cache tree to a
    // clean base afterwards so `verify-patches` can reuse it.
    let (tree, restore): (PathBuf, Option<(PathBuf, String)>) = match args.kernel_path {
        Some(p) => (p, None),
        None => {
            let lock = root.lock(recipe)?;
            let kernel_pin = lock.kernel.as_ref().ok_or_else(|| {
                format!("the lock for '{recipe}' pins no kernel — re-run `boot2deb update`")
            })?;
            // A kernel with no patch profile reads no `patches` checkout: the config
            // gate then runs against the pristine locked tree.
            let series = match lock.patches.as_ref() {
                Some(pin) => {
                    let (patches_root, _dev) = resolve_patches_source(
                        args.patches_path.as_deref(),
                        args.patches_url.as_deref(),
                        &build,
                        pin,
                        root,
                        &sink,
                    )?;
                    let profile = load_profile(&patches_root, &pin.profile)?;
                    profile.ensure_applies(&pin.profile, &kernel_pin.reference)?;
                    Some((patches_root, profile))
                }
                None => None,
            };
            // `--kernel-src` overrides the configured upstream for the fetch (a local
            // ../linux is near-instant); the tree still lands at the locked commit.
            let url = match args.kernel_src {
                Some(s) => s,
                None => pins::kernel_source_url(&kernel.source)?,
            };
            let tree = fetch_verify_tree(
                &url,
                &kernel_pin.reference,
                &kernel_pin.commit,
                "kernel",
                &verify_trees_cache(root),
                &sink,
            )?;
            if let Some((patches_root, profile)) = series {
                let target = format!("{} @ {}", kernel_pin.id, kernel_pin.reference);
                let step = Step::start(&sink, "apply-patches");
                let n = boot2deb_engine::srcfetch::apply_kernel_series(
                    &tree,
                    &kernel_pin.commit,
                    &patches_root,
                    &profile.kernel,
                    &target,
                )?;
                step.log(format!("applied {n} kernel patch(es) for the config gate"));
                step.finish();
            }
            (tree.clone(), Some((tree, kernel_pin.commit.clone())))
        }
    };

    let inputs = kconfig::ConfigInputs {
        tree: &tree,
        arch: &build.kernel_arch,
        cross_compile: cross.as_deref(),
        base_defconfig: &kernel.base_defconfig,
        fragments: &fragments,
    };
    let work_dir = args
        .work_dir
        .unwrap_or_else(|| std::env::temp_dir().join(format!("boot2deb-{recipe}-kconfig")));

    let result = run_config_gate(&inputs, args.reference_config.as_deref(), &work_dir, recipe, &sink);
    // Restore the shared cache tree to a clean base regardless of the gate's outcome,
    // so a later verify-patches reuse (and this command's own next run) sees the pin.
    if let Some((tree, base)) = &restore {
        let _ = boot2deb_engine::srcfetch::restore_tree(tree, base);
    }
    result
}

/// Run the kconfig gate on a prepared (patched) kernel `tree`: with a reference
/// config, check byte-identical `CONFIG_*` parity; without, a clean-merge check.
/// The config `make` runs (defconfig / merge_config / olddefconfig) stream like any
/// build stage, so a long or wedged run is visible rather than silent.
fn run_config_gate(
    inputs: &kconfig::ConfigInputs,
    reference_config: Option<&Path>,
    work_dir: &Path,
    recipe: &str,
    sink: &dyn EventSink,
) -> Result<(), Box<dyn std::error::Error>> {
    let step = Step::start(sink, "verify-config");
    match reference_config {
        Some(reference) => {
            let report = kconfig::check_parity(inputs, reference, work_dir, &step)?;
            for sym in &report.unmet {
                println!("warning: fragment symbol not in final .config: {sym}");
            }
            if report.is_match() {
                println!(
                    "verify-config {recipe}: CONFIG_* parity OK ({} symbols) vs {}",
                    report.reference_symbols,
                    reference.display()
                );
            } else {
                eprintln!(
                    "verify-config {recipe}: {} CONFIG_* difference(s) vs {} (generated {} / reference {}):",
                    report.differences.len(),
                    reference.display(),
                    report.generated_symbols,
                    report.reference_symbols
                );
                for d in &report.differences {
                    eprintln!("  {}: generated={} reference={}", d.symbol, d.left, d.right);
                }
                return Err("kernel config parity check failed".into());
            }
        }
        None => {
            let generated = kconfig::generate(inputs, &work_dir.join("gen"), &step)?;
            if generated.unmet.is_empty() {
                println!(
                    "verify-config {recipe}: clean merge ({} symbols); no reference config given",
                    generated.config.len()
                );
            } else {
                eprintln!(
                    "verify-config {recipe}: {} fragment symbol(s) not in final .config:",
                    generated.unmet.len()
                );
                for sym in &generated.unmet {
                    eprintln!("  {sym}");
                }
                return Err("kernel config merge left symbols unmet".into());
            }
        }
    }
    step.finish();
    Ok(())
}
