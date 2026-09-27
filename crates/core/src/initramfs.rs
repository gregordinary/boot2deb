//! Initramfs module coverage — the modules a board's initramfs must carry, and the
//! decision whether a built initrd carries them.
//!
//! Pure: the name rule, the parse of what a built initrd holds, and the verdict. The
//! rootfs stage runs a target-side report program over the finished rootfs and hands
//! its output to [`InitrdReport::parse`]. [`coverage`] then decides.
//!
//! The check exists because `initramfs-tools` does not make one. It hands each listed
//! name to `dracut-install` with `-o` (`--optional`). A name it cannot find is logged
//! below the default warning threshold and skipped with exit 0. A board whose list
//! names a module its kernel does not build therefore gets an initrd without it. The
//! first sign is a boot that never reaches its root.
//!
//! A listed name is **covered** when the built initrd carries its `.ko`, under any
//! compression suffix, or when the kernel builds it in (`modules.builtin`). Names match
//! exactly by basename, with `-` and `_` equivalent, as `modprobe` treats them.
//!
//! The list itself is config: [`InitramfsLayer`](crate::model::InitramfsLayer) at the
//! SoC and device layers, resolved into
//! [`ResolvedImage::initramfs_modules`](crate::model::ResolvedImage::initramfs_modules).

use crate::error::ConfigError;
use std::collections::{BTreeMap, BTreeSet};

/// The compression suffixes a kernel installs a module or a firmware file under.
///
/// `modules_install` appends one of these to `.ko` under `CONFIG_MODULE_COMPRESS_*`,
/// and firmware packages ship `.xz` or `.zst` files that the kernel's loader
/// decompresses. A path carrying one is the same file for every question asked here.
const COMPRESSION_SUFFIXES: [&str; 3] = [".xz", ".zst", ".gz"];

/// Reject a module name the generated drop-in and the landing check cannot hold.
///
/// A name is what `modules.order` spells: the `.ko` basename without its suffix, from
/// ASCII letters, digits, `-` and `_`. It becomes one line of a `modules.d` drop-in,
/// which `mkinitramfs` reads word by word. A name carrying a `.ko` suffix, a path or
/// whitespace would match no module, and that failure is silent until boot.
pub fn check_module_name(name: &str) -> Result<(), ConfigError> {
    let bad = |why| {
        Err(ConfigError::InvalidField {
            what: "initramfs module",
            value: name.to_string(),
            why,
        })
    };
    if name.is_empty() {
        return bad("empty");
    }
    if name.ends_with(".ko") || name.contains(".ko.") {
        return bad("carries a `.ko` suffix, where the list takes the name modules.order spells");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    {
        return bad(
            "contains a character a module name cannot hold (letters, digits, `-` and `_` only)",
        );
    }
    Ok(())
}

/// The form two module names compare in: `-` and `_` are one character, as they are
/// to `modprobe` and the kernel's own module loader.
pub fn module_key(name: &str) -> String {
    name.replace('-', "_")
}

/// `path` with one trailing [`COMPRESSION_SUFFIXES`] entry removed, if it carries one.
fn strip_compression(path: &str) -> &str {
    COMPRESSION_SUFFIXES
        .iter()
        .find_map(|suffix| path.strip_suffix(suffix))
        .unwrap_or(path)
}

/// The [`module_key`] of a module file's path, or `None` when the path names no
/// module file.
///
/// A module file is a `.ko`, optionally compressed. `lsinitramfs` and
/// `modules.builtin` both list paths, and only the basename identifies the module.
fn module_file_key(path: &str) -> Option<String> {
    let file = path.rsplit('/').next().unwrap_or(path);
    let stem = strip_compression(file).strip_suffix(".ko")?;
    (!stem.is_empty()).then(|| module_key(stem))
}

/// Whether an initrd path lies in a kernel module tree (`lib/modules/<release>/`, or
/// its merged-`/usr` spelling). A `.ko` anywhere else is not a module the kernel
/// loads.
fn in_module_tree(path: &str) -> bool {
    let rel = path.trim_start_matches("./").trim_start_matches('/');
    let rel = rel.strip_prefix("usr/").unwrap_or(rel);
    rel.starts_with("lib/modules/")
}

/// The firmware-relative path of an initrd entry under the firmware tree, with any
/// compression suffix removed, or `None` for an entry outside it.
fn firmware_file(path: &str) -> Option<&str> {
    let rel = path.trim_start_matches("./").trim_start_matches('/');
    let rel = rel.strip_prefix("usr/").unwrap_or(rel);
    rel.strip_prefix("lib/firmware/").map(strip_compression)
}

/// Whether `name` matches the firmware declaration `pattern`.
///
/// A driver can declare firmware with a `*` wildcard (`MODULE_FIRMWARE("brcm/brcmfmac*-sdio.*.bin")`),
/// which `modinfo` reports verbatim. `*` matches any run of characters, `/` included.
/// Every other character matches itself.
fn firmware_matches(pattern: &str, name: &str) -> bool {
    let Some((head, rest)) = pattern.split_once('*') else {
        return pattern == name;
    };
    let Some(after_head) = name.strip_prefix(head) else {
        return false;
    };
    (0..=after_head.len())
        .filter(|&i| after_head.is_char_boundary(i))
        .any(|i| firmware_matches(rest, &after_head[i..]))
}

/// What a built initrd holds, and what the kernel beside it builds in, as the
/// target-side report program describes them.
///
/// The program prints one fact per line, each tagged by its first word:
///
/// | line | meaning |
/// |---|---|
/// | `kernel <release>` | the kernel release the initrd was built for, exactly once |
/// | `initrd <path>` | one path the initrd holds, as `lsinitramfs` lists it |
/// | `builtin <path>` | one line of that kernel's `modules.builtin` |
/// | `firmware <module path> <file>` | one firmware file a module in the initrd declares |
///
/// Tagged lines rather than a structured document, because the program is POSIX `sh`
/// running on the target. A line is what `sed` can prefix without quoting.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InitrdReport {
    /// The kernel release the initrd belongs to, as `uname -r` reports it.
    pub kernel: String,
    /// Every path the initrd holds, in listing order.
    pub initrd: Vec<String>,
    /// The kernel's `modules.builtin` lines, in file order.
    pub builtin: Vec<String>,
    /// `(module path, declared firmware)` pairs for the modules the initrd holds.
    pub firmware: Vec<(String, String)>,
}

impl InitrdReport {
    /// Parse the report program's output.
    ///
    /// An untagged line, a missing `kernel` line or a second one is an error naming
    /// it. A report the build cannot read is a check that did not run, and that has to
    /// fail rather than pass.
    pub fn parse(text: &str) -> Result<Self, String> {
        let mut report = InitrdReport::default();
        let mut kernel = None;
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let (tag, rest) = line
                .split_once(' ')
                .ok_or_else(|| format!("an untagged report line: {line:?}"))?;
            match tag {
                "kernel" => {
                    if kernel.replace(rest.to_string()).is_some() {
                        return Err(format!("a second kernel line: {line:?}"));
                    }
                }
                "initrd" => report.initrd.push(rest.to_string()),
                "builtin" => report.builtin.push(rest.to_string()),
                "firmware" => {
                    let (module, file) = rest
                        .split_once(' ')
                        .ok_or_else(|| format!("a firmware line without a file: {line:?}"))?;
                    report.firmware.push((module.to_string(), file.to_string()));
                }
                _ => return Err(format!("an unknown report tag {tag:?} in {line:?}")),
            }
        }
        report.kernel = kernel.ok_or("no kernel line: the report names no kernel release")?;
        Ok(report)
    }

    /// The [`module_key`] of every module file the initrd holds.
    fn initrd_modules(&self) -> BTreeSet<String> {
        self.initrd
            .iter()
            .filter(|p| in_module_tree(p))
            .filter_map(|p| module_file_key(p))
            .collect()
    }

    /// The [`module_key`] of every module the kernel builds in.
    fn builtin_modules(&self) -> BTreeSet<String> {
        self.builtin
            .iter()
            .filter_map(|p| module_file_key(p))
            .collect()
    }

    /// Every firmware file the initrd holds, relative to the firmware tree.
    fn initrd_firmware(&self) -> BTreeSet<&str> {
        self.initrd
            .iter()
            .filter_map(|p| firmware_file(p))
            .collect()
    }
}

/// A module in the initrd whose declared firmware is absent from it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingFirmware {
    /// The module, as its [`module_key`].
    pub module: String,
    /// Every file it declares, in declaration order. None of them is in the initrd.
    pub firmware: Vec<String>,
}

/// The verdict on one initrd against the resolved module list.
///
/// Each listed name lands in exactly one of the three name lists, in the order the list
/// named them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Coverage {
    /// Listed names whose `.ko` the initrd carries.
    pub in_initrd: Vec<String>,
    /// Listed names the kernel builds in, so no `.ko` exists to carry.
    pub built_in: Vec<String>,
    /// Listed names in neither. Non-empty means the initrd cannot do what the list
    /// says it must, and the build fails naming these.
    pub missing: Vec<String>,
    /// Modules in the initrd that declare firmware, none of which the initrd holds.
    ///
    /// Reported rather than enforced. A driver's declarations name alternatives, one
    /// per chip revision or configuration, and a board needs at most one of them. A
    /// module that needs none at early boot declares them just the same.
    pub missing_firmware: Vec<MissingFirmware>,
}

impl Coverage {
    /// Whether every listed name is covered. Firmware is not part of the answer.
    pub fn complete(&self) -> bool {
        self.missing.is_empty()
    }
}

/// Decide whether `report`'s initrd covers every name in `listed`.
///
/// A name is covered by a `.ko` in the initrd's module tree, or by a `modules.builtin`
/// entry, matched by [`module_key`]. A `.ko` found outside `lib/modules/` does not
/// count. Neither does a name that is only a substring of one that does, so `ext4` is
/// not covered by `fsck.ext4`.
pub fn coverage(listed: &[String], report: &InitrdReport) -> Coverage {
    let in_initrd = report.initrd_modules();
    let built_in = report.builtin_modules();
    let mut verdict = Coverage::default();
    for name in listed {
        let key = module_key(name);
        let bucket = if in_initrd.contains(&key) {
            &mut verdict.in_initrd
        } else if built_in.contains(&key) {
            &mut verdict.built_in
        } else {
            &mut verdict.missing
        };
        bucket.push(name.clone());
    }

    let firmware = report.initrd_firmware();
    let mut declared: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (module, file) in &report.firmware {
        if let Some(key) = module_file_key(module) {
            declared.entry(key).or_default().push(file.clone());
        }
    }
    verdict.missing_firmware = declared
        .into_iter()
        .filter(|(_, files)| {
            !files
                .iter()
                .any(|pattern| firmware.iter().any(|f| firmware_matches(pattern, f)))
        })
        .map(|(module, firmware)| MissingFirmware { module, firmware })
        .collect();
    verdict
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// A report as the program prints it: a kernel line, then the tagged facts.
    fn report(initrd: &[&str], builtin: &[&str], firmware: &[(&str, &str)]) -> InitrdReport {
        let mut text = String::from("kernel 7.2.8-1-armv7\n");
        for p in initrd {
            text.push_str(&format!("initrd {p}\n"));
        }
        for p in builtin {
            text.push_str(&format!("builtin {p}\n"));
        }
        for (m, f) in firmware {
            text.push_str(&format!("firmware {m} {f}\n"));
        }
        InitrdReport::parse(&text).expect("the fixture parses")
    }

    const MMC: &str = "usr/lib/modules/7.2.8-1-armv7/kernel/drivers/mmc/host/dw_mmc-rockchip.ko";

    /// A name the initrd carries is covered, and one it does not is named. The missing
    /// name is the build failure, so it is asserted by value rather than by count.
    #[test]
    fn a_listed_module_the_initrd_lacks_is_named() {
        let r = report(&[MMC], &[], &[]);
        let c = coverage(&names(&["dw_mmc-rockchip", "rk808-regulator"]), &r);
        assert_eq!(c.in_initrd, ["dw_mmc-rockchip"]);
        assert_eq!(c.missing, ["rk808-regulator"]);
        assert!(!c.complete());
    }

    /// A kernel that builds the driver in has no `.ko` to copy, and the boot path still
    /// reaches it. `modules.builtin` is what says so.
    #[test]
    fn a_built_in_module_is_covered_without_a_ko() {
        let r = report(&[], &["kernel/drivers/mmc/core/mmc_block.ko"], &[]);
        let c = coverage(&names(&["mmc_block"]), &r);
        assert_eq!(c.built_in, ["mmc_block"]);
        assert!(c.complete());
    }

    /// `-` and `_` are one character to the module loader, so either spelling in the
    /// list matches either spelling on disk.
    #[test]
    fn dash_and_underscore_are_equivalent_both_ways() {
        let r = report(
            &[MMC],
            &["kernel/drivers/regulator/rk808_regulator.ko"],
            &[],
        );
        let c = coverage(&names(&["dw_mmc_rockchip", "rk808-regulator"]), &r);
        assert_eq!(c.in_initrd, ["dw_mmc_rockchip"]);
        assert_eq!(c.built_in, ["rk808-regulator"]);
        assert!(c.complete());
    }

    /// A compressed module is the same module. Each suffix `modules_install` can write
    /// is accepted, and the bare `.ko` is too.
    #[test]
    fn every_compression_suffix_is_the_same_module() {
        for suffix in ["", ".xz", ".zst", ".gz"] {
            let path = format!("usr/lib/modules/7.2.8/kernel/fs/ext4/ext4.ko{suffix}");
            let r = report(&[&path], &[], &[]);
            let c = coverage(&names(&["ext4"]), &r);
            assert_eq!(c.in_initrd, ["ext4"], "suffix {suffix:?}");
        }
        // An unknown suffix is not a module file.
        let r = report(
            &["usr/lib/modules/7.2.8/kernel/fs/ext4/ext4.ko.bz2"],
            &[],
            &[],
        );
        assert_eq!(coverage(&names(&["ext4"]), &r).missing, ["ext4"]);
    }

    /// The match is exact by basename. `fsck.ext4` holds `ext4` as a substring, and a
    /// `.ko` outside the module tree is not a module the kernel loads, so neither
    /// covers a listed name.
    #[test]
    fn a_substring_or_a_stray_ko_does_not_cover_a_name() {
        let r = report(
            &[
                "usr/sbin/fsck.ext4",
                "usr/share/doc/ext4.ko",
                "usr/lib/modules/7.2.8/kernel/fs/ext4/ext4_extra.ko",
            ],
            &["kernel/fs/jbd2/jbd2.ko"],
            &[],
        );
        assert_eq!(coverage(&names(&["ext4"]), &r).missing, ["ext4"]);
    }

    /// Both the merged-`/usr` path and the classic `lib/modules` one are module trees,
    /// with or without a leading `./`.
    #[test]
    fn both_module_tree_spellings_count() {
        for path in [
            "lib/modules/7.2.8/kernel/fs/ext4/ext4.ko",
            "./usr/lib/modules/7.2.8/kernel/fs/ext4/ext4.ko",
        ] {
            let r = report(&[path], &[], &[]);
            assert!(coverage(&names(&["ext4"]), &r).complete(), "{path}");
        }
    }

    /// A module none of whose declared firmware is in the initrd is reported, and one
    /// that has any of its alternatives is not. Firmware never fails the verdict.
    #[test]
    fn firmware_is_reported_only_when_no_declared_alternative_is_present() {
        let brcm = "usr/lib/modules/7.2.8/kernel/drivers/net/brcmfmac.ko.xz";
        let mali = "usr/lib/modules/7.2.8/kernel/drivers/gpu/panthor.ko";
        let r = report(
            &[
                brcm,
                mali,
                "usr/lib/firmware/brcm/brcmfmac4354-sdio.bin.zst",
            ],
            &[],
            &[
                (brcm, "brcm/brcmfmac4350-sdio.bin"),
                (brcm, "brcm/brcmfmac4354-sdio.bin"),
                (mali, "arm/mali/arch10.8/mali_csffw.bin"),
            ],
        );
        let c = coverage(&names(&["brcmfmac"]), &r);
        assert!(c.complete());
        assert_eq!(
            c.missing_firmware,
            [MissingFirmware {
                module: "panthor".into(),
                firmware: vec!["arm/mali/arch10.8/mali_csffw.bin".into()],
            }]
        );
    }

    /// A declaration can carry a `*` wildcard, which `modinfo` reports as written.
    #[test]
    fn a_wildcard_firmware_declaration_matches_what_it_names() {
        assert!(firmware_matches(
            "brcm/brcmfmac*-sdio.*.bin",
            "brcm/brcmfmac4354-sdio.google,veyron-speedy.bin"
        ));
        assert!(!firmware_matches(
            "brcm/brcmfmac*-sdio.*.bin",
            "brcm/brcmfmac4354-pcie.bin"
        ));
        assert!(firmware_matches("a.bin", "a.bin"));
        assert!(!firmware_matches("a.bin", "a.bin.old"));
    }

    /// A report the build cannot read is a check that did not run, so every malformed
    /// shape is an error rather than an empty, passing report.
    #[test]
    fn a_malformed_report_is_refused() {
        for bad in [
            "initrd a\n",
            "kernel 1\nkernel 2\n",
            "kernel 1\nbogus x\n",
            "kernel 1\nfirmware only-a-module\n",
            "kernel 1\nuntagged\n",
        ] {
            assert!(InitrdReport::parse(bad).is_err(), "{bad:?}");
        }
        let ok = InitrdReport::parse("kernel 7.2.8\n\ninitrd a b\n").unwrap();
        assert_eq!(ok.kernel, "7.2.8");
        // A path is taken whole to the end of the line.
        assert_eq!(ok.initrd, ["a b"]);
    }

    /// The names a drop-in can carry: what `modules.order` spells, and nothing a word
    /// reader could split or a basename match could never meet.
    #[test]
    fn a_module_name_is_what_modules_order_spells() {
        for good in ["dw_mmc-rockchip", "ext4", "i2c-rk3x", "hid-generic"] {
            assert!(check_module_name(good).is_ok(), "{good}");
        }
        for bad in [
            "",
            "ext4.ko",
            "ext4.ko.xz",
            "kernel/fs/ext4",
            "dw mmc",
            "rk808*",
        ] {
            assert!(check_module_name(bad).is_err(), "{bad:?}");
        }
    }
}
