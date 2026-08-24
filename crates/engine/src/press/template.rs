//! Templated tree additions: the `{{image.<name>}}` substitution a `.tmpl`
//! addition receives at press time, and the closed set of names it may draw on.
//!
//! An addition is normally a byte-for-byte copy, which cannot express a file
//! whose content depends on the image it lands in. The values that matter there
//! are the ones an operator *cannot* produce by hand — above all the partition
//! and filesystem identifiers, which [`ImageIdentity`] derives from the recipe,
//! so naming one in a config file would otherwise mean pressing the image,
//! reading its GPT back, editing, and pressing again.
//!
//! The vocabulary is exactly **the image's identity**: every name here is a
//! field of the `/etc/boot2deb/image.toml` the tree already carries
//! ([`SystemIdentity`]) or of the identifiers stamped into its GPT and
//! superblock ([`ImageIdentity`]). That is what the `image.` namespace states,
//! and it is why the set is closed: a template draws on what the image knows
//! about itself, not on the press's environment.
//!
//! The namespace is also what keeps the substitution safe to run over a file
//! that carries braces of its own. `{{` is claimed **only** when `image.`
//! follows it, so a Go, Helm, or Jinja template shipped as data passes through
//! untouched; inside the namespace the rules are strict, and an unknown name is
//! an error at press time rather than an empty string in the shipped image.

use crate::error::EngineError;
use crate::image::ImageIdentity;
use boot2deb_core::provenance::SystemIdentity;

/// Source-file suffix that marks an addition as a template. A `--copy-tree`
/// entry's destination drops it (`site.conf.tmpl` lands as `site.conf`), which
/// also gives the escape for shipping a literal one: name it `site.tmpl.tmpl`.
pub const TEMPLATE_SUFFIX: &str = ".tmpl";

/// The opening delimiter of a reference. Claimed only when [`NAMESPACE`]
/// follows it.
const OPEN: &str = "{{";

/// The closing delimiter of a reference.
const CLOSE: &str = "}}";

/// The one namespace the substitution claims, dot included.
const NAMESPACE: &str = "image.";

/// Largest template a press will read.
///
/// A template is a config file, and unlike a plain `--copy` — whose bytes stay
/// on the host in a [`FileRange`](ferrosys::FileRange) until the formatter
/// places them — a template must be read into memory to be parsed. The cap
/// keeps a mis-named multi-gigabyte payload from being loaded as one, and says
/// so instead.
pub const MAX_TEMPLATE_BYTES: u64 = 1 << 20;

/// One name the `image.` namespace admits.
///
/// Every variant resolves from the image's own identity — see
/// [`ImageFacts::new`] for which document each is read out of. The set is
/// closed by design: a name outside it is a press-time error listing the whole
/// vocabulary, because a silently empty substitution would ship a broken config
/// that only fails on the board.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageValue {
    /// The hostname this unit will answer to — the `--hostname` seed key when
    /// the press names one, else the recipe's. The seed wins because it is what
    /// the booted system ends up with: a template that baked the recipe's
    /// default would ship a file disagreeing with the machine reading it.
    Hostname,
    /// Device slug (`turing-rk1`).
    Device,
    /// The board's human-readable description.
    Description,
    /// Debian architecture (`arm64`).
    Arch,
    /// SoC slug (`rk3588`).
    Soc,
    /// Boot method (`rockchip-rkbin`).
    BootMethod,
    /// Debian suite (`forky`).
    Suite,
    /// Image layout (`combined` / `split`).
    Layout,
    /// Kernel definition id (`rk3588-mainline-7.2`).
    Kernel,
    /// The build point this image was pressed from (`turing-rk1/forky`).
    Recipe,
    /// The rootfs partition's PARTUUID, hyphenated — the form `root=PARTUUID=`
    /// and `/etc/fstab` take.
    RootfsPartuuid,
    /// The rootfs ext4 superblock UUID, hyphenated — the form `UUID=` takes.
    RootfsUuid,
    /// The seed partition's PARTUUID, hyphenated.
    SeedPartuuid,
    /// The GPT header's disk GUID, hyphenated.
    DiskGuid,
}

impl ImageValue {
    /// Every admitted name, in the order the error message lists them.
    pub const ALL: &'static [ImageValue] = &[
        ImageValue::Hostname,
        ImageValue::Device,
        ImageValue::Description,
        ImageValue::Arch,
        ImageValue::Soc,
        ImageValue::BootMethod,
        ImageValue::Suite,
        ImageValue::Layout,
        ImageValue::Kernel,
        ImageValue::Recipe,
        ImageValue::RootfsPartuuid,
        ImageValue::RootfsUuid,
        ImageValue::SeedPartuuid,
        ImageValue::DiskGuid,
    ];

    /// The name as a template spells it, without the `image.` namespace.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            ImageValue::Hostname => "hostname",
            ImageValue::Device => "device",
            ImageValue::Description => "description",
            ImageValue::Arch => "arch",
            ImageValue::Soc => "soc",
            ImageValue::BootMethod => "boot_method",
            ImageValue::Suite => "suite",
            ImageValue::Layout => "layout",
            ImageValue::Kernel => "kernel",
            ImageValue::Recipe => "recipe",
            ImageValue::RootfsPartuuid => "rootfs_partuuid",
            ImageValue::RootfsUuid => "rootfs_uuid",
            ImageValue::SeedPartuuid => "seed_partuuid",
            ImageValue::DiskGuid => "disk_guid",
        }
    }

    /// The value for a name, or `None` when the name is outside the set.
    fn parse(name: &str) -> Option<ImageValue> {
        ImageValue::ALL.iter().copied().find(|v| v.as_str() == name)
    }

    /// The vocabulary as one line, for an error that has to name it.
    fn vocabulary() -> String {
        ImageValue::ALL
            .iter()
            .map(|v| format!("{NAMESPACE}{}", v.as_str()))
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// The image's identity, resolved to the strings a template expands to.
///
/// Built once per press and shared by every template in it, so two additions
/// cannot disagree about the image they are being placed into.
#[derive(Debug, Clone)]
pub struct ImageFacts {
    hostname: String,
    device: String,
    description: String,
    arch: String,
    soc: String,
    boot_method: String,
    suite: String,
    layout: String,
    kernel: String,
    recipe: String,
    rootfs_partuuid: String,
    rootfs_uuid: String,
    seed_partuuid: String,
    disk_guid: String,
}

impl ImageFacts {
    /// Resolve the vocabulary from the two documents that own it.
    ///
    /// `identity` is the tree's own `/etc/boot2deb/image.toml`, read back out of
    /// the entry list being merged — so a template sees what the image says
    /// about itself rather than a second copy of the build's opinion. `image`
    /// carries the identifiers that exist only in the GPT and the superblock,
    /// and `recipe` the build point, which the identity document does not
    /// record (it states the axes the reference resolves to instead).
    ///
    /// `seed_hostname` is the press's `--hostname`, and supersedes the identity
    /// document's: the seed is applied at first boot, so it — not the recipe's
    /// default — is the name the running system will answer to.
    #[must_use]
    pub fn new(
        identity: &SystemIdentity,
        image: &ImageIdentity,
        recipe: &str,
        seed_hostname: Option<&str>,
    ) -> ImageFacts {
        ImageFacts {
            hostname: seed_hostname
                .unwrap_or(&identity.image.hostname)
                .to_string(),
            device: identity.image.device.clone(),
            description: identity.image.description.clone(),
            arch: identity.image.arch.clone(),
            soc: identity.image.soc.clone(),
            boot_method: identity.image.boot_method.clone(),
            suite: identity.image.suite.clone(),
            layout: identity.image.layout.clone(),
            kernel: identity.kernel.id.clone(),
            recipe: recipe.to_string(),
            rootfs_partuuid: image.rootfs_partuuid.hyphenated().to_string(),
            rootfs_uuid: image.ext4_uuid.hyphenated().to_string(),
            seed_partuuid: image.seed_partuuid.hyphenated().to_string(),
            disk_guid: image.disk_guid.hyphenated().to_string(),
        }
    }

    /// The string one name expands to.
    fn get(&self, value: ImageValue) -> &str {
        match value {
            ImageValue::Hostname => &self.hostname,
            ImageValue::Device => &self.device,
            ImageValue::Description => &self.description,
            ImageValue::Arch => &self.arch,
            ImageValue::Soc => &self.soc,
            ImageValue::BootMethod => &self.boot_method,
            ImageValue::Suite => &self.suite,
            ImageValue::Layout => &self.layout,
            ImageValue::Kernel => &self.kernel,
            ImageValue::Recipe => &self.recipe,
            ImageValue::RootfsPartuuid => &self.rootfs_partuuid,
            ImageValue::RootfsUuid => &self.rootfs_uuid,
            ImageValue::SeedPartuuid => &self.seed_partuuid,
            ImageValue::DiskGuid => &self.disk_guid,
        }
    }
}

/// One piece of a parsed template.
#[derive(Debug, PartialEq, Eq)]
enum Segment {
    /// Text carried through unchanged — including any `{{` the namespace did
    /// not claim.
    Literal(String),
    /// A reference, replaced by its value when the template is rendered.
    Value(ImageValue),
}

/// A parsed template: the file's text with its references resolved to names.
///
/// Parsing is separate from rendering because the two are checked at different
/// times. The **names** are validated when the addition is collected, on the
/// command line, so a typo fails before any artifact is read; the **values**
/// arrive at merge time, from the tree being assembled. A template that parses
/// therefore always renders.
#[derive(Debug)]
pub struct Template {
    segments: Vec<Segment>,
    /// How many references the text carries, for the press's own report — an
    /// operator who expected four and is told three has found their typo.
    references: usize,
}

impl Template {
    /// Parse `text`, validating every `image.` reference against the closed set.
    ///
    /// A `{{` not followed by `image.` is literal text, so a file that is itself
    /// a template in some other dialect survives the pass intact.
    ///
    /// # Errors
    ///
    /// [`EngineError::PressAddition`] naming `dest` when a reference opens and
    /// never closes, or names something outside the vocabulary.
    pub fn parse(text: &str, dest: &str) -> Result<Template, EngineError> {
        let bad = |detail: String| EngineError::PressAddition {
            dest: dest.to_string(),
            detail,
        };
        let mut segments = Vec::new();
        let mut references = 0;
        let mut literal = String::new();
        let mut rest = text;
        while let Some(at) = rest.find(OPEN) {
            let (before, from_open) = rest.split_at(at);
            let after_open = &from_open[OPEN.len()..];
            // Only `{{image.` is ours. Anything else is text, and the scan
            // resumes *inside* the delimiter so `{{{{image.x}}` still finds it.
            let Some(after_ns) = after_open.trim_start().strip_prefix(NAMESPACE) else {
                literal.push_str(before);
                literal.push_str(OPEN);
                rest = after_open;
                continue;
            };
            let Some(end) = after_ns.find(CLOSE) else {
                return Err(bad(format!(
                    "a reference opens with `{OPEN}{NAMESPACE}` and never closes with `{CLOSE}`"
                )));
            };
            let name = after_ns[..end].trim();
            let Some(value) = ImageValue::parse(name) else {
                return Err(bad(format!(
                    "`{OPEN}{NAMESPACE}{name}{CLOSE}` names nothing this image knows about \
                     itself; the set is {}",
                    ImageValue::vocabulary()
                )));
            };
            literal.push_str(before);
            if !literal.is_empty() {
                segments.push(Segment::Literal(std::mem::take(&mut literal)));
            }
            segments.push(Segment::Value(value));
            references += 1;
            rest = &after_ns[end + CLOSE.len()..];
        }
        literal.push_str(rest);
        if !literal.is_empty() {
            segments.push(Segment::Literal(literal));
        }
        Ok(Template {
            segments,
            references,
        })
    }

    /// How many references this template expands — reported per file, so a
    /// reference that went unrecognized (a namespace typo, which is literal
    /// text by design) is visible rather than silent.
    #[must_use]
    pub fn references(&self) -> usize {
        self.references
    }

    /// The file this template becomes for an image with these facts.
    #[must_use]
    pub fn render(&self, facts: &ImageFacts) -> Vec<u8> {
        let mut out = String::new();
        for segment in &self.segments {
            match segment {
                Segment::Literal(text) => out.push_str(text),
                Segment::Value(value) => out.push_str(facts.get(*value)),
            }
        }
        out.into_bytes()
    }
}

/// The destination a template source lands at: its path with
/// [`TEMPLATE_SUFFIX`] removed, or `None` when the name does not carry one.
#[must_use]
pub fn strip_suffix(name: &str) -> Option<&str> {
    name.strip_suffix(TEMPLATE_SUFFIX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use boot2deb_core::provenance::{IdentityImage, IdentityKernel};

    /// The identity document a boot2deb rootfs carries, as the merge reads it
    /// back out of the tree.
    fn identity_doc() -> SystemIdentity {
        SystemIdentity {
            version: 1,
            image: IdentityImage {
                device: "turing-rk1".into(),
                description: "Turing RK1 (RK3588) compute module".into(),
                arch: "arm64".into(),
                soc: "rk3588".into(),
                boot_method: "rockchip-rkbin".into(),
                board: None,
                suite: "forky".into(),
                features: Vec::new(),
                layout: "combined".into(),
                hostname: "rk1-03".into(),
            },
            kernel: IdentityKernel {
                id: "rk3588-mainline-7.2".into(),
                flavor: "mainline".into(),
                package: None,
                reference: Some("v7.2".into()),
                commit: None,
                patch_series: Vec::new(),
            },
            pressed: None,
        }
    }

    /// The identifiers this build point's image carries.
    fn image_identity() -> ImageIdentity {
        ImageIdentity::derive("turing-rk1/forky", "turing-rk1")
    }

    fn facts() -> ImageFacts {
        ImageFacts::new(&identity_doc(), &image_identity(), "turing-rk1/forky", None)
    }

    fn render(text: &str) -> String {
        String::from_utf8(Template::parse(text, "/etc/t").unwrap().render(&facts())).unwrap()
    }

    /// Every name in the vocabulary resolves, and the identifiers come out in
    /// the hyphenated form `PARTUUID=` and `UUID=` are written with.
    #[test]
    fn every_name_resolves_to_the_images_own_value() {
        assert_eq!(render("{{image.hostname}}"), "rk1-03");
        assert_eq!(render("{{image.device}}"), "turing-rk1");
        assert_eq!(
            render("{{image.description}}"),
            "Turing RK1 (RK3588) compute module"
        );
        assert_eq!(render("{{image.arch}}"), "arm64");
        assert_eq!(render("{{image.soc}}"), "rk3588");
        assert_eq!(render("{{image.boot_method}}"), "rockchip-rkbin");
        assert_eq!(render("{{image.suite}}"), "forky");
        assert_eq!(render("{{image.layout}}"), "combined");
        assert_eq!(render("{{image.kernel}}"), "rk3588-mainline-7.2");
        assert_eq!(render("{{image.recipe}}"), "turing-rk1/forky");

        let identifiers = image_identity();
        for (text, expected) in [
            ("{{image.rootfs_partuuid}}", identifiers.rootfs_partuuid),
            ("{{image.rootfs_uuid}}", identifiers.ext4_uuid),
            ("{{image.seed_partuuid}}", identifiers.seed_partuuid),
            ("{{image.disk_guid}}", identifiers.disk_guid),
        ] {
            let rendered = render(text);
            assert_eq!(rendered, expected.hyphenated().to_string(), "{text}");
            // Lowercase, hyphenated, 36 characters: what the kernel matches a
            // `root=PARTUUID=` against.
            assert_eq!(rendered.len(), 36, "{text}");
            assert!(
                rendered
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "{text}: {rendered}"
            );
        }

        // No name is unreachable: ALL is what the vocabulary error lists, and
        // every entry of it parses back to itself.
        for value in ImageValue::ALL {
            assert_eq!(ImageValue::parse(value.as_str()), Some(*value));
        }
    }

    /// A realistic file: literals kept verbatim, references replaced in place,
    /// surrounding whitespace inside the braces ignored.
    #[test]
    fn a_config_file_renders_with_its_literals_intact() {
        let out = render(
            "# generated by boot2deb press\n\
             node_id  = {{image.hostname}}\n\
             root_dev = PARTUUID={{ image.rootfs_partuuid }}\n\
             built    = {{image.recipe}} ({{image.suite}}/{{image.arch}})\n",
        );
        let uuid = image_identity().rootfs_partuuid.hyphenated().to_string();
        assert_eq!(
            out,
            format!(
                "# generated by boot2deb press\n\
                 node_id  = rk1-03\n\
                 root_dev = PARTUUID={uuid}\n\
                 built    = turing-rk1/forky (forky/arm64)\n"
            )
        );
        assert_eq!(
            Template::parse("a{{image.arch}}b{{image.soc}}", "/etc/t")
                .unwrap()
                .references(),
            2
        );
    }

    /// The namespace is what makes the pass safe over a file that carries
    /// braces of its own: only `image.` is claimed, everything else is text.
    #[test]
    fn foreign_braces_pass_through_untouched() {
        for text in [
            "{{ .Values.replicas }}",               // Helm
            "{{if eq .Kind \"pod\"}}x{{end}}",      // Go
            "{% for x in xs %}{{ x }}{% endfor %}", // Jinja
            "{{}}",
            "{{",
            "a { { image.arch } } b",
        ] {
            assert_eq!(render(text), text, "{text}");
        }
        // A doubled delimiter still finds the real reference inside it, and
        // keeps the outer pair as the text it is.
        assert_eq!(render("{{{{image.arch}}"), "{{arm64");
    }

    /// The seed's hostname is the one a template bakes: it is what the booted
    /// unit will answer to, and the identity document still holds the recipe's.
    #[test]
    fn the_seed_hostname_supersedes_the_recipes() {
        let doc = identity_doc();
        let image = image_identity();
        let seeded = ImageFacts::new(&doc, &image, "turing-rk1/forky", Some("rk1-07"));
        let plain = ImageFacts::new(&doc, &image, "turing-rk1/forky", None);
        let render = |f: &ImageFacts| {
            String::from_utf8(
                Template::parse("{{image.hostname}}", "/etc/t")
                    .unwrap()
                    .render(f),
            )
            .unwrap()
        };
        assert_eq!(render(&seeded), "rk1-07");
        assert_eq!(
            render(&plain),
            "rk1-03",
            "the identity document's own value"
        );
    }

    /// The refusals: an unclosed reference and a name outside the set, both
    /// naming the destination and the vocabulary.
    #[test]
    fn the_refusals_name_the_problem() {
        let err = Template::parse("x {{image.arch", "/etc/site.conf").unwrap_err();
        assert!(err.to_string().contains("never closes"), "{err}");
        assert!(err.to_string().contains("/etc/site.conf"), "{err}");

        let err = Template::parse("{{image.hostnme}}", "/etc/site.conf").unwrap_err();
        let text = err.to_string();
        assert!(text.contains("hostnme"), "{text}");
        assert!(text.contains("image.hostname"), "{text}");
        assert!(text.contains("image.rootfs_partuuid"), "{text}");
    }

    /// The suffix rule, including the escape it gives for shipping a literal
    /// `.tmpl` file.
    #[test]
    fn the_suffix_names_the_destination() {
        assert_eq!(strip_suffix("site.conf.tmpl"), Some("site.conf"));
        assert_eq!(strip_suffix("site.tmpl.tmpl"), Some("site.tmpl"));
        assert_eq!(strip_suffix("site.conf"), None);
        assert_eq!(strip_suffix("tmpl"), None);
    }
}
