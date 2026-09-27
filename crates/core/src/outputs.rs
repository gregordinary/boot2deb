//! Build outputs — the artifacts a build emitted, as its provenance manifest records
//! them, and the comparison `reproduce` makes between two builds' records.
//!
//! Pure: the record type and the verdicts. Hashing the files is the build's work, and
//! reading the two records and the two images' file manifests is the command's.
//!
//! A reproduction is judged output by output against the original build's record:
//!
//! - An output whose bytes are a function of the lock compares by sha256.
//! - An output that carries the per-image first-boot password
//!   ([`per_image`](Output::per_image)) can never match byte for byte. It is compared
//!   through the two images instead ([`ImageComparison`]). Their file manifests
//!   ([`crate::files`]) must agree outside the password's content
//!   ([`PER_IMAGE_PATHS`](crate::files::PER_IMAGE_PATHS)). The disk around the rootfs
//!   must agree too.
//!
//! [`CommittedOutputs`] is the same comparison held to a record committed beside a lock,
//! `recipes/<recipe>.outputs`. It names the bytes a validated pin was flashed from, so a
//! later build of that lock can prove it made the same image.

use crate::files::FilesDiff;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// One artifact a build emitted, as the provenance manifest's `[[outputs]]` records it.
///
/// The rows come from the build's own artifact events, so the record lists exactly what
/// the build wrote. The provenance manifest and the bills of materials describe the
/// outputs and are not rows. A run of some stages alone records that run's outputs, and
/// no earlier run's.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Output {
    /// The build step that emitted it (`kernel`, `uboot`, `rootfs`, `image`).
    pub step: String,
    /// What it is within that step (`image_deb`, `tar`, `compressed`).
    pub role: String,
    /// Its file name in the artifact directory. The key a reproduction's output is
    /// matched by.
    pub file: String,
    /// Its size in bytes.
    pub size: u64,
    /// The lowercase-hex sha256 of its bytes.
    pub sha256: String,
    /// Whether it carries the per-image first-boot password: an image holding the rootfs
    /// partition, a compressed copy of one, or the rootfs file manifest. Written even
    /// when false, so a reader never has to decide what an absent key meant.
    pub per_image: bool,
}

impl Output {
    /// Whether this is a disk image or a container of one: the whole-disk image, a split
    /// layout's boot or rootfs image, or a compressed copy.
    ///
    /// Which of these a build writes follows its flags (`--keep-raw`, `--compress`,
    /// `--layout`) rather than its lock, so a [`CommittedOutputs`] record leaves them out.
    /// What they hold is recorded elsewhere: the rootfs tar and the bootloader payloads.
    pub fn is_disk_image(&self) -> bool {
        self.step == "image"
            && matches!(
                self.role.as_str(),
                "image" | "boot_img" | "rootfs_img" | "compressed"
            )
    }
}

/// Two builds' images compared, which is how the outputs that carry the per-image
/// password are judged.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ImageComparison {
    /// The two rootfs file manifests, compared path by path.
    pub files: FilesDiff,
    /// What differs in the disk around the rootfs, one sentence each: the partition
    /// table, the filesystem record, the disk's size. Empty when the two agree there.
    pub disk: Vec<String>,
}

/// What one output of the original build came to in the reproduction.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "verdict", rename_all = "kebab-case")]
pub enum Verdict {
    /// The reproduction wrote the same bytes. For a [`per_image`](Output::per_image)
    /// output, it wrote an image whose disk agrees, and whose files agree everywhere but
    /// in the password's content.
    Identical,
    /// The reproduction wrote something different.
    Differs {
        /// What differs, in a sentence.
        detail: String,
    },
    /// The reproduction did not write this output at all.
    Missing,
    /// No verdict is possible from the two records, and why.
    NotComparable {
        /// Why the output cannot be judged.
        reason: String,
    },
}

impl Verdict {
    /// Whether this verdict says the reproduction failed: a difference, or an output it
    /// never wrote.
    pub fn is_failure(&self) -> bool {
        matches!(self, Verdict::Differs { .. } | Verdict::Missing)
    }
}

/// One output and its verdict.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct OutputVerdict {
    /// The output's file name.
    pub file: String,
    /// The step that emitted it in the original build.
    pub step: String,
    /// Its role in that step.
    pub role: String,
    /// What the reproduction came to.
    #[serde(flatten)]
    pub verdict: Verdict,
}

/// A reproduction judged against the original build, output by output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Reproduction {
    /// One verdict per output the original recorded, in the original's order.
    pub outputs: Vec<OutputVerdict>,
    /// Outputs the reproduction wrote that the original did not record. They are not a
    /// failure, since a record is judged on what it claims, but they are named so an
    /// added artifact is not silent.
    pub unrecorded: Vec<String>,
    /// The per-image paths whose difference the image comparison set aside, when it ran.
    pub set_aside: Vec<String>,
}

impl Reproduction {
    /// Whether every recorded output reproduced: nothing differs and nothing is missing.
    pub fn reproduced(&self) -> bool {
        !self.outputs.iter().any(|o| o.verdict.is_failure())
    }
}

/// Judge a reproduction's outputs against the original's.
///
/// `images` compares the two builds' images, when both recorded a file manifest. It
/// decides every [`per_image`](Output::per_image) output. The output is identical when
/// the disks agree and the files agree outside the per-image content
/// ([`FilesDiff::without_per_image`]), and different otherwise. Without it those outputs
/// are not comparable.
pub fn judge(
    original: &[Output],
    reproduced: &[Output],
    images: Option<ImageComparison>,
) -> Reproduction {
    let images = images.map(|c| {
        let (files, set_aside) = c.files.without_per_image();
        (files, c.disk, set_aside)
    });
    let set_aside = images
        .as_ref()
        .map(|(_, _, s)| s.clone())
        .unwrap_or_default();
    let outputs = original
        .iter()
        .map(|o| OutputVerdict {
            file: o.file.clone(),
            step: o.step.clone(),
            role: o.role.clone(),
            verdict: match reproduced.iter().find(|r| r.file == o.file) {
                None => Verdict::Missing,
                Some(_) if o.per_image => match &images {
                    Some((rest, disk, _)) if rest.is_empty() && disk.is_empty() => {
                        Verdict::Identical
                    }
                    Some((rest, disk, _)) => Verdict::Differs {
                        detail: disk
                            .iter()
                            .cloned()
                            .chain((!rest.is_empty()).then(|| files_detail(rest)))
                            .collect::<Vec<_>>()
                            .join(". "),
                    },
                    None => Verdict::NotComparable {
                        reason: "it carries the per-image password, and one of the two \
                                 builds recorded no file manifest to compare it through"
                            .into(),
                    },
                },
                Some(r) if r.sha256 == o.sha256 => Verdict::Identical,
                Some(r) => Verdict::Differs {
                    detail: format!(
                        "sha256 {} -> {}, {} -> {} bytes",
                        short(&o.sha256),
                        short(&r.sha256),
                        o.size,
                        r.size
                    ),
                },
            },
        })
        .collect();
    let unrecorded = reproduced
        .iter()
        .filter(|r| !original.iter().any(|o| o.file == r.file))
        .map(|r| r.file.clone())
        .collect();
    Reproduction {
        outputs,
        unrecorded,
        set_aside,
    }
}

/// The banner a committed outputs record opens with. A TOML comment, so the committed
/// text parses as it stands.
const OUTPUTS_BANNER: &str = "\
# Generated by `boot2deb build --save-outputs`; do not hand-edit.
# The bytes a build of this lock wrote, per build-host architecture. `verify-image`
# holds a later build to them. Outputs that carry the per-image password are not here.
";

/// The committed record of what a build of one lock wrote: `recipes/<recipe>.outputs`,
/// beside the lock, which `build --save-outputs` writes and `verify-image` checks.
///
/// Keyed by build-host architecture, as the provenance manifest's `host_arch` names it.
/// A cross build and a native build use different compilers, so a record from one host
/// says nothing about the other's bytes. Recording one host leaves the others' entries
/// as they were.
///
/// Only outputs that are a function of the lock are recorded. A disk image and its
/// containers are not ([`Output::is_disk_image`]). Which of them a build writes follows
/// its flags, and the image carries the per-image password, which never matches.
/// Neither is the file manifest, which lists that password's file. The rootfs tar stands
/// in for the image, since the account in it is still locked. The image is that tar
/// through a deterministic formatter plus the password splice.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommittedOutputs {
    /// Each build-host architecture's outputs, sorted by file name.
    #[serde(default)]
    pub hosts: BTreeMap<String, Vec<CommittedOutput>>,
}

/// One output in a [`CommittedOutputs`] record: an [`Output`] without the per-image flag,
/// which is false for every output the record holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CommittedOutput {
    /// The build step that emitted it.
    pub step: String,
    /// What it is within that step.
    pub role: String,
    /// Its file name in the artifact directory.
    pub file: String,
    /// Its size in bytes.
    pub size: u64,
    /// The lowercase-hex sha256 of its bytes.
    pub sha256: String,
}

impl CommittedOutputs {
    /// Parse a committed record.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Parse`](crate::ConfigError::Parse) when the text is not a record.
    /// `path` names the file in that error and is not read from.
    pub fn from_toml_str(text: &str, path: &str) -> Result<Self, crate::ConfigError> {
        toml::from_str(text).map_err(|source| crate::ConfigError::Parse {
            path: path.to_string(),
            source,
        })
    }

    /// Serialize to the canonical committed form: the banner, then the TOML body.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Serialize`](crate::ConfigError::Serialize) when the body cannot be
    /// written as TOML.
    pub fn to_toml_string(&self) -> Result<String, crate::ConfigError> {
        let body = toml::to_string(self).map_err(|source| crate::ConfigError::Serialize {
            what: "committed outputs",
            source,
        })?;
        Ok(format!("{OUTPUTS_BANNER}{body}"))
    }

    /// Record a build's outputs as `host_arch`'s entry, replacing any earlier one.
    ///
    /// The per-image outputs and the disk images are left out, and the rest are sorted by
    /// file name.
    pub fn record(&mut self, host_arch: &str, outputs: &[Output]) {
        let mut rows: Vec<CommittedOutput> = outputs
            .iter()
            .filter(|o| !o.per_image && !o.is_disk_image())
            .map(|o| CommittedOutput {
                step: o.step.clone(),
                role: o.role.clone(),
                file: o.file.clone(),
                size: o.size,
                sha256: o.sha256.clone(),
            })
            .collect();
        rows.sort_by(|a, b| a.file.cmp(&b.file));
        self.hosts.insert(host_arch.to_string(), rows);
    }

    /// Hold a build's outputs to `host_arch`'s entry, output by output, as [`judge`]
    /// holds a reproduction to its original.
    ///
    /// `None` when the record has no entry for `host_arch`: a record from one host says
    /// nothing about another's bytes. The build's per-image outputs and disk images are
    /// left out of the comparison, as they are left out of the record.
    pub fn check(&self, host_arch: &str, outputs: &[Output]) -> Option<Reproduction> {
        let committed: Vec<Output> = self
            .hosts
            .get(host_arch)?
            .iter()
            .map(|c| Output {
                step: c.step.clone(),
                role: c.role.clone(),
                file: c.file.clone(),
                size: c.size,
                sha256: c.sha256.clone(),
                per_image: false,
            })
            .collect();
        let built: Vec<Output> = outputs
            .iter()
            .filter(|o| !o.per_image && !o.is_disk_image())
            .cloned()
            .collect();
        Some(judge(&committed, &built, None))
    }
}

/// The first twelve hex digits of a digest, the width a report shows. A value that is
/// not hex, from a hand-edited record, is shown whole.
fn short(hash: &str) -> &str {
    hash.get(..12).unwrap_or(hash)
}

/// A sentence naming how many paths of an image differ, with the first few. A path added
/// or removed is named before one that changed, since it says more about what moved.
fn files_detail(diff: &FilesDiff) -> String {
    const SHOWN: usize = 3;
    let mut paths: Vec<String> = diff.added.iter().map(|p| format!("{p} (added)")).collect();
    paths.extend(diff.removed.iter().map(|p| format!("{p} (removed)")));
    paths.extend(
        diff.changed
            .iter()
            .map(|c| format!("{} ({})", c.path, c.fields.join(", "))),
    );
    let total = paths.len();
    let mut detail = format!(
        "{total} path(s) differ outside the per-image password: {}",
        paths[..total.min(SHOWN)].join("; ")
    );
    if total > SHOWN {
        detail.push_str(&format!("; and {} more", total - SHOWN));
    }
    detail
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::ChangedFile;

    fn out(file: &str, sha: char, per_image: bool) -> Output {
        Output {
            step: "kernel".into(),
            role: "image_deb".into(),
            file: file.into(),
            size: 100,
            sha256: sha.to_string().repeat(64),
            per_image,
        }
    }

    /// Each verdict for a byte-compared output: the same digest, a different one, and an
    /// output the reproduction never wrote. Only the last two fail.
    #[test]
    fn a_byte_compared_output_gets_each_verdict() {
        let original = vec![
            out("a.deb", 'a', false),
            out("b.deb", 'b', false),
            out("c.deb", 'c', false),
        ];
        let reproduced = vec![out("a.deb", 'a', false), out("b.deb", 'f', false)];
        let r = judge(&original, &reproduced, None);
        assert_eq!(r.outputs[0].verdict, Verdict::Identical);
        assert!(
            matches!(&r.outputs[1].verdict, Verdict::Differs { detail } if detail.contains("bbbbbbbbbbbb -> ffffffffffff"))
        );
        assert_eq!(r.outputs[2].verdict, Verdict::Missing);
        assert!(!r.reproduced());
        assert!(judge(&original[..1], &reproduced, None).reproduced());
    }

    /// An image never matches byte for byte, so without both manifests it is not
    /// comparable, which is not a failure.
    #[test]
    fn an_image_without_manifests_is_not_comparable() {
        let original = vec![out("x.img.xz", '1', true)];
        let reproduced = vec![out("x.img.xz", '2', true)];
        let r = judge(&original, &reproduced, None);
        assert!(matches!(
            r.outputs[0].verdict,
            Verdict::NotComparable { .. }
        ));
        assert!(r.reproduced());
    }

    /// Through the manifests, an image that differs only in the per-image password is
    /// identical, and one that differs anywhere else fails, naming the path.
    #[test]
    fn an_image_is_judged_through_its_file_manifest() {
        let original = vec![out("x.img.xz", '1', true)];
        let reproduced = vec![out("x.img.xz", '2', true)];
        let shadow_only = FilesDiff {
            changed: vec![ChangedFile {
                path: "/etc/shadow".into(),
                fields: vec!["size", "sha256"],
            }],
            ..FilesDiff::default()
        };
        let images = |files: FilesDiff, disk: Vec<String>| Some(ImageComparison { files, disk });
        let r = judge(&original, &reproduced, images(shadow_only.clone(), vec![]));
        assert_eq!(r.outputs[0].verdict, Verdict::Identical);
        assert_eq!(r.set_aside, ["/etc/shadow"]);

        let mut elsewhere = shadow_only.clone();
        elsewhere.changed.push(ChangedFile {
            path: "/var/log/dpkg.log".into(),
            fields: vec!["sha256"],
        });
        let r = judge(&original, &reproduced, images(elsewhere, vec![]));
        assert!(
            matches!(&r.outputs[0].verdict, Verdict::Differs { detail } if detail.contains("/var/log/dpkg.log (sha256)"))
        );
        assert!(!r.reproduced());

        // Files that agree do not excuse a disk that does not.
        let moved = vec!["the partition table of x.img.xz differs".to_string()];
        let r = judge(&original, &reproduced, images(shadow_only, moved));
        assert!(
            matches!(&r.outputs[0].verdict, Verdict::Differs { detail } if detail.contains("partition table"))
        );
    }

    /// An output only the reproduction wrote is named, and does not fail the comparison.
    #[test]
    fn an_unrecorded_output_is_named_and_not_a_failure() {
        let r = judge(
            &[out("a.deb", 'a', false)],
            &[out("a.deb", 'a', false), out("new.deb", 'n', false)],
            None,
        );
        assert_eq!(r.unrecorded, ["new.deb"]);
        assert!(r.reproduced());
    }

    /// Recording keeps the byte-compared outputs sorted and drops the per-image ones, and
    /// checking a build against the record names the output that moved.
    #[test]
    fn a_committed_record_holds_a_later_build_to_its_bytes() {
        let mut boot_img = out("board-boot.img", 'b', false);
        boot_img.step = "image".into();
        boot_img.role = "boot_img".into();
        let build = vec![
            out("rootfs.tar", 'r', false),
            out("image.img.xz", 'i', true),
            out("kernel.deb", 'k', false),
            boot_img,
        ];
        let mut record = CommittedOutputs::default();
        record.record("x86_64", &build);
        let files: Vec<&str> = record.hosts["x86_64"]
            .iter()
            .map(|c| c.file.as_str())
            .collect();
        assert_eq!(files, ["kernel.deb", "rootfs.tar"]);

        // The same build passes, with a fresh image that differs as every image does.
        let mut again = build.clone();
        again[1].sha256 = "2".repeat(64);
        assert!(record.check("x86_64", &again).unwrap().reproduced());

        // A moved kernel is named, and a host the record never saw is not judged.
        again[2].sha256 = "f".repeat(64);
        let r = record.check("x86_64", &again).unwrap();
        assert!(!r.reproduced());
        let moved: Vec<&str> = r
            .outputs
            .iter()
            .filter(|o| o.verdict.is_failure())
            .map(|o| o.file.as_str())
            .collect();
        assert_eq!(moved, ["kernel.deb"]);
        assert!(record.check("aarch64", &again).is_none());
        assert_eq!(short("é"), "é");
    }

    /// The committed text opens with its banner, round-trips, and recording a second host
    /// keeps the first host's entry.
    #[test]
    fn a_committed_record_round_trips_and_keeps_other_hosts() {
        let mut record = CommittedOutputs::default();
        record.record("x86_64", &[out("kernel.deb", 'k', false)]);
        record.record("aarch64", &[out("kernel.deb", 'a', false)]);
        let text = record.to_toml_string().unwrap();
        assert!(text.starts_with("# Generated by `boot2deb build --save-outputs`"));
        assert!(text.contains("[[hosts.x86_64]]"), "{text}");
        let back = CommittedOutputs::from_toml_str(&text, "x.outputs").unwrap();
        assert_eq!(back, record);
        assert!(CommittedOutputs::from_toml_str("[hosts]\nx = 1\n", "x.outputs").is_err());
    }

    /// The record round-trips through TOML with every key written, `per_image` included.
    #[test]
    fn an_output_row_round_trips_with_every_key() {
        let row = out("a.deb", 'a', false);
        let text = toml::to_string(&row).unwrap();
        assert!(text.contains("per_image = false"), "{text}");
        assert_eq!(toml::from_str::<Output>(&text).unwrap(), row);
    }
}
