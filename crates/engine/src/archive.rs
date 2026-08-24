//! Asking the configured archives about a recipe's package set, before a build.
//!
//! Two questions, answered separately because they fail differently.
//!
//! [`available`] is the read half of a resolve and nothing more: it downloads the
//! release and the package indexes, projects the names out of them, and stops. No
//! closure is computed, no `.deb` is fetched, and the answer is served for an
//! architecture the host cannot execute — so it costs one index download however many
//! names are asked about, where a resolve per name would re-fetch that index every time.
//! It exists because a top-level include naming nothing fails the *whole* resolve, so a
//! batch of names would report that they were not all there and never which were not.
//!
//! [`closure`] then asks what the names *depend* on. A name being in the archive says
//! nothing about its dependencies being there, and that gap is not hypothetical: a
//! package can be present, install, and leave the rootfs unable to configure anything
//! because a versioned dependency of its own is absent from the suite. Only a resolve
//! sees that, and the resolver reports every refusal it meets rather than the first — so
//! one pass names the whole list a user has to correct.
//!
//! Both are read-only and network-only, and both run before anything is built.

use crate::archfetch::ArchiveFetch;
use crate::bootstrap::components;
use crate::error::EngineError;
use crate::rootfs::{feature_repositories, AptRepo};
use boot2deb_core::model::ImageBuild;
use ferroday_cage::provision::debian::{
    Debian, DebianBuilder, DebianError, DebianEvent, DebianObserver, Priority,
};
use std::path::Path;

/// What the configured archives said about one batch of package names.
///
/// [`present`](Self::present) and [`missing`](Self::missing) partition the query set, so
/// the two together are exactly what was asked. [`provided`](Self::provided) annotates
/// some of `present` rather than adding to it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AvailabilityReport {
    /// Every queried name the archives resolve, sorted — as a real package, as a name
    /// something `Provides`, or as both.
    pub present: Vec<String>,
    /// The providers of each resolvable name something `Provides`, sorted by name.
    ///
    /// An annotation over [`present`](Self::present), not a separate class, because the
    /// two are not exclusive: `dhcpcd` in a modern suite is a real package *and* a name
    /// other packages provide, so a report that split them would have to pick one and be
    /// wrong either way. What this adds is that apt has a choice here — a name with
    /// providers can be satisfied by something other than the package of that name.
    pub provided: Vec<(String, Vec<String>)>,
    /// Names the archives do not offer at all, sorted — the answer the query is for.
    pub missing: Vec<String>,
}

impl AvailabilityReport {
    /// Whether every queried name resolves, directly or as a virtual package.
    pub fn is_complete(&self) -> bool {
        self.missing.is_empty()
    }
}

/// The `Bootstrap` error a provisioner failure becomes, naming what was being done.
///
/// One definition because both questions here fail into the same variant, and a reader
/// comparing two `context` strings should be comparing the strings rather than two
/// spellings of the same closure.
fn bootstrap_failure() -> impl Fn(&str) -> Box<dyn Fn(DebianError) -> EngineError> {
    |context: &str| {
        let context = context.to_string();
        Box::new(move |e: DebianError| EngineError::Bootstrap {
            context: context.clone(),
            message: e.to_string(),
        })
    }
}

/// The archives a build would resolve against, configured but not yet read.
///
/// Shared by both questions deliberately: verifying against a different archive than the
/// one the other question asks — or than the build itself resolves from — would not be a
/// verification. `purpose` names the caller in the one failure raised before a builder
/// exists.
///
/// The build's **local** `.deb` pool is not among them. It holds what a build produces,
/// and nothing has been produced yet.
fn archives<'a>(
    ib: ImageBuild<'a>,
    mirrors: &[String],
    keyring: Option<&Path>,
    apt_sources: &[AptRepo],
    purpose: &str,
) -> Result<DebianBuilder<'a>, EngineError> {
    let ImageBuild { build, image } = ib;
    let (primary, fallbacks) = mirrors
        .split_first()
        .ok_or_else(|| EngineError::Bootstrap {
            context: format!("configure the archive {purpose}"),
            message: "no Debian mirror was resolved".into(),
        })?;

    // No `include`, no `exclude`, no base priority here: what the archives offer is
    // shaped by none of them, and the caller that does resolve a closure adds its own
    // `include`. Nor an identity map or a cache dir — nothing is unpacked, and nothing
    // is downloaded past the indexes.
    let mut b = Debian::builder(&image.suite)
        .architecture(build.arch.debian_arch())
        .components(components(image).split(','))
        // The same transport a build uses, so a feature repository this can reach is
        // one the build can reach and vice versa — see [`ArchiveFetch`].
        .fetcher(Box::new(ArchiveFetch::new()))
        .mirror(primary);
    for fallback in fallbacks {
        b = b.mirror_fallback(fallback);
    }
    // A point-in-time archive's release is expired by design, exactly as in a build.
    if crate::snapshot::has_snapshot(mirrors) {
        b = b.allow_stale_release(true);
    }
    if let Some(keyring) = keyring {
        b = b.keyring(keyring);
    }
    for repo in feature_repositories(apt_sources)? {
        b = b.repository(repo);
    }
    Ok(b)
}

/// Ask the archives a build would resolve against which of `names` they offer.
///
/// The archives are the build's own: the primary mirror with any snapshot backstop,
/// plus every repository the selected features contribute — so a package that exists
/// only in a feature's own repository (Jellyfin's, say) is found where the recipe
/// expects it. The build's **local** `.deb` pool is deliberately not among them: it
/// holds what a build produces, and nothing has been produced yet.
///
/// `mirrors` is the resolved mirror list (primary first) and must be non-empty;
/// `keyring` is the vendored archive keyring, or `None` to fall back to the host apt
/// trust store.
///
/// # Errors
///
/// [`EngineError::Bootstrap`] when the archives cannot be configured, or when the
/// release or an index cannot be fetched or verified — the same failures a resolve's
/// read half has, surfaced before a build rather than during one.
pub fn available(
    ib: ImageBuild,
    mirrors: &[String],
    keyring: Option<&Path>,
    apt_sources: &[AptRepo],
    names: &[String],
) -> Result<AvailabilityReport, EngineError> {
    let ImageBuild { build, image } = ib;
    let fail = bootstrap_failure();
    let mut debian = archives(ib, mirrors, keyring, apt_sources, "availability query")?
        .build()
        .map_err(fail(&format!(
            "configure the {} {} archives",
            build.arch, image.suite
        )))?;

    let available = debian
        .available()
        .map_err(fail("read the archive package indexes"))?;

    let mut report = AvailabilityReport {
        present: Vec::new(),
        provided: Vec::new(),
        missing: Vec::new(),
    };
    for name in names {
        if !available.contains(name) {
            report.missing.push(name.clone());
            continue;
        }
        report.present.push(name.clone());
        // A package that provides its own name — a metapackage over a `-base` split, as
        // `dhcpcd` is — would otherwise be reported as its own alternative, which says
        // nothing. What the annotation is for is the *other* packages that could satisfy
        // the name, so the self-provider is dropped and a name left with none is not
        // annotated at all.
        let providers: Vec<String> = available
            .providers(name)
            .filter(|provider| *provider != name.as_str())
            .map(str::to_string)
            .collect();
        if !providers.is_empty() {
            report.provided.push((name.clone(), providers));
        }
    }
    report.present.sort();
    report.provided.sort();
    report.missing.sort();
    Ok(report)
}

/// One dependency a resolution could not satisfy.
///
/// The three fields are the resolver's own account of the refusal, kept apart rather
/// than pre-formatted into a sentence: a caller rendering JSON wants them separate, and
/// a caller rendering a line wants to join them its own way.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// What could not be supplied: a dependency group as the archive declares it,
    /// alternatives included, or a package name the recipe asked for directly.
    pub requirement: String,
    /// Who asked for it — a selected package whose dependency it was, or the install
    /// list, which is the recipe's own `rootfs_packages`.
    pub required_by: String,
    /// Why it could not be: the name is absent, it is excluded, or the archive carries
    /// it only at a version the constraint rules out.
    pub reason: String,
}

/// What resolving a recipe's package set against the configured archives produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClosureReport {
    /// How many packages the closure came to, or `None` where it did not close.
    ///
    /// An `Option` rather than a zero: a resolution that refused something never
    /// finished selecting, so it has no size, and a count of nothing would read as a set
    /// that closed empty. The caller renders the two differently because they are
    /// different answers.
    pub installed: Option<usize>,
    /// Every dependency the resolution refused, in the order it met them.
    pub refusals: Vec<Refusal>,
}

impl ClosureReport {
    /// Whether the package set closes — every dependency of everything it names is
    /// satisfiable from the archives this build resolves against.
    pub fn is_complete(&self) -> bool {
        self.refusals.is_empty()
    }
}

/// Resolve the dependency closure of `names` against the archives a build would use.
///
/// This is the question [`available`] does not ask. A name being in the archive says
/// nothing about its dependencies being there, and a package whose dependency is absent
/// installs anyway — dpkg configures with `--force-depends` — leaving a rootfs that
/// cannot configure packages at all, including unrelated ones. The failure then surfaces
/// long after the build, on hardware, as a broken image.
///
/// Nothing is downloaded past the indexes: the resolution is computed from them and
/// stopped before the first `.deb`. So this costs what [`available`] costs plus the
/// closure, and answers before any node has compiled.
///
/// A refused resolve is reported, not raised: the refusals *are* the answer, and a
/// caller wants all of them rather than the first. Anything else that goes wrong —
/// an unreachable mirror, an unverifiable release — is still an error, because that is
/// a failure to ask the question rather than an answer to it.
///
/// # Errors
///
/// [`EngineError::Bootstrap`] when the archives cannot be configured, or when the
/// release or an index cannot be fetched or verified.
pub fn closure(
    ib: ImageBuild,
    mirrors: &[String],
    keyring: Option<&Path>,
    apt_sources: &[AptRepo],
    names: &[String],
) -> Result<ClosureReport, EngineError> {
    /// Collects the refusals a resolution reports on its way to failing.
    ///
    /// The events and the error carry the same set, and neither is a summary of the
    /// other — but only the events keep the three fields apart, so they are what this
    /// reads.
    #[derive(Default)]
    struct Refused {
        refusals: Vec<Refusal>,
    }

    impl DebianObserver for Refused {
        fn progress(&mut self, event: DebianEvent<'_>) {
            if let DebianEvent::Unsatisfiable {
                requirement,
                required_by,
                reason,
                ..
            } = event
            {
                self.refusals.push(Refusal {
                    requirement: requirement.to_string(),
                    required_by: required_by.to_string(),
                    reason: reason.to_string(),
                });
            }
        }
    }

    let ImageBuild { build, image } = ib;
    let fail = bootstrap_failure();
    // The same three settings the rootfs bootstrap resolves under, because a closure
    // computed under different ones would not be this build's closure. The base priority
    // seeds the same essential band, and `exclude` matters most: a group whose only
    // satisfier the recipe excluded is refused in a build, and a check that did not
    // exclude it would pass and let the build fail later. `extra_packages` is not here —
    // it is a flag on one invocation rather than anything the recipe declares.
    let mut debian = archives(ib, mirrors, keyring, apt_sources, "closure query")?
        .base_priority(Priority::Important)
        .include(names.iter().cloned())
        .exclude(image.rootfs_exclude.iter().cloned())
        .build()
        .map_err(fail(&format!(
            "configure the {} {} archives",
            build.arch, image.suite
        )))?;

    let mut refused = Refused::default();
    let resolved = debian.observe(&mut refused).resolve();
    match resolved {
        Ok(plan) => Ok(ClosureReport {
            installed: Some(plan.packages.len()),
            refusals: refused.refusals,
        }),
        // The resolver's own refusal, already collected above as events. Reported rather
        // than raised, so the caller renders the list.
        Err(DebianError::Resolve { .. }) => Ok(ClosureReport {
            installed: None,
            refusals: refused.refusals,
        }),
        Err(other) => Err(fail("resolve the recipe's package set")(other)),
    }
}
