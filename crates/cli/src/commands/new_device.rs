//! `new-device`: scaffold a device (and, by default, its recipe) from the typed model.
//!
//! Interactive on a terminal (menus over the closed axis enums and the SoC/arch-
//! compatible kernels + features), flag-driven when `--non-interactive` or piped.
//! Every derivable value is filled from the layers; the researched ones are left as
//! `# TODO:` suggestions. Writes into the highest-precedence `--overlay` when one is
//! given — so a third party scaffolds into their own tree — else the primary root,
//! then resolve-checks the result and prints the values the author must still verify.

use crate::args::NewDeviceArgs;
use crate::fsutil::write_scaffold_file;
use crate::prompt::{ask_choice, ask_features, ask_value};
use boot2deb_core::model::{BootMethod, Layout, Overrides, Soc};
use boot2deb_core::scaffold::DeviceScaffold;
use boot2deb_core::{resolve_device, ConfigRoot};

/// Run `new-device <name>`.
pub(crate) fn run(
    root: &ConfigRoot,
    name: &str,
    args: NewDeviceArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    // The name is a file stem and a TOML value; keep it to the safe set the loader
    // accepts for the layers it will live beside.
    if name.is_empty()
        || !name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        return Err(format!(
            "device name '{name}' is invalid — use lowercase letters, digits, and dashes"
        )
        .into());
    }
    let interactive = !args.non_interactive && std::io::IsTerminal::is_terminal(&std::io::stdin());

    // SoC: the closed enum, narrowed to those that actually have a `socs/<soc>.toml`
    // here (a genuinely new SoC family needs a model.rs edit first — out of scope for
    // scaffolding a board). The SoC fixes arch, dt_dir, and the module list.
    let available_socs: Vec<Soc> =
        Soc::all().iter().copied().filter(|s| root.soc(*s).is_ok()).collect();
    if available_socs.is_empty() {
        return Err(
            "no socs/<soc>.toml found under the config root — nothing to build a device on".into(),
        );
    }
    // The SoC is the identifying choice, so it is required (never silently defaulted)
    // when there is no terminal to prompt at.
    if !interactive && args.soc.is_none() {
        let valid = available_socs.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", ");
        return Err(
            format!("--soc is required in non-interactive mode (choose one of: {valid})").into(),
        );
    }
    let soc = ask_choice(
        "SoC",
        args.soc.as_deref(),
        &available_socs,
        interactive,
        |s: &Soc| s.as_str().to_string(),
        |s| s.parse(),
    )?;
    let soc_layer = root.soc(soc)?;
    let arch = soc_layer.arch;

    let boot_method = ask_choice(
        "boot method",
        args.boot_method.as_deref(),
        BootMethod::all(),
        interactive,
        |b: &BootMethod| b.as_str().to_string(),
        |s| s.parse(),
    )?;

    // Kernels valid for this SoC (its `supported_socs` lists the SoC).
    let kernels: Vec<String> = root
        .list("kernels")?
        .into_iter()
        .filter(|k| {
            root.kernel(k)
                .map(|kd| kd.supported_socs().contains(&soc))
                .unwrap_or(false)
        })
        .collect();
    if kernels.is_empty() {
        return Err(format!(
            "no kernel definition supports soc '{soc}' — add one under kernels/ first"
        )
        .into());
    }
    let kernel = ask_choice(
        "kernel",
        args.kernel.as_deref(),
        &kernels,
        interactive,
        |k: &String| k.clone(),
        |s| Ok(s.to_string()),
    )?;

    let layout = ask_choice(
        "layout",
        args.layout.as_deref(),
        Layout::all(),
        interactive,
        |l: &Layout| l.as_str().to_string(),
        |s| s.parse(),
    )?;
    let suite = ask_value("suite", args.suite, "forky", interactive);
    let hostname = ask_value("hostname", args.hostname, name, interactive);
    let image_size = ask_value("image size", args.image_size, "2G", interactive);
    let description =
        ask_value("description", args.description, &format!("{name} ({soc})"), interactive);

    // Features compatible with the resolved SoC/arch (the same gates resolution
    // enforces), offered for the recipe scaffold.
    let compatible: Vec<String> = root
        .list("features")?
        .into_iter()
        .filter(|f| {
            root.feature(f)
                .map(|ft| ft.supports_soc(soc) && ft.supports_arch(arch))
                .unwrap_or(false)
        })
        .collect();
    let features = if !args.features.is_empty() {
        for f in &args.features {
            if !compatible.contains(f) {
                return Err(format!(
                    "feature '{f}' is not compatible with soc '{soc}'/arch '{arch}' (or does not exist)"
                )
                .into());
            }
        }
        args.features.clone()
    } else if interactive {
        ask_features(&compatible)
    } else {
        Vec::new()
    };

    let scaffold = DeviceScaffold {
        name: name.to_string(),
        description,
        soc,
        boot_method,
        kernel,
        suite,
        layout,
        hostname,
        image_size,
        dt_dir: soc_layer.dt_dir.clone(),
        soc_rkbin: soc_layer.rkbin.clone(),
        features,
        emit_recipe: !args.no_recipe,
    };

    // Write into the highest-precedence search path: the last `--overlay` if any
    // (the third-party's own tree), else the primary root.
    let out_dir = root
        .search_paths()
        .last()
        .expect("a config root always has a primary path")
        .clone();
    let device_path = out_dir.join("devices").join(format!("{name}.toml"));
    write_scaffold_file(&device_path, &scaffold.device_toml(), args.force)?;
    println!("wrote {}", device_path.display());
    if let Some(recipe) = scaffold.recipe_toml() {
        let recipe_path = out_dir.join("recipes").join(format!("{name}.toml"));
        write_scaffold_file(&recipe_path, &recipe, args.force)?;
        println!("wrote {}", recipe_path.display());
    }

    // Resolve-check the freshly written device against the same root (which already
    // includes `out_dir` in its search path). The scaffold's placeholder values are
    // structurally valid, so this should pass — proving the layer composition — while
    // the research notes below name what still fails at build time if left as-is.
    match resolve_device(root, name, &Overrides::default()) {
        Ok(_) => println!("\nresolves cleanly against the config root."),
        Err(e) => println!(
            "\nnote: resolve reported: {e}\n  (fix the flagged values, then re-run `boot2deb resolve {name}`)"
        ),
    }

    let notes = scaffold.research_notes();
    if !notes.is_empty() {
        println!("\nvalues to research before building (each is a best-effort guess):");
        for n in &notes {
            println!("  {:<16} = {:?}\n      {}", n.field, n.value, n.guidance);
        }
    }

    println!("\nnext steps:");
    println!("  1. edit {} and replace the TODO values", device_path.display());
    if !args.no_recipe {
        println!("  2. boot2deb update {name}    # resolve pins into the lock");
        println!("  3. boot2deb build  {name}    # build the image");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::{repo_root, repo_root_path};
    use boot2deb_core::resolve_recipe;

    /// A `NewDeviceArgs` with every knob unset — the non-interactive baseline a test
    /// tweaks per case. `non_interactive` is set so the helpers never touch stdin.
    fn new_device_args() -> NewDeviceArgs {
        NewDeviceArgs {
            description: None,
            soc: None,
            boot_method: None,
            kernel: None,
            suite: None,
            layout: None,
            hostname: None,
            image_size: None,
            features: vec![],
            no_recipe: false,
            force: false,
            non_interactive: true,
        }
    }

    #[test]
    fn new_device_rejects_a_bad_name() {
        // An invalid name fails before any file is written (no SoC lookup needed).
        let root = repo_root();
        let err = run(&root, "Bad Name", new_device_args()).unwrap_err().to_string();
        assert!(err.contains("invalid"), "{err}");
    }

    #[test]
    fn new_device_non_interactive_requires_soc() {
        let root = repo_root();
        let err = run(&root, "some-board", new_device_args()).unwrap_err().to_string();
        assert!(err.contains("--soc is required"), "{err}");
    }

    #[test]
    fn new_device_scaffolds_into_an_overlay_and_resolves() {
        // Primary = shipped repo (for the SoC/kernel/feature definitions), overlay =
        // a scratch dir the new files land in — the third-party path.
        let overlay = tempfile::tempdir().unwrap();
        let root =
            ConfigRoot::with_overlays(repo_root_path(), [overlay.path().to_path_buf()]).unwrap();
        let args = NewDeviceArgs {
            soc: Some("rk3588".into()),
            features: vec!["media-accel-rockchip".into()],
            ..new_device_args()
        };
        run(&root, "test-board", args).unwrap();
        // Files land in the overlay, not the primary root.
        assert!(overlay.path().join("devices/test-board.toml").is_file());
        assert!(overlay.path().join("recipes/test-board.toml").is_file());
        // The scaffolded recipe resolves against the composed search path, carrying
        // its selected feature — and the media-accel feature pulls in the SoC sources
        //. (The device alone has no features; they live in the recipe.)
        let build = resolve_recipe(&root, "test-board", &Overrides::default()).unwrap();
        assert_eq!(build.soc, Soc::Rk3588);
        assert_eq!(build.features, vec!["media-accel-rockchip"]);
        assert!(build.userspace.is_some());
    }

    #[test]
    fn new_device_refuses_an_incompatible_feature() {
        let overlay = tempfile::tempdir().unwrap();
        let root =
            ConfigRoot::with_overlays(repo_root_path(), [overlay.path().to_path_buf()]).unwrap();
        let args = NewDeviceArgs {
            soc: Some("rk3588".into()),
            features: vec!["no-such-feature".into()],
            ..new_device_args()
        };
        let err = run(&root, "test-board", args).unwrap_err().to_string();
        assert!(err.contains("not compatible") || err.contains("does not exist"), "{err}");
    }
}
