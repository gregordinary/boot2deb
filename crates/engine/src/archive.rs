//! Asking the configured archives which package names they offer, without resolving
//! anything.
//!
//! This is the read half of a resolve and nothing more: it downloads the release and
//! the package indexes, projects the names out of them, and stops. No closure is
//! computed, no `.deb` is fetched, and the answer is served for an architecture the host
//! cannot execute — so it costs one index download however many names are asked about,
//! where a resolve per name would re-fetch that index every time.
//!
//! It exists because "does this suite carry that package" and "what would a bootstrap
//! install" are different questions, and the resolver answers only the second. A
//! top-level include naming nothing fails the *whole* resolve, so a batch of names
//! reports that they were not all there and never which were not — and it reports it
//! deep in a build, after everything has already compiled. One pass here answers the
//! first question for every name at once, before anything is built.

use crate::archfetch::ArchiveFetch;
use crate::bootstrap::components;
use crate::error::EngineError;
use crate::rootfs::{feature_repositories, AptRepo};
use boot2deb_core::model::ResolvedBuild;
use ferroday_cage::provision::debian::Debian;
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
    build: &ResolvedBuild,
    mirrors: &[String],
    keyring: Option<&Path>,
    apt_sources: &[AptRepo],
    names: &[String],
) -> Result<AvailabilityReport, EngineError> {
    let (primary, fallbacks) = mirrors
        .split_first()
        .ok_or_else(|| EngineError::Bootstrap {
            context: "configure the archive availability query".into(),
            message: "no Debian mirror was resolved".into(),
        })?;
    let fail = |context: &str| {
        let context = context.to_string();
        move |e: ferroday_cage::provision::debian::DebianError| EngineError::Bootstrap {
            context: context.clone(),
            message: e.to_string(),
        }
    };

    // No `include`, no `exclude`, no base priority: none of them shapes what the
    // archives *offer*, which is the only question asked here. Nor an identity map or a
    // cache dir — nothing is unpacked and nothing is downloaded past the indexes.
    let mut b = Debian::builder(build.image_suite())
        .architecture(build.arch.debian_arch())
        .components(components(build).split(','))
        // The same transport a build uses, so a feature repository this can reach is
        // one the build can reach and vice versa — see [`ArchiveFetch`].
        .fetcher(Box::new(ArchiveFetch::new()))
        .mirror(primary);
    for fallback in fallbacks {
        b = b.mirror_fallback(fallback);
    }
    // A snapshot backstop's release is expired by design, exactly as in a build.
    if !fallbacks.is_empty() {
        b = b.allow_stale_release(true);
    }
    if let Some(keyring) = keyring {
        b = b.keyring(keyring);
    }
    for repo in feature_repositories(apt_sources)? {
        b = b.repository(repo);
    }
    let mut debian = b.build().map_err(fail(&format!(
        "configure the {} {} archives",
        build.arch,
        build.image_suite()
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
