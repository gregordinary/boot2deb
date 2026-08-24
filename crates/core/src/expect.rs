//! Selftest expectations — the `[[expect]]` array a config layer declares and
//! the on-image `.checks` grammar it compiles to.
//!
//! Pure: parsing, validation, and rendering only. An expectation states what a
//! *booted* image must have — a firmware file the driver asks for, a device node,
//! a driver bound to a DT address — as a property of the layer that knows it: the
//! SoC's GPU firmware on the SoC layer, a board's Wi-Fi module on the board's
//! kmod, a capability's render node on the feature. Resolution collects them into
//! [`ResolvedImage::expectations`](crate::model::ResolvedImage::expectations),
//! and the rootfs stage flattens each group into
//! `/etc/boot2deb/selftest.d/<scope>-<name>.checks`, one line per check, which
//! `boot2deb-selftest` (POSIX sh, shipped in the base overlay) runs on the
//! device. The TOML is parsed here on the build host so the shell never parses
//! anything richer than a line.
//!
//! The `.checks` line grammar is deliberately parser-free: `<kind>` then the
//! argument text to end of line, split on whitespace only where the kind takes
//! two arguments (`driver-bound`). Blank lines and full-line `#` comments are
//! skipped; there are no inline comments and no quoting, so a pattern or a card
//! name is carried verbatim.
//!
//! Three check kinds exist only in the generated stream and cannot be authored.
//! `kernel-release` and `kernel-flavor` are derived from the image identity by
//! the build (see the rootfs stage), because a layer restating the pinned kernel
//! would drift from the lock that owns it. `single-kernel` takes no argument at
//! all and is not a property of any layer: it states that the image carries one
//! kernel, which is true of every image this builder produces and is a claim
//! only the build is in a position to make.

use serde::{Deserialize, Serialize};

/// Image-relative directory the generated `.checks` files land in.
///
/// Beside `etc/boot2deb/image.toml` because the two are the same kind of thing:
/// the image's own account of itself, written at build time, read on the device.
pub const CHECKS_DIR: &str = "etc/boot2deb/selftest.d";

/// One runtime check a config layer expects a booted image to pass, as authored
/// in a layer's `[[expect]]` array.
///
/// Every entry is a table with a `check` key naming the kind and the kind's own
/// argument fields — an unknown kind, a missing argument, or an argument that
/// belongs to a different kind is a parse error naming the field, so a typo
/// fails at config load rather than on the board. The kinds split along what the
/// selftest runner can observe:
///
/// - **Disk content** (checkable on any boot of the rootfs, including under
///   `boot2deb try`): [`File`](Self::File), [`Dtb`](Self::Dtb),
///   [`Firmware`](Self::Firmware), [`InitramfsModule`](Self::InitramfsModule).
/// - **Hardware state** (meaningful only on the board; reported not-applicable
///   under emulation): [`DriverBound`](Self::DriverBound),
///   [`Devnode`](Self::Devnode), [`SoundCard`](Self::SoundCard),
///   [`NoDmesgMatch`](Self::NoDmesgMatch).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "check", rename_all = "kebab-case", try_from = "ExpectRaw")]
pub enum Expectation {
    /// A path that must exist on the booted image. `path` is absolute and may
    /// glob (`/boot/vmlinuz-*`); the check passes when the glob matches at least
    /// one entry.
    File {
        /// Absolute path, `*` allowed (e.g. `/boot/initrd.img-*`).
        path: String,
    },
    /// The board's device-tree blob must be installed where its boot path reads
    /// it. `path` is DT-dir-relative (`rockchip/rk3588-turing-rk1.dtb`), matching
    /// [`DeviceLayer::kernel_dtb`](crate::model::DeviceLayer::kernel_dtb); the
    /// runner accepts any of the layouts the shipped kernels install
    /// (`/boot/<name>-<kver>`, a `bindeb-pkg` `/usr/lib/linux-image-<ver>/`, or
    /// Debian's flat `/usr/lib/modules/<ver>/dtb/`). Authored only for a *second*
    /// DTB an overlay ships — the build emits the resolved `kernel_dtb` check
    /// itself, so a board never restates its own.
    Dtb {
        /// DT-dir-relative blob path ending in `.dtb`.
        path: String,
    },
    /// A firmware file a driver on this hardware requests must be present under
    /// `/lib/firmware`. This is the check that catches a blob package that
    /// stopped shipping the path the kernel asks for — the failure that
    /// otherwise surfaces as a GPU (or radio) that is silently absent.
    Firmware {
        /// Path relative to `/lib/firmware` (e.g.
        /// `arm/mali/arch10.8/mali_csffw.bin`).
        path: String,
    },
    /// A kernel module that must be reachable at early boot: either built into
    /// the installed kernel or present in its initramfs. The runner checks
    /// `modules.builtin` first, so a kernel that compiles the driver in passes
    /// without an initrd copy — the invariant is "the boot path can load it",
    /// not "the initrd carries it".
    InitramfsModule {
        /// Module name; `-` and `_` are interchangeable, as modprobe treats them.
        module: String,
    },
    /// A driver must be bound to a specific device — the check that catches a
    /// probe that deferred forever or a power domain that never acked. Passes
    /// when `/sys/bus/*/drivers/<driver>/<device>` exists.
    DriverBound {
        /// Kernel device name, usually `<unit-address>.<node-name>` for a
        /// platform device (e.g. `fb000000.gpu`).
        device: String,
        /// Driver name as it appears under `/sys/bus/*/drivers/` (e.g.
        /// `panthor`).
        driver: String,
    },
    /// A device node that must exist under `/dev` — the userspace-visible proof
    /// that a subsystem came up (`/dev/dri/renderD128`, `/dev/video0`,
    /// `/dev/rga`).
    Devnode {
        /// Absolute path under `/dev`, `*` allowed (e.g. `/dev/dri/renderD*`).
        path: String,
    },
    /// An ALSA card that must be registered, by the name the DT gives it
    /// (`/proc/asound/cards` substring match). Spaces are part of the name
    /// (`H96 Analog`).
    SoundCard {
        /// Card name as registered (the DT `simple-audio-card,name` /
        /// `rockchip,model` string).
        name: String,
    },
    /// A pattern that must **not** appear in the kernel log — the check that
    /// turns "the boot looked fine" into "nothing SError'd on the way up". The
    /// pattern is a POSIX extended regular expression, matched with `grep -E`
    /// against `dmesg`.
    ///
    /// Author these narrowly: a pattern that matches a benign line on some boot
    /// makes every selftest run red. Known-cosmetic lines belong outside the
    /// pattern, not allow-listed after the fact.
    NoDmesgMatch {
        /// POSIX ERE the kernel log must not match.
        pattern: String,
    },
}

/// The authored shape of one `[[expect]]` table, before kind dispatch.
///
/// A flat optional-field struct rather than a serde enum so `deny_unknown_fields`
/// holds (an internally tagged enum would forfeit it) and so the conversion can
/// say *which* field a kind is missing or refusing — serde's own "did not match
/// any variant" names neither.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectRaw {
    /// The check kind, kebab-case (`driver-bound`).
    check: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    module: Option<String>,
    #[serde(default)]
    device: Option<String>,
    #[serde(default)]
    driver: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    pattern: Option<String>,
}

impl TryFrom<ExpectRaw> for Expectation {
    type Error = String;

    /// Dispatch on `check` and validate the argument shape, so an `[[expect]]`
    /// typo is a load-time error naming the kind and the field.
    fn try_from(raw: ExpectRaw) -> Result<Self, Self::Error> {
        // Collect what was authored so the error for a misapplied field can name
        // it ("check = \"firmware\" does not take `driver`").
        let provided: Vec<&str> = [
            raw.path.as_ref().map(|_| "path"),
            raw.module.as_ref().map(|_| "module"),
            raw.device.as_ref().map(|_| "device"),
            raw.driver.as_ref().map(|_| "driver"),
            raw.name.as_ref().map(|_| "name"),
            raw.pattern.as_ref().map(|_| "pattern"),
        ]
        .into_iter()
        .flatten()
        .collect();
        let takes: &[&str] = match raw.check.as_str() {
            "file" | "dtb" | "firmware" | "devnode" => &["path"],
            "initramfs-module" => &["module"],
            "driver-bound" => &["device", "driver"],
            "sound-card" => &["name"],
            "no-dmesg-match" => &["pattern"],
            other => {
                return Err(format!(
                    "unknown check kind '{other}' (known: file, dtb, firmware, \
                     initramfs-module, driver-bound, devnode, sound-card, no-dmesg-match)"
                ))
            }
        };
        if let Some(extra) = provided.iter().find(|f| !takes.contains(f)) {
            return Err(format!(
                "check = \"{}\" does not take `{extra}` (it takes {})",
                raw.check,
                takes.join(", ")
            ));
        }
        let take = |field: &str, value: Option<String>| {
            value.ok_or_else(|| format!("check = \"{}\" requires `{field}`", raw.check))
        };
        let expect = match raw.check.as_str() {
            "file" => Expectation::File {
                path: take("path", raw.path)?,
            },
            "dtb" => Expectation::Dtb {
                path: take("path", raw.path)?,
            },
            "firmware" => Expectation::Firmware {
                path: take("path", raw.path)?,
            },
            "initramfs-module" => Expectation::InitramfsModule {
                module: take("module", raw.module)?,
            },
            "driver-bound" => Expectation::DriverBound {
                device: take("device", raw.device)?,
                driver: take("driver", raw.driver)?,
            },
            "devnode" => Expectation::Devnode {
                path: take("path", raw.path)?,
            },
            "sound-card" => Expectation::SoundCard {
                name: take("name", raw.name)?,
            },
            "no-dmesg-match" => Expectation::NoDmesgMatch {
                pattern: take("pattern", raw.pattern)?,
            },
            _ => unreachable!("kind admitted above"),
        };
        expect.validate()?;
        Ok(expect)
    }
}

impl Expectation {
    /// The check kind as the `.checks` grammar and the error messages spell it.
    pub fn kind(&self) -> &'static str {
        match self {
            Expectation::File { .. } => "file",
            Expectation::Dtb { .. } => "dtb",
            Expectation::Firmware { .. } => "firmware",
            Expectation::InitramfsModule { .. } => "initramfs-module",
            Expectation::DriverBound { .. } => "driver-bound",
            Expectation::Devnode { .. } => "devnode",
            Expectation::SoundCard { .. } => "sound-card",
            Expectation::NoDmesgMatch { .. } => "no-dmesg-match",
        }
    }

    /// The check's argument text as one `.checks` line carries it — the fields
    /// joined with single spaces, in field order. The inverse of nothing: the
    /// runner never reconstructs the TOML, it splits `driver-bound`'s two words
    /// and takes every other kind's text whole.
    pub fn args(&self) -> String {
        match self {
            Expectation::File { path }
            | Expectation::Dtb { path }
            | Expectation::Firmware { path }
            | Expectation::Devnode { path } => path.clone(),
            Expectation::InitramfsModule { module } => module.clone(),
            Expectation::DriverBound { device, driver } => format!("{device} {driver}"),
            Expectation::SoundCard { name } => name.clone(),
            Expectation::NoDmesgMatch { pattern } => pattern.clone(),
        }
    }

    /// One rendered `.checks` line, kind column padded so a generated file reads
    /// as the table it is.
    pub fn render(&self) -> String {
        render_line(self.kind(), &self.args())
    }

    /// Shape rules per kind, enforced at parse so no malformed argument reaches
    /// the flat `.checks` grammar (which has no quoting to hide in).
    fn validate(&self) -> Result<(), String> {
        match self {
            Expectation::File { path } | Expectation::Devnode { path } => {
                absolute_path(self.kind(), path)?;
                if matches!(self, Expectation::Devnode { .. }) && !path.starts_with("/dev/") {
                    return Err(format!("devnode path '{path}' must start with /dev/"));
                }
                Ok(())
            }
            Expectation::Dtb { path } => {
                relative_path(self.kind(), path)?;
                if !path.ends_with(".dtb") {
                    return Err(format!("dtb path '{path}' must end in .dtb"));
                }
                Ok(())
            }
            Expectation::Firmware { path } => relative_path(self.kind(), path),
            Expectation::InitramfsModule { module } => {
                if module.is_empty()
                    || !module
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
                {
                    return Err(format!(
                        "initramfs-module '{module}' is not a module name \
                         (alphanumeric, '-', '_')"
                    ));
                }
                Ok(())
            }
            Expectation::DriverBound { device, driver } => {
                word("driver-bound device", device)?;
                word("driver-bound driver", driver)
            }
            Expectation::SoundCard { name } => line("sound-card name", name),
            Expectation::NoDmesgMatch { pattern } => line("no-dmesg-match pattern", pattern),
        }
    }
}

/// Format one `.checks` line: the kind padded to a fixed column, then the
/// argument text. Shared with the engine's generated identity lines
/// (`kernel-release`, `kernel-flavor`, `single-kernel`) so authored and derived
/// checks render identically.
///
/// Trailing whitespace is trimmed, so a kind that takes no argument at all
/// (`single-kernel`) renders as the bare word rather than as a word and the
/// padding of an argument that is not there.
pub fn render_line(kind: &str, args: &str) -> String {
    format!("{kind:<17} {args}").trim_end().to_string()
}

/// An absolute single-token path: rooted, one word, no `..` segment. Globs are
/// admitted — the runner expands them — but whitespace is not, because the
/// grammar splits on it.
fn absolute_path(kind: &str, path: &str) -> Result<(), String> {
    if !path.starts_with('/') {
        return Err(format!("{kind} path '{path}' must be absolute"));
    }
    word(&format!("{kind} path"), path)?;
    no_parent(kind, path)
}

/// A relative single-token path: not rooted, one word, no `..` segment.
fn relative_path(kind: &str, path: &str) -> Result<(), String> {
    if path.is_empty() || path.starts_with('/') {
        return Err(format!(
            "{kind} path '{path}' must be relative and non-empty"
        ));
    }
    word(&format!("{kind} path"), path)?;
    no_parent(kind, path)
}

fn no_parent(kind: &str, path: &str) -> Result<(), String> {
    if path.split('/').any(|seg| seg == "..") {
        return Err(format!("{kind} path '{path}' must not contain '..'"));
    }
    Ok(())
}

/// One whitespace-free word — the shape the space-split grammar fields need.
fn word(what: &str, value: &str) -> Result<(), String> {
    if value.is_empty() || value.chars().any(|c| c.is_whitespace()) {
        return Err(format!(
            "{what} '{value}' must be a single non-empty word (no whitespace)"
        ));
    }
    Ok(())
}

/// One trimmed single-line value — spaces inside are fine (a card name, an ERE
/// with alternation), but a newline would break the line grammar and edge
/// whitespace would be invisible in review.
fn line(what: &str, value: &str) -> Result<(), String> {
    if value.is_empty() || value.contains('\n') || value != value.trim() {
        return Err(format!(
            "{what} '{value}' must be a single trimmed non-empty line"
        ));
    }
    Ok(())
}

/// Which config layer declared a group of expectations.
///
/// Carried for the same reason [`CaveatScope`](crate::model::CaveatScope) is —
/// "the SoC expects this" and "one feature expects this" answer different
/// questions when a check fails — and because the scope plus the layer name is
/// the generated file's identity: `<scope>-<name>.checks`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExpectScope {
    /// From the SoC layer: true of every board on the part.
    Soc,
    /// From the boot-method layer: the boot artifacts that method installs.
    BootMethod,
    /// From the device layer (the resolved device's or an ancestor's).
    Device,
    /// From the kernel definition.
    Kernel,
    /// From a selected feature.
    Feature,
    /// From an out-of-tree kernel module the device selected.
    Kmod,
}

impl ExpectScope {
    /// The scope as the generated filename and renderings spell it.
    pub fn as_str(self) -> &'static str {
        match self {
            ExpectScope::Soc => "soc",
            ExpectScope::BootMethod => "boot-method",
            ExpectScope::Device => "device",
            ExpectScope::Kernel => "kernel",
            ExpectScope::Feature => "feature",
            ExpectScope::Kmod => "kmod",
        }
    }
}

/// One layer's expectations, resolved: the scope, the layer's name, and its
/// checks in authored order.
///
/// Groups are kept per layer rather than flattened because the layer is the
/// unit of authorship and of the generated file — a failing check names the
/// layer that expected it, which is where the fix (or the stale expectation)
/// lives. Identical checks declared by two layers run twice by design: each
/// layer's file states that layer's contract, and de-duplicating across files
/// would make one layer's edit silently change another's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ExpectationGroup {
    /// The declaring layer kind.
    pub scope: ExpectScope,
    /// The declaring layer's name (SoC slug, device slug, kernel id, feature or
    /// kmod name, boot-method name).
    pub name: String,
    /// The layer's checks, in authored order.
    pub expect: Vec<Expectation>,
}

impl ExpectationGroup {
    /// The generated file's basename: `<scope>-<name>.checks`.
    ///
    /// Every name component is already a config-tree filename stem (device slug,
    /// kernel id, …), so the result is filename-safe by construction.
    pub fn file_name(&self) -> String {
        format!("{}-{}.checks", self.scope.as_str(), self.name)
    }

    /// The generated file's content: a provenance header naming the declaring
    /// layer, then one line per check.
    pub fn render(&self) -> String {
        let mut out = String::new();
        out.push_str(&format!(
            "# Expectations declared by the {} layer '{}'.\n",
            self.scope.as_str(),
            self.name
        ));
        out.push_str(
            "# Run by boot2deb-selftest; see the boot2deb manual (reference/self-test).\n",
        );
        for check in &self.expect {
            out.push_str(&check.render());
            out.push('\n');
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(toml: &str) -> Result<Expectation, String> {
        toml::from_str::<Expectation>(toml).map_err(|e| e.to_string())
    }

    #[test]
    fn every_kind_parses_and_round_trips_its_line() {
        let cases = [
            (
                r#"check = "file"
path = "/boot/vmlinuz-*""#,
                "file              /boot/vmlinuz-*",
            ),
            (
                r#"check = "dtb"
path = "rockchip/rk3588-turing-rk1.dtb""#,
                "dtb               rockchip/rk3588-turing-rk1.dtb",
            ),
            (
                r#"check = "firmware"
path = "arm/mali/arch10.8/mali_csffw.bin""#,
                "firmware          arm/mali/arch10.8/mali_csffw.bin",
            ),
            (
                r#"check = "initramfs-module"
module = "dw_mmc-rockchip""#,
                "initramfs-module  dw_mmc-rockchip",
            ),
            (
                r#"check = "driver-bound"
device = "fb000000.gpu"
driver = "panthor""#,
                "driver-bound      fb000000.gpu panthor",
            ),
            (
                r#"check = "devnode"
path = "/dev/dri/renderD128""#,
                "devnode           /dev/dri/renderD128",
            ),
            (
                r#"check = "sound-card"
name = "H96 Analog""#,
                "sound-card        H96 Analog",
            ),
            (
                r#"check = "no-dmesg-match"
pattern = "SError|Synchronous External Abort""#,
                "no-dmesg-match    SError|Synchronous External Abort",
            ),
        ];
        for (toml, line) in cases {
            let expect = parse(toml).expect(toml);
            assert_eq!(expect.render(), line);
        }
    }

    #[test]
    fn an_unknown_kind_is_named_in_the_error() {
        let err = parse(r#"check = "device-node""#).unwrap_err();
        assert!(err.contains("unknown check kind 'device-node'"), "{err}");
        // The known set rides along so the fix needs no manual lookup.
        assert!(err.contains("devnode"), "{err}");
    }

    #[test]
    fn a_field_from_another_kind_is_refused_by_name() {
        let err = parse(
            r#"check = "firmware"
path = "arm/mali/arch10.8/mali_csffw.bin"
driver = "panthor""#,
        )
        .unwrap_err();
        assert!(err.contains("does not take `driver`"), "{err}");
    }

    #[test]
    fn a_missing_argument_is_named() {
        let err = parse(r#"check = "driver-bound""#).unwrap_err();
        assert!(err.contains("requires `device`"), "{err}");
    }

    #[test]
    fn an_unknown_field_is_a_parse_error() {
        // deny_unknown_fields on the raw shape: a typo'd field name fails even
        // when every real field is well-formed.
        let err = parse(
            r#"check = "devnode"
path = "/dev/rga"
node = "extra""#,
        )
        .unwrap_err();
        assert!(err.contains("node"), "{err}");
    }

    #[test]
    fn shape_rules_reject_what_the_line_grammar_cannot_carry() {
        // Space in a path: the grammar splits on whitespace.
        assert!(parse(
            r#"check = "file"
path = "/boot/my kernel""#
        )
        .is_err());
        // Relative file path: the runner roots at /.
        assert!(parse(
            r#"check = "file"
path = "boot/vmlinuz""#
        )
        .is_err());
        // Parent escape in a firmware path.
        assert!(parse(
            r#"check = "firmware"
path = "../etc/shadow""#
        )
        .is_err());
        // A devnode outside /dev.
        assert!(parse(
            r#"check = "devnode"
path = "/sys/class/drm""#
        )
        .is_err());
        // A dtb that is not a .dtb.
        assert!(parse(
            r#"check = "dtb"
path = "rockchip/rk3588-turing-rk1.dts""#
        )
        .is_err());
        // A multi-word driver name.
        assert!(parse(
            r#"check = "driver-bound"
device = "fb000000.gpu"
driver = "pan thor""#
        )
        .is_err());
        // A newline in a dmesg pattern breaks the one-check-per-line grammar.
        assert!(parse("check = \"no-dmesg-match\"\npattern = \"\"\"SError\nAbort\"\"\"").is_err());
    }

    #[test]
    fn a_group_renders_a_provenance_header_and_its_checks() {
        let group = ExpectationGroup {
            scope: ExpectScope::Soc,
            name: "rk3588".to_string(),
            expect: vec![
                Expectation::Firmware {
                    path: "arm/mali/arch10.8/mali_csffw.bin".to_string(),
                },
                Expectation::DriverBound {
                    device: "fb000000.gpu".to_string(),
                    driver: "panthor".to_string(),
                },
            ],
        };
        assert_eq!(group.file_name(), "soc-rk3588.checks");
        let rendered = group.render();
        assert!(rendered.starts_with("# Expectations declared by the soc layer 'rk3588'.\n"));
        assert!(rendered.contains("\nfirmware          arm/mali/arch10.8/mali_csffw.bin\n"));
        assert!(rendered.ends_with("driver-bound      fb000000.gpu panthor\n"));
    }
}
