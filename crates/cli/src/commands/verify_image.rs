//! `verify-image`: hold a finished image artifact to the invariants that are checkable
//! without a board.
//!
//! This is the off-board half of the hardware gate. What it asks:
//!
//!  1. The artifact set is there — the image, its provenance manifest, and the plan
//!     document the rootfs published.
//!  2. The plan parses, and its digest is the one the provenance records. A mismatch
//!     means the manifest describes a document other than the one that shipped.
//!  3. `[[archives]]` is well formed: at least the mirror and the build's own pool, the
//!     pool marked `local` with no mirror URL (a per-run path is not portable
//!     provenance), and `signed_by` written on every row — an empty `signed_by` is a
//!     *fact* (trusted unsigned), so it must be present rather than absent.
//!  4. **The ext4 filesystem is exactly its GPT partition.** Larger and it will not
//!     mount at all; smaller and the difference is wasted. This is the invariant the
//!     fit ordering exists to preserve, so it is checked on every image and not only on
//!     the fitted one.
//!  5. A fitted `image_size` left the slack it asked for.
//!
//! Every structure is read by the code that writes it —
//! [`image::inspect`](boot2deb_engine::image::inspect) for the GPT and the superblock,
//! [`ProvenanceManifest`] for the record — so the gate cannot drift from the build by
//! parsing the same bytes differently. The alternative is a second implementation of
//! both parsers that nothing tests.
//!
//! Read-only, and no root: only the head of the artifact is decompressed.

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
                "  {:<14} {}",
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
/// form. Only its head is decompressed either way, so the choice costs nothing.
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
