//! `sbom`: export an image's bill of materials as SPDX or CycloneDX.
//!
//! Reads two documents a build already published — the provenance manifest and the
//! solved package manifest beside it — and renders them. Offline, and it builds
//! nothing: an SBOM describes an image that exists, so the input is a *published
//! build* rather than a recipe's lock. A lock says what an image would be made of;
//! only a build says what one is.
//!
//! All the deciding is [`boot2deb_core::sbom`]; this module finds the files, reads
//! them, and writes the JSON.

use boot2deb_core::sbom::{cyclonedx, spdx, Sbom};
use boot2deb_core::{manifest, provenance::ProvenanceManifest, ConfigRoot};
use clap::ValueEnum;
use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Which document format to write.
#[derive(Clone, Copy, PartialEq, Eq, ValueEnum)]
pub(crate) enum FormatArg {
    /// SPDX 2.3, JSON.
    Spdx,
    /// CycloneDX 1.6, JSON.
    Cyclonedx,
}

/// Run `sbom <recipe|provenance.toml>`.
///
/// `target` is either a recipe — whose published provenance manifest is looked for in
/// the same output directory `build` writes to and `reproduce` reads from — or a path
/// to a `.provenance.toml` from anywhere, which is the form someone shipping a device
/// uses against an image they were handed.
pub(crate) fn run(
    root: &ConfigRoot,
    target: &str,
    format: FormatArg,
    out: Option<PathBuf>,
    features: Vec<String>,
) -> Result<()> {
    let path = locate(root, target, features)?;
    let manifest_text = std::fs::read_to_string(&path)
        .map_err(|e| format!("read provenance manifest {}: {e}", path.display()))?;
    let provenance =
        ProvenanceManifest::from_toml_str(&manifest_text, &path.display().to_string())?;

    // The solved manifest published beside it, named for the same artifact stem. The
    // *committed* manifest the provenance names lives beside the lock under a
    // different name, and it is the wrong file to read: it describes the recipe's
    // pinned package set, not necessarily the one this image shipped.
    let stem = artifact_stem(&path)?;
    let packages_path = path.with_file_name(boot2deb_core::manifest::manifest_name(&stem));
    let packages_text = std::fs::read_to_string(&packages_path).map_err(|e| {
        format!(
            "read the solved package manifest {}: {e}\nIt is published beside the provenance \
             manifest by the same build; without it an SBOM would list an image's sources and \
             none of its packages.",
            packages_path.display()
        )
    })?;
    let packages = manifest::parse(&packages_text, &packages_path.display().to_string())?;

    let sources = source_index(&path.with_file_name(format!("{stem}.plan")));
    let sbom = Sbom::from_provenance(&provenance, &packages, &sources, &stem, &created()?);
    let json = match format {
        FormatArg::Spdx => serde_json::to_string_pretty(&spdx::Document::render(&sbom))?,
        FormatArg::Cyclonedx => serde_json::to_string_pretty(&cyclonedx::Bom::render(&sbom))?,
    };
    match out {
        // A written file gets its trailing newline; a piped document does too, so a
        // shell prompt does not land on the closing brace.
        Some(dest) => {
            std::fs::write(&dest, format!("{json}\n"))
                .map_err(|e| format!("write {}: {e}", dest.display()))?;
            eprintln!(
                "wrote {} ({} components, {})",
                dest.display(),
                sbom.components.len(),
                format.label()
            );
        }
        None => println!("{json}"),
    }
    Ok(())
}

impl FormatArg {
    /// The format's name, for the confirmation line.
    fn label(self) -> &'static str {
        match self {
            FormatArg::Spdx => "SPDX 2.3",
            FormatArg::Cyclonedx => "CycloneDX 1.6",
        }
    }

    /// The infix a document written beside an image is named with:
    /// `<stem>.spdx.json` / `<stem>.cyclonedx.json`. Both formats are JSON, so the
    /// format has to be in the name or the second one would overwrite the first.
    pub(crate) fn extension(self) -> &'static str {
        match self {
            FormatArg::Spdx => "spdx",
            FormatArg::Cyclonedx => "cyclonedx",
        }
    }
}

/// Write `formats` of `provenance`'s SBOM beside the image, returning each path
/// written in the order asked for.
///
/// The build-time half of this command: `build --sbom <format>` calls it once the
/// provenance manifest and the solved manifest it describes are both on disk, so the
/// documents are rendered from exactly the files that shipped rather than from the
/// values still in memory. Off by default — a build never silently gains a file — and
/// producible later from the same documents by [`run`], which is the form someone
/// handed an image uses.
pub(crate) fn write_beside(
    provenance: &ProvenanceManifest,
    stem: &str,
    out_dir: &Path,
    packages_path: &Path,
    formats: &[FormatArg],
) -> Result<Vec<PathBuf>> {
    if formats.is_empty() {
        return Ok(Vec::new());
    }
    let text = std::fs::read_to_string(packages_path).map_err(|e| {
        format!(
            "read the solved package manifest {}: {e}",
            packages_path.display()
        )
    })?;
    let packages = manifest::parse(&text, &packages_path.display().to_string())?;
    let sources = source_index(&out_dir.join(format!("{stem}.plan")));
    let sbom = Sbom::from_provenance(provenance, &packages, &sources, stem, &created()?);
    let mut written = Vec::with_capacity(formats.len());
    for format in formats {
        let path = out_dir.join(format!("{stem}.{}.json", format.extension()));
        let json = match format {
            FormatArg::Spdx => serde_json::to_string_pretty(&spdx::Document::render(&sbom))?,
            FormatArg::Cyclonedx => serde_json::to_string_pretty(&cyclonedx::Bom::render(&sbom))?,
        };
        std::fs::write(&path, format!("{json}\n"))
            .map_err(|e| format!("write {}: {e}", path.display()))?;
        written.push(path);
    }
    Ok(written)
}

/// Which source package each binary package was built from, read from the plan document
/// published beside the image.
///
/// **Best-effort, and deliberately so.** The attribution is an enrichment of an
/// otherwise complete document, not a component of it: a plan that is absent — an image
/// handed over without one — or written in a format version this boot2deb no longer
/// reads should not cost the operator their SBOM. Both cases produce an empty index and
/// one line on stderr, so the omission is visible without being fatal.
fn source_index(plan: &Path) -> std::collections::BTreeMap<String, String> {
    if !plan.is_file() {
        return Default::default();
    }
    match boot2deb_engine::rootfs::read_plan_weights(plan) {
        Ok(weights) => boot2deb_core::weight::source_index(&weights.packages),
        Err(e) => {
            eprintln!(
                "warning: {e}\nThe document is complete without it; what is missing is which \
                 source package each binary package was built from."
            );
            Default::default()
        }
    }
}

/// Resolve `target` to a provenance manifest path.
///
/// A path is taken as given. A recipe resolves to the manifest in its own output
/// directory — where a build on this machine published it — and a missing one is an
/// error naming the build that would write it, since there is nothing to describe yet.
fn locate(root: &ConfigRoot, target: &str, features: Vec<String>) -> Result<PathBuf> {
    let as_path = Path::new(target);
    if target.ends_with(".provenance.toml") || as_path.is_file() {
        if !as_path.is_file() {
            return Err(format!("no provenance manifest at {target}").into());
        }
        return Ok(as_path.to_path_buf());
    }
    // The same point resolution `build` and `reproduce` perform: a feature variant's
    // artifacts are named for the variant, so its SBOM must not be read from the base
    // recipe's document.
    let point = crate::config::build_point(target, features)?;
    let stem = point.artifact_stem();
    let path = crate::fsutil::absolutize(
        crate::workdir::work_dir_for(root, point.reference().as_str(), None)
            .join("artifacts")
            .join(format!("{stem}.provenance.toml")),
    );
    if !path.is_file() {
        return Err(format!(
            "no provenance manifest at {} — an SBOM describes an image that exists, so \
             build one first (`boot2deb build {target}`) or pass the path to a \
             `.provenance.toml` shipped with an image.",
            path.display()
        )
        .into());
    }
    Ok(path)
}

/// The artifact stem a published document is named for: `turing-rk1-forky` out of
/// `turing-rk1-forky.provenance.toml`. It is what names every other file of that
/// build, which is how the solved manifest beside it is found.
fn artifact_stem(path: &Path) -> Result<String> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| format!("{} has no file name", path.display()))?;
    name.strip_suffix(".provenance.toml")
        .map(str::to_string)
        .ok_or_else(|| {
            format!(
                "{name} is not a published provenance manifest — those are named \
                 `<artifact-stem>.provenance.toml`, and the stem is what finds the solved \
                 package manifest beside it"
            )
            .into()
        })
}

/// The document's creation timestamp, RFC 3339 UTC.
///
/// `SOURCE_DATE_EPOCH` wins where it is set, which is what makes the output
/// byte-reproducible: everything else in the document is derived from the image's own
/// content, so the clock is the only thing that would make two runs differ. Without it
/// the wall clock is used, because both formats require the field and there is no
/// honest constant to substitute.
fn created() -> Result<String> {
    let secs = match std::env::var("SOURCE_DATE_EPOCH") {
        Ok(value) => value.trim().parse::<u64>().map_err(|_| {
            format!("SOURCE_DATE_EPOCH is '{value}', which is not a whole number of seconds")
        })?,
        Err(_) => std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .map_err(|_| "the host clock is before the Unix epoch")?,
    };
    Ok(boot2deb_core::datetime::format_rfc3339(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_published_documents_stem_is_what_names_its_siblings() {
        assert_eq!(
            artifact_stem(Path::new("out/turing-rk1-forky.provenance.toml")).unwrap(),
            "turing-rk1-forky"
        );
        // A feature variant's stem carries the feature, which is the whole reason the
        // sibling manifest is found by stem rather than by directory: both documents
        // sit in one output directory.
        assert_eq!(
            artifact_stem(Path::new("h96-max-m9-forky+jellyfin.provenance.toml")).unwrap(),
            "h96-max-m9-forky+jellyfin"
        );
        let err = artifact_stem(Path::new("image.toml"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("provenance.toml"), "{err}");
    }

    #[test]
    fn source_date_epoch_pins_the_only_field_the_image_does_not_determine() {
        // Every other field is derived from the image's content, so this is the one
        // input that decides whether two runs produce the same bytes.
        //
        // Safety: the environment is process-global and this test sets and clears one
        // variable no other test in this binary reads.
        unsafe { std::env::set_var("SOURCE_DATE_EPOCH", "1767225600") };
        assert_eq!(created().unwrap(), "2026-01-01T00:00:00Z");
        unsafe { std::env::set_var("SOURCE_DATE_EPOCH", "not-a-number") };
        let err = created().unwrap_err().to_string();
        assert!(err.contains("SOURCE_DATE_EPOCH"), "{err}");
        unsafe { std::env::remove_var("SOURCE_DATE_EPOCH") };
        // With none set the wall clock answers, and it still parses as the shape both
        // formats require.
        let now = created().unwrap();
        assert!(now.ends_with('Z') && now.len() == 20, "{now}");
    }
}
