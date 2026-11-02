//! Distro layers: which archive serves a suite, how its releases are verified, and
//! what an image of that distribution adds to the shared base set.
//!
//! A `distros/<name>.toml` layer states one distribution's archive facts. A recipe
//! never names it. The recipe names a suite, and resolution finds the one layer whose
//! [`suites`](DistroLayer::suites) contains that suite. Debian codenames and Ubuntu
//! codenames do not collide, and [`ConfigRoot::distro_for_suite`] refuses a suite that
//! two layers claim, so that property is enforced rather than assumed.
//!
//! The layer is the one place the build learns where a suite lives:
//!
//! - The archive every build root and the image are provisioned from, pocket by pocket.
//! - The components those roots resolve against.
//! - The keyring that verifies each release during the build.
//! - The apt sources the finished image offers, and the keyring they name.
//!
//! Pure and data-only. Nothing here touches the network or the filesystem, so the
//! pocket set and the validation rules are unit-tested on any host.
//!
//! [`ConfigRoot::distro_for_suite`]: crate::loader::ConfigRoot::distro_for_suite

use crate::error::ConfigError;
use crate::model::PackageEntry;
use serde::{Deserialize, Serialize};
use std::fmt;

/// The program that builds an image's initramfs. One is installed per image, because
/// the two packages conflict.
///
/// A distro layer names its default in
/// [`initramfs_generator`](DistroLayer::initramfs_generator), and a recipe can pick the
/// other one. The resolved choice is
/// [`ResolvedImage::initramfs_generator`](crate::model::ResolvedImage::initramfs_generator).
/// Its package joins the rootfs set from the axis, never from a layer's package list,
/// so an image cannot end up asking for both.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum InitramfsGenerator {
    /// `initramfs-tools`: `mkinitramfs`, driven by `update-initramfs`.
    InitramfsTools,
    /// `dracut`, driven by its own kernel hook.
    Dracut,
}

impl InitramfsGenerator {
    /// The package that installs this generator. It is also the name a layer and a
    /// recipe write.
    pub fn package(self) -> &'static str {
        match self {
            InitramfsGenerator::InitramfsTools => "initramfs-tools",
            InitramfsGenerator::Dracut => "dracut",
        }
    }
}

impl fmt::Display for InitramfsGenerator {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.package())
    }
}

/// One `distros/<name>.toml`: a distribution's archive facts, its default account and
/// initramfs generator, and the packages its images add to the base set.
///
/// Validated by [`validate`](Self::validate) before resolution reads any of it. A
/// malformed URL or component therefore fails on the layer that contains it, not in a
/// bootstrap.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DistroLayer {
    /// The distribution's name for a person, such as `Debian`.
    pub description: String,
    /// The suites this layer's archive facts hold for: codenames, and symbolic names
    /// where the archive publishes them.
    ///
    /// Enumerated, never a pattern. Whether a suite exists, and whether it is served
    /// from the same archive as its neighbours, is a fact about one archive at one
    /// time. A new release is therefore one line here, written by someone who checked.
    pub suites: Vec<String>,
    /// The suites among [`suites`](Self::suites) that publish their release pocket and
    /// nothing else, such as Debian's unstable.
    ///
    /// Offering a pocket the archive never serves ships an image whose every
    /// `apt update` reports it missing. Leaving out one that exists costs that image
    /// its security updates. Each suite here is a statement that the first failure is
    /// the one that applies.
    #[serde(default)]
    pub rolling: Vec<String>,
    /// The archive serving each suite's release pocket, as an `http` or `https` URL with
    /// no trailing `/`.
    pub mirror: String,
    /// The point-in-time copy of [`mirror`](Self::mirror). A captured snapshot
    /// timestamp is appended as `<snapshot>/<timestamp>/`. Absent for a distribution
    /// with no such service, and a build that asks for a snapshot then fails.
    #[serde(default)]
    pub snapshot: Option<String>,
    /// The pockets beside the release one, in the order the image's apt sources list
    /// them. Each is solved at build time as well as offered on the board.
    #[serde(default)]
    pub pockets: Vec<DistroPocket>,
    /// The archive components every build root and image resolves against.
    pub components: Vec<String>,
    /// The subset of [`components`](Self::components) a Linux-libre image resolves
    /// against, containing no nonfree firmware. Absent where the distribution offers no
    /// such subset, and resolution then refuses a libre kernel on it.
    #[serde(default)]
    pub free_components: Option<Vec<String>>,
    /// The keyring verifying each release during the build: a file name under
    /// `blobs/keyrings/`, held to the fingerprint manifest beside it.
    ///
    /// Resolved from the shipped tree only. An overlay copy of an archive keyring would
    /// be a swap of the anchor every package is trusted through.
    pub keyring: String,
    /// The absolute path the image's apt sources name in `Signed-By`. The
    /// distribution's own keyring package installs it, so the board keeps verifying as
    /// the archive rotates keys.
    pub image_keyring: String,
    /// The login the image's account is created under.
    pub default_user: String,
    /// The initramfs generator an image of this distribution installs unless its recipe
    /// picks the other one.
    pub initramfs_generator: InitramfsGenerator,
    /// Packages an image of this distribution adds to the base set, merged after the
    /// base layer's.
    #[serde(default)]
    pub packages: Vec<PackageEntry>,
    /// Packages an image of this distribution never installs, unioned into the build's
    /// exclude set.
    #[serde(default)]
    pub exclude: Vec<String>,
}

/// One pocket beside a suite's release pocket: `-updates`, `-security`, `-backports`.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DistroPocket {
    /// Appended to the suite's codename to name the pocket, such as `-updates`.
    pub suffix: String,
    /// The archive serving this pocket, where it is not the release pocket's
    /// [`mirror`](DistroLayer::mirror).
    #[serde(default)]
    pub mirror: Option<String>,
    /// The point-in-time copy of this pocket's archive, where it is not the release
    /// pocket's [`snapshot`](DistroLayer::snapshot).
    #[serde(default)]
    pub snapshot: Option<String>,
}

/// One archive pocket a build resolves against and an image's apt sources offer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Pocket {
    /// The suite as the archive names this pocket: `forky`, `forky-updates`.
    pub suite: String,
    /// The archive serving it.
    pub mirror: String,
    /// The point-in-time copy of that archive, when the distribution has one.
    pub snapshot: Option<String>,
}

/// The distribution a build's suite resolved to, reduced to what the engine reads.
///
/// It serves [`ResolvedBuild::packaging_suite`](crate::model::ResolvedBuild::packaging_suite),
/// which on an image build is the image's suite. Every root a build provisions draws
/// from it, so the compiler, the archiver and the image's userland come from one
/// archive.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ResolvedDistro {
    /// The layer's stem, such as `debian`. It also names the image's apt sources file,
    /// `/etc/apt/sources.list.d/<name>.sources`.
    pub name: String,
    /// The pockets the suite publishes, the release pocket first. Never empty.
    pub pockets: Vec<Pocket>,
    /// The archive components every build root resolves against. An image's own set is
    /// [`ResolvedImage::components`](crate::model::ResolvedImage::components), which
    /// narrows to the free subset on a Linux-libre build.
    pub components: Vec<String>,
    /// The keyring verifying each release during the build, a file name under
    /// `blobs/keyrings/`.
    pub keyring: String,
    /// The path the image's apt sources name in `Signed-By`.
    pub image_keyring: String,
}

impl ResolvedDistro {
    /// The suite's release pocket, which every bootstrap is configured around.
    pub fn release(&self) -> &Pocket {
        // `DistroLayer::resolve` always emits the release pocket first.
        &self.pockets[0]
    }
}

impl DistroLayer {
    /// Whether this layer serves `suite`. Compared verbatim, since a suite is an
    /// archive's own name for itself.
    pub fn serves(&self, suite: &str) -> bool {
        self.suites.iter().any(|s| s == suite)
    }

    /// The pockets `suite` publishes, the release pocket first and then
    /// [`pockets`](Self::pockets) in order. A [`rolling`](Self::rolling) suite
    /// publishes its release pocket alone.
    pub fn pockets_for(&self, suite: &str) -> Vec<Pocket> {
        let release = Pocket {
            suite: suite.to_string(),
            mirror: self.mirror.clone(),
            snapshot: self.snapshot.clone(),
        };
        if self.rolling.iter().any(|s| s == suite) {
            return vec![release];
        }
        std::iter::once(release)
            .chain(self.pockets.iter().map(|p| Pocket {
                suite: format!("{suite}{}", p.suffix),
                mirror: p.mirror.clone().unwrap_or_else(|| self.mirror.clone()),
                snapshot: p.snapshot.clone().or_else(|| self.snapshot.clone()),
            }))
            .collect()
    }

    /// The resolved form for a build of `suite`, named `name`.
    ///
    /// Call it on a layer that passed [`validate`](Self::validate) and serves `suite`.
    /// Resolution does both through
    /// [`ConfigRoot::distro_for_suite`](crate::loader::ConfigRoot::distro_for_suite).
    pub fn resolve(&self, name: &str, suite: &str) -> ResolvedDistro {
        ResolvedDistro {
            name: name.to_string(),
            pockets: self.pockets_for(suite),
            components: self.components.clone(),
            keyring: self.keyring.clone(),
            image_keyring: self.image_keyring.clone(),
        }
    }

    /// The components an image resolves against: the full set, or the free subset for
    /// a Linux-libre one. `None` when the build is libre and this distribution offers no
    /// free subset.
    pub fn image_components(&self, libre: bool) -> Option<&[String]> {
        match libre {
            false => Some(&self.components),
            true => self.free_components.as_deref(),
        }
    }

    /// Refuse a layer whose facts a bootstrap or an apt sources file could not hold.
    /// `name` is the layer's stem, which every failure names.
    ///
    /// The checks catch the typos that would otherwise surface as a bootstrap failure or
    /// on a board whose `apt update` errors:
    ///
    /// - Every suite and pocket name is a bare archive token, and none repeats.
    /// - Every rolling suite is one the layer serves.
    /// - Every URL is `http` or `https`, with no whitespace and no trailing `/`.
    /// - Every component is a bare token, and the free set is a non-empty subset.
    /// - The keyring is a bare file name, and the image keyring an absolute path.
    /// - The default user is a login `useradd` accepts.
    pub fn validate(&self, name: &str) -> Result<(), ConfigError> {
        let bad = |field: &'static str, value: &str, why: &'static str| {
            Err(ConfigError::DistroField {
                distro: name.to_string(),
                field,
                value: value.to_string(),
                why,
            })
        };
        if self.description.trim().is_empty() {
            return bad("description", &self.description, "is empty");
        }
        if self.suites.is_empty() {
            return bad("suites", "[]", "lists no suite");
        }
        let mut seen = std::collections::BTreeSet::new();
        for suite in &self.suites {
            check_suite(suite)?;
            if !seen.insert(suite.as_str()) {
                return bad("suites", suite, "is listed twice");
            }
        }
        for suite in &self.rolling {
            if !self.serves(suite) {
                return bad("rolling", suite, "is not one of the layer's suites");
            }
        }
        check_url(name, "mirror", &self.mirror)?;
        if let Some(snapshot) = &self.snapshot {
            check_url(name, "snapshot", snapshot)?;
        }
        let mut suffixes = std::collections::BTreeSet::new();
        for pocket in &self.pockets {
            let rest = pocket.suffix.strip_prefix('-').unwrap_or_default();
            if rest.is_empty() || !rest.chars().all(is_suite_char) {
                return bad(
                    "pockets.suffix",
                    &pocket.suffix,
                    "must be a `-` followed by suite characters",
                );
            }
            if !suffixes.insert(pocket.suffix.as_str()) {
                return bad("pockets.suffix", &pocket.suffix, "is listed twice");
            }
            if let Some(mirror) = &pocket.mirror {
                check_url(name, "pockets.mirror", mirror)?;
            }
            if let Some(snapshot) = &pocket.snapshot {
                check_url(name, "pockets.snapshot", snapshot)?;
            }
        }
        if self.components.is_empty() {
            return bad("components", "[]", "lists no component");
        }
        for component in &self.components {
            if !is_archive_token(component) {
                return bad("components", component, "is not a bare archive token");
            }
        }
        if let Some(free) = &self.free_components {
            if free.is_empty() {
                return bad("free_components", "[]", "lists no component");
            }
            for component in free {
                if !self.components.contains(component) {
                    return bad(
                        "free_components",
                        component,
                        "is not one of the layer's components",
                    );
                }
            }
        }
        if !is_file_stem(&self.keyring) {
            return bad("keyring", &self.keyring, "must be a bare file name");
        }
        if !self.image_keyring.starts_with('/') || !is_archive_token(&self.image_keyring) {
            return bad(
                "image_keyring",
                &self.image_keyring,
                "must be an absolute path with no whitespace",
            );
        }
        check_user_name(&self.default_user)
            .or_else(|why| bad("default_user", &self.default_user, why))?;
        for entry in &self.packages {
            entry.validate(&format!("distro '{name}'"))?;
        }
        Ok(())
    }
}

/// Reject a suite that is not a bare archive token. The suite becomes an apt sources
/// entry and the path a bootstrap fetches under. It must therefore start with an
/// alphanumeric and be drawn from `[A-Za-z0-9._-]`. That shape also keeps a leading `-`
/// from ever reaching a command line as an option.
///
/// Shape only. Whether any distro layer serves the suite is
/// [`ConfigRoot::distro_for_suite`](crate::loader::ConfigRoot::distro_for_suite)'s
/// question.
pub fn check_suite(suite: &str) -> Result<(), ConfigError> {
    let mut chars = suite.chars();
    let ok =
        matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric()) && chars.all(is_suite_char);
    if ok {
        Ok(())
    } else {
        Err(ConfigError::InvalidField {
            what: "suite",
            value: suite.to_string(),
            why: "expected a suite codename: a bare token in [A-Za-z0-9._-] starting with \
                  an alphanumeric",
        })
    }
}

/// Whether `c` may appear in a suite name.
fn is_suite_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-')
}

/// One field of an apt sources entry: non-empty printable ASCII with no whitespace and
/// no `[` or `]`, which a one-line entry would read as its option block.
fn is_archive_token(v: &str) -> bool {
    !v.is_empty()
        && v.chars()
            .all(|c| c.is_ascii_graphic() && c != '[' && c != ']')
}

/// A portable file name: non-empty, drawn from `[A-Za-z0-9._-]`, and not `.` or `..`.
/// It cannot name a file outside the directory it is joined onto.
fn is_file_stem(v: &str) -> bool {
    !v.is_empty()
        && v != "."
        && v != ".."
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// Refuse a URL a bootstrap and an apt sources file could not both hold.
fn check_url(distro: &str, field: &'static str, url: &str) -> Result<(), ConfigError> {
    let why = if !(url.starts_with("http://") || url.starts_with("https://")) {
        Some("must be an http or https URL")
    } else if !is_archive_token(url) {
        Some("must contain no whitespace or brackets")
    } else if url.ends_with('/') {
        Some("must not end in `/`, which the build appends itself")
    } else {
        None
    };
    match why {
        None => Ok(()),
        Some(why) => Err(ConfigError::DistroField {
            distro: distro.to_string(),
            field,
            value: url.to_string(),
            why,
        }),
    }
}

/// Whether `name` is a login `useradd` creates without `--badname`: a lowercase letter
/// or `_`, then lowercase letters, digits, `_` and `-`, 32 characters at most. `root`
/// is refused, since the image's account is a second, unprivileged one.
fn check_user_name(name: &str) -> Result<(), &'static str> {
    let mut chars = name.chars();
    let shape = matches!(chars.next(), Some(c) if c.is_ascii_lowercase() || c == '_')
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '_' | '-'));
    if !shape || name.len() > 32 {
        Err("must be a lowercase login of at most 32 characters")
    } else if name == "root" {
        Err("must not be root")
    } else {
        Ok(())
    }
}

/// A minimal `distros/debian.toml` for a test's config tree: the Debian suites the
/// tests name, no packages of its own, and initramfs-tools as the generator.
#[cfg(test)]
pub(crate) const TEST_DEBIAN: &str = r#"
description = "Debian"
suites = ["bookworm", "trixie", "forky", "sid", "oldstable", "stable", "testing", "unstable"]
rolling = ["sid", "unstable"]
mirror = "http://deb.debian.org/debian"
snapshot = "https://snapshot.debian.org/archive/debian"
components = ["main", "contrib", "non-free", "non-free-firmware"]
free_components = ["main"]
keyring = "debian-archive-keyring.gpg"
image_keyring = "/usr/share/keyrings/debian-archive-keyring.gpg"
default_user = "debian"
initramfs_generator = "initramfs-tools"
[[pockets]]
suffix = "-updates"
[[pockets]]
suffix = "-security"
mirror = "http://deb.debian.org/debian-security"
snapshot = "https://snapshot.debian.org/archive/debian-security"
"#;

/// A minimal `distros/ubuntu.toml` for a test's config tree: one suite, no free
/// component set, a package and an exclude of its own, and dracut as the generator.
#[cfg(test)]
pub(crate) const TEST_UBUNTU: &str = r#"
description = "Ubuntu"
suites = ["resolute"]
mirror = "http://archive.ubuntu.com/ubuntu"
snapshot = "https://snapshot.ubuntu.com/ubuntu"
components = ["main", "restricted", "universe", "multiverse"]
keyring = "ubuntu-archive-keyring.gpg"
image_keyring = "/usr/share/keyrings/ubuntu-archive-keyring.gpg"
default_user = "ubuntu"
initramfs_generator = "dracut"
packages = ["ubuntu-only"]
exclude = ["ubuntu-excluded"]
[[pockets]]
suffix = "-updates"
[[pockets]]
suffix = "-security"
mirror = "http://security.ubuntu.com/ubuntu"
"#;

/// Write [`TEST_DEBIAN`] into a test config tree rooted at `root`.
#[cfg(test)]
pub(crate) fn write_test_distro(root: &std::path::Path) {
    std::fs::create_dir_all(root.join("distros")).unwrap();
    std::fs::write(root.join("distros/debian.toml"), TEST_DEBIAN).unwrap();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layer(text: &str) -> DistroLayer {
        toml::from_str(text).expect("the fixture parses")
    }

    fn debian() -> DistroLayer {
        layer(
            r#"
            description = "Debian"
            suites = ["trixie", "forky", "sid", "unstable"]
            rolling = ["sid", "unstable"]
            mirror = "http://deb.debian.org/debian"
            snapshot = "https://snapshot.debian.org/archive/debian"
            components = ["main", "contrib", "non-free", "non-free-firmware"]
            free_components = ["main"]
            keyring = "debian-archive-keyring.gpg"
            image_keyring = "/usr/share/keyrings/debian-archive-keyring.gpg"
            default_user = "debian"
            initramfs_generator = "initramfs-tools"
            [[pockets]]
            suffix = "-updates"
            [[pockets]]
            suffix = "-security"
            mirror = "http://deb.debian.org/debian-security"
            snapshot = "https://snapshot.debian.org/archive/debian-security"
            "#,
        )
    }

    #[test]
    fn a_released_suite_publishes_every_pocket_in_order() {
        let p = debian().pockets_for("forky");
        assert_eq!(
            p.iter().map(|p| p.suite.as_str()).collect::<Vec<_>>(),
            ["forky", "forky-updates", "forky-security"]
        );
        // A pocket without its own archive is served by the release pocket's, and the
        // security pocket by the archive it names.
        assert_eq!(p[1].mirror, "http://deb.debian.org/debian");
        assert_eq!(p[2].mirror, "http://deb.debian.org/debian-security");
        assert_eq!(
            p[1].snapshot.as_deref(),
            Some("https://snapshot.debian.org/archive/debian")
        );
        assert_eq!(
            p[2].snapshot.as_deref(),
            Some("https://snapshot.debian.org/archive/debian-security")
        );
    }

    #[test]
    fn a_rolling_suite_publishes_only_its_release_pocket() {
        // Probed against the live archive: `sid` is 200, and `sid-security` and
        // `sid-updates` are both 404. Emitting them shipped an image that errored on two
        // sources every time apt ran.
        for rolling in ["sid", "unstable"] {
            let p = debian().pockets_for(rolling);
            assert_eq!(p.len(), 1, "{rolling}");
            assert_eq!(p[0].suite, rolling);
        }
    }

    #[test]
    fn the_resolved_form_lists_the_release_pocket_first() {
        let r = debian().resolve("debian", "trixie");
        assert_eq!(r.name, "debian");
        assert_eq!(r.release().suite, "trixie");
        assert_eq!(r.release().mirror, "http://deb.debian.org/debian");
        assert_eq!(r.pockets.len(), 3);
    }

    #[test]
    fn a_libre_image_takes_the_free_subset_or_nothing() {
        let d = debian();
        assert_eq!(d.image_components(false).unwrap().len(), 4);
        assert_eq!(d.image_components(true).unwrap(), ["main"]);
        let mut no_free = debian();
        no_free.free_components = None;
        assert!(no_free.image_components(true).is_none());
        assert!(no_free.image_components(false).is_some());
    }

    #[test]
    fn the_generator_is_named_by_its_package() {
        assert_eq!(
            InitramfsGenerator::InitramfsTools.package(),
            "initramfs-tools"
        );
        assert_eq!(InitramfsGenerator::Dracut.package(), "dracut");
        assert_eq!(InitramfsGenerator::Dracut.to_string(), "dracut");
        let parsed: InitramfsGenerator = toml::Value::String("initramfs-tools".into())
            .try_into()
            .unwrap();
        assert_eq!(parsed, InitramfsGenerator::InitramfsTools);
    }

    #[test]
    fn a_well_formed_layer_validates() {
        debian().validate("debian").unwrap();
    }

    #[test]
    fn a_malformed_layer_names_the_field_that_is_wrong() {
        let field_of = |l: DistroLayer| match l.validate("x") {
            Err(ConfigError::DistroField { field, .. }) => field,
            other => panic!("expected a DistroField refusal, got {other:?}"),
        };
        let mut l = debian();
        l.suites.push("forky".into());
        assert_eq!(field_of(l), "suites");
        let mut l = debian();
        l.rolling = vec!["experimental".into()];
        assert_eq!(field_of(l), "rolling");
        let mut l = debian();
        l.mirror = "http://deb.debian.org/debian/".into();
        assert_eq!(field_of(l), "mirror");
        let mut l = debian();
        l.mirror = "ftp://deb.debian.org/debian".into();
        assert_eq!(field_of(l), "mirror");
        let mut l = debian();
        l.snapshot = Some("https://snapshot debian".into());
        assert_eq!(field_of(l), "snapshot");
        let mut l = debian();
        l.pockets[0].suffix = "updates".into();
        assert_eq!(field_of(l), "pockets.suffix");
        let mut l = debian();
        l.pockets[1].suffix = "-updates".into();
        assert_eq!(field_of(l), "pockets.suffix");
        let mut l = debian();
        l.components.push("non free".into());
        assert_eq!(field_of(l), "components");
        let mut l = debian();
        l.free_components = Some(vec!["restricted".into()]);
        assert_eq!(field_of(l), "free_components");
        let mut l = debian();
        l.keyring = "../debian-archive-keyring.gpg".into();
        assert_eq!(field_of(l), "keyring");
        let mut l = debian();
        l.image_keyring = "usr/share/keyrings/x.gpg".into();
        assert_eq!(field_of(l), "image_keyring");
        for user in ["Debian", "root", "9lives", "a b", ""] {
            let mut l = debian();
            l.default_user = user.into();
            assert_eq!(field_of(l), "default_user", "{user:?}");
        }
    }

    #[test]
    fn a_suite_must_be_a_bare_token() {
        for ok in ["forky", "trixie", "sid", "noble-proposed", "v1.0_x"] {
            assert!(check_suite(ok).is_ok(), "{ok}");
        }
        for bad in ["", "-forky", ".forky", "for ky", "forky/updates", "fork[y]"] {
            assert!(
                matches!(
                    check_suite(bad),
                    Err(ConfigError::InvalidField { what: "suite", .. })
                ),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn an_unknown_key_fails_the_parse() {
        let text = r#"
            description = "x"
            suites = ["a"]
            mirror = "http://a"
            components = ["main"]
            keyring = "k.gpg"
            image_keyring = "/k.gpg"
            default_user = "u"
            initramfs_generator = "dracut"
            mirorr = "http://b"
        "#;
        assert!(toml::from_str::<DistroLayer>(text).is_err());
    }
}
