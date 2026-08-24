//! `press`: produce a distributable image file from a build's artifacts —
//! verified, per-unit personalized, and optionally extended with per-site files.
//!
//! boot2deb does not write devices; the pressed file is what gets handed to a
//! flasher (`dd`, pyrographer, a vendor tool). Two paths produce it, chosen per
//! output: a press with nothing to add **streams** the existing compressed
//! artifact (decompress + digest tap + verify — a plain card must not cost a
//! rebuild), while a press with tree additions **re-assembles** the image from
//! the kept rootfs tar through [`boot2deb_engine::image::press_image`]. Either
//! way the seed keys are written last, into the finished file.
//!
//! What this module owns is config: which build point's artifacts these are,
//! how the resolved roles bind to output paths (one positional for one
//! artifact; `--boot-out` + `--rootfs-out` for a split build), and validating
//! the seed keys and additions before anything is written. The engine owns the
//! bytes.

use crate::args::{PressArgs, SeedKeyArgs};
use crate::fsutil::absolutize;
use crate::render::{print_event_at, Verbosity};
use boot2deb_core::model::Overrides;
use boot2deb_core::press::{roles, ArtifactRole};
use boot2deb_core::{resolve_recipe, ConfigRoot, ResolvedBuild};
use boot2deb_engine::event::{Event, Step};
use boot2deb_engine::image::{press_image, BootPayload, ImageIdentity, PressOptions};
use boot2deb_engine::press::additions::TreeAdditions;
use boot2deb_engine::press::seed::SeedKeys;
use boot2deb_engine::press::{seed, verify, write};
use std::path::{Path, PathBuf};

/// Run `press <recipe> [output] [...]`.
pub(crate) fn run(
    root: &ConfigRoot,
    recipe: &str,
    output: Option<PathBuf>,
    args: PressArgs,
    verbosity: Verbosity,
) -> Result<(), Box<dyn std::error::Error>> {
    let point = crate::config::build_point(recipe, Vec::new())?;
    let reference = point.reference();
    let recipe = reference.as_str();
    let stem = point.artifact_stem();

    let overrides = Overrides {
        layout: args.layout,
        ..Overrides::default()
    };
    let resolved = resolve_recipe(root, recipe, &overrides)?;
    let roles = roles(&resolved);

    let work_dir = crate::workdir::work_dir_for(root, recipe, args.work_dir.clone());
    let out_dir = absolutize(
        args.out_dir
            .clone()
            .unwrap_or_else(|| work_dir.join("artifacts")),
    );

    // The identifiers this recipe's image carries, derived once: the
    // re-assembly stamps them into the GPT and the superblock, and a template
    // addition names them before any disk carries them.
    let identity = ImageIdentity::derive(recipe, &resolved.device);

    let keys = seed_keys(&args.keys)?;
    let additions = collect_additions(
        &args,
        &resolved,
        recipe,
        &stem,
        identity,
        keys.as_ref().and_then(|k| k.hostname.clone()),
        &out_dir,
    )?;

    // Seed keys and tree additions ride the rootfs; a build whose only artifact
    // is a boot image has nowhere to put either.
    if !roles.iter().any(|r| r.carries_rootfs()) {
        if keys.is_some() {
            return Err("this build presses only a boot image, which has no seed \
                        partition — seed keys do not apply"
                .into());
        }
        if !additions.is_empty() {
            return Err("this build presses only a boot image, which has no rootfs \
                        — tree additions do not apply"
                .into());
        }
    }

    let bound = bind_outputs(&roles, output, &args)?;

    if args.dry_run {
        return dry_run(&bound, &out_dir, &stem, &keys, &additions);
    }

    let sink = move |e: Event| print_event_at(verbosity, &e);
    for (role, out) in &bound {
        if role.carries_rootfs() && !additions.is_empty() {
            reassemble(
                &resolved, recipe, &stem, *role, out, &out_dir, &work_dir, &args, &additions,
                identity, &sink,
            )?;
        } else {
            stream(&out_dir, &stem, *role, out, &args, &sink)?;
        }
        // Personalization lands on the finished file, whichever path made it —
        // and only on the artifact that carries the seed.
        if role.carries_rootfs() {
            if let Some(keys) = &keys {
                seed::rewrite_seed(out, keys, now_secs())?;
                let step = Step::start(&sink, "press");
                step.log(format!(
                    "personalized {}: {}",
                    out.display(),
                    describe_keys(keys)
                ));
                step.finish();
            }
        }
    }
    Ok(())
}

/// The stream path: decompress the existing artifact into the output file with a
/// digest tap, then re-read and verify — the fast path a plain card takes.
fn stream(
    out_dir: &Path,
    stem: &str,
    role: ArtifactRole,
    out: &Path,
    args: &PressArgs,
    sink: &impl Fn(Event),
) -> Result<(), Box<dyn std::error::Error>> {
    let artifact = find_artifact(out_dir, stem, role)?;
    let step = Step::start(sink, "press");
    step.log(format!(
        "{}: {} -> {}",
        role.describe(),
        artifact.display(),
        out.display()
    ));
    if let Some(parent) = out.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)?;
    }
    let written = {
        let mut dest = std::fs::File::create(out)?;
        let written = write::stream_image(&artifact, &mut dest, &step)?;
        dest.sync_all()?;
        written
    };
    step.log(format!("wrote {} bytes", written.bytes));
    if !args.no_verify {
        step.log("verifying: re-reading what was written");
        verify::verify_digest(out, &written, &step)?;
        // The table compare runs only where the artifact carries a table — a
        // boot image does not.
        match verify::planned_table(&artifact) {
            Ok(planned) => {
                let read_back = verify::read_back_table(out)?;
                verify::compare_tables(&out.display().to_string(), &planned, &read_back)?;
                step.log(format!(
                    "verified: digest matches, partition table matches ({} entries)",
                    planned.len()
                ));
            }
            Err(_) => step.log("verified: digest matches (no partition table to compare)"),
        }
    }
    step.finish();
    Ok(())
}

/// The re-assembly path: rebuild the image from the kept rootfs tar with the
/// additions merged in, writing the output file directly.
#[allow(clippy::too_many_arguments)] // one call site; the args are the press itself
fn reassemble(
    resolved: &ResolvedBuild,
    recipe: &str,
    stem: &str,
    role: ArtifactRole,
    out: &Path,
    out_dir: &Path,
    work_dir: &Path,
    args: &PressArgs,
    additions: &TreeAdditions,
    identity: ImageIdentity,
    sink: &impl Fn(Event),
) -> Result<(), Box<dyn std::error::Error>> {
    let rootfs_tar = out_dir.join(format!("{stem}-rootfs.tar"));
    if !rootfs_tar.exists() {
        return Err(format!(
            "rootfs tar not found at {} — tree additions re-assemble the image from \
             the kept artifacts; run `boot2deb build {recipe}` first",
            rootfs_tar.display()
        )
        .into());
    }
    boot2deb_engine::rootfs::validate_tar(&rootfs_tar)?;

    // The boot payload, only where this output places one: a combined image
    // carries it, a split rootfs image is bootloader-agnostic.
    let idbloader = out_dir.join(format!("{stem}-idbloader.img"));
    let uboot_itb = out_dir.join(format!("{stem}-u-boot.itb"));
    let boot = match (role, &resolved.boot) {
        (ArtifactRole::Combined, boot2deb_core::model::ResolvedBoot::RockchipRkbin(_)) => {
            for p in [&idbloader, &uboot_itb] {
                if !p.exists() {
                    return Err(format!(
                        "{} not found — run `boot2deb build {recipe} --stage uboot` first",
                        p.display()
                    )
                    .into());
                }
            }
            Some(BootPayload::RockchipRkbin {
                idbloader: &idbloader,
                uboot_itb: &uboot_itb,
            })
        }
        (ArtifactRole::Combined, boot2deb_core::model::ResolvedBoot::Depthcharge(_)) => {
            Some(BootPayload::Depthcharge)
        }
        _ => None,
    };

    // A re-assembly rebuilds a rootfs into a new image, so it needs the image axis; a
    // u-boot deliverable never reaches here — `roles` gives it the boot role alone, and
    // this path is only entered for a role that carries a rootfs.
    let image = resolved.as_image().ok_or(
        "a u-boot deliverable has no rootfs to re-assemble; press streams its boot artifact",
    )?;
    let pressed = press_image(
        image,
        &PressOptions {
            rootfs_tar: &rootfs_tar,
            boot,
            role,
            output: out,
            work_dir: &work_dir.join("press"),
            rootfs_label: &args.rootfs_label,
            identity,
            additions,
        },
        sink,
    )?;
    let step = Step::start(sink, "press");
    if !args.no_verify {
        // The rootfs was already scan-verified inside the assembly; what is left
        // to hold is the disk around it, whose table must read back whole.
        let table = verify::read_back_table(out)?;
        step.log(format!(
            "verified: the pressed image carries a readable GPT ({} entries)",
            table.len()
        ));
    }
    // The pressed file's own credential — it exists nowhere else, so it is
    // surfaced exactly as a build's is.
    step.log(format!(
        "first-boot pw: {}  (user {}, expired — change at first login)",
        pressed.password,
        boot2deb_engine::rootfs::DEFAULT_USER
    ));
    step.finish();
    Ok(())
}

/// Bind each resolved role to the output path that names it: the positional for
/// a single-artifact build, `--boot-out` + `--rootfs-out` for a split build —
/// refusing the mixed forms so a half-specified press fails whole.
fn bind_outputs(
    roles: &[ArtifactRole],
    output: Option<PathBuf>,
    args: &PressArgs,
) -> Result<Vec<(ArtifactRole, PathBuf)>, Box<dyn std::error::Error>> {
    if roles.len() == 1 {
        if args.boot_out.is_some() || args.rootfs_out.is_some() {
            return Err(format!(
                "this build presses one file (the {}) — name it as the positional \
                 output, not with --boot-out/--rootfs-out",
                roles[0].describe()
            )
            .into());
        }
        let out = output
            .ok_or_else(|| format!("press needs an output path for the {}", roles[0].describe()))?;
        return Ok(vec![(roles[0], out)]);
    }
    if output.is_some() {
        return Err(
            "this build is a split layout: two artifacts, two files. Name both \
                    outputs with --boot-out and --rootfs-out instead of one positional"
                .into(),
        );
    }
    let boot_out = args
        .boot_out
        .clone()
        .ok_or("the boot image needs --boot-out <file>")?;
    let rootfs_out = args
        .rootfs_out
        .clone()
        .ok_or("the rootfs image needs --rootfs-out <file>")?;
    Ok(vec![
        (ArtifactRole::Boot, boot_out),
        (ArtifactRole::Rootfs, rootfs_out),
    ])
}

/// The dry run: everything the press would produce, nothing written.
fn dry_run(
    bound: &[(ArtifactRole, PathBuf)],
    out_dir: &Path,
    stem: &str,
    keys: &Option<SeedKeys>,
    additions: &TreeAdditions,
) -> Result<(), Box<dyn std::error::Error>> {
    for (role, out) in bound {
        let reassembles = role.carries_rootfs() && !additions.is_empty();
        if reassembles {
            println!(
                "{}: re-assemble from {} -> {}",
                role.describe(),
                out_dir.join(format!("{stem}-rootfs.tar")).display(),
                out.display()
            );
            for line in additions.describe() {
                println!("  {line}");
            }
        } else {
            let artifact = find_artifact(out_dir, stem, *role)?;
            println!(
                "{}: stream {} -> {}",
                role.describe(),
                artifact.display(),
                out.display()
            );
            if let Ok(bytes) = verify::planned_image_bytes(&artifact) {
                println!("  needs a medium of at least {bytes} bytes");
            }
        }
        if role.carries_rootfs() {
            println!(
                "  seed: {}",
                keys.as_ref().map_or_else(
                    || "the empty template (no keys named)".into(),
                    describe_keys
                )
            );
        }
    }
    println!("dry run: nothing was written");
    Ok(())
}

/// Validate and collect the seed keys named on the command line. `None` when no
/// key was named at all — the pressed file then keeps its built-in empty seed.
pub(crate) fn seed_keys(
    args: &SeedKeyArgs,
) -> Result<Option<SeedKeys>, Box<dyn std::error::Error>> {
    if args.is_empty() {
        return Ok(None);
    }
    if let Some(hostname) = &args.hostname {
        boot2deb_core::hostname::check(hostname)
            .map_err(|why| format!("--hostname {hostname:?}: {why}"))?;
    }
    for key in &args.ssh_keys {
        boot2deb_core::authkeys::check_authorized_key(key)
            .map_err(|why| format!("--ssh-key: {why}"))?;
    }
    if let Some(ssid) = &args.wifi_ssid {
        boot2deb_core::wifi::check_ssid(ssid).map_err(|why| format!("--wifi-ssid: {why}"))?;
    }
    if let Some(psk) = &args.wifi_psk {
        boot2deb_core::wifi::check_psk(psk).map_err(|why| format!("--wifi-psk: {why}"))?;
    }
    if let Some(ip) = &args.static_ip {
        boot2deb_core::staticip::check(ip).map_err(|why| format!("--static-ip: {why}"))?;
    }
    Ok(Some(SeedKeys {
        hostname: args.hostname.clone(),
        authorized_keys: args.ssh_keys.clone(),
        wifi_ssid: args.wifi_ssid.clone(),
        wifi_psk: args.wifi_psk.clone(),
        static_ip: args.static_ip.clone(),
    }))
}

/// Collect the tree additions named on the command line, resolving
/// `--embed-image` to the recipe's own compressed artifact.
fn collect_additions(
    args: &PressArgs,
    resolved: &ResolvedBuild,
    recipe: &str,
    stem: &str,
    identity: ImageIdentity,
    seed_hostname: Option<String>,
    out_dir: &Path,
) -> Result<TreeAdditions, Box<dyn std::error::Error>> {
    let mut additions = TreeAdditions::new(stem, recipe, identity).seed_hostname(seed_hostname);
    for spec in &args.copy {
        let Some((src, dest)) = spec.split_once(':') else {
            return Err(format!("--copy takes SRC:DEST, got {spec:?}").into());
        };
        additions.copy(Path::new(src), dest)?;
    }
    for dir in &args.copy_tree {
        additions.copy_tree(dir)?;
    }
    for deb in &args.debs {
        additions.deb(deb)?;
    }
    if args.embed_image {
        if resolved.layout != boot2deb_core::model::Layout::Combined || !resolved.produces_image() {
            return Err(
                "--embed-image carries the recipe's own installable artifact, \
                        which only a combined-layout image build has"
                    .into(),
            );
        }
        additions.embed_image(&find_compressed_artifact(out_dir, stem)?)?;
    }
    Ok(additions)
}

/// The artifact a role streams: the raw image if the build kept it, else the
/// compressed form, in the build's own preference order.
pub(crate) fn find_artifact(
    out_dir: &Path,
    stem: &str,
    role: ArtifactRole,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let base = role.file_name(stem);
    let candidates = [base.clone(), format!("{base}.xz"), format!("{base}.gz")];
    for name in &candidates {
        let path = out_dir.join(name);
        if path.exists() {
            return Ok(path);
        }
    }
    Err(format!(
        "no {} found under {} (looked for {}) — run `boot2deb build` first",
        role.describe(),
        out_dir.display(),
        candidates.join(", "),
    )
    .into())
}

/// The compressed combined artifact `--embed-image` carries: compressed only,
/// because the embedded copy is decompressed by the board at install time and a
/// raw multi-gigabyte payload would double the pressed image for nothing.
fn find_compressed_artifact(
    out_dir: &Path,
    stem: &str,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let candidates = [format!("{stem}.img.xz"), format!("{stem}.img.gz")];
    for name in &candidates {
        let path = out_dir.join(name);
        if path.exists() {
            return Ok(path);
        }
    }
    Err(format!(
        "no compressed image artifact under {} (looked for {}) — --embed-image \
         carries the build's compressed image; run `boot2deb build` with \
         compression on (the default) first",
        out_dir.display(),
        candidates.join(", "),
    )
    .into())
}

/// What the seed will say, for logs and the dry run.
fn describe_keys(keys: &SeedKeys) -> String {
    if keys.is_empty() {
        return "reset to the empty template".into();
    }
    let mut parts = Vec::new();
    if let Some(h) = &keys.hostname {
        parts.push(format!("hostname={h}"));
    }
    if !keys.authorized_keys.is_empty() {
        parts.push(format!("{} ssh key(s)", keys.authorized_keys.len()));
    }
    if let Some(ssid) = &keys.wifi_ssid {
        parts.push(format!(
            "wifi {ssid:?} ({})",
            if keys.wifi_psk.is_some() {
                "wpa"
            } else {
                "open"
            }
        ));
    }
    if let Some(ip) = &keys.static_ip {
        parts.push(format!("static ip {ip}"));
    }
    parts.join(", ")
}

/// The wall clock, for the seed FAT's timestamp — per-unit data, not a
/// reproducible artifact.
pub(crate) fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}
