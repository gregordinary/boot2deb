//! `verify-config`: the kernel-config gate.
//!
//! Generates the kernel `.config` (base defconfig + fragments via `merge_config.sh`)
//! on a patched kernel tree and, with a reference config, checks byte-identical
//! `CONFIG_*` parity against it. The tree is an explicit `--kernel-path` (assumed
//! already at the locked ref with the series applied) or the locked kernel
//! auto-fetched and patched for the caller, so the gate works on a fresh clone.

use crate::args::ConfigArgs;
use crate::config::{
    fetch_verify_tree, fragment_paths, resolve_patches_source, verify_trees_cache,
};
use crate::fsutil::absolutize;
use crate::render::{print_event_at, Verbosity};
use boot2deb_core::model::Overrides;
use boot2deb_core::{load_series, resolve_recipe, ConfigRoot};
use boot2deb_engine::event::{Event, Step};
use boot2deb_engine::sandbox::{BuildSandbox, RootlessSandbox, SandboxRole};
use boot2deb_engine::{kconfig, pins, EventSink};
use serde_json::json;
use std::path::{Path, PathBuf};

/// Run `verify-config <recipe>`.
///
/// Under `--json` the *verdict* is one document on stdout; the config `make` runs
/// still stream to the terminal as they do for a build, because a wedged
/// `olddefconfig` is something a CI log has to show.
pub(crate) fn run(
    root: &ConfigRoot,
    recipe: &str,
    args: ConfigArgs,
    json_out: bool,
    verbosity: Verbosity,
) -> Result<(), Box<dyn std::error::Error>> {
    let build = resolve_recipe(root, recipe, &Overrides::default())?;
    // There is a kernel config to verify only where a kernel is configured. A distro
    // kernel arrives pre-built from the mirror: Debian owns its `.config`, so there
    // are no fragments to merge and nothing this gate could compare.
    let kernel = build
        .image
        .as_ref()
        .and_then(|i| i.kernel.compiled())
        .ok_or_else(|| {
            format!(
                "recipe '{recipe}' uses kernel '{}', a distro package built by Debian — its \
             kernel config is not ours to generate, so there is nothing to verify",
                build
                    .image
                    .as_ref()
                    .map(|i| i.kernel.id())
                    .unwrap_or("(none)")
            )
        })?;
    // Fragment names resolve to fragments/<name>.config along the config search
    // path (overlay-aware), erroring if any is missing.
    let fragments = fragment_paths(root, &build)?;
    // Resolve the config in the same toolchain context the kernel build uses, so the
    // gate validates the config the build actually ships (cross-toolchain-probed
    // symbols included), not a host-probed variant. That means the same *root* a build
    // would compile in, not merely the same `CROSS_COMPILE` string: `cc-option` probes
    // are answered by whichever compiler is on the path, and here that is a package of
    // the cross root rather than anything the host installed.
    let pf = boot2deb_engine::preflight(build.arch);
    let host_deb_arch = pf.host.debian_arch().ok_or_else(|| {
        format!(
            "cannot name a Debian architecture for this host ({}) — generating a kernel \
             config runs kbuild in a host-arch root, and there is no name to provision \
             one under",
            pf.host.arch
        )
    })?;
    let cross = (host_deb_arch != build.arch.debian_arch()).then(|| build.cross_compile.clone());
    // The same archive a build would resolve the root from: the lock's captured
    // snapshot mode where it has one, else the live mirror. A gate that generated its
    // config with a different toolchain than the build would use is not a gate.
    let gate_lock = root.lock(recipe).ok();
    let mirrors = boot2deb_engine::snapshot::resolve_mirrors(
        boot2deb_engine::DEFAULT_MIRROR,
        gate_lock.as_ref().and_then(|l| l.snapshot.as_ref()),
        gate_lock
            .as_ref()
            .and_then(|l| l.snapshot.as_ref())
            .map(|s| s.mode)
            .unwrap_or(boot2deb_core::lock::SnapshotMode::Off),
    )?;
    // The vendored archive keyring, held to its fingerprint manifest, as a build uses
    // it; `None` falls back to the host apt trust store.
    let keyring = {
        let vendored =
            root.find_trust_anchor("blobs/keyrings/debian-archive-keyring.gpg", false)?;
        if let Some(path) = &vendored {
            boot2deb_engine::keyring::verify(path)?;
        }
        vendored
    };
    let sink = move |e: Event| print_event_at(verbosity, &e);

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
            // A kernel with no patch series reads no `patches` checkout: the config
            // gate then runs against the pristine locked tree.
            let series = match lock.patches.as_ref() {
                Some(pin) => {
                    let (patches_root, _dev) = resolve_patches_source(
                        args.patches_path.as_deref(),
                        args.patches_url.as_deref(),
                        pin,
                        root,
                        &sink,
                    )?;
                    // The `.config` this gate judges is generated from the series as the
                    // checkout holds it, so a drifted checkout means the verdict is about
                    // a kernel other than the locked one. Warn rather than refuse: the
                    // point of reading the working tree is co-development.
                    if let Some(drift) = pins::patches_drift(&patches_root, &pin.commit)? {
                        println!(
                            "warning: {drift} — configuring against the working tree's series, \
                             not the pinned one"
                        );
                    }
                    // Load and envelope-gate every composed series before fetching, so
                    // a series that does not cover the locked kernel fails fast; the
                    // config gate then compiles against the full composed series.
                    let mut loaded = Vec::with_capacity(pin.series.len());
                    for name in &pin.series {
                        let series = load_series(&patches_root, name)?;
                        series.ensure_applies(name, &kernel_pin.reference)?;
                        loaded.push((name.clone(), series));
                    }
                    Some((patches_root, loaded))
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
            if let Some((patches_root, series)) = series {
                let target = format!("{} @ {}", kernel_pin.id, kernel_pin.reference);
                let step = Step::start(&sink, "apply-patches");
                // The config gate compiles against what this kernel actually gets, so
                // every series' kernel series is concatenated in order and narrowed
                // by the locked kernel exactly as a build narrows it — release-strict,
                // since the lock pins a released tag.
                let mut kernel_series: Vec<&str> = Vec::new();
                for (name, series) in &series {
                    kernel_series.extend(series.series_for(
                        boot2deb_core::series::Scope::Kernel,
                        name,
                        &kernel_pin.reference,
                        boot2deb_core::RangeMatch::Release,
                    )?);
                }
                let n = boot2deb_engine::srcfetch::apply_kernel_series(
                    &tree,
                    &kernel_pin.commit,
                    &patches_root,
                    &kernel_series,
                    &target,
                )?;
                step.log(format!("applied {n} kernel patch(es) for the config gate"));
                step.finish();
            }
            (tree.clone(), Some((tree, kernel_pin.commit.clone())))
        }
    };

    let work_dir = absolutize(args.work_dir.unwrap_or_else(|| {
        // Under the config root's cache, and deliberately **not** under `TMPDIR`. The
        // out-of-tree config builds happen inside the cross root, and every cage mounts
        // its own `/tmp` — so a scratch dir in the host's temp dir is shadowed by that
        // tmpfs, and everything kbuild writes there is discarded when the run ends. The
        // slash-free recipe identity (device included) keeps two boards' verifies from
        // colliding on one dir.
        let slug = recipe.replace('/', "-");
        crate::config::kconfig_cache(root).join(slug)
    }));
    std::fs::create_dir_all(&work_dir)
        .map_err(|e| format!("failed to create {}: {e}", work_dir.display()))?;

    // The root kbuild runs in, provisioned exactly as a build would provision it —
    // same role, same host arch, same suite, same mirrors — so the gate answers
    // `cc-option` probes with the compiler the build will use. Bootstrapped here rather
    // than lazily because every path below this point runs `make`.
    let cross_role = SandboxRole::Cross {
        target: build.arch.debian_arch(),
    };
    let cross_sandbox = RootlessSandbox::new(
        cross_role,
        boot2deb_engine::sandbox::SandboxSpec {
            rootfs: boot2deb_engine::sandbox::build_sandbox_dir(
                &work_dir,
                cross_role,
                host_deb_arch,
                &build.packaging_suite,
                &mirrors,
            ),
            suite: build.packaging_suite.clone(),
            arch: host_deb_arch.to_string(),
            mirrors,
            keyring,
            cache_dir: Some(work_dir.join("cache").join("provisioner-debs")),
        },
        boot2deb_engine::sandbox::build_root_uppers(&work_dir),
    );
    let gate_step = Step::start(&sink, "config-root");
    cross_sandbox.ensure_ready(&gate_step)?;
    let config_root = cross_sandbox.build_root(
        &boot2deb_engine::sandbox::BuildRootSpec {
            packages: boot2deb_engine::build::kernel::BUILD_DEPS,
            pool: None,
            stage: "verify-config",
        },
        &gate_step,
    )?;
    gate_step.finish();

    // The tree, plus each fragment: `merge_config.sh` opens the fragments from inside
    // the root by the absolute path this side hands it. Absolute throughout — a bind is
    // established inside at its own path, and `--root .` makes both of these relative by
    // default.
    let tree = absolutize(tree);
    // The work dir is bound too: the `O=` out-of-tree builds write into it, and a path
    // the cage cannot see is a path `make` reports as missing.
    let mut binds = vec![tree.clone(), work_dir.clone()];
    binds.extend(fragments.iter().map(|f| absolutize(f.clone())));

    let inputs = kconfig::ConfigInputs {
        tree: &tree,
        arch: &build.kernel_arch,
        cross_compile: cross.as_deref(),
        base_defconfig: &kernel.base_defconfig,
        fragments: &fragments,
        cr: &boot2deb_engine::sandbox::CompileRoot {
            root: &config_root,
            binds: &binds,
        },
    };

    let result = run_config_gate(
        &inputs,
        args.reference_config.as_deref(),
        &work_dir,
        recipe,
        json_out,
        &sink,
    );
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
    json_out: bool,
    sink: &dyn EventSink,
) -> Result<(), Box<dyn std::error::Error>> {
    let step = Step::start(sink, "verify-config");
    match reference_config {
        Some(reference) => {
            let report = kconfig::check_parity(inputs, reference, work_dir, &step)?;
            if json_out {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "recipe": recipe,
                        "mode": "parity",
                        "reference": reference.display().to_string(),
                        "generated_symbols": report.generated_symbols,
                        "reference_symbols": report.reference_symbols,
                        "unmet": report.unmet,
                        "differences": report.differences.iter().map(|d| json!({
                            "symbol": d.symbol,
                            // Rendered through Display: a kconfig value is the
                            // verbatim text after the `=`, and "not set" is a value.
                            "generated": d.left.to_string(), "reference": d.right.to_string(),
                        })).collect::<Vec<_>>(),
                        "result": if report.is_match() { "pass" } else { "fail" },
                    }))?
                );
                step.finish();
                return if report.is_match() {
                    Ok(())
                } else {
                    Err("kernel config parity check failed".into())
                };
            }
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
            if json_out {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "recipe": recipe,
                        "mode": "merge",
                        "generated_symbols": generated.config.len(),
                        "unmet": generated.unmet,
                        "result": if generated.unmet.is_empty() { "pass" } else { "fail" },
                    }))?
                );
                step.finish();
                return if generated.unmet.is_empty() {
                    Ok(())
                } else {
                    Err("kernel config merge left symbols unmet".into())
                };
            }
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
