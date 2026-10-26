//! The rules every `.deb` boot2deb produces is held to before it becomes apt input.
//!
//! A build makes its own packages several ways. `dpkg-buildpackage` makes the media
//! userspace, the kernel's `bindeb-pkg` makes the kernel's, and boot2deb assembles the
//! u-boot, module, firmware, ffmpeg and application packages itself. The rootfs then
//! installs all of them from a *trusted* local repository. So a defect in one ships in
//! the image with nothing to say so. A host binary in a target package is one such
//! defect, and a file owned by the build user is another. These rules are what each
//! package has to show instead.
//!
//! Every rule is about the package's own structure, with no knowledge of what the
//! package is for:
//!
//! - [`Rule::ControlField`]: the control file names the package, its version, its
//!   architecture, a maintainer and a description.
//! - [`Rule::Architecture`]: the package is `all` or the build's target architecture,
//!   which is what a cross build that picked up the host's comes out as.
//! - [`Rule::ForeignElf`] and [`Rule::ElfInArchAll`]: every ELF object is for the
//!   target, and an `all` package carries none. Firmware is exempt from both, since it
//!   runs on a processor that is not the one the package installs on.
//! - [`Rule::Owner`]: every entry is owned by root. Anything else is the build user's
//!   id leaking through a missing `--root-owner-group`.
//! - [`Rule::Setuid`] and [`Rule::WorldWritable`]: no package of boot2deb's needs
//!   either mode.
//! - [`Rule::UsrLocal`]: nothing under `/usr/local`, which Debian Policy reserves for
//!   the administrator.
//! - [`Rule::MemberPath`]: every member is a relative `./` path with no `..` step.
//! - [`Rule::Empty`]: the package installs at least one non-directory.
//!
//! Pure: the rules read a [`DebContents`] the engine's reader produced, and decide
//! nothing about files on disk.

use crate::model::Arch;
use serde::Serialize;
use std::collections::BTreeMap;

/// The control fields [`Rule::ControlField`] requires, in the order a finding names them.
pub const REQUIRED_FIELDS: &[&str] = &[
    "Package",
    "Version",
    "Architecture",
    "Maintainer",
    "Description",
];

/// The directories whose contents run on another processor, and so are exempt from the
/// ELF rules. A path is inside one when it starts with it.
pub const FIRMWARE_DIRS: &[&str] = &["./lib/firmware/", "./usr/lib/firmware/"];

/// What kind of filesystem object a data member is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A directory.
    Dir,
    /// A symbolic link.
    Symlink,
    /// A hard link to an earlier member.
    Hardlink,
    /// A device node, FIFO, or anything else tar can carry.
    Other,
}

/// The ELF class and machine of an object, read from its first bytes by [`elf_ident`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct ElfIdent {
    /// `true` for a 64-bit object, `false` for 32-bit.
    pub wide: bool,
    /// The `e_machine` field (`EM_AARCH64` is 183, `EM_ARM` is 40).
    pub machine: u16,
}

/// The bytes [`elf_ident`] needs: the identification block and `e_type`/`e_machine`.
pub const ELF_IDENT_LEN: usize = 20;

/// Identify an ELF object from its first [`ELF_IDENT_LEN`] bytes, or `None` for
/// anything that is not one. The class and byte order come from the identification
/// block, and `e_machine` is read in that byte order.
pub fn elf_ident(prefix: &[u8]) -> Option<ElfIdent> {
    if prefix.len() < ELF_IDENT_LEN || &prefix[..4] != b"\x7fELF" {
        return None;
    }
    let wide = match prefix[4] {
        1 => false,
        2 => true,
        _ => return None,
    };
    let bytes = [prefix[18], prefix[19]];
    let machine = match prefix[5] {
        1 => u16::from_le_bytes(bytes),
        2 => u16::from_be_bytes(bytes),
        _ => return None,
    };
    Some(ElfIdent { wide, machine })
}

/// The ELF identity an object built for `arch` carries.
pub fn target_elf(arch: Arch) -> ElfIdent {
    match arch {
        Arch::Arm64 => ElfIdent {
            wide: true,
            machine: 183,
        },
        Arch::Armv7 => ElfIdent {
            wide: false,
            machine: 40,
        },
        Arch::Riscv64 => ElfIdent {
            wide: true,
            machine: 243,
        },
    }
}

/// One member of a package's data archive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DebEntry {
    /// The member path as the archive stores it (`./usr/bin/ffmpeg`).
    pub path: String,
    /// What kind of object it is.
    pub kind: EntryKind,
    /// The permission bits, including the setuid, setgid and sticky bits.
    pub mode: u32,
    /// The numeric owner.
    pub uid: u64,
    /// The numeric group.
    pub gid: u64,
    /// The ELF identity of a regular file that is an ELF object, else `None`.
    pub elf: Option<ElfIdent>,
}

/// What the rules read of one `.deb`: its control fields and its data members.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DebContents {
    /// The control file's fields, by name. A continuation line is folded into the
    /// field it continues.
    pub control: BTreeMap<String, String>,
    /// Every data member, in archive order.
    pub entries: Vec<DebEntry>,
}

/// A rule a package can break. Serialized as its kebab-case id, the token a finding
/// is reported under.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum Rule {
    /// A required control field is missing or empty.
    ControlField,
    /// `Architecture` is neither `all` nor the build's target.
    Architecture,
    /// An ELF object is for another architecture than the package's.
    ForeignElf,
    /// An `Architecture: all` package carries an ELF object.
    ElfInArchAll,
    /// A member is not owned by root.
    Owner,
    /// A member carries the setuid or setgid bit.
    Setuid,
    /// A member is writable by everyone (a sticky directory excepted).
    WorldWritable,
    /// A member lies under `/usr/local`.
    UsrLocal,
    /// A member path is not relative to `./`, or steps through `..`.
    MemberPath,
    /// The package installs no file, link or node, only directories.
    Empty,
}

impl Rule {
    /// The rule's kebab-case id, as a finding names it.
    pub fn id(&self) -> &'static str {
        match self {
            Rule::ControlField => "control-field",
            Rule::Architecture => "architecture",
            Rule::ForeignElf => "foreign-elf",
            Rule::ElfInArchAll => "elf-in-arch-all",
            Rule::Owner => "owner",
            Rule::Setuid => "setuid",
            Rule::WorldWritable => "world-writable",
            Rule::UsrLocal => "usr-local",
            Rule::MemberPath => "member-path",
            Rule::Empty => "empty",
        }
    }
}

/// One broken rule.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Finding {
    /// The rule broken.
    pub rule: Rule,
    /// The member it was broken by, or `None` for a rule about the package as a whole.
    pub path: Option<String>,
    /// What was found, phrased for an operator.
    pub detail: String,
}

/// Hold one package to every rule, for a build whose target is `arch`.
///
/// Returns every finding rather than the first, so one run names everything a
/// producer has to fix. An empty result admits the package to the local repository.
pub fn check(contents: &DebContents, arch: Arch) -> Vec<Finding> {
    let mut out = Vec::new();
    for field in REQUIRED_FIELDS {
        if contents
            .control
            .get(*field)
            .is_none_or(|v| v.trim().is_empty())
        {
            out.push(Finding {
                rule: Rule::ControlField,
                path: None,
                detail: format!("the control file has no {field} field"),
            });
        }
    }
    let package_arch = contents
        .control
        .get("Architecture")
        .map(|a| a.trim())
        .unwrap_or("");
    let arch_all = package_arch == "all";
    if !package_arch.is_empty() && !arch_all && package_arch != arch.debian_arch() {
        out.push(Finding {
            rule: Rule::Architecture,
            path: None,
            detail: format!(
                "Architecture is {package_arch}, and this build targets {}",
                arch.debian_arch()
            ),
        });
    }
    let target = target_elf(arch);
    for entry in &contents.entries {
        let path = || Some(entry.path.clone());
        if entry.path != "./" && entry.path != "." {
            let relative = entry.path.starts_with("./");
            let climbs = entry.path.split('/').any(|step| step == "..");
            if !relative || climbs {
                out.push(Finding {
                    rule: Rule::MemberPath,
                    path: path(),
                    detail: "a member path must start with ./ and never step through ..".into(),
                });
            }
        }
        if entry.uid != 0 || entry.gid != 0 {
            out.push(Finding {
                rule: Rule::Owner,
                path: path(),
                detail: format!("owned by {}:{}, not root", entry.uid, entry.gid),
            });
        }
        if entry.kind != EntryKind::Symlink {
            if entry.mode & 0o6000 != 0 {
                out.push(Finding {
                    rule: Rule::Setuid,
                    path: path(),
                    detail: format!("mode {:04o} sets the setuid or setgid bit", entry.mode),
                });
            }
            let sticky_dir = entry.kind == EntryKind::Dir && entry.mode & 0o1000 != 0;
            if entry.mode & 0o002 != 0 && !sticky_dir {
                out.push(Finding {
                    rule: Rule::WorldWritable,
                    path: path(),
                    detail: format!("mode {:04o} is writable by everyone", entry.mode),
                });
            }
        }
        if entry.path.starts_with("./usr/local/") {
            out.push(Finding {
                rule: Rule::UsrLocal,
                path: path(),
                detail: "Debian Policy reserves /usr/local for the administrator".into(),
            });
        }
        let firmware = FIRMWARE_DIRS.iter().any(|d| entry.path.starts_with(d));
        if let (Some(elf), false) = (entry.elf, firmware) {
            if arch_all {
                out.push(Finding {
                    rule: Rule::ElfInArchAll,
                    path: path(),
                    detail: "an Architecture: all package carries an ELF object".into(),
                });
            } else if elf != target {
                out.push(Finding {
                    rule: Rule::ForeignElf,
                    path: path(),
                    detail: format!(
                        "a {}-bit ELF object for machine {}, and this build targets {} \
                         ({}-bit, machine {})",
                        if elf.wide { 64 } else { 32 },
                        elf.machine,
                        arch.debian_arch(),
                        if target.wide { 64 } else { 32 },
                        target.machine
                    ),
                });
            }
        }
    }
    if !contents.entries.iter().any(|e| e.kind != EntryKind::Dir) {
        out.push(Finding {
            rule: Rule::Empty,
            path: None,
            detail: "the package installs only directories".into(),
        });
    }
    out
}

/// Parse a control file into its fields. A line that starts with whitespace continues
/// the field before it, joined with a newline as deb822 reads it. A line with no `:`
/// that does not continue a field is skipped, since the rules ask only after named
/// fields.
pub fn parse_control(text: &str) -> BTreeMap<String, String> {
    let mut fields: BTreeMap<String, String> = BTreeMap::new();
    let mut last: Option<String> = None;
    for line in text.lines() {
        if line.starts_with([' ', '\t']) {
            if let Some(value) = last.as_ref().and_then(|k| fields.get_mut(k)) {
                value.push('\n');
                value.push_str(line.trim());
            }
            continue;
        }
        if let Some((key, value)) = line.split_once(':') {
            let key = key.trim().to_string();
            fields.insert(key.clone(), value.trim().to_string());
            last = Some(key);
        }
    }
    fields
}

#[cfg(test)]
mod tests {
    use super::*;

    fn control(arch: &str) -> BTreeMap<String, String> {
        parse_control(&format!(
            "Package: librga2\nVersion: 2.2.0-1\nArchitecture: {arch}\n\
             Maintainer: boot2deb <build@boot2deb>\nDescription: RGA\n more text\n"
        ))
    }

    fn file(path: &str) -> DebEntry {
        DebEntry {
            path: path.into(),
            kind: EntryKind::File,
            mode: 0o644,
            uid: 0,
            gid: 0,
            elf: None,
        }
    }

    fn dir(path: &str) -> DebEntry {
        DebEntry {
            kind: EntryKind::Dir,
            mode: 0o755,
            ..file(path)
        }
    }

    fn elf(path: &str, wide: bool, machine: u16) -> DebEntry {
        DebEntry {
            mode: 0o755,
            elf: Some(ElfIdent { wide, machine }),
            ..file(path)
        }
    }

    fn rules(findings: &[Finding]) -> Vec<Rule> {
        findings.iter().map(|f| f.rule).collect()
    }

    #[test]
    fn a_well_formed_target_package_breaks_no_rule() {
        let deb = DebContents {
            control: control("arm64"),
            entries: vec![
                dir("./"),
                dir("./usr/lib/aarch64-linux-gnu/"),
                elf("./usr/lib/aarch64-linux-gnu/librga.so.2", true, 183),
                DebEntry {
                    kind: EntryKind::Symlink,
                    mode: 0o777,
                    ..file("./usr/lib/aarch64-linux-gnu/librga.so")
                },
            ],
        };
        assert_eq!(check(&deb, Arch::Arm64), []);
    }

    #[test]
    fn a_host_binary_in_a_target_package_is_foreign() {
        // The defect a cross build that reached for the host compiler produces.
        let deb = DebContents {
            control: control("arm64"),
            entries: vec![elf("./usr/bin/tool", true, 62)],
        };
        let findings = check(&deb, Arch::Arm64);
        assert_eq!(rules(&findings), [Rule::ForeignElf]);
        assert_eq!(findings[0].path.as_deref(), Some("./usr/bin/tool"));
        // A 64-bit ARM object in a 32-bit ARM package is foreign by class alone.
        let armhf = DebContents {
            control: control("armhf"),
            entries: vec![elf("./usr/bin/tool", true, 183)],
        };
        assert_eq!(rules(&check(&armhf, Arch::Armv7)), [Rule::ForeignElf]);
    }

    #[test]
    fn firmware_is_exempt_from_the_elf_rules() {
        // A co-processor image is ELF for its own core, in either kind of package.
        for arch in ["arm64", "all"] {
            let deb = DebContents {
                control: control(arch),
                entries: vec![elf("./lib/firmware/mcu.elf", false, 40)],
            };
            assert_eq!(check(&deb, Arch::Arm64), [], "{arch}");
        }
    }

    #[test]
    fn an_arch_all_package_carries_no_elf_and_names_no_other_arch() {
        let deb = DebContents {
            control: control("all"),
            entries: vec![elf("./usr/share/x/helper", true, 183)],
        };
        assert_eq!(rules(&check(&deb, Arch::Arm64)), [Rule::ElfInArchAll]);
        let amd64 = DebContents {
            control: control("amd64"),
            entries: vec![file("./usr/share/doc/x/README")],
        };
        assert_eq!(rules(&check(&amd64, Arch::Arm64)), [Rule::Architecture]);
    }

    #[test]
    fn ownership_modes_and_paths_are_each_their_own_finding() {
        let deb = DebContents {
            control: control("arm64"),
            entries: vec![
                DebEntry {
                    uid: 1000,
                    gid: 1000,
                    ..file("./etc/x.conf")
                },
                DebEntry {
                    mode: 0o4755,
                    ..file("./usr/bin/su-ish")
                },
                DebEntry {
                    mode: 0o666,
                    ..file("./var/lib/x/state")
                },
                file("./usr/local/bin/x"),
                file("usr/bin/no-dot"),
                file("./usr/../etc/escape"),
                // A sticky world-writable directory is the one shape that mode is for.
                DebEntry {
                    mode: 0o1777,
                    ..dir("./var/tmp/x/")
                },
            ],
        };
        assert_eq!(
            rules(&check(&deb, Arch::Arm64)),
            [
                Rule::Owner,
                Rule::Setuid,
                Rule::WorldWritable,
                Rule::UsrLocal,
                Rule::MemberPath,
                Rule::MemberPath,
            ]
        );
    }

    #[test]
    fn missing_control_fields_and_an_empty_package_are_package_level() {
        let deb = DebContents {
            control: parse_control("Package: x\nVersion: 1\nArchitecture: arm64\n"),
            entries: vec![dir("./"), dir("./usr/")],
        };
        let findings = check(&deb, Arch::Arm64);
        assert_eq!(
            rules(&findings),
            [Rule::ControlField, Rule::ControlField, Rule::Empty]
        );
        assert!(findings.iter().all(|f| f.path.is_none()));
        assert!(findings[0].detail.contains("Maintainer"), "{findings:?}");
    }

    #[test]
    fn elf_ident_reads_class_and_machine_in_the_objects_byte_order() {
        let mut le = [0u8; ELF_IDENT_LEN];
        le[..4].copy_from_slice(b"\x7fELF");
        le[4] = 2;
        le[5] = 1;
        le[18..20].copy_from_slice(&183u16.to_le_bytes());
        assert_eq!(
            elf_ident(&le),
            Some(ElfIdent {
                wide: true,
                machine: 183
            })
        );
        let mut be = le;
        be[4] = 1;
        be[5] = 2;
        be[18..20].copy_from_slice(&40u16.to_be_bytes());
        assert_eq!(
            elf_ident(&be),
            Some(ElfIdent {
                wide: false,
                machine: 40
            })
        );
        assert_eq!(elf_ident(b"#!/bin/sh\nexit 0\n\0\0\0"), None);
        assert_eq!(elf_ident(b"\x7fELF"), None, "too short to carry a machine");
    }

    #[test]
    fn parse_control_folds_continuation_lines() {
        let c = control("arm64");
        assert_eq!(c["Description"], "RGA\nmore text");
        assert_eq!(c["Architecture"], "arm64");
    }

    #[test]
    fn every_rule_id_is_its_serialized_name() {
        for rule in [
            Rule::ControlField,
            Rule::Architecture,
            Rule::ForeignElf,
            Rule::ElfInArchAll,
            Rule::Owner,
            Rule::Setuid,
            Rule::WorldWritable,
            Rule::UsrLocal,
            Rule::MemberPath,
            Rule::Empty,
        ] {
            assert_eq!(serde_json::to_value(rule).unwrap(), rule.id());
        }
    }
}
