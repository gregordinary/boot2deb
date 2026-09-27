//! `reproduce`: rebuild an image from a published plan document rather than from a
//! fresh archive resolve, and judge the rebuild against the original's record.
//!
//! A recipe's `.lock` pins sources, patches, and the builder. It does not pin *which
//! package versions the archive served*, so the same lock a month later resolves a
//! different userland. The plan document a build publishes beside its image pins
//! exactly that, and this command replays it. That document holds every package
//! name, version and sha256, plus the state of each repository they were selected
//! from.
//!
//! It is [`build`](super::build) with one substitution: the rootfs installs the plan
//! it is given instead of resolving one. Every other input, flag and stage is the
//! same, so the two commands share one pipeline.
//!
//! What differs is the trust model, and that is why this is its own command rather
//! than a flag. A pinned install reads neither a release nor a package index. The
//! plan document, rather than an archive signature, is what the package digests
//! chain to. See [`RootfsOptions::pinned_plan`](boot2deb_engine::rootfs::RootfsOptions::pinned_plan).
//!
//! The builder is the third reproducibility axis and is not enforced here. The
//! published provenance manifest records which boot2deb produced the image. This
//! reports how the running checkout compares, and leaves the decision to the
//! operator. A stamped commit is a floor rather than a ceiling, and a newer builder
//! usually reproduces the image and can carry fixes.
//!
//! What *is* enforced is the result. The rebuild writes to a directory of its own, so the
//! originals survive to be compared against. It runs with both caches off, since a
//! restored output is the earlier build's and says nothing about this one.
//!
//! Each output the original's provenance manifest recorded then gets a [`Verdict`] from
//! [`boot2deb_core::outputs::judge`]. An image carries a fresh first-boot password, so it
//! is judged through its file manifest and the disk around it. The disk is the partition
//! table, the filesystem record, and the disk's size. A difference or a missing output
//! exits non-zero.

use crate::args::BuildArgs;
use crate::render::{note, Verbosity};
use boot2deb_core::outputs::Verdict;
use boot2deb_core::provenance::{BuiltWithProvenance, ProvenanceManifest};
use boot2deb_core::ConfigRoot;
use std::path::{Path, PathBuf};

/// Run `reproduce <recipe>`.
pub(crate) fn run(
    root: &ConfigRoot,
    recipe: &str,
    from: Option<PathBuf>,
    with_caches: bool,
    mut args: BuildArgs,
    json: bool,
    verbosity: Verbosity,
) -> Result<(), Box<dyn std::error::Error>> {
    // The same point resolution `build` performs, for the same reason: every published
    // artifact is named for the build point, so a feature variant's plan sits beside a
    // variant-named image and never beside the base recipe's.
    let point = crate::config::build_point(recipe, args.features.clone())?;
    let reference = point.reference();
    let stem = point.artifact_stem();

    // Where the earlier build published. Defaulted to where this build point's own
    // artifacts land, so checking a build made on this machine needs no flag.
    let own_artifacts =
        crate::workdir::work_dir_for(root, reference.as_str(), args.work_dir.clone())
            .join("artifacts");
    let published = crate::fsutil::absolutize(from.unwrap_or_else(|| own_artifacts.clone()));
    // Where the rebuild writes: a directory of its own, so the originals it is judged
    // against are still there afterwards.
    let rebuilt = crate::fsutil::absolutize(
        args.out_dir
            .clone()
            .unwrap_or_else(|| own_artifacts.join("reproduce")),
    );
    if rebuilt == published {
        return Err(format!(
            "the reproduction would write over the build it is judged against ({}) — \
             pass an --out-dir of its own",
            published.display()
        )
        .into());
    }
    args.out_dir = Some(rebuilt.clone());
    // A restored output is the earlier build's. Off unless asked for, so every output
    // judged below is this run's own.
    if !with_caches {
        args.no_artifact_cache = true;
        args.refresh_rootfs = true;
    }

    let plan = published.join(format!("{stem}.plan"));
    if !plan.exists() {
        return Err(format!(
            "no plan document at {} — `reproduce` replays the one a build publishes \
             beside its image. Point --from at the directory holding {stem}.plan (it \
             ships with the image and its provenance manifest).",
            plan.display()
        )
        .into());
    }

    // The build's own event stream does not exist yet — `build::run` owns it — so these
    // two lines go out on the same stdout contract, rendered for a human or as NDJSON,
    // and are then followed by the build's own stream on the same terminal.
    let sink = move |e: boot2deb_engine::event::Event| {
        if json {
            crate::render::print_event_json(&e)
        } else {
            crate::render::print_event_at(verbosity, &e)
        }
    };
    note(
        json,
        verbosity,
        &sink,
        "reproduce",
        format!(
            "replaying {} — the rootfs installs this plan and consults no archive index",
            plan.display()
        ),
    );
    // The builder advisory: what produced the image, against what is running now. Read
    // from the provenance manifest beside the plan when one is there, and skipped
    // quietly when it is not — a plan alone is enough to replay, and refusing for a
    // missing advisory would make the record a requirement it was never meant to be.
    let provenance = published.join(format!("{stem}.provenance.toml"));
    let line = match builder_stamp(&provenance)? {
        Some(stamp) => advice(&stamp, crate::builder::config_stamp(root)),
        None => format!(
            "no provenance manifest at {} — replaying the plan without a builder \
             comparison",
            provenance.display()
        ),
    };
    note(json, verbosity, &sink, "reproduce", line);

    super::build::run(root, recipe, args, Some(&plan), json, verbosity)?;

    // The judgment needs the original's record. A plan alone is enough to replay, and
    // then there is nothing recorded to hold the rebuild to.
    if !provenance.exists() {
        note(
            json,
            verbosity,
            &sink,
            "reproduce",
            "no provenance manifest recorded the original's outputs, so the rebuild is not \
             judged"
                .into(),
        );
        return Ok(());
    }
    let judged = judge(&published, &rebuilt, &stem)?;
    report(&judged, &published, &rebuilt, &stem, json);
    if judged.reproduced() {
        Ok(())
    } else {
        let failed = judged
            .outputs
            .iter()
            .filter(|o| o.verdict.is_failure())
            .count();
        Err(format!(
            "{failed} of {} recorded outputs did not reproduce",
            judged.outputs.len()
        )
        .into())
    }
}

/// Judge the rebuild in `rebuilt` against the original in `published`, from the two
/// provenance manifests and, for the image, the two rootfs file manifests they name and
/// the disks around them.
fn judge(
    published: &Path,
    rebuilt: &Path,
    stem: &str,
) -> Result<boot2deb_core::outputs::Reproduction, Box<dyn std::error::Error>> {
    let read = |dir: &Path| -> Result<ProvenanceManifest, Box<dyn std::error::Error>> {
        let path = dir.join(format!("{stem}.provenance.toml"));
        let text =
            std::fs::read_to_string(&path).map_err(|e| format!("read {}: {e}", path.display()))?;
        Ok(ProvenanceManifest::from_toml_str(
            &text,
            &path.display().to_string(),
        )?)
    };
    let (original, rebuild) = (read(published)?, read(rebuilt)?);
    let images = match (
        crate::artifacts::files_manifest(&original, published)?,
        crate::artifacts::files_manifest(&rebuild, rebuilt)?,
    ) {
        (Some(a), Some(b)) => Some(boot2deb_core::outputs::ImageComparison {
            files: boot2deb_core::files::compare(&a, &b),
            disk: disk_differences(&original, published, &rebuild, rebuilt),
        }),
        _ => None,
    };
    Ok(boot2deb_core::outputs::judge(
        &original.outputs,
        &rebuild.outputs,
        images,
    ))
}

/// What differs between two builds' disks outside the rootfs's files, one sentence each.
///
/// The partition table of each disk image the original recorded, read from both builds'
/// copies. The table carries every partition's placement, type, GUID, name and
/// attributes, and a split rootfs image carries one too. The filesystem record
/// (`[filesystem]`) carries the format contract and the geometry it realized, and the
/// disk's size is compared as recorded.
fn disk_differences(
    original: &ProvenanceManifest,
    published: &Path,
    rebuild: &ProvenanceManifest,
    rebuilt: &Path,
) -> Vec<String> {
    use boot2deb_engine::press::verify;
    let mut disk = Vec::new();
    if original.image.image_bytes != rebuild.image.image_bytes {
        disk.push(format!(
            "the disk is {} bytes, not {}",
            rebuild.image.image_bytes, original.image.image_bytes
        ));
    }
    if original.filesystem != rebuild.filesystem {
        disk.push("the rootfs filesystem record ([filesystem]) differs".into());
    }
    for o in original.outputs.iter().filter(|o| o.is_disk_image()) {
        let theirs = rebuilt.join(&o.file);
        if !theirs.exists() {
            // A missing output has its own verdict.
            continue;
        }
        match (
            verify::planned_table(&published.join(&o.file)),
            verify::planned_table(&theirs),
        ) {
            (Ok(a), Ok(b)) => {
                if let Err(e) = verify::compare_tables(&o.file, &a, &b) {
                    disk.push(format!("the partition table of {} differs: {e}", o.file));
                }
            }
            (Err(_), Err(_)) => {}
            (Ok(_), Err(e)) | (Err(e), Ok(_)) => disk.push(format!(
                "only one build's {} has a readable partition table: {e}",
                o.file
            )),
        }
    }
    disk
}

/// Print the verdicts: one NDJSON `reproduction` record under `--json`, a table
/// otherwise. Printed whatever the result, since an identical output is as much the
/// answer as a different one.
///
/// A verdict names the first few paths an image differs in. When an image differs, the
/// table ends with the `diff` command that lists every one.
fn report(
    judged: &boot2deb_core::outputs::Reproduction,
    published: &Path,
    rebuilt: &Path,
    stem: &str,
    json: bool,
) {
    if json {
        let mut value = serde_json::to_value(judged).unwrap_or_default();
        if let Some(map) = value.as_object_mut() {
            map.insert("event".into(), "reproduction".into());
        }
        println!("{value}");
        return;
    }
    println!("\nreproduction, output by output:");
    for o in &judged.outputs {
        let verdict = match &o.verdict {
            Verdict::Identical => "identical".to_string(),
            Verdict::Differs { detail } => format!("DIFFERS  {detail}"),
            Verdict::Missing => "MISSING  not written by the rebuild".to_string(),
            Verdict::NotComparable { reason } => format!("not comparable: {reason}"),
        };
        println!("  {:<48} {verdict}", o.file);
    }
    if !judged.set_aside.is_empty() {
        println!(
            "  (set aside as different by design: {})",
            judged.set_aside.join(", ")
        );
    }
    for file in &judged.unrecorded {
        println!("  {file:<48} written by the rebuild, recorded by no original");
    }
    let image_differs = judged
        .outputs
        .iter()
        .any(|o| o.role == "files-manifest" && o.verdict.is_failure());
    if image_differs {
        let record = format!("{stem}.provenance.toml");
        println!(
            "\nevery path the images differ in:\n  boot2deb diff {} {} --section files",
            published.join(&record).display(),
            rebuilt.join(&record).display()
        );
    }
}

/// One line comparing the builder that produced the image with the running one, plus
/// the config tree and the provisioning library where each can answer for itself.
///
/// Advisory in both directions. A match is worth stating because it is the case that
/// needs no action; a mismatch names the checkout to step back to without claiming the
/// replay will fail, because a stamp is the commit at which the build *worked* and never
/// the commit past which it breaks — that change is in the future and unknowable at
/// build time.
///
/// `current_config` is the running config tree's stamp, as
/// [`config_stamp`](crate::builder::config_stamp) reads it. The config tree earns its
/// own sentence here rather than a shared one: a replay reads the board's `.dts` and
/// layers from disk, so a moved config tree changes what is rebuilt even when the
/// binary is identical — the failure mode the builder comparison alone would miss.
fn advice(stamp: &BuiltWithProvenance, current_config: Option<(String, bool)>) -> String {
    let mut line = builder_advice(stamp);
    for extra in [
        config_advice(stamp, current_config),
        provisioner_advice(stamp),
    ]
    .into_iter()
    .flatten()
    {
        line.push(' ');
        line.push_str(&extra);
    }
    line
}

/// The provisioning half of [`advice`], or `None` where the library has not moved.
///
/// Silence for an unchanged version, for the same reason [`config_advice`] is silent: a
/// dependency that held still does not need a sentence.
///
/// It matters to a replay because this library bootstraps the rootfs. A change in it can
/// move bytes that the lock and the config tree both held still, which is the one cause
/// the other two halves of the line cannot name.
fn provisioner_advice(stamp: &BuiltWithProvenance) -> Option<String> {
    let running = boot2deb_engine::CAGE_VERSION;
    (stamp.ferroday_cage != running).then(|| {
        format!(
            "The rootfs was provisioned by ferroday-cage {}, and {running} is linked now.",
            stamp.ferroday_cage
        )
    })
}

/// The config-tree half of [`advice`], or `None` where there is nothing to compare —
/// an image built before the field existed, or a replay from a config tree that is not
/// a checkout. Silence beats a sentence that says only that it cannot answer.
fn config_advice(stamp: &BuiltWithProvenance, current: Option<(String, bool)>) -> Option<String> {
    let built = stamp.config_commit.as_deref()?;
    let (running, running_dirty) = current?;
    // The stamped side's own dirty flag disclaims the commit, so say that before
    // comparing to it — "unchanged since" would be a false reassurance about a tree
    // whose commit never described it.
    if stamp.config_dirty {
        return Some(format!(
            "The config tree it was built from had uncommitted changes, so {built} does \
             not describe the layers that went in."
        ));
    }
    if running.starts_with(built) || built.starts_with(running.as_str()) {
        return Some(if running_dirty {
            format!(
                "The config tree is the same commit ({built}) but has uncommitted \
                 changes, so the replay reads layers the image was not built from."
            )
        } else {
            format!("The config tree is unchanged at {built}.")
        });
    }
    Some(format!(
        "The config tree has moved {built} → {running}; the replay resolves layers from \
         the tree on disk, so a board `.dts` or device layer that changed between them \
         changes what is rebuilt."
    ))
}

/// The builder half of [`advice`]: what produced the image, against what is running.
fn builder_advice(stamp: &BuiltWithProvenance) -> String {
    let running_version = env!("CARGO_PKG_VERSION");
    let running_commit = option_env!("BOOT2DEB_GIT_COMMIT").filter(|s| !s.is_empty());
    let built = match (&stamp.commit, stamp.dirty) {
        (Some(commit), true) => format!("{} ({commit}, dirty)", stamp.version),
        (Some(commit), false) => format!("{} ({commit})", stamp.version),
        (None, _) => stamp.version.clone(),
    };
    let running = match running_commit {
        Some(commit) => format!("{running_version} ({commit})"),
        None => running_version.to_string(),
    };
    if stamp.dirty {
        return format!(
            "built with boot2deb {built}; running {running}. The stamped checkout had \
             uncommitted changes, so no commit identifies the builder that produced this \
             image."
        );
    }
    match (&stamp.commit, running_commit) {
        (Some(built_commit), Some(running_commit)) if built_commit == running_commit => {
            format!("built with boot2deb {built}; running the same checkout.")
        }
        (Some(built_commit), _) => format!(
            "built with boot2deb {built}; running {running}. A newer builder usually \
             reproduces the image and may carry fixes — step back with \
             `git checkout {built_commit}` only if it diverges."
        ),
        (None, _) => format!(
            "built with boot2deb {built}; running {running}. The image was built outside \
             a git checkout, so only the version identifies its builder."
        ),
    }
}

/// Read the builder stamp from a provenance manifest, or `None` when there is no
/// manifest to read.
///
/// A manifest that exists and cannot be parsed *is* an error: it names the file the
/// operator pointed at, and silently treating a corrupt record as an absent one would
/// report "no builder comparison" for a document that has one.
fn builder_stamp(path: &Path) -> Result<Option<BuiltWithProvenance>, Box<dyn std::error::Error>> {
    if !path.exists() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok(Some(boot2deb_core::provenance::builder_stamp(
        &text,
        &path.display().to_string(),
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(version: &str, commit: Option<&str>, dirty: bool) -> BuiltWithProvenance {
        BuiltWithProvenance {
            version: version.to_string(),
            commit: commit.map(str::to_string),
            dirty,
            config_commit: None,
            config_dirty: false,
            ferroday_cage: "0.4.4".to_string(),
        }
    }

    /// A provisioner that moved is named, and one that held still says nothing — the
    /// replay's rootfs comes out of this library, so it is the cause the builder and
    /// config comparisons cannot see.
    #[test]
    fn a_provisioner_that_moved_is_named_and_an_unchanged_one_is_silent() {
        let mut moved = stamp("0.1.0", Some("abc1234"), false);
        moved.ferroday_cage = "0.4.3".to_string();
        let line = provisioner_advice(&moved).expect("a moved provisioner is worth saying");
        assert!(line.contains("0.4.3"), "{line}");
        assert!(line.contains(boot2deb_engine::CAGE_VERSION), "{line}");

        let mut held = stamp("0.1.0", Some("abc1234"), false);
        held.ferroday_cage = boot2deb_engine::CAGE_VERSION.to_string();
        assert!(provisioner_advice(&held).is_none());
    }

    /// The three halves join into one line, in order, separated by single spaces. The
    /// shape is what a reader sees, so it is asserted rather than assumed.
    #[test]
    fn the_line_joins_the_builder_config_and_provisioner_halves() {
        let mut s = with_config(stamp("0.1.0", Some("abc1234"), false), "cafe1234", false);
        s.ferroday_cage = "0.4.3".to_string();
        let line = advice(&s, Some(("cafe1234".to_string(), false)));
        assert!(line.starts_with("built with boot2deb"), "{line}");
        assert!(
            line.contains("The config tree is unchanged at cafe1234."),
            "{line}"
        );
        assert!(
            line.contains("provisioned by ferroday-cage 0.4.3"),
            "{line}"
        );
        assert!(!line.contains("  "), "no double spaces: {line}");
    }

    /// A stamp that also names the config tree it resolved from.
    fn with_config(mut s: BuiltWithProvenance, commit: &str, dirty: bool) -> BuiltWithProvenance {
        s.config_commit = Some(commit.to_string());
        s.config_dirty = dirty;
        s
    }

    /// A dirty stamp is the one case where the commit says nothing, so the advice must
    /// not offer it as somewhere to step back to.
    #[test]
    fn a_dirty_stamp_does_not_offer_a_checkout_to_step_back_to() {
        let advice = advice(&stamp("0.1.0", Some("abc1234"), true), None);
        assert!(
            advice.contains("uncommitted changes") && !advice.contains("git checkout"),
            "a dirty stamp must not name a commit to return to: {advice}"
        );
    }

    /// The mismatch case is the one an operator acts on, so it names the commit and
    /// frames it as advice rather than as a requirement.
    #[test]
    fn a_differing_commit_names_it_and_stays_advisory() {
        let advice = advice(&stamp("0.1.0", Some("abc1234"), false), None);
        assert!(
            advice.contains("git checkout abc1234"),
            "the advice must name the stamped commit: {advice}"
        );
        assert!(
            advice.contains("only if it diverges"),
            "the advice must stay advisory: {advice}"
        );
    }

    /// An image built before the config stamp existed, or replayed from a tree that is
    /// not a checkout, must not gain a sentence that only says it cannot answer.
    #[test]
    fn an_unanswerable_config_comparison_is_silent() {
        let s = stamp("0.1.0", Some("abc1234"), false);
        assert!(config_advice(&s, Some(("deadbeef".into(), false))).is_none());
        assert!(config_advice(&with_config(s, "cafe1234", false), None).is_none());
    }

    /// The whole point of the field: a replay reads layers off disk, so a moved config
    /// tree is named even when the builder is identical.
    #[test]
    fn a_moved_config_tree_is_named_with_both_commits() {
        let s = with_config(stamp("0.1.0", Some("abc1234"), false), "cafe1234", false);
        let line = config_advice(&s, Some(("beef5678".into(), false))).expect("a move to report");
        assert!(line.contains("cafe1234"), "{line}");
        assert!(line.contains("beef5678"), "{line}");
    }

    /// Same commit but an edited tree is not "unchanged" — saying so would reassure
    /// about exactly the state that makes a replay diverge.
    #[test]
    fn a_dirty_config_tree_at_the_same_commit_is_not_reported_as_unchanged() {
        let s = with_config(stamp("0.1.0", Some("abc1234"), false), "cafe1234", false);
        let line = config_advice(&s, Some(("cafe1234".into(), true))).expect("a state to report");
        assert!(line.contains("uncommitted changes"), "{line}");
        assert!(!line.contains("unchanged"), "{line}");
    }

    /// A stamp whose own config tree was dirty disclaims its commit, so the comparison
    /// must not present that commit as a baseline the tree still matches.
    #[test]
    fn a_stamp_with_a_dirty_config_tree_disclaims_its_commit() {
        let s = with_config(stamp("0.1.0", Some("abc1234"), false), "cafe1234", true);
        let line = config_advice(&s, Some(("cafe1234".into(), false))).expect("a caveat to report");
        assert!(line.contains("does not describe"), "{line}");
        assert!(!line.contains("unchanged"), "{line}");
    }

    /// A manifest that is present but unreadable is an error rather than a missing
    /// advisory — the difference between "there is nothing to compare" and "the record
    /// is broken" is exactly what an operator reproducing an image needs told.
    #[test]
    fn a_corrupt_manifest_is_an_error_not_an_absent_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.provenance.toml");
        std::fs::write(&path, "this is not toml\x00").unwrap();
        assert!(builder_stamp(&path).is_err());
        assert!(builder_stamp(&dir.path().join("absent.toml"))
            .unwrap()
            .is_none());
    }

    /// The stamp is read out of a whole provenance manifest, and the manifest carries a
    /// banner of TOML comments plus sections this struct does not name — including the
    /// first-boot credential, which must not have to be parsed to read the builder.
    #[test]
    fn the_stamp_is_read_from_a_full_manifest_without_its_other_sections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.provenance.toml");
        std::fs::write(
            &path,
            "# boot2deb provenance manifest\n\
             [image]\n\
             device = \"turing-rk1\"\n\
             \n\
             [credentials]\n\
             password = \"secret\"\n\
             \n\
             [built_with]\n\
             version = \"0.1.0\"\n\
             commit = \"abc1234\"\n\
             dirty = false\n\
             ferroday_cage = \"0.4.4\"\n",
        )
        .unwrap();
        let stamp = builder_stamp(&path).unwrap().expect("the stamp is present");
        assert_eq!(stamp.version, "0.1.0");
        assert_eq!(stamp.commit.as_deref(), Some("abc1234"));
    }
}
