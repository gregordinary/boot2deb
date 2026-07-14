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
//! plus the DDR TPL — inherited from the SoC layer but board-memory-specific, so
//! worth confirming — are the [`research_notes`](DeviceScaffold::research_notes) a
//! caller surfaces after writing.

use crate::model::{BootMethod, Layout, RkbinLayer, Soc};
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
    /// The SoC layer's rkbin defaults, read from `socs/<soc>.toml`. When the SoC
    /// supplies `atf` + `tpl`, a standard-memory board inherits them and the
    /// scaffold emits no `[rkbin]` block — only a note on overriding the DDR TPL
    /// for different memory. When the SoC has no defaults, the scaffold writes a
    /// `[rkbin]` block with `CHANGEME` placeholders the author must fill.
    pub soc_rkbin: RkbinLayer,
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

    /// Whether the SoC layer supplies a complete rkbin default set (ATF + DDR TPL),
    /// so a standard-memory board inherits it and the scaffold emits no `[rkbin]`
    /// block. A partial SoC set (or none) falls back to explicit placeholders.
    fn soc_supplies_blobs(&self) -> bool {
        let set = |o: &Option<String>| o.as_deref().is_some_and(|s| !s.trim().is_empty());
        set(&self.soc_rkbin.atf) && set(&self.soc_rkbin.tpl)
    }

    /// The rkbin ATF blob the SoC layer supplies, or a `CHANGEME` placeholder when
    /// it has none. The ATF (BL31) is SoC-generic, so the SoC default is normally
    /// right; still vendored under `blobs/<soc>/` and content-checked at `update`.
    pub fn atf_suggestion(&self) -> String {
        self.soc_rkbin
            .atf
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("{PLACEHOLDER}-{}_bl31.elf", self.soc.as_str()))
    }

    /// The rkbin DDR TPL blob the SoC layer supplies, or a `CHANGEME` placeholder.
    /// The TPL is **board-memory-specific** (DDR type/speed), so even a SoC default
    /// is only a starting point the author must match to the board's memory.
    pub fn tpl_suggestion(&self) -> String {
        self.soc_rkbin
            .tpl
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| format!("{PLACEHOLDER}-{}_ddr.bin", self.soc.as_str()))
    }

    /// The suggested depthcharge board profile: the device name, which is the
    /// convention `depthcharge-tools` follows for its board codenames (the C201 is
    /// `speedy`). A guess — the codename must exist in that tool's board database,
    /// which resolution cannot check.
    pub fn board_suggestion(&self) -> String {
        self.name.clone()
    }

    /// Render `devices/<name>.toml`. Derivable values are filled; the researched
    /// values carry a best-effort suggestion and a `# TODO:` line, so the file
    /// resolves immediately while flagging what still needs verifying.
    ///
    /// The boot method decides which fields the board even *has*: a board that
    /// compiles u-boot needs a defconfig and a blob set, and one whose firmware is
    /// its own needs a board profile instead. Emitting the other method's fields
    /// would not merely be noise — they are unknown fields, and the file would not
    /// parse.
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
        if self.boot_method == BootMethod::RockchipRkbin {
            let _ = writeln!(s, "\n# TODO: verify this defconfig exists in the u-boot tree (unvalidated — fails at the u-boot build).");
            let _ = writeln!(s, "uboot_defconfig         = {:?}", self.uboot_defconfig_suggestion());
        }
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
        // Left commented, because the honest default for a board nobody has typed at is
        // *no* keymap: boot2deb then writes no /etc/default/keyboard and Debian's own
        // default stands. Only a board with a keyboard under the user's hands — a
        // laptop — has a layout to declare. (Emitted before the boot-method table:
        // every key after a TOML table header is scoped into it.)
        let _ = writeln!(s, "# Console keymap. Uncomment only if this board HAS a keyboard (a laptop); a");
        let _ = writeln!(s, "# headless board leaves it unset. A table gives the model/variant/options too.");
        let _ = writeln!(s, "#   keymap                = \"us\"");
        match self.boot_method {
            BootMethod::RockchipRkbin => self.write_rkbin_block(&mut s),
            BootMethod::Depthcharge => self.write_depthcharge_block(&mut s),
        }
        s
    }

    /// The `[rkbin]` half of a `rockchip-rkbin` device file: inherited from the SoC
    /// where it supplies defaults, explicit placeholders where it does not.
    fn write_rkbin_block(&self, s: &mut String) {
        if self.soc_supplies_blobs() {
            // Standard-memory board: inherit the SoC's rkbin. Emit no `[rkbin]`
            // block, only the override recipe for a board with different DRAM.
            let _ = writeln!(s, "\n# rkbin (ATF + DDR TPL) is inherited from socs/{}.toml. The DDR TPL is", self.soc.as_str());
            let _ = writeln!(s, "# board-memory-specific: if this board's DRAM differs from the SoC default,");
            let _ = writeln!(s, "# override just the TPL (vendor the file under blobs/{}/):", self.soc.as_str());
            let _ = writeln!(s, "#   [rkbin]");
            let _ = writeln!(s, "#   tpl = {:?}", self.tpl_suggestion());
        } else {
            // No SoC default: the author must supply the whole blob set here.
            let _ = writeln!(s, "\n# TODO: the SoC layer supplies no rkbin defaults — provide the blob set and");
            let _ = writeln!(s, "# vendor the files under blobs/{}/. The DDR TPL must match this board's memory.", self.soc.as_str());
            let _ = writeln!(s, "[rkbin]");
            let _ = writeln!(s, "atf = {:?}", self.atf_suggestion());
            let _ = writeln!(s, "tpl = {:?}", self.tpl_suggestion());
        }
    }

    /// The `[depthcharge]` half of a depthcharge device file: which board profile
    /// `depthchargectl` signs for.
    fn write_depthcharge_block(&self, s: &mut String) {
        let board = self.board_suggestion();
        let _ = writeln!(s, "\n# TODO: the depthcharge-tools board profile for this unit — its `board` codename");
        let _ = writeln!(s, "# (`depthchargectl list-boards`). A profile describes the *firmware* the unit runs,");
        let _ = writeln!(s, "# not the board model, so a unit with replacement firmware may take a different one.");
        let _ = writeln!(s, "# Prefer the stock profile as the default: a stock-profile image also boots on a");
        let _ = writeln!(s, "# unit with replacement firmware, while the reverse is not true.");
        let _ = writeln!(s, "[depthcharge]");
        let _ = writeln!(s, "board            = {board:?}");
        let _ = writeln!(s, "supported_boards = [{board:?}]");
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
        let mut notes = Vec::new();
        if self.boot_method == BootMethod::RockchipRkbin {
            notes.push(ResearchNote {
                field: "uboot_defconfig",
                value: self.uboot_defconfig_suggestion(),
                guidance: "must name a defconfig present in the u-boot source tree; \
                           unvalidated at resolve, fails at the u-boot build if wrong",
            });
        }
        notes.push(ResearchNote {
            field: "kernel_dtb",
            value: self.kernel_dtb_suggestion(),
            guidance: "must name a DTB the kernel builds under its dt_dir; \
                       unvalidated at resolve, fails at the kernel build if wrong",
        });
        match self.boot_method {
            // The DDR TPL is always worth verifying against the board's memory, but
            // the guidance differs by whether it is inherited or must be supplied.
            BootMethod::RockchipRkbin if self.soc_supplies_blobs() => {
                notes.push(ResearchNote {
                    field: "rkbin.tpl",
                    value: self.tpl_suggestion(),
                    guidance: "inherited SoC-default DDR init blob — confirm it matches this \
                               board's memory; if the DRAM differs, override `tpl` on the \
                               device layer and vendor the file under blobs/<soc>/",
                });
            }
            BootMethod::RockchipRkbin => {
                // No SoC default: both blobs must be supplied here.
                notes.push(ResearchNote {
                    field: "rkbin.atf",
                    value: self.atf_suggestion(),
                    guidance: "no SoC-default ATF/BL31 blob — supply one and vendor it \
                               under blobs/<soc>/",
                });
                notes.push(ResearchNote {
                    field: "rkbin.tpl",
                    value: self.tpl_suggestion(),
                    guidance: "board-memory-specific DDR init blob — match it to this board's \
                               DRAM type/speed and vendor it under blobs/<soc>/",
                });
            }
            BootMethod::Depthcharge => {
                notes.push(ResearchNote {
                    field: "depthcharge.board",
                    value: self.board_suggestion(),
                    guidance: "must name a board profile depthcharge-tools knows \
                               (`depthchargectl list-boards`); it describes the firmware the \
                               unit runs, so a unit with replacement firmware may take a \
                               different profile. A wrong profile builds an image the \
                               firmware silently refuses to boot",
                });
            }
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
            soc_rkbin: RkbinLayer {
                atf: Some("rk3588_bl31_v1.51.elf".into()),
                tpl: Some("rk3588_ddr_lp4_2112MHz_lp5_2400MHz_v1.19.bin".into()),
                bl32: None,
            },
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
    fn soc_without_blob_defaults_emits_placeholders_and_flags_both() {
        // A SoC layer with no rkbin defaults: the scaffold writes an explicit
        // `[rkbin]` block with CHANGEME placeholders and flags both blobs.
        let mut d = rk1_like();
        d.soc = Soc::Rk3288;
        d.soc_rkbin = RkbinLayer::default();
        assert!(d.atf_suggestion().contains(PLACEHOLDER));
        let toml = d.device_toml();
        assert!(toml.contains("[rkbin]"), "must emit an explicit rkbin block");
        // Parses as a DeviceLayer even with the placeholder blob set.
        let _: crate::model::DeviceLayer = toml::from_str(&toml).unwrap();
        let fields: Vec<&str> = d.research_notes().iter().map(|n| n.field).collect();
        assert!(fields.contains(&"rkbin.atf") && fields.contains(&"rkbin.tpl"));
    }

    #[test]
    fn a_depthcharge_board_scaffolds_a_board_profile_and_no_uboot() {
        // A board whose firmware is its own has no u-boot defconfig and no rkbin
        // blobs — those fields are not merely unnecessary, they are unknown fields on
        // this method's layer, so emitting them would produce a file that fails to
        // parse. What it needs instead is a board profile.
        let mut d = rk1_like();
        d.name = "asus-c201".into();
        d.soc = Soc::Rk3288;
        d.boot_method = BootMethod::Depthcharge;
        d.soc_rkbin = RkbinLayer::default();
        d.features = vec![];

        let toml = d.device_toml();
        let parsed: crate::model::DeviceLayer = toml::from_str(&toml).unwrap();
        assert!(parsed.uboot_defconfig.is_none(), "no u-boot is compiled here");
        assert_eq!(parsed.rkbin, RkbinLayer::default(), "no rkbin chain on this board");
        let dc = parsed.depthcharge.expect("a depthcharge board needs a board profile");
        assert_eq!(dc.board, "asus-c201");
        assert_eq!(dc.supported_boards, vec!["asus-c201"]);
        assert!(!toml.contains("uboot_defconfig"));
        assert!(!toml.lines().any(|l| l.trim_start() == "[rkbin]"));

        // The researched values follow the method: the board profile replaces the
        // u-boot defconfig and the blob set.
        let fields: Vec<&str> = d.research_notes().iter().map(|n| n.field).collect();
        assert_eq!(fields, vec!["kernel_dtb", "depthcharge.board"]);
    }

    #[test]
    fn soc_with_blob_defaults_inherits_and_omits_the_rkbin_block() {
        // A standard-memory board on a SoC with defaults inherits them: no
        // `[rkbin]` block, only the inherited-TPL verification note.
        let d = rk1_like();
        let toml = d.device_toml();
        // The only `[rkbin]` text is the commented override recipe, never a live table.
        assert!(!toml.lines().any(|l| l.trim_start() == "[rkbin]"));
        assert!(toml.contains("inherited from socs/rk3588.toml"));
        let parsed: crate::model::DeviceLayer = toml::from_str(&toml).unwrap();
        assert_eq!(parsed.rkbin, RkbinLayer::default(), "inherits, overrides nothing");
        let notes = d.research_notes();
        let tpl = notes.iter().find(|n| n.field == "rkbin.tpl").unwrap();
        assert!(tpl.guidance.contains("inherited"));
    }
}
