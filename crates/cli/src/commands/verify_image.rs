//! `verify-image`: hold a finished image artifact to the invariants that are checkable
//! without a board.
//!
//! This is the off-board half of the hardware gate. It takes several points and gives
//! one exit status for them all, the form a gate pass checks its builds in.
//!
//! First it reads the point's build record ([`boot2deb_core::buildrecord`]). The
//! artifact directory holds whatever the last build to finish a stage left there. Only
//! the record says whether the newest build of the point was a whole one that
//! succeeded. When it was not (a failed or unfinished run, or a later `--stage kernel`)
//! that is the point's whole report, since every other check would be judging files the
//! newest build did not write. Then it asks:
//!
//!  1. The artifact set is there — the image, its provenance manifest, and the plan
//!     document the rootfs published.
//!  2. The plan parses, and its digest is the one the provenance records. A mismatch
//!     means the manifest describes a document other than the one that shipped.
//!  3. `[[archives]]` is well formed. It carries at least the mirror and the build's
//!     own pool. The pool is marked `local` with no mirror URL, since a per-run path
//!     is not portable provenance. `signed_by` is written on every row, because an
//!     empty `signed_by` is a *fact* (trusted unsigned) and must be present rather
//!     than absent.
//!  4. **The ext4 filesystem is exactly its GPT partition.** Larger and it will not
//!     mount at all. Smaller and the difference is wasted. This is the invariant the
//!     fit ordering exists to preserve, so it is checked on every image and not only on
//!     the fitted one.
//!  5. **The rootfs GPT entry is marked bootable.** U-Boot's `bootflow scan` narrows
//!     to the partitions carrying the legacy-BIOS-bootable attribute, as soon as any
//!     partition on the medium carries it. It scans partition 1 alone when none
//!     does, and partition 1 is the seed. Nothing about the filesystem's contents
//!     says whether the bootloader will ever open it.
//!  6. A fitted `image_size` left the slack it asked for.
//!  7. **The rootfs holds exactly the files its manifest lists.** The rootfs partition is
//!     walked where it sits and compared with the `<stem>.rootfs.uapi16` the build
//!     wrote from the same filesystem, every field of every path. A compressed image
//!     is decompressed into a sparse temporary file for this, since the walk seeks.
//!  8. Where the recipe has a committed outputs record (`recipes/<recipe>.outputs`),
//!     **the artifact directory holds every file it names for this build host, byte for
//!     byte.** The files are hashed where they sit, so the check holds what would
//!     ship, whichever run wrote it. A record with no entry for this host passes, saying
//!     so, since another host's compilers make other bytes.
//!  9. Where the build compiles anything, **every `.deb` it recorded as
//!     local-repository input holds to the package rules**
//!     ([`boot2deb_core::debcheck`]). That is the check that admitted each package,
//!     run again on the bytes in the artifact directory.
//!
//! Every structure is read by the code that writes it.
//! [`image::inspect`](boot2deb_engine::image::inspect) reads the GPT and the
//! superblock, and [`ProvenanceManifest`] reads the record. The gate therefore
//! cannot drift from the build by parsing the same bytes differently. The
//! alternative is a second implementation of both parsers that nothing tests.
//!
//! It needs no root, and changes nothing it verifies. Checks 1 to 6 decompress only the
//! head of the artifact. The seventh walks the whole rootfs, and a walk seeks. A
//! compressed image is therefore decompressed first, into a sparse temporary file in the
//! artifact directory that is removed when the check ends. The eighth hashes the recorded
//! files, and the ninth reads each recorded package's members.

use boot2deb_core::buildrecord::BuildRecord;
use boot2deb_core::model::Overrides;
use boot2deb_core::provenance::ProvenanceManifest;
use boot2deb_core::{resolve_recipe, ConfigRoot};
use serde_json::json;
use std::path::{Path, PathBuf};

/// One checked invariant and what it came to.
struct Check {
    /// Short label, for the human table and the JSON key.
    what: &'static str,
    /// What was found — printed either way, because a passing check's *value* is what
    /// makes the report worth reading.
    detail: String,
    /// Whether the invariant holds.
    ok: bool,
}

/// What verifying one point came to: the image read, and every invariant checked.
struct PointReport {
    /// The point's reference.
    point: String,
    /// The image artifact read, once one was found.
    artifact: Option<PathBuf>,
    /// The invariants, in the order they were checked.
    checks: Vec<Check>,
}

impl PointReport {
    /// How many invariants failed.
    fn failed(&self) -> usize {
        self.checks.iter().filter(|c| !c.ok).count()
    }
}

/// Run `verify-image <recipe>...`.
///
/// Verifies every point, then exits non-zero when any invariant of any point failed,
/// so a CI job or the gate script branches on one status rather than on the text. A
/// point that cannot be verified at all (no image, no provenance) is a failure of that
/// point, reported in its place, and the others are still verified.
pub(crate) fn run(
    root: &ConfigRoot,
    points: &[String],
    out_dir: Option<PathBuf>,
    json_out: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    if out_dir.is_some() && points.len() > 1 {
        return Err(format!(
            "--out-dir names one point's artifact directory, and {} points were given",
            points.len()
        )
        .into());
    }
    let mut reports = Vec::with_capacity(points.len());
    for point in points {
        reports.push(match verify_point(root, point, out_dir.clone()) {
            Ok(report) => report,
            Err(e) => PointReport {
                point: point.clone(),
                artifact: None,
                checks: vec![Check {
                    what: "artifacts",
                    detail: e.to_string(),
                    ok: false,
                }],
            },
        });
    }
    let failed_points = reports.iter().filter(|r| r.failed() > 0).count();
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "points": reports.iter().map(|r| json!({
                    "recipe": r.point,
                    "artifact": r.artifact.as_ref().map(|a| a.display().to_string()),
                    "checks": r.checks.iter().map(|c| json!({
                        "what": c.what, "detail": c.detail, "ok": c.ok,
                    })).collect::<Vec<_>>(),
                    "failed": r.failed(),
                    "result": if r.failed() == 0 { "pass" } else { "fail" },
                })).collect::<Vec<_>>(),
                "failed": failed_points,
                "result": if failed_points == 0 { "pass" } else { "fail" },
            }))?
        );
    } else {
        for (i, r) in reports.iter().enumerate() {
            if i > 0 {
                println!();
            }
            match &r.artifact {
                Some(image) => println!("{}  {}", r.point, image.display()),
                None => println!("{}", r.point),
            }
            for c in &r.checks {
                println!(
                    "  {:<15} {}",
                    if c.ok { c.what } else { "FAIL" },
                    if c.ok {
                        c.detail.clone()
                    } else {
                        format!("{}: {}", c.what, c.detail)
                    }
                );
            }
        }
    }
    match (failed_points, reports.as_slice()) {
        (0, _) => Ok(()),
        (_, [only]) => Err(format!(
            "{} of {} image invariants failed",
            only.failed(),
            only.checks.len()
        )
        .into()),
        (n, all) => Err(format!("{n} of {} points failed verification", all.len()).into()),
    }
}

/// Verify one point's artifact directory.
///
/// The build record is read first. When the newest build of the point does not vouch
/// for the directory, that is the whole report: the other checks would be judging
/// files the newest build did not write.
fn verify_point(
    root: &ConfigRoot,
    recipe: &str,
    out_dir: Option<PathBuf>,
) -> Result<PointReport, Box<dyn std::error::Error>> {
    let point = crate::config::build_point(recipe, Vec::new())?;
    let reference = point.reference();
    let stem = point.artifact_stem();
    let build = resolve_recipe(root, reference.as_str(), &Overrides::default())?;
    if !build.produces_image() {
        return Err(format!(
            "recipe '{recipe}' builds only a bootloader, so there is no image to verify"
        )
        .into());
    }
    let dir = match out_dir {
        Some(d) => d,
        None => crate::workdir::work_dir_for(root, reference.as_str(), None).join("artifacts"),
    };

    // 0. The newest build of this point wrote this directory, whole.
    let record_check = build_record_check(&dir, &stem);
    if !record_check.ok {
        return Ok(PointReport {
            point: reference.as_str().to_string(),
            artifact: None,
            checks: vec![record_check],
        });
    }

    let mut checks = vec![record_check];
    let image = find_image(&dir, &stem)?;
    let prov_path = dir.join(format!("{stem}.provenance.toml"));
    let plan_path = dir.join(format!("{stem}.plan"));
    for p in [&prov_path, &plan_path] {
        if !p.is_file() {
            return Err(format!("missing {} — run `boot2deb build {recipe}`", p.display()).into());
        }
    }
    let prov: ProvenanceManifest = boot2deb_core::provenance::parse_manifest(
        &std::fs::read_to_string(&prov_path)?,
        &prov_path.display().to_string(),
    )?;

    // 1. The plan parses, and it is the document the provenance describes.
    let record = boot2deb_engine::rootfs::read_plan_record(&plan_path)?;
    let weights = boot2deb_engine::rootfs::read_plan_weights(&plan_path)?;
    checks.push(Check {
        what: "plan",
        detail: format!(
            "{} packages from {} archive(s)",
            weights.packages.len(),
            record.archives.len()
        ),
        ok: !weights.packages.is_empty(),
    });
    checks.push(Check {
        what: "plan sha256",
        detail: if record.sha256 == prov.rootfs.plan_sha256 {
            "matches the provenance record".into()
        } else {
            format!("{} != recorded {}", record.sha256, prov.rootfs.plan_sha256)
        },
        ok: record.sha256 == prov.rootfs.plan_sha256,
    });

    // 2. The archive rows.
    let pool = prov.archives.iter().find(|a| a.local);
    checks.push(Check {
        what: "[[archives]]",
        detail: format!("{} rows", prov.archives.len()),
        ok: prov.archives.len() >= 2,
    });
    checks.push(Check {
        what: "pool row",
        detail: match pool {
            None => "no `local = true` row".into(),
            Some(p) if p.mirror.is_some() => {
                "the pool carries a mirror URL — a build-host path is not portable \
                 provenance"
                    .into()
            }
            Some(_) => "local = true, no mirror URL".into(),
        },
        ok: pool.is_some_and(|p| p.mirror.is_none()),
    });

    // 3. The filesystem is exactly its GPT partition.
    let geom = &prov.filesystem.geometry;
    let fs_bytes = geom.total_blocks * geom.block_size as u64;
    let part = boot2deb_engine::image::inspect::rootfs_partition(&image);
    checks.push(Check {
        what: "ext4 fits",
        detail: match &part {
            Err(e) => format!("could not read the rootfs partition: {e}"),
            Ok(p) if p.bytes == fs_bytes => format!(
                "{} x {} = {fs_bytes} bytes — the filesystem fills the partition exactly",
                geom.total_blocks, geom.block_size
            ),
            // Larger and the difference is wasted; smaller and the filesystem does not
            // mount at all ("block count exceeds size of device").
            Ok(p) => format!(
                "the rootfs partition is {} bytes but the filesystem is {fs_bytes}",
                p.bytes
            ),
        },
        ok: part.as_ref().is_ok_and(|p| p.bytes == fs_bytes),
    });

    // 3b. The rootfs is the partition a bootloader will look inside. U-Boot's
    //     `bootflow scan` considers only partitions marked bootable once any is
    //     marked, and only partition 1 when none is — and partition 1 is the seed.
    //     Nothing about the filesystem's contents says whether the bootloader will
    //     ever open it, so the attribute is its own check.
    checks.push(Check {
        what: "rootfs bootable",
        detail: match &part {
            Err(e) => format!("could not read the rootfs partition: {e}"),
            Ok(p) if p.bootable => "the GPT entry carries the bootable attribute".into(),
            Ok(_) => "the rootfs GPT entry is not marked bootable — a scanning \
                      bootloader will never open it"
                .into(),
        },
        ok: part.as_ref().is_ok_and(|p| p.bootable),
    });

    // 4. The size, as authored and as realized. They differ in kind for a fitted image:
    //    the recipe names a rule, and only the record says what it came to.
    checks.push(Check {
        what: "size",
        detail: format!(
            "{} -> {} bytes on disk",
            prov.image.image_size, prov.image.image_bytes
        ),
        ok: prov.image.image_bytes > 0,
    });

    // 5. A fitted size additionally has to have left the slack it asked for. The
    //    formatter measures that as free blocks once the source is written and computes
    //    the requirement by integer division, so the comparison truncates the same way —
    //    a filesystem sitting exactly on the floor is a pass, and comparing the
    //    untruncated ratio instead would report it as a failure by less than one block.
    if let boot2deb_core::size::ImageSize::Fit(slack) =
        boot2deb_core::size::parse_image_size(&prov.image.image_size)?
    {
        let free = boot2deb_engine::image::inspect::rootfs_free_blocks(&image);
        let required = match slack {
            boot2deb_core::size::Slack::Share(hundredths) => {
                geom.total_blocks * hundredths as u64 / 10_000
            }
            // A byte floor rounds *up* to whole blocks: the formatter cannot leave a
            // fraction of one free, so anything less than a full block short is short.
            boot2deb_core::size::Slack::Bytes(bytes) => bytes.div_ceil(geom.block_size as u64),
        };
        checks.push(Check {
            what: "slack",
            detail: match &free {
                Err(e) => format!("could not read the free-block count: {e}"),
                Ok(f) if *f >= required => format!(
                    "{f} free of {} blocks; the floor is {required} — honoured",
                    geom.total_blocks
                ),
                Ok(f) => format!(
                    "{f} free blocks, under the {required} that {} requires",
                    prov.image.image_size
                ),
            },
            ok: free.is_ok_and(|f| f >= required),
        });
    }

    // 6. The files the rootfs holds are the files its manifest lists, down to every field.
    //    The manifest was written from this filesystem before it was placed, so any
    //    difference is a defect in the placement or the record, and nothing is set aside.
    checks.push(match crate::artifacts::files_manifest(&prov, &dir) {
        Err(e) => Check {
            what: "rootfs files",
            detail: format!("could not read the file manifest: {e}"),
            ok: false,
        },
        Ok(None) => Check {
            what: "rootfs files",
            detail: "the provenance records no file manifest".into(),
            ok: false,
        },
        Ok(Some(listed)) => match boot2deb_engine::image::files::manifest_of_image(&image, &dir) {
            Err(e) => Check {
                what: "rootfs files",
                detail: format!("could not walk the rootfs partition: {e}"),
                ok: false,
            },
            Ok(found) => {
                let diff = boot2deb_core::files::compare(&listed, &found);
                Check {
                    what: "rootfs files",
                    detail: if diff.is_empty() {
                        format!(
                            "{} paths, each as the manifest lists it",
                            found.entries.len() + 1
                        )
                    } else {
                        format!(
                            "{} changed, {} added, {} removed against the manifest{}",
                            diff.changed.len(),
                            diff.added.len(),
                            diff.removed.len(),
                            diff.changed
                                .first()
                                .map(|c| format!(", first {} ({})", c.path, c.fields.join(", ")))
                                .unwrap_or_default()
                        )
                    },
                    ok: diff.is_empty(),
                }
            }
        },
    });

    // 7. The bytes a build of this lock wrote before, where the recipe records them.
    let committed_path = root.outputs_path(reference.as_str())?;
    if committed_path.is_file() {
        let committed = boot2deb_core::outputs::CommittedOutputs::from_toml_str(
            &std::fs::read_to_string(&committed_path)?,
            &committed_path.display().to_string(),
        )?;
        let host = &prov.toolchain.host_arch;
        let found = found_outputs(&committed, host, &dir)?;
        checks.push(committed_check(&committed, host, &found));
    }

    // 8. Every package the build recorded as local-repository input holds to the
    //    package rules — the same check that admitted it, run again on the bytes that
    //    are there now. A build that compiles nothing records no package, and its
    //    every package came from the mirror.
    if build.compiles_from_source() {
        checks.push(match crate::artifacts::ledger_debs(&dir) {
            Err(e) => Check {
                what: "packages",
                detail: e.to_string(),
                ok: false,
            },
            Ok(debs) => match crate::artifacts::check_packages(&debs, build.arch) {
                Ok(()) => Check {
                    what: "packages",
                    detail: format!(
                        "{} recorded .deb(s), each within the package rules",
                        debs.len()
                    ),
                    ok: true,
                },
                Err(e) => Check {
                    what: "packages",
                    detail: e.to_string(),
                    ok: false,
                },
            },
        });
    }

    Ok(PointReport {
        point: reference.as_str().to_string(),
        artifact: Some(image),
        checks,
    })
}

/// Check 0: the build record beside the artifacts says the newest build of this point
/// was a whole build that succeeded ([`BuildRecord::vouches_for_image`]).
fn build_record_check(dir: &Path, stem: &str) -> Check {
    let path = dir.join(boot2deb_core::buildrecord::record_name(stem));
    let parsed = std::fs::read_to_string(&path)
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                format!(
                    "no {} — nothing records which build wrote this directory, so nothing \
                     here can be vouched for; build the point again",
                    path.display()
                )
            } else {
                format!("cannot read {} ({e})", path.display())
            }
        })
        .and_then(|text| {
            BuildRecord::from_toml_str(&text, &path.display().to_string())
                .map_err(|e| e.to_string())
        });
    match parsed.and_then(|r| r.vouches_for_image().map(|()| r)) {
        Ok(record) => Check {
            what: "build",
            detail: format!(
                "the newest whole build succeeded, finished {}",
                record
                    .finished
                    .as_deref()
                    .unwrap_or("at an unrecorded time")
            ),
            ok: true,
        },
        Err(why) => Check {
            what: "build",
            detail: why,
            ok: false,
        },
    }
}

/// The image artifact to read: the raw `.img` if the build kept it, else the compressed
/// form. The raw image is read in place, so keeping it saves the file check a
/// decompression.
fn find_image(dir: &Path, stem: &str) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let candidates = [
        format!("{stem}.img"),
        format!("{stem}.img.xz"),
        format!("{stem}.img.gz"),
    ];
    for name in &candidates {
        let path = dir.join(name);
        if path.is_file() {
            return Ok(path);
        }
    }
    Err(format!(
        "no image in {} (looked for {}) — run `boot2deb build` first",
        dir.display(),
        candidates.join(", ")
    )
    .into())
}

/// The files `committed` names for `host_arch`, as they are in `dir`: each one present,
/// hashed. A file that is absent is left out, which the check reports as not written.
fn found_outputs(
    committed: &boot2deb_core::outputs::CommittedOutputs,
    host_arch: &str,
    dir: &Path,
) -> Result<Vec<boot2deb_core::outputs::Output>, Box<dyn std::error::Error>> {
    let mut found = Vec::new();
    for c in committed.hosts.get(host_arch).into_iter().flatten() {
        let path = dir.join(&c.file);
        if !path.is_file() {
            continue;
        }
        let (size, sha256) = boot2deb_engine::blobs::sha256_file(&path)?;
        found.push(boot2deb_core::outputs::Output {
            step: c.step.clone(),
            role: c.role.clone(),
            file: c.file.clone(),
            size,
            sha256,
            per_image: false,
        });
    }
    Ok(found)
}

/// The committed-outputs check: the files found in the artifact directory against the
/// recipe's committed record, for the host that built them.
///
/// Passes when the record has no entry for `host_arch`, with a detail that says so,
/// because a record from one host says nothing about another's bytes. Otherwise it fails
/// on any output that differs or is missing, and names the first few.
fn committed_check(
    committed: &boot2deb_core::outputs::CommittedOutputs,
    host_arch: &str,
    outputs: &[boot2deb_core::outputs::Output],
) -> Check {
    const WHAT: &str = "outputs";
    let Some(judged) = committed.check(host_arch, outputs) else {
        let hosts: Vec<&str> = committed.hosts.keys().map(String::as_str).collect();
        return Check {
            what: WHAT,
            detail: format!(
                "recorded for {} only, and this build ran on {host_arch}, so not compared",
                hosts.join(", ")
            ),
            ok: true,
        };
    };
    let failures: Vec<String> = judged
        .outputs
        .iter()
        .filter(|o| o.verdict.is_failure())
        .map(|o| match &o.verdict {
            boot2deb_core::outputs::Verdict::Differs { detail } => {
                format!("{} ({detail})", o.file)
            }
            _ => format!("{} (not written)", o.file),
        })
        .collect();
    Check {
        what: WHAT,
        detail: if failures.is_empty() {
            format!(
                "{} outputs byte-identical to the record for {host_arch}",
                judged.outputs.len()
            )
        } else {
            let shown = failures.len().min(3);
            let mut detail = format!(
                "{} of {} differ from the record for {host_arch}: {}",
                failures.len(),
                judged.outputs.len(),
                failures[..shown].join("; ")
            );
            if failures.len() > shown {
                detail.push_str(&format!("; and {} more", failures.len() - shown));
            }
            detail
        },
        ok: failures.is_empty(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use boot2deb_core::outputs::{CommittedOutputs, Output};

    fn out(file: &str, sha: char) -> Output {
        Output {
            step: "kernel".into(),
            role: "image_deb".into(),
            file: file.into(),
            size: 1,
            sha256: sha.to_string().repeat(64),
            per_image: false,
        }
    }

    /// The check passes on the recorded bytes, fails naming a moved output, and passes a
    /// build from a host the record does not cover, saying why it compared nothing.
    #[test]
    fn the_build_record_vouches_only_for_a_whole_build_that_succeeded() {
        let tmp = tempfile::tempdir().unwrap();
        let write = |record: BuildRecord| {
            std::fs::write(tmp.path().join("p.build.toml"), record.to_toml()).unwrap()
        };
        // No record: nothing says which build wrote the directory.
        let none = build_record_check(tmp.path(), "p");
        assert!(!none.ok && none.detail.contains("no "), "{}", none.detail);

        let started = || BuildRecord::started("d/p", "all", "2026-10-08T12:00:00Z".into());
        write(started().settled("2026-10-08T13:00:00Z".into(), None));
        let ok = build_record_check(tmp.path(), "p");
        assert!(
            ok.ok && ok.detail.contains("2026-10-08T13:00:00Z"),
            "{}",
            ok.detail
        );

        write(started().settled(
            "2026-10-08T12:10:00Z".into(),
            Some("u-boot: make failed".into()),
        ));
        let failed = build_record_check(tmp.path(), "p");
        assert!(
            !failed.ok && failed.detail.contains("make failed"),
            "{}",
            failed.detail
        );

        write(BuildRecord::started("d/p", "kernel", "t".into()).settled("t".into(), None));
        assert!(!build_record_check(tmp.path(), "p").ok);

        std::fs::write(tmp.path().join("p.build.toml"), "not = [toml").unwrap();
        assert!(!build_record_check(tmp.path(), "p").ok);
    }

    #[test]
    fn the_committed_outputs_check_names_what_moved_and_skips_other_hosts() {
        let mut record = CommittedOutputs::default();
        record.record("x86_64", &[out("kernel.deb", 'a'), out("rootfs.tar", 'b')]);

        let same = committed_check(
            &record,
            "x86_64",
            &[out("kernel.deb", 'a'), out("rootfs.tar", 'b')],
        );
        assert!(same.ok, "{}", same.detail);

        let moved = committed_check(
            &record,
            "x86_64",
            &[out("kernel.deb", 'f'), out("rootfs.tar", 'b')],
        );
        assert!(!moved.ok);
        assert!(
            moved.detail.starts_with("1 of 2 differ"),
            "{}",
            moved.detail
        );
        assert!(moved.detail.contains("kernel.deb"), "{}", moved.detail);

        let other = committed_check(&record, "aarch64", &[out("kernel.deb", 'f')]);
        assert!(other.ok);
        assert!(
            other.detail.contains("recorded for x86_64 only"),
            "{}",
            other.detail
        );
    }
}
