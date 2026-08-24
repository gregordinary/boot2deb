//! The five `list-*` commands: the discovery surface over the config search path.
//!
//! Each renders one row per entry (or a JSON array under `--json`) and collects the
//! entries that failed to parse, so a corrupt layer file is reported rather than
//! silently dropped. An unreadable entry never fails the listing.
//!
//! The human rendering goes through [`print_columns`], which sizes every column from
//! the data. A listing whose widest name decides its own column stays readable as
//! names grow; a hardcoded width silently breaks the day one does.

use crate::render::{constraint, finish_listing, print_columns};
use boot2deb_core::ConfigRoot;

type Result = std::result::Result<(), Box<dyn std::error::Error>>;

/// `list-devices`: every device layer, with its description.
pub(crate) fn devices(root: &ConfigRoot, json: bool) -> Result {
    let mut broken = Vec::new();
    let mut rows = Vec::new();
    let mut table = Vec::new();
    for name in root.list("devices")? {
        match root.device(&name) {
            Ok(d) if json => {
                rows.push(serde_json::json!({"name": name, "description": d.description}));
            }
            Ok(d) => table.push(vec![name, d.description.clone()]),
            Err(e) if json => {
                rows.push(serde_json::json!({"name": name, "error": e.to_string()}));
            }
            Err(e) => {
                table.push(vec![name.clone(), "(unreadable)".into()]);
                broken.push((name, e.to_string()));
            }
        }
    }
    print_columns(&table);
    finish_listing(json, rows, "device", &broken)
}

/// `list-recipes`: every recipe, its device, its support claim, and whether it has a
/// committed lock — a recipe without one is not buildable until `update` resolves it,
/// so the listing says so up front instead of letting `build` be the first to fail.
///
/// The support claim is shown here because it is what a reader choosing a recipe most
/// wants to know about it; `support-matrix` is the same claim beside the pins it was
/// made against. A locally authored recipe declaring none renders `-`.
pub(crate) fn recipes(root: &ConfigRoot, json: bool) -> Result {
    let mut broken = Vec::new();
    let mut rows = Vec::new();
    let mut table = Vec::new();
    for name in root.list_recipes()? {
        let (lock_state, lock_note) = match root.lock(&name) {
            Ok(_) => ("ok", ""),
            Err(boot2deb_core::ConfigError::NotFound { .. }) => (
                "missing",
                "[no lock — run `boot2deb update` to make it buildable]",
            ),
            Err(_) => ("unreadable", "[lock unreadable]"),
        };
        match root.recipe(&name) {
            Ok(r) if json => {
                rows.push(serde_json::json!({
                    "name": name, "device": r.device, "lock": lock_state,
                    "support": r.support.as_ref().map(|s| serde_json::json!({
                        "status": s.status.as_str(), "date": s.date,
                    })),
                }));
            }
            Ok(r) => {
                let support = r.support.as_ref().map_or("-", |s| s.status.as_str());
                table.push(vec![
                    name,
                    format!("device={}", r.device),
                    format!("support={support}"),
                    lock_note.to_string(),
                ]);
            }
            Err(e) if json => {
                rows.push(serde_json::json!({"name": name, "error": e.to_string()}));
            }
            Err(e) => {
                table.push(vec![name.clone(), "(unreadable)".into()]);
                broken.push((name, e.to_string()));
            }
        }
    }
    print_columns(&table);
    finish_listing(json, rows, "recipe", &broken)
}

/// `list-kernels`: the `--kernel` override's valid values, each with the
/// version-ish knob (a mainline track, a `-` for a vendor tree pinned by ref, or the
/// package a distro kernel installs) and the SoCs it accepts, so a reader can pick
/// one and know it fits their device.
pub(crate) fn kernels(root: &ConfigRoot, json: bool) -> Result {
    let mut broken = Vec::new();
    let mut rows = Vec::new();
    let mut table = Vec::new();
    for name in root.list("kernels")? {
        match root.kernel(&name) {
            Ok(k) if json => {
                let socs: Vec<&str> = k.supported_socs().iter().map(|s| s.as_str()).collect();
                let (flavor, track, patches) = kernel_fields(&k);
                rows.push(serde_json::json!({
                    "name": name,
                    "flavor": flavor,
                    "track": track,
                    "socs": socs,
                    "patches": patches,
                }));
            }
            Ok(k) => {
                let (flavor, track, patches) = kernel_fields(&k);
                let socs = k
                    .supported_socs()
                    .iter()
                    .map(|s| s.as_str())
                    .collect::<Vec<_>>()
                    .join(",");
                // The version knob is labelled for what it *is*: a compiled kernel
                // tracks an upstream version, a distro kernel names a package and lets
                // the suite decide the version.
                let version = match &k {
                    boot2deb_core::model::KernelDef::Compiled(_) => {
                        format!("track={}", track.as_deref().unwrap_or("-"))
                    }
                    boot2deb_core::model::KernelDef::Distro(_) => {
                        format!("package={}", track.as_deref().unwrap_or("-"))
                    }
                };
                table.push(vec![
                    name,
                    flavor,
                    version,
                    format!("socs={socs}"),
                    format!("patches={patches}"),
                ]);
            }
            Err(e) if json => {
                rows.push(serde_json::json!({"name": name, "error": e.to_string()}));
            }
            Err(e) => {
                table.push(vec![name.clone(), "(unreadable)".into()]);
                broken.push((name, e.to_string()));
            }
        }
    }
    print_columns(&table);
    finish_listing(json, rows, "kernel", &broken)
}

/// The three display fields of a kernel definition, per flavor: how it is obtained,
/// its version knob, and its patch series.
///
/// The knobs differ because the kernels do. A compiled kernel tracks an upstream
/// version and applies a patch series; a distro kernel has neither — its version
/// comes from the suite and it is patched by Debian — so what a reader wants to see
/// there is the package that installs it.
fn kernel_fields(k: &boot2deb_core::model::KernelDef) -> (String, Option<String>, String) {
    use boot2deb_core::model::KernelDef;
    match k {
        KernelDef::Compiled(k) => (
            k.flavor.as_str().to_string(),
            k.track.clone(),
            if k.patch_series.is_empty() {
                "none".to_string()
            } else {
                k.patch_series.join(", ")
            },
        ),
        KernelDef::Distro(k) => (
            k.flavor.as_str().to_string(),
            Some(k.package.clone()),
            "none".to_string(),
        ),
    }
}

/// `list-features`: the `--feature` override's valid values with their selection
/// gates. An empty `requires_soc`/`requires_arch` imposes no constraint and renders
/// as `any`; conflicts are the other selection-time gate.
pub(crate) fn features(root: &ConfigRoot, json: bool) -> Result {
    let mut broken = Vec::new();
    let mut rows = Vec::new();
    let mut table = Vec::new();
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
                // The conflicts cell is empty rather than absent for a feature with
                // none, so every row's description still lands in one column.
                let conflicts = if f.conflicts.is_empty() {
                    String::new()
                } else {
                    format!("conflicts={}", f.conflicts.join(","))
                };
                table.push(vec![
                    name,
                    format!("soc={}", constraint(&f.requires_soc)),
                    format!("arch={}", constraint(&f.requires_arch)),
                    conflicts,
                    f.description.clone(),
                ]);
            }
            Err(e) if json => {
                rows.push(serde_json::json!({"name": name, "error": e.to_string()}));
            }
            Err(e) => {
                table.push(vec![name.clone(), "(unreadable)".into()]);
                broken.push((name, e.to_string()));
            }
        }
    }
    print_columns(&table);
    finish_listing(json, rows, "feature", &broken)
}

/// `list-kmods`: the out-of-tree kernel-module sets a device's `device_kmods` may name,
/// with the driver ref they track and the modules they ship — the two things that decide
/// whether an existing kmod already covers a new board's chip.
pub(crate) fn kmods(root: &ConfigRoot, json: bool) -> Result {
    let mut broken = Vec::new();
    let mut rows = Vec::new();
    let mut table = Vec::new();
    for name in root.list("kmods")? {
        match root.kmod(&name) {
            Ok(k) if json => {
                rows.push(serde_json::json!({
                    "name": name,
                    "git": k.git,
                    "ref": k.git_ref,
                    "modules": k.modules,
                    "description": k.description,
                }));
            }
            Ok(k) => {
                let modules = if k.modules.is_empty() {
                    "all".to_string()
                } else {
                    k.modules.join(",")
                };
                table.push(vec![
                    name,
                    format!("ref={}", k.git_ref),
                    format!("modules={modules}"),
                    k.description.clone(),
                ]);
            }
            Err(e) if json => {
                rows.push(serde_json::json!({"name": name, "error": e.to_string()}));
            }
            Err(e) => {
                table.push(vec![name.clone(), "(unreadable)".into()]);
                broken.push((name, e.to_string()));
            }
        }
    }
    print_columns(&table);
    finish_listing(json, rows, "kmod", &broken)
}

#[cfg(test)]
mod tests {
    use crate::testsupport::repo_root;

    #[test]
    fn the_shipped_layers_all_parse() {
        // Every list-* over the shipped config must produce zero unreadable entries;
        // this is the regression gate on a layer file that stops deserializing.
        let root = repo_root();
        // The flat layers scan a single directory; recipes nest one level under their
        // device folder, so they have their own recursive lister.
        for kind in ["devices", "kernels", "features", "kmods"] {
            let names = root.list(kind).unwrap();
            assert!(!names.is_empty(), "{kind} lists nothing");
        }
        assert!(
            !root.list_recipes().unwrap().is_empty(),
            "recipes lists nothing"
        );
        assert!(root
            .list("devices")
            .unwrap()
            .iter()
            .all(|n| root.device(n).is_ok()));
        assert!(root
            .list_recipes()
            .unwrap()
            .iter()
            .all(|n| root.recipe(n).is_ok()));
        assert!(root
            .list("kernels")
            .unwrap()
            .iter()
            .all(|n| root.kernel(n).is_ok()));
        assert!(root
            .list("features")
            .unwrap()
            .iter()
            .all(|n| root.feature(n).is_ok()));
        assert!(root
            .list("kmods")
            .unwrap()
            .iter()
            .all(|n| root.kmod(n).is_ok()));
    }

    #[test]
    fn a_features_hardware_gate_names_only_hardware_that_exists() {
        // `list-features` prints these gates verbatim, so a SoC or arch with no layer
        // under the config root advertises a configuration nobody can select: no
        // device resolves to it, so the feature it gates is unreachable. The enums are
        // wider than the tree deliberately (a variant lands in `model.rs` before its
        // layer does), which is exactly why the shipped config must not name one.
        let root = repo_root();
        for name in root.list("features").unwrap() {
            let f = root.feature(&name).unwrap();
            for soc in &f.requires_soc {
                assert!(
                    root.soc(*soc).is_ok(),
                    "feature '{name}' requires_soc = {soc}, but there is no socs/{soc}.toml"
                );
            }
            for arch in &f.requires_arch {
                assert!(
                    root.arch(*arch).is_ok(),
                    "feature '{name}' requires_arch = {arch}, but there is no arches/{arch}.toml"
                );
            }
        }
    }
}
