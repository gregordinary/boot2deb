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
//! newest build did not write.
//!
//! The image is the one the point's artifact inventory ([`boot2deb_core::inventory`])
//! names. That is the image stage's entry for the current lock, in the layout that stage
//! laid out. The artifact is the one for the role that holds the rootfs: the whole disk,
//! or a split build's rootfs image. Then it asks:
//!
//!  1. **The artifact is the bytes the build recorded.** A file changed since ends the
//!     report there, since everything below reads it. An image formatted from
//!     an external `--rootfs-tar` fails here too: it is not the lock's.
//!  2. The provenance manifest records this artifact among its outputs, with the same
//!     digest, so the manifest describes this image and not an earlier one.
//!  3. The plan parses, and its digest is the one the provenance records.
//!  4. `[[archives]]` is well formed. It contains at least the mirror and the build's
//!     own pool. The pool is marked `local` with no mirror URL, since a per-run path
//!     is not portable provenance. `signed_by` is written on every row, because an
//!     empty `signed_by` is a *fact* (trusted unsigned) and must be present rather
//!     than absent.
//!  5. The disk is the length the build's digest record and the provenance both state.
//!  6. **Both copies of the partition table validate.** The primary is read strictly
//!     (header and entry-array CRCs, every extent inside the disk), and the backup sits
//!     on the disk's last LBA and agrees with it.
//!  7. The rootfs partition is the one with the PARTUUID the image's identity gives it,
//!     whatever `--rootfs-label` named it, and its type is a Linux filesystem's.
//!  8. **The ext4 filesystem is exactly its GPT partition**, as the superblock on the
//!     disk states it and as the provenance recorded it. Larger and it will not mount
//!     at all. Smaller and the difference is wasted.
//!  9. **The partition a scanning bootloader must open is marked bootable.** U-Boot's
//!     `bootflow scan` narrows to the partitions with the legacy-BIOS-bootable
//!     attribute, as soon as any partition on the medium has it. It scans partition 1
//!     alone when none does, and partition 1 is the seed. The partition is the rootfs,
//!     or under the `efi` loader the EFI system partition. That one also has to have the
//!     EFI system type, be FAT32, and scan clean. It has to hold the loader at the
//!     removable-media path, and the DTB under the name u-boot loads it by.
//! 10. A fitted `image_size` left the slack it asked for, by the superblock's count.
//! 11. Under `rockchip-rkbin`, **the boot region holds the payloads the image stage
//!     placed**. Each is read at its offset and held to the digest the stage recorded for
//!     the file it consumed. A split build's boot image is held to the same record.
//! 12. **The rootfs holds exactly the files its manifest lists.** The rootfs partition
//!     is walked where it sits and compared with the `<stem>.rootfs.uapi16` the build
//!     wrote from the same filesystem, every field of every path.
//! 13. Where the recipe has a committed outputs record (`recipes/<recipe>.outputs`),
//!     **the artifact directory holds every file it names for this build host, byte for
//!     byte.** The files are hashed where they sit, so the check holds what would
//!     ship, whichever run wrote it. A record with no entry for this host passes, saying
//!     so, since another host's compilers make other bytes.
//! 14. Where the build compiles anything, **every `.deb` it recorded as
//!     local-repository input holds to the package rules**
//!     ([`boot2deb_core::debcheck`]). That is the check that admitted each package,
//!     run again on the bytes in the artifact directory.
//!
//! Every structure is read by the code that writes it, or by the strict reader that
//! checks it. [`image::inspect`](boot2deb_engine::image::inspect) reads the tables, the
//! superblock and the ESP, and [`ProvenanceManifest`] reads the record. The gate
//! therefore cannot drift from the build by parsing the same bytes differently.
//!
//! It needs no root, and changes nothing it verifies. The disk checks read the image by
//! offset, every read bounded by the disk. A compressed image is therefore decompressed
//! once, into a sparse temporary file in the artifact directory that is removed when the
//! point's checks end. The committed-outputs check hashes the recorded files, and the
//! package check reads each recorded package's members.

use boot2deb_core::buildrecord::BuildRecord;
use boot2deb_core::model::Overrides;
use boot2deb_core::press::ArtifactRole;
use boot2deb_core::provenance::ProvenanceManifest;
use boot2deb_core::{resolve_recipe, ConfigRoot};
use boot2deb_engine::image::inspect::{self, EspReport, RawImage};
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
/// files the newest build did not write. The image itself is the one the point's
/// artifact inventory names for the current lock, in the layout the build laid it out
/// in, and it is checked against its recorded digest before anything reads it.
fn verify_point(
    root: &ConfigRoot,
    recipe: &str,
    out_dir: Option<PathBuf>,
) -> Result<PointReport, Box<dyn std::error::Error>> {
    let point = crate::config::build_point(recipe, Vec::new())?;
    let reference = point.reference();
    let stem = point.artifact_stem();
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

    // The image, as the inventory records it: the image stage's entry for this lock, in
    // the layout that stage laid out, and the artifact for the role that holds the rootfs.
    let inventory = crate::inventory::load(&dir, &stem)?;
    let entry = crate::inventory::current(
        &inventory,
        boot2deb_core::inventory::Stage::Image,
        &crate::inventory::lock_digest(root, reference.as_str())?,
        &dir,
        reference.as_str(),
    )?;
    let layout = entry
        .layout
        .ok_or("the image stage's inventory entry records no layout — rebuild")?;
    let build = resolve_recipe(
        root,
        reference.as_str(),
        &Overrides {
            layout: Some(layout),
            ..Overrides::default()
        },
    )?;
    if !build.produces_image() {
        return Err(format!(
            "recipe '{recipe}' builds only a bootloader, so there is no image to verify"
        )
        .into());
    }
    let os_role = boot2deb_core::press::roles(&build)
        .into_iter()
        .find(|r| r.contains_rootfs())
        .expect("an image build has a rootfs-containing artifact");
    let row = entry.disk_image(os_role).ok_or_else(|| {
        format!(
            "the build published no {} — run `boot2deb build {recipe}`",
            os_role.describe()
        )
    })?;
    let image = dir.join(&row.file);
    let disk = row
        .disk
        .clone()
        .expect("a disk image's inventory row records its disk");

    // 1. The artifact is the bytes the build recorded. Everything below reads it, so a
    //    file changed since is the whole report.
    let artifact_check = match crate::inventory::verified(&dir, row) {
        Ok(_) => Check {
            what: "artifact",
            detail: format!(
                "the {} the build recorded ({layout} layout{})",
                os_role.describe(),
                if entry.external_rootfs {
                    ", from an external --rootfs-tar"
                } else {
                    ""
                }
            ),
            ok: !entry.external_rootfs,
        },
        Err(e) => Check {
            what: "artifact",
            detail: e.to_string(),
            ok: false,
        },
    };
    let artifact_ok = artifact_check.ok;
    checks.push(artifact_check);
    if !artifact_ok {
        return Ok(PointReport {
            point: reference.as_str().to_string(),
            artifact: Some(image),
            checks,
        });
    }

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

    // 2. The provenance describes this image: its record of the build's outputs names
    //    the artifact with the digest the inventory has.
    let recorded = prov.outputs.iter().find(|o| o.file == row.file);
    checks.push(Check {
        what: "provenance",
        detail: match recorded {
            Some(o) if o.sha256 == row.sha256 => {
                "records this artifact with the same digest".into()
            }
            Some(o) => format!(
                "records {} with sha256 {}, and the artifact is {}",
                row.file, o.sha256, row.sha256
            ),
            None => format!("does not record {} among its outputs", row.file),
        },
        ok: recorded.is_some_and(|o| o.sha256 == row.sha256),
    });

    // 3. The plan parses, and it is the document the provenance describes.
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

    // 4. The archive rows.
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
                "the pool contains a mirror URL — a build-host path is not portable \
                 provenance"
                    .into()
            }
            Some(_) => "local = true, no mirror URL".into(),
        },
        ok: pool.is_some_and(|p| p.mirror.is_none()),
    });

    // From here every check reads the disk itself, opened once: a container is
    // decompressed into a sparse temporary file in the artifact directory, which is
    // removed when the point's checks end.
    let raw = RawImage::open(&image, &dir)?;
    let identity = boot2deb_engine::image::ImageIdentity::derive(reference.as_str(), &build.device);

    // 5. The disk is the length every record states: the build's digest record, the
    //    provenance, and what the container decompressed to.
    let lengths_agree = raw.len() == disk.bytes && disk.bytes == prov.image.image_bytes;
    checks.push(Check {
        what: "size",
        detail: if lengths_agree {
            format!(
                "{} -> {} bytes on disk, as recorded",
                prov.image.image_size,
                raw.len()
            )
        } else {
            format!(
                "the disk is {} bytes; the build recorded {} and the provenance {}",
                raw.len(),
                disk.bytes,
                prov.image.image_bytes
            )
        },
        ok: lengths_agree && !raw.is_empty(),
    });

    // 6. Both copies of the partition table, read strictly: the primary validates, and
    //    the backup sits on the disk's last LBA and agrees with it.
    let table = inspect::read_table(&raw);
    checks.push(Check {
        what: "gpt",
        detail: match &table {
            Err(e) => e.to_string(),
            Ok(t) => match &t.backup {
                Ok(()) => format!(
                    "{} entries; the backup on the last LBA agrees with the primary",
                    t.entries.len()
                ),
                Err(why) => why.clone(),
            },
        },
        ok: table.as_ref().is_ok_and(|t| t.backup.is_ok()),
    });

    // 7. The rootfs partition, found by the PARTUUID the image's identity gives it, so a
    //    `--rootfs-label` is no obstacle. Its type is a separate fact.
    let part = table.as_ref().map_err(|e| e.to_string()).and_then(|t| {
        t.by_guid(&raw, identity.rootfs_partuuid)
            .map_err(|e| e.to_string())
    });
    checks.push(Check {
        what: "rootfs partition",
        detail: match &part {
            Err(e) => e.clone(),
            Ok(p) if p.type_guid == inspect::LINUX_FS_TYPE => format!(
                "entry {} ({:?}), PARTUUID {}, Linux filesystem type",
                p.index, p.name, identity.rootfs_partuuid
            ),
            Ok(p) => format!(
                "entry {} has type {}, not a Linux filesystem's",
                p.index, p.type_guid
            ),
        },
        ok: part
            .as_ref()
            .is_ok_and(|p| p.type_guid == inspect::LINUX_FS_TYPE),
    });

    // 8. The ext4 filesystem is exactly its partition, as the superblock on the disk
    //    states it, and as the provenance recorded it. Larger and it will not mount;
    //    smaller and the difference is wasted.
    let geom = &prov.filesystem.geometry;
    let recorded_bytes = geom.total_blocks * geom.block_size as u64;
    let sb = part
        .as_ref()
        .map_err(Clone::clone)
        .and_then(|p| inspect::superblock(&raw, p).map_err(|e| e.to_string()));
    checks.push(Check {
        what: "ext4 fits",
        detail: match (&part, &sb) {
            (Err(e), _) | (_, Err(e)) => format!("could not read the rootfs: {e}"),
            (Ok(p), Ok(sb)) if sb.bytes() == Some(p.bytes) && p.bytes == recorded_bytes => {
                format!(
                    "{} x {} = {} bytes — the filesystem fills the partition exactly, as \
                     recorded",
                    sb.blocks, sb.block_size, p.bytes
                )
            }
            (Ok(p), Ok(sb)) => format!(
                "the partition is {} bytes, the superblock states {} x {}, and the \
                 provenance {} x {}",
                p.bytes, sb.blocks, sb.block_size, geom.total_blocks, geom.block_size
            ),
        },
        ok: matches!((&part, &sb), (Ok(p), Ok(sb))
            if sb.bytes() == Some(p.bytes) && p.bytes == recorded_bytes),
    });

    // 9. The partition a bootloader will look inside is marked so. U-Boot's
    //    `bootflow scan` considers only partitions marked bootable once any is
    //    marked, and only partition 1 when none is — and partition 1 is the seed.
    //    Nothing about a filesystem's contents says whether the bootloader will ever
    //    open it, so the attribute is its own check. Under the efi loader the
    //    partition is the ESP, found by its PARTUUID and held to the EFI type, and read
    //    back with what the firmware loads from it.
    let efi = build
        .image
        .as_ref()
        .and_then(|i| i.loader.as_ref())
        .and_then(|l| l.efi());
    match efi {
        None => checks.push(Check {
            what: "rootfs bootable",
            detail: match &part {
                Err(e) => format!("could not read the rootfs partition: {e}"),
                Ok(p) if p.bootable => "the GPT entry has the bootable attribute".into(),
                Ok(_) => "the rootfs GPT entry is not marked bootable — a scanning \
                          bootloader will never open it"
                    .into(),
            },
            ok: part.as_ref().is_ok_and(|p| p.bootable),
        }),
        Some(efi) => {
            let required = [
                format!("/EFI/BOOT/{}", efi.arch.removable_file),
                format!("/dtb/{}", efi.dtb),
            ];
            let esp = table
                .as_ref()
                .map_err(|e| e.to_string())
                .and_then(|t| {
                    t.by_guid(&raw, identity.esp_partuuid)
                        .map_err(|e| e.to_string())
                })
                .and_then(|p| inspect::esp(&raw, &p, &required).map_err(|e| e.to_string()));
            checks.push(Check {
                what: "esp bootable",
                detail: match &esp {
                    Err(e) => format!("could not read the EFI system partition: {e}"),
                    Ok(e) if !e.efi_type => {
                        "the EFI system partition's entry does not have the EFI system type \
                         — firmware will not select it"
                            .into()
                    }
                    Ok(e) if e.bootable => "the EFI system partition has the EFI system type \
                                            and the bootable attribute"
                        .into(),
                    Ok(_) => "the EFI system partition is not marked bootable — u-boot's \
                              scan will never open it"
                        .into(),
                },
                ok: esp.as_ref().is_ok_and(|e| e.efi_type && e.bootable),
            });
            checks.push(Check {
                what: "esp contents",
                detail: match &esp {
                    Err(e) => format!("could not read the EFI system partition: {e}"),
                    Ok(e) if !e.fat32 => "the volume is not FAT32".into(),
                    Ok(EspReport {
                        findings: Some(found),
                        ..
                    }) => format!("the volume does not scan clean:\n{found}"),
                    Ok(e) if !e.missing.is_empty() => {
                        format!("the volume holds no {}", e.missing.join(", "))
                    }
                    Ok(_) => format!("FAT32, scans clean, holds {}", required.join(" and ")),
                },
                ok: esp
                    .as_ref()
                    .is_ok_and(|e| e.fat32 && e.findings.is_none() && e.missing.is_empty()),
            });
        }
    }

    // 10. A fitted size additionally has to have left the slack it asked for, measured
    //     as the formatter measures it: free blocks once the source is written, read
    //     from the superblock on the disk.
    if let boot2deb_core::size::ImageSize::Fit(slack) =
        boot2deb_core::size::parse_image_size(&prov.image.image_size)?
    {
        let required = required_free_blocks(slack, geom.total_blocks, geom.block_size as u64);
        checks.push(Check {
            what: "slack",
            detail: match &sb {
                Err(e) => format!("could not read the free-block count: {e}"),
                Ok(sb) if sb.free_blocks >= required => format!(
                    "{} free of {} blocks; the minimum is {required} — honored",
                    sb.free_blocks, sb.blocks
                ),
                Ok(sb) => format!(
                    "{} free blocks, under the {required} that {} requires",
                    sb.free_blocks, prov.image.image_size
                ),
            },
            ok: sb.as_ref().is_ok_and(|sb| sb.free_blocks >= required),
        });
    }

    // 11. The boot region holds the bootloader the image stage placed, byte for byte:
    //     each payload, at its offset, against the digest the stage recorded for the
    //     file it consumed. A split build's boot image is held to the same record.
    if let Some(boot) = build.rkbin_boot() {
        let mut media = vec![(None, &raw)];
        let boot_raw;
        if layout == boot2deb_core::model::Layout::Split {
            boot_raw = match entry
                .disk_image(ArtifactRole::Boot)
                .ok_or_else(|| "the build published no boot image".to_string())
                .and_then(|r| crate::inventory::verified(&dir, r).map_err(|e| e.to_string()))
                .and_then(|p| RawImage::open(&p, &dir).map_err(|e| e.to_string()))
            {
                Ok(r) => Some(r),
                Err(e) => {
                    checks.push(Check {
                        what: "boot image",
                        detail: e,
                        ok: false,
                    });
                    None
                }
            };
            media = boot_raw.iter().map(|r| (Some("boot image"), r)).collect();
        }
        for (medium, raw) in media {
            checks.push(boot_region_check(
                raw,
                entry,
                medium,
                &[
                    (
                        boot2deb_core::inventory::role::IDBLOADER,
                        &boot.offsets.idbloader,
                    ),
                    (
                        boot2deb_core::inventory::role::UBOOT_ITB,
                        &boot.offsets.uboot_itb,
                    ),
                ],
            ));
        }
    }

    // 12. The files the rootfs holds are the files its manifest lists, down to every
    //     field. The manifest was written from this filesystem before it was placed, so
    //     any difference is a defect in the placement or the record, and nothing is set
    //     aside.
    checks.push(
        match (crate::artifacts::files_manifest(&prov, &dir), &part) {
            (Err(e), _) => Check {
                what: "rootfs files",
                detail: format!("could not read the file manifest: {e}"),
                ok: false,
            },
            (Ok(None), _) => Check {
                what: "rootfs files",
                detail: "the provenance records no file manifest".into(),
                ok: false,
            },
            (Ok(Some(_)), Err(e)) => Check {
                what: "rootfs files",
                detail: format!("could not find the rootfs partition: {e}"),
                ok: false,
            },
            (Ok(Some(listed)), Ok(part)) => {
                match boot2deb_engine::image::files::manifest_of_image(&raw, part) {
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
                                        .map(|c| format!(
                                            ", first {} ({})",
                                            c.path,
                                            c.fields.join(", ")
                                        ))
                                        .unwrap_or_default()
                                )
                            },
                            ok: diff.is_empty(),
                        }
                    }
                }
            }
        },
    );
    drop(raw);

    // 13. The bytes a build of this lock wrote before, where the recipe records them.
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

    // 14. Every package the build recorded as local-repository input holds to the
    //     package rules — the same check that admitted it, run again on the bytes that
    //     are there now. A build that compiles nothing records no package, and its
    //     every package came from the mirror.
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

/// The boot-region check of one medium: each payload the image stage consumed, read at
/// its offset for its recorded length, holds the recorded digest. `medium` names a split
/// build's boot image; `None` is the image itself.
fn boot_region_check(
    raw: &RawImage,
    entry: &boot2deb_core::inventory::StageEntry,
    medium: Option<&str>,
    payloads: &[(&str, &String)],
) -> Check {
    let what = if medium.is_some() {
        "boot image region"
    } else {
        "boot region"
    };
    let mut held = Vec::new();
    for (name, offset) in payloads {
        let Some(input) = entry.input(name) else {
            return Check {
                what,
                detail: format!("the image stage recorded no {name} it placed"),
                ok: false,
            };
        };
        let offset = match boot2deb_core::size::parse_size(offset) {
            Ok(o) => o,
            Err(e) => {
                return Check {
                    what,
                    detail: format!("the {name} offset {offset:?} does not parse: {e}"),
                    ok: false,
                }
            }
        };
        match raw.sha256_at(offset, input.size) {
            Ok(sha) if sha == input.sha256 => {
                held.push(format!("{} at {offset}", input.file));
            }
            Ok(sha) => {
                return Check {
                    what,
                    detail: format!(
                        "the {input_size} bytes at {offset} hash to {sha}, not to the {} \
                         ({}) the image stage placed",
                        input.file,
                        input.sha256,
                        input_size = input.size
                    ),
                    ok: false,
                }
            }
            Err(e) => {
                return Check {
                    what,
                    detail: e.to_string(),
                    ok: false,
                }
            }
        }
    }
    Check {
        what,
        detail: format!("holds {}", held.join(" and ")),
        ok: true,
    }
}

/// The fewest free blocks a fitted filesystem of `total_blocks` must keep to honor
/// `slack`.
///
/// Both minimums round up to whole blocks, because the formatter cannot leave a fraction
/// of one free. A share is the fewest blocks that are at least that share of the
/// filesystem, which is what the formatter searches for, so a filesystem sitting exactly
/// at the minimum passes and one a fraction of a block short of the share does not.
fn required_free_blocks(
    slack: boot2deb_core::size::Slack,
    total_blocks: u64,
    block_size: u64,
) -> u64 {
    match slack {
        boot2deb_core::size::Slack::Share(hundredths) => {
            (total_blocks * hundredths as u64).div_ceil(10_000)
        }
        boot2deb_core::size::Slack::Bytes(bytes) => bytes.div_ceil(block_size),
    }
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
        write(started().ended("2026-10-08T13:00:00Z".into(), None));
        let ok = build_record_check(tmp.path(), "p");
        assert!(
            ok.ok && ok.detail.contains("2026-10-08T13:00:00Z"),
            "{}",
            ok.detail
        );

        write(started().ended(
            "2026-10-08T12:10:00Z".into(),
            Some("u-boot: make failed".into()),
        ));
        let failed = build_record_check(tmp.path(), "p");
        assert!(
            !failed.ok && failed.detail.contains("make failed"),
            "{}",
            failed.detail
        );

        write(BuildRecord::started("d/p", "kernel", "t".into()).ended("t".into(), None));
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

    /// A share asks for whole blocks rounded up, as the formatter's fit search does: a
    /// fifth of 1001 blocks is 200.2, so 200 free is short and 201 is the minimum.
    #[test]
    fn a_slack_minimum_rounds_up_to_whole_blocks() {
        use boot2deb_core::size::Slack;
        assert_eq!(required_free_blocks(Slack::Share(2000), 1001, 4096), 201);
        // A share that divides exactly asks for exactly that many.
        assert_eq!(required_free_blocks(Slack::Share(2000), 1000, 4096), 200);
        assert_eq!(required_free_blocks(Slack::Share(150), 10_000, 4096), 150);
        // A byte minimum is whole blocks too: one byte past a block needs a second.
        assert_eq!(required_free_blocks(Slack::Bytes(4097), 1000, 4096), 2);
        assert_eq!(required_free_blocks(Slack::Bytes(8192), 1000, 4096), 2);
    }
}
