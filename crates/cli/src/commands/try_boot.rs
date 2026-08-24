//! `try`: boot a built image under QEMU before it is flashed — see
//! [`boot2deb_engine::tryboot`] for what the run asserts and how the guest is
//! driven. This module owns the config half: locating the recipe's image
//! artifact and provenance manifest, standing up the target-arch sandbox the
//! fixture kernel harvests in, and reporting the result.

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

    // The artifact that carries the rootfs — on a split build the boot image is
    // not bootable under `-M virt` and is not what `try` tests.
    let role = boot2deb_core::press::roles(&resolved)
        .into_iter()
        .find(|r| r.carries_rootfs())
        .expect("an image build has a rootfs-carrying artifact");
    let image = super::press::find_artifact(&out_dir, &stem, role)?;

    // The generated password rides the provenance manifest, which stays with
    // the build — `try` authenticates with it, which is the assertion that it
    // works.
    let prov_path = out_dir.join(format!("{stem}.provenance.toml"));
    let prov_text = std::fs::read_to_string(&prov_path).map_err(|e| {
        format!(
            "no provenance manifest at {} ({e}) — `try` logs in with the generated \
             first-boot password recorded there; run `boot2deb build {recipe}` first",
            prov_path.display()
        )
    })?;
    let credentials = boot2deb_core::provenance::manifest_credentials(
        &prov_text,
        &prov_path.display().to_string(),
    )
    .map_err(|e| {
        format!("{e} — if this manifest is from an earlier build, rebuild: boot2deb build {recipe}")
    })?;

    // The target-arch sandbox the fixture kernel installs in — the same root
    // the package stages build in, so `try` adds no provisioning of its own.
    let pf = boot2deb_engine::preflight(resolved.arch);
    let host_deb_arch = crate::sandboxes::host_deb_arch(&pf)?;
    let keyring = crate::sandboxes::keyring(root, args.keyring.clone(), false)?;
    let mirrors = vec![boot2deb_engine::DEFAULT_MIRROR.to_string()];
    let roots = crate::sandboxes::roots(
        &resolved,
        &crate::sandboxes::RootInputs {
            work_dir: &work_dir,
            host_deb_arch,
            mirrors: &mirrors,
            keyring,
            deb_cache: work_dir.join("cache").join("provisioner-debs"),
        },
    );
    let sandbox = roots
        .target
        .expect("an image build resolves a suite and a target sandbox");

    let sink = move |e: Event| print_event_at(verbosity, &e);
    let try_dir = work_dir.join("try");
    let fixture = {
        let step = boot2deb_engine::Step::start(&sink, "try-fixture");
        let fixture = tryboot::fixture_kernel(
            sandbox.as_ref(),
            resolved.arch,
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
