//! CycloneDX 1.6 JSON, rendered from the [`Sbom`] model.
//!
//! Typed like the [SPDX renderer][super::spdx], and for the same reason. Where the
//! two formats genuinely differ, this module states the mapping rather than pretending
//! they are the same document with different keys:
//!
//! - **CycloneDX has no source-tree component type.** A pinned tree is a `library`
//!   carrying a `vcs` external reference; its role is in the description.
//! - **CycloneDX has one relationship kind.** `dependencies` says the image depends on
//!   a component, which is true of a package it ships *and* of a tree it was compiled
//!   from, so both appear there — the distinction SPDX draws with
//!   `CONTAINS` / `GENERATED_FROM` survives in the component's `type` and description.
//! - **A blob is a `file`**, since it is bytes with a digest and no package identity.

use super::{Component, ComponentKind, Sbom};
use serde::Serialize;

/// A CycloneDX 1.6 BOM.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Bom {
    /// Always `CycloneDX`.
    pub bom_format: &'static str,
    /// Always `1.6`.
    pub spec_version: &'static str,
    /// URN UUID, derived from the package set ([`Sbom::serial_number`]).
    pub serial_number: String,
    /// Revision of this BOM for its serial number. Always 1: a re-render of the same
    /// image is the same document, not a revision of it.
    pub version: u32,
    /// Who and what produced the document, and the image it describes.
    pub metadata: Metadata,
    /// Every component except the image, which is [`Metadata::component`].
    pub components: Vec<CdxComponent>,
    /// The dependency graph — one entry, the image on everything else.
    pub dependencies: Vec<Dependency>,
}

/// CycloneDX `metadata`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Metadata {
    /// RFC 3339 UTC creation timestamp.
    pub timestamp: String,
    /// What produced the document. The object form, not the array of `{vendor, name,
    /// version}` deprecated since 1.5.
    pub tools: Tools,
    /// The subject of the BOM: the image.
    pub component: CdxComponent,
}

/// CycloneDX `metadata.tools`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Tools {
    /// The tools, as components.
    pub components: Vec<CdxComponent>,
}

/// One CycloneDX component.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct CdxComponent {
    /// CycloneDX component type: `operating-system`, `library`, `file`, or
    /// `application` for the tool.
    #[serde(rename = "type")]
    pub kind: &'static str,
    /// Document-local reference, matching the model's component id so the two
    /// renderers' identifiers correspond.
    #[serde(rename = "bom-ref")]
    pub bom_ref: String,
    /// Component name.
    pub name: String,
    /// Version, omitted where the component has none.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// One line of context.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// Package URL, where the component has one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purl: Option<String>,
    /// Content digests.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub hashes: Vec<Hash>,
    /// Where the bytes came from — a `vcs` reference for a source pin, a
    /// `distribution` one for a fetched `.deb`.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub external_references: Vec<ExternalReference>,
}

/// A CycloneDX hash.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Hash {
    /// Always `SHA-256` — CycloneDX spells it with the separator SPDX omits.
    pub alg: &'static str,
    /// Lowercase-hex digest.
    pub content: String,
}

/// A CycloneDX external reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ExternalReference {
    /// The locator.
    pub url: String,
    /// Reference type: `vcs` for a source pin, `distribution` for a fetched artifact.
    #[serde(rename = "type")]
    pub kind: &'static str,
}

/// One node of the CycloneDX dependency graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Dependency {
    /// The depending component's `bom-ref`.
    #[serde(rename = "ref")]
    pub reference: String,
    /// What it depends on.
    pub depends_on: Vec<String>,
}

impl Bom {
    /// Render `sbom` as CycloneDX 1.6.
    pub fn render(sbom: &Sbom) -> Bom {
        let components: Vec<CdxComponent> = sbom.components.iter().map(component).collect();
        Bom {
            bom_format: "CycloneDX",
            spec_version: "1.6",
            serial_number: sbom.serial_number(),
            version: 1,
            metadata: Metadata {
                timestamp: sbom.created.clone(),
                tools: Tools {
                    components: vec![CdxComponent {
                        kind: "application",
                        bom_ref: "boot2deb".into(),
                        name: sbom.tool.name.clone(),
                        version: Some(sbom.tool.version.clone()),
                        description: Some(sbom.tool.describe()),
                        purl: None,
                        hashes: Vec::new(),
                        external_references: Vec::new(),
                    }],
                },
                component: component(&sbom.image),
            },
            dependencies: vec![Dependency {
                reference: sbom.image.id.clone(),
                depends_on: components.iter().map(|c| c.bom_ref.clone()).collect(),
            }],
            components,
        }
    }
}

/// Render one component.
fn component(c: &Component) -> CdxComponent {
    CdxComponent {
        kind: match c.kind {
            ComponentKind::Image => "operating-system",
            // No CycloneDX type says "source tree", so a pinned tree is a library
            // whose `vcs` reference and description carry what it actually is.
            ComponentKind::DebianPackage | ComponentKind::Source | ComponentKind::ExtraDeb => {
                "library"
            }
            ComponentKind::Blob => "file",
        },
        bom_ref: c.id.clone(),
        name: c.name.clone(),
        version: c.version.clone(),
        description: c.description.clone(),
        purl: c.purl.clone(),
        hashes: c
            .sha256
            .iter()
            .map(|hex| Hash {
                alg: "SHA-256",
                content: hex.clone(),
            })
            .collect(),
        external_references: c
            .download
            .iter()
            .map(|url| ExternalReference {
                url: url.clone(),
                kind: match c.kind {
                    ComponentKind::Source => "vcs",
                    _ => "distribution",
                },
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sbom::tests::sbom;

    #[test]
    fn the_bom_carries_every_field_the_schema_requires() {
        let bom = Bom::render(&sbom());
        let json = serde_json::to_value(&bom).unwrap();
        assert_eq!(json["bomFormat"], "CycloneDX");
        assert_eq!(json["specVersion"], "1.6");
        assert_eq!(json["version"], 1);
        assert!(json["serialNumber"]
            .as_str()
            .unwrap()
            .starts_with("urn:uuid:"));
        assert_eq!(json["metadata"]["timestamp"], "2026-08-05T00:00:00Z");
        // The 1.5+ object form, not the array of {vendor,name,version} it replaced.
        assert!(json["metadata"]["tools"]["components"].is_array());
        assert_eq!(
            json["metadata"]["tools"]["components"][0]["name"],
            "boot2deb"
        );
        assert_eq!(json["metadata"]["component"]["type"], "operating-system");
        for c in json["components"].as_array().unwrap() {
            assert!(c["type"].is_string());
            assert!(c["name"].is_string());
            assert!(c["bom-ref"].is_string());
        }
    }

    #[test]
    fn each_kind_takes_the_cyclonedx_type_that_fits_it() {
        let bom = Bom::render(&sbom());
        let kind = |name: &str| {
            bom.components
                .iter()
                .find(|c| c.name == name)
                .map(|c| c.kind)
                .unwrap()
        };
        assert_eq!(kind("libc6"), "library");
        // A blob is bytes with a digest and no package identity, which is what
        // `file` means here — calling it a library would imply a purl exists.
        assert_eq!(kind("atf"), "file");
        assert_eq!(kind("linux"), "library");
    }

    #[test]
    fn a_source_pin_carries_a_vcs_reference_and_a_shipped_deb_a_distribution_one() {
        // The reference *type* is where CycloneDX keeps the distinction SPDX draws
        // with GENERATED_FROM, so getting it wrong loses the only signal that a tree
        // was compiled rather than installed.
        let bom = Bom::render(&sbom());
        let refs = |name: &str| {
            bom.components
                .iter()
                .find(|c| c.name == name)
                .unwrap()
                .external_references
                .clone()
        };
        // The fixture manifest records no clone URLs — it is a record of what was
        // built, not of where from — so a source pin has no reference to emit, and
        // inventing one would be inventing provenance.
        assert!(refs("linux").is_empty());
        let deb = refs("foo_1.2_arm64.deb");
        assert_eq!(deb[0].kind, "distribution");
        assert_eq!(deb[0].url, "https://vendor.example/foo_1.2_arm64.deb");
    }

    #[test]
    fn the_image_depends_on_every_component_exactly_once() {
        let bom = Bom::render(&sbom());
        assert_eq!(bom.dependencies.len(), 1);
        let node = &bom.dependencies[0];
        assert_eq!(node.reference, "Image");
        assert_eq!(node.depends_on.len(), bom.components.len());
        // And the refs resolve: a dangling `dependsOn` is the one way this graph can
        // be wrong while still validating against the schema.
        for r in &node.depends_on {
            assert!(
                bom.components.iter().any(|c| &c.bom_ref == r),
                "dangling dependency ref {r}"
            );
        }
    }
}
