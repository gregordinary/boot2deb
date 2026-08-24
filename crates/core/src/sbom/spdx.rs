//! SPDX 2.3 JSON, rendered from the [`Sbom`] model.
//!
//! Typed rather than hand-built JSON: the required fields are struct fields, so a
//! document that would fail the published schema mostly fails to compile instead. The
//! caller serializes the [`Document`] with any JSON writer.
//!
//! Two SPDX conventions shape what comes out:
//!
//! - **Every component is a `package`**, including the image itself and each blob.
//!   SPDX has no other container for "a thing with a name, a version and a digest",
//!   and `files` would claim a file-level analysis that never happened
//!   (`filesAnalyzed` is `false` throughout for the same reason).
//! - **Identifiers are positional** (`SPDXRef-Package-0`), because an SPDX id admits
//!   only `[a-zA-Z0-9.-]` and Debian package names do not (`libstdc++6`).

use super::{Component, ComponentKind, Relation, Sbom};
use serde::Serialize;

/// The SPDX value meaning "no claim is made here" — used for every license field and
/// for a download location the build does not record. It is a claim of ignorance, and
/// the honest one: see the [module][super] note on licenses.
const NOASSERTION: &str = "NOASSERTION";

/// An SPDX 2.3 document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Document {
    /// Always `SPDX-2.3`.
    pub spdx_version: &'static str,
    /// Always `CC0-1.0` — the license of the SBOM document itself, which SPDX fixes.
    pub data_license: &'static str,
    /// Always `SPDXRef-DOCUMENT`.
    #[serde(rename = "SPDXID")]
    pub spdx_id: &'static str,
    /// Document name — the image's artifact stem.
    pub name: String,
    /// Globally unique document URI ([`Sbom::namespace`]).
    pub document_namespace: String,
    /// Who created the document and when.
    pub creation_info: CreationInfo,
    /// The elements the document describes: exactly the image.
    pub document_describes: Vec<String>,
    /// Every component, the image included.
    pub packages: Vec<SpdxPackage>,
    /// How they relate.
    pub relationships: Vec<Relationship>,
}

/// SPDX `creationInfo`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CreationInfo {
    /// RFC 3339 UTC creation timestamp.
    pub created: String,
    /// Creators, each `Tool: <name>-<version>`. SPDX admits no more than a name and a
    /// version there, so the builder's commit rides in [`comment`](Self::comment).
    pub creators: Vec<String>,
    /// The builder in full, including the commit and whether its checkout was dirty.
    pub comment: String,
}

/// One SPDX package. Every [`Component`] renders as one of these, whatever kind it is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SpdxPackage {
    /// `SPDXRef-<component id>`.
    #[serde(rename = "SPDXID")]
    pub spdx_id: String,
    /// Component name.
    pub name: String,
    /// Version, omitted where the component has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version_info: Option<String>,
    /// Where the bytes come from, or `NOASSERTION`. Required by SPDX, so it is always
    /// written.
    pub download_location: String,
    /// Always `false`: this document describes packages, not the files inside them,
    /// and claiming otherwise would imply an analysis that did not run.
    pub files_analyzed: bool,
    /// `NOASSERTION` — see the [module][super] note.
    pub license_concluded: &'static str,
    /// `NOASSERTION` — see the [module][super] note.
    pub license_declared: &'static str,
    /// `NOASSERTION` — see the [module][super] note.
    pub copyright_text: &'static str,
    /// Content digests, in practice at most one sha256.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub checksums: Vec<Checksum>,
    /// External identifiers — the purl, where the component has one.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub external_refs: Vec<ExternalRef>,
    /// Free text, where the model carries a description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

/// An SPDX checksum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Checksum {
    /// Always `SHA256` — SPDX spells it without a separator.
    pub algorithm: &'static str,
    /// Lowercase-hex digest.
    pub checksum_value: String,
}

/// An SPDX external reference. Only the package-manager purl is emitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalRef {
    /// Always `PACKAGE-MANAGER`.
    pub reference_category: &'static str,
    /// Always `purl`.
    pub reference_type: &'static str,
    /// The purl itself.
    pub reference_locator: String,
}

/// An SPDX relationship between two elements.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Relationship {
    /// The subject element's id.
    pub spdx_element_id: String,
    /// `DESCRIBES`, `CONTAINS`, or `GENERATED_FROM`.
    pub relationship_type: &'static str,
    /// The object element's id.
    pub related_spdx_element: String,
}

/// The document id every SPDX document's root carries.
const DOCUMENT_ID: &str = "SPDXRef-DOCUMENT";

impl Document {
    /// Render `sbom` as SPDX 2.3.
    pub fn render(sbom: &Sbom) -> Document {
        let id = |c: &Component| format!("SPDXRef-{}", c.id);
        let image_id = id(&sbom.image);
        let mut packages = vec![package(&sbom.image)];
        let mut relationships = vec![Relationship {
            spdx_element_id: DOCUMENT_ID.into(),
            relationship_type: "DESCRIBES",
            related_spdx_element: image_id.clone(),
        }];
        for component in &sbom.components {
            packages.push(package(component));
            // SPDX's `GENERATED_FROM` reads "A was generated from B", so the image is
            // the subject in both directions and the relation only changes the verb.
            relationships.push(Relationship {
                spdx_element_id: image_id.clone(),
                relationship_type: match component.relation {
                    Relation::Contains => "CONTAINS",
                    Relation::GeneratedFrom => "GENERATED_FROM",
                },
                related_spdx_element: id(component),
            });
        }
        Document {
            spdx_version: "SPDX-2.3",
            data_license: "CC0-1.0",
            spdx_id: DOCUMENT_ID,
            name: sbom.name.clone(),
            document_namespace: sbom.namespace(),
            creation_info: CreationInfo {
                created: sbom.created.clone(),
                creators: vec![format!("Tool: {}-{}", sbom.tool.name, sbom.tool.version)],
                comment: format!("built by {}", sbom.tool.describe()),
            },
            document_describes: vec![image_id],
            packages,
            relationships,
        }
    }
}

/// Render one component as an SPDX package.
fn package(component: &Component) -> SpdxPackage {
    SpdxPackage {
        spdx_id: format!("SPDXRef-{}", component.id),
        name: component.name.clone(),
        version_info: component.version.clone(),
        download_location: component
            .download
            .clone()
            .unwrap_or_else(|| NOASSERTION.to_string()),
        files_analyzed: false,
        license_concluded: NOASSERTION,
        license_declared: NOASSERTION,
        copyright_text: NOASSERTION,
        checksums: component
            .sha256
            .iter()
            .map(|hex| Checksum {
                algorithm: "SHA256",
                checksum_value: hex.clone(),
            })
            .collect(),
        external_refs: component
            .purl
            .iter()
            .map(|purl| ExternalRef {
                reference_category: "PACKAGE-MANAGER",
                reference_type: "purl",
                reference_locator: purl.clone(),
            })
            .collect(),
        comment: component.description.clone().or_else(|| {
            // A component the document can say nothing else about still says what
            // kind of thing it is, which is the difference between "a blob" and "an
            // unexplained entry with a digest".
            matches!(component.kind, ComponentKind::Source)
                .then(|| "source tree the image was compiled from".to_string())
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sbom::tests::sbom;

    #[test]
    fn the_document_carries_every_field_the_schema_requires() {
        let doc = Document::render(&sbom());
        let json = serde_json::to_value(&doc).unwrap();
        assert_eq!(json["spdxVersion"], "SPDX-2.3");
        assert_eq!(json["dataLicense"], "CC0-1.0");
        assert_eq!(json["SPDXID"], "SPDXRef-DOCUMENT");
        assert!(json["documentNamespace"]
            .as_str()
            .unwrap()
            .starts_with("https://"));
        assert_eq!(json["creationInfo"]["created"], "2026-08-05T00:00:00Z");
        assert_eq!(json["creationInfo"]["creators"][0], "Tool: boot2deb-0.1.0");
        // Every package carries the three fields SPDX requires of one, and the
        // license triple that a compliance scanner reads.
        for p in json["packages"].as_array().unwrap() {
            assert!(p["SPDXID"].as_str().unwrap().starts_with("SPDXRef-"));
            assert!(p["name"].is_string());
            assert!(p["downloadLocation"].is_string());
            assert_eq!(p["filesAnalyzed"], false);
            for field in ["licenseConcluded", "licenseDeclared", "copyrightText"] {
                assert_eq!(p[field], "NOASSERTION", "{field}");
            }
        }
    }

    #[test]
    fn identifiers_are_legal_spdx_even_for_a_package_name_that_is_not() {
        // `libstdc++6` cannot be an SPDX id, and a document that emitted it would be
        // rejected outright — so ids are positional and the name lives in `name`.
        let doc = Document::render(&sbom());
        let stdcpp = doc
            .packages
            .iter()
            .find(|p| p.name == "libstdc++6")
            .expect("the fixture carries it");
        assert_eq!(stdcpp.spdx_id, "SPDXRef-Package-1");
        for p in &doc.packages {
            let body = p.spdx_id.strip_prefix("SPDXRef-").unwrap();
            assert!(
                body.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '.'),
                "illegal SPDX id: {}",
                p.spdx_id
            );
        }
    }

    #[test]
    fn the_relationship_graph_describes_the_image_and_distinguishes_sources() {
        // The one substantive claim the graph makes: a kernel tree the image was
        // compiled from is not a thing the image contains.
        let doc = Document::render(&sbom());
        let first = &doc.relationships[0];
        assert_eq!(first.spdx_element_id, "SPDXRef-DOCUMENT");
        assert_eq!(first.relationship_type, "DESCRIBES");
        assert_eq!(first.related_spdx_element, "SPDXRef-Image");
        assert_eq!(doc.document_describes, ["SPDXRef-Image"]);

        let verb = |id: &str| {
            doc.relationships
                .iter()
                .find(|r| r.related_spdx_element == format!("SPDXRef-{id}"))
                .map(|r| r.relationship_type)
                .unwrap()
        };
        assert_eq!(verb("Package-0"), "CONTAINS");
        assert_eq!(verb("Blob-0"), "CONTAINS");
        assert_eq!(verb("ExtraDeb-0"), "CONTAINS");
        assert_eq!(verb("Source-0"), "GENERATED_FROM");
        // Every component is related exactly once, plus the DESCRIBES edge.
        assert_eq!(doc.relationships.len(), doc.packages.len());
    }

    #[test]
    fn a_source_pin_states_its_commit_where_a_consumer_looks_for_it() {
        // Its purl version and its comment both carry the commit, because a consumer
        // scanning purls and a human reading the document look in different places.
        let doc = Document::render(&sbom());
        let linux = doc.packages.iter().find(|p| p.name == "linux").unwrap();
        assert_eq!(linux.version_info.as_deref(), Some("v7.1.1"));
        assert!(linux.external_refs[0].reference_locator.ends_with("@kc"));
        assert!(linux.comment.as_deref().unwrap().contains("kc"));
        // No digest is claimed for a source tree — a commit is its content identity,
        // and a sha256 field would have to be invented.
        assert!(linux.checksums.is_empty());
    }
}
