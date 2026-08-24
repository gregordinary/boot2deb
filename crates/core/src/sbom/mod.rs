//! Software bill of materials: one internal model of what an image is made of, and
//! two renderers over it.
//!
//! Pure — the caller reads the [provenance manifest](crate::provenance) and the solved
//! [manifest](crate::manifest); this decides what the document says. Adding a third
//! format is then a renderer over [`Sbom`] rather than a second traversal of the
//! provenance, which is the whole reason for the intermediate model: SPDX and
//! CycloneDX disagree about nearly every field name and agree about the facts.
//!
//! - [`spdx`] — SPDX 2.3, JSON.
//! - [`cyclonedx`] — CycloneDX 1.6, JSON.
//!
//! Both are typed `Serialize` documents rather than hand-built JSON, so the shape a
//! validator checks is visible in the source and a missing required field is a
//! compile error rather than a rejected document.
//!
//! ## What it claims, and what it does not
//!
//! **Licenses are `NOASSERTION`.** boot2deb records no per-package license, and
//! synthesizing one by reading `/usr/share/doc/*/copyright` out of the rootfs would
//! produce a field that looks authoritative and is not — Debian's copyright files are
//! prose, and a wrong SPDX license identifier in a document consumers scan for license
//! compliance is worse than an honest absence. Recording real license data is a
//! follow-on with its own accuracy question.
//!
//! **The document is deterministic.** Its identity — the SPDX `documentNamespace` and
//! the CycloneDX `serialNumber` — is derived from the solved manifest's digest, so two
//! SBOMs of the same package set are byte-identical rather than differing in a random
//! UUID. The one input that is not content-derived is the creation timestamp, which
//! both formats require and which the caller supplies: pass `SOURCE_DATE_EPOCH` for a
//! reproducible document.

pub mod cyclonedx;
pub mod spdx;

use crate::manifest::Package;
use crate::provenance::ProvenanceManifest;
use std::collections::BTreeMap;

/// What a component is, which decides how each renderer types it and how it relates
/// to the image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ComponentKind {
    /// The image itself — the document's subject.
    Image,
    /// A Debian binary package installed into the rootfs, from the solved manifest.
    DebianPackage,
    /// A pinned source tree the build compiled from. Not *in* the image; the image
    /// was generated from it.
    Source,
    /// A vendored boot blob consumed by the boot chain (rkbin ATF / TPL / BL32),
    /// identified by its sha256 rather than by a version.
    Blob,
    /// A pre-built `.deb` pulled from outside the Debian mirror and content-pinned by
    /// sha256.
    ExtraDeb,
}

/// How a component relates to the image.
///
/// One relation per component rather than a free relationship graph: every component
/// this model can hold relates to the image exactly one way, and a graph would be a
/// mechanism with a single shape flowing through it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Relation {
    /// The image ships these bytes.
    Contains,
    /// The image was built from this, which is not the same as shipping it: a kernel
    /// source tree is compiled into the image, not installed in it.
    GeneratedFrom,
}

/// One thing an image is made of.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Component {
    /// Document-local identifier, unique within one [`Sbom`] and always valid as an
    /// SPDX id (`[a-zA-Z0-9.-]+`). Assigned by [`Sbom::from_provenance`] by position
    /// and kind rather than derived from the name, because a package name may carry
    /// characters SPDX ids forbid (`libstdc++6`).
    pub id: String,
    /// Component name as its ecosystem spells it.
    pub name: String,
    /// Version, where the component has one. A blob has none — its identity is its
    /// digest.
    pub version: Option<String>,
    /// What kind of thing it is.
    pub kind: ComponentKind,
    /// Package URL, where one can be stated exactly. Absent for a blob, which no
    /// package ecosystem names.
    pub purl: Option<String>,
    /// Lowercase-hex sha256 of the component's bytes, where the build pinned one.
    pub sha256: Option<String>,
    /// Where the bytes came from: a `git+<url>@<commit>` VCS locator for a source
    /// pin, an HTTPS URL for a fetched `.deb`. Absent where the build records none —
    /// rendered as `NOASSERTION` by SPDX, which requires the field.
    pub download: Option<String>,
    /// One line of human context, where there is something to say that the fields
    /// above do not carry.
    pub description: Option<String>,
    /// How it relates to the image.
    pub relation: Relation,
}

/// An image's bill of materials, format-independent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sbom {
    /// Document name — the image's artifact stem (`turing-rk1-forky`).
    pub name: String,
    /// Creation timestamp, RFC 3339 UTC. Supplied by the caller because this module
    /// reads no clock; both output formats require it.
    pub created: String,
    /// The builder that produced the image — taken from the provenance manifest, so
    /// the tool credited is the one that built the image and not whichever binary
    /// rendered the document.
    pub tool: Tool,
    /// Content-derived document identity: the solved manifest's sha256. The SPDX
    /// namespace and the CycloneDX serial number are both derived from it, which is
    /// what makes two SBOMs of one package set identical.
    pub content_id: String,
    /// The image the document describes.
    pub image: Component,
    /// Everything else, in a stable order: packages, then sources, then blobs, then
    /// extra `.deb`s.
    pub components: Vec<Component>,
}

impl Sbom {
    /// Build the model from a published provenance manifest and the solved package
    /// manifest it names.
    ///
    /// `name` is the document name — the image's artifact stem. `created` is an
    /// RFC 3339 UTC timestamp ([`crate::datetime::format_rfc3339`]); pass the value of
    /// `SOURCE_DATE_EPOCH` through it for a reproducible document.
    ///
    /// `packages` must be the manifest whose digest the provenance records. Nothing
    /// here re-checks that — the caller reads both files and is the only party that
    /// can — but a mismatch would make the document describe one image's packages
    /// under another's identity.
    ///
    /// `sources` maps a binary package name to the source package it was built from,
    /// for the packages whose source is named separately
    /// ([`weight::source_index`](crate::weight::source_index) builds it from the
    /// published plan). It ties the several binary packages of one source back to the
    /// thing that was built, which no other document boot2deb writes can answer. Empty
    /// is legitimate — an image handed over without its plan — and simply leaves the
    /// attribution out rather than guessing it from a name.
    pub fn from_provenance(
        manifest: &ProvenanceManifest,
        packages: &[Package],
        sources: &BTreeMap<String, String>,
        name: &str,
        created: &str,
    ) -> Sbom {
        let mut components = Vec::with_capacity(packages.len() + 8);
        for (i, p) in packages.iter().enumerate() {
            let source = sources.get(&p.name);
            components.push(Component {
                id: format!("Package-{i}"),
                name: p.name.clone(),
                version: Some(p.version.clone()),
                kind: ComponentKind::DebianPackage,
                purl: Some(deb_purl(
                    &p.name,
                    &p.version,
                    &p.architecture,
                    source.map(String::as_str),
                )),
                sha256: Some(p.sha256.clone()),
                // The archive a package came from is recorded per *repository* in the
                // provenance, and the per-package join lives in the plan document
                // rather than here — so rather than guess a pool URL, the digest is
                // the locator and the download location is not asserted.
                download: None,
                // Said in prose as well as in the purl qualifier, because the two reach
                // different readers: a scanner parses the purl, and a person reading a
                // rendered SBOM sees the comment.
                description: source.map(|s| format!("built from source package {s}")),
                relation: Relation::Contains,
            });
        }
        for (i, src) in source_pins(manifest).into_iter().enumerate() {
            components.push(Component {
                id: format!("Source-{i}"),
                name: src.name,
                version: Some(src.reference.clone()),
                kind: ComponentKind::Source,
                purl: Some(generic_purl(&src.purl_name, &src.commit)),
                sha256: None,
                download: src.url.map(|url| format!("git+{url}@{}", src.commit)),
                description: Some(format!("pinned at {}", src.commit)),
                relation: Relation::GeneratedFrom,
            });
        }
        if let Some(blobs) = &manifest.blobs {
            let pins = [Some(&blobs.atf), Some(&blobs.tpl), blobs.bl32.as_ref()];
            for (i, pin) in pins.into_iter().flatten().enumerate() {
                // A blob pin is `"<file>@sha256:<hex>"`; a malformed one would be a
                // hand-edited manifest, and the whole pin is then the name so nothing
                // is silently dropped from the document.
                let (file, digest) = match pin.rsplit_once("@sha256:") {
                    Some((file, digest)) => (file, Some(digest.to_string())),
                    None => (pin.as_str(), None),
                };
                components.push(Component {
                    id: format!("Blob-{i}"),
                    name: file.to_string(),
                    version: None,
                    kind: ComponentKind::Blob,
                    purl: None,
                    sha256: digest,
                    download: None,
                    description: Some("vendored Rockchip boot blob".into()),
                    relation: Relation::Contains,
                });
            }
        }
        for (i, deb) in manifest.extra_debs.iter().enumerate() {
            let name = deb
                .url
                .as_deref()
                .or(deb.path.as_deref())
                .and_then(|l| l.rsplit('/').next())
                .unwrap_or("extra-deb")
                .to_string();
            components.push(Component {
                id: format!("ExtraDeb-{i}"),
                name,
                version: None,
                kind: ComponentKind::ExtraDeb,
                purl: None,
                sha256: Some(deb.sha256.clone()),
                download: deb.url.clone(),
                description: Some("pre-built .deb from outside the Debian mirror".into()),
                relation: Relation::Contains,
            });
        }

        let image = &manifest.image;
        let mut about = format!(
            "{} image for {} ({}, {}, {})",
            image.suite, image.device, image.arch, image.soc, image.boot_method
        );
        if !image.features.is_empty() {
            about.push_str(&format!(" with features {}", image.features.join(", ")));
        }
        Sbom {
            name: name.to_string(),
            created: created.to_string(),
            tool: tool_identity(manifest),
            content_id: manifest.rootfs.manifest_sha256.clone(),
            image: Component {
                id: "Image".into(),
                name: name.to_string(),
                // The image has no version of its own; the suite and the pins below
                // are what identify it, and inventing one would be a number nothing
                // else in the project uses.
                version: None,
                kind: ComponentKind::Image,
                purl: None,
                sha256: None,
                download: None,
                description: Some(about),
                relation: Relation::Contains,
            },
            components,
        }
    }

    /// The SPDX `documentNamespace`: a URI under the project's own repository,
    /// distinguished by the document name and the solved manifest's digest.
    ///
    /// Content-derived rather than random, so re-rendering an SBOM for one image
    /// yields the same document. SPDX requires the namespace to be unique per
    /// document; two documents that agree on every byte are one document, so
    /// sharing an identity is the correct reading rather than a collision.
    pub fn namespace(&self) -> String {
        format!(
            "https://github.com/gregordinary/boot2deb/spdxdocs/{}-{}",
            self.name, self.content_id
        )
    }

    /// The CycloneDX `serialNumber`: a URN UUID derived from
    /// [`content_id`](Self::content_id), so it is stable for a package set.
    ///
    /// The digest's first 32 hex characters are laid out as a UUID with the version
    /// nibble set to 8 (RFC 9562 custom) and the variant bits to `10x`, which is what
    /// makes the result a *valid* UUID rather than merely UUID-shaped — the format's
    /// pattern is checked by validators.
    pub fn serial_number(&self) -> String {
        // A short or non-hex content id would be a corrupt provenance manifest; pad
        // deterministically rather than panic, since a document is still worth
        // producing and the digest is recorded in the components either way.
        let mut hex: String = self
            .content_id
            .chars()
            .filter(|c| c.is_ascii_hexdigit())
            .map(|c| c.to_ascii_lowercase())
            .take(32)
            .collect();
        while hex.len() < 32 {
            hex.push('0');
        }
        // Force the two high bits of the variant octet to `10`, which leaves that
        // nibble as one of `8`, `9`, `a`, `b` — the RFC 9562 variant every validator's
        // pattern admits.
        let nibble = hex.as_bytes()[16] as char;
        let value = nibble.to_digit(16).unwrap_or(0) as u8;
        let variant = std::char::from_digit(((value & 0x3) | 0x8) as u32, 16).unwrap_or('8');
        format!(
            "urn:uuid:{}-{}-8{}-{}{}-{}",
            &hex[0..8],
            &hex[8..12],
            &hex[13..16],
            variant,
            &hex[17..20],
            &hex[20..32]
        )
    }
}

/// The builder credited as the document's creator.
///
/// Kept as fields rather than one rendered string because the two formats want it
/// differently: SPDX's `creators` takes `Tool: <name>-<version>` and nothing else,
/// while CycloneDX takes a component with a name and a version of its own. A commit
/// and a dirty flag fit in neither, so they ride in the free-text field each format
/// does have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Tool {
    /// Tool name, always `boot2deb`.
    pub name: String,
    /// Crate version the image was built by.
    pub version: String,
    /// Short git commit of that builder's checkout, where it was one.
    pub commit: Option<String>,
    /// Whether that checkout had uncommitted changes — in which case the commit does
    /// not identify the builder, and the document says so rather than implying it does.
    pub dirty: bool,
}

impl Tool {
    /// One line naming the builder as precisely as the manifest allows, for the
    /// free-text field each format carries.
    pub fn describe(&self) -> String {
        let mut out = format!("{} {}", self.name, self.version);
        match (&self.commit, self.dirty) {
            (Some(commit), false) => out.push_str(&format!(" ({commit})")),
            (Some(commit), true) => out.push_str(&format!(" ({commit}, dirty)")),
            (None, true) => out.push_str(" (dirty)"),
            (None, false) => {}
        }
        out
    }
}

/// A pinned source tree, flattened out of the provenance manifest's several
/// per-axis fields into the one shape the model needs.
struct SourcePin {
    /// Axis name for the component (`linux`, `u-boot`, `patches`, `mpp`, …).
    name: String,
    /// The same, in the restricted spelling a purl name takes.
    purl_name: String,
    /// The ref that was pinned.
    reference: String,
    /// The exact commit.
    commit: String,
    /// The clone URL, where the manifest records one.
    url: Option<String>,
}

/// Every source pin the manifest records, in a stable order.
///
/// The provenance manifest records refs and commits but not, for most axes, the clone
/// URL — it is a record of *what* was built, and the URL lives in the lock. So a pin
/// whose URL is unknown still becomes a component, identified by its commit, with no
/// download location asserted. That is the honest shape: naming a repository the
/// document does not record would be inventing provenance.
fn source_pins(manifest: &ProvenanceManifest) -> Vec<SourcePin> {
    let sources = &manifest.sources;
    let mut pins = Vec::new();
    pins.extend(pin(
        "linux",
        sources.kernel_ref.as_ref(),
        sources.kernel_commit.as_ref(),
    ));
    pins.extend(pin(
        "u-boot",
        sources.uboot_ref.as_ref(),
        sources.uboot_commit.as_ref(),
    ));
    // The patch series has a commit and no ref of its own in this document, so the
    // series names stand in for one — which is also what a reader is looking for here.
    if let Some(commit) = &sources.patches_commit {
        pins.push(SourcePin {
            name: "patches".into(),
            purl_name: "patches".into(),
            reference: sources.patch_series.join(", "),
            commit: commit.clone(),
            url: None,
        });
    }
    if let Some(ma) = &sources.media_accel {
        pins.extend(pin("mpp", ma.mpp_ref.as_ref(), ma.mpp_commit.as_ref()));
        pins.extend(pin(
            "librga",
            ma.librga_ref.as_ref(),
            ma.librga_commit.as_ref(),
        ));
        pins.extend(pin(
            "libmali",
            ma.libmali_ref.as_ref(),
            ma.libmali_commit.as_ref(),
        ));
        pins.extend(pin(
            "ffmpeg",
            Some(&ma.ffmpeg_base_ref),
            Some(&ma.ffmpeg_base_commit),
        ));
        pins.extend(pin(
            "ffmpeg-rockchip",
            ma.ffmpeg_rockchip_ref.as_ref(),
            ma.ffmpeg_rockchip_commit.as_ref(),
        ));
    }
    pins
}

/// One source pin, present only when the manifest records both halves of it. An axis
/// with a ref and no commit (or the reverse) is a malformed record, and half a pin is
/// worse than none: it would name a tree without saying which state of it was built.
fn pin(name: &str, reference: Option<&String>, commit: Option<&String>) -> Option<SourcePin> {
    Some(SourcePin {
        name: name.to_string(),
        purl_name: name.to_string(),
        reference: reference?.clone(),
        commit: commit?.clone(),
        url: None,
    })
}

/// The builder credited as the document's creator: the boot2deb that produced the
/// image, from the manifest's own record of it rather than from the running binary.
fn tool_identity(manifest: &ProvenanceManifest) -> Tool {
    let built = &manifest.built_with;
    Tool {
        name: "boot2deb".into(),
        version: built.version.clone(),
        commit: built.commit.clone(),
        dirty: built.dirty,
    }
}

/// A Debian binary package's purl: `pkg:deb/debian/<name>@<version>?arch=<arch>`, with
/// `&upstream=<source>` where the source package is named separately.
///
/// `upstream` is the qualifier Debian-aware scanners read for that relationship, and it
/// is emitted only where the source name differs — absence is how the ecosystem spells
/// "the source carries this package's own name", so writing the redundant case would
/// make the qualifier's absence ambiguous rather than informative.
///
/// Qualifiers are ordered alphabetically, which the purl specification requires of a
/// canonical form and which also keeps two renderings of one package identical.
fn deb_purl(name: &str, version: &str, architecture: &str, source: Option<&str>) -> String {
    let mut purl = format!(
        "pkg:deb/debian/{}@{}?arch={}",
        percent_encode(name),
        percent_encode(version),
        percent_encode(architecture)
    );
    if let Some(source) = source {
        purl.push_str(&format!("&upstream={}", percent_encode(source)));
    }
    purl
}

/// A source tree's purl: `pkg:generic/<name>@<commit>`. `generic` because no package
/// ecosystem publishes these trees — they are git checkouts the build compiled, and
/// the commit is their version.
fn generic_purl(name: &str, commit: &str) -> String {
    format!("pkg:generic/{}@{}", percent_encode(name), commit)
}

/// Percent-encode a purl component: everything outside the unreserved set
/// (`A-Za-z0-9-._~`) is escaped.
///
/// Strictly required rather than cosmetic. A Debian version carries an epoch as
/// `1:2.41-1`, and a bare `:` in a purl version terminates the type; a package name
/// carries `+` (`libstdc++6`), which some readers decode as a space. Encoding to the
/// unreserved set is the canonical purl form and leaves neither ambiguity.
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for byte in s.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char)
            }
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::provenance::ProvenanceManifest;

    /// A package for the fixtures.
    fn pkg(name: &str, version: &str, arch: &str, digest: char) -> Package {
        Package {
            name: name.into(),
            version: version.into(),
            architecture: arch.into(),
            sha256: std::iter::repeat_n(digest, 64).collect(),
        }
    }

    /// The solved package set the fixture manifest describes — deliberately carrying
    /// the two names that break a naive purl: an epoch in the version and a `+` in the
    /// name.
    pub(crate) fn packages() -> Vec<Package> {
        vec![
            pkg("libc6", "2.41-1", "arm64", 'a'),
            pkg("libstdc++6", "1:14.2.0-19", "arm64", 'b'),
        ]
    }

    /// The provenance manifest the SBOM fixtures describe — the one
    /// [`crate::provenance`]'s own tests assemble, so a rendered document is exercised
    /// against a manifest a build actually produces rather than against a hand-written
    /// copy that could drift from the type.
    pub(crate) fn manifest() -> ProvenanceManifest {
        crate::provenance::tests::sample_manifest()
    }

    /// The source attribution a published plan supplies, covering one of the two
    /// packages above and not the other — which is the real shape, since Debian names a
    /// `Source` only where it differs from the binary package's own name.
    pub(crate) fn sources() -> BTreeMap<String, String> {
        [("libstdc++6".to_string(), "gcc-14".to_string())]
            .into_iter()
            .collect()
    }

    /// The fixture SBOM, at a fixed timestamp so the documents are byte-stable.
    pub(crate) fn sbom() -> Sbom {
        Sbom::from_provenance(
            &manifest(),
            &packages(),
            &sources(),
            "turing-rk1-media-accel-forky",
            "2026-08-05T00:00:00Z",
        )
    }

    #[test]
    fn every_kind_of_component_reaches_the_model_exactly_once() {
        let sbom = sbom();
        let of = |kind: ComponentKind| {
            sbom.components
                .iter()
                .filter(|c| c.kind == kind)
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>()
        };
        assert_eq!(of(ComponentKind::DebianPackage), ["libc6", "libstdc++6"]);
        // Ordered as the build point states them, and only the axes this image has:
        // no librga or libmali, because the fixture's SoC declares none.
        assert_eq!(
            of(ComponentKind::Source),
            [
                "linux",
                "u-boot",
                "patches",
                "mpp",
                "librga",
                "libmali",
                "ffmpeg",
                "ffmpeg-rockchip"
            ]
        );
        assert_eq!(of(ComponentKind::Blob), ["atf", "tpl"]);
        assert_eq!(of(ComponentKind::ExtraDeb), ["foo_1.2_arm64.deb"]);
        // Every id is unique and SPDX-legal, which is why they are positional rather
        // than derived from names like `libstdc++6`.
        let mut ids: Vec<&str> = sbom.components.iter().map(|c| c.id.as_str()).collect();
        let count = ids.len();
        ids.sort_unstable();
        ids.dedup();
        assert_eq!(ids.len(), count, "component ids must be unique");
        assert!(sbom.components.iter().all(|c| c
            .id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '-' || ch == '.')));
    }

    /// The several binary packages of one source have nothing tying them to the thing
    /// that was built unless the document says so — which is the whole point of the
    /// field, and the reason it is stated twice: a scanner reads the purl qualifier and
    /// a person reads the comment.
    #[test]
    fn a_package_names_the_source_it_was_built_from_where_that_differs() {
        let sbom = sbom();
        let package = |name: &str| {
            sbom.components
                .iter()
                .find(|c| c.name == name && c.kind == ComponentKind::DebianPackage)
                .unwrap_or_else(|| panic!("a {name} component"))
        };

        let attributed = package("libstdc++6");
        let purl = attributed.purl.as_deref().expect("a package has a purl");
        assert!(
            purl.ends_with("&upstream=gcc-14"),
            "the source belongs in the purl a scanner reads: {purl}"
        );
        assert!(
            purl.starts_with("pkg:deb/debian/libstdc%2B%2B6@1%3A14.2.0-19?arch=arm64"),
            "the qualifier is appended to the canonical form, not spliced into it: {purl}"
        );
        assert_eq!(
            attributed.description.as_deref(),
            Some("built from source package gcc-14")
        );

        // `libc6`'s source carries its own name, which Debian encodes by omitting the
        // field — so the qualifier must be absent rather than redundant, since absence
        // is what a consumer reads as "the same name".
        let plain = package("libc6");
        let purl = plain.purl.as_deref().expect("a package has a purl");
        assert!(!purl.contains("upstream="), "{purl}");
        assert_eq!(plain.description, None);
    }

    #[test]
    fn a_source_is_generated_from_and_everything_else_is_contained() {
        // The distinction is the one substantive claim in the relationship graph: a
        // kernel tree is compiled into the image, not installed in it, and an SBOM
        // that said `CONTAINS` would assert the source is shipped.
        let sbom = sbom();
        for c in &sbom.components {
            let expected = match c.kind {
                ComponentKind::Source => Relation::GeneratedFrom,
                _ => Relation::Contains,
            };
            assert_eq!(c.relation, expected, "{} ({:?})", c.name, c.kind);
        }
    }

    #[test]
    fn purls_encode_the_two_debian_spellings_that_break_a_naive_one() {
        // An epoch's `:` terminates a purl type, and a `+` decodes as a space in some
        // readers — both are ordinary in Debian, so both must round-trip.
        let sbom = sbom();
        let purl = |name: &str| {
            sbom.components
                .iter()
                .find(|c| c.name == name)
                .and_then(|c| c.purl.clone())
                .unwrap()
        };
        assert_eq!(purl("libc6"), "pkg:deb/debian/libc6@2.41-1?arch=arm64");
        // The fixture attributes this one to a source package, so its purl carries the
        // qualifier too — encoding is what is under test here, and the qualifier is part
        // of the string that has to come out right.
        assert_eq!(
            purl("libstdc++6"),
            "pkg:deb/debian/libstdc%2B%2B6@1%3A14.2.0-19?arch=arm64&upstream=gcc-14"
        );
        // A source's version is its commit, which is what a consumer can act on.
        assert_eq!(purl("linux"), "pkg:generic/linux@kc");
    }

    #[test]
    fn the_documents_identity_is_derived_from_the_package_set() {
        // Determinism is the property: re-rendering an SBOM for one image must not
        // produce a document that differs only in a random identifier, or every
        // consumer diffing two SBOMs sees a change that is not one.
        let sbom = sbom();
        assert!(sbom.namespace().ends_with(&sbom.content_id));
        assert_eq!(sbom.serial_number(), sbom.serial_number());

        // And it is a *valid* UUID, not merely UUID-shaped: version nibble 8, variant
        // bits `10x`. Validators check the pattern.
        let urn = sbom.serial_number();
        let uuid = urn.strip_prefix("urn:uuid:").expect("a URN UUID");
        let groups: Vec<&str> = uuid.split('-').collect();
        assert_eq!(
            groups.iter().map(|g| g.len()).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12]
        );
        assert!(uuid.chars().all(|c| c.is_ascii_hexdigit() || c == '-'));
        assert!(groups[2].starts_with('8'), "version nibble: {uuid}");
        assert!(
            ['8', '9', 'a', 'b'].contains(&groups[3].chars().next().unwrap()),
            "variant nibble: {uuid}"
        );

        // A different package set is a different document.
        let mut other = manifest();
        other.rootfs.manifest_sha256 = "f".repeat(64);
        let other =
            Sbom::from_provenance(&other, &packages(), &sources(), "x", "2026-08-05T00:00:00Z");
        assert_ne!(other.serial_number(), sbom.serial_number());
    }

    #[test]
    fn the_creator_is_the_builder_that_made_the_image() {
        // Not the binary rendering the document: an SBOM generated later from a
        // shipped manifest must credit what built the image, or the field is a claim
        // about the wrong machine.
        assert_eq!(sbom().tool.describe(), "boot2deb 0.1.0 (abc1234)");
        let mut dirty = manifest();
        dirty.built_with.dirty = true;
        dirty.built_with.commit = None;
        let sbom = Sbom::from_provenance(&dirty, &[], &sources(), "x", "2026-08-05T00:00:00Z");
        assert_eq!(sbom.tool.describe(), "boot2deb 0.1.0 (dirty)");
        assert_eq!(sbom.tool.version, "0.1.0");
    }
}
