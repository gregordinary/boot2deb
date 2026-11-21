//! `try`: boot a built image under QEMU before it is flashed. See
//! [`boot2deb_engine::tryboot`] for what the run asserts and how the guest is
//! driven.
//!
//! This module handles the config half. It locates the recipe's image artifact through
//! the point's artifact inventory, and the password in the image's credential document.
//! It says where the fixture root is provisioned from, and reports the result.

use crate::args::TryArgs;
use crate::fsutil::absolutize;
use crate::render::{print_event_at, Verbosity};
use boot2deb_core::model::Overrides;
use boot2deb_core::{resolve_recipe, ConfigRoot};
use boot2deb_engine::event::Event;
use boot2deb_engine::tryboot;
use std::time::Duration;

pub(crate) fn run(
    root: &ConfigRoot,
    recipe: &str,
    args: TryArgs,
    verbosity: Verbosity,
) -> Result<(), Box<dyn std::error::Error>> {
    let point = crate::config::build_point(recipe, Vec::new())?;
    let reference = point.reference();
    let recipe = reference.as_str();
    let stem = point.artifact_stem();

    let resolved = resolve_recipe(root, recipe, &Overrides::default())?;
    let Some(resolved_image) = resolved.image.as_ref() else {
        return Err(format!(
            "'{recipe}' builds only a bootloader — there is no image to boot; \
             `try` tests the userland of an image build"
        )
        .into());
    };

    let work_dir = crate::workdir::work_dir_for(root, recipe, args.work_dir.clone());
    let out_dir = absolutize(
        args.out_dir
            .clone()
            .unwrap_or_else(|| work_dir.join("artifacts")),
    );

    // The artifact that contains the rootfs — on a split build the boot image is
    // not bootable under `-M virt` and is not what `try` tests. Taken through the
    // point's artifact inventory: the image stage's entry for the current lock and
    // layout, and the file's bytes as that entry records them.
    let role = boot2deb_core::press::roles(&resolved)
        .into_iter()
        .find(|r| r.contains_rootfs())
        .expect("an image build has a rootfs-containing artifact");
    let inventory = crate::inventory::load(&out_dir, &stem)?;
    let entry = crate::inventory::current(
        &inventory,
        boot2deb_core::inventory::Stage::Image,
        &crate::inventory::lock_digest(root, recipe)?,
        &out_dir,
        recipe,
    )?;
    if entry.layout != Some(resolved.layout) {
        return Err(format!(
            "the build laid the image out {}, and the recipe resolves {} — rebuild it",
            entry
                .layout
                .map_or_else(|| "with no recorded layout".to_string(), |l| l.to_string()),
            resolved.layout
        )
        .into());
    }
    let row = entry.disk_image(role).ok_or_else(|| {
        format!(
            "the build published no {} — run `boot2deb build {recipe}`",
            role.describe()
        )
    })?;
    let image = crate::inventory::verified(&out_dir, row)?;

    // `try` logs in with the generated password, which is the assertion that it works.
    // It is in the image's private credential document and nowhere else.
    let cred_path = out_dir.join(boot2deb_core::credential::credential_name(&stem));
    let cred_text = std::fs::read_to_string(&cred_path).map_err(|e| {
        format!(
            "no credential document at {} ({e}) — `try` logs in with the generated \
             first-boot password recorded there; run `boot2deb build {recipe}` first",
            cred_path.display()
        )
    })?;
    let credentials = boot2deb_core::credential::FirstBootCredential::from_toml_str(
        &cred_text,
        &cred_path.display().to_string(),
    )?;
    if credentials.image != role.file_name(&stem) {
        return Err(format!(
            "{} is the password for {}, not for the {} — rebuild: boot2deb build {recipe}",
            cred_path.display(),
            credentials.image,
            role.describe()
        )
        .into());
    }

    // Where the fixture root is provisioned from. It is the image's own suite and
    // architecture, resolved against the same archive and cached in the same download
    // cache the build's provisioner runs use — but it is a root of `try`'s own, for
    // the reason [`tryboot::FixtureSpec`] gives.
    let keyring = crate::sandboxes::keyring(root, &resolved.distro, args.keyring.clone(), false)?;
    let archive = boot2deb_engine::Archive::resolve(
        &resolved.distro,
        None,
        boot2deb_core::lock::SnapshotMode::Off,
    )?;
    let deb_cache = work_dir.join("cache").join("provisioner-debs");

    let sink = move |e: Event| print_event_at(verbosity, &e);
    let try_dir = work_dir.join("try");
    let fixture = {
        let step = boot2deb_engine::Step::start(&sink, tryboot::FIXTURE_STAGE);
        let fixture = tryboot::fixture_kernel(
            &tryboot::FixtureSpec {
                arch: resolved.arch,
                kernel: tryboot::fixture_package(&resolved.distro, resolved.arch)?,
                archive: &archive,
                components: &resolved.distro.components,
                keyring: &keyring,
                deb_cache: &deb_cache,
                scratch_dir: &work_dir,
                // The firmware the UEFI phase boots an efi image's ESP under.
                uefi: resolved_image
                    .loader
                    .as_ref()
                    .is_some_and(|l| l.efi().is_some()),
            },
            &try_dir.join("fixture"),
            args.refresh_fixture,
            &step,
        )?;
        step.finish();
        fixture
    };

    // A distro-package kernel build boots the same kernel it ships; say so, per
    // the honest framing that the kernel here is a fixture.
    if let Some(boot2deb_core::model::ResolvedKernel::Distro(k)) =
        resolved.image.as_ref().map(|i| &i.kernel)
    {
        println!(
            "note: this build ships {} itself, so the fixture kernel and the shipped \
             kernel coincide",
            k.package
        );
    }

    let opts = tryboot::TryOptions {
        build: &resolved,
        resolved_image,
        image: &image,
        disk: try_dir.join(format!("{stem}.try.img")),
        fixture: &fixture,
        user: &credentials.user,
        password: &credentials.password,
        boot_timeout: Duration::from_secs(args.timeout),
        keep_disk: args.keep_disk,
    };
    let report = tryboot::try_boot(&opts, &sink)?;

    println!("try {recipe}: PASS");
    if let Some(uefi) = &report.uefi {
        println!(
            "  UEFI boot    firmware -> GRUB (removable path) -> Linux {} running ({})",
            uefi.kernel, uefi.cpu
        );
    }
    println!(
        "  first boot   {}, first-boot completed, selftest: {}",
        report.first.state, report.first.selftest
    );
    println!(
        "  second boot  {}, first-boot did not re-run, selftest: {}",
        report.second.state, report.second.selftest
    );
    if args.keep_disk {
        println!(
            "  disk kept at {} (account '{}', password now '{}')",
            opts.disk.display(),
            credentials.user,
            report.disk_password
        );
    }
    Ok(())
}
