//! `update`: resolve upstream refs, hash the blobs, and write the recipe's `.lock`.
//!
//! The sole path that consults upstream — `build` reads only the lock. An omitted
//! per-tree ref flag re-pins the *previous lock's* ref (not the config's symbolic
//! one), so a routine re-pin only moves what the caller named. After the lock is
//! written, every pinned source is checked for re-fetch durability and any
//! ephemeral/unadvertised pin is flagged (advisory — it never blocks the write).

use crate::args::UpdateArgs;
use crate::config::{extra_debs_store, preflight_config, source_axes};
use crate::render::{print_event, short};
use boot2deb_core::model::Overrides;
use boot2deb_core::{resolve_recipe, ConfigRoot};
use boot2deb_engine::debstore::DebStore;
use boot2deb_engine::event::{Event, Step};
use boot2deb_engine::{extradebs, pins, sources};

/// Run `update <recipe>`.
pub(crate) fn run(
    root: &ConfigRoot,
    recipe: &str,
    args: UpdateArgs,
) -> Result<(), Box<dyn std::error::Error>> {
    let build = resolve_recipe(root, recipe, &Overrides::default())?;
    // Validate the local config invariants (image geometry, kernel-fragment and
    // apt-keyring existence) before resolving/committing the lock, so a bad
    // `rootfs_offset` or a typo'd fragment fails here rather than being pinned into
    // the lock and failing at the next build (CFG-4).
    preflight_config(root, &build)?;
    // An omitted per-tree ref flag preserves the *previous lock's* ref, not the
    // config's symbolic ref (COR-12). Otherwise a routine `update` that only bumps
    // the kernel would silently re-pin every other tree from its committed exact
    // commit back to the current branch head. Flags still override; a first update
    // (no prior lock) falls back to the config default.
    let prev = root.lock(recipe).ok();
    let ref_for =
        |flag: Option<String>, from_lock: fn(&boot2deb_core::lock::Lock) -> String, default: &str| {
            flag.or_else(|| prev.as_ref().map(from_lock))
                .unwrap_or_else(|| default.to_string())
        };
    // The kernel ref has no config default (the config carries only a `track`, not a
    // concrete tag), so an omitted `--kernel-ref` inherits the previous lock's ref —
    // the "re-pin what changed" model for a patch-only update. Only the first update
    // (no prior lock) must supply it.
    let kernel_ref = match args.kernel_ref {
        Some(r) => r,
        None => prev
            .as_ref()
            .map(|l| l.kernel.reference.clone())
            .ok_or_else(|| {
                format!(
                    "no --kernel-ref given and no existing lock for '{recipe}' to inherit it \
                     from — pass --kernel-ref <tag> (e.g. v7.1.1) for the first update"
                )
            })?,
    };
    let uboot_ref = ref_for(args.uboot_ref, |l| l.uboot.reference.clone(), &build.uboot_ref);
    // Media-accel source refs are pinned only when the recipe builds the transcode
    // stack (its resolved build carries the sources). A base build leaves them empty —
    // `resolve_lock` never reads a ref without a matching source, and the lock omits
    // both pin tables. Each ref inherits the prior lock's pin, else the SoC-layer
    // default constraint ("re-pin what changed").
    let prev_us = prev.as_ref().and_then(|l| l.userspace.as_ref());
    let prev_ff = prev.as_ref().and_then(|l| l.ffmpeg.as_ref());
    let pick = |flag: Option<String>, prev: Option<String>, default: &str| {
        flag.or(prev).unwrap_or_else(|| default.to_string())
    };
    let (mpp_ref, librga_ref, libmali_ref) = match &build.userspace {
        Some(us) => (
            pick(args.mpp_ref, prev_us.map(|u| u.mpp.reference.clone()), &us.mpp.git_ref),
            pick(args.librga_ref, prev_us.map(|u| u.librga.reference.clone()), &us.librga.git_ref),
            pick(args.libmali_ref, prev_us.map(|u| u.libmali.reference.clone()), &us.libmali.git_ref),
        ),
        None => (String::new(), String::new(), String::new()),
    };
    let (ffmpeg_base_ref, ffmpeg_rockchip_ref) = match &build.ffmpeg {
        Some(ff) => (
            pick(args.ffmpeg_base_ref, prev_ff.map(|f| f.base.reference.clone()), &ff.base.git_ref),
            pick(
                args.ffmpeg_rockchip_ref,
                prev_ff.map(|f| f.rockchip.reference.clone()),
                &ff.rockchip.git_ref,
            ),
        ),
        None => (String::new(), String::new()),
    };
    let blobs_dir = args.blobs_dir.clone().unwrap_or_else(|| {
        let rel = format!("blobs/{}", build.soc.as_str());
        root.find_asset(&rel).unwrap_or_else(|| root.path().join(rel))
    });
    let manifest = args
        .rootfs_manifest
        .unwrap_or_else(|| format!("{recipe}.pkgs.lock"));
    let opts = pins::UpdateOptions {
        kernel_ref: &kernel_ref,
        uboot_ref: &uboot_ref,
        mpp_ref: &mpp_ref,
        librga_ref: &librga_ref,
        libmali_ref: &libmali_ref,
        ffmpeg_base_ref: &ffmpeg_base_ref,
        ffmpeg_rockchip_ref: &ffmpeg_rockchip_ref,
        blobs_dir: &blobs_dir,
        patches_path: &args.patches_path,
        rootfs_manifest: &manifest,
    };
    let lock = pins::resolve_lock(&build, &opts)?;
    // Fetch + verify + store each pre-built extra_deb before committing the lock, so
    // a dead URL, a missing file, or a wrong hash fails now rather than at the next
    // build. Fills the durable content store `build` later reads.
    if !lock.extra_debs.is_empty() {
        let sink = |e: Event| print_event(&e);
        let step = Step::start(&sink, "extra-debs");
        let store = DebStore::open(&extra_debs_store(root))?;
        extradebs::materialize(root, &lock.extra_debs, &store, &step)?;
        step.finish();
    }
    let path = root.lock_path(recipe)?;
    pins::write_lock(&path, &lock)?;

    println!("wrote {}", path.display());
    println!(
        "  kernel   {} {} {}",
        lock.kernel.id,
        lock.kernel.reference,
        short(&lock.kernel.commit)
    );
    println!("  u-boot   {} {}", lock.uboot.reference, short(&lock.uboot.commit));
    // A no-patch kernel has no series to report; printing an empty row would imply
    // one exists.
    match &lock.patches {
        Some(p) => println!("  patches  {} {}", p.profile, short(&p.commit)),
        None => println!("  patches  (none — this kernel applies no series)"),
    }
    if let Some(us) = &lock.userspace {
        println!("  mpp      {} {}", us.mpp.reference, short(&us.mpp.commit));
        println!("  librga   {} {}", us.librga.reference, short(&us.librga.commit));
        println!("  libmali  {} {}", us.libmali.reference, short(&us.libmali.commit));
    }
    if let Some(ff) = &lock.ffmpeg {
        println!("  ffmpeg   {} {}", ff.base.reference, short(&ff.base.commit));
        println!(
            "  ff-rk    {} {} (graft provenance)",
            ff.rockchip.reference,
            short(&ff.rockchip.commit)
        );
    }
    println!("  rootfs   {} (manifest {})", lock.rootfs.suite, lock.rootfs.manifest);
    println!("  blob atf {}", lock.blobs.atf);
    println!("  blob tpl {}", lock.blobs.tpl);
    if let Some(bl32) = &lock.blobs.bl32 {
        println!("  blob bl32 {bl32}");
    }
    for d in &lock.extra_debs {
        println!("  extradeb {} {}", d.locator_label(), short(&d.sha256));
    }

    // Source-pin durability: flag, at pin time, any source that did not
    // resolve to a durable release tag — an ephemeral branch tip, or a commit
    // advertised by no ref (which may exist only in a local checkout and is then not
    // reproducible from upstream). Cheap: one `git ls-remote` per source against its
    // *configured* URL, no ancestry fetch; `verify-sources` does the deep reachability
    // probe. Advisory — never blocks the lock write (the onus is on whoever pins a
    // non-durable source).
    let axes = source_axes(&build, &lock)?;
    let mut flagged = false;
    for axis in &axes {
        match sources::pin_warning(&axis.url, axis.reference, axis.commit) {
            sources::PinWarning::Durable => {}
            sources::PinWarning::Ephemeral(branch) => {
                flagged = true;
                eprintln!(
                    "  warning: {} pins the tip of branch '{branch}' — a force-push/rebase/delete \
                     can orphan it; pin a release tag for durability",
                    axis.name
                );
            }
            sources::PinWarning::Unadvertised => {
                flagged = true;
                eprintln!(
                    "  note: {} commit {} is advertised by no tag or branch on {} — if it exists \
                     only in a local checkout this pin is NOT reproducible from upstream; run \
                     `boot2deb verify-sources {recipe}` to confirm reachability",
                    axis.name,
                    short(axis.commit),
                    axis.url
                );
            }
            sources::PinWarning::Skipped(reason) => {
                eprintln!("  note: could not check {} pin durability: {reason}", axis.name);
            }
        }
    }
    if flagged {
        eprintln!(
            "  (durable = a release tag, re-fetchable forever; see \
             `boot2deb verify-sources {recipe}` for the full reachability report)"
        );
    }
    Ok(())
}
