//! `resolve`: resolve a device or recipe to a complete build and print it.
//!
//! The documented first coherence gate — it does no build work, but it validates the
//! cheap local invariants (geometry, fragments, keyrings) after the printout, so the
//! resolved values sit beside any failure they explain.
//!
//! It accepts a wider override set than any command that can *build* the result,
//! deliberately: the point of the command is to see what a choice resolves to before
//! committing it to config. [`unbuildable_note`] closes the gap that opens, by saying
//! so at the moment a resolution names a point only config can reach.

use crate::config::{preflight_config, resolve};
use crate::render::print_build;
use boot2deb_core::model::Overrides;
use boot2deb_core::ConfigRoot;

/// Run `resolve <target>`, rendering the resolved build for a human or as one JSON
/// document under `--json`.
pub(crate) fn run(
    root: &ConfigRoot,
    target: &str,
    overrides: Overrides,
    json: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let advice = unbuildable_note(target, &overrides);
    let build = resolve(root, target, overrides)?;
    if json {
        println!("{}", serde_json::to_string_pretty(&build)?);
    } else {
        print_build(&build);
        // After the printout, like the preflight below: the resolved values are what
        // the advice is about, so they should already be on screen.
        if let Some(advice) = advice {
            println!("\n{advice}");
        }
    }
    preflight_config(root, &build)?;
    Ok(())
}

/// The axes `resolve` accepts that no build command can act on, in flag order, with
/// the recipe key each maps to.
///
/// `--layout`, `--image-size`, and `--feature` are absent because `build` takes them
/// directly. `--boot-method` is absent because it is not a recipe key at all — it is
/// handled separately below, since the answer for it is a device rather than a recipe.
const RECIPE_ONLY_AXES: &[(&str, &str)] = &[
    ("--kernel", "kernel"),
    ("--uboot-series", "uboot_series"),
    ("--suite", "suite"),
    ("--board", "board"),
    ("--locale", "locale"),
    ("--locale-gen", "locales_generate"),
    ("--timezone", "timezone"),
    ("--ntp-server", "ntp_servers"),
    ("--keymap", "keymap"),
    ("--sudo", "sudo"),
    ("--password-length", "first_boot_password_length"),
];

/// Build the advisory for a resolution whose overrides name a point `build` cannot
/// reach, or `None` when every override in play is one `build` also takes.
///
/// A green `resolve` reads as "this point is buildable", and for these axes it is
/// not: `build` reads them from the config its lock was resolved against, so the way
/// to build the point is to write it down. The note names the file and the keys, which
/// is the whole answer — without it the user has to discover from the docs that the
/// flags they just used do not exist on `build`.
///
/// Pure (string in, string out), so it is unit-testable without a config tree.
fn unbuildable_note(target: &str, ov: &Overrides) -> Option<String> {
    // The device half of the reference: `turing-rk1/forky` and `turing-rk1` both
    // suggest a recipe under `recipes/turing-rk1/`.
    let device = target.split('/').next().unwrap_or(target);

    // `--boot-method` first: it is the one axis whose answer is a device file, so a
    // note that lumped it in with the recipe keys would name the wrong file.
    if let Some(bm) = ov.boot_method {
        return Some(format!(
            "note: --boot-method is a resolve-only override — how a board boots is a \
             property of the hardware, so it is not a recipe axis. To build this point, \
             give it a device: devices/<name>.toml with\n    \
             extends     = \"{device}\"\n    boot_method = \"{}\"\n\
             then a recipe naming that device.",
            bm.as_str()
        ));
    }

    let mut keys: Vec<(&str, String)> = Vec::new();
    let mut flags: Vec<&str> = Vec::new();
    let mut push = |flag: &'static str, key: &'static str, value: Option<String>| {
        if let Some(v) = value {
            flags.push(flag);
            keys.push((key, v));
        }
    };
    for (flag, key) in RECIPE_ONLY_AXES {
        let value = match *flag {
            "--kernel" => ov.kernel.clone(),
            "--uboot-series" => ov.uboot_series.clone(),
            "--suite" => ov.suite.clone(),
            "--board" => ov.board.clone(),
            "--locale" => ov.locale.clone(),
            "--locale-gen" => ov.locales_generate.as_ref().map(|l| {
                format!(
                    "[{}]",
                    l.iter()
                        .map(|s| format!("{s:?}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }),
            "--timezone" => ov.timezone.clone(),
            // An array like `locales_generate`, so it renders as TOML array syntax
            // rather than being quoted as one string by the fallback below.
            "--ntp-server" => ov.ntp_servers.as_ref().map(|s| {
                format!(
                    "[{}]",
                    s.iter()
                        .map(|s| format!("{s:?}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                )
            }),
            "--keymap" => ov.keymap.as_ref().map(|k| k.layout.clone()),
            "--sudo" => ov.sudo.map(|s| s.as_str().to_string()),
            "--password-length" => ov.first_boot_password_length.map(|n| n.to_string()),
            _ => None,
        };
        push(flag, key, value);
    }
    if keys.is_empty() {
        return None;
    }

    // `device` is emitted alongside them, so it joins the alignment: the note is a
    // TOML fragment to paste, and a misaligned `=` reads as two separate blocks.
    let width = keys
        .iter()
        .map(|(k, _)| k.len())
        .chain(std::iter::once(DEVICE_KEY.len()))
        .max()
        .unwrap_or(0);
    let body = keys
        .iter()
        .map(|(k, v)| {
            // The fragment is pasted into a recipe, so each value has to be the TOML
            // its field deserializes from: `locales_generate` is already an array,
            // `first_boot_password_length` is an integer, and a quoted form of either
            // would fail to load. Everything else is a string.
            let rendered = if v.starts_with('[') || v.parse::<u64>().is_ok() {
                v.clone()
            } else {
                format!("{v:?}")
            };
            format!("    {k:<width$} = {rendered}")
        })
        .collect::<Vec<_>>()
        .join("\n");
    let (verb, subject) = if flags.len() == 1 {
        ("is", "that axis")
    } else {
        ("are", "those axes")
    };
    Some(format!(
        "note: {} {verb} resolve-only — `build` reads {subject} from the config its \
         lock was resolved against, not from a flag. To build this point, write it \
         down: recipes/{device}/<leaf>.toml with\n    \
         {DEVICE_KEY:<width$} = {device:?}\n{body}\n\
         then `boot2deb update {device}/<leaf>` to pin it.",
        flags.join(", "),
    ))
}

/// The recipe key naming the device, emitted with every suggested fragment because a
/// recipe without it does not load.
const DEVICE_KEY: &str = "device";

#[cfg(test)]
mod tests {
    use super::*;
    use boot2deb_core::model::{BootMethod, Keymap, Layout};

    #[test]
    fn an_override_build_also_takes_produces_no_note() {
        let ov = Overrides {
            layout: Some(Layout::Split),
            image_size: Some("4G".into()),
            features: Some(vec!["jellyfin".into()]),
            ..Default::default()
        };
        assert!(unbuildable_note("turing-rk1", &ov).is_none());
        assert!(unbuildable_note("turing-rk1", &Overrides::default()).is_none());
    }

    #[test]
    fn a_recipe_only_override_names_the_file_and_every_key() {
        let ov = Overrides {
            suite: Some("trixie".into()),
            keymap: Some(Keymap::from_layout("gb")),
            // A buildable axis alongside them is not mentioned: `build` takes it.
            layout: Some(Layout::Split),
            ..Default::default()
        };
        let note = unbuildable_note("turing-rk1/forky", &ov).expect("a note");
        assert!(
            note.contains("--suite, --keymap are resolve-only"),
            "{note}"
        );
        assert!(note.contains("recipes/turing-rk1/<leaf>.toml"), "{note}");
        assert!(note.contains("device = \"turing-rk1\""), "{note}");
        assert!(note.contains("suite  = \"trixie\""), "{note}");
        assert!(note.contains("keymap = \"gb\""), "{note}");
        assert!(!note.contains("layout"), "{note}");
        assert!(note.contains("boot2deb update turing-rk1/<leaf>"), "{note}");
    }

    #[test]
    fn a_single_override_reads_as_singular() {
        let ov = Overrides {
            board: Some("speedy-libreboot".into()),
            ..Default::default()
        };
        let note = unbuildable_note("asus-c201", &ov).expect("a note");
        assert!(note.contains("--board is resolve-only"), "{note}");
        assert!(note.contains("board  = \"speedy-libreboot\""), "{note}");
    }

    #[test]
    fn the_boot_method_axis_points_at_a_device_not_a_recipe() {
        // The one override a recipe cannot express: naming a recipe for it would send
        // the user to write a key that does not exist.
        let ov = Overrides {
            boot_method: Some(BootMethod::Depthcharge),
            suite: Some("trixie".into()),
            ..Default::default()
        };
        let note = unbuildable_note("turing-rk1", &ov).expect("a note");
        assert!(note.contains("--boot-method"), "{note}");
        assert!(note.contains("devices/<name>.toml"), "{note}");
        assert!(note.contains("extends     = \"turing-rk1\""), "{note}");
        assert!(!note.contains("recipes/"), "{note}");
    }

    #[test]
    fn a_repeatable_axis_renders_as_a_toml_array() {
        let ov = Overrides {
            locales_generate: Some(vec!["fr_FR.UTF-8".into(), "de_DE.UTF-8".into()]),
            ..Default::default()
        };
        let note = unbuildable_note("h96-max-m9", &ov).expect("a note");
        assert!(
            note.contains("locales_generate = [\"fr_FR.UTF-8\", \"de_DE.UTF-8\"]"),
            "{note}"
        );
    }
}
