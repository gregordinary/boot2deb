//! `seed`: rewrite the per-unit seed partition of an already-pressed image file.
//!
//! The smaller half of `press`'s personalization, for the file that already
//! exists: no recipe, no artifacts — the seed partition is found by its GPT
//! label (`b2d-seed`), so the file is the whole input. With no keys the seed
//! resets to the empty template.
//!
//! Deliberately a *file* operation. boot2deb does not write devices, so a block
//! device is refused by name — a card that is already written is
//! re-personalized by editing `seed.txt` on its `B2D-SEED` volume from any
//! machine, which is the seed's whole design.

use crate::args::SeedArgs;
use crate::render::{print_event_at, Verbosity};
use boot2deb_engine::event::{Event, Step};
use boot2deb_engine::press::seed;
use std::path::Path;

/// Run `seed <image-file> [...]`.
pub(crate) fn run(
    image: &Path,
    args: SeedArgs,
    verbosity: Verbosity,
) -> Result<(), Box<dyn std::error::Error>> {
    let meta =
        std::fs::metadata(image).map_err(|e| format!("cannot read {}: {e}", image.display()))?;
    if !meta.is_file() {
        return Err(format!(
            "{} is not a regular file. boot2deb does not write devices — for a card \
             that is already written, edit seed.txt on its B2D-SEED volume directly",
            image.display()
        )
        .into());
    }

    let keys = super::press::seed_keys(&args.keys)?.unwrap_or_default();
    if args.dry_run {
        println!(
            "would rewrite the seed partition of {}: {}",
            image.display(),
            describe(&keys)
        );
        println!("dry run: nothing was written");
        return Ok(());
    }

    let sink = move |e: Event| print_event_at(verbosity, &e);
    let step = Step::start(&sink, "seed");
    seed::rewrite_seed(image, &keys, super::press::now_secs())?;
    step.log(format!(
        "rewrote the seed partition of {}: {}",
        image.display(),
        describe(&keys)
    ));
    step.finish();
    Ok(())
}

/// What the seed now says, for the log line and the dry run.
fn describe(keys: &seed::SeedKeys) -> String {
    if keys.is_empty() {
        "reset to the empty template".into()
    } else {
        let mut parts = Vec::new();
        if let Some(h) = &keys.hostname {
            parts.push(format!("hostname={h}"));
        }
        if !keys.authorized_keys.is_empty() {
            parts.push(format!("{} ssh key(s)", keys.authorized_keys.len()));
        }
        if let Some(ssid) = &keys.wifi_ssid {
            parts.push(format!(
                "wifi {ssid:?} ({})",
                if keys.wifi_psk.is_some() {
                    "wpa"
                } else {
                    "open"
                }
            ));
        }
        if let Some(ip) = &keys.static_ip {
            parts.push(format!("static ip {ip}"));
        }
        parts.join(", ")
    }
}
