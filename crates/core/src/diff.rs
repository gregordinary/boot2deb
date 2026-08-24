//! Compare two build points: what moved between them, section by section.
//!
//! Pure — a deterministic comparison of values the caller has already read, so the
//! whole report is unit-testable without a build, a network, or a checkout. Reading
//! the documents is the caller's job ([`Side`] is what it hands over); deciding what
//! changed is this module's.
//!
//! A side can be described by a [`Lock`], by a [`ProvenanceManifest`], or by both,
//! and the two carry overlapping but different facts — a lock has no toolchain, a
//! manifest has no patch-series `ref`. So both are normalized into one [`Side`]
//! first, and a section neither side can answer reports itself
//! [`Unavailable`](Section::Unavailable) rather than as "nothing changed". The
//! distinction matters: "the blobs are identical" and "neither document records
//! blobs" are different answers, and only one of them is evidence.
//!
//! Typed rather than a generic TOML-value diff, so a version bump renders as
//! `linux-image-arm64 7.1.5 -> 7.1.6` and not as two opaque lines.

use crate::kconfig::{self, FragmentSet};
use crate::lock::Lock;
use crate::manifest::Package;
use crate::provenance::ProvenanceManifest;
use serde::Serialize;
use std::collections::BTreeMap;

/// Everything a comparison can read about one build point, normalized out of
/// whatever document described it.
///
/// Built with [`Side::from_lock`] or [`Side::from_provenance`] — or both, folded
/// together with [`Side::merge`] where a caller has the lock *and* the manifest for
/// one build. The two fields that need I/O to fill,
/// [`packages`](Self::packages) and [`kconfig`](Self::kconfig), are set by the
/// caller afterwards, which is what keeps this module pure.
///
/// Every field is optional because every document answers a different subset. A
/// `None` means "this document does not say", never "there is none": a build with no
/// rkbin blobs and a document that does not record blobs both arrive here as `None`,
/// and neither is a change.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Side {
    /// What to call this side in the report — a recipe name or a file path.
    pub label: String,
    /// The kernel this build point names.
    pub kernel: Option<KernelFacts>,
    /// The patch axes it pins, at most one per axis (`kernel`, `uboot`).
    pub patches: Vec<PatchAxis>,
    /// Every non-kernel source axis it pins, in a stable order.
    pub sources: Vec<SourcePin>,
    /// The rkbin blobs its boot chain consumes.
    pub blobs: Option<BlobFacts>,
    /// The solved package set of its rootfs — read from the manifest the lock or the
    /// provenance names, which is why the caller fills it.
    pub packages: Option<Vec<Package>>,
    /// Its kernel's merged config fragments — read from the config tree, likewise.
    pub kconfig: Option<FragmentSet>,
    /// What built it. Recorded only by a provenance manifest: a lock describes a
    /// build point, and this is a property of a run.
    pub builder: Option<BuilderFacts>,
}

/// The kernel a build point names.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct KernelFacts {
    /// Kernel definition id (`rk3588-mainline-7.1`).
    pub id: String,
    /// How it is obtained (`mainline`, `vendor`, `distro-package`). Recorded by a
    /// provenance manifest; a lock states it only implicitly, by whether it pins a
    /// commit at all.
    pub flavor: Option<String>,
    /// The clone URL the commit was pinned from. Lock-only: a commit id means
    /// nothing outside its repo, so a source that moved reinterprets every pin under
    /// it.
    pub source: Option<String>,
    /// The pinned ref. Absent for a distro-package kernel, which is not fetched.
    pub reference: Option<String>,
    /// The exact commit that ref pointed at.
    pub commit: Option<String>,
    /// The package a distro-package kernel installs. Absent for a compiled one.
    pub package: Option<String>,
}

/// One patch axis's pin: which series, from which commit of the patches repo.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchAxis {
    /// Which axis (`kernel` or `uboot`) — the two pin independently.
    pub axis: String,
    /// Series names, in application order.
    pub series: Vec<String>,
    /// The patches-repo ref. Absent from a provenance manifest, which records the
    /// commit alone.
    pub reference: Option<String>,
    /// The patches-repo commit.
    pub commit: Option<String>,
}

/// One pinned source tree outside the kernel and patch axes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SourcePin {
    /// Axis name: `uboot`, `mpp`, `librga`, `libmali`, `ffmpeg-base`,
    /// `ffmpeg-rockchip`, or `kmod:<name>`.
    pub axis: String,
    /// The pinned ref.
    pub reference: Option<String>,
    /// The exact commit.
    pub commit: Option<String>,
}

/// The rkbin blob pins a boot chain consumes, each `"<file>@sha256:<hex>"`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BlobFacts {
    /// ATF / BL31 pin.
    pub atf: String,
    /// DDR TPL pin.
    pub tpl: String,
    /// Optional BL32 (OP-TEE) pin.
    pub bl32: Option<String>,
}

/// What produced a build, as only a provenance manifest records it.
///
/// The section that answers "nothing changed in the config but the output moved":
/// the builder that ran, the host it cross-compiled from, and the archive state the
/// rootfs resolved against.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuilderFacts {
    /// boot2deb version that ran the build.
    pub version: String,
    /// Its git commit, where it was built from a checkout.
    pub commit: Option<String>,
    /// Whether that checkout was dirty.
    pub dirty: bool,
    /// The config tree's git commit, where the root was a checkout. Moves independently
    /// of [`commit`](Self::commit): the same binary can build from two config trees, and
    /// two binaries from one.
    pub config_commit: Option<String>,
    /// Whether that config tree was dirty.
    pub config_dirty: bool,
    /// Build-host architecture.
    pub host_arch: String,
    /// Target architecture.
    pub target_arch: String,
    /// The `CROSS_COMPILE` prefix the compile ran under.
    pub cross_compile: String,
    /// Each configured repository's release digest, keyed by the mirror URL — or by
    /// `local pool #<index>` for the build's own pool, whose URL is a path on the
    /// build host and therefore not recorded. The digest is the sharp value: it
    /// identifies the exact archive state a signature vouched for.
    pub archives: BTreeMap<String, String>,
}

impl Side {
    /// Normalize a [`Lock`] into a side.
    ///
    /// Fills every section a lock can answer. [`packages`](Self::packages) and
    /// [`kconfig`](Self::kconfig) are left `None` for the caller, and
    /// [`builder`](Self::builder) stays `None` permanently: a lock describes a build
    /// point, not a run of one.
    pub fn from_lock(label: impl Into<String>, lock: &Lock) -> Self {
        let mut sources = Vec::new();
        if let Some(u) = &lock.uboot {
            sources.push(SourcePin {
                axis: "uboot".into(),
                reference: Some(u.reference.clone()),
                commit: Some(u.commit.clone()),
            });
        }
        if let Some(us) = &lock.userspace {
            for (axis, pin) in [
                ("mpp", &us.mpp),
                ("librga", &us.librga),
                ("libmali", &us.libmali),
            ] {
                if let Some(p) = pin {
                    sources.push(SourcePin {
                        axis: axis.into(),
                        reference: Some(p.reference.clone()),
                        commit: Some(p.commit.clone()),
                    });
                }
            }
        }
        if let Some(ff) = &lock.ffmpeg {
            sources.push(SourcePin {
                axis: "ffmpeg-base".into(),
                reference: Some(ff.base.reference.clone()),
                commit: Some(ff.base.commit.clone()),
            });
            if let Some(rk) = &ff.rockchip {
                sources.push(SourcePin {
                    axis: "ffmpeg-rockchip".into(),
                    reference: Some(rk.reference.clone()),
                    commit: Some(rk.commit.clone()),
                });
            }
        }
        for k in &lock.kmods {
            sources.push(SourcePin {
                axis: format!("kmod:{}", k.name),
                reference: Some(k.reference.clone()),
                commit: Some(k.commit.clone()),
            });
        }
        let patch_axis = |axis: &str, pin: &crate::lock::PatchesPin| PatchAxis {
            axis: axis.to_string(),
            series: pin.series.clone(),
            reference: Some(pin.reference.clone()),
            commit: Some(pin.commit.clone()),
        };
        Side {
            label: label.into(),
            kernel: lock.kernel.as_ref().map(|k| KernelFacts {
                id: k.id.clone(),
                flavor: None,
                source: Some(k.source.clone()),
                reference: Some(k.reference.clone()),
                commit: Some(k.commit.clone()),
                package: None,
            }),
            patches: [
                lock.patches.as_ref().map(|p| patch_axis("kernel", p)),
                lock.uboot_patches.as_ref().map(|p| patch_axis("uboot", p)),
            ]
            .into_iter()
            .flatten()
            .collect(),
            sources,
            blobs: lock.blobs.as_ref().map(|b| BlobFacts {
                atf: b.atf.clone(),
                tpl: b.tpl.clone(),
                bl32: b.bl32.clone(),
            }),
            packages: None,
            kconfig: None,
            builder: None,
        }
    }

    /// Normalize a [`ProvenanceManifest`] into a side.
    ///
    /// Answers one section a lock cannot ([`builder`](Self::builder)) and answers two
    /// others more thinly: it records the kernel's flavor but not its clone URL, and
    /// the kernel patch series' commit but not its ref, and it records nothing about
    /// the u-boot patch axis at all. Each of those arrives as `None`, so comparing a
    /// manifest against a lock reports them unavailable rather than as a change.
    pub fn from_provenance(label: impl Into<String>, prov: &ProvenanceManifest) -> Self {
        let s = &prov.sources;
        let mut sources = Vec::new();
        if s.uboot_ref.is_some() || s.uboot_commit.is_some() {
            sources.push(SourcePin {
                axis: "uboot".into(),
                reference: s.uboot_ref.clone(),
                commit: s.uboot_commit.clone(),
            });
        }
        if let Some(ma) = &s.media_accel {
            for (axis, reference, commit) in [
                ("mpp", &ma.mpp_ref, &ma.mpp_commit),
                ("librga", &ma.librga_ref, &ma.librga_commit),
                ("libmali", &ma.libmali_ref, &ma.libmali_commit),
                (
                    "ffmpeg-rockchip",
                    &ma.ffmpeg_rockchip_ref,
                    &ma.ffmpeg_rockchip_commit,
                ),
            ] {
                if reference.is_some() || commit.is_some() {
                    sources.push(SourcePin {
                        axis: axis.into(),
                        reference: reference.clone(),
                        commit: commit.clone(),
                    });
                }
            }
            sources.push(SourcePin {
                axis: "ffmpeg-base".into(),
                reference: Some(ma.ffmpeg_base_ref.clone()),
                commit: Some(ma.ffmpeg_base_commit.clone()),
            });
        }
        // Stable order regardless of which document filled it, so two sides read out
        // of different documents still line up axis for axis.
        sources.sort_by(|a, b| a.axis.cmp(&b.axis));
        Side {
            label: label.into(),
            kernel: Some(KernelFacts {
                id: s.kernel_id.clone(),
                flavor: Some(s.kernel_flavor.clone()),
                source: None,
                reference: s.kernel_ref.clone(),
                commit: s.kernel_commit.clone(),
                package: s.kernel_package.clone(),
            }),
            patches: if s.patch_series.is_empty() && s.patches_commit.is_none() {
                Vec::new()
            } else {
                vec![PatchAxis {
                    axis: "kernel".into(),
                    series: s.patch_series.clone(),
                    reference: None,
                    commit: s.patches_commit.clone(),
                }]
            },
            sources,
            blobs: prov.blobs.as_ref().map(|b| BlobFacts {
                atf: b.atf.clone(),
                tpl: b.tpl.clone(),
                bl32: b.bl32.clone(),
            }),
            packages: None,
            kconfig: None,
            builder: Some(BuilderFacts {
                version: prov.built_with.version.clone(),
                commit: prov.built_with.commit.clone(),
                dirty: prov.built_with.dirty,
                config_commit: prov.built_with.config_commit.clone(),
                config_dirty: prov.built_with.config_dirty,
                host_arch: prov.toolchain.host_arch.clone(),
                target_arch: prov.toolchain.target_arch.clone(),
                cross_compile: prov.toolchain.cross_compile.clone(),
                archives: prov
                    .archives
                    .iter()
                    .map(|a| {
                        let key = a
                            .mirror
                            .clone()
                            .unwrap_or_else(|| format!("local pool #{}", a.index));
                        (key, a.release_sha256.clone())
                    })
                    .collect(),
            }),
        }
    }

    /// Normalize a resolved build point's kernel identity into a side.
    ///
    /// Only the kernel, and only its identity — the id, the flavor, and the package a
    /// distro-package kernel installs. That is exactly the gap a lock leaves: a
    /// distro kernel pins no commit, so a lock records no `[kernel]` table at all and
    /// two boards' kernels would compare as absent. Everything else a resolve holds
    /// is a *declaration*; the lock's pins are what a build used, and those are what
    /// the other constructors carry.
    ///
    /// Folded onto a lock's side with [`merge`](Self::merge), which keeps the lock's
    /// pinned ref over the declared one.
    pub fn from_resolved(label: impl Into<String>, build: &crate::ResolvedBuild) -> Self {
        Side {
            label: label.into(),
            kernel: build.kernel.as_ref().map(|k| KernelFacts {
                id: k.id().to_string(),
                flavor: Some(k.flavor().to_string()),
                package: match k {
                    crate::ResolvedKernel::Distro(d) => Some(d.package.clone()),
                    crate::ResolvedKernel::Compiled(_) => None,
                },
                ..KernelFacts::default()
            }),
            ..Side::default()
        }
    }

    /// Fold `other`'s facts into this side wherever this one has none.
    ///
    /// For the caller that holds both documents for one build: the lock supplies the
    /// clone URLs, the patches ref and the u-boot patch axis; the manifest supplies
    /// the flavor and the builder. Taking this side's value wherever it has one makes
    /// the merge order the caller's declaration of which document it trusts.
    pub fn merge(mut self, other: Side) -> Self {
        self.kernel = match (self.kernel, other.kernel) {
            (Some(a), Some(b)) => Some(KernelFacts {
                id: a.id,
                flavor: a.flavor.or(b.flavor),
                source: a.source.or(b.source),
                reference: a.reference.or(b.reference),
                commit: a.commit.or(b.commit),
                package: a.package.or(b.package),
            }),
            (a, b) => a.or(b),
        };
        for axis in other.patches {
            if !self.patches.iter().any(|p| p.axis == axis.axis) {
                self.patches.push(axis);
            }
        }
        self.patches.sort_by(|a, b| a.axis.cmp(&b.axis));
        for pin in other.sources {
            if !self.sources.iter().any(|s| s.axis == pin.axis) {
                self.sources.push(pin);
            }
        }
        self.sources.sort_by(|a, b| a.axis.cmp(&b.axis));
        self.blobs = self.blobs.or(other.blobs);
        self.packages = self.packages.or(other.packages);
        self.kconfig = self.kconfig.or(other.kconfig);
        self.builder = self.builder.or(other.builder);
        self
    }
}

/// One section's result: either both sides could answer it, or they could not.
///
/// The two are kept apart because "identical" and "not recorded" are different
/// answers to the same question, and reporting the second as the first would state
/// evidence that does not exist.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Section<T> {
    /// One or both sides carry nothing to compare, and why.
    Unavailable {
        /// What is missing, in words, for the report to print in place of a diff.
        why: String,
    },
    /// Both sides answered. `changes` is empty when they agree.
    Compared {
        /// What differs. Empty means the two sides state the same thing.
        changes: T,
    },
}

impl<T: IsEmpty> Section<T> {
    /// Whether this section found nothing to report — either because it could not be
    /// compared, or because the two sides agree.
    pub fn is_quiet(&self) -> bool {
        match self {
            Section::Unavailable { .. } => true,
            Section::Compared { changes } => changes.is_empty(),
        }
    }
}

/// A section payload that can be empty — the "both sides agree" case.
pub trait IsEmpty {
    /// Whether the payload carries no differences.
    fn is_empty(&self) -> bool;
}

/// One value that moved. `None` on a side means that side does not state the value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Change {
    /// What the value was on the left side.
    pub from: Option<String>,
    /// What it is on the right side.
    pub to: Option<String>,
}

impl Change {
    /// A change between two optional values, or `None` when they agree.
    ///
    /// Two `None`s agree, so a value neither side states is not a change. A value one
    /// side states and the other does not *is* one, and renders with a `-` for the
    /// silent side — a lock that gained a u-boot patch axis really did change.
    fn between(from: Option<&str>, to: Option<&str>) -> Option<Change> {
        (from != to).then(|| Change {
            from: from.map(str::to_string),
            to: to.map(str::to_string),
        })
    }
}

/// Every section's comparison, in the order a reader checks them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Report {
    /// The left side's label.
    pub left: String,
    /// The right side's label.
    pub right: String,
    /// Package set: what the image gained, lost, and re-versioned.
    pub packages: Section<PackageChanges>,
    /// Kernel pin and requested kernel configuration.
    pub kernel: Section<KernelChanges>,
    /// Patch series membership and pins.
    pub patches: Section<Vec<PatchAxisChange>>,
    /// Every other pinned source tree.
    pub sources: Section<Vec<SourceChange>>,
    /// rkbin blob pins.
    pub blobs: Section<Vec<Change>>,
    /// What built each side.
    pub builder: Section<BuilderChanges>,
}

impl Report {
    /// Whether nothing at all was found to report — every section either agreed or
    /// could not be compared.
    pub fn is_quiet(&self) -> bool {
        self.packages.is_quiet()
            && self.kernel.is_quiet()
            && self.patches.is_quiet()
            && self.sources.is_quiet()
            && self.blobs.is_quiet()
            && self.builder.is_quiet()
    }
}

/// How two solved package sets differ.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct PackageChanges {
    /// Packages the right side installs and the left does not.
    pub added: Vec<PackageRef>,
    /// Packages the left side installs and the right does not.
    pub removed: Vec<PackageRef>,
    /// Packages both install at different versions. No direction is claimed: Debian
    /// version ordering is its own algorithm, and a downgrade is as real an event as
    /// an upgrade.
    pub changed: Vec<VersionChange>,
    /// Packages both install at the *same* version whose `.deb` bytes differ. A
    /// binNMU rebuilt under one name, or an archive that reissued a version — either
    /// way, the thing a name-and-version comparison would call identical.
    pub rebuilt: Vec<PackageRef>,
}

impl IsEmpty for PackageChanges {
    fn is_empty(&self) -> bool {
        self.added.is_empty()
            && self.removed.is_empty()
            && self.changed.is_empty()
            && self.rebuilt.is_empty()
    }
}

/// One package, as a manifest line names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PackageRef {
    /// Binary package name.
    pub name: String,
    /// Its version on the side being described.
    pub version: String,
    /// Its Debian architecture.
    pub architecture: String,
}

/// A package installed on both sides at two versions.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VersionChange {
    /// Binary package name.
    pub name: String,
    /// The left side's version.
    pub from: String,
    /// The right side's version.
    pub to: String,
}

/// How two kernels differ: the pin, and the configuration requested of it.
///
/// No `Default`: [`kconfig`](Self::kconfig) has no default that is not a claim, since
/// "compared, and identical" is evidence and an unbuilt value has none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct KernelChanges {
    /// The kernel definition id, when the build points name different kernels.
    pub id: Option<Change>,
    /// How the kernel is obtained.
    pub flavor: Option<Change>,
    /// The clone URL the commit was pinned from. A change here reinterprets the
    /// commit below in a different repository, which is why it is its own line.
    pub source: Option<Change>,
    /// The pinned ref.
    pub reference: Option<Change>,
    /// The exact commit.
    pub commit: Option<Change>,
    /// The package a distro-package kernel installs.
    pub package: Option<Change>,
    /// Symbols whose requested value differs between the two fragment sets — a
    /// section of its own, because a side can carry a kernel pin and no fragment set
    /// at all, and an empty delta then means "nothing compared" rather than
    /// "identical configuration".
    pub kconfig: Section<Vec<SymbolChange>>,
}

impl IsEmpty for KernelChanges {
    fn is_empty(&self) -> bool {
        self.id.is_none()
            && self.flavor.is_none()
            && self.source.is_none()
            && self.reference.is_none()
            && self.commit.is_none()
            && self.package.is_none()
            && self.kconfig.is_quiet()
    }
}

impl IsEmpty for Vec<SymbolChange> {
    fn is_empty(&self) -> bool {
        self.as_slice().is_empty()
    }
}

/// One `CONFIG_*` symbol requested differently by the two fragment sets, with the
/// fragment that set it on each side.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SymbolChange {
    /// The symbol name.
    pub symbol: String,
    /// Its value on the left, rendered (`y`, `m`, `18`, `(not set)`).
    pub from: String,
    /// Its value on the right.
    pub to: String,
    /// The fragment that last set it on the left, where one did.
    pub from_fragment: Option<String>,
    /// The fragment that last set it on the right, where one did.
    pub to_fragment: Option<String>,
}

/// How one patch axis differs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PatchAxisChange {
    /// Which axis (`kernel`, `uboot`).
    pub axis: String,
    /// Series the right side applies and the left does not.
    pub series_added: Vec<String>,
    /// Series the left side applies and the right does not.
    pub series_removed: Vec<String>,
    /// The patches-repo ref.
    pub reference: Option<Change>,
    /// The patches-repo commit. A move here is what the per-patch file delta
    /// resolves into named files.
    pub commit: Option<Change>,
}

impl IsEmpty for Vec<PatchAxisChange> {
    fn is_empty(&self) -> bool {
        self.as_slice().is_empty()
    }
}

/// How one source axis's pin differs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SourceChange {
    /// Axis name.
    pub axis: String,
    /// The pinned ref.
    pub reference: Option<Change>,
    /// The exact commit.
    pub commit: Option<Change>,
}

impl IsEmpty for Vec<SourceChange> {
    fn is_empty(&self) -> bool {
        self.as_slice().is_empty()
    }
}

impl IsEmpty for Vec<Change> {
    fn is_empty(&self) -> bool {
        self.as_slice().is_empty()
    }
}

/// How the two builds' builders and archive states differ.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct BuilderChanges {
    /// The boot2deb version.
    pub version: Option<Change>,
    /// Its git commit.
    pub commit: Option<Change>,
    /// Whether the checkout was dirty.
    pub dirty: Option<Change>,
    /// The config tree's git commit.
    pub config_commit: Option<Change>,
    /// Whether the config tree was dirty.
    pub config_dirty: Option<Change>,
    /// The build host's architecture.
    pub host_arch: Option<Change>,
    /// The target architecture.
    pub target_arch: Option<Change>,
    /// The `CROSS_COMPILE` prefix.
    pub cross_compile: Option<Change>,
    /// Per repository, the release digest that moved — keyed by mirror URL. A
    /// repository present on one side only appears with a `-` for the other.
    pub archives: Vec<ArchiveChange>,
}

impl IsEmpty for BuilderChanges {
    fn is_empty(&self) -> bool {
        self.version.is_none()
            && self.commit.is_none()
            && self.dirty.is_none()
            && self.config_commit.is_none()
            && self.config_dirty.is_none()
            && self.host_arch.is_none()
            && self.target_arch.is_none()
            && self.cross_compile.is_none()
            && self.archives.is_empty()
    }
}

/// One repository whose release state moved between the two builds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ArchiveChange {
    /// The mirror URL, or `local pool #<index>` for the build's own pool.
    pub mirror: String,
    /// The `Release` file's sha256 on each side.
    pub release_sha256: Change,
}

/// Compare two build points.
///
/// Every section is answered independently, so a comparison between documents of
/// different kinds still reports everything they have in common and marks the rest
/// unavailable.
pub fn compare(left: &Side, right: &Side) -> Report {
    Report {
        left: left.label.clone(),
        right: right.label.clone(),
        packages: compare_packages(left, right),
        kernel: compare_kernel(left, right),
        patches: compare_patches(left, right),
        sources: compare_sources(left, right),
        blobs: compare_blobs(left, right),
        builder: compare_builder(left, right),
    }
}

/// Which side (or sides) is missing a section, in words, for an
/// [`Unavailable`](Section::Unavailable) reason.
fn missing(left: &Side, right: &Side, has_left: bool, has_right: bool, what: &str) -> String {
    match (has_left, has_right) {
        (false, false) => format!("neither side records {what}"),
        (false, true) => format!("{} does not record {what}", left.label),
        (true, false) => format!("{} does not record {what}", right.label),
        (true, true) => unreachable!("both sides have it, so nothing is missing"),
    }
}

fn compare_packages(left: &Side, right: &Side) -> Section<PackageChanges> {
    let (Some(l), Some(r)) = (&left.packages, &right.packages) else {
        return Section::Unavailable {
            why: missing(
                left,
                right,
                left.packages.is_some(),
                right.packages.is_some(),
                "a solved package manifest",
            ),
        };
    };
    // Keyed by name alone, not by (name, architecture): a package that moved from
    // `all` to `arm64` is one package whose build changed, and reporting it as an
    // addition beside a removal would hide that.
    let index = |ps: &[Package]| -> BTreeMap<String, Package> {
        ps.iter().map(|p| (p.name.clone(), p.clone())).collect()
    };
    let (l, r) = (index(l), index(r));
    let as_ref = |p: &Package| PackageRef {
        name: p.name.clone(),
        version: p.version.clone(),
        architecture: p.architecture.clone(),
    };
    let mut changes = PackageChanges::default();
    for (name, pkg) in &r {
        match l.get(name) {
            None => changes.added.push(as_ref(pkg)),
            Some(was) if was.version != pkg.version => changes.changed.push(VersionChange {
                name: name.clone(),
                from: was.version.clone(),
                to: pkg.version.clone(),
            }),
            // Same version, different bytes — which is the case a name-and-version
            // comparison calls identical and a content pin does not.
            Some(was) if was.sha256 != pkg.sha256 => changes.rebuilt.push(as_ref(pkg)),
            Some(_) => {}
        }
    }
    for (name, pkg) in &l {
        if !r.contains_key(name) {
            changes.removed.push(as_ref(pkg));
        }
    }
    Section::Compared { changes }
}

fn compare_kernel(left: &Side, right: &Side) -> Section<KernelChanges> {
    let (Some(l), Some(r)) = (&left.kernel, &right.kernel) else {
        return Section::Unavailable {
            why: missing(
                left,
                right,
                left.kernel.is_some(),
                right.kernel.is_some(),
                "a kernel pin",
            ),
        };
    };
    let kconfig = match (&left.kconfig, &right.kconfig) {
        (Some(lc), Some(rc)) => Section::Compared {
            changes: kconfig::diff(lc.config(), rc.config())
                .into_iter()
                .map(|d| SymbolChange {
                    from: d.left.to_string(),
                    to: d.right.to_string(),
                    from_fragment: lc.origin(&d.symbol).map(str::to_string),
                    to_fragment: rc.origin(&d.symbol).map(str::to_string),
                    symbol: d.symbol,
                })
                .collect(),
        },
        _ => Section::Unavailable {
            why: missing(
                left,
                right,
                left.kconfig.is_some(),
                right.kconfig.is_some(),
                "a kernel fragment set (a distro-package kernel merges none, and a \
                 fragment set is resolved from the config tree rather than named by \
                 any document a build writes)",
            ),
        },
    };
    Section::Compared {
        changes: KernelChanges {
            id: Change::between(Some(&l.id), Some(&r.id)),
            flavor: Change::between(l.flavor.as_deref(), r.flavor.as_deref()),
            source: Change::between(l.source.as_deref(), r.source.as_deref()),
            reference: Change::between(l.reference.as_deref(), r.reference.as_deref()),
            commit: Change::between(l.commit.as_deref(), r.commit.as_deref()),
            package: Change::between(l.package.as_deref(), r.package.as_deref()),
            kconfig,
        },
    }
}

fn compare_patches(left: &Side, right: &Side) -> Section<Vec<PatchAxisChange>> {
    if left.patches.is_empty() && right.patches.is_empty() {
        return Section::Unavailable {
            why: "neither side applies a patch series".to_string(),
        };
    }
    let find = |side: &[PatchAxis], axis: &str| side.iter().find(|p| p.axis == axis).cloned();
    let mut axes: Vec<String> = left
        .patches
        .iter()
        .chain(&right.patches)
        .map(|p| p.axis.clone())
        .collect();
    axes.sort();
    axes.dedup();
    let changes = axes
        .into_iter()
        .filter_map(|axis| {
            let (l, r) = (find(&left.patches, &axis), find(&right.patches, &axis));
            // An axis one side does not have is the empty series at no commit, so a
            // build that gained or dropped one reports its whole series list.
            let ls = l.as_ref().map(|p| p.series.as_slice()).unwrap_or(&[]);
            let rs = r.as_ref().map(|p| p.series.as_slice()).unwrap_or(&[]);
            let change = PatchAxisChange {
                series_added: rs.iter().filter(|s| !ls.contains(s)).cloned().collect(),
                series_removed: ls.iter().filter(|s| !rs.contains(s)).cloned().collect(),
                reference: Change::between(
                    l.as_ref().and_then(|p| p.reference.as_deref()),
                    r.as_ref().and_then(|p| p.reference.as_deref()),
                ),
                commit: Change::between(
                    l.as_ref().and_then(|p| p.commit.as_deref()),
                    r.as_ref().and_then(|p| p.commit.as_deref()),
                ),
                axis,
            };
            let quiet = change.series_added.is_empty()
                && change.series_removed.is_empty()
                && change.reference.is_none()
                && change.commit.is_none();
            (!quiet).then_some(change)
        })
        .collect();
    Section::Compared { changes }
}

fn compare_sources(left: &Side, right: &Side) -> Section<Vec<SourceChange>> {
    if left.sources.is_empty() && right.sources.is_empty() {
        return Section::Unavailable {
            why: "neither side pins a source outside the kernel".to_string(),
        };
    }
    let mut axes: Vec<String> = left
        .sources
        .iter()
        .chain(&right.sources)
        .map(|s| s.axis.clone())
        .collect();
    axes.sort();
    axes.dedup();
    fn find<'a>(side: &'a [SourcePin], axis: &str) -> Option<&'a SourcePin> {
        side.iter().find(|s| s.axis == axis)
    }
    let changes = axes
        .into_iter()
        .filter_map(|axis| {
            let (l, r) = (find(&left.sources, &axis), find(&right.sources, &axis));
            let change = SourceChange {
                reference: Change::between(
                    l.and_then(|s| s.reference.as_deref()),
                    r.and_then(|s| s.reference.as_deref()),
                ),
                commit: Change::between(
                    l.and_then(|s| s.commit.as_deref()),
                    r.and_then(|s| s.commit.as_deref()),
                ),
                axis,
            };
            (change.reference.is_some() || change.commit.is_some()).then_some(change)
        })
        .collect();
    Section::Compared { changes }
}

fn compare_blobs(left: &Side, right: &Side) -> Section<Vec<Change>> {
    let (Some(l), Some(r)) = (&left.blobs, &right.blobs) else {
        return Section::Unavailable {
            why: missing(
                left,
                right,
                left.blobs.is_some(),
                right.blobs.is_some(),
                "rkbin blob pins",
            ),
        };
    };
    Section::Compared {
        changes: [
            Change::between(Some(&l.atf), Some(&r.atf)),
            Change::between(Some(&l.tpl), Some(&r.tpl)),
            Change::between(l.bl32.as_deref(), r.bl32.as_deref()),
        ]
        .into_iter()
        .flatten()
        .collect(),
    }
}

fn compare_builder(left: &Side, right: &Side) -> Section<BuilderChanges> {
    let (Some(l), Some(r)) = (&left.builder, &right.builder) else {
        return Section::Unavailable {
            why: missing(
                left,
                right,
                left.builder.is_some(),
                right.builder.is_some(),
                "a provenance manifest, which is where the builder and archive state \
                 are recorded",
            ),
        };
    };
    let mut mirrors: Vec<&String> = l.archives.keys().chain(r.archives.keys()).collect();
    mirrors.sort();
    mirrors.dedup();
    Section::Compared {
        changes: BuilderChanges {
            version: Change::between(Some(&l.version), Some(&r.version)),
            commit: Change::between(l.commit.as_deref(), r.commit.as_deref()),
            dirty: Change::between(Some(bool_str(l.dirty)), Some(bool_str(r.dirty))),
            config_commit: Change::between(l.config_commit.as_deref(), r.config_commit.as_deref()),
            config_dirty: Change::between(
                Some(bool_str(l.config_dirty)),
                Some(bool_str(r.config_dirty)),
            ),
            host_arch: Change::between(Some(&l.host_arch), Some(&r.host_arch)),
            target_arch: Change::between(Some(&l.target_arch), Some(&r.target_arch)),
            cross_compile: Change::between(Some(&l.cross_compile), Some(&r.cross_compile)),
            archives: mirrors
                .into_iter()
                .filter_map(|m| {
                    Change::between(
                        l.archives.get(m).map(String::as_str),
                        r.archives.get(m).map(String::as_str),
                    )
                    .map(|release_sha256| ArchiveChange {
                        mirror: m.clone(),
                        release_sha256,
                    })
                })
                .collect(),
        },
    }
}

/// A bool as the word a report prints for it.
fn bool_str(b: bool) -> &'static str {
    if b {
        "yes"
    } else {
        "no"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lock::*;

    fn git(reference: &str, commit: &str) -> GitPin {
        GitPin {
            source: "https://example.invalid/x.git".into(),
            reference: reference.into(),
            commit: commit.repeat(40 / commit.len()),
        }
    }

    fn base_lock() -> Lock {
        Lock {
            kernel: Some(KernelPin {
                id: "rk3588-mainline-7.1".into(),
                source: "https://example.invalid/linux.git".into(),
                reference: "v7.1.5".into(),
                commit: "a".repeat(40),
            }),
            patches: Some(PatchesPin {
                series: vec!["rk3588-fixes".into(), "media-accel".into()],
                source: "https://example.invalid/patches.git".into(),
                reference: "v1.2.0".into(),
                commit: "b".repeat(40),
            }),
            uboot: Some(UbootPin {
                source: "https://example.invalid/u-boot.git".into(),
                reference: "v2026.04".into(),
                commit: "c".repeat(40),
            }),
            uboot_patches: None,
            userspace: Some(UserspacePins {
                mpp: Some(git("mainline-cma-fix", "d")),
                librga: Some(git("master", "e")),
                libmali: None,
            }),
            ffmpeg: Some(FfmpegPins {
                base: git("v4l2-request-n8.1", "f"),
                rockchip: None,
            }),
            rootfs: Some(RootfsPin {
                suite: "forky".into(),
                manifest: "m.pkgs.lock".into(),
                manifest_sha256: None,
            }),
            blobs: Some(BlobsPin {
                atf: format!("bl31.elf@sha256:{}", "1".repeat(64)),
                tpl: format!("ddr.bin@sha256:{}", "2".repeat(64)),
                bl32: None,
            }),
            kmods: vec![],
            extra_debs: vec![],
            snapshot: None,
        }
    }

    fn pkg(name: &str, version: &str, sha: &str) -> Package {
        Package {
            name: name.into(),
            version: version.into(),
            architecture: "arm64".into(),
            sha256: sha.into(),
        }
    }

    /// The section that answers "why did the image grow" and "what did the snapshot
    /// move" — including the case a name-and-version comparison misses, where the
    /// version held still and the bytes did not.
    #[test]
    fn packages_split_into_added_removed_changed_and_rebuilt() {
        let left = Side {
            packages: Some(vec![
                pkg("libc6", "2.41-1", "aa"),
                pkg("ffmpeg", "8.1-1", "bb"),
                pkg("gone", "1.0", "cc"),
                pkg("stable", "3.0", "dd"),
            ]),
            ..Side::default()
        };
        let right = Side {
            packages: Some(vec![
                pkg("libc6", "2.41-2", "ee"),
                pkg("ffmpeg", "8.1-1", "ff"),
                pkg("brand-new", "0.1", "gg"),
                pkg("stable", "3.0", "dd"),
            ]),
            ..Side::default()
        };
        let Section::Compared { changes } = compare_packages(&left, &right) else {
            panic!("both sides have a manifest");
        };
        assert_eq!(
            changes.changed,
            vec![VersionChange {
                name: "libc6".into(),
                from: "2.41-1".into(),
                to: "2.41-2".into()
            }]
        );
        assert_eq!(
            changes.added.iter().map(|p| &p.name).collect::<Vec<_>>(),
            vec!["brand-new"]
        );
        assert_eq!(
            changes.removed.iter().map(|p| &p.name).collect::<Vec<_>>(),
            vec!["gone"]
        );
        // Same version, different `.deb` — the archive reissued it.
        assert_eq!(
            changes.rebuilt.iter().map(|p| &p.name).collect::<Vec<_>>(),
            vec!["ffmpeg"]
        );
        // A package identical on both sides is in no list at all.
        assert!(!changes.added.iter().any(|p| p.name == "stable"));
    }

    /// A lock and a provenance manifest answer overlapping but different questions.
    /// A section neither can answer says so, rather than reporting agreement it has
    /// no evidence for.
    #[test]
    fn a_section_neither_side_records_is_unavailable_not_unchanged() {
        let side = Side::from_lock("turing-rk1/forky", &base_lock());
        let report = compare(&side, &side);
        // Two locks: no manifest was read, and no builder is recorded by either.
        match &report.packages {
            Section::Unavailable { why } => assert!(why.contains("neither side"), "{why}"),
            other => panic!("expected unavailable, got {other:?}"),
        }
        match &report.builder {
            Section::Unavailable { why } => assert!(why.contains("provenance"), "{why}"),
            other => panic!("expected unavailable, got {other:?}"),
        }
        // Everything a lock does answer compares clean against itself.
        assert!(report.is_quiet());
    }

    /// Only one side missing a section names *which* side, so a reader knows whether
    /// to go find the other document or accept that it does not exist.
    #[test]
    fn one_sided_absence_names_the_side_that_is_silent() {
        let mut left = Side::from_lock("old.lock", &base_lock());
        left.packages = Some(vec![pkg("libc6", "2.41-1", "aa")]);
        let right = Side::from_lock("new.lock", &base_lock());
        match compare(&left, &right).packages {
            Section::Unavailable { why } => {
                assert!(why.contains("new.lock"), "{why}");
                assert!(!why.contains("old.lock"), "{why}");
            }
            other => panic!("expected unavailable, got {other:?}"),
        }
    }

    #[test]
    fn a_kernel_bump_reports_the_ref_and_the_commit_but_not_the_repo() {
        let left = Side::from_lock("old", &base_lock());
        let mut newer = base_lock();
        let k = newer.kernel.as_mut().unwrap();
        k.reference = "v7.1.6".into();
        k.commit = "9".repeat(40);
        let right = Side::from_lock("new", &newer);

        let Section::Compared { changes } = compare(&left, &right).kernel else {
            panic!("both sides pin a kernel");
        };
        assert_eq!(
            changes.reference,
            Some(Change {
                from: Some("v7.1.5".into()),
                to: Some("v7.1.6".into())
            })
        );
        assert!(changes.commit.is_some());
        // The pin moved within the same repository, so the source line stays silent.
        assert!(changes.source.is_none());
        assert!(changes.id.is_none());
        // No fragments were read, which is not the same as their agreeing.
        assert!(matches!(changes.kconfig, Section::Unavailable { .. }));
    }

    /// The kconfig delta names the fragment behind each symbol, which is what a diff
    /// of two generated `.config` files cannot do.
    #[test]
    fn the_kconfig_delta_attributes_each_symbol_to_its_fragment() {
        let mut left = Side::from_lock("old", &base_lock());
        left.kconfig = Some(FragmentSet::merge([(
            "fragments/rk3588-base.config",
            "CONFIG_A=y\nCONFIG_B=m\n",
        )]));
        let mut right = Side::from_lock("new", &base_lock());
        right.kconfig = Some(FragmentSet::merge([
            ("fragments/rk3588-base.config", "CONFIG_A=y\nCONFIG_B=m\n"),
            ("fragments/rocket-npu.config", "CONFIG_B=y\nCONFIG_C=y\n"),
        ]));

        let Section::Compared { changes } = compare(&left, &right).kernel else {
            panic!("both sides pin a kernel");
        };
        let Section::Compared { changes: symbols } = &changes.kconfig else {
            panic!("both sides resolved a fragment set");
        };
        assert_eq!(symbols.len(), 2);

        let b = &symbols[0];
        assert_eq!(b.symbol, "CONFIG_B");
        assert_eq!((b.from.as_str(), b.to.as_str()), ("m", "y"));
        assert_eq!(
            b.from_fragment.as_deref(),
            Some("fragments/rk3588-base.config")
        );
        assert_eq!(
            b.to_fragment.as_deref(),
            Some("fragments/rocket-npu.config")
        );

        let c = &symbols[1];
        assert_eq!(c.symbol, "CONFIG_C");
        assert_eq!(c.from, "(not set)");
        // No fragment on the left mentions it, so there is no left fragment to name.
        assert_eq!(c.from_fragment, None);
        assert_eq!(
            c.to_fragment.as_deref(),
            Some("fragments/rocket-npu.config")
        );
    }

    #[test]
    fn patch_series_membership_and_the_pin_move_together() {
        let left = Side::from_lock("old", &base_lock());
        let mut newer = base_lock();
        let p = newer.patches.as_mut().unwrap();
        p.series = vec!["rk3588-fixes".into(), "rocket".into()];
        p.reference = "v1.3.0".into();
        p.commit = "7".repeat(40);
        // A u-boot series where there was none: an axis that appeared.
        newer.uboot_patches = Some(PatchesPin {
            series: vec!["rk3588-uboot".into()],
            source: "https://example.invalid/patches.git".into(),
            reference: "v1.3.0".into(),
            commit: "7".repeat(40),
        });
        let right = Side::from_lock("new", &newer);

        let Section::Compared { changes } = compare(&left, &right).patches else {
            panic!("both sides apply series");
        };
        assert_eq!(changes.len(), 2);
        let kernel = changes.iter().find(|c| c.axis == "kernel").unwrap();
        assert_eq!(kernel.series_added, vec!["rocket"]);
        assert_eq!(kernel.series_removed, vec!["media-accel"]);
        assert_eq!(
            kernel.reference,
            Some(Change {
                from: Some("v1.2.0".into()),
                to: Some("v1.3.0".into())
            })
        );

        // The axis that appeared reports its whole series list as added, and its ref
        // as arriving from nothing.
        let uboot = changes.iter().find(|c| c.axis == "uboot").unwrap();
        assert_eq!(uboot.series_added, vec!["rk3588-uboot"]);
        assert!(uboot.series_removed.is_empty());
        assert_eq!(
            uboot.reference,
            Some(Change {
                from: None,
                to: Some("v1.3.0".into())
            })
        );
    }

    #[test]
    fn only_the_source_axes_that_moved_are_reported() {
        let left = Side::from_lock("old", &base_lock());
        let mut newer = base_lock();
        newer.userspace.as_mut().unwrap().librga = Some(git("multicore", "5"));
        let right = Side::from_lock("new", &newer);

        let Section::Compared { changes } = compare(&left, &right).sources else {
            panic!("both sides pin sources");
        };
        assert_eq!(
            changes.iter().map(|c| c.axis.as_str()).collect::<Vec<_>>(),
            vec!["librga"]
        );
        assert_eq!(
            changes[0].reference,
            Some(Change {
                from: Some("master".into()),
                to: Some("multicore".into())
            })
        );
    }

    #[test]
    fn a_blob_that_moved_under_a_fixed_name_shows_as_a_digest_change() {
        let left = Side::from_lock("old", &base_lock());
        let mut newer = base_lock();
        newer.blobs.as_mut().unwrap().tpl = format!("ddr.bin@sha256:{}", "3".repeat(64));
        let right = Side::from_lock("new", &newer);

        let Section::Compared { changes } = compare(&left, &right).blobs else {
            panic!("both sides pin blobs");
        };
        assert_eq!(changes.len(), 1);
        assert!(changes[0].from.as_ref().unwrap().contains("ddr.bin"));
        assert!(changes[0].to.as_ref().unwrap().ends_with(&"3".repeat(64)));
    }

    /// Merging the two documents for one build gives a side that answers everything
    /// either of them does — the lock's clone URLs and patches ref, the manifest's
    /// flavor and builder.
    #[test]
    fn a_side_merged_from_both_documents_answers_more_than_either() {
        let lock_side = Side::from_lock("turing-rk1/forky", &base_lock());
        let prov_side = Side {
            label: "turing-rk1-forky.provenance.toml".into(),
            kernel: Some(KernelFacts {
                id: "rk3588-mainline-7.1".into(),
                flavor: Some("mainline".into()),
                ..KernelFacts::default()
            }),
            builder: Some(BuilderFacts {
                version: "0.4.2".into(),
                commit: None,
                dirty: false,
                config_commit: None,
                config_dirty: false,
                host_arch: "x86_64".into(),
                target_arch: "arm64".into(),
                cross_compile: "aarch64-linux-gnu-".into(),
                archives: BTreeMap::new(),
            }),
            ..Side::default()
        };
        let merged = lock_side.merge(prov_side);
        let kernel = merged.kernel.as_ref().unwrap();
        // The lock's value where it has one, the manifest's where it does not.
        assert_eq!(kernel.reference.as_deref(), Some("v7.1.5"));
        assert_eq!(kernel.flavor.as_deref(), Some("mainline"));
        assert!(kernel.source.is_some());
        assert!(merged.builder.is_some());
        // The label is the left side's: merge folds facts in, it does not rename.
        assert_eq!(merged.label, "turing-rk1/forky");
    }

    /// The case the report exists to make mechanical: the config held still and the
    /// output moved anyway, because the archive underneath it did.
    #[test]
    fn an_archive_that_moved_under_an_unchanged_config_is_its_own_section() {
        let facts = |release: &str| BuilderFacts {
            version: "0.4.2".into(),
            commit: Some("deadbeef".into()),
            dirty: false,
            config_commit: None,
            config_dirty: false,
            host_arch: "x86_64".into(),
            target_arch: "arm64".into(),
            cross_compile: "aarch64-linux-gnu-".into(),
            archives: [("https://deb.debian.org/debian".to_string(), release.into())]
                .into_iter()
                .collect(),
        };
        let left = Side {
            label: "july".into(),
            builder: Some(facts("aaa")),
            ..Side::default()
        };
        let right = Side {
            label: "august".into(),
            builder: Some(facts("bbb")),
            ..Side::default()
        };
        let Section::Compared { changes } = compare(&left, &right).builder else {
            panic!("both sides have a provenance manifest");
        };
        assert!(changes.version.is_none());
        assert_eq!(changes.archives.len(), 1);
        assert_eq!(changes.archives[0].mirror, "https://deb.debian.org/debian");
        assert_eq!(
            changes.archives[0].release_sha256,
            Change {
                from: Some("aaa".into()),
                to: Some("bbb".into())
            }
        );
    }

    #[test]
    fn a_report_over_two_identical_sides_is_quiet() {
        let mut side = Side::from_lock("same", &base_lock());
        side.packages = Some(vec![pkg("libc6", "2.41-1", "aa")]);
        side.kconfig = Some(FragmentSet::merge([("f.config", "CONFIG_A=y\n")]));
        let report = compare(&side, &side);
        assert!(report.is_quiet(), "{report:?}");
        // Quiet is not the same as unavailable: the sections that *were* compared say
        // so, and only the ones neither side records are unavailable.
        assert!(matches!(report.packages, Section::Compared { .. }));
        assert!(matches!(report.kernel, Section::Compared { .. }));
    }
}
