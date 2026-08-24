//! `update`: resolve upstream refs, hash the blobs, and write the recipe's `.lock`.
//!
//! The sole path that consults upstream — `build` reads only the lock. An omitted
//! per-tree ref flag re-pins the config layer's declared ref, so an authored
//! constraint bump propagates, while a lock pinned to a bare commit sha is left
//! alone as a deliberate hand-pin ([`boot2deb_core::repin`]); the kernel, whose
//! config declares no ref, inherits the previous lock's. After the lock is written,
//! every pinned source is checked for re-fetch durability and any
//! ephemeral/unadvertised pin is flagged (advisory — it never blocks the write).

use crate::args::UpdateArgs;
use crate::config::{default_patches_checkout, extra_debs_store, preflight_config, source_axes};
use crate::render::{print_event_at, short, Verbosity};
use boot2deb_core::model::Overrides;
use boot2deb_core::series::Scope;
use boot2deb_core::{resolve_recipe, ConfigRoot};
use boot2deb_engine::debstore::DebStore;
use boot2deb_engine::event::{Event, Step};
use boot2deb_engine::{extradebs, pins, sources};

/// Choose the ref to pin for one source axis and record the choice when it follows a
/// moved config constraint.
///
/// Thin wrapper over [`boot2deb_core::repin::pick_ref`] that adds the two things the
/// command needs around it: `configured: None` is the "this build carries no such
/// tree" case (an undeclared SoC source, or u-boot on a board whose firmware is its
/// own), which pins nothing and yields the empty string `resolve_lock` never reads;
/// and a followed bump is appended to `bumps` under `axis` for the post-write report.
fn take(
    bumps: &mut Vec<String>,
    axis: &str,
    flag: Option<String>,
    locked: Option<&str>,
    configured: Option<&str>,
) -> String {
    let Some(configured) = configured else {
        return String::new();
    };
    let chosen = boot2deb_core::repin::pick_ref(flag, locked, configured);
    if boot2deb_core::repin::is_config_bump(locked, &chosen, configured) {
        bumps.push(format!("{axis} {} -> {chosen}", locked.unwrap_or_default()));
    }
    chosen
}

/// Run `update <recipe>`.
pub(crate) fn run(
    root: &ConfigRoot,
    recipe: &str,
    args: UpdateArgs,
    verbosity: Verbosity,
) -> Result<(), Box<dyn std::error::Error>> {
    // A `--feature` selection makes this a *variant* of the recipe. Every path below
    // is derived from the reference rather than the recipe name, so the variant's
    // lock and solved manifest land beside — never on top of — the recipe's own.
    let point = crate::config::build_point(recipe, args.features)?;
    let reference = point.reference();
    let recipe = reference.as_str();
    let build = resolve_recipe(root, recipe, &Overrides::default())?;
    // Validate the local config invariants (image geometry, kernel-fragment and
    // apt-keyring existence) before resolving/committing the lock, so a bad
    // `rootfs_offset` or a typo'd fragment fails here rather than being pinned into
    // the lock and failing at the next build.
    preflight_config(root, &build)?;
    // An omitted per-tree ref flag takes the *config's* declared ref, so editing a
    // constraint in `boot-methods/` or `socs/` reaches every already-locked recipe on
    // the next `update` rather than moving only the boards that have no lock yet. A
    // lock pinned to a bare commit sha is the exception and is left alone — see
    // [`boot2deb_core::repin`] for the whole rule. Flags still override.
    //
    // A variant's *first* update inherits the recipe's lock instead of starting bare.
    // A feature selection is the only thing that differs from the recipe, so every
    // source axis should start where the recipe already pinned it — otherwise the
    // variant would demand a `--kernel-ref` the operator never chose and could easily
    // answer differently, silently building the "same" image against another kernel.
    let prev = root
        .lock(recipe)
        .ok()
        .or_else(|| point.is_variant().then(|| root.lock(point.recipe()).ok())?);
    // Constraint bumps this run followed, reported after the lock is written so a
    // propagated move is visible at pin time instead of surfacing later as an
    // unexplained ref change in the lock diff.
    let mut bumps: Vec<String> = Vec::new();
    // The kernel ref has no config default (the config carries only a `track`, not a
    // concrete tag), so an omitted `--kernel-ref` inherits the previous lock's ref —
    // the "re-pin what changed" model for a patch-only update. Only the first update
    // (no prior lock) must supply it.
    // Only a compiled kernel has a ref to pin; a distro kernel's version rides the
    // package set, so an absent `--kernel-ref` is fine and the value goes unread.
    let kernel_ref = match (args.kernel_ref, build.compiles_kernel()) {
        (Some(r), _) => r,
        (None, false) => String::new(),
        (None, true) => prev
            .as_ref()
            .and_then(|l| l.kernel.as_ref())
            .map(|k| k.reference.clone())
            .ok_or_else(|| {
                format!(
                    "no --kernel-ref given and no existing lock for '{recipe}' to inherit it \
                     from — pass --kernel-ref <tag> (e.g. v7.2) for the first update"
                )
            })?,
    };
    // Likewise u-boot: only the boot method that compiles one has a ref. The *boot
    // method* declares it, not the device, so bumping that one constraint is a
    // statement about every board on the method and reaches all of them here.
    let uboot_ref = take(
        &mut bumps,
        "u-boot",
        args.uboot_ref,
        prev.as_ref()
            .and_then(|l| l.uboot.as_ref())
            .map(|u| u.reference.as_str()),
        build.rkbin_boot().map(|b| b.uboot_ref.as_str()),
    );
    // Media-accel source refs are pinned only when the recipe builds the transcode
    // stack (its resolved build carries the sources). A base build leaves them empty —
    // `resolve_lock` never reads a ref without a matching source, and the lock omits
    // both pin tables.
    let prev_us = prev.as_ref().map(|l| l.userspace.as_slice()).unwrap_or(&[]);
    let prev_ff = prev.as_ref().and_then(|l| l.ffmpeg.as_ref());
    // One ref per tree the resolved build carries, keyed by name — the same rule the
    // out-of-tree modules follow, and for the same reason: which trees exist is the
    // SoC's statement, so there is no fixed set of flags to write.
    let userspace_refs: Vec<(String, String)> = build
        .image
        .iter()
        .flat_map(|i| &i.userspace)
        .map(|t| {
            let flag = args
                .userspace_refs
                .iter()
                .find(|(name, _)| name == &t.name)
                .map(|(_, r)| r.clone());
            let locked = prev_us
                .iter()
                .find(|p| p.name == t.name)
                .map(|p| p.reference.as_str());
            (
                t.name.clone(),
                take(&mut bumps, &t.name, flag, locked, Some(&t.git_ref)),
            )
        })
        .collect();
    let ff = build.image.as_ref().and_then(|i| i.ffmpeg.as_ref());
    let ffmpeg_base_ref = take(
        &mut bumps,
        "ffmpeg",
        args.ffmpeg_base_ref,
        prev_ff.map(|f| f.base.reference.as_str()),
        ff.map(|f| f.base.git_ref.as_str()),
    );
    let ffmpeg_rockchip_ref = take(
        &mut bumps,
        "ff-rk",
        args.ffmpeg_rockchip_ref,
        prev_ff
            .and_then(|f| f.rockchip.as_ref())
            .map(|p| p.reference.as_str()),
        ff.and_then(|f| f.rockchip.as_ref())
            .map(|s| s.git_ref.as_str()),
    );
    // Out-of-tree module refs follow the same rule, keyed by name. There is no
    // per-module `--*-ref` flag — a `device_kmods` entry names its own ref, so the
    // kmod layer's `ref` is the only constraint there is to follow.
    let prev_kmods = prev.as_ref().map(|l| l.kmods.as_slice()).unwrap_or(&[]);
    let kmod_refs: Vec<(String, String)> = build
        .image
        .iter()
        .flat_map(|i| &i.device_kmods)
        .map(|k| {
            let locked = prev_kmods
                .iter()
                .find(|p| p.name == k.name)
                .map(|p| p.reference.as_str());
            let reference = take(&mut bumps, &k.name, None, locked, Some(&k.git_ref));
            (k.name.clone(), reference)
        })
        .collect();
    let blobs_dir = args.blobs_dir.clone().unwrap_or_else(|| {
        let rel = format!("blobs/{}", build.soc.as_str());
        root.find_asset(&rel)
            .unwrap_or_else(|| root.path().join(rel))
    });
    // The `patches` checkout whose HEAD pins the series: the explicit flag, else the
    // config root's sibling — anchored to `--root` so the same tree is read whichever
    // directory `update` runs from.
    let patches_path = args
        .patches_path
        .clone()
        .unwrap_or_else(|| default_patches_checkout(root));
    let manifest = args.rootfs_manifest.unwrap_or_else(|| {
        // The manifest is a bare filename living beside the recipe in its device
        // folder, so it is named for the recipe's leaf, not the slashed reference:
        // `recipes/turing-rk1/media-accel-forky.pkgs.lock`, filename
        // `media-accel-forky.pkgs.lock`. Deliberately *not* the point's artifact stem,
        // which carries the device: the stem exists to disambiguate a flat output
        // directory, and inside `recipes/turing-rk1/` the device is the folder name
        // already. `build` publishes its own copy under the stem.
        let leaf = recipe.rsplit('/').next().unwrap_or(recipe);
        boot2deb_core::manifest::manifest_name(leaf)
    });
    let opts = pins::UpdateOptions {
        kernel_ref: &kernel_ref,
        uboot_ref: &uboot_ref,
        userspace_refs: &userspace_refs,
        ffmpeg_base_ref: &ffmpeg_base_ref,
        ffmpeg_rockchip_ref: &ffmpeg_rockchip_ref,
        kmod_refs: &kmod_refs,
        blobs_dir: &blobs_dir,
        patches_path: &patches_path,
        rootfs_manifest: &manifest,
    };
    let lock = pins::resolve_lock(&build, &opts)?;
    // Fetch + verify + store each pre-built extra_deb before committing the lock, so
    // a dead URL, a missing file, or a wrong hash fails now rather than at the next
    // build. Fills the durable content store `build` later reads.
    if !lock.extra_debs.is_empty() {
        let sink = move |e: Event| print_event_at(verbosity, &e);
        let step = Step::start(&sink, "extra-debs");
        let store = DebStore::open(&extra_debs_store(root))?;
        extradebs::materialize(root, &lock.extra_debs, &store, &step)?;
        step.finish();
    }
    let path = root.lock_path(recipe)?;
    pins::write_lock(&path, &lock)?;

    println!("wrote {}", path.display());
    // Name every pin that moved because a config constraint moved, not because the
    // caller named it. Without this the ref change is indistinguishable from a no-op
    // re-pin until someone reads the lock diff, which is exactly when a bump that
    // should have been noticed has already been committed.
    for bump in &bumps {
        println!("  bumped   {bump} (config constraint)");
    }
    // Only the pins this build actually has are printed. A row for an absent one
    // would claim a dependency the lock deliberately does not record.
    match (&lock.kernel, build.image.as_ref().map(|i| &i.kernel)) {
        (Some(k), _) => println!("  kernel   {} {} {}", k.id, k.reference, short(&k.commit)),
        (None, Some(k)) => println!(
            "  kernel   {} (distro package — version pinned in the package manifest)",
            k.id()
        ),
        (None, None) => println!("  kernel   (none — u-boot-only build)"),
    }
    match &lock.uboot {
        Some(u) => println!("  u-boot   {} {}", u.reference, short(&u.commit)),
        None => println!("  u-boot   (none — this board's firmware is its own)"),
    }
    if let Some(p) = &lock.uboot_patches {
        println!(
            "  u-boot patches {} {}",
            p.series.join(", "),
            short(&p.commit)
        );
    }
    // A no-patch kernel has no series to report; printing an empty row would imply
    // one exists.
    match &lock.patches {
        Some(p) => println!("  patches  {} {}", p.series.join(", "), short(&p.commit)),
        None => println!("  patches  (none — this kernel applies no series)"),
    }
    // Only the trees this SoC has. A line reading "none" would suggest something
    // failed to pin; the absence is the SoC not having that hardware.
    for pin in &lock.userspace {
        println!("  {:<8} {} {}", pin.name, pin.reference, short(&pin.commit));
    }
    if let Some(ff) = &lock.ffmpeg {
        println!(
            "  ffmpeg   {} {}",
            ff.base.reference,
            short(&ff.base.commit)
        );
        if let Some(rk) = &ff.rockchip {
            println!(
                "  ff-rk    {} {} (graft provenance)",
                rk.reference,
                short(&rk.commit)
            );
        }
    }
    match &lock.rootfs {
        Some(r) => println!("  rootfs   {} (manifest {})", r.suite, r.manifest),
        None => println!("  rootfs   (none — u-boot-only build)"),
    }
    if let Some(blobs) = &lock.blobs {
        println!("  blob atf {}", blobs.atf);
        println!("  blob tpl {}", blobs.tpl);
        if let Some(bl32) = &blobs.bl32 {
            println!("  blob bl32 {bl32}");
        }
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
                eprintln!(
                    "  note: could not check {} pin durability: {reason}",
                    axis.name
                );
            }
        }
    }
    if flagged {
        eprintln!(
            "  (durable = a release tag, re-fetchable forever; see \
             `boot2deb verify-sources {recipe}` for the full reachability report)"
        );
    }

    // Prerequisite check: does every composed series even *claim* the kernel just
    // pinned? Pure metadata — the manifests are already on disk — so it costs nothing
    // and answers at pin time what would otherwise surface only after the build had
    // cloned the kernel. Bumping onto a kernel the series predates is exactly the
    // routine move that hits this, and finding out then is late.
    //
    // Advisory, like the durability checks above. The lock is not wrong: pinning the
    // new kernel is the first step of adopting it, and the envelope is widened once
    // the evidence says it can be. Blocking here would force the claim to be widened
    // before anything had measured it, which is backwards.
    if let (Some(pin), Some(kernel)) = (&lock.patches, &lock.kernel) {
        let outside = crate::config::series_outside_envelope(
            &patches_path,
            &pin.series,
            Scope::Kernel,
            &kernel.reference,
        )?;
        for (name, declared) in &outside {
            eprintln!(
                "  note: kernel {} is outside series '{name}' (declared {declared}) — a build \
                 will refuse it. Measure it first, which needs no re-pin:\n    \
                 boot2deb verify-patches {recipe} --kernel {} --kernel-path <checkout> \
                 --keep-going\n  then widen applies_to_kernel in the series if it comes back \
                 clean, or retire the patches it names.",
                kernel.reference, kernel.reference
            );
        }
    }
    // The same advisory on the u-boot axis, against the u-boot tag this update just
    // pinned. Bumping a board's `uboot_ref` past what its series claims is the u-boot
    // equivalent of the kernel bump above, and equally worth hearing about at pin
    // time rather than after the build has cloned u-boot. There is no candidate path
    // here — u-boot has no `--kernel` equivalent — so the remedy is the claim itself.
    if let (Some(pin), Some(uboot)) = (&lock.uboot_patches, &lock.uboot) {
        let outside = crate::config::series_outside_envelope(
            &patches_path,
            &pin.series,
            Scope::Uboot,
            &uboot.reference,
        )?;
        for (name, declared) in &outside {
            eprintln!(
                "  note: u-boot {} is outside series '{name}' (declared {declared}) — a build \
                 will refuse it. Verify it first, which needs no re-pin:\n    \
                 boot2deb verify-patches {recipe} --keep-going\n  then widen \
                 applies_to_uboot in the series if it comes back clean, or retire the patches \
                 it names.",
                uboot.reference
            );
        }
    }

    // A `validated` claim asserts that an image from *these* pins booted. Moving any
    // of them retires that evidence, and this is the only moment both locks exist to
    // compare — after the write the previous pins are gone. Advisory like the
    // durability checks above: re-validating is the caller's call, not the tool's,
    // and a lock write that already succeeded is not undone by a stale claim.
    //
    // A variant is skipped: the claim belongs to the recipe, and a different feature
    // selection is a different build, so a variant neither inherits the claim nor can
    // retire it. Comparing here would be actively wrong on a variant's *first* update,
    // whose `prev` is the recipe's own lock — every pin would read as unmoved while
    // the build is not the one the claim describes at all.
    if point.is_variant() {
        return Ok(());
    }
    if let (Some(prev), Some(claim)) = (&prev, root.recipe(recipe)?.support) {
        let moved = boot2deb_core::support::pin_changes(prev, &lock);
        if claim.status == boot2deb_core::model::SupportStatus::Validated && !moved.is_empty() {
            eprintln!(
                "  warning: recipe '{recipe}' claims support = \"validated\" as of {}, but this \
                 update moved its pins:",
                claim.date
            );
            for change in &moved {
                eprintln!("    {change}");
            }
            eprintln!(
                "  that claim now describes a combination nothing has booted — re-validate on \
                 hardware and update the date, or set status = \"expected\" until you do"
            );
        }
        // The published support matrix is generated from these pins, so moving one
        // makes the committed page describe a build that no longer exists. A gate test
        // catches it, but only after the fact — as a red CI on whoever pushed the
        // re-pin. The reminder belongs at the moment the pins move, which is here, and
        // only when they actually did: a no-op re-pin has nothing to regenerate.
        if !moved.is_empty() {
            eprintln!(
                "  note: this re-pin changes the generated support matrix — regenerate it:\n    \
                 boot2deb support-matrix --markdown > docs/src/reference/support-matrix.md"
            );
        }
    }
    Ok(())
}
