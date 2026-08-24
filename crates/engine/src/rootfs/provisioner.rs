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
//!    `.deb`'s archive-recorded sha256, without downloading. This one call keys the
//!    early-cutoff cache *and* becomes the content-pinned manifest.
//! 2. On a cache miss, **provision** packages into a staging tree
//!    (`provision::ensure`), with the build's own `.deb`s as a local trusted
//!    `dists/` mirror, the feature repositories, and the pre-install overlay laid in
//!    via [`pre_configure_overlay`](ferroday_cage::provision::debian::DebianBuilder::pre_configure_overlay)
//!    so a package's maintainer scripts see the l10n/depthcharge config as they
//!    configure.
//! 3. **Customize** the tree boot2deb-side: lay the post-install overlay in, then
//!    run the account/`postinst.d`/depthcharge steps as commands in a subordinate
//!    cage over the finished tree.
//! 4. **Export** the ownership-preserving tar and write the plan manifest; store
//!    both in the shared [`RootfsStore`].
//!
//! The account is created **locked**; the unique per-image first-boot password is
//! spliced into `/etc/shadow` at image assembly, keeping the cached tree reusable.

use super::{
    config, stage_overlay, stage_preinstall_overlay, AptRepo, BootConfig, RootfsArtifacts,
    RootfsOptions, DEFAULT_USER, REQUIRED_INITRD_MODULES,
};
use crate::bootstrap::COMPONENTS;
use crate::error::EngineError;
use crate::event::{EventSink, Step};
use crate::repo::LocalDistsRepo;
use crate::rootcache::{self, RootfsStore};
use crate::sandbox::{forward_bootstrap_event, StepObserver};
use boot2deb_core::model::ResolvedBuild;
use ferroday_cage::provision::debian::{Debian, DebianEvent, Plan, Priority, Repository};
use ferroday_cage::provision::{self, Export};
use ferroday_cage::{IdentityMap, Network};
use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::process::Command;

/// The name of the build's own `.deb` pool as a provisioner [`Repository`], and so
/// the stem of the `/etc/apt/sources.list.d/<name>.list` entry ferroday-cage writes
/// for it. This repo is a **build-time-only** trusted `file://` mirror living under a
/// temp dir that is gone by the time the image runs, so [`customize_script`] deletes
/// its sources entry before export — unlike the feature repositories, whose entries
/// are meant to persist for on-device updates.
const LOCAL_REPO_NAME: &str = "boot2deb-local";

/// Bootstrap `build`'s rootfs per `opts`, emitting the step's
/// [`Event`](crate::event::Event)s to `sink`: the `tar` archive the image node
/// formats and the content-pinned package manifest beside it.
pub fn build_rootfs(
    build: &ResolvedBuild,
    opts: &RootfsOptions,
    sink: &dyn EventSink,
) -> Result<RootfsArtifacts, EngineError> {
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
    let work = tempfile::Builder::new()
        .prefix("boot2deb-prov-")
        .tempdir()
        .map_err(|s| EngineError::io(&std::env::temp_dir(), s))?;

    // 1. Overlays: the pre-install tree (laid before maintainer scripts run,
    //    via the provisioner's pre-configure hook) and the customize tree
    //    (laid after, so it wins). Both merge the layer overlays plus this
    //    node's generated config.
    let preinstall = work.path().join("overlay-pre");
    stage_preinstall_overlay(
        &preinstall,
        opts.preinstall_overlay_dirs,
        build,
        opts.boot_config,
        &step,
    )?;
    let overlay = work.path().join("overlay");
    stage_overlay(
        &overlay,
        opts.overlay_dirs,
        build,
        opts.rootfs_partuuid,
        opts.image_identity,
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
    let localrepo =
        LocalDistsRepo::assemble(&pool_dir, opts.repo_debs, build.image_suite(), arch, &step)?;

    let tarball = opts.out_dir.join(format!("{}-rootfs.tar", build.device));

    // 4. Build the provisioner and resolve the plan. The plan is both the
    //    cache key and the manifest, so it is taken up front, before the
    //    cache is even consulted. The progress sink is bound per call, so
    //    its borrow of `step` lasts one run and never outlives the
    //    provisioner into the logging below.
    let mut sink_fn = |event: DebianEvent<'_>| forward_bootstrap_event(&step, event);
    let mut debian = build_debian(
        build,
        opts,
        &localrepo.file_url(),
        feature_repositories(opts.apt_sources)?,
        &deb_cache,
        &preinstall,
    )?;
    step.log("resolving the rootfs package plan (ferroday-cage provisioner)");
    let plan = debian
        .observe(&mut sink_fn)
        .resolve()
        .map_err(|e| EngineError::Bootstrap {
            context: "resolve the rootfs plan".into(),
            message: e.to_string(),
        })?;
    step.log(format!(
        "resolved {} packages ({} mirror(s), {} local .deb(s))",
        plan.packages.len(),
        opts.mirrors.len(),
        opts.repo_debs.len()
    ));

    // 5. Early-cutoff cache, keyed by the plan's solved set plus the overlay
    //    and local-repo content: a moved mirror resolves a different plan, so a
    //    hit can never be stale.
    let cache = match opts.cache_dir {
        Some(dir) => {
            let key = rootfs_key(build, opts, &plan, &preinstall, &overlay)?;
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
            build.arch, build.image_suite(),
        ));
        // A transient provisioned tree carrying subordinate-mapped ownership
        // the caller cannot unlink; the guard removes it through the map. Kept
        // out of `work` so its TempDir drop is not asked to remove ids it
        // cannot. `provision::ensure` requires a non-existent destination.
        let rootfs_dir = std::env::temp_dir()
            .join(format!("boot2deb-prov-rootfs-{}", std::process::id()));
        let _ = provision::remove(&rootfs_dir);
        // The bootstrap resolves the install closure a second time, for itself,
        // and installs *that* result — the plan above was resolved earlier and
        // only keys the cache. `DebianEvent::Resolved` carries the bootstrap's
        // own resolution, so capturing it here is what lets the manifest describe
        // the packages the image actually carries: between the two resolutions
        // the archive can publish, and a manifest resolved before the install is
        // then a claim about a set that was never installed.
        let mut installed: Option<Plan> = None;
        let mut bootstrap_sink = |event: DebianEvent<'_>| {
            if let DebianEvent::Resolved { plan, .. } = &event {
                installed = Some((*plan).clone());
            }
            forward_bootstrap_event(&step, event);
        };
        provision::ensure(&rootfs_dir, &mut debian.observe(&mut bootstrap_sink)).map_err(
            |e| EngineError::Bootstrap {
                context: "provision the rootfs".into(),
                message: e.to_string(),
            },
        )?;
        let _provisioned = ProvisionedRoot(rootfs_dir.clone());
        // The two resolutions agreeing is the ordinary case; a divergence means
        // the archive moved mid-build, which is worth seeing rather than
        // silently absorbing — the cached entry is keyed on the earlier plan
        // while its tar and manifest describe the later one.
        let installed = match installed {
            Some(installed) => {
                if installed != plan {
                    step.log(format!(
                        "note: the bootstrap resolved {} packages, not the {} the cache key \
                         was taken from — the archive published mid-build; the manifest \
                         describes what was installed",
                        installed.packages.len(),
                        plan.packages.len()
                    ));
                }
                installed
            }
            // The bootstrap always reports its plan; falling back to the
            // cache-key resolution keeps a manifest written either way.
            None => plan.clone(),
        };
        step.progress(55);

        customize(&rootfs_dir, &overlay, build, opts.boot_config, DEFAULT_USER, &step)?;
        step.progress(65);

        export_rootfs_tar(&rootfs_dir, &tarball, opts.source_date_epoch, &step)?;
        write_plan_manifest(&installed, opts.manifest_out, &step)?;
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
    })
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

/// Assemble the [`Debian`] provisioner from the resolved build and options.
///
/// The primary mirror (plus any `snapshot.debian.org` backstop, which relaxes
/// freshness) is configured on the builder; the local trusted `dists/` pool and the
/// feature repositories are merged in as additional [`Repository`] sources. The
/// base seeds the `important` variant — a full base system, unlike the build
/// sandbox's minimal one —
/// the resolved package set and the build's `extra_packages` are the includes, the
/// resolved excludes are dropped, and the subordinate map gives the tree real
/// ownership. The pre-install overlay is handed to the provisioner's pre-configure
/// hook, and the download cache is content-addressed for reuse.
///
/// The provisioner borrows nothing from the caller — its progress sink is bound
/// per call by [`Debian::observe`] — so it is a `Debian<'static>` that outlives
/// every observed run.
fn build_debian(
    build: &ResolvedBuild,
    opts: &RootfsOptions,
    local_url: &str,
    feature_repos: Vec<Repository>,
    deb_cache: &Path,
    preinstall: &Path,
) -> Result<Debian<'static>, EngineError> {
    let arch = build.arch.debian_arch();
    let (primary, fallbacks) = opts
        .mirrors
        .split_first()
        .expect("mirrors are non-empty (checked by the caller)");

    let mut b = Debian::builder(build.image_suite())
        .architecture(arch)
        .components(COMPONENTS.split(','))
        .base_priority(Priority::Important)
        .include(build.rootfs_packages.iter().cloned())
        .include(opts.extra_packages.iter().cloned())
        .exclude(build.rootfs_exclude.iter().cloned())
        .identity_map(IdentityMap::Subordinate)
        .cache_dir(deb_cache)
        .pre_configure_overlay(preinstall)
        .mirror(primary);
    for fallback in fallbacks {
        b = b.mirror_fallback(fallback);
    }
    // A snapshot backstop's release is expired by design; accepting a
    // signed-but-stale release is a repository-wide posture.
    if !fallbacks.is_empty() {
        b = b.allow_stale_release(true);
    }
    if let Some(keyring) = opts.keyring {
        b = b.keyring(keyring);
    }

    // The build's own `.deb`s: a trusted `file://` pool, apt's `[trusted=yes]`.
    let local = Repository::builder(build.image_suite())
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
        context: format!("configure the {} {} bootstrap", build.arch, build.image_suite()),
        message: e.to_string(),
    })
}

/// Build a signed [`Repository`] per resolved feature apt source (e.g. Jellyfin),
/// each verified against its own keyring and carrying its own suite/components, so
/// an out-of-mirror app resolves in the provisioner's closure. Each writes its own
/// `/etc/apt/sources.list.d/<name>.list` into the finished rootfs.
fn feature_repositories(apt_sources: &[AptRepo]) -> Result<Vec<Repository>, EngineError> {
    apt_sources
        .iter()
        .map(|repo| {
            Repository::builder(&repo.source.suite)
                .mirror(&repo.source.uri)
                .components(repo.source.components.iter().cloned())
                .keyring(&repo.keyring)
                .name(sanitize_repo_name(&repo.source.name))
                .build()
                .map_err(|e| EngineError::Bootstrap {
                    context: format!("configure feature repository {}", repo.source.name),
                    message: e.to_string(),
                })
        })
        .collect()
}

/// Reduce a feature-source name to the portable file-name stem a repository name
/// accepts (ASCII letters, digits, `.`, `-`, `_`), so its `sources.list.d` entry
/// and keyring file cannot escape their directories.
fn sanitize_repo_name(name: &str) -> String {
    let stem: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if stem.is_empty() || stem == "." || stem == ".." {
        "feature".to_string()
    } else {
        stem
    }
}

/// The rootfs cache key: the plan's solved set plus the two staged overlay trees'
/// content and the local-repo `.deb`s' content. Keying on the *solved* set is what
/// makes a hit safe — a moved mirror resolves a different plan, hence a different
/// key, and rebuilds.
fn rootfs_key(
    build: &ResolvedBuild,
    opts: &RootfsOptions,
    plan: &Plan,
    preinstall: &Path,
    overlay: &Path,
) -> Result<crate::signature::Signature, EngineError> {
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
    Ok(rootcache::cache_key(
        &solved,
        &overlay_fp,
        &repo_fp,
        &build.arch.to_string(),
        build.image_suite(),
    ))
}

/// The plan's packages as `name version arch` lines — the solved set folded into
/// the cache key. Order-insensitive there, so this does not sort.
fn plan_solved(plan: &Plan) -> Vec<String> {
    plan.packages
        .iter()
        .map(|p| format!("{} {} {}", p.name, p.version, p.architecture))
        .collect()
}

/// Write the content-pinned manifest from the resolved plan (`name version arch
/// sha256`, sorted by name), the lock's `[rootfs].manifest`. The plan carries each
/// `.deb`'s archive-recorded sha256, so no dpkg-status parse or in-bootstrap hash
/// hook is needed: the plan *is* the installed set, resolved through the same path
/// the provisioner installs.
fn write_plan_manifest(plan: &Plan, out: &Path, step: &Step) -> Result<(), EngineError> {
    let rows: Vec<(String, String, String, String)> = plan
        .packages
        .iter()
        .map(|p| {
            (
                p.name.clone(),
                p.version.clone(),
                p.architecture.clone(),
                p.sha256.clone(),
            )
        })
        .collect();
    let body = render_plan_manifest(&rows);
    if let Some(parent) = out.parent() {
        std::fs::create_dir_all(parent).map_err(|s| EngineError::io(parent, s))?;
    }
    std::fs::write(out, &body).map_err(|s| EngineError::io(out, s))?;
    step.log(format!(
        "wrote solved manifest ({} packages, sha256-pinned from the plan) to {}",
        rows.len(),
        out.display()
    ));
    Ok(())
}

/// Render the manifest text from plan rows: a header then one sorted `name version
/// arch sha256` line per package. Pure, so the derivation is testable without a
/// bootstrap. Sorted by the full row (name first), so it is deterministic.
fn render_plan_manifest(rows: &[(String, String, String, String)]) -> String {
    let mut rows: Vec<&(String, String, String, String)> = rows.iter().collect();
    rows.sort();
    let mut body =
        String::from("# Solved rootfs package manifest: installed name version arch sha256.\n");
    for (name, version, arch, sha) in rows {
        let _ = writeln!(body, "{name} {version} {arch} {sha}");
    }
    body
}

/// Customize the provisioned tree: lay the post-install overlay in, then run the
/// account, kernel-`postinst.d`, l10n, and depthcharge steps as commands in a
/// subordinate cage over the finished tree.
fn customize(
    rootfs: &Path,
    overlay: &Path,
    build: &ResolvedBuild,
    boot: Option<BootConfig>,
    user: &str,
    step: &Step,
) -> Result<(), EngineError> {
    // Lay the customize overlay (layer trees + generated config) into the rootfs
    // host-side. Under the subordinate map, `/etc` and the other config paths are
    // root inside — the calling user outside — so a caller-side copy writes them
    // correctly, landing them root-owned (preserve mode, not owner). --remove-
    // destination replaces base files even through a dangling symlink; -dR keeps
    // symlinks and directory structure.
    step.log("laying the customize overlay into the provisioned rootfs");
    let mut cp = Command::new("cp");
    cp.arg("-dR")
        .arg("--remove-destination")
        .arg("--preserve=mode")
        .arg(format!("{}/.", overlay.display()))
        .arg(format!("{}/", rootfs.display()));
    crate::build::run(cp, "cp", "lay customize overlay into rootfs", step)?;

    // Run the target-side config in one cage over the rootfs (rootfs = `/`), under
    // the same subordinate map the tree was built with. Target binaries
    // (`useradd`, `run-parts`, `depthchargectl`) run directly, or via the host's
    // `qemu-user` binfmt when cross-arch — as the build sandbox's do.
    step.log("running the target-chroot customize steps in a cage");
    run_customize_cage(rootfs, &customize_script(user, build, boot), step)
}

/// Run one `sh -c` script in a subordinate-mapped cage rooted at `rootfs`,
/// streaming output to `step`. Customize needs no network.
///
/// It runs under the same [`baseline`](crate::sandbox::baseline) profile as the package
/// stages, and adds only the subordinate map its ownership-preserving tree needs. The
/// maintainer scripts this runs are sensitive to `LC_ALL`, `TZ`, and `DEBIAN_FRONTEND`,
/// and the profile declares all three — so what they see is the environment the image's
/// provenance records.
fn run_customize_cage(rootfs: &Path, script: &str, step: &Step) -> Result<(), EngineError> {
    let cage = crate::sandbox::baseline(rootfs)
        .identity_map(IdentityMap::Subordinate)
        .command("sh")
        .args(["-c", script])
        .network(Network::Isolated)
        .current_dir("/")
        .build()
        .map_err(|source| EngineError::Sandbox {
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

/// The customize script, run in a cage where the rootfs is `/` — so it needs no
/// `chroot` and no `$rootfs` prefix.
///
/// It creates the default account **locked** (the per-image password is spliced in
/// at image assembly, so the tree stays cacheable), grants group access, sets
/// passwordless sudo, clears the ssh host keys for first-boot regeneration, drops
/// the build-time-only local `.deb` repository's apt source (its `file://` temp dir
/// is gone once the image runs), and re-runs the kernel `postinst.d` hooks so `/boot`
/// gains the initrd, board dtb,
/// and `extlinux.conf` — the kernel package configured before the overlay was laid
/// in, so its own postinst produced none of them. It closes with the localization
/// asserts and, on a depthcharge board, the signed-kernel finalize.
fn customize_script(user: &str, build: &ResolvedBuild, boot: Option<BootConfig>) -> String {
    let mut s = String::from("set -eu\n");
    // Default account, created locked; the per-image password is spliced in later.
    let _ = write!(
        s,
        "useradd -m -s /bin/bash '{user}'\n\
         usermod -aG video,render '{user}'\n\
         mkdir -p /etc/sudoers.d\n\
         printf '%s ALL=(ALL) NOPASSWD: ALL\\n' '{user}' > /etc/sudoers.d/{user}\n\
         chmod 0440 /etc/sudoers.d/{user}\n\
         rm -f /etc/ssh/ssh_host_*\n\
         rm -f /etc/apt/sources.list.d/{local_repo}.list\n",
        local_repo = LOCAL_REPO_NAME,
    );
    // Boot artifacts: re-run the kernel postinst.d hooks for the installed kernel,
    // now that the overlay's hooks and the PARTUUID-rooted fstab are in place.
    // --exit-on-error fails the build rather than shipping a kernel with nothing
    // to boot it. The kernel version is reused by the depthcharge tail below.
    s.push_str(
        "kver=\"$(linux-version list | linux-version sort --reverse | head -n1)\"\n\
         run-parts --exit-on-error --arg=\"$kver\" /etc/kernel/postinst.d\n",
    );
    s.push_str(&l10n_asserts(build));
    if let Some(BootConfig::Depthcharge { board, .. }) = boot {
        s.push_str(&depthcharge_finalize(board));
    }
    s
}

/// The localization tail: prove the two things resolution could not. A timezone
/// missing from the target's `tzdata` leaves `/etc/localtime` dangling and the
/// clock silently wrong; a `locales` package that generated nothing leaves `LANG`
/// naming an ungenerated locale. Cage-native (paths are `/…`, not `$rootfs/…`).
fn l10n_asserts(build: &ResolvedBuild) -> String {
    let mut s = format!(
        "[ -e /usr/share/zoneinfo/{tz} ] || \
         {{ echo \"timezone '{tz}' is not in this suite's tzdata\" >&2; exit 1; }}\n",
        tz = build.timezone,
    );
    // The locale-archive is absent on an image that generates nothing (also what a
    // base system looks like), so the check only means something when locales were
    // asked for.
    if !build.locales_generate.is_empty() {
        s.push_str(
            "[ -s /usr/lib/locale/locale-archive ] || \
             { echo 'locale-gen produced no locale-archive: LANG would name an ungenerated locale' >&2; exit 1; }\n",
        );
    }
    s
}

/// The depthcharge tail: build the signed kernel partition, prove it is bootable,
/// and arm the on-device kernel hooks — every check guarding a failure that is
/// silent on the serial-console-less hardware. Cage-native: the rootfs is `/`.
fn depthcharge_finalize(board: &str) -> String {
    let mut s = String::new();
    // Assert every module the initramfs lists actually exists for this kernel:
    // MODULES=list silently drops an unresolvable name, so a typo would ship an
    // initramfs missing (say) the PMIC driver and the board would hang at a white
    // screen. `</dev/null` so the inner command cannot consume the list.
    s.push_str(
        "for list in /usr/share/initramfs-tools/modules.d/*; do\n\
         \x20 [ -f \"$list\" ] || continue\n\
         \x20 while read -r mod; do\n\
         \x20   case \"$mod\" in ''|\\#*) continue ;; esac\n\
         \x20   modprobe --set-version \"$kver\" --show-depends \"$mod\" </dev/null >/dev/null 2>&1 || {\n\
         \x20     echo \"initramfs module '$mod' does not exist in kernel $kver (from $(basename \"$list\"))\" >&2\n\
         \x20     exit 1\n\
         \x20   }\n\
         \x20 done < \"$list\"\n\
         done\n",
    );
    // Build the signed payload; board profile and cmdline come from the pre-install
    // overlay's config, root= from /etc/fstab.
    s.push_str(
        "depthchargectl build --verbose\n\
         kpart=\"$(ls /boot/depthcharge/*.img 2>/dev/null | head -n1)\"\n\
         [ -n \"$kpart\" ] || { echo 'depthchargectl build produced no image' >&2; exit 1; }\n\
         futility vbutil_kernel --verify \"/boot/depthcharge/$(basename \"$kpart\")\"\n",
    );
    // The initramfs is inside the signature now — last chance to confirm the
    // modules that must be in it actually are.
    let _ = write!(
        s,
        "initrd_list=\"$(lsinitramfs \"/boot/initrd.img-$kver\")\"\n\
         for need in {modules}; do\n\
         \x20 case \"$initrd_list\" in *\"$need\"*) ;; *)\n\
         \x20   echo \"the built initramfs is missing $need — MODULES=list did not take\" >&2\n\
         \x20   exit 1 ;;\n\
         \x20 esac\n\
         done\n",
        modules = REQUIRED_INITRD_MODULES.join(" "),
    );
    // Arm the package's kernel hooks for the shipped system: an on-device apt
    // kernel upgrade re-signs and writes the other slot itself. They were off
    // during the build so they could not hunt the build host's disks.
    let _ = write!(
        s,
        "cat > /etc/depthcharge-tools/config <<'B2D_EOF'\n{enabled}B2D_EOF\n\
         grep -q '^enable-system-hooks = True$' /etc/depthcharge-tools/config\n",
        enabled = config::depthcharge_config(board, true),
    );
    // And assert the other half of the upgrade protocol is armed: the
    // depthcharge-tools .service blesses a freshly-written slot once it boots.
    // Without it, a successful kernel upgrade is rolled back one reboot later.
    s.push_str(
        "systemctl is-enabled depthcharge-tools.service >/dev/null || {\n\
         \x20 echo 'depthcharge-tools.service is not enabled: a kernel upgrade would be' >&2\n\
         \x20 echo 'rolled back one reboot after it succeeded (nothing would bless it)' >&2\n\
         \x20 exit 1\n\
         }\n",
    );
    s
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
    use super::*;
    use boot2deb_core::{resolve_recipe, ConfigRoot, Overrides};

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
        resolve_recipe(&repo_root(), "turing-rk1/media-accel-forky", &Overrides::default()).unwrap()
    }

    /// A depthcharge board with a distro kernel.
    fn c201() -> ResolvedBuild {
        resolve_recipe(&repo_root(), "asus-c201/forky", &Overrides::default()).unwrap()
    }

    #[test]
    fn render_plan_manifest_is_sorted_and_pinned() {
        let rows = vec![
            ("libc6".into(), "2.41-1".into(), "arm64".into(), "aaaa".into()),
            ("ffmpeg-rk".into(), "3e53143".into(), "arm64".into(), "cccc".into()),
        ];
        let body = render_plan_manifest(&rows);
        let lines: Vec<&str> = body.lines().filter(|l| !l.starts_with('#')).collect();
        // Sorted by name, each carrying its archive-recorded sha256.
        assert_eq!(lines, vec!["ffmpeg-rk 3e53143 arm64 cccc", "libc6 2.41-1 arm64 aaaa"]);
    }

    #[test]
    fn sanitize_repo_name_yields_a_portable_stem() {
        assert_eq!(sanitize_repo_name("jellyfin"), "jellyfin");
        assert_eq!(sanitize_repo_name("my repo/x"), "my-repo-x");
        assert_eq!(sanitize_repo_name(".."), "feature");
        assert_eq!(sanitize_repo_name(""), "feature");
    }

    /// The customize script is built by string formatting and handed to `sh` ~10
    /// minutes into a build, so parse it here with `sh -n` — a syntax error caught
    /// at the worst possible moment otherwise.
    fn assert_valid_shell(name: &str, script: &str) {
        let mut child = Command::new("sh")
            .arg("-n")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .expect("sh is available on any unix test host");
        std::io::Write::write_all(child.stdin.as_mut().unwrap(), script.as_bytes()).unwrap();
        drop(child.stdin.take());
        let out = child.wait_with_output().unwrap();
        assert!(
            out.status.success(),
            "the {name} customize script is not valid shell:\n{}\n--- script ---\n{script}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn customize_script_is_valid_cage_native_shell() {
        // Cage-native: no chroot, no `$rootfs/` prefix — the rootfs is `/`.
        let rkbin = customize_script(DEFAULT_USER, &rk1(), None);
        assert_valid_shell("rkbin", &rkbin);
        assert!(!rkbin.contains("chroot"), "cage-native: the rootfs is /");
        assert!(!rkbin.contains("$rootfs"), "cage-native: no host-side rootfs prefix");
        // The account is created locked; the per-image password is spliced later.
        assert!(rkbin.contains("useradd -m -s /bin/bash 'debian'"));
        assert!(!rkbin.contains("chpasswd"));
        assert!(!rkbin.contains("passwd -e"));
        assert!(rkbin.contains("usermod -aG video,render 'debian'"));
        // The build-time-only local .deb repo's apt source is dropped: its `file://`
        // temp dir is gone by the time the image runs, so leaving it would fail every
        // on-device `apt-get update`.
        assert!(rkbin.contains("rm -f /etc/apt/sources.list.d/boot2deb-local.list"));
        assert!(rkbin.contains("run-parts --exit-on-error --arg=\"$kver\" /etc/kernel/postinst.d"));
        // A raw-gap board gets no depthcharge tail.
        assert!(!rkbin.contains("depthchargectl"));

        let depth = customize_script(
            DEFAULT_USER,
            &c201(),
            Some(BootConfig::Depthcharge {
                board: "speedy",
                cmdline: "console=tty1 ro",
            }),
        );
        assert_valid_shell("depthcharge", &depth);
    }

    #[test]
    fn the_depthcharge_customize_tail_verifies_before_it_ships() {
        let script = customize_script(
            DEFAULT_USER,
            &c201(),
            Some(BootConfig::Depthcharge {
                board: "speedy",
                cmdline: "console=tty1 ro",
            }),
        );
        assert!(script.contains("depthchargectl build"), "builds the signed payload");
        assert!(script.contains("vbutil_kernel --verify"), "proves the firmware will take it");
        assert!(script.contains("lsinitramfs"), "proves the initramfs has what it needs");
        for module in REQUIRED_INITRD_MODULES {
            assert!(script.contains(module), "asserts {module} into the initramfs");
        }
        assert!(script.contains("--show-depends"), "asserts the module list resolves");
        assert!(
            script.contains("enable-system-hooks = True"),
            "arms on-device kernel upgrades before shipping"
        );
        assert!(
            script.contains("systemctl is-enabled depthcharge-tools.service"),
            "asserts the unit that blesses a booted kernel slot is enabled"
        );
    }

    #[test]
    fn the_customize_script_asserts_the_l10n_config_took() {
        let script = customize_script(DEFAULT_USER, &rk1(), None);
        assert!(script.contains("/usr/share/zoneinfo/"));
        assert!(script.contains("is not in this suite's tzdata"));
        assert!(script.contains("/usr/lib/locale/locale-archive"));
    }
}
