//! The four `list-*` commands: the discovery surface over the config search path.
//!
//! Each renders one row per entry (or a JSON array under `--json`) and collects the
//! entries that failed to parse, so a corrupt layer file is reported rather than
//! silently dropped (UX-28). An unreadable entry never fails the listing.

use crate::render::{constraint, finish_listing};
use boot2deb_core::ConfigRoot;

type Result = std::result::Result<(), Box<dyn std::error::Error>>;

/// `list-devices`: every device layer, with its description.
pub(crate) fn devices(root: &ConfigRoot, json: bool) -> Result {
    let mut broken = Vec::new();
    let mut rows = Vec::new();
    for name in root.list("devices")? {
        match root.device(&name) {
            Ok(d) if json => {
                rows.push(serde_json::json!({"name": name, "description": d.description}));
            }
            Ok(d) => println!("{name:<20} {}", d.description),
            Err(e) if json => {
                rows.push(serde_json::json!({"name": name, "error": e.to_string()}));
            }
            Err(e) => {
                println!("{name:<20} (unreadable)");
                broken.push((name, e.to_string()));
            }
        }
    }
    finish_listing(json, rows, "device", &broken)
}

/// `list-recipes`: every recipe, its device, and whether it has a committed lock —
/// a recipe without one is not buildable until `update` resolves it, so the listing
/// says so up front instead of letting `build` be the first to fail.
pub(crate) fn recipes(root: &ConfigRoot, json: bool) -> Result {
    let mut broken = Vec::new();
    let mut rows = Vec::new();
    for name in root.list("recipes")? {
        let (lock_state, lock_note) = match root.lock(&name) {
            Ok(_) => ("ok", ""),
            Err(boot2deb_core::ConfigError::NotFound { .. }) => {
                ("missing", "  [no lock — run `boot2deb update` to make it buildable]")
            }
            Err(_) => ("unreadable", "  [lock unreadable]"),
        };
        match root.recipe(&name) {
            Ok(r) if json => {
                rows.push(serde_json::json!({
                    "name": name, "device": r.device, "lock": lock_state,
                }));
            }
            Ok(r) => println!("{name:<24} device={}{lock_note}", r.device),
            Err(e) if json => {
                rows.push(serde_json::json!({"name": name, "error": e.to_string()}));
            }
            Err(e) => {
                println!("{name:<24} (unreadable)");
                broken.push((name, e.to_string()));
            }
        }
    }
    finish_listing(json, rows, "recipe", &broken)
}

/// `list-kernels`: the `--kernel` override's valid values, each with the
/// version-ish knob (a mainline track, or `-` for a vendor tree pinned by ref) and
/// the SoCs it accepts, so a reader can pick one and know it fits their device.
pub(crate) fn kernels(root: &ConfigRoot, json: bool) -> Result {
    let mut broken = Vec::new();
    let mut rows = Vec::new();
    for name in root.list("kernels")? {
        match root.kernel(&name) {
            Ok(k) if json => {
                let socs: Vec<&str> = k.supported_socs.iter().map(|s| s.as_str()).collect();
                rows.push(serde_json::json!({
                    "name": name,
                    "flavor": k.flavor.as_str(),
                    "track": k.track,
                    "socs": socs,
                    "patches": k.patch_profile,
                }));
            }
            Ok(k) => {
                let track = k.track.as_deref().unwrap_or("-");
                let socs = k
                    .supported_socs
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                println!(
                    "{name:<24} {:<8} track={track:<8} socs={socs:<20} patches={}",
                    k.flavor.as_str(),
                    k.patch_profile
                );
            }
            Err(e) if json => {
                rows.push(serde_json::json!({"name": name, "error": e.to_string()}));
            }
            Err(e) => {
                println!("{name:<24} (unreadable)");
                broken.push((name, e.to_string()));
            }
        }
    }
    finish_listing(json, rows, "kernel", &broken)
}

/// `list-features`: the `--feature` override's valid values with their selection
/// gates. An empty `requires_soc`/`requires_arch` imposes no constraint and renders
/// as `any`; conflicts are the other selection-time gate.
pub(crate) fn features(root: &ConfigRoot, json: bool) -> Result {
    let mut broken = Vec::new();
    let mut rows = Vec::new();
    for name in root.list("features")? {
        match root.feature(&name) {
            Ok(f) if json => {
                let socs: Vec<String> = f.requires_soc.iter().map(|s| s.to_string()).collect();
                let arches: Vec<String> = f.requires_arch.iter().map(|a| a.to_string()).collect();
                rows.push(serde_json::json!({
                    "name": name,
                    "requires_soc": socs,
                    "requires_arch": arches,
                    "conflicts": f.conflicts,
                    "description": f.description,
                }));
            }
            Ok(f) => {
                let socs = constraint(&f.requires_soc);
                let arches = constraint(&f.requires_arch);
                print!("{name:<24} soc={socs:<20} arch={arches:<12}");
                if !f.conflicts.is_empty() {
                    print!(" conflicts={}", f.conflicts.join(","));
                }
                println!("  {}", f.description);
            }
            Err(e) if json => {
                rows.push(serde_json::json!({"name": name, "error": e.to_string()}));
            }
            Err(e) => {
                println!("{name:<24} (unreadable)");
                broken.push((name, e.to_string()));
            }
        }
    }
    finish_listing(json, rows, "feature", &broken)
}

#[cfg(test)]
mod tests {
    use crate::testsupport::repo_root;

    #[test]
    fn the_shipped_layers_all_parse() {
        // Every list-* over the shipped config must produce zero unreadable entries;
        // this is the regression gate on a layer file that stops deserializing.
        let root = repo_root();
        for kind in ["devices", "recipes", "kernels", "features"] {
            let names = root.list(kind).unwrap();
            assert!(!names.is_empty(), "{kind} lists nothing");
        }
        assert!(root.list("devices").unwrap().iter().all(|n| root.device(n).is_ok()));
        assert!(root.list("recipes").unwrap().iter().all(|n| root.recipe(n).is_ok()));
        assert!(root.list("kernels").unwrap().iter().all(|n| root.kernel(n).is_ok()));
        assert!(root.list("features").unwrap().iter().all(|n| root.feature(n).is_ok()));
    }
}
