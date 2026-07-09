//! Device/recipe scaffolding — the pure text generator behind `boot2deb
//! new-device`.
//!
//! Bringing up a new board is mostly *transcription*: the closed axis enums
//! ([`Soc`], [`BootMethod`], [`Layout`]) fix the valid choices, the SoC layer
//! supplies the inherited hardware facts, and only a handful of values genuinely
//! have to be researched per board. This module turns a [`DeviceScaffold`] — the
//! decisions a wizard (or UI) has gathered — into the exact `devices/<name>.toml`
//! and `recipes/<name>.toml` text, pre-filling every derivable value and marking
//! the researched ones with `# TODO:` comments plus greppable placeholder values.
//!
//! Pure and deterministic: it renders strings and reports which fields still need a
//! human, doing no I/O. The CLI writes the files and runs the resolve check; a
//! future UI reuses the same rendering. The two unvalidatable, build-late values
//! ([`kernel_dtb`](DeviceScaffold::kernel_dtb_suggestion) and the u-boot defconfig)
//! and the board-memory-specific rkbin blobs are the
//! [`research_notes`](DeviceScaffold::research_notes) a caller surfaces after writing.

use crate::model::{blob_hints, BootMethod, Layout, Soc};
use std::fmt::Write as _;

/// The decisions needed to scaffold a new device (and, optionally, its default
/// recipe). Every enum-typed axis is already a valid choice; the string fields are
/// either derivable defaults or the researched values the caller has gathered.
#[derive(Debug, Clone)]
pub struct DeviceScaffold {
    /// Device name — the `devices/<name>.toml` (and recipe) file stem, and the
    /// `recipe.device` reference.
    pub name: String,
    /// Human-readable board description.
    pub description: String,
    /// The SoC this board uses; fixes arch, `dt_dir`, and the module list by
    /// inheritance, so none of those appear in the device file.
    pub soc: Soc,
    /// Boot method; written as both `boot_method` and the sole
    /// `supported_boot_methods` entry.
    pub boot_method: BootMethod,
    /// Kernel definition id; written as both `default_kernel` and the sole
    /// `supported_kernels` entry.
    pub kernel: String,
    /// Debian suite the board defaults to.
    pub suite: String,
    /// Default image layout.
    pub layout: Layout,
    /// Default image hostname.
    pub hostname: String,
    /// Default image size (authored string, e.g. `2G`).
    pub image_size: String,
    /// The SoC layer's device-tree subdirectory (e.g. `rockchip`), read from
    /// `socs/<soc>.toml` — used only to shape the `kernel_dtb` suggestion.
    pub dt_dir: String,
    /// Features the scaffolded recipe selects. Empty means a plain base image.
    pub features: Vec<String>,
    /// Whether to also render a `recipes/<name>.toml` pinning this device.
    pub emit_recipe: bool,
}

/// A greppable sentinel embedded in every placeholder value the author must
/// replace. Chosen so `grep -r CHANGEME devices/ recipes/` finds all remaining
/// work, and so a placeholder never looks like a real value.
pub const PLACEHOLDER: &str = "CHANGEME";

/// One value the scaffold could not determine, surfaced to the author after the
/// files are written. The rendered file carries a best-effort suggestion so it
/// still *resolves* (proving the layer composition); these notes say which
/// suggestions are guesses that fail late if wrong.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResearchNote {
    /// The TOML key (or `key.subkey`) the author must verify.
    pub field: &'static str,
    /// The value the scaffold wrote — a guess or a `CHANGEME` placeholder.
    pub value: String,
    /// Why it needs research and how to find the real value.
    pub guidance: &'static str,
}

impl DeviceScaffold {
    /// The suggested `uboot_defconfig`: the conventional `<board>-<soc>_defconfig`
    /// name. A guess — the defconfig must exist in the u-boot tree, which resolution
    /// cannot check, so it fails at the u-boot build if wrong.
    pub fn uboot_defconfig_suggestion(&self) -> String {
        format!("{}-{}_defconfig", self.name, self.soc.as_str())
    }

    /// The suggested `kernel_dtb`: `<dt_dir>/<soc>-<board>.dtb`, the usual Rockchip
    /// layout. A guess — the DTB must exist in the kernel tree, unvalidatable until
    /// the kernel build.
    pub fn kernel_dtb_suggestion(&self) -> String {
        format!("{}/{}-{}.dtb", self.dt_dir, self.soc.as_str(), self.name)
    }

    /// The suggested rkbin ATF blob, or a `CHANGEME` placeholder when the SoC has no
    /// known default. The ATF (BL31) is SoC-generic, so the suggestion is usually
    /// right; still vendored under `blobs/<soc>/` and content-checked at `update`.
    pub fn atf_suggestion(&self) -> String {
        blob_hints(self.soc)
            .atf
            .map(String::from)
            .unwrap_or_else(|| format!("{PLACEHOLDER}-{}_bl31.elf", self.soc.as_str()))
    }

    /// The suggested rkbin DDR TPL blob, or a `CHANGEME` placeholder. The TPL is
    /// **board-memory-specific** (LPDDR type/speed), so even a SoC default is only a
    /// starting point the author must match to the board's memory.
    pub fn tpl_suggestion(&self) -> String {
        blob_hints(self.soc)
            .tpl
            .map(String::from)
            .unwrap_or_else(|| format!("{PLACEHOLDER}-{}_ddr.bin", self.soc.as_str()))
    }

    /// Render `devices/<name>.toml`. Derivable values are filled; the four
    /// researched values carry a best-effort suggestion and a `# TODO:` line, so the
    /// file resolves immediately while flagging what still needs verifying.
    pub fn device_toml(&self) -> String {
        let mut s = String::new();
        let _ = writeln!(
            s,
            "# devices/{name}.toml — generated by `boot2deb new-device`.\n\
             # Fill every `# TODO:` value below, then `boot2deb resolve {name}` (or the\n\
             # recipe). The SoC layer supplies arch, dt_dir, and modules by inheritance.",
            name = self.name
        );
        let _ = writeln!(s, "description             = {:?}", self.description);
        let _ = writeln!(s, "soc                     = {:?}                # -> arch, dt_dir, modules", self.soc.as_str());
        let _ = writeln!(s, "boot_method             = {:?}", self.boot_method.as_str());
        let _ = writeln!(s, "supported_boot_methods  = [{:?}]", self.boot_method.as_str());
        let _ = writeln!(s, "\n# TODO: verify this defconfig exists in the u-boot tree (unvalidated — fails at the u-boot build).");
        let _ = writeln!(s, "uboot_defconfig         = {:?}", self.uboot_defconfig_suggestion());
        let _ = writeln!(s, "# TODO: verify this DTB path exists in the kernel tree (unvalidated — fails at the kernel build).");
        let _ = writeln!(s, "kernel_dtb              = {:?}", self.kernel_dtb_suggestion());
        let _ = writeln!(s, "# TODO: board-specific kconfig fragments, or [] for none. Naming a fragment makes its file mandatory.");
        let _ = writeln!(s, "device_config_fragments = []");
        let _ = writeln!(s, "supported_kernels       = [{:?}]", self.kernel);
        let _ = writeln!(s, "default_kernel          = {:?}", self.kernel);
        let _ = writeln!(s, "default_suite           = {:?}", self.suite);
        let _ = writeln!(s, "default_layout          = {:?}               # combined | split", self.layout.as_str());
        let _ = writeln!(s, "hostname                = {:?}", self.hostname);
        let _ = writeln!(s, "image_size              = {:?}", self.image_size);
        let _ = writeln!(s, "\n# DDR TPL is board-memory-specific, so the rkbin blob set lives at the device layer.");
        let _ = writeln!(s, "# TODO: vendor these blobs under blobs/{}/ and verify the TPL matches this board's memory.", self.soc.as_str());
        let _ = writeln!(s, "[rkbin]");
        let _ = writeln!(s, "atf = {:?}", self.atf_suggestion());
        let _ = writeln!(s, "tpl = {:?}", self.tpl_suggestion());
        s
    }

    /// Render `recipes/<name>.toml` — the buildable point pinning this device with
    /// its default kernel, suite, features, and layout. Returns `None` when
    /// [`emit_recipe`](Self::emit_recipe) is unset.
    pub fn recipe_toml(&self) -> Option<String> {
        if !self.emit_recipe {
            return None;
        }
        let mut s = String::new();
        let _ = writeln!(
            s,
            "# recipes/{name}.toml — generated by `boot2deb new-device`.\n\
             # Constraints only; the exact resolution is written to the sibling .lock by\n\
             # `boot2deb update {name}`.",
            name = self.name
        );
        let _ = writeln!(s, "device     = {:?}", self.name);
        let _ = writeln!(s, "kernel     = {:?}", self.kernel);
        let _ = writeln!(s, "suite      = {:?}", self.suite);
        let features = self
            .features
            .iter()
            .map(|f| format!("{f:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        let _ = writeln!(s, "features   = [{features}]");
        let _ = writeln!(s, "layout     = {:?}", self.layout.as_str());
        let _ = writeln!(s, "image_size = {:?}", self.image_size);
        Some(s)
    }

    /// The values the author must verify before the scaffold builds, each with its
    /// written suggestion and guidance. The caller prints these after writing the
    /// files, closing the loop between "it resolves" and "it will actually build".
    pub fn research_notes(&self) -> Vec<ResearchNote> {
        let mut notes = vec![
            ResearchNote {
                field: "uboot_defconfig",
                value: self.uboot_defconfig_suggestion(),
                guidance: "must name a defconfig present in the u-boot source tree; \
                           unvalidated at resolve, fails at the u-boot build if wrong",
            },
            ResearchNote {
                field: "kernel_dtb",
                value: self.kernel_dtb_suggestion(),
                guidance: "must name a DTB the kernel builds under its dt_dir; \
                           unvalidated at resolve, fails at the kernel build if wrong",
            },
            ResearchNote {
                field: "rkbin.tpl",
                value: self.tpl_suggestion(),
                guidance: "board-memory-specific DDR init blob — match it to this board's \
                           LPDDR type/speed and vendor it under blobs/<soc>/",
            },
        ];
        // The ATF suggestion is high-confidence when known; only flag it when it is a
        // placeholder the author must replace outright.
        if self.atf_suggestion().contains(PLACEHOLDER) {
            notes.push(ResearchNote {
                field: "rkbin.atf",
                value: self.atf_suggestion(),
                guidance: "no known default ATF/BL31 blob for this SoC — supply one and \
                           vendor it under blobs/<soc>/",
            });
        }
        notes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rk1_like() -> DeviceScaffold {
        DeviceScaffold {
            name: "h96-max-m9".into(),
            description: "H96 Max M9 (RK3588)".into(),
            soc: Soc::Rk3588,
            boot_method: BootMethod::RockchipRkbin,
            kernel: "rk3588-mainline-7.1".into(),
            suite: "forky".into(),
            layout: Layout::Combined,
            hostname: "h96-max-m9".into(),
            image_size: "2G".into(),
            dt_dir: "rockchip".into(),
            features: vec!["media-accel-rockchip".into()],
            emit_recipe: true,
        }
    }

    #[test]
    fn suggestions_follow_the_rockchip_conventions() {
        let d = rk1_like();
        assert_eq!(d.uboot_defconfig_suggestion(), "h96-max-m9-rk3588_defconfig");
        assert_eq!(d.kernel_dtb_suggestion(), "rockchip/rk3588-h96-max-m9.dtb");
        // RK3588 has known blob hints, so the ATF is a concrete suggestion, not a
        // placeholder; the TPL is a (board-memory-specific) starting point.
        assert!(!d.atf_suggestion().contains(PLACEHOLDER));
        assert!(d.atf_suggestion().contains("bl31"));
    }

    #[test]
    fn device_toml_resolves_and_flags_the_researched_values() {
        let toml = rk1_like().device_toml();
        // Parses as a DeviceLayer (all required keys present, no unknown fields).
        let parsed: crate::model::DeviceLayer = toml::from_str(&toml).unwrap();
        assert_eq!(parsed.soc, Soc::Rk3588);
        assert_eq!(parsed.default_kernel, "rk3588-mainline-7.1");
        assert_eq!(parsed.supported_kernels, vec!["rk3588-mainline-7.1"]);
        // The two unvalidated values are present (so it resolves) and TODO-flagged.
        assert!(toml.contains("uboot_defconfig"));
        assert!(toml.contains("# TODO:"));
    }

    #[test]
    fn recipe_toml_pins_the_device_and_features() {
        let d = rk1_like();
        let recipe = d.recipe_toml().unwrap();
        let parsed: crate::model::Recipe = toml::from_str(&recipe).unwrap();
        assert_eq!(parsed.device, "h96-max-m9");
        assert_eq!(parsed.features, vec!["media-accel-rockchip"]);
        // No recipe when not requested.
        let mut base = d;
        base.emit_recipe = false;
        assert!(base.recipe_toml().is_none());
    }

    #[test]
    fn research_notes_cover_the_late_failing_values() {
        let notes = rk1_like().research_notes();
        let fields: Vec<&str> = notes.iter().map(|n| n.field).collect();
        // RK3588 has a known ATF, so it is not flagged; the three late-failing values are.
        assert_eq!(fields, vec!["uboot_defconfig", "kernel_dtb", "rkbin.tpl"]);
    }

    #[test]
    fn unknown_soc_blobs_are_placeholders_and_flagged() {
        // A SoC with no blob hints yields CHANGEME placeholders and an extra ATF note.
        let mut d = rk1_like();
        d.soc = Soc::Rk3288;
        assert!(d.atf_suggestion().contains(PLACEHOLDER));
        let fields: Vec<&str> = d.research_notes().iter().map(|n| n.field).collect();
        assert!(fields.contains(&"rkbin.atf"));
    }
}
