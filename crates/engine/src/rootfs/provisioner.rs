//! The rootfs bootstrap — an in-process `Debian` provisioner run, no external
//! bootstrap binary.
//!
//! [`build_rootfs`] resolves and materializes the device userland with
//! [`ferroday_cage`]'s pure-Rust Debian provisioner — the same library the build
//! sandbox uses ([`crate::sandbox`]) — talking to the archive directly.
//!
//! The image is a **deployed product**, so its files carry real system ownership
//! (`root:shadow` on `/etc/shadow`, `_apt`, `systemd-journald`, the setgid `dbus`
//! and `ssh` helpers). The provisioner therefore runs under ferroday-cage's
//! **subordinate** identity map (real ids, via the `subid` feature's
//! `newuidmap`/`newgidmap`), and the finished tree is emitted with [`Export`],
//! which re-enters that map so the on-host offset ids (`100000 + n`) round-trip to
//! the ids the rootfs intends; a plain host-side `tar` would record the offset ids
//! and miss the `security.*` xattrs a setcap'd binary carries. Device nodes are
//! excluded, as the runtime provides its own.
//!
//! The pipeline, keyed by the resolved [`Plan`](ferroday_cage::provision::debian::Plan):
//!
//! 1. **Resolve** the plan (`Debian::resolve`) — the exact install set with each
//!    `.deb`'s archive-recorded sha256, without downloading. The build resolves
//!    **once**: this one call keys the early-cutoff cache, is handed back to the
//!    bootstrap as the set to install, and becomes the content-pinned manifest, so
//!    all three describe the same packages by construction. The plan is published
//!    beside the tar as a deb822 document carrying the archive state it resolved
//!    against; a later run replays that document instead of resolving
//!    ([`RootfsOptions::pinned_plan`]), which is what `reproduce` does.
//! 2. On a cache miss, **provision** that plan into a staging tree
//!    (`provision::ensure`), with the build's own `.deb`s as a local trusted
//!    `dists/` mirror, the feature repositories, and the pre-install overlay laid in
//!    via [`pre_configure_overlay`](ferroday_cage::provision::debian::DebianBuilder::pre_configure_overlay)
//!    so a package's maintainer scripts see the l10n/depthcharge config as they
//!    configure. A pinned install fetches neither a release nor a package index —
//!    see [`build_debian`].
//! 3. **Customize** the tree boot2deb-side: lay the post-install overlay in, then
//!    run the account/`postinst.d`/depthcharge steps as commands in a subordinate
//!    cage over the finished tree.
//! 4. **Export** the ownership-preserving tar and write the plan manifest; store
//!    both in the shared [`RootfsStore`].
//!
//! The account is created **locked**; the unique per-image first-boot password is
//! spliced into `/etc/shadow` at image assembly, keeping the cached tree reusable. Its
//! `sudoers` drop-in and `authorized_keys` *are* part of the tree, and so part of the
//! [`cache_key`](crate::rootcache::cache_key) — they are resolved config, identical for
//! every image built from one build point, unlike the password.

use super::{
    config, stage_overlay, stage_preinstall_overlay, AptRepo, BootConfig, RootfsArtifacts,
    RootfsOptions, DEFAULT_USER, REQUIRED_INITRD_MODULES,
};
use crate::archfetch::ArchiveFetch;
use crate::bootstrap::components;
use crate::error::EngineError;
use crate::event::{EventSink, Step};
use crate::repo::LocalDistsRepo;
use crate::rootcache::{self, RootfsStore};
use crate::sandbox::{forward_bootstrap_event, StepObserver};
use boot2deb_core::model::{ImageBuild, ResolvedImage};
use boot2deb_core::weight::PlannedWeight;
use ferroday_cage::provision::debian::{Debian, DebianEvent, Plan, Priority, Repository};
use ferroday_cage::provision::{self, Export};
use ferroday_cage::IdentityMap;
use std::path::{Path, PathBuf};

/// The name of the build's own `.deb` pool as a provisioner [`Repository`], and so
/// the stem of the `/etc/apt/sources.list.d/<name>.list` entry ferroday-cage writes
/// for it. This repo is a **build-time-only** trusted `file://` mirror living under a
/// temp dir that is gone by the time the image runs, so [`CUSTOMIZE`] deletes
/// its sources entry before export — unlike the feature repositories, whose entries
/// are meant to persist for on-device updates.
///
/// The pool's own `Release` publishes this as its `Label`, because it *is* that
/// constant: the sources entry, `apt policy`'s rendering, and a `Pin: release l=…` name
/// one archive by construction rather than by two strings staying in step.
const LOCAL_REPO_NAME: &str = crate::repo::POOL_LABEL;

/// Header line of the solved rootfs manifest — the lock's `[rootfs].manifest`, whose
/// sha256 covers this line too, so it is part of the pinned identity.
const ROOTFS_MANIFEST_HEADER: &str =
    "Solved rootfs package manifest: installed name version arch sha256.";

/// Bootstrap `build`'s rootfs per `opts`, emitting the step's
/// [`Event`](crate::event::Event)s to `sink`: the `tar` archive the image node
/// formats and the content-pinned package manifest beside it.
pub fn build_rootfs(
    ib: ImageBuild,
    opts: &RootfsOptions,
    sink: &dyn EventSink,
) -> Result<RootfsArtifacts, EngineError> {
    let ImageBuild { build, image } = ib;
    let step = Step::start(sink, "rootfs");
    std::fs::create_dir_all(opts.out_dir).map_err(|s| EngineError::io(opts.out_dir, s))?;
    if opts.mirrors.is_empty() {
        return Err(EngineError::Bootstrap {
            context: "configure the rootfs bootstrap".into(),
            message: "no Debian mirror was resolved".into(),
        });
    }
    let arch = build.arch.debian_arch();

    // Caller-owned staging (overlays + local repo). A private temp under the
    // caller's own ownership suffices: the provisioner's acquire runs in-process
    // as the caller, not under a separate mapped `_apt` uid, so nothing here has
    // to be traversable by another identity. Removed on drop.
    //
    // Rooted in the build's scratch tree rather than `TMPDIR` — see
    // [`RootfsOptions::scratch_dir`] for why this node keeps off it.
    std::fs::create_dir_all(opts.scratch_dir).map_err(|s| EngineError::io(opts.scratch_dir, s))?;
    let work = tempfile::Builder::new()
        .prefix("boot2deb-prov-")
        .tempdir_in(opts.scratch_dir)
        .map_err(|s| EngineError::io(opts.scratch_dir, s))?;

    // 1. Overlays: the pre-install tree (laid before maintainer scripts run,
    //    via the provisioner's pre-configure hook) and the customize tree
    //    (laid after, so it wins). Both merge the layer overlays plus this
    //    node's generated config.
    let preinstall = work.path().join("overlay-pre");
    stage_preinstall_overlay(
        &preinstall,
        opts.preinstall_overlay_dirs,
        image,
        opts.boot_config,
        opts.source_date_epoch,
        &step,
    )?;
    let overlay = work.path().join("overlay");
    stage_overlay(
        &overlay,
        opts.overlay_dirs,
        ib,
        opts.rootfs_partuuid,
        opts.image_identity,
        opts.source_date_epoch,
        &step,
    )?;

    // 2. The persistent download cache (content-addressed, reused across
    //    builds), beside boot2deb's own rootfs cache when one is configured.
    let deb_cache = match opts.cache_dir {
        Some(dir) => dir.join("provisioner-debs"),
        None => work.path().join("debs"),
    };

    // 3. The build's own `.deb`s as a trusted `dists/` local mirror, and the
    //    feature repositories, both merged into the provisioner's resolution.
    //
    //    The pool sits beside the download cache rather than in `work`, so it lands
    //    on the volume sized for build artifacts instead of `TMPDIR`, and on a
    //    filesystem that can share extents with the build tree it holds each `.deb`
    //    as a reflink rather than a second copy. It is named per-build because
    //    [`LocalDistsRepo::assemble`] clears the directory first: two builds sharing
    //    one cache directory would otherwise wipe each other's pool, which is a
    //    collision no lock inside `Pool` can resolve. The guard removes it when the
    //    node returns.
    let pool_dir = match opts.cache_dir {
        Some(dir) => dir.join(format!("provisioner-pool-{}", std::process::id())),
        None => work.path().join("localrepo"),
    };
    let _pool = PoolDir(pool_dir.clone());
    let localrepo = LocalDistsRepo::assemble(
        &pool_dir,
        opts.repo_debs,
        &image.suite,
        arch,
        opts.source_date_epoch,
        &step,
    )?;

    let tarball = opts.out_dir.join(format!("{}-rootfs.tar", opts.stem));

    // 4. Obtain the plan: read the one a `reproduce` run handed in, or resolve one
    //    against the mirrors. The plan is the cache key, the install set, and the
    //    manifest, so it is taken up front, before the cache is even consulted. The
    //    progress sink is bound per call, so its borrow of `step` lasts one run and
    //    never outlives the provisioner into the logging below.
    let (plan, plan_document) =
        match opts.pinned_plan {
            Some(path) => read_pinned_plan(path, &step)?,
            None => {
                let mut sink_fn = |event: DebianEvent<'_>| forward_bootstrap_event(&step, event);
                let mut resolver = build_debian(
                    ib,
                    opts,
                    localrepo.file_url(),
                    feature_repositories(opts.apt_sources)?,
                    &deb_cache,
                    &preinstall,
                    None,
                )?;
                step.log("resolving the rootfs package plan (ferroday-cage provisioner)");
                let plan = resolver.observe(&mut sink_fn).resolve().map_err(|e| {
                    EngineError::Bootstrap {
                        context: "resolve the rootfs plan".into(),
                        message: e.to_string(),
                    }
                })?;
                drop(resolver);
                step.log(format!(
                    "resolved {} packages ({} mirror(s), {} local .deb(s))",
                    plan.packages.len(),
                    opts.mirrors.len(),
                    opts.repo_debs.len()
                ));
                let document = plan.to_document().map_err(|e| EngineError::Bootstrap {
                    context: "render the rootfs plan document".into(),
                    message: e.to_string(),
                })?;
                (plan, document)
            }
        };
    // The plan document is published beside the tar whether the plan was resolved or
    // replayed, so every build's artifact set has the same shape and a replay's own
    // output is itself replayable. It is written outside the cache branch because a
    // cache hit skips the bootstrap, not the plan — the resolve above already happened.
    let plan_out = opts.out_dir.join(format!("{}.plan", opts.stem));
    std::fs::write(&plan_out, plan_document.as_bytes())
        .map_err(|s| EngineError::io(&plan_out, s))?;
    step.log(format!(
        "wrote the plan document ({} packages, {} archive(s)) to {}",
        plan.packages.len(),
        plan.archives.len(),
        plan_out.display()
    ));

    // 5. Early-cutoff cache, keyed by the plan's solved set plus the overlay
    //    and local-repo content: a moved mirror resolves a different plan, so a
    //    hit can never be stale.
    let cache = match opts.cache_dir {
        Some(dir) => {
            let key = rootfs_key(ib, opts, &plan, &preinstall, &overlay)?;
            step.log(format!("rootfs cache key {}", key.short()));
            Some((RootfsStore::new(dir), key))
        }
        None => None,
    };
    let hit = match &cache {
        Some((store, key)) if !opts.refresh => store.get(key),
        _ => None,
    };

    if let Some(hit) = hit {
        let key = &cache.as_ref().expect("hit implies a cache").1;
        step.log(format!(
            "rootfs cache hit {} — restoring, skipping bootstrap",
            key.short()
        ));
        std::fs::copy(&hit.tar, &tarball).map_err(|s| EngineError::io(&hit.tar, s))?;
        std::fs::copy(&hit.manifest, opts.manifest_out)
            .map_err(|s| EngineError::io(&hit.manifest, s))?;
        step.progress(75);
    } else {
        step.progress(20);
        step.log(format!(
            "bootstrapping {} {} rootfs into a staging tree (subordinate id-map, real ownership)",
            build.arch, image.suite,
        ));
        // A transient provisioned tree carrying subordinate-mapped ownership
        // the caller cannot unlink; the guard removes it through the map. Kept
        // out of `work` so its TempDir drop is not asked to remove ids it
        // cannot, and out of `TMPDIR` because it is the whole target userland —
        // see [`RootfsOptions::scratch_dir`]. `provision::ensure` requires a
        // non-existent destination.
        //
        // Per-pid so two concurrent builds of one recipe do not collide, and swept
        // by prefix first: a hard-killed run leaves a tree only `provision::remove`
        // can reclaim, and it sits in the work dir a later `clean` will try to
        // delete as the caller.
        let rootfs_dir = opts
            .scratch_dir
            .join(format!("{PROVISIONED_PREFIX}{}", std::process::id()));
        sweep_provisioned(opts.scratch_dir);
        // The bootstrap installs the plan resolved above, verbatim. That is what
        // makes the cache key, the installed set, and the manifest one claim
        // rather than three: a bootstrap left to resolve for itself could pick up
        // an archive that published in between, and the entry cached under the
        // earlier key would then hold a tar and a manifest describing a different
        // set. Handing the plan back also drops the second resolution's release
        // and index fetches, which are the bulk of what a resolve costs.
        let mut installer = build_debian(
            ib,
            opts,
            localrepo.file_url(),
            feature_repositories(opts.apt_sources)?,
            &deb_cache,
            &preinstall,
            Some(plan.clone()),
        )?;
        let mut bootstrap_sink = |event: DebianEvent<'_>| forward_bootstrap_event(&step, event);
        provision::ensure(&rootfs_dir, &mut installer.observe(&mut bootstrap_sink)).map_err(
            |e| EngineError::Bootstrap {
                context: "provision the rootfs".into(),
                message: e.to_string(),
            },
        )?;
        let _provisioned = ProvisionedRoot(rootfs_dir.clone());
        step.progress(55);

        customize(
            &rootfs_dir,
            &overlay,
            image,
            opts.boot_config,
            DEFAULT_USER,
            &step,
        )?;
        step.progress(65);

        export_rootfs_tar(&rootfs_dir, &tarball, opts.source_date_epoch, &step)?;
        write_plan_manifest(&plan, opts.manifest_out, &step)?;
        step.progress(70);

        if let Some((store, key)) = &cache {
            store.put(key, &tarball, opts.manifest_out, &step)?;
        }
        step.progress(75);
        // `_provisioned` drops here: the staging tree is removed through the map.
    }

    step.progress(100);
    step.finish();
    Ok(RootfsArtifacts {
        tar: tarball,
        manifest: opts.manifest_out.to_path_buf(),
        plan: plan_out,
    })
}

/// Read a plan document a `reproduce` run handed in, returning the parsed plan and the
/// document's own bytes.
///
/// Both halves are returned because they are not interchangeable. The plan drives the
/// install; the *document* is what gets republished, verbatim, so the artifact this run
/// leaves behind is the one it was given rather than this build's re-rendering of it. A
/// document may carry fields written by a newer library than this one links — those are
/// carried through a round trip, but re-emitted after the fields this version knows
/// rather than where they were, so a re-render is a different file with the same
/// meaning. Publishing the original keeps the digest a reader compares against stable.
///
/// The plan's suite, architecture, and archive count are checked against the configured
/// bootstrap by the provisioner library at `build()` time, so they are not re-checked
/// here — a mismatch surfaces as its own configuration error naming both values.
fn read_pinned_plan(path: &Path, step: &Step) -> Result<(Plan, String), EngineError> {
    let document = std::fs::read_to_string(path).map_err(|s| EngineError::io(path, s))?;
    let plan = Plan::parse_document(&document).map_err(|e| EngineError::PlanDocument {
        path: path.display().to_string(),
        message: e.to_string(),
    })?;
    step.log(format!(
        "replaying the pinned plan {} ({} packages, {} archive(s)) — the archive is not consulted",
        path.display(),
        plan.packages.len(),
        plan.archives.len()
    ));
    Ok((plan, document))
}

/// Read a published plan document into what the provenance manifest records of it.
///
/// The digest is of the file's bytes, and the archive rows are parsed from the same
/// read, so the manifest describes the document that was published rather than a value
/// carried alongside it. That also lets an `--stage image` re-run over an existing
/// rootfs tar record the plan the earlier rootfs stage left in the output directory,
/// the same way it records that stage's solved manifest.
pub fn read_plan_record(path: &Path) -> Result<PlanRecord, EngineError> {
    let bytes = std::fs::read(path).map_err(|s| EngineError::io(path, s))?;
    let text = String::from_utf8(bytes.clone()).map_err(|_| EngineError::PlanDocument {
        path: path.display().to_string(),
        message: "the document is not valid UTF-8".into(),
    })?;
    let plan = Plan::parse_document(&text).map_err(|e| EngineError::PlanDocument {
        path: path.display().to_string(),
        message: e.to_string(),
    })?;
    Ok(PlanRecord {
        sha256: crate::blobs::sha256_hex(&bytes),
        archives: archive_records(&plan),
    })
}

/// What a published plan document contributes to the provenance manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRecord {
    /// Lowercase-hex sha256 of the document's bytes on disk.
    pub sha256: String,
    /// One row per repository the plan resolved against, in configuration order.
    pub archives: Vec<boot2deb_core::provenance::ArchiveProvenance>,
}

/// Read a published plan document into what a size report rolls up.
///
/// The plan is the right file to read this from, and the solved manifest is the wrong
/// one. That manifest is a **content pin** — its sha256 is committed in the lock's
/// `RootfsPin.manifest_sha256`, and a mismatch is a hard `ManifestDrift` — so adding
/// columns to it would invalidate every committed pin for a reason that has nothing to
/// do with the package set. The plan carries the same rows plus the archive's own
/// `Installed-Size` and `Source`, and nothing pins its shape.
///
/// A document written before those fields existed parses fine and reports every size as
/// absent; a report says how many that was rather than presenting the result as
/// complete.
pub fn read_plan_weights(path: &Path) -> Result<PlanWeights, EngineError> {
    let text = std::fs::read_to_string(path).map_err(|s| EngineError::io(path, s))?;
    let plan = Plan::parse_document(&text).map_err(|e| EngineError::PlanDocument {
        path: path.display().to_string(),
        message: e.to_string(),
    })?;
    Ok(PlanWeights {
        packages: plan
            .packages
            .iter()
            .map(|p| PlannedWeight {
                name: p.name.clone(),
                version: p.version.clone(),
                source: p.source.clone(),
                installed_kib: p.installed_size,
                archive: p.archive,
            })
            .collect(),
        archives: archive_records(&plan),
    })
}

/// A published plan document's package rows and the repositories they name.
///
/// Both halves come from one read, so the labels a report puts on its archive rows are
/// the ones that document's own packages index into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanWeights {
    /// One row per package the plan installs, in document order.
    pub packages: Vec<PlannedWeight>,
    /// One row per repository, in configuration order — the same projection the
    /// provenance manifest's `[[archives]]` carries, so a `file://` pool is marked
    /// local and its build-host path is dropped here too.
    pub archives: Vec<boot2deb_core::provenance::ArchiveProvenance>,
}

/// Project a plan's archive states into the provenance manifest's `[[archives]]` rows.
///
/// A repository served over `file://` is marked local and its URL dropped: that URL is a
/// path on the build host — for the build's own `.deb` pool, a per-run path under a
/// per-run directory — and a record whose value is being portable may not carry one, the
/// same reason the sandbox record drops a run's own working and artifact binds. Every
/// other field is carried across as the resolve reported it.
fn archive_records(plan: &Plan) -> Vec<boot2deb_core::provenance::ArchiveProvenance> {
    plan.archives
        .iter()
        .enumerate()
        .map(|(index, archive)| {
            let local = archive.mirror.starts_with("file://");
            boot2deb_core::provenance::ArchiveProvenance {
                index,
                local,
                mirror: (!local).then(|| archive.mirror.clone()),
                suite: archive.suite.clone(),
                components: archive.components.clone(),
                release_sha256: archive.release_sha256.clone(),
                date: archive.date.clone(),
                valid_until: archive.valid_until.clone(),
                signed_by: archive.signed_by.clone(),
                signing_key: archive.signing_key.clone(),
            }
        })
        .collect()
}

/// Removes a provisioned staging tree through the subordinate map on drop, so a
/// tree whose non-root files the plain caller cannot unlink never leaks — the
/// bundled delegate chain (including the `subid` `newuidmap`/`newgidmap` helpers)
/// re-enters the map to delete it. Best-effort: a removal failure is swallowed, as
/// there is nothing more to do with it in a destructor.
struct ProvisionedRoot(PathBuf);

impl Drop for ProvisionedRoot {
    fn drop(&mut self) {
        let _ = provision::remove(&self.0);
    }
}

/// Name prefix of a transient provisioned rootfs under the build's scratch tree.
///
/// Public through [`sweep_provisioned`] because such a tree is the one thing in the
/// work dir the calling user **cannot** delete: the provisioner gives it real
/// ownership through a subordinate id-map, so its files belong to subuids outside the
/// caller's own. Only `provision::remove` — which re-enters that map — can reclaim it.
/// A run that completes removes its own through [`ProvisionedRoot`]; one that is
/// hard-killed does not, and a later `clean` would then fail on a directory it has no
/// permission to unlink.
const PROVISIONED_PREFIX: &str = "prov-rootfs-";

/// Reclaim every provisioned rootfs left in `scratch_dir` by an earlier run, through
/// the id-map that owns them (those named `prov-rootfs-*`).
///
/// Best-effort and quiet: a tree that resists removal is not a reason to fail the
/// operation that called this, and a live concurrent build's own tree is skipped
/// naturally — `provision::remove` on a directory another process still holds fails,
/// leaving it alone. Called at the start of the rootfs node and by `clean`, so the
/// user-facing rule is simply that the work dir is always removable.
pub fn sweep_provisioned(scratch_dir: &Path) {
    let Ok(entries) = std::fs::read_dir(scratch_dir) else {
        return;
    };
    for entry in entries.flatten() {
        if entry
            .file_name()
            .to_string_lossy()
            .starts_with(PROVISIONED_PREFIX)
        {
            let _ = provision::remove(entry.path());
        }
    }
}

/// Removes the build's local `.deb` pool on drop. The pool lives beside the download
/// cache rather than under the node's `TempDir`, so nothing else would reclaim it.
/// Every file in it is the pool's own — `Pool::publish` gives each package its own
/// inode rather than aliasing the build's — so a plain recursive delete cannot reach a
/// build artifact. Best-effort, as a destructor has nothing to do with a failure.
struct PoolDir(PathBuf);

impl Drop for PoolDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Assemble the [`Debian`] provisioner from the resolved build and options, either to
/// **resolve** a plan (`plan` is `None`) or to **install** one it was already given.
///
/// The resolved mirror list is configured on the builder, primary first; a
/// `snapshot.debian.org` mirror anywhere in it relaxes the release freshness check
/// ([`snapshot::has_snapshot`](crate::snapshot::has_snapshot)); the local trusted `dists/` pool and the
/// feature repositories are merged in as additional [`Repository`] sources. The
/// subordinate map gives the tree real ownership, the pre-install overlay is handed to
/// the provisioner's pre-configure hook, and the download cache is content-addressed
/// for reuse. All of that describes *how* a bootstrap runs and applies to both modes.
///
/// What differs is *what* it installs, and the two are mutually exclusive by the
/// library's own rule:
///
/// - **Resolving.** The base seeds the `important` variant — a full base system, unlike
///   the build sandbox's minimal one — the resolved package set and the build's
///   `extra_packages` are the includes, and the resolved excludes are dropped.
/// - **Installing.** A pinned [`Plan`] already names every package, so the selectors
///   that would shape a resolution contradict it and are refused at `build()` with a
///   configuration error; they are omitted here rather than passed and rejected. The
///   bootstrap then fetches exactly the plan's packages by the digests it records,
///   touching neither a release nor a package index — the resolve those pay for already
///   happened.
///
/// Both modes configure the same repositories in the same order, which is what lets a
/// plan resolved by the first be installed by the second: a planned package names its
/// archive as an index into that list.
///
/// The provisioner borrows nothing from the caller — its progress sink is bound
/// per call by [`Debian::observe`] — so it is a `Debian<'static>` that outlives
/// every observed run.
fn build_debian(
    ib: ImageBuild,
    opts: &RootfsOptions,
    local_url: &str,
    feature_repos: Vec<Repository>,
    deb_cache: &Path,
    preinstall: &Path,
    plan: Option<Plan>,
) -> Result<Debian<'static>, EngineError> {
    let ImageBuild { build, image } = ib;
    let arch = build.arch.debian_arch();
    let (primary, fallbacks) = opts
        .mirrors
        .split_first()
        .expect("mirrors are non-empty (checked by the caller)");

    let mut b = Debian::builder(&image.suite)
        .architecture(arch)
        .components(components(image).split(','))
        .identity_map(IdentityMap::Subordinate)
        .cache_dir(deb_cache)
        .pre_configure_overlay(preinstall)
        // A feature repository can be published at any URL its vendor chose, and the
        // library's own client speaks no TLS — so the transport is boot2deb's to
        // supply. See [`ArchiveFetch`].
        .fetcher(Box::new(ArchiveFetch::new()))
        .mirror(primary);
    b = match plan {
        Some(plan) => b.plan(plan),
        None => b
            .base_priority(Priority::Important)
            .include(image.rootfs_packages.iter().cloned())
            .include(opts.extra_packages.iter().cloned())
            .exclude(image.rootfs_exclude.iter().cloned()),
    };
    for fallback in fallbacks {
        b = b.mirror_fallback(fallback);
    }
    // A point-in-time archive's release is expired by design; accepting a
    // signed-but-stale release is a repository-wide posture.
    if crate::snapshot::has_snapshot(opts.mirrors) {
        b = b.allow_stale_release(true);
    }
    if let Some(keyring) = opts.keyring {
        b = b.keyring(keyring);
    }

    // The build's own `.deb`s: a trusted `file://` pool, apt's `[trusted=yes]`.
    let local = Repository::builder(&image.suite)
        .mirror(local_url)
        .components(["main"])
        .trust_unsigned(true)
        .name(LOCAL_REPO_NAME)
        .build()
        .map_err(|e| EngineError::Bootstrap {
            context: "configure the local .deb repository".into(),
            message: e.to_string(),
        })?;
    b = b.repository(local);
    for repo in feature_repos {
        b = b.repository(repo);
    }

    b.build().map_err(|e| EngineError::Bootstrap {
        context: format!("configure the {} {} bootstrap", build.arch, image.suite),
        message: e.to_string(),
    })
}

/// Build a signed [`Repository`] per resolved feature apt source (e.g. Jellyfin),
/// each verified against its own keyring and carrying its own suite/components, so
/// an out-of-mirror app resolves in the provisioner's closure. Each writes its own
/// `/etc/apt/sources.list.d/<name>.list` into the finished rootfs.
///
/// The source's `name` is passed through verbatim, because resolution already held it
/// to the portable file-name stem a repository name accepts. Reducing an
/// out-of-set name to a legal one instead would not be safer: the map is not
/// injective, so two sources resolution accepted as distinct could land on one
/// `sources.list.d` entry, and the repository whose line lost would be absent from the
/// finished image with its packages already installed. An unusable name is rejected
/// where it is authored.
pub(crate) fn feature_repositories(
    apt_sources: &[AptRepo],
) -> Result<Vec<Repository>, EngineError> {
    apt_sources
        .iter()
        .map(|repo| {
            Repository::builder(&repo.source.suite)
                .mirror(&repo.source.uri)
                .components(repo.source.components.iter().cloned())
                .keyring(&repo.keyring)
                .name(&repo.source.name)
                .build()
                .map_err(|e| EngineError::Bootstrap {
                    context: format!("configure feature repository {}", repo.source.name),
                    message: e.to_string(),
                })
        })
        .collect()
}

/// The rootfs cache key: the plan's solved set plus the two staged overlay trees'
/// content, the local-repo `.deb`s' content, the feature repositories, the
/// interpreter that configures the tree, and the account policy the customize step
/// writes into it. Keying on the *solved* set is what makes a hit safe — a moved mirror
/// resolves a different plan, hence a different key, and rebuilds.
///
/// The account policy has to be folded explicitly because it is the one part of the
/// tree the customize script writes from resolved config rather than from a staged
/// file: the overlay fingerprints below cannot see it, so without it a build that added
/// an authorized key or tightened `sudo` would key alike with one that had not, and be
/// served the older tree.
fn rootfs_key(
    ib: ImageBuild,
    opts: &RootfsOptions,
    plan: &Plan,
    preinstall: &Path,
    overlay: &Path,
) -> Result<crate::signature::Signature, EngineError> {
    let ImageBuild { build, image } = ib;
    let solved = plan_solved(plan);
    let mut overlay_fp = Vec::new();
    for (stage, dir) in [("overlay", overlay), ("overlay-pre", preinstall)] {
        overlay_fp.extend(
            rootcache::dir_fingerprints(dir)?
                .into_iter()
                .map(|record| format!("{stage}\0{record}")),
        );
    }
    let repo_fp = rootcache::file_fingerprints(opts.repo_debs)?;
    let apt_fp = apt_source_records(opts.apt_sources)?;
    let arch = build.arch.to_string();
    Ok(rootcache::cache_key(&rootcache::CacheKeyInputs {
        solved: &solved,
        overlay: &overlay_fp,
        repo_debs: &repo_fp,
        apt_sources: &apt_fp,
        arch: &arch,
        suite: &image.suite,
        components: components(image),
        interpreter: opts.interpreter_id,
        sudo: image.sudo.as_str(),
        authorized_keys: &image.ssh_authorized_keys,
    }))
}

/// One cache-key record per feature apt repository: the source's identity plus the
/// *content* of the keyring it is verified against.
///
/// Both halves reach the image directly — [`feature_repositories`] hands each to the
/// provisioner, which writes its `sources.list.d` entry and its keyring into the
/// finished tree — so a re-pointed URI or a rotated key changes what the image carries
/// without moving a single package version. Fields are NUL-separated for the same
/// reason [`rootcache::dir_fingerprints`] does it: no value can forge a field boundary.
///
/// A keyring that cannot be read is an error, not a skipped record: the CLI resolved
/// and existence-checked the path, so a failure here means the build is about to
/// verify a repository against a keyring it does not have.
fn apt_source_records(sources: &[AptRepo]) -> Result<Vec<String>, EngineError> {
    sources
        .iter()
        .map(|repo| {
            let bytes =
                std::fs::read(&repo.keyring).map_err(|s| EngineError::io(&repo.keyring, s))?;
            let source = repo.source;
            Ok(format!(
                "{}\0{}\0{}\0{}\0{}",
                source.name,
                source.uri,
                source.suite,
                source.components.join(" "),
                crate::blobs::sha256_hex(&bytes),
            ))
        })
        .collect()
}

/// The plan's packages as `name version arch` lines — the solved set folded into
/// the cache key. Order-insensitive there, so this does not sort.
fn plan_solved(plan: &Plan) -> Vec<String> {
    plan.packages
        .iter()
        .map(|p| format!("{} {} {}", p.name, p.version, p.architecture))
        .collect()
}

/// Write the content-pinned manifest from the resolved plan, the lock's
/// `[rootfs].manifest`.
fn write_plan_manifest(plan: &Plan, out: &Path, step: &Step) -> Result<(), EngineError> {
    let count = crate::manifest::write(ROOTFS_MANIFEST_HEADER, plan, out)?;
    step.log(format!(
        "wrote solved manifest ({count} packages, sha256-pinned from the plan) to {}",
        out.display()
    ));
    Ok(())
}

/// Customize the provisioned tree: lay the post-install overlay in with
/// [`provision::CopyIn`], then run the account, kernel-`postinst.d`, l10n, and
/// depthcharge steps as commands in a subordinate cage over the finished tree.
///
/// The copy is the mirror of [`export_rootfs_tar`]'s [`Export`]: both enter the
/// subordinate map, one to write the tree at the ownership it intends and one to read it
/// back. No host tool is on this path — the same posture the filesystem write and the
/// bootstrap hold to.
fn customize(
    rootfs: &Path,
    overlay: &Path,
    image: &ResolvedImage,
    boot: Option<BootConfig>,
    user: &str,
    step: &Step,
) -> Result<(), EngineError> {
    // Lay the customize overlay (layer trees + generated config) into the rootfs
    // through the same subordinate map the tree was provisioned under, so each entry
    // is created at the ownership the rootfs intends: the copy forks a child into the
    // map and reads each source id *backwards* through it, so a caller-owned staged
    // file lands as root inside — and a file the host stores at `100000 + n` would land
    // as uid n, which is the thing a caller-side `cp` structurally cannot express.
    // A directory already in the rootfs is kept and descended into; a file or symlink
    // at an existing path is replaced.
    step.log("laying the customize overlay into the provisioned rootfs");
    let report = provision::CopyIn::new(overlay, rootfs)
        .map(IdentityMap::Subordinate)
        .run()
        .map_err(|e| EngineError::Bootstrap {
            context: "lay the customize overlay into the rootfs".into(),
            message: e.to_string(),
        })?;
    // A device node or socket the copy could not create is not something these trees
    // hold — they are git-tracked config, and the runtime supplies its own `/dev` — so
    // a non-empty list means the staged tree gained something unexpected, and a rootfs
    // quietly missing a file it was told to carry is exactly the failure the report
    // exists to prevent. Fail rather than log it past.
    if !report.skipped.is_empty() {
        let listed = report
            .skipped
            .iter()
            .map(|entry| format!("{} ({})", entry.path.display(), entry.kind))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(EngineError::Bootstrap {
            context: "lay the customize overlay into the rootfs".into(),
            message: format!(
                "the staged overlay holds {} entry/entries the copy cannot create, so the \
                 rootfs would be missing them: {listed}",
                report.skipped.len()
            ),
        });
    }

    // Run the target-side config in one cage over the rootfs (rootfs = `/`), under
    // the same subordinate map the tree was built with. Target binaries
    // (`useradd`, `run-parts`, `depthchargectl`) run directly, or via the host's
    // `qemu-user` binfmt when cross-arch — as the build sandbox's do.
    step.log("running the target-chroot customize steps in a cage");
    run_customize_cage(rootfs, &customize_env(user, image, boot), step)
}

/// The target-side customize program: one committed POSIX `sh` file, byte-identical for
/// every build.
///
/// **Nothing this build resolved is interpolated into it.** Every value arrives through
/// the environment ([`customize_env`]), so a hostname carrying a newline or a board
/// profile spelling a heredoc delimiter cannot change what runs — it can only be a wrong
/// value in a right program. That is the same split
/// [`core::expect`](boot2deb_core::expect) and the on-image selftest runner use, and it
/// is what makes the program reviewable as a file rather than as the output of six
/// `format!` calls: `shellcheck` reads it, a diff shows what changed, and the tests below
/// drive these exact bytes through `dash`.
const CUSTOMIZE: &str = include_str!("customize/customize.sh");

/// The environment [`CUSTOMIZE`] reads, from the resolved build.
///
/// Every entry is a value, never syntax. The absent cases are the empty string rather
/// than a missing variable, because the script runs under `set -u` and a branch on
/// `[ -n "$X" ]` reads better than one on whether a variable exists at all: an image
/// that authorizes nobody, generates no locale, or boots from a raw gap takes the empty
/// string and the script's own `if` decides.
fn customize_env(
    user: &str,
    image: &ResolvedImage,
    boot: Option<BootConfig>,
) -> Vec<(String, String)> {
    let depthcharge_board = match boot {
        Some(BootConfig::Depthcharge { board, .. }) => board,
        _ => "",
    };
    vec![
        ("B2D_USER".into(), user.to_string()),
        ("B2D_SUDOERS".into(), image.sudo.sudoers_spec().to_string()),
        // One key per line, exactly as authored. Resolution guarantees each entry is a
        // single well-formed `authorized_keys` line, which is what makes the join
        // unambiguous.
        (
            "B2D_AUTHORIZED_KEYS".into(),
            image.ssh_authorized_keys.join("\n"),
        ),
        ("B2D_LOCAL_REPO".into(), LOCAL_REPO_NAME.to_string()),
        (
            "B2D_INITRAMFS_STUB".into(),
            crate::rootfs::INITRAMFS_STUB.to_string(),
        ),
        (
            "B2D_INITRAMFS_STUB_LOG".into(),
            crate::rootfs::INITRAMFS_STUB_LOG.to_string(),
        ),
        ("B2D_TIMEZONE".into(), image.timezone.clone()),
        // A flag, not the list: the script only asks whether `locale-gen` was given
        // anything to do, and the list itself reaches the image through
        // `/etc/locale.gen` in the pre-install overlay.
        (
            "B2D_LOCALES_GENERATED".into(),
            if image.locales_generate.is_empty() {
                String::new()
            } else {
                "1".to_string()
            },
        ),
        (
            "B2D_DEPTHCHARGE_BOARD".into(),
            depthcharge_board.to_string(),
        ),
        // The armed form — `enable-system-hooks = True` — which the build-time config in
        // the pre-install overlay deliberately is not: during the build the hooks must
        // not hunt the build host's disks.
        (
            "B2D_DEPTHCHARGE_CONFIG".into(),
            if depthcharge_board.is_empty() {
                String::new()
            } else {
                config::depthcharge_config(depthcharge_board, true)
            },
        ),
        (
            "B2D_REQUIRED_INITRD_MODULES".into(),
            REQUIRED_INITRD_MODULES.join(" "),
        ),
    ]
}

/// Run [`CUSTOMIZE`] in a subordinate-mapped cage rooted at `rootfs`, streaming its
/// output to `step`. Customize needs no network, and the profile's
/// [`Network::Isolated`] gives it none.
///
/// It runs under the same [`baseline`](crate::sandbox::baseline) profile as the package
/// stages, and adds only the subordinate map its ownership-preserving tree needs. The
/// maintainer scripts it runs are sensitive to `LC_ALL`, `TZ`, and `DEBIAN_FRONTEND`,
/// and the profile declares all three — so what they see is the environment the image's
/// provenance records. The `B2D_*` entries are this run's own, applied over the
/// profile's.
fn run_customize_cage(
    rootfs: &Path,
    env: &[(String, String)],
    step: &Step,
) -> Result<(), EngineError> {
    let mut builder = crate::sandbox::baseline(rootfs)
        .identity_map(IdentityMap::Subordinate)
        .command("sh")
        .args(["-c", CUSTOMIZE])
        .current_dir("/");
    for (key, value) in env {
        builder = builder.env(key, value);
    }
    let cage = builder.build().map_err(|source| EngineError::Sandbox {
        context: "customize the rootfs".into(),
        source,
    })?;
    let mut observer = StepObserver::new(step);
    let status = cage
        .run_with(&mut observer)
        .map_err(|source| EngineError::Sandbox {
            context: "customize the rootfs".into(),
            source,
        })?;
    observer.flush();
    if status.success() {
        Ok(())
    } else {
        Err(EngineError::CommandFailed {
            command: "sh".into(),
            context: "customize the rootfs".into(),
            status: status.code(),
            stderr: observer.stderr_tail(),
        })
    }
}

/// Export the provisioned tree to the ownership-preserving `tar` the image node
/// formats. [`provision::export_tar`] re-enters the subordinate map, so the
/// on-host offset ids round-trip to the ids the rootfs intends, and the setcap
/// `security.*` xattrs and setgid ownerships come through; device nodes are
/// excluded, as the runtime provides its own.
///
/// `source_date_epoch` is the `SOURCE_DATE_EPOCH` ceiling: each member's mtime is
/// recorded as `min(mtime, epoch)`, pulling the bootstrap's wall-clock stamps down
/// to the epoch so only the deliberate per-image secret varies between builds of
/// one lock. `None` (a rootfs-only build with no
/// kernel tree to date) records the real times.
fn export_rootfs_tar(
    rootfs: &Path,
    tarball: &Path,
    source_date_epoch: Option<u64>,
    step: &Step,
) -> Result<(), EngineError> {
    step.log(format!(
        "exporting ownership-preserving rootfs tar to {}",
        tarball.display()
    ));
    let file = std::fs::File::create(tarball).map_err(|s| EngineError::io(tarball, s))?;
    let writer = std::io::BufWriter::new(file);
    let mut export = Export::new(rootfs).map(IdentityMap::Subordinate);
    // The tar encoder applies the clamp as it writes: under the subordinate map the
    // provisioned files sit at ids the host user cannot set times on, so the encoder
    // is the one place that can pull an mtime down to the epoch.
    if let Some(epoch) = source_date_epoch {
        export = export.clamp_mtime(epoch as i64);
    }
    export.write_to(writer).map_err(|e| EngineError::Bootstrap {
        context: "export the rootfs tar".into(),
        message: e.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use boot2deb_core::model::ResolvedBuild;

    /// The image half of a fixture build. Every fixture here resolves a shipped image
    /// recipe, so the axis is there; the unwrap states that rather than threading an
    /// `Option` through every assertion.
    fn image_of(build: &boot2deb_core::ResolvedBuild) -> &boot2deb_core::ResolvedImage {
        pair_of(build).image
    }

    /// The same fixture build as an [`ImageBuild`] pair, for the stages that take one.
    fn pair_of(build: &boot2deb_core::ResolvedBuild) -> boot2deb_core::ImageBuild<'_> {
        build.as_image().expect("the fixture recipes build images")
    }
    use super::*;
    use boot2deb_core::model::{AptSource, InitramfsCompress, SudoPolicy};
    use boot2deb_core::{resolve_recipe, ConfigRoot, Overrides};
    use std::process::Command;

    fn repo_root() -> ConfigRoot {
        ConfigRoot::new(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(2)
                .unwrap()
                .to_path_buf(),
        )
    }

    /// The media-accel build (carries ffmpeg-rk et al. + depthcharge is absent).
    fn rk1() -> ResolvedBuild {
        resolve_recipe(
            &repo_root(),
            "turing-rk1/media-accel-forky",
            &Overrides::default(),
        )
        .unwrap()
    }

    /// A depthcharge board with a distro kernel.
    fn c201() -> ResolvedBuild {
        resolve_recipe(&repo_root(), "asus-c201/forky", &Overrides::default()).unwrap()
    }

    /// A minimal plan document standing in for one [`Debian::resolve`] would return, so
    /// the pinned-install configuration is exercisable with no archive to resolve
    /// against. One archive and one package is enough: what is under test is which
    /// *settings* the installing provisioner carries, not what it would install.
    fn sample_plan(suite: &str, architecture: &str) -> Plan {
        Plan::parse_document(&format!(
            "Format: ferroday-cage-plan 2\n\
             Suite: {suite}\n\
             Architecture: {architecture}\n\
             \n\
             Archive: 0\n\
             Mirror: https://deb.debian.org/debian\n\
             Suite: {suite}\n\
             Components: main\n\
             Release-SHA256: {zeros}\n\
             Signed-By:\n\
             Signing-Key:\n\
             \n\
             Package: base-files\n\
             Version: 13\n\
             Architecture: {architecture}\n\
             SHA256: {zeros}\n\
             Filename: pool/main/b/base-files/base-files_13_{architecture}.deb\n\
             Archive: 0\n",
            zeros = "0".repeat(64),
        ))
        .expect("the document is well-formed")
    }

    /// Options standing in for a build's, carrying only what [`build_debian`] reads:
    /// the mirror list, the excludes/includes, and the paths. The rest is the struct's
    /// own shape.
    fn sample_options<'a>(
        mirrors: &'a [String],
        dir: &'a Path,
        identity: &'a boot2deb_core::provenance::SystemIdentity,
    ) -> RootfsOptions<'a> {
        RootfsOptions {
            repo_debs: &[],
            overlay_dirs: &[],
            preinstall_overlay_dirs: &[],
            boot_config: None,
            image_identity: identity,
            rootfs_partuuid: uuid::Uuid::nil(),
            out_dir: dir,
            stem: "sample",
            scratch_dir: dir,
            keyring: None,
            interpreter_id: None,
            manifest_out: dir,
            pinned_plan: None,
            mirrors,
            extra_packages: &[],
            cache_dir: None,
            refresh: false,
            apt_sources: &[],
            source_date_epoch: None,
        }
    }

    /// A stand-in system identity: [`build_debian`] never reads it, and the options
    /// struct requires one.
    fn sample_identity() -> boot2deb_core::provenance::SystemIdentity {
        use boot2deb_core::provenance::{IdentityImage, IdentityKernel, SystemIdentity};
        SystemIdentity {
            version: 1,
            image: IdentityImage {
                device: "sample".into(),
                description: "sample".into(),
                arch: "arm64".into(),
                soc: "sample".into(),
                boot_method: "rockchip-rkbin".into(),
                board: None,
                suite: "forky".into(),
                features: Vec::new(),
                layout: "combined".into(),
                hostname: "sample".into(),
            },
            kernel: IdentityKernel {
                id: "sample".into(),
                flavor: "mainline".into(),
                package: None,
                reference: None,
                commit: None,
                patch_series: Vec::new(),
            },
            pressed: None,
        }
    }

    /// The build resolves once and installs what it resolved, which the provisioner
    /// library enforces rather than trusts: a pinned plan already names every package,
    /// so `include`, `exclude`, and `base_priority` contradict it and are refused at
    /// `build()`. Constructing the installing provisioner successfully is therefore the
    /// assertion that [`build_debian`] omits all three — and moving any of them out of
    /// the `None` arm and into the shared chain fails this test rather than a build ten
    /// minutes in.
    ///
    /// The resolving provisioner is built from the same options in the same test, so the
    /// two modes are held to differ *only* in that: a change that broke the resolving
    /// configuration would fail here too.
    #[test]
    fn the_installing_provisioner_drops_the_selectors_its_pinned_plan_replaces() {
        let build = rk1();
        let tmp = tempfile::tempdir().unwrap();
        let mirrors = vec!["https://deb.debian.org/debian".to_string()];
        let identity = sample_identity();
        let opts = sample_options(&mirrors, tmp.path(), &identity);
        let deb_cache = tmp.path().join("debs");
        let preinstall = tmp.path().join("overlay-pre");
        let local_url = "file:///nonexistent/localrepo";

        // The build resolves excludes, so the resolving mode really does carry a
        // selector the pinned mode must drop — without that this test would pass
        // vacuously.
        assert!(
            !image_of(&build).rootfs_exclude.is_empty(),
            "the fixture must exercise the selectors the pinned mode omits"
        );
        build_debian(
            pair_of(&build),
            &opts,
            local_url,
            Vec::new(),
            &deb_cache,
            &preinstall,
            None,
        )
        .expect("the resolving provisioner carries the selectors");

        let plan = sample_plan(&image_of(&build).suite, build.arch.debian_arch());
        build_debian(
            pair_of(&build),
            &opts,
            local_url,
            Vec::new(),
            &deb_cache,
            &preinstall,
            Some(plan),
        )
        .expect("a pinned plan is refused alongside include/exclude/base_priority");
    }

    /// A plan document is what a `reproduce` run is handed, so the read has to accept
    /// what a build writes — round-tripped here through the real renderer rather than
    /// against a hand-written string, which would only prove the fixture parses.
    #[test]
    fn a_published_plan_document_reads_back_as_the_plan_that_wrote_it() {
        let plan = sample_plan("forky", "arm64");
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sample.plan");
        std::fs::write(&path, plan.to_document().unwrap()).unwrap();

        let step = Step::start(&|_| {}, "test");
        let (read_back, document) = read_pinned_plan(&path, &step).unwrap();
        assert_eq!(read_back, plan, "the replayed plan is the published one");
        assert_eq!(
            document,
            std::fs::read_to_string(&path).unwrap(),
            "the document is carried verbatim, not re-rendered"
        );
    }

    /// The provenance rows are the manifest's account of what the packages were selected
    /// from. Three properties decide whether that account is portable: a `file://`
    /// repository is the build host's own, so its path must not reach the record; an
    /// unsigned repository must read as unsigned rather than as unrecorded; and the
    /// certificate and the key that signed under it are carried as the two separate
    /// answers they are.
    #[test]
    fn the_archive_rows_drop_a_build_host_path_and_keep_an_empty_signer() {
        let plan = Plan::parse_document(&format!(
            "Format: ferroday-cage-plan 2\n\
             Suite: forky\n\
             Architecture: arm64\n\
             \n\
             Archive: 0\n\
             Mirror: https://deb.debian.org/debian\n\
             Suite: forky\n\
             Components: main non-free-firmware\n\
             Release-SHA256: {zeros}\n\
             Date: Sun, 02 Aug 2026 08:12:34 UTC\n\
             Signed-By: B8B80B5B623EAB6AD8775C45B7C5D7D6350947F8\n\
             Signing-Key: 4CB50190207B4758A3F73A796ED0E7B82643E131\n\
             \n\
             Archive: 1\n\
             Mirror: file:///build-host/cache/provisioner-pool-4242\n\
             Suite: forky\n\
             Components: main\n\
             Release-SHA256: {zeros}\n\
             Signed-By:\n\
             Signing-Key:\n",
            zeros = "0".repeat(64),
        ))
        .unwrap();
        let rows = archive_records(&plan);

        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].index, 0);
        assert!(!rows[0].local);
        assert_eq!(
            rows[0].mirror.as_deref(),
            Some("https://deb.debian.org/debian")
        );
        assert_eq!(rows[0].components, ["main", "non-free-firmware"]);
        // The certificate primary, which is what `blobs/keyrings/*.fingerprints` pins,
        // and the subkey that made the signature — carried side by side, because a
        // certificate rotating its signing subkey moves the second and not the first.
        assert_eq!(
            rows[0].signed_by,
            ["B8B80B5B623EAB6AD8775C45B7C5D7D6350947F8"]
        );
        assert_eq!(
            rows[0].signing_key,
            ["4CB50190207B4758A3F73A796ED0E7B82643E131"]
        );

        assert_eq!(rows[1].index, 1);
        assert!(
            rows[1].local,
            "a file:// repository is the build host's own"
        );
        assert_eq!(
            rows[1].mirror, None,
            "a per-run path on this machine must not reach the record"
        );
        assert!(
            rows[1].signed_by.is_empty() && rows[1].signing_key.is_empty(),
            "an unsigned repository is recorded as unsigned, in both fields"
        );
    }

    /// The digest recorded is of the file, not of a re-rendering of the plan it holds —
    /// which is what keeps it comparable against a document written by a library version
    /// that carried fields this one would re-emit elsewhere.
    #[test]
    fn the_plan_record_digests_the_published_file() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("sample.plan");
        let document = sample_plan("forky", "arm64").to_document().unwrap();
        std::fs::write(&path, &document).unwrap();

        let record = read_plan_record(&path).unwrap();
        assert_eq!(record.sha256, crate::blobs::sha256_hex(document.as_bytes()));
        assert_eq!(record.archives.len(), 1);
    }

    /// A source name is carried to the `sources.list.d` stem as authored, and one
    /// that is not a portable stem fails the build of the repository rather than
    /// being folded into a legal-looking neighbour. Resolution rejects such a name
    /// first; this is the backstop at the boundary that writes the file.
    #[test]
    fn a_feature_repository_takes_its_name_verbatim_or_fails() {
        let tmp = tempfile::tempdir().unwrap();
        let keyring = tmp.path().join("jellyfin.gpg");
        std::fs::write(&keyring, b"KEY-ONE").unwrap();
        let source = AptSource {
            name: "jellyfin".into(),
            uri: "https://repo.jellyfin.org/debian".into(),
            suite: "trixie".into(),
            components: vec!["main".into()],
            signed_by: "jellyfin.gpg".into(),
        };
        let build = |source: &AptSource| {
            feature_repositories(&[AptRepo {
                source,
                keyring: keyring.clone(),
            }])
        };
        assert!(build(&source).is_ok());

        for bad in ["my repo/x", "..", "a:b"] {
            let err = build(&AptSource {
                name: bad.into(),
                ..source.clone()
            })
            .expect_err("a name that is not a portable stem must not build a repository");
            assert!(
                err.to_string().contains(bad),
                "the failure names the offending value, got: {err}"
            );
        }
    }

    #[test]
    fn an_apt_source_record_covers_its_identity_and_its_keyring_bytes() {
        let tmp = tempfile::tempdir().unwrap();
        let keyring = tmp.path().join("jellyfin.gpg");
        std::fs::write(&keyring, b"KEY-ONE").unwrap();
        let source = AptSource {
            name: "jellyfin".into(),
            uri: "https://repo.jellyfin.org/debian".into(),
            suite: "trixie".into(),
            components: vec!["main".into()],
            signed_by: "jellyfin.gpg".into(),
        };
        let record = |source: &AptSource| {
            apt_source_records(&[AptRepo {
                source,
                keyring: keyring.clone(),
            }])
            .unwrap()
        };
        let base = record(&source);

        // Every field of the source's identity reaches the tree's sources entry, so
        // each one moves the record.
        for changed in [
            AptSource {
                name: "jellyfin-unstable".into(),
                ..source.clone()
            },
            AptSource {
                uri: "https://mirror.invalid/jellyfin".into(),
                ..source.clone()
            },
            AptSource {
                suite: "forky".into(),
                ..source.clone()
            },
            AptSource {
                components: vec!["main".into(), "unstable".into()],
                ..source.clone()
            },
        ] {
            assert_ne!(base, record(&changed), "{changed:?} must move the record");
        }

        // And the keyring is folded by content, not by path: a rotated key at the same
        // filename is a different image.
        std::fs::write(&keyring, b"KEY-TWO").unwrap();
        assert_ne!(
            base,
            record(&source),
            "a rotated keyring must move the record"
        );

        // A build whose features contribute no repository folds nothing at all.
        assert!(apt_source_records(&[]).unwrap().is_empty());
    }

    /// The customize script is built by string formatting and handed to `sh` ~10
    /// minutes into a build, so parse it here with `sh -n` — a syntax error caught
    /// A value the customize environment carries, by name.
    fn env_of(pairs: &[(String, String)], key: &str) -> String {
        pairs
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone())
            .unwrap_or_else(|| panic!("{key} is not in the customize environment"))
    }

    /// The shipped bytes must parse as POSIX `sh`, because the only thing between them
    /// and a rootfs is the target's `/bin/sh` — and a syntax error there fails a
    /// multi-hour build at the last step. `sh -n` on the *committed file* rather than on
    /// one build's rendering, which is the whole of what the constant now buys.
    #[test]
    fn the_customize_program_is_valid_posix_shell() {
        let mut child = Command::new("sh")
            .arg("-n")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("sh is available on any unix test host");
        std::io::Write::write_all(child.stdin.as_mut().unwrap(), CUSTOMIZE.as_bytes()).unwrap();
        drop(child.stdin.take());
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "the customize program is not valid shell:\n{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    /// **No build value is ever syntax.** Every variable the program reads is a `B2D_*`
    /// one the environment supplies, and the environment supplies exactly those — so a
    /// hostname carrying a newline, or a board profile spelling a heredoc delimiter,
    /// cannot change what runs.
    ///
    /// Both directions matter: a `B2D_*` the program reads and the environment does not
    /// set is an unbound variable under `set -u`, and one the environment sets and the
    /// program does not read is a value that silently reaches nothing.
    #[test]
    fn every_value_the_program_reads_comes_from_the_environment() {
        let env = customize_env(DEFAULT_USER, image_of(&rk1()), None);
        let supplied: std::collections::BTreeSet<&str> =
            env.iter().map(|(k, _)| k.as_str()).collect();

        let mut read = std::collections::BTreeSet::new();
        for (idx, _) in CUSTOMIZE.match_indices("$B2D_") {
            let tail = &CUSTOMIZE[idx + 1..];
            let end = tail
                .find(|c: char| !c.is_ascii_alphanumeric() && c != '_')
                .unwrap_or(tail.len());
            read.insert(&tail[..end]);
        }
        assert!(
            !read.is_empty(),
            "the program reads no B2D_ variable at all"
        );
        assert_eq!(
            read, supplied,
            "the program's variables and the environment's must be the same set"
        );
    }

    /// Cage-native, and the account is created *locked*: the per-image password is
    /// spliced into `/etc/shadow` at image assembly, which is what keeps the provisioned
    /// tree cacheable across images built from one build point.
    #[test]
    fn the_customize_program_is_cage_native_and_leaves_the_account_locked() {
        assert!(
            !CUSTOMIZE.contains("chroot"),
            "cage-native: the rootfs is /"
        );
        assert!(
            !CUSTOMIZE.contains("$rootfs"),
            "cage-native: no host-side rootfs prefix"
        );
        assert!(!CUSTOMIZE.contains("chpasswd"));
        assert!(!CUSTOMIZE.contains("passwd -e"));
        assert!(CUSTOMIZE.contains(r#"useradd -m -s /bin/bash "$B2D_USER""#));
        assert!(CUSTOMIZE.contains(r#"usermod -aG video,render "$B2D_USER""#));
        // The build-time-only local `.deb` repository's apt source is dropped: its
        // `file://` temp dir is gone by the time the image runs, so leaving it would
        // fail every on-device `apt-get update`.
        assert!(CUSTOMIZE.contains(r#"rm -f "/etc/apt/sources.list.d/$B2D_LOCAL_REPO.list""#));
        assert_eq!(
            env_of(
                &customize_env(DEFAULT_USER, image_of(&rk1()), None),
                "B2D_LOCAL_REPO"
            ),
            "boot2deb-local"
        );
    }

    /// The update-initramfs placeholder is removed *before* the hooks run, so the one
    /// initrd the image ships is the one built here — after the overlay and after the
    /// depmod hook, which is the first point it can be built right.
    #[test]
    fn the_placeholder_is_dropped_before_the_kernel_hooks_run() {
        let removed = CUSTOMIZE
            .find(r#"rm -f "$B2D_INITRAMFS_STUB""#)
            .expect("the placeholder is removed");
        let hooks = CUSTOMIZE
            .find(r#"run-parts --exit-on-error --arg="$kver" /etc/kernel/postinst.d"#)
            .expect("the hooks run");
        assert!(removed < hooks, "removal precedes the hooks");
        let env = customize_env(DEFAULT_USER, image_of(&rk1()), None);
        assert_eq!(
            env_of(&env, "B2D_INITRAMFS_STUB"),
            crate::rootfs::INITRAMFS_STUB
        );
        assert_eq!(
            env_of(&env, "B2D_INITRAMFS_STUB_LOG"),
            crate::rootfs::INITRAMFS_STUB_LOG
        );
    }

    /// Every image enables `systemd-time-wait-sync`, so `time-sync.target` means what
    /// Debian's maintenance jobs already assume it means. Without it nothing reaches
    /// that target, and the `After=time-sync.target` ordering on apt-daily, logrotate,
    /// anacron and e2scrub is inert — they run against whatever the clock says at boot,
    /// which on an RTC-less board is the last power-off.
    #[test]
    fn every_image_enables_the_bounded_clock_wait() {
        assert!(
            CUSTOMIZE.contains(
                "ln -sf /usr/lib/systemd/system/systemd-time-wait-sync.service \\\n    \
                 /etc/systemd/system/sysinit.target.wants/systemd-time-wait-sync.service"
            ),
            "the wait-sync unit must be enabled"
        );
        // Enabled from the customize program, which runs *after* the systemd package is
        // installed. A `.wants` symlink staged in the base overlay would land before
        // that package, where `deb-systemd-helper` may still apply the unit's preset.
        assert!(
            CUSTOMIZE.contains("[ -f /usr/lib/systemd/system/systemd-time-wait-sync.service ]"),
            "assert the unit exists rather than enabling a name that does not"
        );
        // Both halves of the base overlay's bounded-wait drop-in: it is inert if the
        // unit is missing, and it fails closed — holding the boot for the full 45
        // seconds on every offline boot — if `timeout` is.
        assert!(CUSTOMIZE.contains("[ -x /usr/bin/timeout ]"));
    }

    /// The account policy reaches the image as resolved: the sudoers spec the build
    /// chose, and the keys the config authorized, in order.
    #[test]
    fn the_customize_env_carries_the_resolved_account_policy() {
        const ED25519: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBl5Nn9dY/aLK4WVQ5c4tYlYCkkC1J3Ry+d0nc3TgtDe operator@workstation";
        const RSA: &str = "ssh-rsa AAAAB3NzaC1yc2EA laptop";

        // The shipped default: root with no prompt, and nobody authorized by key.
        let plain = customize_env(DEFAULT_USER, image_of(&rk1()), None);
        assert_eq!(env_of(&plain, "B2D_SUDOERS"), "NOPASSWD: ALL");
        assert_eq!(env_of(&plain, "B2D_AUTHORIZED_KEYS"), "");

        let build = resolve_recipe(
            &repo_root(),
            "turing-rk1/forky",
            &Overrides {
                sudo: Some(SudoPolicy::Password),
                ssh_authorized_keys: Some(vec![ED25519.to_string(), RSA.to_string()]),
                ..Default::default()
            },
        )
        .unwrap();
        let env = customize_env(DEFAULT_USER, image_of(&build), None);
        // `password` writes the prompting spec. sudo takes the *last* matching rule, so
        // a stale NOPASSWD would not be inert — and there is now nowhere for one to be,
        // since the program writes exactly this value once.
        assert_eq!(env_of(&env, "B2D_SUDOERS"), "ALL");
        // Both keys, one per line, in the order the config named them.
        assert_eq!(
            env_of(&env, "B2D_AUTHORIZED_KEYS"),
            format!("{ED25519}\n{RSA}")
        );

        // sshd's StrictModes refuses a key it can reach through a group-writable path,
        // so the directory mode, the file mode, and the ownership are all load-bearing —
        // and the `.ssh` directory is made after `useradd -m` has created the home it
        // sits in. Reversed, `install -d` would create a root-owned /home/<user> and
        // `useradd -m` would then decline to populate or chown it.
        assert!(CUSTOMIZE.contains(r#"install -d -m 0700 "/home/$B2D_USER/.ssh""#));
        assert!(CUSTOMIZE.contains(r#"chmod 0600 "/home/$B2D_USER/.ssh/authorized_keys""#));
        // Trailing colon: the account's login group, whose name is the target's to
        // decide.
        assert!(CUSTOMIZE.contains(r#"chown -R "$B2D_USER": "/home/$B2D_USER/.ssh""#));
        assert!(
            !CUSTOMIZE.contains("$B2D_USER:$B2D_USER"),
            "the group name must not be guessed"
        );
        let useradd = CUSTOMIZE
            .find("useradd -m")
            .expect("the account is created");
        let ssh_dir = CUSTOMIZE.find("install -d").expect("the .ssh dir is made");
        assert!(
            useradd < ssh_dir,
            "the home must exist before .ssh goes in it"
        );
    }

    /// The keys, written by the program's own line. What has to hold is that the file
    /// `sshd` reads carries the authored bytes **verbatim** — no expansion, no
    /// word-splitting, nothing executed — whatever a key's comment holds.
    ///
    /// The line is lifted out of the committed program rather than restated here, so
    /// this exercises the bytes that ship. It is the one place the old generated form
    /// could go wrong and this one structurally cannot: the keys arrive in the
    /// environment, and `printf '%s\n' "$X"` has no quoting left to get wrong.
    #[test]
    fn a_hostile_authorized_key_is_written_verbatim_and_never_run() {
        // Every metacharacter that would matter in an unquoted context, plus the
        // delimiter the old generated heredoc used, as a comment word.
        let hostile = "ssh-ed25519 AAAAB3NzaC1yc2EA $(touch /pwned) `id` 'x' \"y\" \
                       $HOME ${PATH} \\ BOOT2DEB_AUTHORIZED_KEYS";
        let plain = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBl5Nn9dY/aLK4WVQ5c4tYlYCkkC1J3Ry+d0nc3TgtDe second@key";

        let write_line = CUSTOMIZE
            .lines()
            .find(|l| l.contains("printf '%s\\n' \"$B2D_AUTHORIZED_KEYS\""))
            .expect("the program writes the keys with printf");

        let tmp = tempfile::tempdir().unwrap();
        let home = tmp.path().join("home/tester");
        std::fs::create_dir_all(home.join(".ssh")).unwrap();
        // The same line, with the program's own path expression pointed at the fixture.
        let rooted = write_line.replace(
            r#""/home/$B2D_USER/.ssh/authorized_keys""#,
            &format!(r#""{}/.ssh/authorized_keys""#, home.display()),
        );
        let status = Command::new("sh")
            .arg("-eu")
            .arg("-c")
            .arg(&rooted)
            .env("B2D_AUTHORIZED_KEYS", format!("{hostile}\n{plain}"))
            .env("HOME", "/should-not-appear")
            .status()
            .expect("sh runs");
        assert!(status.success(), "the line runs:\n{rooted}");

        let written = std::fs::read_to_string(home.join(".ssh/authorized_keys")).unwrap();
        assert_eq!(written, format!("{hostile}\n{plain}\n"));
        assert!(
            !tmp.path().join("pwned").exists() && !std::path::Path::new("/pwned").exists(),
            "a command substitution in a comment must not have run"
        );
    }

    /// The depthcharge tail: build the signed kernel partition, prove it is bootable,
    /// and arm the on-device kernel hooks. Every check guards a failure that is silent
    /// on the serial-console-less hardware.
    #[test]
    fn the_depthcharge_tail_verifies_before_it_ships() {
        for expected in [
            "depthchargectl build",
            "vbutil_kernel --verify",
            "lsinitramfs",
            "--show-depends",
            "systemctl is-enabled depthcharge-tools.service",
        ] {
            assert!(CUSTOMIZE.contains(expected), "the tail runs {expected}");
        }

        let env = customize_env(
            DEFAULT_USER,
            image_of(&c201()),
            Some(BootConfig::Depthcharge {
                board: "speedy",
                cmdline: "console=tty1 ro",
                initramfs_compress: InitramfsCompress::Xz,
            }),
        );
        assert_eq!(env_of(&env, "B2D_DEPTHCHARGE_BOARD"), "speedy");
        // Armed — unlike the build-time config in the pre-install overlay, which must
        // not let the hooks hunt the build host's disks.
        assert!(env_of(&env, "B2D_DEPTHCHARGE_CONFIG").contains("enable-system-hooks = True"));
        let modules = env_of(&env, "B2D_REQUIRED_INITRD_MODULES");
        for module in REQUIRED_INITRD_MODULES {
            assert!(
                modules.split(' ').any(|m| m == *module),
                "asserts {module} into the initramfs"
            );
        }

        // A raw-gap board takes the early exit instead: no board, so no tail.
        let raw = customize_env(DEFAULT_USER, image_of(&rk1()), None);
        assert_eq!(env_of(&raw, "B2D_DEPTHCHARGE_BOARD"), "");
        assert_eq!(env_of(&raw, "B2D_DEPTHCHARGE_CONFIG"), "");
        assert!(
            CUSTOMIZE.contains(r#"[ -n "$B2D_DEPTHCHARGE_BOARD" ] || exit 0"#),
            "an empty board ends the program before the tail"
        );
    }

    /// The localization tail proves the two things resolution could not: a timezone
    /// missing from the target's `tzdata` leaves `/etc/localtime` dangling and the clock
    /// silently wrong, and a `locales` package that generated nothing leaves `LANG`
    /// naming an ungenerated locale.
    #[test]
    fn the_customize_program_asserts_the_l10n_config_took() {
        assert!(CUSTOMIZE.contains(r#"[ -e "/usr/share/zoneinfo/$B2D_TIMEZONE" ]"#));
        assert!(CUSTOMIZE.contains("is not in this suite's tzdata"));
        assert!(CUSTOMIZE.contains("[ -s /usr/lib/locale/locale-archive ]"));

        let env = customize_env(DEFAULT_USER, image_of(&rk1()), None);
        assert_eq!(env_of(&env, "B2D_TIMEZONE"), "UTC");
        // The flag is set because the RK1 image generates locales; the check is
        // meaningless on an image that generates none, where the archive is absent by
        // design.
        assert_eq!(env_of(&env, "B2D_LOCALES_GENERATED"), "1");
    }
}
