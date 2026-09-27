//! `verify-image`: hold a finished image artifact to the invariants that are checkable
//! without a board.
//!
//! This is the off-board half of the hardware gate. What it asks:
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
//! artifact directory that is removed when the check ends. The eighth hashes the recorded files.

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

/// Run `verify-image <recipe>`.
///
/// Exits non-zero when any invariant fails, so a CI job or the gate script can branch
/// on the status rather than on the text.
pub(crate) fn run(
    root: &ConfigRoot,
    recipe: &str,
    out_dir: Option<PathBuf>,
    json_out: bool,
) -> Result<(), Box<dyn std::error::Error>> {
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

    let mut checks = Vec::new();
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

    let failed = checks.iter().filter(|c| !c.ok).count();
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "recipe": reference.as_str(),
                "artifact": image.display().to_string(),
                "checks": checks.iter().map(|c| json!({
                    "what": c.what, "detail": c.detail, "ok": c.ok,
                })).collect::<Vec<_>>(),
                "failed": failed,
                "result": if failed == 0 { "pass" } else { "fail" },
            }))?
        );
    } else {
        println!("{}  {}", reference.as_str(), image.display());
        for c in &checks {
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
    if failed == 0 {
        Ok(())
    } else {
        Err(format!("{failed} of {} image invariants failed", checks.len()).into())
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
