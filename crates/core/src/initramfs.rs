//! Initramfs coverage: what a built initrd must contain, and the decision whether it
//! does. That is the modules a board lists, and the firmware the kernel's built-in
//! drivers need from it.
//!
//! Pure: the name rule, the parse of what a built initrd holds, and the verdicts. The
//! rootfs stage runs a target-side report program over the finished rootfs and hands
//! its output to [`InitrdReport::parse`]. [`coverage`] and [`missing_builtin_firmware`]
//! then decide.
//!
//! The check exists because neither generator makes one. `initramfs-tools` hands each
//! listed name to `dracut-install` with `-o` (`--optional`). A name it cannot find is
//! logged below the default warning threshold and skipped with exit 0. Under dracut the
//! same miss is logged as an error, and dracut still exits 0, having written the initrd
//! without it.
//! Either way a board whose list names a module its kernel does not build gets an
//! initrd without it. The first sign is a boot that never reaches its root.
//!
//! A listed name is **covered** when the built initrd contains its `.ko`, under any
//! compression suffix, or when the kernel builds it in (`modules.builtin`). Names match
//! exactly by basename, with `-` and `_` equivalent, as `modprobe` treats them.
//!
//! The list itself is config: [`InitramfsLayer`](crate::model::InitramfsLayer) at the
//! SoC and device layers, resolved into
//! [`ResolvedImage::initramfs_modules`](crate::model::ResolvedImage::initramfs_modules).
//!
//! The firmware check needs no config. A driver built into the kernel probes while the
//! initrd is still the root filesystem, and its firmware request reads the initrd. Both
//! generators install each firmware file a built-in driver declares and the image holds.
//! `initramfs-tools` does it in `add_builtin_firmware`, and dracut through the module
//! boot2deb installs for it.
//!
//! A declared file the image holds and the initrd lacks therefore means a generator
//! skipped it. On the RK3588 that is panthor's CSF firmware, and the GPU never appears.
//! Nothing else catches it before hardware, since `boot2deb try` boots a different
//! kernel.

use crate::error::ConfigError;
use std::collections::{BTreeMap, BTreeSet};

/// The compression suffixes a kernel installs a module or a firmware file under.
///
/// `modules_install` appends one of these to `.ko` under `CONFIG_MODULE_COMPRESS_*`,
/// and firmware packages ship `.xz` or `.zst` files that the kernel's loader
/// decompresses. A path ending in one is the same file for every question asked here.
const COMPRESSION_SUFFIXES: [&str; 3] = [".xz", ".zst", ".gz"];

/// Reject a module name the generated drop-in and the landing check cannot hold.
///
/// A name is what `modules.order` spells: the `.ko` basename without its suffix, from
/// ASCII letters, digits, `-` and `_`. It becomes one line of a `modules.d` drop-in,
/// which `mkinitramfs` reads word by word. A name containing a `.ko` suffix, a path or
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
        return bad("has a `.ko` suffix, where the list takes the name modules.order spells");
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

/// `path` with one trailing [`COMPRESSION_SUFFIXES`] entry removed, if it has one.
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
///
/// The name is the one a driver requests. The kernel's loader looks for a request in
/// `updates/<release>/`, `updates/`, `<release>/` and then the firmware root, so an
/// entry in one of the first three directories answers the name without that
/// directory. `release` is the kernel the initrd belongs to.
fn firmware_file<'a>(path: &'a str, release: &str) -> Option<&'a str> {
    let rel = path.trim_start_matches("./").trim_start_matches('/');
    let rel = rel.strip_prefix("usr/").unwrap_or(rel);
    let rel = strip_compression(rel.strip_prefix("lib/firmware/")?);
    let under = |dir: &str| {
        rel.strip_prefix(dir)
            .and_then(|r| r.strip_prefix('/'))
            .filter(|r| !r.is_empty())
    };
    let updates = rel.strip_prefix("updates/");
    Some(
        updates
            .and_then(|r| r.strip_prefix(release).and_then(|r| r.strip_prefix('/')))
            .or(updates)
            .or_else(|| under(release))
            .unwrap_or(rel),
    )
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
/// | `builtin-firmware <module> <file>` | one firmware file a built-in driver declares |
/// | `image-firmware <file>` | one `builtin-firmware` file the image holds |
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
    /// `(module path, declared firmware)` pairs for the modules the initrd holds. Empty
    /// unless the program was asked for them, which a build with an initramfs module
    /// list does.
    pub firmware: Vec<(String, String)>,
    /// `(module name, declared firmware)` pairs for the drivers built into the kernel,
    /// in `modules.builtin.modinfo` order.
    pub builtin_firmware: Vec<(String, String)>,
    /// The declared files of [`builtin_firmware`](Self::builtin_firmware) that the
    /// image holds, in any firmware directory the kernel's loader searches and under
    /// any compression suffix. Each is named as its driver declares it.
    pub image_firmware: Vec<String>,
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
                "builtin-firmware" => {
                    let (module, file) = rest.split_once(' ').ok_or_else(|| {
                        format!("a builtin-firmware line without a file: {line:?}")
                    })?;
                    report
                        .builtin_firmware
                        .push((module.to_string(), file.to_string()));
                }
                "image-firmware" => report.image_firmware.push(rest.to_string()),
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

    /// Every firmware file the initrd holds, by the name a driver requests it under.
    fn initrd_firmware(&self) -> BTreeSet<&str> {
        self.initrd
            .iter()
            .filter_map(|p| firmware_file(p, &self.kernel))
            .collect()
    }
}

/// A module whose firmware is absent from the initrd.
///
/// For a module in the initrd ([`Coverage::missing_firmware`]) it lists every file the
/// module declares. For a built-in driver ([`missing_builtin_firmware`]) it lists the
/// declared files the image holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MissingFirmware {
    /// The module, as its [`module_key`].
    pub module: String,
    /// The files, in declaration order. None of them is in the initrd.
    pub firmware: Vec<String>,
}

/// The verdict on one initrd against the resolved module list.
///
/// Each listed name goes into exactly one of the three name lists, in the order the list
/// named them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Coverage {
    /// Listed names whose `.ko` the initrd contains.
    pub in_initrd: Vec<String>,
    /// Listed names the kernel builds in, so no `.ko` exists to include.
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

/// The firmware built-in drivers declare that the image holds and the initrd does not,
/// grouped by driver in the order the kernel lists them.
///
/// Empty is the only passing answer. A declaration the image does not hold is not
/// counted, since a driver declares one file per chip it supports. The image installs
/// the firmware packages its board needs. Both generators install every declaration the
/// image holds. Its absence from the initrd is therefore a generator that skipped it,
/// and the driver's probe from the initrd fails.
pub fn missing_builtin_firmware(report: &InitrdReport) -> Vec<MissingFirmware> {
    let in_initrd = report.initrd_firmware();
    let in_image: BTreeSet<&str> = report.image_firmware.iter().map(String::as_str).collect();
    let mut missing: Vec<MissingFirmware> = Vec::new();
    for (module, file) in &report.builtin_firmware {
        if !in_image.contains(file.as_str()) || in_initrd.contains(file.as_str()) {
            continue;
        }
        let key = module_key(module);
        match missing.iter_mut().find(|m| m.module == key) {
            Some(m) if !m.firmware.contains(file) => m.firmware.push(file.clone()),
            Some(_) => {}
            None => missing.push(MissingFirmware {
                module: key,
                firmware: vec![file.clone()],
            }),
        }
    }
    missing
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

    /// A name the initrd contains is covered, and one it does not is named. The missing
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

    /// A declaration can contain a `*` wildcard, which `modinfo` reports as written.
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
            "kernel 1\nbuiltin-firmware only-a-module\n",
            "kernel 1\nuntagged\n",
        ] {
            assert!(InitrdReport::parse(bad).is_err(), "{bad:?}");
        }
        let ok = InitrdReport::parse("kernel 7.2.8\n\ninitrd a b\n").unwrap();
        assert_eq!(ok.kernel, "7.2.8");
        // A path is taken whole to the end of the line.
        assert_eq!(ok.initrd, ["a b"]);
    }

    const RK1: &str = "7.3.3-1-arm64";
    const CSF: &str = "arm/mali/arch10.8/mali_csffw.bin";

    /// A report from the RK1's kernel, whose panthor and rockchipdrm are built in: the
    /// initrd's paths, then each driver's declarations and the ones the image holds.
    fn builtin_report(initrd: &[&str], image: &[&str]) -> InitrdReport {
        let mut text = format!("kernel {RK1}\n");
        for p in initrd {
            text.push_str(&format!("initrd {p}\n"));
        }
        for (m, f) in [
            ("rockchipdrm", "rockchip/dptx.bin"),
            ("panthor", "arm/mali/arch14.8/mali_csffw.bin"),
            ("panthor", CSF),
        ] {
            text.push_str(&format!("builtin-firmware {m} {f}\n"));
        }
        for f in image {
            text.push_str(&format!("image-firmware {f}\n"));
        }
        InitrdReport::parse(&text).expect("the fixture parses")
    }

    /// The failure this check exists for: the image holds panthor's CSF firmware and the
    /// initrd does not. A declaration the image does not hold is not counted, since a
    /// driver declares one file per GPU it supports.
    #[test]
    fn built_in_firmware_the_image_holds_must_be_in_the_initrd() {
        let r = builtin_report(&[], &[CSF, "rockchip/dptx.bin"]);
        assert_eq!(
            missing_builtin_firmware(&r),
            [
                MissingFirmware {
                    module: "rockchipdrm".into(),
                    firmware: vec!["rockchip/dptx.bin".into()],
                },
                MissingFirmware {
                    module: "panthor".into(),
                    firmware: vec![CSF.into()],
                },
            ]
        );

        let shipped = builtin_report(
            &["usr/lib/firmware/arm/mali/arch10.8/mali_csffw.bin.zst"],
            &[CSF],
        );
        assert!(missing_builtin_firmware(&shipped).is_empty());

        // An image that holds none of a driver's firmware owes the initrd none of it.
        assert!(missing_builtin_firmware(&builtin_report(&[], &[])).is_empty());
    }

    /// The kernel's loader answers a request from `updates/<release>/`, `updates/` and
    /// `<release>/` before the firmware root, and both generators install a file where
    /// they found it. Each of those places, in either `/usr` spelling, is the file.
    #[test]
    fn built_in_firmware_counts_in_every_directory_the_loader_searches() {
        for path in [
            format!("lib/firmware/updates/{RK1}/{CSF}"),
            format!("usr/lib/firmware/updates/{CSF}.xz"),
            format!("./lib/firmware/{RK1}/{CSF}"),
            format!("usr/lib/firmware/{CSF}"),
        ] {
            let r = builtin_report(&[&path], &[CSF]);
            assert!(missing_builtin_firmware(&r).is_empty(), "{path}");
        }
        // Another kernel's directory does not answer this kernel's request.
        let other = builtin_report(&[&format!("lib/firmware/7.2.9-1-arm64/{CSF}")], &[CSF]);
        assert_eq!(missing_builtin_firmware(&other).len(), 1);
    }

    /// The names a drop-in can contain: what `modules.order` spells, and nothing a word
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
