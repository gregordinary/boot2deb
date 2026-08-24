//! Axis resolution: a device (+ optional recipe + CLI overrides) collapses to a
//! fully-validated [`ResolvedBuild`]. Deterministic, no I/O beyond reading the
//! config layers through [`ConfigRoot`].

use crate::error::ConfigError;
use crate::loader::ConfigRoot;
use crate::model::*;

/// Resolve a device directly (device defaults, then `overrides`).
pub fn resolve_device(
    root: &ConfigRoot,
    device_name: &str,
    overrides: &Overrides,
) -> Result<ResolvedBuild, ConfigError> {
    let (device, device_lineage) = root.device_with_lineage(device_name)?;
    let soc = root.soc(device.soc)?;
    let arch = root.arch(soc.arch)?;
    // Taken before the layers are consumed by the moves below, since both
    // construction sites need them and neither runs before the other.
    let (soc_caveats, device_caveats) = (soc.caveats.clone(), device.caveats.clone());

    // Boot method: override must be within the device's supported set.
    let boot_method = overrides.boot_method.unwrap_or(device.boot_method);
    if !device.supported_boot_methods.contains(&boot_method) {
        return Err(ConfigError::UnsupportedBootMethod {
            device: device_name.to_string(),
            boot_method: boot_method.to_string(),
            supported: join(&device.supported_boot_methods),
        });
    }
    let bm = root.boot_method(boot_method)?;

    let uboot_only = overrides.deliverable == Deliverable::Uboot;
    // A u-boot-only deliverable only exists where the boot method builds a u-boot of
    // ours; a depthcharge board's firmware is not ours to emit.
    if uboot_only && boot_method != BootMethod::RockchipRkbin {
        return Err(ConfigError::UbootOnlyWithoutBootloader {
            device: device_name.to_string(),
            boot_method: boot_method.to_string(),
        });
    }

    // A board carrying its own device tree must actually build the DTB it boots;
    // check that here so a filename typo is a typed error rather than a kernel that
    // builds fine and then finds no DTB at boot.
    validate_device_dts(&device.device_dts, &device.kernel_dtb, device_name)?;

    // Out-of-tree modules: each name the board opted into loads its own `kmods/` layer
    // and is shape-checked before any build work — an escaping subdir would feed
    // `make M=` a foreign tree, an escaping local-patch path would read a file from
    // outside the config root, and a bad name would produce an unbuildable `.deb`. The
    // "distro kernel compiles nothing" case is caught separately in `resolve_kernel`,
    // where the kernel choice is known. Validated here, before the u-boot-only early
    // return, so a broken `kmods/<name>.toml` is an error on every path the board
    // builds — not only the ones that would compile it.
    let device_kmods = resolve_kmods(root, &device.device_kmods, device_name)?;

    let kernel_cmdline = validate_kernel_cmdline(device.kernel_cmdline.as_deref())?;

    // The board's name on the network, checked before the u-boot-only early return so
    // a typo in `devices/<name>.toml` is an error on every path the board builds. Only
    // the image path writes it, but a device file that cannot produce a valid image is
    // worth failing on whichever recipe finds it first.
    validate_hostname(&device.hostname)?;

    let layout = overrides.layout.unwrap_or(device.default_layout);

    // The boot method's *own* requirements, enforced only for the method that has
    // them: rkbin blobs, a `uboot_defconfig`, and the selected u-boot series where
    // u-boot is compiled; a board profile where a signed kernel partition is written.
    // A board is never asked for a field its boot method would not read.
    let boot = resolve_boot(
        &bm,
        &device,
        &soc,
        device_name,
        layout,
        overrides.board.as_deref(),
        &kernel_cmdline,
        overrides.uboot_series.as_deref(),
    )?;

    // A u-boot-only build resolves no kernel, suite, features, or rootfs: its sole
    // deliverable is the bootloader the u-boot node emits, so every image axis is
    // absent and the lock records only the u-boot pins. The `image_size`/`hostname`
    // it carries are the device defaults, never read on this path (no image node).
    if uboot_only {
        // Every rootfs-axis override is rejected before the early return rather than
        // dropped with it. This path skips feature loading, `resolve_l10n`, and the
        // `image_size` parse, so a value that is a hard error on every other path
        // — a misspelled `--feature`, an `--image-size 1M` — would otherwise be
        // accepted in silence and exit 0.
        reject_rootfs_overrides(device_name, overrides)?;
        // This deliverable resolves no image suite, but it still produces a `.deb`, and
        // the root that archives it has to be provisioned for some suite. The board's
        // own declared default is that suite: it is the one this board's image builds
        // resolve, so they share a packaging root rather than provisioning two. Shape-
        // validated like any suite that reaches the archive — `supported_suites` is not
        // consulted, since that list constrains which suites this board is *imaged*
        // for, and nothing here is imaged.
        validate_suite(&device.default_suite)?;
        return Ok(ResolvedBuild {
            device: device_name.to_string(),
            device_lineage,
            description: device.description,
            arch: soc.arch,
            soc: device.soc,
            boot_method,
            kernel: None,
            suite: None,
            packaging_suite: device.default_suite,
            features: Vec::new(),
            // A bootloader has no rootfs and so no fstab to mount anything into.
            data_volumes: Vec::new(),
            rootfs_packages: Vec::new(),
            rootfs_exclude: Vec::new(),
            // A u-boot-only build resolves no kernel and no rootfs, so it has neither
            // the thing that decides this nor anything for it to subtract from.
            libre: false,
            layout,
            image_size: device.image_size,
            hostname: device.hostname,
            locale: String::new(),
            locales_generate: Vec::new(),
            timezone: String::new(),
            ntp_servers: Vec::new(),
            keymap: None,
            // A u-boot-only build creates no account: there is no rootfs to hold one,
            // no `/etc/sudoers.d`, and no `/etc/shadow` to splice a password into. The
            // neutral values below are placeholders for an axis this deliverable does
            // not have, and `reject_rootfs_overrides` refuses any flag that names one —
            // so nothing reads them.
            sudo: SudoPolicy::default(),
            first_boot_password_length: crate::model::DEFAULT_PASSWORD_LENGTH,
            ssh_authorized_keys: Vec::new(),
            boot,
            kernel_dtb: device.kernel_dtb,
            device_dts: device.device_dts,
            // A u-boot-only build compiles no kernel, so it has nothing to build a
            // module against and ships no `.ko`. The list is therefore empty rather
            // than carried: the lock pins one entry per resolved kmod, and pinning a
            // driver repo this deliverable never fetches would put a commit in the
            // lock that no artifact of the build depends on — and state, in the
            // support matrix, that a bootloader image carries a Wi-Fi driver.
            device_kmods: Vec::new(),
            kernel_cmdline,
            dt_dir: soc.dt_dir,
            modules: soc.modules,
            kernel_arch: arch.kernel_arch,
            cross_compile: arch.cross_compile,
            kbuild_image: arch.kbuild_image,
            userspace: None,
            ffmpeg: None,
            apt_sources: Vec::new(),
            extra_debs: Vec::new(),
            // The hardware limitations still hold: this deliverable produces the
            // firmware that board boots, and a bootloader is no less constrained by
            // the silicon than an image is.
            caveats: hardware_caveats(&soc_caveats, &device_caveats),
        });
    }

    // Features are resolved before the kernel, because they feed it: a capability
    // whose driver is out-of-tree contributes the patch series and kconfig
    // fragment that build it, and `resolve_kernel` composes those onto the kernel's
    // and the device's.
    let features = overrides.features.clone().unwrap_or_default();
    // Reject a feature selected twice: its overlay + packages would otherwise apply
    // twice.
    let mut seen_features = std::collections::HashSet::new();
    for name in &features {
        if !seen_features.insert(name) {
            return Err(ConfigError::DuplicateFeature {
                feature: name.clone(),
            });
        }
    }
    // Load + validate the selected features: each must exist, support the
    // resolved SoC, and not conflict with another in the set. The package-set +
    // overlay merge is the rootfs node's job; resolution rejects an ill-formed
    // selection up front.
    let mut loaded_features = Vec::with_capacity(features.len());
    for name in &features {
        let feat = root.feature(name)?;
        feat.ensure_supports_soc(name, device.soc)?;
        feat.ensure_supports_arch(name, soc.arch)?;
        loaded_features.push((name.clone(), feat));
    }
    crate::feature::ensure_no_conflicts(&loaded_features)?;

    // Kernel: override or device default, validated against the device's list
    // and the kernel's supported SoCs.
    let kernel_id = overrides
        .kernel
        .clone()
        .unwrap_or_else(|| device.default_kernel.clone());
    if !device.supported_kernels.contains(&kernel_id) {
        return Err(ConfigError::UnknownKernelForDevice {
            device: device_name.to_string(),
            kernel: kernel_id,
            supported: device.supported_kernels.join(", "),
        });
    }
    let kdef = root.kernel(&kernel_id)?;
    if !kdef.supported_socs().contains(&device.soc) {
        return Err(ConfigError::SocMismatch {
            kernel: kernel_id,
            soc: device.soc.to_string(),
            supported: join(kdef.supported_socs()),
        });
    }
    let kernel = resolve_kernel(kdef, kernel_id, &device, device_name, &loaded_features)?;

    let suite = overrides
        .suite
        .clone()
        .unwrap_or_else(|| device.default_suite.clone());
    // A bad suite otherwise fails deep in the bootstrap; reject it here, and
    // the shape guard also keeps a leading-`-` suite from ever reaching the archive as
    // a positional. Shape first, membership second, so a malformed string is named as
    // malformed rather than reported as absent from a list it could never join.
    validate_suite(&suite)?;
    check_suite_supported(device_name, &device.supported_suites, &suite)?;
    // A feature that builds the media-accel stack (`requires_media_accel`) needs the
    // SoC to supply the `[userspace]`/`[ffmpeg]` source trees its `.deb`s compile
    // from. Gate here so a bad composition fails at resolve, not deep in the build.
    // The flag also decides whether this build carries sources at all: a selection
    // with no such feature drops them, and the userspace/ffmpeg nodes are skipped.
    let needs_media_accel = crate::feature::first_requiring_media_accel(&loaded_features);
    if let Some(feature) = needs_media_accel {
        if soc.userspace.is_none() || soc.ffmpeg.is_none() {
            return Err(ConfigError::FeatureRequiresMediaAccel {
                feature: feature.to_string(),
                soc: device.soc.to_string(),
            });
        }
    }
    let build_media_accel = needs_media_accel.is_some();

    // Union the features' third-party apt sources, de-duplicated by name.
    // Two features contributing an identical source share it; a same-name/
    // different-definition clash is a resolution error, since the bootstrap could
    // not tell which repo to activate.
    let apt_sources = merge_apt_sources(&loaded_features)?;

    // Merge the rootfs package set: base ∪ soc ∪ boot-method ∪ device ∪ kernel ∪ Σ
    // features, de-duplicated with order preserved (base first). apt solves the
    // set, so order is not load-bearing — it only keeps the merged list stable.
    //
    // A distro-package kernel joins the set here: it installs from the mirror like
    // any other package (and is pinned like one, in the solved manifest), rather than
    // arriving as a built artifact through the local repo.
    let base = root.base()?;
    let kernel_packages: Vec<String> = match &kernel {
        ResolvedKernel::Distro(k) => vec![k.package.clone()],
        ResolvedKernel::Compiled(_) => Vec::new(),
    };
    //
    // The two hardware layers' nonfree firmware merges in its own layer's position on
    // an ordinary build and is left out entirely on a libre one, where the kernel has
    // no loader that could ask for it — see [`ResolvedBuild::libre`]. It is a
    // subtraction from the *include* set rather than an entry in `rootfs_exclude`,
    // because the package is not unwanted: it is unreachable, and naming it in an
    // exclude set would also forbid it as any other package's dependency.
    let libre = kernel.libre();
    let (soc_firmware, device_firmware) = match libre {
        true => (&[][..], &[][..]),
        false => (
            soc.nonfree_firmware_packages.as_slice(),
            device.nonfree_firmware_packages.as_slice(),
        ),
    };
    let mut rootfs_packages = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for src in [
        base.packages.as_slice(),
        soc.packages.as_slice(),
        soc_firmware,
        bm.packages(),
        device.packages.as_slice(),
        device_firmware,
        kernel_packages.as_slice(),
    ] {
        extend_unique(&mut rootfs_packages, &mut seen, src);
    }
    for (_, feat) in &loaded_features {
        extend_unique(&mut rootfs_packages, &mut seen, &feat.packages);
    }

    // Exclude set: every layer's + feature's `exclude`, unioned. A scoped
    // subtraction is the one thing a pure package union cannot express, so it is a
    // separate set rather than a negative entry in the include list.
    let mut rootfs_exclude = Vec::new();
    let mut seen_exclude = std::collections::HashSet::new();
    for src in [
        base.exclude.as_slice(),
        soc.exclude.as_slice(),
        bm.exclude(),
        device.exclude.as_slice(),
    ] {
        extend_unique(&mut rootfs_exclude, &mut seen_exclude, src);
    }
    for (_, feat) in &loaded_features {
        extend_unique(&mut rootfs_exclude, &mut seen_exclude, &feat.exclude);
    }
    // Exclude wins: a name both included by some layer and excluded by
    // another is dropped from the include set, so the bootstrap is never handed the
    // same package as both `--include` and `--exclude`.
    rootfs_packages.retain(|pkg| !seen_exclude.contains(pkg));

    // Union the pre-built extra debs across every layer + feature,
    // de-duplicated by sha256 (the content identity) and validated (exactly one
    // locator, well-formed hash) up front — a malformed pin fails at resolve, not
    // mid-build.
    let extra_debs = merge_extra_debs(&base, &soc, &bm, &device, &loaded_features)?;

    let image_size = overrides
        .image_size
        .clone()
        .unwrap_or_else(|| device.image_size.clone());

    // Validate the authored image size up front: a typo (`"2GB!"`) must fail at
    // resolve, not deep in the image stage after the whole pipeline has run. The
    // resolved build keeps the authored string; the image node re-parses it into its
    // byte/LBA `Geometry`. A zero-size image is not buildable — a *fitted* one has no
    // size yet to be zero, since the rootfs decides it several stages later. (The boot
    // method's own offsets are parsed the same way, in `resolve_boot`.)
    if crate::size::parse_image_size(&image_size)? == crate::size::ImageSize::Fixed(0) {
        return Err(ConfigError::InvalidSize {
            value: image_size.clone(),
        });
    }

    // Localization: base-layer distro policy (locale, generated locales, timezone) plus
    // the device's keymap, each overridable. Validated here so a bad zone or a locale
    // with no codeset is a typed error at resolve, not a dangling /etc/localtime or an
    // ungenerated LANG discovered on the booted board.
    let (locale, locales_generate, timezone, keymap) = resolve_l10n(&base, &device, overrides)?;
    // Time *sync* rather than localization: the zone above says how to render the
    // clock, this says where the clock comes from.
    let ntp_servers = resolve_ntp_servers(&base, overrides)?;

    // The account axis: who can log in to the finished image and what reaching root
    // costs them. Base-layer policy, each part overridable. Validated here for the same
    // reason as the localization axes — a key `sshd` will skip and a password too short
    // to resist guessing are both invisible on a booted board.
    let account = resolve_account(&base, overrides)?;

    Ok(ResolvedBuild {
        device: device_name.to_string(),
        device_lineage,
        description: device.description,
        arch: soc.arch,
        soc: device.soc,
        boot_method,
        kernel: Some(kernel),
        // The image's own suite archives the image's own `.deb`s: one `dpkg` for the
        // whole build, so a `--suite sid` image does not carry a u-boot deb that
        // forky's `dpkg` produced.
        packaging_suite: suite.clone(),
        suite: Some(suite),
        features,
        // Filled in by `resolve_recipe`: a direct device build names no recipe and
        // so declares no volumes.
        data_volumes: Vec::new(),
        rootfs_packages,
        rootfs_exclude,
        libre,
        layout,
        image_size,
        hostname: device.hostname,
        locale,
        locales_generate,
        timezone,
        ntp_servers,
        keymap,
        sudo: account.sudo,
        first_boot_password_length: account.password_length,
        ssh_authorized_keys: account.authorized_keys,
        boot,
        kernel_dtb: device.kernel_dtb,
        device_dts: device.device_dts,
        device_kmods,
        kernel_cmdline,
        dt_dir: soc.dt_dir,
        modules: soc.modules,
        kernel_arch: arch.kernel_arch,
        cross_compile: arch.cross_compile,
        kbuild_image: arch.kbuild_image,
        // Sources ride only when a feature builds the stack; a base build drops
        // them (validated above: `build_media_accel` implies the SoC supplies both).
        userspace: build_media_accel.then_some(soc.userspace).flatten(),
        ffmpeg: build_media_accel.then_some(soc.ffmpeg).flatten(),
        apt_sources,
        extra_debs,
        // Silicon, then board, then each selected feature. A recipe's own caveats are
        // appended by [`resolve_recipe`], which is the only caller that has them.
        caveats: feature_caveats(
            hardware_caveats(&soc_caveats, &device_caveats),
            &loaded_features,
        ),
    })
}

/// Resolve the boot-method-specific half of a build, enforcing that method's
/// requirements — and only that method's.
///
/// This is where the layered config stops being uniform: `rockchip-rkbin` compiles
/// u-boot and so demands a `uboot_defconfig` and an rkbin blob set, while
/// `depthcharge` compiles no bootloader at all and instead demands a board profile.
/// Asking every device for every method's fields would make a Chromebook declare a
/// u-boot defconfig it will never build.
#[allow(clippy::too_many_arguments)]
fn resolve_boot(
    bm: &BootMethodLayer,
    device: &DeviceLayer,
    soc: &SocLayer,
    device_name: &str,
    layout: Layout,
    board_override: Option<&str>,
    kernel_cmdline: &str,
    uboot_series_override: Option<&str>,
) -> Result<ResolvedBoot, ConfigError> {
    match bm {
        BootMethodLayer::RockchipRkbin(l) => {
            let uboot_defconfig = device
                .uboot_defconfig
                .clone()
                .filter(|v| !v.trim().is_empty())
                .ok_or(ConfigError::MissingBootField {
                    device: device_name.to_string(),
                    boot_method: BootMethod::RockchipRkbin.as_str(),
                    what: "uboot_defconfig",
                })?;
            // rkbin is layered: the SoC supplies the defaults (SoC-generic ATF, a
            // common-memory DDR TPL, and BL32 where the boot chain needs OP-TEE) and
            // the device overrides per field (typically just the DDR TPL).
            let rkbin = resolve_rkbin(&soc.rkbin, &device.rkbin, device_name)?;
            for s in [&l.idbloader_offset, &l.uboot_itb_offset, &l.rootfs_offset] {
                crate::size::parse_size(s)?;
            }
            let (uboot_series, uboot_patches_url, uboot_patches_ref) =
                resolve_uboot_series(l, device, device_name, uboot_series_override)?;
            Ok(ResolvedBoot::RockchipRkbin(ResolvedRkbinBoot {
                uboot_defconfig,
                uboot_source: l.uboot_source.clone(),
                uboot_ref: l.uboot_ref.clone(),
                uboot_series,
                uboot_patches_url,
                uboot_patches_ref,
                rkbin,
                offsets: Offsets {
                    idbloader: l.idbloader_offset.clone(),
                    uboot_itb: l.uboot_itb_offset.clone(),
                    rootfs: l.rootfs_offset.clone(),
                },
            }))
        }
        BootMethodLayer::Depthcharge(l) => {
            // `split` exists to put the bootloader on a *different* medium from the
            // rootfs. Depthcharge has no bootloader of ours, and the firmware finds
            // the kernel partition by scanning the GPT of the same disk it will root
            // from — so there is nothing to split off.
            if layout == Layout::Split {
                return Err(ConfigError::UnsupportedLayout {
                    boot_method: BootMethod::Depthcharge.as_str(),
                    layout: layout.to_string(),
                    why: "the firmware finds the kernel partition by scanning the boot \
                          medium's own GPT, so there is no separate bootloader medium to emit",
                });
            }
            let dc = device
                .depthcharge
                .as_ref()
                .ok_or(ConfigError::MissingBootField {
                    device: device_name.to_string(),
                    boot_method: BootMethod::Depthcharge.as_str(),
                    what: "a [depthcharge] block (board, supported_boards)",
                })?;
            let board = board_override.unwrap_or(&dc.board).to_string();
            if !dc.supported_boards.contains(&board) {
                return Err(ConfigError::UnknownBoardProfile {
                    device: device_name.to_string(),
                    board,
                    supported: dc.supported_boards.join(", "),
                });
            }
            // Membership says the profile is one the device offers; this says the string
            // is one the rootfs stage can carry. Both are needed: `supported_boards` is
            // itself config, so a device file could offer a profile whose *name* is
            // hostile.
            validate_board_profile(&board)?;
            // The device's slot size wins over the method's when it states one: the
            // slot has to be at least as large as the payload the selected profile's
            // firmware will accept, and only the device knows which firmware it is
            // built for.
            let kpart_size = dc.kpart_size.as_ref().unwrap_or(&l.kpart_size);
            let kpart_offset_bytes = crate::size::parse_size(&l.kpart_offset)?;
            let kpart_size_bytes = crate::size::parse_size(kpart_size)?;
            // The signing cmdline is the boot method's, then the base console gate,
            // then the device's extra arguments; the merged value must pass the same
            // rules (a device-authored `root=` is caught by validate_kernel_cmdline,
            // but a `%` is only rejected here, where depthchargectl is in the path).
            // The device's arguments come last so a board can override the gate.
            let cmdline = if kernel_cmdline.is_empty() {
                format!("{} {CONSOLE_LOGLEVEL_ARG}", l.cmdline)
            } else {
                format!("{} {CONSOLE_LOGLEVEL_ARG} {kernel_cmdline}", l.cmdline)
            };
            validate_depthcharge_cmdline(&cmdline)?;
            if l.kpart_slots == 0 || l.kpart_slots > crate::chromeos::MAX_KPART_SLOTS {
                return Err(ConfigError::InvalidKpartSlots {
                    value: l.kpart_slots,
                    max: crate::chromeos::MAX_KPART_SLOTS,
                });
            }
            let flags =
                crate::chromeos::kpart_flags(l.kpart_priority, l.kpart_tries, l.kpart_successful)?;
            // The rootfs begins where the last slot ends. Derived rather than
            // authored so widening a slot moves the rootfs with it instead of
            // overlapping it; the image node re-checks alignment and fit against the
            // same value. Rendered back through [`size::format_size`] so it reads like
            // the authored offsets it sits beside rather than as a bare byte count.
            //
            // Checked arithmetic even though the slot count is already bounded above:
            // the offset and size are author-supplied u64s, and a wrap here would
            // place the rootfs *inside* the kernel slots.
            let rootfs_offset = kpart_size_bytes
                .checked_mul(u64::from(l.kpart_slots))
                .and_then(|slots| kpart_offset_bytes.checked_add(slots))
                .ok_or_else(|| ConfigError::KpartGeometryOverflow {
                    offset: l.kpart_offset.clone(),
                    size: kpart_size.clone(),
                    slots: l.kpart_slots,
                })?;
            Ok(ResolvedBoot::Depthcharge(ResolvedDepthchargeBoot {
                board,
                kpart: Kpart {
                    offset: l.kpart_offset.clone(),
                    size: kpart_size.clone(),
                    slots: l.kpart_slots,
                    priority: l.kpart_priority,
                    tries: l.kpart_tries,
                    successful: l.kpart_successful,
                    flags,
                },
                cmdline,
                rootfs_offset: crate::size::format_size(rootfs_offset),
                // The slot is the whole input: it is the budget the initramfs shares
                // with the kernel image, and a board with room to spare should not be
                // paying xz's decompression time at every boot for margin it does not
                // need.
                initramfs_compress: InitramfsCompress::for_kpart_size(kpart_size_bytes),
            }))
        }
    }
}

/// The resolved u-boot series plus its paired patches source and ref (series,
/// url, ref) — all `Some` together, or all `None` when u-boot ships pristine.
type UbootSeriesPins = (Option<String>, Option<String>, Option<String>);

/// Resolve the u-boot patch series for a `rockchip-rkbin` build: the CLI/recipe
/// override, else the device default, validated against the device's
/// `supported_uboot_series`. Returns the series plus its paired patches source
/// and ref (all `Some` together, or all `None` when u-boot ships pristine).
///
/// A board that declares no series ships pristine u-boot (`None`); a board that
/// declares series but no default, with none selected, is a config error. The
/// [`NO_PATCH_SERIES`](crate::series::NO_PATCH_SERIES) sentinel selects pristine
/// even on a board that lists series. A real series requires the boot method's
/// `patches_url`, mirroring a kernel definition's rule.
fn resolve_uboot_series(
    l: &RockchipRkbinLayer,
    device: &DeviceLayer,
    device_name: &str,
    override_series: Option<&str>,
) -> Result<UbootSeriesPins, ConfigError> {
    let selected = override_series
        .map(str::to_string)
        .or_else(|| device.default_uboot_series.clone());
    let Some(name) = selected else {
        if !device.supported_uboot_series.is_empty() {
            return Err(ConfigError::MissingDefaultUbootSeries {
                device: device_name.to_string(),
            });
        }
        return Ok((None, None, None));
    };
    // The pristine sentinel is always accepted; a named series must be one the
    // device declares.
    if crate::series::patch_series(&name).is_some()
        && !device.supported_uboot_series.contains(&name)
    {
        return Err(ConfigError::UnknownUbootSeriesForDevice {
            device: device_name.to_string(),
            series: name,
            supported: device.supported_uboot_series.join(", "),
        });
    }
    let Some(series) = crate::series::patch_series(&name).map(str::to_string) else {
        return Ok((None, None, None));
    };
    // A series must name the repo it comes from: the lock records the source beside
    // the commit, and a commit id means nothing without one.
    let Some(url) = l.patches_url.clone() else {
        return Err(ConfigError::MissingUbootPatchesUrl {
            device: device_name.to_string(),
            series,
        });
    };
    let patches_ref = l
        .patches_ref
        .clone()
        .unwrap_or_else(|| crate::model::DEFAULT_PATCHES_REF.to_string());
    Ok((Some(series), Some(url), Some(patches_ref)))
}

/// Validate and normalize a device's extra kernel command-line arguments
/// ([`DeviceLayer::kernel_cmdline`]): trimmed, or empty when absent.
///
/// The value is embedded verbatim inside a double-quoted assignment in
/// `/etc/boot2deb/board.conf`, a file **sourced by shell scripts** on the device
/// (`mk_extlinux`, the kernel postinst hooks) — so any character the shell would
/// interpret inside double quotes is rejected rather than escaped: `"`, `\`, `$`,
/// backticks, and any control character (newlines would end the assignment).
/// `root=` is rejected on every boot path for the same reason as depthcharge's
/// rule: the device derives root from `/etc/fstab`, and an authored `root=` is a
/// second source of truth that silently wins or loses depending on argument order.
fn validate_kernel_cmdline(cmdline: Option<&str>) -> Result<String, ConfigError> {
    let Some(raw) = cmdline else {
        return Ok(String::new());
    };
    let value = raw.trim();
    let bad = |why| {
        Err(ConfigError::InvalidCmdline {
            value: value.to_string(),
            why,
        })
    };
    if value.chars().any(|c| ['"', '\\', '$', '`'].contains(&c)) {
        return bad(
            "it contains a shell-active character (one of \" \\ $ `); the value is embedded \
             in a double-quoted assignment in /etc/boot2deb/board.conf, which device scripts \
             source — plain `key=value` arguments only",
        );
    }
    if value.chars().any(|c| c.is_control()) {
        return bad(
            "it contains a control character; the value must be a single line of \
             space-separated kernel arguments",
        );
    }
    if value.split_whitespace().any(|tok| tok.starts_with("root=")) {
        return bad(
            "it sets `root=`, which the device derives from its /etc/fstab on every boot \
             path — remove it and let fstab be the single source",
        );
    }
    Ok(value.to_string())
}

/// Reject a depthcharge cmdline that `depthchargectl` cannot carry, or that claims
/// something it is not ours to claim.
///
/// Two rules, each learned from a boot that failed:
///  - **No `%`.** `depthchargectl` writes its computed cmdline back through a
///    `ConfigParser` whose interpolation rejects a raw `%` — it is a hard error, and
///    no escaping works (`%%U` is un-escaped on read and rejected on write). The
///    `kern_guid=%U` the firmware substitutes is prepended later, by `mkdepthcharge`,
///    past that round-trip.
///  - **No `root=`.** `depthchargectl` derives root from `/etc/fstab` and *strips* any
///    `root=` that disagrees with it — here and again on every on-device kernel
///    upgrade. Authoring one would be a value that silently does not survive.
fn validate_depthcharge_cmdline(cmdline: &str) -> Result<(), ConfigError> {
    let bad = |why| {
        Err(ConfigError::InvalidCmdline {
            value: cmdline.to_string(),
            why,
        })
    };
    if cmdline.contains('%') {
        return bad(
            "it contains a '%', which depthchargectl's config round-trip rejects outright \
             (no escaping works); the kern_guid=%U substitution is added by mkdepthcharge, \
             past that round-trip",
        );
    }
    if cmdline
        .split_whitespace()
        .any(|tok| tok.starts_with("root="))
    {
        return bad(
            "it sets `root=`, which depthchargectl derives from the image's /etc/fstab and \
             strips when it disagrees — remove it and let fstab be the single source",
        );
    }
    Ok(())
}

/// Resolve the kernel axis, and reject the inputs a distro-package kernel could
/// never act on.
///
/// A distro kernel compiles nothing, so a `device_dts`, `device_config_fragments`,
/// `device_patch_series`, or `device_kmods` on such a build is not merely redundant —
/// it is a board whose device tree will never be compiled, whose kconfig will never be
/// merged, and whose out-of-tree modules have no tree to build against. That reads as
/// configured and boots as broken, so it is a typed error instead. A selected
/// feature's kernel contributions are rejected for the same reason, under
/// [`ConfigError::FeatureNeedsCompiledKernel`].
///
/// `features` is the validated selection, in recipe order; its kconfig fragments
/// and patch series compose last, after the kernel's and the device's.
fn resolve_kernel(
    kdef: KernelDef,
    kernel_id: String,
    device: &DeviceLayer,
    device_name: &str,
    features: &[(String, crate::feature::Feature)],
) -> Result<ResolvedKernel, ConfigError> {
    let (feature_fragments, feature_series) = crate::feature::kernel_contributions(features);
    match kdef {
        KernelDef::Compiled(k) => {
            // Apply order, narrowest last: kernel-owned fragments, then the device's,
            // then the selected features'. A feature is the opt-in, so it gets the
            // final say on a symbol the layers below it also set.
            let mut config_fragments = k.config_fragments;
            config_fragments.extend(device.device_config_fragments.iter().cloned());
            config_fragments.extend(feature_fragments);
            // The resolved list is the kernel's series, then the device's, then the
            // features', in that order: an empty list is a kernel that applies no
            // series, and nothing downstream reads the `patches` repo for it. Composing
            // them (a SoC-wide fix series from the kernel, an out-of-tree driver
            // series from the board or from a capability feature) all ride the one
            // `patches_url` checkout below — the patch-series analogue of the
            // `config_fragments` merge just above.
            let mut patch_series = k.patch_series;
            patch_series.extend(device.device_patch_series.iter().cloned());
            patch_series.extend(feature_series);
            // Named series must name the repo they come from: the lock records the
            // source beside the commit, and a commit id means nothing without one.
            // Caught here rather than at pin time, so the config is wrong at `resolve`
            // instead of surfacing much later in `update`.
            if !patch_series.is_empty() && k.patches_url.is_none() {
                return Err(ConfigError::MissingPatchesUrl {
                    kernel: kernel_id,
                    series: patch_series.join(", "),
                });
            }
            // Paired with the series so the two are present together — nothing
            // downstream has to consider a source without a series.
            let patches_ref = (!patch_series.is_empty()).then(|| {
                k.patches_ref
                    .clone()
                    .unwrap_or_else(|| crate::model::DEFAULT_PATCHES_REF.to_string())
            });
            Ok(ResolvedKernel::Compiled(ResolvedCompiledKernel {
                id: kernel_id,
                flavor: k.flavor,
                source: k.source,
                track: k.track,
                base_defconfig: k.base_defconfig,
                patch_series,
                patches_url: k.patches_url,
                patches_ref,
                config_fragments,
                libre: k.libre,
            }))
        }
        KernelDef::Distro(k) => {
            // A feature's kernel contributions fail the same way the device's do, but
            // name the feature: the fix is to drop the capability from the recipe or
            // move to a compiled kernel, neither of which is a device-layer edit.
            if let Some((feature, what)) = crate::feature::first_contributing_kernel_input(features)
            {
                return Err(ConfigError::FeatureNeedsCompiledKernel {
                    feature: feature.to_string(),
                    kernel: kernel_id,
                    what,
                });
            }
            for (what, declared) in [
                ("device_dts", !device.device_dts.is_empty()),
                (
                    "device_config_fragments",
                    !device.device_config_fragments.is_empty(),
                ),
                (
                    "device_patch_series",
                    !device.device_patch_series.is_empty(),
                ),
                ("device_kmods", !device.device_kmods.is_empty()),
            ] {
                if declared {
                    return Err(ConfigError::DistroKernelCompilesNothing {
                        device: device_name.to_string(),
                        kernel: kernel_id,
                        what,
                    });
                }
            }
            Ok(ResolvedKernel::Distro(ResolvedDistroKernel {
                id: kernel_id,
                package: k.package,
            }))
        }
    }
}

/// Validate a device's loose device-tree sources against its `kernel_dtb`.
///
/// Two checks, both cheap and both fatal before any build work:
///  - **Shape**: every entry is a relative, `..`-free path to a `.dts` or `.dtsi`.
///    The engine joins these onto the config-root search path and copies the result
///    into the kernel tree, so an escaping path would smuggle in a foreign file.
///  - **Correspondence**: `kernel_dtb`'s basename is produced by one of the listed
///    `.dts` sources (`rockchip/board.dtb` ← `.../board.dts`). Without this a typo
///    yields a kernel that builds and then boots to a missing DTB.
///
/// An empty `device_dts` is the upstream-DTB case and imposes no constraint: the
/// kernel's own tree builds the board's DTB.
fn validate_device_dts(
    device_dts: &[String],
    kernel_dtb: &str,
    device_name: &str,
) -> Result<(), ConfigError> {
    let invalid = |path: &str, why| ConfigError::InvalidDeviceDts {
        device: device_name.to_string(),
        path: path.to_string(),
        why,
    };
    for entry in device_dts {
        let path = std::path::Path::new(entry);
        if entry.trim().is_empty() {
            return Err(invalid(entry, "the entry is empty"));
        }
        if path.is_absolute() {
            return Err(invalid(entry, "the path is absolute"));
        }
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err(invalid(entry, "the path escapes the config root via '..'"));
        }
        if !matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("dts" | "dtsi")
        ) {
            return Err(invalid(entry, "the file is not a .dts or .dtsi"));
        }
    }
    if device_dts.is_empty() {
        return Ok(());
    }
    // `kernel_dtb` is DT-output-dir-relative (`rockchip/board.dtb`); only its
    // basename can match a source file, whose own directory is a config-root layout
    // choice unrelated to the in-tree DT dir.
    let stem = std::path::Path::new(kernel_dtb)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default();
    let expected = format!("{stem}.dts");
    let built = device_dts
        .iter()
        .filter_map(|e| std::path::Path::new(e).file_name()?.to_str())
        .any(|name| name == expected);
    if !built {
        return Err(ConfigError::KernelDtbNotInDeviceDts {
            device: device_name.to_string(),
            kernel_dtb: kernel_dtb.to_string(),
            sources: device_dts.join(", "),
            expected,
        });
    }
    Ok(())
}

/// Load and validate each kmod a device named, in declared order.
///
/// The name list is the device's; everything else comes from `kmods/<name>.toml`, so a
/// second board carrying the same chip names it instead of copying its declaration. A
/// repeat in the list is rejected: one kmod is one build node, one deb, and one lock pin.
///
/// An empty list imposes no constraint — the board carries no out-of-tree module.
fn resolve_kmods(
    root: &ConfigRoot,
    names: &[String],
    device_name: &str,
) -> Result<Vec<ResolvedKmod>, ConfigError> {
    let mut seen = std::collections::BTreeSet::new();
    let mut resolved = Vec::with_capacity(names.len());
    for name in names {
        if !seen.insert(name.as_str()) {
            return Err(ConfigError::DuplicateKmod {
                device: device_name.to_string(),
                kmod: name.clone(),
            });
        }
        let k = root.kmod(name)?;
        validate_kmod(&k, name)?;
        resolved.push(ResolvedKmod {
            name: name.clone(),
            description: k.description,
            git: k.git,
            git_ref: k.git_ref,
            subdir: k.subdir,
            patch_dir: k.patch_dir,
            repo_patches: k.repo_patches,
            local_patches: k.local_patches,
            make_args: k.make_args,
            modules: k.modules,
            firmware: k.firmware,
        });
    }
    Ok(resolved)
}

/// Validate one out-of-tree module declaration (`kmods/<name>.toml`).
///
/// Every field must be safe to feed the build without escaping the trees it is joined
/// onto, and the kmod must name a well-formed package:
///  - **name**: dpkg-package-safe — it becomes `<name>-modules-<kver>` and the
///    `kmod:<name>` build/cache node.
///  - **git / ref**: non-empty (the exact commit is pinned in the lock).
///  - **subdir / patch_dir**: relative and `..`-free (the subdir feeds `make M=`).
///  - **repo_patches**: bare filenames under `patch_dir` (joined as `patch_dir/<file>`).
///  - **local_patches**: bare filenames under `kmods/<name>/patches/` (read along the
///    overlay search path like a fragment).
///  - **make_args**: a bare `KEY=VALUE` — no leading `-`, whitespace, or shell
///    metacharacters. The entry becomes one `make` argv word, and make reads a leading
///    `-` as an option (`-C`, `-f` would redirect the build at another tree).
///  - **modules**: bare `.ko` basenames.
fn validate_kmod(k: &KmodLayer, name: &str) -> Result<(), ConfigError> {
    let invalid = |why| ConfigError::InvalidKmod {
        kmod: name.to_string(),
        why,
    };
    // Relative, non-empty, `..`-free.
    let contained = |p: &str| -> Result<(), &'static str> {
        if p.trim().is_empty() {
            return Err("a path is empty");
        }
        let path = std::path::Path::new(p);
        if path.is_absolute() {
            return Err("a path is absolute");
        }
        if path
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            return Err("a path escapes the tree via '..'");
        }
        Ok(())
    };
    // A bare filename: non-empty, no separator, not a directory reference.
    let bare = |p: &str| -> Result<(), &'static str> {
        if p.trim().is_empty() {
            return Err("an entry is empty");
        }
        if p.contains('/') {
            return Err("an entry has a path separator (expected a bare filename)");
        }
        if p == "." || p == ".." {
            return Err("an entry is a directory reference");
        }
        Ok(())
    };
    let name_ok = name
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, '-' | '+' | '.'));
    if !name_ok {
        return Err(invalid(
            "the name is not dpkg-package-safe (lowercase alphanumeric plus '-', '+', '.', starting alphanumeric)",
        ));
    }
    if k.git.trim().is_empty() {
        return Err(invalid("the git url is empty"));
    }
    if k.git_ref.trim().is_empty() {
        return Err(invalid("the git ref is empty"));
    }
    contained(&k.subdir).map_err(invalid)?;
    contained(&k.patch_dir).map_err(invalid)?;
    for p in &k.repo_patches {
        bare(p).map_err(invalid)?;
    }
    // Bare like the repo's own: a local patch is looked up at `kmods/<name>/patches/<p>`,
    // so a separator would reach outside the kmod's own directory.
    for lp in &k.local_patches {
        bare(lp).map_err(invalid)?;
    }
    // Each entry goes into `make`'s argv verbatim, so it must read as a variable
    // assignment and nothing else. A leading `-` is refused for the same reason a
    // defconfig name is (see `reject_unsafe_make_target` in the engine): make parses
    // it as an option, and `-C`/`-f` redirect the build at another tree or makefile.
    // The shell metacharacters are refused because the value is also folded into
    // shell-quoted build scripts.
    for a in &k.make_args {
        let unsafe_arg = !a.contains('=')
            || a.starts_with('-')
            || a.chars().any(|c| {
                c.is_whitespace()
                    || matches!(
                        c,
                        ';' | '&' | '|' | '$' | '`' | '<' | '>' | '(' | ')' | '\'' | '"' | '\\'
                    )
            });
        if unsafe_arg {
            return Err(invalid(
                "a make_args entry is not a bare KEY=VALUE (no leading '-', whitespace, or shell metacharacters)",
            ));
        }
    }
    for m in &k.modules {
        bare(m).map_err(invalid)?;
    }
    if let Some(fw) = &k.firmware {
        // Both paths are joined under a build/rootfs root, so both must be relative
        // and `..`-free — the source under the fetched repo, the install under the deb.
        contained(&fw.subdir).map_err(invalid)?;
        contained(&fw.install).map_err(invalid)?;
    }
    Ok(())
}

/// Merge the SoC-layer rkbin defaults with the device overrides (device wins per
/// field) and validate the result: `atf` and `tpl` are required (a missing or
/// blank one is a [`ConfigError::MissingBlob`]), `bl32` stays optional.
fn resolve_rkbin(
    soc: &RkbinLayer,
    device: &RkbinLayer,
    device_name: &str,
) -> Result<Rkbin, ConfigError> {
    // A blank string counts as unset (filtered per side), so an empty device
    // override never masks a good SoC default; then device wins over SoC.
    let clean = |o: &Option<String>| o.clone().filter(|v| !v.trim().is_empty());
    let pick = |dev: &Option<String>, soc: &Option<String>| clean(dev).or_else(|| clean(soc));
    let require = |v: Option<String>, what: &str| {
        v.ok_or_else(|| ConfigError::MissingBlob {
            device: device_name.to_string(),
            what: what.into(),
        })
    };
    Ok(Rkbin {
        atf: require(pick(&device.atf, &soc.atf), "rkbin.atf")?,
        tpl: require(pick(&device.tpl, &soc.tpl), "rkbin.tpl")?,
        bl32: pick(&device.bl32, &soc.bl32),
    })
}

/// Union the pre-built [`ExtraDeb`]s a build's layers and features pull from
/// outside the Debian mirror, keyed by sha256 — the content identity.
///
/// Two layers/features pulling byte-identical bytes (same sha256) collapse to one
/// entry, even if their locators differ, since moving identical bytes is not a new
/// deb. Each entry is validated (exactly one locator, lowercase-hex hash) as
/// it is seen, so a malformed pin fails at resolve. Order follows first appearance
/// across base → soc → boot-method → device → features.
fn merge_extra_debs(
    base: &BaseLayer,
    soc: &SocLayer,
    bm: &BootMethodLayer,
    device: &DeviceLayer,
    features: &[(String, crate::feature::Feature)],
) -> Result<Vec<ExtraDeb>, ConfigError> {
    let mut merged: Vec<ExtraDeb> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut absorb = |set: &[ExtraDeb]| -> Result<(), ConfigError> {
        for d in set {
            d.validate()?;
            if seen.insert(d.sha256.clone()) {
                merged.push(d.clone());
            }
        }
        Ok(())
    };
    absorb(&base.extra_debs)?;
    absorb(&soc.extra_debs)?;
    absorb(bm.extra_debs())?;
    absorb(&device.extra_debs)?;
    for (_, feat) in features {
        absorb(&feat.extra_debs)?;
    }
    Ok(merged)
}

/// One field of the apt one-line source format: non-empty printable ASCII with
/// no whitespace and no `[`/`]` — whitespace separates the line's positional
/// fields and the brackets delimit its option block, so either would be parsed
/// as structure, not content.
fn apt_line_token(v: &str) -> bool {
    !v.is_empty()
        && v.chars()
            .all(|c| c.is_ascii_graphic() && c != '[' && c != ']')
}

/// A portable file-name stem: non-empty, drawn from `[A-Za-z0-9._-]`, and not a
/// directory reference. The character set excludes every path separator, so a value
/// that passes is a *basename* — it names a file inside whichever directory the
/// consumer joins it onto and cannot climb out of one.
fn apt_file_stem(v: &str) -> bool {
    !v.is_empty()
        && v != "."
        && v != ".."
        && v.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
}

/// Validate a feature's [`AptSource`] against the one-line source grammar the
/// bootstrap renders it into: every field a clean token, the URI http(s) (any other
/// transport would sidestep the mirror trust model), and at least one component
/// unless the suite is an exact path (ends in `/`).
///
/// `name` and `signed_by` are held to the stricter [`apt_file_stem`] instead,
/// because each becomes a *file name* rather than a line field:
///
///  - `name` is the dedup key here and the `sources.list.d/<name>.list` and
///    `<name>.gpg` stem in the finished rootfs. Holding it to the portable stem set
///    is what makes the dedup key and the file name the same string: any looser set
///    would need a sanitizing map on the way to disk, and a map that is not injective
///    lets two names this function accepted as distinct land on one file.
///  - `signed_by` is joined onto the vendored keyring directory to find the repo's
///    trust anchor, so a value carrying `/` or `..` would choose a key from outside
///    the set a reviewer vetted.
fn validate_apt_source(feature: &str, src: &AptSource) -> Result<(), ConfigError> {
    let bad = |field: &'static str, value: &str| ConfigError::AptSourceBadField {
        feature: feature.to_string(),
        name: src.name.clone(),
        field,
        value: value.to_string(),
    };
    if !apt_file_stem(&src.name) {
        return Err(bad("name", &src.name));
    }
    if !apt_line_token(&src.uri)
        || !(src.uri.starts_with("https://") || src.uri.starts_with("http://"))
    {
        return Err(bad("uri", &src.uri));
    }
    if !apt_line_token(&src.suite) {
        return Err(bad("suite", &src.suite));
    }
    if src.components.is_empty() && !src.suite.ends_with('/') {
        return Err(bad("components", "(empty)"));
    }
    for component in &src.components {
        if !apt_line_token(component) {
            return Err(bad("components", component));
        }
    }
    if !apt_file_stem(&src.signed_by) {
        return Err(bad("signed_by", &src.signed_by));
    }
    Ok(())
}

/// Union the selected features' [`AptSource`]s, keyed by `name`. Each source is
/// validated against the apt line grammar first ([`validate_apt_source`]).
/// Two features may legitimately reference the same repo — those
/// collapse to one entry — but a same-name pair with different settings is
/// [`ConfigError::ConflictingAptSource`], since the bootstrap solve could not tell
/// which repo to activate. Order follows first appearance across the feature list.
///
/// Keying on the raw `name` is sound because validation already holds it to the
/// portable file-name stem the rootfs writes it as: the dedup key here and the
/// `sources.list.d` entry there are the same string, so two names that merge into one
/// file cannot pass as distinct repositories.
fn merge_apt_sources(
    features: &[(String, crate::feature::Feature)],
) -> Result<Vec<AptSource>, ConfigError> {
    let mut merged: Vec<(String, AptSource)> = Vec::new();
    for (feat_name, feat) in features {
        for src in &feat.apt_sources {
            validate_apt_source(feat_name, src)?;
            if let Some((owner, existing)) = merged.iter().find(|(_, s)| s.name == src.name) {
                if existing != src {
                    return Err(ConfigError::ConflictingAptSource {
                        feature: owner.clone(),
                        other: feat_name.clone(),
                        name: src.name.clone(),
                    });
                }
                // Identical duplicate — already present, skip.
            } else {
                merged.push((feat_name.clone(), src.clone()));
            }
        }
    }
    Ok(merged.into_iter().map(|(_, s)| s).collect())
}

/// Resolve a [build reference](crate::buildpoint::BuildPoint::reference): recipe
/// fields are the base axes; `cli` overrides win.
///
/// The reference is a recipe name, optionally carrying a `+`-separated feature
/// suffix. A suffix replaces the recipe's own `features` list the same way
/// `--feature` does — so `turing-rk1/forky+jellyfin` resolves the `turing-rk1/forky`
/// axes with `jellyfin` as the feature selection. An explicit `cli.features` still
/// wins over both, so a caller that already parsed the reference into a
/// [`BuildPoint`](crate::buildpoint::BuildPoint) and passed its selection through
/// `cli` gets the same answer.
pub fn resolve_recipe(
    root: &ConfigRoot,
    reference: &str,
    cli: &Overrides,
) -> Result<ResolvedBuild, ConfigError> {
    let point = crate::buildpoint::BuildPoint::parse(reference)?;
    let recipe = root.recipe(point.recipe())?;
    let merged = Overrides {
        deliverable: recipe.deliverable,
        kernel: cli.kernel.clone().or(recipe.kernel),
        uboot_series: cli.uboot_series.clone().or(recipe.uboot_series),
        suite: cli.suite.clone().or(recipe.suite),
        layout: cli.layout.or(recipe.layout),
        boot_method: cli.boot_method,
        board: cli.board.clone().or(recipe.board),
        features: cli
            .features
            .clone()
            .or_else(|| point.feature_override())
            .or_else(|| (!recipe.features.is_empty()).then_some(recipe.features)),
        image_size: cli.image_size.clone().or(recipe.image_size),
        locale: cli.locale.clone().or(recipe.locale),
        locales_generate: cli.locales_generate.clone().or(recipe.locales_generate),
        timezone: cli.timezone.clone().or(recipe.timezone),
        ntp_servers: cli.ntp_servers.clone().or(recipe.ntp_servers),
        keymap: cli.keymap.clone().or(recipe.keymap),
        sudo: cli.sudo.or(recipe.sudo),
        first_boot_password_length: cli
            .first_boot_password_length
            .or(recipe.first_boot_password_length),
        ssh_authorized_keys: cli
            .ssh_authorized_keys
            .clone()
            .or(recipe.ssh_authorized_keys),
    };
    let mut build = resolve_device(root, &recipe.device, &merged)?;
    // Data volumes are recipe-only: no CLI flag and no device default, because
    // whether a board's second slot is populated is a property of the deployment
    // this recipe describes. Validated here rather than in `resolve_device` since
    // the errors name the recipe.
    crate::datavolume::validate_all(&recipe.data_volumes)?;
    let has_feature = build
        .features
        .iter()
        .any(|f| f == crate::datavolume::FEATURE);
    match (has_feature, recipe.data_volumes.is_empty()) {
        // The hook without anything to act on, or declarations with nothing to act
        // on them. Either half alone is inert, and silently ignoring it would give a
        // board that comes up without the disk the recipe says it has.
        (true, true) => {
            return Err(ConfigError::DataVolumeFeatureMismatch {
                recipe: reference.to_string(),
                problem: format!(
                    "selects the '{}' feature but declares no [[data_volumes]]",
                    crate::datavolume::FEATURE
                ),
            })
        }
        (false, false) => {
            return Err(ConfigError::DataVolumeFeatureMismatch {
                recipe: reference.to_string(),
                problem: format!(
                    "declares [[data_volumes]] but does not select the '{}' feature",
                    crate::datavolume::FEATURE
                ),
            })
        }
        _ => {}
    }
    build.data_volumes = recipe.data_volumes;
    // The build point's own caveats, after the hardware's: a recipe states what *this
    // point* does not do, which is the narrowest scope and so the last a reader meets.
    // De-duplicated against what the layers already said, so a recipe restating an
    // inherited limitation does not print it twice.
    for text in recipe.support.iter().flat_map(|s| &s.caveats) {
        if !build.caveats.iter().any(|c| &c.text == text) {
            build.caveats.push(Caveat {
                text: text.clone(),
                scope: CaveatScope::Recipe,
            });
        }
    }
    Ok(build)
}

/// The hardware half of a build point's caveats: the SoC's then the board's, each
/// tagged with the layer that stated it, de-duplicated by text in first-appearance
/// order.
///
/// Silicon first because it constrains the most: a reader meets the limitation that
/// holds for every board on the part before the one that holds for this board alone.
/// First-appearance de-duplication is what makes that ordering matter — a board
/// restating a SoC caveat keeps the SoC's tag, which is the wider and so the truer
/// of the two.
/// `hardware` extended with each selected feature's caveats, tagged
/// [`CaveatScope::Feature`] and de-duplicated by text against what is already there.
///
/// Between the hardware's and the recipe's because that is where a capability sits:
/// its limits hold only where it is selected, so they are narrower than the
/// silicon's, and they hold in every recipe that selects it, so they are wider than
/// a recipe's. Features keep the recipe's order, so a reader meets them in the order
/// the recipe names them. De-duplication keeps the wider tag, as it does for the
/// hardware pair — a feature restating a SoC limitation stays the SoC's.
fn feature_caveats(
    mut hardware: Vec<Caveat>,
    features: &[(String, crate::Feature)],
) -> Vec<Caveat> {
    for (_, feat) in features {
        for text in &feat.caveats {
            if !hardware.iter().any(|c| &c.text == text) {
                hardware.push(Caveat {
                    text: text.clone(),
                    scope: CaveatScope::Feature,
                });
            }
        }
    }
    hardware
}

fn hardware_caveats(soc: &[String], device: &[String]) -> Vec<Caveat> {
    let mut out: Vec<Caveat> = Vec::new();
    let tagged = soc
        .iter()
        .map(|c| (c, CaveatScope::Soc))
        .chain(device.iter().map(|c| (c, CaveatScope::Device)));
    for (text, scope) in tagged {
        if !out.iter().any(|c| &c.text == text) {
            out.push(Caveat {
                text: text.clone(),
                scope,
            });
        }
    }
    out
}

/// Append `src` package names to `acc`, skipping any already present, so the
/// merged rootfs set is order-preserving and de-duplicated.
fn extend_unique(
    acc: &mut Vec<String>,
    seen: &mut std::collections::HashSet<String>,
    src: &[String],
) {
    for pkg in src {
        if seen.insert(pkg.clone()) {
            acc.push(pkg.clone());
        }
    }
}

fn join<T: std::fmt::Display>(items: &[T]) -> String {
    items
        .iter()
        .map(|i| i.to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

/// Reject any override that names an axis a u-boot-only deliverable does not have.
///
/// The bootloader is the whole artifact: no kernel is compiled, no suite is
/// bootstrapped, no image is assembled. So the kernel, rootfs, and image axes have
/// nothing to act on, and accepting one would be indistinguishable from acting on it.
/// The axes that *do* survive — `layout`, `boot_method`, `uboot_series` — reach
/// [`resolve_boot`] and are validated there like any other build's.
///
/// Checked in flag order so the first-reported error is stable.
fn reject_rootfs_overrides(device_name: &str, overrides: &Overrides) -> Result<(), ConfigError> {
    let inapplicable: [(&'static str, bool); 12] = [
        ("--kernel", overrides.kernel.is_some()),
        ("--suite", overrides.suite.is_some()),
        ("--feature", overrides.features.is_some()),
        ("--image-size", overrides.image_size.is_some()),
        ("--locale", overrides.locale.is_some()),
        ("--locale-gen", overrides.locales_generate.is_some()),
        ("--timezone", overrides.timezone.is_some()),
        ("--ntp-server", overrides.ntp_servers.is_some()),
        ("--keymap", overrides.keymap.is_some()),
        ("--sudo", overrides.sudo.is_some()),
        (
            "--password-length",
            overrides.first_boot_password_length.is_some(),
        ),
        // No flag sets this one, so it is named as the recipe key that does.
        (
            "ssh_authorized_keys",
            overrides.ssh_authorized_keys.is_some(),
        ),
    ];
    match inapplicable.iter().find(|(_, set)| *set) {
        Some((flag, _)) => Err(ConfigError::OverrideNotApplicable {
            device: device_name.to_string(),
            flag,
        }),
        None => Ok(()),
    }
}

/// Reject a suite that is not a well-formed Debian codename. The suite becomes an apt
/// `sources.list` entry and the archive path the bootstrap fetches under, so it must
/// be a bare token starting with an alphanumeric and drawn from `[A-Za-z0-9._-]`.
///
/// Shape only. *Which* pockets a suite publishes is a separate question, answered by
/// [`suite::pockets`](crate::suite::pockets) where the sources file is generated —
/// `sid` is a valid, documented value that simply has no `-security` or `-updates`.
///
/// Pure, so it is unit-testable.
fn validate_suite(suite: &str) -> Result<(), ConfigError> {
    let mut chars = suite.chars();
    let ok = matches!(chars.next(), Some(c) if c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(ConfigError::InvalidSuite {
            value: suite.to_string(),
        })
    }
}

/// Check a well-formed suite against the device's declared
/// [`supported_suites`](crate::model::DeviceLayer::supported_suites).
///
/// The list is validated on the way through rather than at load time, because a
/// contradictory list only matters for a build that resolves a suite: a
/// `deliverable = "uboot"` recipe never reaches here, and refusing to *load* the
/// device would take its bootloader recipes down with it.
///
/// Pure, so it is unit-testable.
fn check_suite_supported(
    device_name: &str,
    supported: &[String],
    suite: &str,
) -> Result<(), ConfigError> {
    let wildcard = supported.iter().any(|s| s == ANY_SUITE);
    if supported.is_empty() {
        return Err(ConfigError::NoSupportedSuites {
            device: device_name.to_string(),
        });
    }
    if wildcard && supported.len() > 1 {
        return Err(ConfigError::SuiteWildcardMixed {
            device: device_name.to_string(),
            supported: supported.join(", "),
        });
    }
    if wildcard || supported.iter().any(|s| s == suite) {
        Ok(())
    } else {
        Err(ConfigError::UnsupportedSuite {
            device: device_name.to_string(),
            suite: suite.to_string(),
            supported: supported.join(", "),
        })
    }
}

/// Resolve the localization axis: the system locale, the locales generated into the
/// image, the timezone, and the console keymap.
///
/// The three system-wide values default at the **base** layer (they are distro policy,
/// not hardware) while the keymap defaults at the **device** layer (whether a console
/// keymap means anything is a property of the board); a recipe or CLI flag overrides
/// any of them.
///
/// The one invariant worth stating: the resolved `locale` is *always* generated. It
/// leads the generated set unconditionally, so `LANG` can never name a locale the
/// image lacks — the failure that makes a shell print `Setting locale failed` on every
/// login.
///
/// It leads the set even when glibc would carry it anyway (`C.UTF-8` is built into
/// `libc-bin` and needs no `locale-gen` line to *work*). That is not redundant: the
/// `locales` package builds the choice list `dpkg-reconfigure locales` offers for the
/// default locale out of `/etc/locale.gen`, so a system locale missing from that file
/// is a system locale the user cannot see or re-select on the running board.
fn resolve_l10n(
    base: &BaseLayer,
    device: &DeviceLayer,
    overrides: &Overrides,
) -> Result<(String, Vec<String>, String, Option<Keymap>), ConfigError> {
    let locale = overrides
        .locale
        .clone()
        .unwrap_or_else(|| base.locale.clone());
    validate_locale(&locale)?;

    let extras = overrides
        .locales_generate
        .clone()
        .unwrap_or_else(|| base.locales_generate.clone());

    // The system locale leads the generated set, then the configured extras.
    let mut locales_generate = vec![locale.clone()];
    let mut seen = std::collections::HashSet::from([locale.clone()]);
    for extra in &extras {
        validate_locale(extra)?;
        if seen.insert(extra.clone()) {
            locales_generate.push(extra.clone());
        }
    }

    let timezone = overrides
        .timezone
        .clone()
        .unwrap_or_else(|| base.timezone.clone());
    validate_timezone(&timezone)?;

    let keymap = overrides.keymap.clone().or_else(|| device.keymap.clone());
    if let Some(k) = &keymap {
        validate_keymap(k)?;
    }

    Ok((locale, locales_generate, timezone, keymap))
}

/// The resolved account axis: what [`resolve_account`] returns.
///
/// A struct rather than a tuple because all three parts are about one thing — how the
/// finished image is reached — and a caller destructuring three same-shaped values
/// positionally is a swap waiting to happen.
struct Account {
    /// What `sudo` asks of the default account.
    sudo: SudoPolicy,
    /// Generated first-boot password length, validated in range.
    password_length: u8,
    /// Authorized `authorized_keys` lines, each validated, in config order.
    authorized_keys: Vec<String>,
}

/// Resolve the account axis: the sudo policy, the generated first-boot password length,
/// and the SSH keys authorized for the default account.
///
/// All three default at the **base** layer — they are distro/security policy, not
/// properties of a board, and a board has no opinion about who its operator is. A
/// recipe or CLI flag overrides any of them.
///
/// Every part is validated here rather than at the point of use, because each failure
/// is silent on the finished image: a malformed key is one `sshd` skips into its own
/// log, and a too-short password looks exactly like a long one from the outside. The
/// keys keep config order — `authorized_keys` is a list `sshd` walks, and preserving
/// the authored order keeps the file a readable statement of who was granted access.
fn resolve_account(base: &BaseLayer, overrides: &Overrides) -> Result<Account, ConfigError> {
    let sudo = overrides.sudo.unwrap_or(base.sudo);

    let password_length = overrides
        .first_boot_password_length
        .unwrap_or(base.first_boot_password_length);
    if !(crate::model::MIN_PASSWORD_LENGTH..=crate::model::MAX_PASSWORD_LENGTH)
        .contains(&password_length)
    {
        return Err(ConfigError::InvalidPasswordLength {
            value: password_length as u32,
            min: crate::model::MIN_PASSWORD_LENGTH as u32,
            max: crate::model::MAX_PASSWORD_LENGTH as u32,
            default: crate::model::DEFAULT_PASSWORD_LENGTH as u32,
        });
    }

    let authorized_keys = overrides
        .ssh_authorized_keys
        .clone()
        .unwrap_or_else(|| base.ssh_authorized_keys.clone());
    for (i, key) in authorized_keys.iter().enumerate() {
        if let Err(why) = crate::authkeys::check_authorized_key(key) {
            return Err(ConfigError::InvalidAuthorizedKey {
                // 1-based: the entry's only name is where it sits in the authored list,
                // and that list reads as a first, second, third in the file.
                index: i + 1,
                value: key.clone(),
                why,
            });
        }
    }

    Ok(Account {
        sudo,
        password_length,
        authorized_keys,
    })
}

/// Reject a locale `locale-gen` could not act on, or that would not survive the two
/// files it lands in.
///
/// The name becomes a `LANG=` value in `/etc/locale.conf` (shell-sourced by `pam_env`)
/// and the left half of an `/etc/locale.gen` line, so it must be a bare locale name —
/// and it must carry a codeset, since `locale-gen` is given `<name> <codeset>` pairs
/// and there is nowhere else for that half to come from. This is a UTF-8-era
/// constraint on the *build-time* knob only: a legacy 8-bit locale is still one
/// `dpkg-reconfigure locales` away on the running image.
fn validate_locale(locale: &str) -> Result<(), ConfigError> {
    if locale.is_empty() {
        return Err(ConfigError::InvalidLocale {
            value: locale.to_string(),
            why: "empty",
        });
    }
    if !locale
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@'))
    {
        return Err(ConfigError::InvalidLocale {
            value: locale.to_string(),
            why: "must be a bare locale name ([A-Za-z0-9._-@], e.g. 'en_US.UTF-8')",
        });
    }
    if crate::model::locale_codeset(locale).is_none() {
        return Err(ConfigError::InvalidLocale {
            value: locale.to_string(),
            why: "has no codeset — locale-gen needs one (write 'de_DE.UTF-8', not 'de_DE')",
        });
    }
    Ok(())
}

/// Reject a timezone that is not a `tzdata` zone name.
///
/// It becomes the target of the `/etc/localtime` symlink under `/usr/share/zoneinfo/`,
/// so a `..` or a leading `/` would aim the system clock at an arbitrary file outside
/// the zone database. Shape only — whether the zone *exists* is a fact about the
/// target's `tzdata`, which the rootfs stage checks in the chroot.
fn validate_timezone(tz: &str) -> Result<(), ConfigError> {
    if tz.is_empty() {
        return Err(ConfigError::InvalidTimezone {
            value: tz.to_string(),
            why: "empty",
        });
    }
    if tz.starts_with('/') || tz.ends_with('/') {
        return Err(ConfigError::InvalidTimezone {
            value: tz.to_string(),
            why: "must be a zone name relative to /usr/share/zoneinfo (e.g. 'America/New_York')",
        });
    }
    for part in tz.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            return Err(ConfigError::InvalidTimezone {
                value: tz.to_string(),
                why: "must not contain an empty or dot component (no path traversal)",
            });
        }
        if !part
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '+'))
        {
            return Err(ConfigError::InvalidTimezone {
                value: tz.to_string(),
                why: "zone components are [A-Za-z0-9_+-] (e.g. 'Etc/GMT+5')",
            });
        }
    }
    Ok(())
}

/// Resolve the NTP servers the image prefers: the override when a recipe or flag names
/// one, otherwise the base-layer list.
///
/// Base-layer rather than device-layer because which time source a board should ask is
/// a property of the network it is deployed on, not of the hardware — the same board on
/// two networks wants two answers, and no board wants a different answer than its
/// neighbour on the same network.
///
/// An empty result is the normal one and means "write no configuration", leaving
/// Debian's compiled-in `FallbackNTP` pool. See [`ResolvedBuild::ntp_servers`].
fn resolve_ntp_servers(
    base: &BaseLayer,
    overrides: &Overrides,
) -> Result<Vec<String>, ConfigError> {
    let servers = overrides
        .ntp_servers
        .clone()
        .unwrap_or_else(|| base.ntp_servers.clone());
    for server in &servers {
        validate_ntp_server(server)?;
    }
    Ok(servers)
}

/// Reject anything that is not a bare host: `timesyncd` splits `NTP=` on whitespace and
/// hands each field to the resolver, so a value with a space in it becomes two servers
/// and a value with a scheme or a port becomes none.
///
/// Deliberately permissive about the host itself — an IPv4 or IPv6 literal and a
/// hostname are all legal, and this cannot know which resolve on the target network.
/// It rejects only the shapes that are wrong regardless of network.
fn validate_ntp_server(server: &str) -> Result<(), ConfigError> {
    if server.is_empty() {
        return Err(ConfigError::InvalidNtpServer {
            value: server.to_string(),
            why: "empty",
        });
    }
    if server.chars().any(char::is_whitespace) {
        return Err(ConfigError::InvalidNtpServer {
            value: server.to_string(),
            why: "must not contain whitespace (NTP= is a space-separated list, so one \
                  entry with a space in it silently becomes two servers)",
        });
    }
    if server.contains("://") {
        return Err(ConfigError::InvalidNtpServer {
            value: server.to_string(),
            why: "must be a bare host, not a URL (e.g. 'ntp.example.org', not \
                  'ntp://ntp.example.org')",
        });
    }
    // A bracketed IPv6 literal is the URL form and carries a port; the bare address is
    // what `timesyncd` wants.
    if server.starts_with('[') || server.ends_with(']') {
        return Err(ConfigError::InvalidNtpServer {
            value: server.to_string(),
            why: "IPv6 addresses go in unbracketed (e.g. 'fd00::1', not '[fd00::1]')",
        });
    }
    // A colon is a port separator on everything except an IPv6 literal, which has at
    // least two. `timesyncd` speaks port 123 and offers no way to say otherwise.
    if server.matches(':').count() == 1 {
        return Err(ConfigError::InvalidNtpServer {
            value: server.to_string(),
            why: "must not carry a port (timesyncd always uses 123)",
        });
    }
    if server
        .chars()
        .any(|c| !(c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | ':' | '_')))
    {
        return Err(ConfigError::InvalidNtpServer {
            value: server.to_string(),
            why: "hosts are [A-Za-z0-9.:_-] (a hostname or a bare IP address)",
        });
    }
    Ok(())
}

/// Reject a depthcharge board profile name the rootfs stage cannot carry.
///
/// The value is written into `/etc/depthcharge-tools/config` — through a **quoted
/// heredoc** in the customize script, so it is not shell-expanded, but a line reading
/// `B2D_EOF` would close the heredoc early and leave the rest of the profile name
/// running as script. It is also a key in an INI file `depthchargectl` parses and a
/// filename component it looks the board's `.conf` up by.
///
/// Real profile names are bare identifiers (`speedy`, `speedy-libreboot`), so the safe
/// set is also the complete one: `[A-Za-z0-9_.-]`, non-empty, not a directory
/// reference.
fn validate_board_profile(board: &str) -> Result<(), ConfigError> {
    let ok = !board.is_empty()
        && board != "."
        && board != ".."
        && board
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'));
    if ok {
        Ok(())
    } else {
        Err(ConfigError::InvalidBoardProfile {
            board: board.to_string(),
        })
    }
}

/// Reject a hostname the image's `/etc/hostname` and `/etc/hosts` could not carry.
///
/// The value is written into both verbatim — as the whole of the first file, and as
/// the name half of a `127.0.1.1` line in the second. `/etc/hosts` is line- and
/// token-oriented, so a newline in the value does not corrupt one entry, it *adds*
/// one: everything after the newline becomes a further host line the operator never
/// wrote, mapping names of an attacker's choosing to addresses of their choosing. A
/// space is the same problem one field down, where the rest of the value becomes an
/// alias of `127.0.1.1`.
///
/// The shape itself is [`hostname::check`](crate::hostname::check), shared with the
/// device-slug rule so the generator's default cannot fall outside what a resolved
/// build accepts.
fn validate_hostname(hostname: &str) -> Result<(), ConfigError> {
    crate::hostname::check(hostname).map_err(|why| ConfigError::InvalidHostname {
        value: hostname.to_string(),
        why,
    })
}

/// Reject a keymap value `/etc/default/keyboard` cannot hold.
///
/// That file is *sourced by shell* — `console-setup` and `keyboard-setup` read it with
/// `.` — so every value is rendered inside double quotes and a `"`, `$`, or backtick in
/// one would end the string and run as code on the target. The XKB grammar needs none
/// of those characters, so the safe set is also the complete one.
fn validate_keymap(keymap: &Keymap) -> Result<(), ConfigError> {
    if keymap.layout.is_empty() {
        return Err(ConfigError::InvalidKeymap {
            field: "layout",
            value: keymap.layout.clone(),
            why: "empty — a keymap must name a layout (e.g. 'us')",
        });
    }
    for (field, value) in [
        ("layout", &keymap.layout),
        ("model", &keymap.model),
        ("variant", &keymap.variant),
        ("options", &keymap.options),
    ] {
        if !value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, ',' | ':' | '_' | '-' | '+' | '.'))
        {
            return Err(ConfigError::InvalidKeymap {
                field,
                value: value.clone(),
                why: "XKB values are [A-Za-z0-9,:_+.-] — /etc/default/keyboard is sourced by shell",
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// The boot2deb repo root (two levels up from this crate's manifest).
    fn repo_root() -> ConfigRoot {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap()
            .to_path_buf();
        ConfigRoot::new(dir)
    }

    fn layer(atf: Option<&str>, tpl: Option<&str>, bl32: Option<&str>) -> RkbinLayer {
        RkbinLayer {
            atf: atf.map(Into::into),
            tpl: tpl.map(Into::into),
            bl32: bl32.map(Into::into),
        }
    }

    #[test]
    fn rkbin_inherits_soc_defaults_when_the_device_overrides_nothing() {
        // Standard-memory board: empty device block, everything from the SoC.
        let soc = layer(Some("atf.elf"), Some("ddr.bin"), None);
        let r = resolve_rkbin(&soc, &RkbinLayer::default(), "dev").unwrap();
        assert_eq!(r.atf, "atf.elf");
        assert_eq!(r.tpl, "ddr.bin");
        assert_eq!(r.bl32, None);
    }

    #[test]
    fn rkbin_device_overrides_win_per_field() {
        // The device swaps only the DDR TPL (different DRAM); ATF still inherited.
        let soc = layer(Some("atf.elf"), Some("ddr-lpddr4.bin"), None);
        let dev = layer(None, Some("ddr-ddr4.bin"), None);
        let r = resolve_rkbin(&soc, &dev, "dev").unwrap();
        assert_eq!(r.atf, "atf.elf", "ATF inherited from the SoC");
        assert_eq!(r.tpl, "ddr-ddr4.bin", "device TPL wins");
    }

    #[test]
    fn rkbin_bl32_resolves_from_either_layer_and_stays_optional() {
        // BL32 from the SoC (the RK3576 case): inherited onto a board that omits it.
        let soc = layer(Some("atf.elf"), Some("ddr.bin"), Some("optee.bin"));
        let r = resolve_rkbin(&soc, &RkbinLayer::default(), "dev").unwrap();
        assert_eq!(r.bl32.as_deref(), Some("optee.bin"));
        // A device may still override it.
        let dev = layer(None, None, Some("optee-board.bin"));
        let r2 = resolve_rkbin(&soc, &dev, "dev").unwrap();
        assert_eq!(r2.bl32.as_deref(), Some("optee-board.bin"));
    }

    #[test]
    fn rkbin_missing_required_field_is_a_typed_error() {
        // No layer supplies the TPL -> MissingBlob naming the device and field.
        let soc = layer(Some("atf.elf"), None, None);
        let err = resolve_rkbin(&soc, &RkbinLayer::default(), "h96").unwrap_err();
        match err {
            ConfigError::MissingBlob { device, what } => {
                assert_eq!(device, "h96");
                assert_eq!(what, "rkbin.tpl");
            }
            other => panic!("expected MissingBlob, got {other:?}"),
        }
        // A blank override does not mask a good SoC default.
        let blanked =
            resolve_rkbin(&soc, &layer(Some("  "), Some("ddr.bin"), None), "h96").unwrap();
        assert_eq!(blanked.atf, "atf.elf");
    }

    #[test]
    fn a_patch_series_always_carries_its_source_and_ref() {
        // The lock records `source` + `ref` beside the pinned commit, so the three
        // travel together or not at all: a commit id is meaningless outside its repo,
        // and `update` takes this one from a local HEAD rather than a remote, making it
        // the pin likeliest to name something unreachable. Resolution is where the
        // pairing is established, so assert both directions on the shipped config.
        let root = repo_root();

        let patched = resolve_recipe(&root, "turing-rk1/forky", &Overrides::default()).unwrap();
        let k = patched
            .kernel
            .as_ref()
            .unwrap()
            .compiled()
            .expect("a compiled kernel");
        assert_eq!(k.patch_series, vec!["rk3588-accel".to_string()]);
        assert!(k
            .patches_url
            .as_deref()
            .is_some_and(|u| u.contains("patches")));
        // No release tag exists yet, so the branch is the honest ref: it says the pin
        // came from the tip of development rather than implying a release never cut.
        assert_eq!(
            k.patches_ref.as_deref(),
            Some(crate::model::DEFAULT_PATCHES_REF)
        );

        // A kernel applying no series pins nothing, so it names neither.
        let unpatched = resolve_recipe(&root, "asus-c201/forky", &Overrides::default()).unwrap();
        let uk = unpatched.kernel.as_ref().unwrap();
        assert!(uk.patch_series().is_empty());
        assert!(uk.compiled().is_none());
    }

    #[test]
    fn a_uboot_only_recipe_resolves_no_suite_or_kernel() {
        // The loader recipe is a pure bootloader deliverable: it resolves no suite and
        // no kernel, only the u-boot series, so the whole image axis is absent. It is
        // a SoC-generic tool homed on the rk3576-generic device (not a board).
        let root = repo_root();
        let b = resolve_recipe(&root, "rk3576-generic/loader", &Overrides::default()).unwrap();
        assert!(b.suite.is_none(), "a u-boot-only build resolves no suite");
        assert!(b.kernel.is_none(), "a u-boot-only build resolves no kernel");
        assert!(!b.produces_image());
        // It still produces a `.deb`, so it still names the suite that archives it —
        // the device's own default, which is what this board's image builds resolve.
        // Sharing that answer is what lets the two share a provisioned packaging root.
        assert_eq!(
            b.packaging_suite,
            root.device("rk3576-generic").unwrap().default_suite
        );
        let boot = b.rkbin_boot().expect("a rockchip-rkbin boot");
        // The u-boot series lives on its own axis, paired with its patches source.
        assert_eq!(boot.uboot_series.as_deref(), Some("rk3576-loader"));
        assert!(boot.uboot_patches_url.is_some());
        assert!(boot.uboot_patches_ref.is_some());

        // The image recipe on the same device resolves a suite, a pristine kernel, and
        // the *display* u-boot series — the series is not a kernel field any more.
        let img = resolve_recipe(&root, "h96-max-m9/forky", &Overrides::default()).unwrap();
        assert!(img.produces_image());
        assert_eq!(img.suite.as_deref(), Some("forky"));
        // An image build archives its own `.deb`s with its own suite's `dpkg`.
        assert_eq!(img.packaging_suite, "forky");
        // The kernel carries the SoC-wide `rk3576-fixes` and the board's own
        // `rk3576-npu`, in that order — a device's series compose after its kernel's.
        // The H96's AIC8800 Wi-Fi driver is *not* among them: it is an out-of-tree
        // module built from a pinned repo, composed via `device_kmods`.
        let kseries: Vec<&str> = img
            .kernel
            .as_ref()
            .unwrap()
            .patch_series()
            .iter()
            .map(String::as_str)
            .collect();
        assert_eq!(kseries, ["rk3576-fixes", "rk3576-npu"]);
        // The board opts into the aic8800 external module by name; every field below is
        // the shipped `kmods/aic8800.toml`, not something the device restates.
        let kmod = img
            .device_kmods
            .iter()
            .find(|k| k.name == "aic8800")
            .expect("aic8800 device_kmod");
        assert_eq!(kmod.modules, ["aic8800_bsp", "aic8800_fdrv"]);
        // Both make args are load-bearing: without NO_REG_SDIO the driver never creates
        // wlan0, and BTLPM does not compile on a 7.1 kernel.
        assert_eq!(
            kmod.make_args,
            [
                "CONFIG_AIC8800_BTLPM_SUPPORT=n",
                "CONFIG_FDRV_NO_REG_SDIO=y"
            ]
        );
        // The compat shims are bare filenames under `kmods/aic8800/patches/`, in apply
        // order — the SDIO 7.1 cfg80211 port, the two quieting patches, then the
        // suspend fix.
        assert_eq!(
            kmod.local_patches,
            [
                "0001-sdio-linux-7.1.patch",
                "0002-quiet-log-level.patch",
                "0003-quiet-bare-printk.patch",
                "0004-suspend-quiesce-sdio.patch"
            ]
        );
        let fw = kmod
            .firmware
            .as_ref()
            .expect("the aic8800 kmod ships firmware");
        // The install path is what fix-sdio-firmware-path.patch compiles into the BSP
        // loader; a drift here is a driver that boots and then finds no firmware.
        assert_eq!(fw.subdir, "src/SDIO/driver_fw/fw/aic8800D80");
        assert_eq!(fw.install, "usr/lib/firmware/aic8800_fw/SDIO/aic8800D80");
        assert_eq!(
            img.rkbin_boot().unwrap().uboot_series.as_deref(),
            Some("rk3576-display")
        );
    }

    #[test]
    fn an_unknown_uboot_series_is_rejected() {
        // The u-boot series is validated against the device's set, exactly like the
        // kernel axis.
        let root = repo_root();
        let ov = Overrides {
            uboot_series: Some("definitely-not-a-series".into()),
            ..Default::default()
        };
        let err = resolve_device(&root, "h96-max-m9", &ov).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::UnknownUbootSeriesForDevice { .. }
        ));
    }

    #[test]
    fn a_uboot_only_deliverable_needs_a_bootloader_board() {
        // A depthcharge board builds no u-boot, so a u-boot-only deliverable on it has
        // nothing to emit — rejected at resolve.
        let root = repo_root();
        let ov = Overrides {
            deliverable: Deliverable::Uboot,
            ..Default::default()
        };
        let err = resolve_device(&root, "asus-c201", &ov).unwrap_err();
        assert!(matches!(
            err,
            ConfigError::UbootOnlyWithoutBootloader { .. }
        ));
    }

    #[test]
    fn a_uboot_only_build_resolves_no_out_of_tree_modules() {
        // The H96 declares an out-of-tree Wi-Fi driver, and its image recipes build
        // one. Its u-boot-only deliverable compiles no kernel, so there is nothing to
        // build a module against — and a resolved kmod would be pinned in the lock and
        // published in the support matrix as a driver this bootloader image carries.
        let root = repo_root();
        let img = resolve_device(&root, "h96-max-m9", &Default::default()).unwrap();
        assert!(
            !img.device_kmods.is_empty(),
            "the board is only a useful test if its image build has kmods"
        );
        let ov = Overrides {
            deliverable: Deliverable::Uboot,
            ..Default::default()
        };
        let uboot_only = resolve_device(&root, "h96-max-m9", &ov).unwrap();
        assert!(uboot_only.device_kmods.is_empty());
    }

    #[test]
    fn a_uboot_only_build_rejects_the_axes_it_does_not_have() {
        // A u-boot-only build skips feature loading, `resolve_l10n`, and the
        // `image_size` parse, so every rootfs-axis override would otherwise be accepted
        // and dropped in silence — including values that are hard errors on every other
        // path. A misspelled `--feature` exiting 0 is the case that matters.
        let root = repo_root();
        for (flag, ov) in [
            (
                "--feature",
                Overrides {
                    features: Some(vec!["no-such-feature".into()]),
                    ..Default::default()
                },
            ),
            (
                "--image-size",
                Overrides {
                    image_size: Some("1M".into()),
                    ..Default::default()
                },
            ),
            (
                "--suite",
                Overrides {
                    suite: Some("trixie".into()),
                    ..Default::default()
                },
            ),
            (
                "--locale",
                Overrides {
                    locale: Some("nonsense".into()),
                    ..Default::default()
                },
            ),
        ] {
            let err = resolve_recipe(&root, "rk3576-generic/loader", &ov).unwrap_err();
            match err {
                ConfigError::OverrideNotApplicable { flag: got, .. } => assert_eq!(got, flag),
                other => panic!("expected {flag} to be rejected, got {other}"),
            }
        }
        // The axes a bootloader *does* have still resolve: the u-boot series picks the
        // series, and the layout decides whether it is emitted standalone.
        let ov = Overrides {
            uboot_series: Some("rk3576-util".into()),
            layout: Some(Layout::Split),
            ..Default::default()
        };
        let b = resolve_recipe(&root, "rk3576-generic/loader", &ov).unwrap();
        assert_eq!(
            b.rkbin_boot().unwrap().uboot_series.as_deref(),
            Some("rk3576-util")
        );
        assert_eq!(b.layout, Layout::Split);
        // And an override from the rejected list is accepted on an image recipe of the
        // same SoC, so the rejection is about the deliverable, not about the values.
        // `--image-size` rather than `--suite`, because the RK3576 boards declare one
        // supported suite and a second value would be refused for that reason instead.
        let ov = Overrides {
            image_size: Some("8G".into()),
            ..Default::default()
        };
        assert!(resolve_recipe(&root, "h96-max-m9/forky", &ov).is_ok());
    }

    #[test]
    fn the_none_sentinel_resolves_to_no_patch_series() {
        // The `"none"` spelling is config-facing only (the u-boot pristine sentinel);
        // resolution turns it into a typed absence so no downstream code compares
        // against the magic string. The kernel axis expresses "no series" as an empty
        // `patch_series` list instead, so it does not go through this helper.
        assert_eq!(crate::series::patch_series("none"), None);
        assert_eq!(
            crate::series::patch_series("rk3588-accel"),
            Some("rk3588-accel")
        );
    }

    #[test]
    fn device_dts_empty_is_the_upstream_dtb_case() {
        // A board whose DTB is already in the kernel lists no sources, and
        // `kernel_dtb` is then unconstrained by this check.
        assert!(validate_device_dts(&[], "rockchip/rk3576-evb1-v10.dtb", "evb1").is_ok());
    }

    #[test]
    fn device_dts_must_build_the_kernel_dtb() {
        let dts = ["devices/h96/dts/rk3576-h96-max-m9.dts".to_string()];
        // The `.dts` basename matches the `.dtb` basename: the board boots what it builds.
        assert!(validate_device_dts(&dts, "rockchip/rk3576-h96-max-m9.dtb", "h96").is_ok());
        // A `.dtsi` alongside it is fine as long as the `.dts` is present.
        let with_dtsi = [
            "devices/h96/dts/rk3576-h96-common.dtsi".to_string(),
            "devices/h96/dts/rk3576-h96-max-m9.dts".to_string(),
        ];
        assert!(validate_device_dts(&with_dtsi, "rockchip/rk3576-h96-max-m9.dtb", "h96").is_ok());

        // A typo'd `kernel_dtb` names a DTB no source builds -> typed error, not a bad boot.
        let err = validate_device_dts(&dts, "rockchip/rk3576-h96-max-m9s.dtb", "h96").unwrap_err();
        match err {
            ConfigError::KernelDtbNotInDeviceDts {
                device, expected, ..
            } => {
                assert_eq!(device, "h96");
                assert_eq!(expected, "rk3576-h96-max-m9s.dts");
            }
            other => panic!("expected KernelDtbNotInDeviceDts, got {other:?}"),
        }
        // A lone `.dtsi` builds no DTB, so it cannot satisfy `kernel_dtb`.
        let only_dtsi = ["devices/h96/dts/rk3576-h96-max-m9.dtsi".to_string()];
        assert!(validate_device_dts(&only_dtsi, "rockchip/rk3576-h96-max-m9.dtb", "h96").is_err());
    }

    #[test]
    fn device_dts_entries_must_be_contained_dt_sources() {
        let bad = |entry: &str| {
            let err =
                validate_device_dts(&[entry.to_string()], "rockchip/b.dtb", "h96").unwrap_err();
            assert!(
                matches!(err, ConfigError::InvalidDeviceDts { .. }),
                "expected InvalidDeviceDts for {entry:?}, got {err:?}"
            );
        };
        bad(""); // empty
        bad("/etc/passwd.dts"); // absolute
        bad("../../outside/b.dts"); // escapes the config root
        bad("devices/h96/dts/../../../b.dts"); // escapes mid-path
        bad("devices/h96/dts/b.dtb"); // a blob, not a source
        bad("devices/h96/dts/b"); // no extension
    }

    #[test]
    fn validate_suite_accepts_codenames_and_rejects_bad_shapes() {
        for s in [
            "forky",
            "trixie",
            "sid",
            "bookworm",
            "stable-proposed-updates",
        ] {
            assert!(validate_suite(s).is_ok(), "{s} should be a valid suite");
        }
        for s in [
            "",               // empty
            "-updates",       // leading dash (option-like)
            "..",             // traversal
            "for ky",         // space
            "forky;rm -rf /", // shell metacharacters
            "forky/../etc",   // path separator
        ] {
            assert!(
                matches!(validate_suite(s), Err(ConfigError::InvalidSuite { .. })),
                "{s} should be rejected"
            );
        }
    }

    #[test]
    fn a_suite_is_checked_against_the_devices_declared_set() {
        let named = ["forky".to_string(), "trixie".to_string()];
        assert!(check_suite_supported("dev", &named, "forky").is_ok());
        assert!(check_suite_supported("dev", &named, "trixie").is_ok());
        // A typo and a suite the board is genuinely not built for land in the same
        // place, and both name the valid set rather than the path they probed.
        for bad in ["forkey", "bookworm"] {
            match check_suite_supported("dev", &named, bad) {
                Err(ConfigError::UnsupportedSuite {
                    suite, supported, ..
                }) => {
                    assert_eq!(suite, bad);
                    assert_eq!(supported, "forky, trixie");
                }
                other => panic!("{bad} should be rejected, got {other:?}"),
            }
        }

        // The wildcard admits anything well-formed, and is the whole list or none.
        let any = [ANY_SUITE.to_string()];
        for s in ["forky", "bookworm", "sid"] {
            assert!(check_suite_supported("dev", &any, s).is_ok());
        }
        let mixed = [ANY_SUITE.to_string(), "forky".to_string()];
        assert!(matches!(
            check_suite_supported("dev", &mixed, "forky"),
            Err(ConfigError::SuiteWildcardMixed { .. })
        ));
        assert!(matches!(
            check_suite_supported("dev", &[], "forky"),
            Err(ConfigError::NoSupportedSuites { .. })
        ));
    }

    #[test]
    fn a_shipped_board_refuses_a_suite_it_is_not_built_for() {
        let root = repo_root();
        // `sid` is a well-formed codename, so only the device's own claim can refuse
        // it — which is the whole point of the axis being typed.
        let ov = Overrides {
            suite: Some("sid".into()),
            ..Default::default()
        };
        match resolve_device(&root, "turing-rk1", &ov).unwrap_err() {
            ConfigError::UnsupportedSuite { suite, .. } => assert_eq!(suite, "sid"),
            other => panic!("expected UnsupportedSuite, got {other}"),
        }
    }

    #[test]
    fn l10n_defaults_come_from_the_layer_that_determines_them() {
        let root = repo_root();

        // The RK1 is a headless server: it takes the base layer's system-wide locale
        // and timezone, and has no keymap at all — nothing is typing at its console.
        let rk1 = resolve_recipe(&root, "turing-rk1/forky", &Overrides::default()).unwrap();
        assert_eq!(rk1.locale, "C.UTF-8");
        assert_eq!(rk1.timezone, "UTC");
        assert_eq!(rk1.keymap, None, "a headless board declares no keymap");

        // The C201 is a laptop, and is the one shipped board a console keymap
        // configures anything on. It takes the same system-wide locale.
        let c201 = resolve_recipe(&root, "asus-c201/forky", &Overrides::default()).unwrap();
        assert_eq!(c201.locale, "C.UTF-8");
        let keymap = c201
            .keymap
            .expect("a board with a keyboard declares a keymap");
        assert_eq!(keymap.layout, "us");
        assert_eq!(
            keymap.model, "pc105",
            "the bare-string form takes Debian's model"
        );
    }

    #[test]
    fn the_system_locale_is_always_generated() {
        // The invariant: LANG can never name a locale the image does not carry. It
        // holds even when the locale is one the config never listed, and even when it
        // is C.UTF-8 — which glibc builds in and *would* work ungenerated, but which
        // must still appear in /etc/locale.gen, because that file is where the `locales`
        // package builds the choice list `dpkg-reconfigure locales` offers.
        let root = repo_root();

        // These assert the invariant, not the languages base.toml happens to ship: which
        // locales are worth the image bytes is a policy that moves, and pinning the list
        // here would only make widening it look like a regression.
        let base = resolve_recipe(&root, "turing-rk1/forky", &Overrides::default()).unwrap();
        assert_eq!(
            base.locales_generate.first(),
            Some(&base.locale),
            "the system locale leads the generated set"
        );
        assert_eq!(
            base.locales_generate
                .iter()
                .filter(|l| **l == base.locale)
                .count(),
            1,
            "and appears in it exactly once"
        );
        assert!(
            base.locales_generate.iter().any(|l| l == "en_US.UTF-8"),
            "en_US.UTF-8 is generated on every image, so an SSH client forwarding it \
             lands on a locale the image carries"
        );

        // An override the base never lists is generated anyway, and leads the set.
        let de = resolve_recipe(
            &root,
            "turing-rk1/forky",
            &Overrides {
                locale: Some("de_DE.UTF-8".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(de.locales_generate.first().unwrap(), "de_DE.UTF-8");
        assert_eq!(
            de.locales_generate
                .iter()
                .filter(|l| *l == "de_DE.UTF-8")
                .count(),
            1,
            "leading the set does not duplicate a base entry for the same locale"
        );
        assert!(
            de.locales_generate.iter().any(|l| l == "en_US.UTF-8"),
            "the base's extras still follow the overridden system locale"
        );

        // Naming it in both places generates it once, not twice.
        let dup = resolve_recipe(
            &root,
            "turing-rk1/forky",
            &Overrides {
                locale: Some("fr_FR.UTF-8".into()),
                locales_generate: Some(vec!["fr_FR.UTF-8".into(), "ja_JP.UTF-8".into()]),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(dup.locales_generate, vec!["fr_FR.UTF-8", "ja_JP.UTF-8"]);
    }

    #[test]
    fn a_keymap_override_reaches_a_board_that_defaults_none() {
        // `console-setup` ships on every image, so a keymap is always *actionable* —
        // a headless board simply has no reason to default one. Plugging a USB keyboard
        // into the RK1's HDMI console is a real thing to do, and `--keymap` covers it.
        let root = repo_root();
        let b = resolve_recipe(
            &root,
            "turing-rk1/forky",
            &Overrides {
                keymap: Some(Keymap::from_layout("gb")),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(b.keymap.unwrap().layout, "gb");
    }

    /// A real ed25519 public key, for the account tests.
    const TEST_KEY: &str = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIBl5Nn9dY/aLK4WVQ5c4tYlYCkkC1J3Ry+d0nc3TgtDe operator@workstation";

    #[test]
    fn account_defaults_come_from_the_base_layer_and_a_recipe_overrides_them() {
        let root = repo_root();

        // The shipped default: root with no prompt, a 12-character generated password,
        // and nobody authorized by key — an image a stock build hands out authorizes
        // whoever holds the password it printed, and no one else.
        let b = resolve_recipe(&root, "turing-rk1/forky", &Overrides::default()).unwrap();
        assert_eq!(b.sudo, SudoPolicy::Nopasswd);
        assert_eq!(
            b.first_boot_password_length,
            crate::model::DEFAULT_PASSWORD_LENGTH
        );
        assert!(b.ssh_authorized_keys.is_empty());

        // Each part overrides independently, and the keys keep the authored order —
        // authorized_keys is a file sshd walks, so the order is part of what was written.
        let second = format!("{TEST_KEY}-two");
        let tightened = resolve_recipe(
            &root,
            "turing-rk1/forky",
            &Overrides {
                sudo: Some(SudoPolicy::Password),
                first_boot_password_length: Some(24),
                ssh_authorized_keys: Some(vec![TEST_KEY.to_string(), second.clone()]),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(tightened.sudo, SudoPolicy::Password);
        assert_eq!(tightened.first_boot_password_length, 24);
        assert_eq!(tightened.ssh_authorized_keys, vec![TEST_KEY, &second]);
    }

    /// The bounds exist because a short generated password is invisible on the finished
    /// image: nothing about a booted board says how much entropy its first credential
    /// had, so the only place to catch it is here.
    #[test]
    fn a_password_length_outside_the_accepted_range_is_refused() {
        use crate::model::{MAX_PASSWORD_LENGTH, MIN_PASSWORD_LENGTH};
        let root = repo_root();
        let with_len = |n: u8| {
            resolve_recipe(
                &root,
                "turing-rk1/forky",
                &Overrides {
                    first_boot_password_length: Some(n),
                    ..Default::default()
                },
            )
        };
        // Both ends of the range are accepted — a floor that rejected its own value
        // would leave the documented minimum unreachable.
        assert!(with_len(MIN_PASSWORD_LENGTH).is_ok());
        assert!(with_len(MAX_PASSWORD_LENGTH).is_ok());
        for bad in [0, 1, MIN_PASSWORD_LENGTH - 1, MAX_PASSWORD_LENGTH + 1, 255] {
            assert!(
                matches!(
                    with_len(bad),
                    Err(ConfigError::InvalidPasswordLength { .. })
                ),
                "length {bad} should be rejected"
            );
        }
    }

    /// Keys are validated at resolution, so a key `sshd` would silently skip fails the
    /// build instead of shipping an image whose login does not work.
    #[test]
    fn a_malformed_authorized_key_is_refused_and_names_its_position() {
        let root = repo_root();
        let with_keys = |keys: Vec<String>| {
            resolve_recipe(
                &root,
                "turing-rk1/forky",
                &Overrides {
                    ssh_authorized_keys: Some(keys),
                    ..Default::default()
                },
            )
        };
        assert!(with_keys(vec![TEST_KEY.to_string()]).is_ok());

        // The offending entry is named by its position in the authored list — the only
        // name it has.
        let err = with_keys(vec![TEST_KEY.to_string(), "ssh-ed25519 nope".to_string()])
            .expect_err("a malformed key fails resolution");
        match err {
            ConfigError::InvalidAuthorizedKey { index, .. } => assert_eq!(index, 2),
            other => panic!("expected InvalidAuthorizedKey, got {other}"),
        }

        // The paste that matters most: a private key would otherwise be baked into
        // every copy of the image.
        let private = "-----BEGIN OPENSSH PRIVATE KEY----- b3BlbnNzaA==";
        let err = with_keys(vec![private.to_string()]).expect_err("private key refused");
        assert!(
            err.to_string().contains("private key material"),
            "the message must name the cause: {err}"
        );
    }

    /// A u-boot-only build has no rootfs, so it has no account — and an axis it cannot
    /// act on must fail rather than be quietly dropped.
    #[test]
    fn account_overrides_do_not_apply_to_a_uboot_deliverable() {
        let root = repo_root();
        for (flag, ov) in [
            (
                "--sudo",
                Overrides {
                    sudo: Some(SudoPolicy::Password),
                    ..Default::default()
                },
            ),
            (
                "--password-length",
                Overrides {
                    first_boot_password_length: Some(24),
                    ..Default::default()
                },
            ),
            (
                "ssh_authorized_keys",
                Overrides {
                    ssh_authorized_keys: Some(vec![TEST_KEY.to_string()]),
                    ..Default::default()
                },
            ),
        ] {
            let err = resolve_recipe(&root, "h96-max-m9/util", &ov)
                .expect_err("a u-boot build has no account axis");
            match err {
                ConfigError::OverrideNotApplicable { flag: got, .. } => assert_eq!(got, flag),
                other => panic!("expected OverrideNotApplicable for {flag}, got {other}"),
            }
        }
    }

    #[test]
    fn validate_locale_demands_a_codeset_and_rejects_shell_metacharacters() {
        for ok in ["C.UTF-8", "en_US.UTF-8", "sr_RS.UTF-8@latin", "ja_JP.UTF-8"] {
            assert!(validate_locale(ok).is_ok(), "{ok} should be a valid locale");
        }
        for bad in [
            "",                    // empty
            "de_DE",               // no codeset: locale-gen takes `<name> <codeset>` pairs
            "en_US.UTF-8; rm -rf", // shell metacharacters (it lands in a sourced file)
            "en_US.UTF-8\"$(id)",  // quote + substitution
            "../../etc/passwd",    // path shape
        ] {
            assert!(
                matches!(validate_locale(bad), Err(ConfigError::InvalidLocale { .. })),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn locale_codeset_is_the_half_after_the_dot_and_before_any_modifier() {
        assert_eq!(crate::model::locale_codeset("en_US.UTF-8"), Some("UTF-8"));
        assert_eq!(crate::model::locale_codeset("C.UTF-8"), Some("UTF-8"));
        // A modifier rides *after* the codeset: `sr_RS.UTF-8@latin UTF-8` is the real
        // /etc/locale.gen line, so the modifier must not be swept into the codeset.
        assert_eq!(
            crate::model::locale_codeset("sr_RS.UTF-8@latin"),
            Some("UTF-8")
        );
        assert_eq!(crate::model::locale_codeset("de_DE"), None);
        assert_eq!(crate::model::locale_codeset("de_DE."), None);
    }

    /// A [`BaseLayer`] with every key at the value a config root that omits it gets.
    ///
    /// Deserialized from an empty document rather than built field by field, so it
    /// stays the *real* set of defaults: a new key with a `#[serde(default)]` lands
    /// here automatically, and a new key without one fails this instead of silently
    /// giving tests a value no config root would produce.
    fn base_layer_fixture() -> BaseLayer {
        toml::from_str("").expect("every base.toml key carries a default")
    }

    #[test]
    fn validate_ntp_server_rejects_anything_that_is_not_a_bare_host() {
        for ok in [
            "ntp.example.org",
            "0.debian.pool.ntp.org",
            "192.168.1.1",
            "fd00::1",
            "time-a_g.nist.gov",
        ] {
            assert!(
                validate_ntp_server(ok).is_ok(),
                "{ok} should be a valid NTP server"
            );
        }
        for bad in [
            "",                      // empty
            "ntp.example.org ntp2",  // whitespace: silently becomes two servers
            "ntp://ntp.example.org", // a scheme the resolver can never answer
            "https://ntp.example.org",
            "ntp.example.org:123", // timesyncd always uses 123 and parses no port
            "192.168.1.1:123",
            "[fd00::1]",   // the bracketed form is for URLs and implies a port
            "[fd00::1]:1", // both at once
            "ntp.example.org/path",
            "ntp,example.org", // a comma is not a list separator here, a space is
        ] {
            assert!(
                matches!(
                    validate_ntp_server(bad),
                    Err(ConfigError::InvalidNtpServer { .. })
                ),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn ntp_servers_default_to_the_base_list_and_an_override_replaces_it() {
        let base = BaseLayer {
            ntp_servers: vec!["ntp.base".to_string()],
            ..base_layer_fixture()
        };

        // No override: the base list stands.
        let none = Overrides::default();
        assert_eq!(
            resolve_ntp_servers(&base, &none).unwrap(),
            vec!["ntp.base".to_string()]
        );

        // An override replaces rather than appends — the base entry is gone, not first.
        let replaced = Overrides {
            ntp_servers: Some(vec!["ntp.lan".to_string(), "192.168.1.1".to_string()]),
            ..Overrides::default()
        };
        assert_eq!(
            resolve_ntp_servers(&base, &replaced).unwrap(),
            vec!["ntp.lan".to_string(), "192.168.1.1".to_string()]
        );

        // `Some([])` is distinguishable from `None`: it drops what the base named and
        // returns the image to Debian's fallback pool, which is the whole reason the
        // override is an `Option<Vec<_>>` rather than a `Vec<_>`.
        let cleared = Overrides {
            ntp_servers: Some(Vec::new()),
            ..Overrides::default()
        };
        assert!(resolve_ntp_servers(&base, &cleared).unwrap().is_empty());
    }

    #[test]
    fn an_invalid_ntp_server_fails_resolution_wherever_it_came_from() {
        let base = BaseLayer {
            ntp_servers: vec!["ntp.example.org:123".to_string()],
            ..base_layer_fixture()
        };
        assert!(matches!(
            resolve_ntp_servers(&base, &Overrides::default()),
            Err(ConfigError::InvalidNtpServer { .. })
        ));

        // And from an override, over a base that is perfectly fine.
        let good_base = BaseLayer {
            ntp_servers: vec!["ntp.example.org".to_string()],
            ..base_layer_fixture()
        };
        let bad_override = Overrides {
            ntp_servers: Some(vec!["ntp with a space".to_string()]),
            ..Overrides::default()
        };
        assert!(matches!(
            resolve_ntp_servers(&good_base, &bad_override),
            Err(ConfigError::InvalidNtpServer { .. })
        ));
    }

    #[test]
    fn validate_timezone_rejects_anything_that_escapes_the_zone_database() {
        for ok in [
            "UTC",
            "America/New_York",
            "Etc/GMT+5",
            "America/Argentina/Buenos_Aires",
        ] {
            assert!(validate_timezone(ok).is_ok(), "{ok} should be a valid zone");
        }
        for bad in [
            "",                         // empty
            "/etc/shadow",              // absolute: escapes /usr/share/zoneinfo
            "../../../etc/shadow",      // traversal: /etc/localtime would point at it
            "America/../../etc/shadow", // traversal mid-path
            "Europe/",                  // trailing separator
            "Europe/Ber lin",           // space
            "Europe/Berlin;id",         // shell metacharacter
        ] {
            assert!(
                matches!(
                    validate_timezone(bad),
                    Err(ConfigError::InvalidTimezone { .. })
                ),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn validate_keymap_rejects_what_a_sourced_shell_file_cannot_hold() {
        assert!(validate_keymap(&Keymap::from_layout("us")).is_ok());
        assert!(validate_keymap(&Keymap {
            layout: "us,de".into(), // XKB takes a layout list
            model: "pc105".into(),
            variant: "nodeadkeys".into(),
            options: "ctrl:nocaps,grp:alt_shift_toggle".into(),
        })
        .is_ok());

        // /etc/default/keyboard is sourced by console-setup, so a quote closes the
        // string and what follows runs as code on the target.
        let injected = Keymap {
            layout: "us\"; id #".into(),
            ..Keymap::from_layout("us")
        };
        assert!(matches!(
            validate_keymap(&injected),
            Err(ConfigError::InvalidKeymap {
                field: "layout",
                ..
            })
        ));
        let subst = Keymap {
            options: "$(id)".into(),
            ..Keymap::from_layout("us")
        };
        assert!(matches!(
            validate_keymap(&subst),
            Err(ConfigError::InvalidKeymap {
                field: "options",
                ..
            })
        ));
        assert!(matches!(
            validate_keymap(&Keymap::from_layout("")),
            Err(ConfigError::InvalidKeymap {
                field: "layout",
                ..
            })
        ));
    }

    #[test]
    fn validate_board_profile_rejects_what_a_heredoc_cannot_hold() {
        // The shipped profiles — the shapes this has to keep accepting.
        for ok in ["speedy", "speedy-libreboot", "veyron_speedy", "jerry.v2"] {
            validate_board_profile(ok).unwrap();
        }
        // A newline is the whole risk: the value is written through a quoted heredoc, so
        // it is not expanded, but a line reading `B2D_EOF` ends the heredoc early and
        // what follows becomes script.
        for bad in [
            "speedy\nB2D_EOF\nrm -rf /",
            "speedy; id",
            "speedy $(id)",
            "../../etc/passwd",
            "",
            "..",
        ] {
            assert!(
                matches!(
                    validate_board_profile(bad),
                    Err(ConfigError::InvalidBoardProfile { .. })
                ),
                "accepted {bad:?}"
            );
        }
        // Every shipped depthcharge board resolves, so the rule is not narrower than
        // the config it has to admit.
        let root = repo_root();
        for device in ["asus-c201", "asus-c100p", "asus-chromebit-cs10"] {
            resolve_device(&root, device, &Overrides::default()).unwrap();
        }
    }

    /// The shape itself is `hostname::check`'s to test; what matters here is that a
    /// device carrying a bad one fails *resolution*, with the typed error naming the
    /// value — rather than reaching the image writer, which would render it verbatim.
    #[test]
    fn a_bad_hostname_is_a_typed_resolve_error() {
        validate_hostname("turing-rk1").unwrap();
        for bad in [
            "rk1\n10.0.0.1 deb.debian.org",
            "rk1 deb.debian.org",
            "rk_1",
            "",
        ] {
            match validate_hostname(bad) {
                Err(ConfigError::InvalidHostname { value, .. }) => assert_eq!(value, bad),
                other => panic!("{bad:?}: expected InvalidHostname, got {other:?}"),
            }
        }
    }

    /// The rule is not narrower than the config it has to admit: every shipped board
    /// resolves, on the image path and on the u-boot-only path that returns before
    /// the rootfs axes are touched.
    #[test]
    fn every_shipped_device_carries_a_valid_hostname() {
        let root = repo_root();
        for device in root.list("devices").unwrap() {
            let build = resolve_device(&root, &device, &Overrides::default())
                .unwrap_or_else(|e| panic!("{device}: {e}"));
            validate_hostname(&build.hostname)
                .unwrap_or_else(|e| panic!("{device} resolved an invalid hostname: {e}"));
        }
    }

    #[test]
    fn a_keymap_parses_from_a_bare_layout_or_a_table() {
        #[derive(serde::Deserialize)]
        struct Holder {
            keymap: Keymap,
        }

        // The common case: a layout code, everything else Debian's default.
        let bare: Holder = toml::from_str("keymap = \"us\"").unwrap();
        assert_eq!(bare.keymap, Keymap::from_layout("us"));

        // The full case: a table, with the unstated fields still defaulted.
        let table: Holder =
            toml::from_str("[keymap]\nlayout = \"gb\"\noptions = \"ctrl:nocaps\"\n").unwrap();
        assert_eq!(table.keymap.layout, "gb");
        assert_eq!(table.keymap.model, "pc105");
        assert_eq!(table.keymap.options, "ctrl:nocaps");

        // A typo in the table is an error, not a silently dropped field — which is the
        // whole reason this type has a hand-written Deserialize.
        let typo = toml::from_str::<Holder>("[keymap]\nlayout = \"gb\"\nvarient = \"extd\"\n");
        assert!(typo.is_err(), "an unknown keymap field must be rejected");
    }

    #[test]
    fn rk1_media_accel_recipe_resolves_expected_axes() {
        let root = repo_root();
        let b =
            resolve_recipe(&root, "turing-rk1/media-accel-forky", &Overrides::default()).unwrap();
        assert_eq!(b.arch, Arch::Arm64);
        assert_eq!(b.soc, Soc::Rk3588);
        assert_eq!(b.boot_method, BootMethod::RockchipRkbin);
        assert_eq!(b.suite.as_deref(), Some("forky"));
        assert_eq!(b.layout, Layout::Combined);
        // The recipe's single capability feature resolves + passes the SoC/arch
        // gates.
        assert_eq!(b.features, vec!["media-accel-rockchip"]);
        // media-accel-rockchip declares `requires_media_accel`, so the resolved
        // build carries the SoC's userspace + ffmpeg source trees (built as a unit).
        assert!(
            b.userspace.is_some(),
            "media-accel build carries userspace sources"
        );
        assert!(
            b.ffmpeg.is_some(),
            "media-accel build carries ffmpeg sources"
        );
        // The shipped media-accel-rockchip feature adds no third-party apt source.
        assert!(b.apt_sources.is_empty());
        // Merged rootfs set: base packages + the feature's packages, base excludes.
        assert!(b.rootfs_packages.contains(&"openssh-server".to_string()));
        assert!(b.rootfs_packages.contains(&"ffmpeg-rk".to_string()));
        assert_eq!(b.rootfs_exclude, vec!["isc-dhcp-client"]);
        assert_eq!(b.kernel.as_ref().unwrap().id(), "rk3588-mainline-7.1");
        assert_eq!(b.kernel_dtb, "rockchip/rk3588-turing-rk1.dtb");
        assert!(b.modules.contains(&"rga3".to_string()));
        assert!(b.modules.contains(&"rkvenc".to_string()));
        // kernel fragments precede device fragments in apply order; the generated
        // Debian baseline is first, then the curated rockchip slices.
        let kernel = b
            .kernel
            .as_ref()
            .unwrap()
            .compiled()
            .expect("the RK1 compiles its kernel");
        assert_eq!(
            kernel.config_fragments,
            vec![
                "base/debian-arm64",
                "soc/rk3588",
                "accel/full",
                "device/turing-rk1"
            ]
        );
        // The boot half resolves as the rkbin variant, carrying the u-boot source,
        // the raw-gap offsets, and the SoC-inherited blob set.
        let boot = b.rkbin_boot().expect("the RK1 boots via rockchip-rkbin");
        assert_eq!(boot.uboot_ref, "v2026.07");
        assert_eq!(boot.uboot_defconfig, "turing-rk1-rk3588_defconfig");
        assert_eq!(boot.offsets.idbloader, "32KiB");
        assert_eq!(boot.offsets.uboot_itb, "8MiB");
        assert_eq!(boot.offsets.rootfs, "16MiB");
        assert_eq!(boot.rkbin.atf, "rk3588_bl31_v1.51.elf");
        assert!(b.depthcharge_boot().is_none());
    }

    #[test]
    fn rk1_base_recipe_keeps_the_accel_kernel_but_no_media_userspace() {
        // The base image is the media-accel build minus the Rockchip userspace: the
        // capability still ships, because the accel patch series and the accel/full
        // kconfig live on the kernel axis, not the feature. A base build carries the
        // same kernel (VEPU/VDPU/RGA + NPU drivers) and only omits the ffmpeg-rk / MPP
        // / RGA userspace, which installs later from the media-accel debs.
        let root = repo_root();
        let b = resolve_recipe(&root, "turing-rk1/forky", &Overrides::default()).unwrap();
        assert_eq!(b.suite.as_deref(), Some("forky"));
        assert!(b.features.is_empty(), "a base recipe selects no features");
        // No feature sets `requires_media_accel`, so no userspace/ffmpeg source trees
        // are scheduled and no media userspace lands in the rootfs.
        assert!(b.userspace.is_none(), "base carries no userspace sources");
        assert!(b.ffmpeg.is_none(), "base carries no ffmpeg sources");
        assert!(!b.rootfs_packages.contains(&"ffmpeg-rk".to_string()));
        assert!(b.rootfs_packages.contains(&"openssh-server".to_string()));
        // The kernel is the accel kernel: accel/full is present exactly as on the
        // media-accel build. This split — capability in the kernel, userspace in the
        // feature — is what lets a base image light up transcode later.
        let kernel = b
            .kernel
            .as_ref()
            .unwrap()
            .compiled()
            .expect("the RK1 compiles its kernel");
        assert!(kernel.config_fragments.contains(&"accel/full".to_string()));
        assert_eq!(b.kernel.as_ref().unwrap().id(), "rk3588-mainline-7.1");
    }

    #[test]
    fn a_capability_feature_contributes_its_kernel_patch_series_and_fragment() {
        // The whole point of the feature reaching the kernel: `media-accel-v4l2` needs
        // an out-of-tree RGA driver, so it carries both the series that adds the source
        // and the fragment that compiles it. Neither is on the RK3576 kernel or the
        // board, so a build that does not select the capability gets neither — even
        // though this board does carry a kernel series and an accel fragment of its
        // own, which is what makes the RGA pair's absence meaningful.
        let root = repo_root();
        let base = resolve_recipe(&root, "h96-max-m9/forky", &Overrides::default()).unwrap();
        let base_k = base.kernel.as_ref().unwrap().compiled().unwrap();
        assert_eq!(
            base_k.patch_series,
            vec!["rk3576-fixes".to_string(), "rk3576-npu".to_string()]
        );
        assert!(!base_k
            .config_fragments
            .contains(&"accel/rk3576-rga".to_string()));

        let accel = resolve_recipe(
            &root,
            "h96-max-m9/forky",
            &Overrides {
                features: Some(vec!["media-accel-v4l2".into()]),
                ..Overrides::default()
            },
        )
        .unwrap();
        let k = accel.kernel.as_ref().unwrap().compiled().unwrap();
        // Composed last: after the kernel's own series, then the device's, then the
        // feature's — the same low-to-high order the fragments follow.
        assert_eq!(
            k.patch_series,
            vec![
                "rk3576-fixes".to_string(),
                "rk3576-npu".to_string(),
                "rk3576-rga".to_string()
            ]
        );
        assert_eq!(
            k.config_fragments,
            vec![
                "base/debian-arm64".to_string(),
                "soc/rk3576".to_string(),
                "device/h96-max-m9".to_string(),
                "accel/rk3576-npu".to_string(),
                "accel/rk3576-rga".to_string(),
            ]
        );
    }

    #[test]
    fn a_soc_declaring_only_some_userspace_trees_resolves_just_those() {
        // The SoC's declared trees are its capability statement, and an absent tree is
        // a fact about the hardware: RK3576 has no vendor `mpp_service` for MPP to bind
        // and a panfrost GPU that takes Mesa rather than libmali, so it declares
        // neither — and no graft applies to its ffmpeg either.
        let root = repo_root();
        let b = resolve_recipe(
            &root,
            "h96-max-m9/forky",
            &Overrides {
                features: Some(vec!["media-accel-v4l2".into()]),
                ..Overrides::default()
            },
        )
        .unwrap();
        let us = b
            .userspace
            .as_ref()
            .expect("the capability builds userspace");
        assert!(us.mpp.is_none(), "no vendor mpp_service on this SoC");
        assert!(
            us.libmali.is_none(),
            "the GPU userspace is Mesa, from the mirror"
        );
        assert!(
            us.librga.is_some(),
            "RGA is the one vendor tree this SoC builds"
        );
        let ff = b.ffmpeg.as_ref().expect("the capability builds ffmpeg");
        assert!(ff.rockchip.is_none(), "the base tree builds unmodified");
    }

    #[test]
    fn a_feature_needing_a_compiled_kernel_is_rejected_against_a_distro_one() {
        // A distro-package kernel merges no kconfig and applies no series, so the
        // capability's driver would never be built — the feature would install its
        // userspace against hardware support that is not there. Named for the feature,
        // since the fix is to drop it or change kernel, not to edit the device.
        let feature = crate::feature::Feature {
            description: "test".into(),
            packages: vec![],
            exclude: vec![],
            requires_soc: vec![],
            requires_arch: vec![],
            apt_sources: vec![],
            extra_debs: vec![],
            conflicts: vec![],
            requires_media_accel: false,
            config_fragments: vec!["accel/whatever".into()],
            patch_series: vec![],
            caveats: vec![],
        };
        let selected = [("cap".to_string(), feature)];
        let (frags, series) = crate::feature::kernel_contributions(&selected);
        assert_eq!(frags, vec!["accel/whatever".to_string()]);
        assert!(series.is_empty());
        assert_eq!(
            crate::feature::first_contributing_kernel_input(&selected),
            Some(("cap", "config_fragments"))
        );
        // And the resolve-time rejection, on a synthetic config: a distro kernel plus a
        // feature that contributes a fragment. The feature carries no SoC gate, so the
        // failure is unambiguously this one and not an earlier gate.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        for sub in [
            "arches",
            "socs",
            "boot-methods",
            "devices",
            "kernels",
            "features",
        ] {
            std::fs::create_dir_all(p.join(sub)).unwrap();
        }
        std::fs::write(
            p.join("arches/armv7.toml"),
            "kernel_arch = \"arm\"\n\
             kbuild_image = \"arch/arm/boot/zImage\"\ncross_compile = \"\"\n",
        )
        .unwrap();
        std::fs::write(p.join("base.toml"), "packages = []\nexclude = []\n").unwrap();
        std::fs::write(
            p.join("socs/rk3288.toml"),
            "description = \"soc\"\narch = \"armv7\"\ndt_dir = \"rockchip\"\nmodules = []\n",
        )
        .unwrap();
        std::fs::write(
            p.join("boot-methods/depthcharge.toml"),
            "description = \"dc\"\nkpart_offset = \"12MiB\"\nkpart_size = \"16MiB\"\n\
             kpart_slots = 2\nkpart_priority = 10\n\
             kpart_tries = 5\nkpart_successful = true\ncmdline = \"ro\"\n",
        )
        .unwrap();
        std::fs::write(
            p.join("kernels/k.toml"),
            "flavor = \"distro-package\"\npackage = \"linux-image-armmp\"\n\
             supported_socs = [\"rk3288\"]\n",
        )
        .unwrap();
        std::fs::write(
            p.join("features/cap.toml"),
            "description = \"a capability whose driver is out-of-tree\"\n\
             config_fragments = [\"accel/whatever\"]\n",
        )
        .unwrap();
        std::fs::write(
            p.join("devices/dev.toml"),
            "description = \"d\"\nsoc = \"rk3288\"\nboot_method = \"depthcharge\"\n\
             supported_boot_methods = [\"depthcharge\"]\nkernel_dtb = \"rockchip/d.dtb\"\n\
             device_config_fragments = []\nsupported_kernels = [\"k\"]\n\
             default_kernel = \"k\"\nsupported_suites = [\"*\"]\n\
             default_suite = \"forky\"\ndefault_layout = \"combined\"\n\
             hostname = \"d\"\nimage_size = \"4G\"\n\n[depthcharge]\nboard = \"d\"\n\
             supported_boards = [\"d\"]\n",
        )
        .unwrap();
        let synthetic = ConfigRoot::new(p);
        let err = resolve_device(
            &synthetic,
            "dev",
            &Overrides {
                features: Some(vec!["cap".into()]),
                ..Overrides::default()
            },
        )
        .unwrap_err();
        match err {
            ConfigError::FeatureNeedsCompiledKernel {
                feature,
                kernel,
                what,
            } => {
                assert_eq!(feature, "cap");
                assert_eq!(kernel, "k");
                assert_eq!(what, "config_fragments");
            }
            other => panic!("expected FeatureNeedsCompiledKernel, got {other:?}"),
        }
    }

    #[test]
    fn rk1_trixie_variants_resolve_the_accel_kernel_over_trixie() {
        // The forky/trixie split is purely the Debian userland: every source pin
        // (kernel, u-boot, userspace, ffmpeg, blobs) is suite-independent, so only the
        // rootfs suite changes. Both trixie variants resolve, carry the accel kernel,
        // and differ from their forky siblings only in `suite`.
        let root = repo_root();

        let base = resolve_recipe(&root, "turing-rk1/trixie", &Overrides::default()).unwrap();
        assert_eq!(base.suite.as_deref(), Some("trixie"));
        // The suite that archives the build's `.deb`s follows the image's own, so this
        // image's u-boot deb is produced by trixie's `dpkg` rather than by whatever the
        // device happens to default to — the board's default is forky.
        assert_eq!(base.packaging_suite, "trixie");
        assert_eq!(
            root.device("turing-rk1").unwrap().default_suite,
            "forky",
            "the assertion above only distinguishes the two while these differ"
        );
        assert!(base.features.is_empty());
        assert!(base.userspace.is_none());
        assert!(base
            .kernel
            .as_ref()
            .unwrap()
            .compiled()
            .unwrap()
            .config_fragments
            .contains(&"accel/full".to_string()));

        let accel = resolve_recipe(
            &root,
            "turing-rk1/media-accel-trixie",
            &Overrides::default(),
        )
        .unwrap();
        assert_eq!(accel.suite.as_deref(), Some("trixie"));
        assert_eq!(accel.features, vec!["media-accel-rockchip"]);
        assert!(
            accel.userspace.is_some(),
            "the media-accel variant carries userspace"
        );
        assert!(accel.ffmpeg.is_some());
    }

    #[test]
    fn cli_override_beats_device_default() {
        let root = repo_root();
        let ov = Overrides {
            suite: Some("trixie".to_string()),
            layout: Some(Layout::Split),
            ..Default::default()
        };
        let b = resolve_device(&root, "turing-rk1", &ov).unwrap();
        assert_eq!(b.suite.as_deref(), Some("trixie"));
        assert_eq!(b.layout, Layout::Split);
    }

    #[test]
    fn c201_recipe_resolves_a_depthcharge_board_with_a_distro_kernel() {
        let root = repo_root();
        let b = resolve_recipe(&root, "asus-c201/forky", &Overrides::default()).unwrap();
        assert_eq!(b.arch, Arch::Armv7);
        assert_eq!(b.arch.debian_arch(), "armhf");
        assert_eq!(b.soc, Soc::Rk3288);
        assert_eq!(b.boot_method, BootMethod::Depthcharge);
        assert_eq!(b.suite.as_deref(), Some("forky"));

        // The kernel is Debian's: no source, no fragments, no patches — and the
        // package joins the rootfs set, which is how it gets installed and pinned.
        assert!(!b.compiles_kernel());
        let k = b.kernel.as_ref().unwrap();
        assert_eq!(k.id(), "debian-armmp");
        assert!(k.patch_series().is_empty());
        assert!(k.compiled().is_none());
        assert!(b.rootfs_packages.contains(&"linux-image-armmp".to_string()));

        // The boot half is the kernel slots, with the bits that make the firmware boot
        // one of them. These are the values read back off the image that boots the unit.
        assert!(b.rkbin_boot().is_none(), "this board has no rkbin chain");
        let boot = b.depthcharge_boot().expect("a depthcharge board");
        assert_eq!(
            boot.board, "speedy",
            "the stock profile, which boots both firmwares"
        );
        assert_eq!(boot.kpart.offset, "12MiB");
        assert_eq!(boot.kpart.size, "16MiB");
        assert_eq!(boot.kpart.flags, 0x015A_0000_0000_0000);
        // Two slots, and this is the assertion that keeps kernel upgrades survivable:
        // `depthchargectl` writes the slot it is NOT booted from, so a kernel that fails
        // to come up leaves the previous one intact for the firmware to fall back to. At
        // one slot it would have to overwrite the running kernel in place, and a bad
        // upgrade would need external media to recover.
        assert_eq!(
            boot.kpart.slots, 2,
            "an A/B pair — the spare is the rollback"
        );
        assert_eq!(
            boot.rootfs_offset, "44MiB",
            "derived: behind both 16 MiB slots (12 + 16 + 16), not behind one"
        );
        assert!(
            !boot.cmdline.contains("root="),
            "root= is depthchargectl's to derive"
        );
        // The console gate is signed in with the rest of the cmdline. It has to be:
        // this value lives inside the vboot signature, so it cannot be edited on the
        // device — a board that shipped a verbose console would need a reflash.
        assert!(
            boot.cmdline.contains(CONSOLE_LOGLEVEL_ARG),
            "the console gate reaches a signed cmdline too: {}",
            boot.cmdline
        );

        // A laptop whose primary link is wifi: NetworkManager owns the interfaces, so
        // the base layer's dhcpcd is dropped rather than left to fight it.
        assert!(b.rootfs_packages.contains(&"network-manager".to_string()));
        assert!(!b.rootfs_packages.contains(&"dhcpcd".to_string()));
        assert!(b.rootfs_exclude.contains(&"dhcpcd".to_string()));
        // The boot method brings the tool that signs the kernel, on the build host and
        // on the running board alike.
        assert!(b.rootfs_packages.contains(&"depthcharge-tools".to_string()));

        // The RK3288 has no Rockchip media-accel stack, so nothing pulls those sources.
        assert!(b.userspace.is_none());
        assert!(b.ffmpeg.is_none());
    }

    #[test]
    fn compiling_from_source_is_the_union_of_the_three_things_a_build_can_compile() {
        // The predicate the host preflight is driven by, asserted over the shipped
        // recipes rather than a fixture — the three shapes it has to separate all exist.
        let root = repo_root();
        let compile = |r: &str| {
            resolve_recipe(&root, r, &Overrides::default())
                .unwrap()
                .compiles_from_source()
        };

        // A kernel and a bootloader and the accel stack: every clause true.
        assert!(compile("turing-rk1/media-accel-forky"));
        // No image at all, so no kernel and no userspace — but a bootloader is compiled,
        // which is why this is a union rather than `compiles_kernel`. Getting it wrong
        // here would tell a `deliverable = uboot` operator that their host needs nothing
        // while the build reached for `git` and an overlay.
        let loader = resolve_recipe(&root, "rk3576-generic/loader", &Overrides::default()).unwrap();
        assert!(!loader.compiles_kernel());
        assert!(loader.userspace.is_none());
        assert!(loader.compiles_from_source());
        // Debian's kernel, the board's own firmware, no accel stack: the one shape that
        // asks its host for neither `git` nor an unprivileged overlay.
        assert!(!compile("asus-c201/forky"));
    }

    #[test]
    fn the_libreboot_variant_pairs_its_profile_with_the_slot_that_profile_needs() {
        // A board profile bounds the payload the *firmware* accepts; the slot bounds
        // what the *image* can carry. Selecting the 32 MiB profile without the 32 MiB
        // slot buys nothing, so the two travel together on one device — and the rootfs
        // offset follows the slots rather than being authored beside them.
        let root = repo_root();
        let b = resolve_recipe(&root, "asus-c201-libreboot/forky", &Overrides::default()).unwrap();
        let boot = b.depthcharge_boot().expect("a depthcharge board");
        assert_eq!(boot.board, "speedy-libreboot");
        assert_eq!(
            boot.kpart.size, "32MiB",
            "the device overrides the method's"
        );
        assert_eq!(boot.kpart.offset, "12MiB", "inherited from the method");
        assert_eq!(
            boot.rootfs_offset, "76MiB",
            "derived from the wider slots: 12 + 32 + 32"
        );
        // And the slot spends itself: the wider budget is what lets the initramfs take
        // the compressor that costs bytes and saves boot time.
        assert_eq!(boot.initramfs_compress, InitramfsCompress::Zstd);

        // Everything the variant does not restate is the C201's, including the DTB and
        // the keymap — the point of extending rather than copying.
        let stock = resolve_recipe(&root, "asus-c201/forky", &Overrides::default()).unwrap();
        assert_eq!(
            stock.depthcharge_boot().unwrap().initramfs_compress,
            InitramfsCompress::Xz,
            "a 16 MiB slot still has to trade boot time for margin"
        );
        assert_eq!(b.kernel_dtb, stock.kernel_dtb);
        assert_eq!(b.arch, stock.arch);
        assert_eq!(b.rootfs_packages, stock.rootfs_packages);
    }

    #[test]
    fn a_recipe_selects_a_board_profile_the_device_offers() {
        // The recipe axis A3 added: a profile that needs no geometry of its own is a
        // recipe field, checked against the device's `supported_boards` like a `--board`
        // override. The stock C201 offers both profiles, so it can be asked for either.
        let root = repo_root();
        let recipe: Recipe = toml::from_str(
            "device = \"asus-c201\"\nkernel = \"debian-armmp\"\nsuite = \"forky\"\n\
             board = \"speedy-libreboot\"\nlayout = \"combined\"\nimage_size = \"2G\"\n",
        )
        .unwrap();
        assert_eq!(recipe.board.as_deref(), Some("speedy-libreboot"));

        // And a profile the device does not offer is refused rather than passed to
        // depthchargectl to fail against its own database.
        let ov = Overrides {
            board: Some("mickey".into()),
            ..Default::default()
        };
        match resolve_device(&root, "asus-c201", &ov).unwrap_err() {
            ConfigError::UnknownBoardProfile { board, .. } => assert_eq!(board, "mickey"),
            other => panic!("expected UnknownBoardProfile, got {other}"),
        }
    }

    #[test]
    fn the_trixie_recipe_differs_only_in_the_suite() {
        // One distro-kernel definition serves both releases: the *suite* picks the
        // version (forky 7.1.x, trixie 6.12.x), which is the whole point of not
        // authoring a kernel per release.
        let root = repo_root();
        let b = resolve_recipe(&root, "asus-c201/trixie", &Overrides::default()).unwrap();
        assert_eq!(b.suite.as_deref(), Some("trixie"));
        assert_eq!(b.kernel.as_ref().unwrap().id(), "debian-armmp");
        assert_eq!(b.device, "asus-c201");
    }

    #[test]
    fn a_veyron_sibling_is_a_device_file_and_nothing_else() {
        // The claim the SoC layer exists to make. The Veyron boards share a radio, an
        // initramfs, a network stack and an audio codec; they differ in a DTB, a
        // depthcharge profile, and a hostname. So a new board in the family must resolve
        // to the *same* rootfs as the board that was validated on hardware — if a
        // sibling needed packages of its own, the family layer would be a fiction.
        let root = repo_root();
        let c201 = resolve_recipe(&root, "asus-c201/forky", &Overrides::default()).unwrap();
        let c100p = resolve_recipe(&root, "asus-c100p/forky", &Overrides::default()).unwrap();
        let cs10 =
            resolve_recipe(&root, "asus-chromebit-cs10/forky", &Overrides::default()).unwrap();

        for b in [&c100p, &cs10] {
            assert_eq!(b.rootfs_packages, c201.rootfs_packages);
            assert_eq!(b.rootfs_exclude, c201.rootfs_exclude);
            assert_eq!(b.arch, Arch::Armv7);
            assert_eq!(b.soc, Soc::Rk3288);
            assert_eq!(b.boot_method, BootMethod::Depthcharge);
            // Debian's kernel, so nothing is compiled and no DTB is built: the board's
            // device tree is already upstream.
            assert!(!b.compiles_kernel());
            assert_eq!(b.kernel.as_ref().unwrap().id(), "debian-armmp");
            // Same kernel-partition geometry and cmdline — those are the boot method's,
            // not the board's.
            let boot = b.depthcharge_boot().expect("a depthcharge board");
            let c201_boot = c201.depthcharge_boot().unwrap();
            assert_eq!(boot.kpart.offset, c201_boot.kpart.offset);
            assert_eq!(boot.kpart.flags, c201_boot.kpart.flags);
            assert_eq!(boot.cmdline, c201_boot.cmdline);
        }

        // And the deltas are exactly the three that make it a different board.
        assert_eq!(c100p.kernel_dtb, "rockchip/rk3288-veyron-minnie.dtb");
        assert_eq!(c100p.depthcharge_boot().unwrap().board, "minnie");
        assert_eq!(c100p.hostname, "asus-c100p");

        assert_eq!(cs10.kernel_dtb, "rockchip/rk3288-veyron-mickey.dtb");
        assert_eq!(cs10.depthcharge_boot().unwrap().board, "mickey");
        assert_eq!(cs10.hostname, "asus-chromebit-cs10");
    }

    #[test]
    fn the_chromebit_declares_a_keymap_despite_having_no_keyboard() {
        // Not an oversight, and the reason is the distinction `keymap` is *for*. The
        // field asks "does a console keymap configure anything on this board?", not
        // "does the board ship a keyboard". The Chromebit drives an HDMI console that a
        // USB keyboard is the only way to type at — so a layout is as meaningful there
        // as on a laptop. A headless board (the RK1) is the case that declares none.
        let root = repo_root();
        let cs10 = resolve_device(&root, "asus-chromebit-cs10", &Overrides::default()).unwrap();
        assert_eq!(cs10.keymap.as_ref().map(|k| k.layout.as_str()), Some("us"));

        let rk1 = resolve_device(&root, "turing-rk1", &Overrides::default()).unwrap();
        assert!(rk1.keymap.is_none(), "a headless board defaults no layout");
    }

    #[test]
    fn a_stock_only_board_offers_exactly_one_profile() {
        // depthcharge-tools ships a libreboot profile for `speedy` and for no other
        // Veyron. The two new boards therefore support one profile each, and asking for
        // the C201's is rejected here rather than by firmware that silently declines to
        // boot the image.
        let root = repo_root();
        for (device, profile) in [("asus-c100p", "minnie"), ("asus-chromebit-cs10", "mickey")] {
            let b = resolve_device(&root, device, &Overrides::default()).unwrap();
            assert_eq!(b.depthcharge_boot().unwrap().board, profile);

            let ov = Overrides {
                board: Some("speedy-libreboot".to_string()),
                ..Default::default()
            };
            match resolve_device(&root, device, &ov).unwrap_err() {
                ConfigError::UnknownBoardProfile { supported, .. } => {
                    assert!(supported.contains(profile));
                }
                other => panic!("expected UnknownBoardProfile, got {other:?}"),
            }
        }
    }

    #[test]
    fn the_board_profile_is_selectable_and_validated() {
        let root = repo_root();
        // A unit running libreboot takes the other profile.
        let ov = Overrides {
            board: Some("speedy-libreboot".to_string()),
            ..Default::default()
        };
        let b = resolve_device(&root, "asus-c201", &ov).unwrap();
        assert_eq!(b.depthcharge_boot().unwrap().board, "speedy-libreboot");

        // A profile the device does not support is rejected here rather than producing
        // an image the firmware silently refuses to boot.
        let ov = Overrides {
            board: Some("kevin".to_string()),
            ..Default::default()
        };
        match resolve_device(&root, "asus-c201", &ov).unwrap_err() {
            ConfigError::UnknownBoardProfile {
                device,
                board,
                supported,
            } => {
                assert_eq!(device, "asus-c201");
                assert_eq!(board, "kevin");
                assert!(supported.contains("speedy"));
            }
            other => panic!("expected UnknownBoardProfile, got {other:?}"),
        }
    }

    #[test]
    fn a_depthcharge_board_cannot_split_its_bootloader_off() {
        // `split` puts the bootloader on a different medium from the rootfs. This board
        // has no bootloader of ours, and the firmware finds its kernel by scanning the
        // GPT of the disk it will root from — so there is nothing to split.
        let root = repo_root();
        let ov = Overrides {
            layout: Some(Layout::Split),
            ..Default::default()
        };
        match resolve_device(&root, "asus-c201", &ov).unwrap_err() {
            ConfigError::UnsupportedLayout {
                boot_method,
                layout,
                ..
            } => {
                assert_eq!(boot_method, "depthcharge");
                assert_eq!(layout, "split");
            }
            other => panic!("expected UnsupportedLayout, got {other:?}"),
        }
    }

    #[test]
    fn a_depthcharge_cmdline_may_not_carry_a_percent_or_a_root() {
        // Both rules are the hardware talking. depthchargectl round-trips the computed
        // cmdline through a ConfigParser that rejects a raw `%` outright — no escaping
        // works — and it derives root from /etc/fstab, stripping any root that
        // disagrees. Either mistake yields an image that boots and finds no disk.
        for bad in [
            "console=tty1 root=PARTUUID=%U/PARTNROFF=1",
            "console=tty1 ro root=PARTUUID=1234",
            "console=tty1 kern_guid=%U",
        ] {
            assert!(
                matches!(
                    validate_depthcharge_cmdline(bad),
                    Err(ConfigError::InvalidCmdline { .. })
                ),
                "{bad:?} must be rejected"
            );
        }
        // What the shipped board actually carries.
        assert!(validate_depthcharge_cmdline("console=tty1 rootwait ro panic=30").is_ok());
    }

    #[test]
    fn a_device_kernel_cmdline_is_trimmed_and_shell_safe() {
        // Absent and blank both resolve to empty — the generated cmdline stands alone.
        assert_eq!(validate_kernel_cmdline(None).unwrap(), "");
        assert_eq!(validate_kernel_cmdline(Some("  ")).unwrap(), "");
        // A real value is trimmed and passed through.
        assert_eq!(
            validate_kernel_cmdline(Some(" video=HDMI-A-1:d cpuidle.off=1 ")).unwrap(),
            "video=HDMI-A-1:d cpuidle.off=1"
        );
        // The value lands inside a double-quoted assignment in a shell-sourced file
        // (board.conf), so shell-active characters, control characters, and a
        // second `root=` source of truth are all typed errors at resolve time.
        for bad in [
            "quiet \"splash\"",
            "arg=$(reboot)",
            "arg=`id`",
            "path=a\\b",
            "line1\nline2",
            "ro root=LABEL=other",
        ] {
            assert!(
                matches!(
                    validate_kernel_cmdline(Some(bad)),
                    Err(ConfigError::InvalidCmdline { .. })
                ),
                "{bad:?} must be rejected"
            );
        }
    }

    #[test]
    fn a_device_kernel_cmdline_reaches_the_resolved_build() {
        // The shipped boards declare no extra arguments, so the resolved value is
        // empty — the field's default must not invent one.
        let root = repo_root();
        let b = resolve_device(&root, "turing-rk1", &Overrides::default()).unwrap();
        assert_eq!(b.kernel_cmdline, "");
    }

    #[test]
    fn unknown_kernel_is_rejected() {
        let root = repo_root();
        let ov = Overrides {
            kernel: Some("rk3588-mainline-9.9".to_string()),
            ..Default::default()
        };
        let err = resolve_device(&root, "turing-rk1", &ov).unwrap_err();
        assert!(matches!(err, ConfigError::UnknownKernelForDevice { .. }));
    }

    #[test]
    fn unsupported_boot_method_is_rejected() {
        let root = repo_root();
        let ov = Overrides {
            boot_method: Some(BootMethod::Depthcharge),
            ..Default::default()
        };
        let err = resolve_device(&root, "turing-rk1", &ov).unwrap_err();
        assert!(matches!(err, ConfigError::UnsupportedBootMethod { .. }));
    }

    #[test]
    fn invalid_image_size_is_rejected_at_resolve() {
        let root = repo_root();
        // A typo'd size fails at resolve, not deep in the image stage.
        let ov = Overrides {
            image_size: Some("2GB!".to_string()),
            ..Default::default()
        };
        assert!(matches!(
            resolve_device(&root, "turing-rk1", &ov).unwrap_err(),
            ConfigError::InvalidSize { .. }
        ));
        // A zero-size image is not buildable.
        let ov = Overrides {
            image_size: Some("0".to_string()),
            ..Default::default()
        };
        assert!(matches!(
            resolve_device(&root, "turing-rk1", &ov).unwrap_err(),
            ConfigError::InvalidSize { .. }
        ));
    }

    #[test]
    fn duplicate_feature_is_rejected() {
        let root = repo_root();
        let ov = Overrides {
            features: Some(vec![
                "media-accel-rockchip".to_string(),
                "media-accel-rockchip".to_string(),
            ]),
            ..Default::default()
        };
        assert!(matches!(
            resolve_device(&root, "turing-rk1", &ov).unwrap_err(),
            ConfigError::DuplicateFeature { .. }
        ));
    }

    #[test]
    fn missing_device_is_not_found() {
        let root = repo_root();
        let err = resolve_device(&root, "no-such-device", &Overrides::default()).unwrap_err();
        assert!(matches!(err, ConfigError::NotFound { kind: "device", .. }));
    }

    #[test]
    fn each_boot_method_requires_only_its_own_fields() {
        // The whole point of the tagged layer: a board is asked for what its boot
        // method reads, and nothing else. The RK1 must supply a u-boot defconfig and an
        // rkbin blob set; the C201 must supply neither, and supplies a board profile
        // instead. Resolving both proves each requirement is scoped, and the reverse —
        // omitting a *required* field — is covered by MissingBootField below.
        let root = repo_root();
        let rk1 = resolve_device(&root, "turing-rk1", &Overrides::default()).unwrap();
        let rk1_boot = rk1.rkbin_boot().unwrap();
        assert!(!rk1_boot.uboot_defconfig.is_empty());
        assert!(!rk1_boot.rkbin.atf.is_empty());

        let c201 = resolve_device(&root, "asus-c201", &Overrides::default()).unwrap();
        assert!(c201.depthcharge_boot().is_some());
        // And the C201's device file genuinely carries neither — this is not an
        // inherited default quietly filling in.
        let device = root.device("asus-c201").unwrap();
        assert!(device.uboot_defconfig.is_none());
        assert_eq!(device.rkbin, RkbinLayer::default());
    }

    #[test]
    fn a_board_missing_its_boot_methods_required_field_is_a_typed_error() {
        // The error names the method that wants the field, not "every device needs
        // this" — because it does not.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        for sub in ["arches", "socs", "boot-methods", "devices", "kernels"] {
            std::fs::create_dir_all(p.join(sub)).unwrap();
        }
        std::fs::write(
            p.join("arches/armv7.toml"),
            "kernel_arch = \"arm\"\n\
             kbuild_image = \"arch/arm/boot/zImage\"\ncross_compile = \"\"\n",
        )
        .unwrap();
        std::fs::write(p.join("base.toml"), "packages = []\nexclude = []\n").unwrap();
        std::fs::write(
            p.join("socs/rk3288.toml"),
            "description = \"soc\"\narch = \"armv7\"\ndt_dir = \"rockchip\"\nmodules = []\n",
        )
        .unwrap();
        std::fs::write(
            p.join("boot-methods/depthcharge.toml"),
            "description = \"dc\"\nkpart_offset = \"12MiB\"\nkpart_size = \"16MiB\"\n\
             kpart_slots = 2\nkpart_priority = 10\n\
             kpart_tries = 5\nkpart_successful = true\ncmdline = \"ro\"\n",
        )
        .unwrap();
        std::fs::write(
            p.join("kernels/k.toml"),
            "flavor = \"distro-package\"\npackage = \"linux-image-armmp\"\n\
             supported_socs = [\"rk3288\"]\n",
        )
        .unwrap();
        // A depthcharge board with no [depthcharge] block: nothing would know which
        // firmware to sign for.
        std::fs::write(
            p.join("devices/dev.toml"),
            "description = \"d\"\nsoc = \"rk3288\"\nboot_method = \"depthcharge\"\n\
             supported_boot_methods = [\"depthcharge\"]\nkernel_dtb = \"rockchip/d.dtb\"\n\
             device_config_fragments = []\nsupported_kernels = [\"k\"]\ndefault_kernel = \"k\"\n\
             supported_suites = [\"*\"]\ndefault_suite = \"forky\"\ndefault_layout = \"combined\"\nhostname = \"d\"\n\
             image_size = \"4G\"\n",
        )
        .unwrap();
        let root = ConfigRoot::new(p);
        match resolve_device(&root, "dev", &Overrides::default()).unwrap_err() {
            ConfigError::MissingBootField {
                device,
                boot_method,
                what,
            } => {
                assert_eq!(device, "dev");
                assert_eq!(boot_method, "depthcharge");
                assert!(what.contains("depthcharge"));
            }
            other => panic!("expected MissingBootField, got {other:?}"),
        }
    }

    #[test]
    fn a_distro_kernel_rejects_the_device_inputs_it_could_never_compile() {
        // A board device tree and board kconfig fragments are compile inputs. Paired
        // with a kernel that compiles nothing, they are not merely redundant — the DTB
        // would never be built, and the board would read as configured and boot as
        // broken. So it is an error, naming the field.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        for sub in ["arches", "socs", "boot-methods", "devices", "kernels"] {
            std::fs::create_dir_all(p.join(sub)).unwrap();
        }
        std::fs::write(
            p.join("arches/armv7.toml"),
            "kernel_arch = \"arm\"\n\
             kbuild_image = \"arch/arm/boot/zImage\"\ncross_compile = \"\"\n",
        )
        .unwrap();
        std::fs::write(p.join("base.toml"), "packages = []\nexclude = []\n").unwrap();
        std::fs::write(
            p.join("socs/rk3288.toml"),
            "description = \"soc\"\narch = \"armv7\"\ndt_dir = \"rockchip\"\nmodules = []\n",
        )
        .unwrap();
        std::fs::write(
            p.join("boot-methods/depthcharge.toml"),
            "description = \"dc\"\nkpart_offset = \"12MiB\"\nkpart_size = \"16MiB\"\n\
             kpart_slots = 2\nkpart_priority = 10\n\
             kpart_tries = 5\nkpart_successful = true\ncmdline = \"ro\"\n",
        )
        .unwrap();
        std::fs::write(
            p.join("kernels/k.toml"),
            "flavor = \"distro-package\"\npackage = \"linux-image-armmp\"\n\
             supported_socs = [\"rk3288\"]\n",
        )
        .unwrap();
        std::fs::write(
            p.join("devices/dev.toml"),
            "description = \"d\"\nsoc = \"rk3288\"\nboot_method = \"depthcharge\"\n\
             supported_boot_methods = [\"depthcharge\"]\nkernel_dtb = \"rockchip/d.dtb\"\n\
             device_config_fragments = [\"device/d\"]\nsupported_kernels = [\"k\"]\n\
             default_kernel = \"k\"\nsupported_suites = [\"*\"]\n\
             default_suite = \"forky\"\ndefault_layout = \"combined\"\n\
             hostname = \"d\"\nimage_size = \"4G\"\n\n[depthcharge]\nboard = \"d\"\n\
             supported_boards = [\"d\"]\n",
        )
        .unwrap();
        let root = ConfigRoot::new(p);
        match resolve_device(&root, "dev", &Overrides::default()).unwrap_err() {
            ConfigError::DistroKernelCompilesNothing {
                device,
                kernel,
                what,
            } => {
                assert_eq!(device, "dev");
                assert_eq!(kernel, "k");
                assert_eq!(what, "device_config_fragments");
            }
            other => panic!("expected DistroKernelCompilesNothing, got {other:?}"),
        }
    }

    /// A valid `kmods/<name>.toml`, mutated per case in the rejection tests below.
    fn valid_kmod() -> KmodLayer {
        KmodLayer {
            description: "AIC8800 SDIO Wi-Fi".into(),
            git: "https://github.com/radxa-pkg/aic8800.git".into(),
            git_ref: "main".into(),
            subdir: "src/SDIO/driver_fw/driver/aic8800".into(),
            patch_dir: "debian/patches".into(),
            repo_patches: vec!["fix-sdio-firmware-path.patch".into()],
            local_patches: vec!["0001-sdio-linux-7.1.patch".into()],
            make_args: vec!["CONFIG_FDRV_NO_REG_SDIO=y".into()],
            modules: vec!["aic8800_bsp".into(), "aic8800_fdrv".into()],
            firmware: Some(KmodFirmware {
                subdir: "src/SDIO/driver_fw/fw/aic8800D80".into(),
                install: "usr/lib/firmware/aic8800_fw/SDIO/aic8800D80".into(),
            }),
        }
    }

    #[test]
    fn validate_kmod_accepts_a_well_formed_layer() {
        validate_kmod(&valid_kmod(), "aic8800").unwrap();
    }

    #[test]
    fn validate_kmod_rejects_malformed_layers() {
        // Each mutation trips exactly one rule; the error names the offending kmod file.
        let escaping_subdir = KmodLayer {
            subdir: "../evil".into(),
            ..valid_kmod()
        };
        let non_bare_patch = KmodLayer {
            repo_patches: vec!["sub/dir.patch".into()],
            ..valid_kmod()
        };
        // A local patch is joined under `kmods/<name>/patches/`, so it is bare too — an
        // absolute path would reach a file outside the kmod's own directory.
        let escaping_local = KmodLayer {
            local_patches: vec!["/etc/shadow".into()],
            ..valid_kmod()
        };
        let bad_make_arg = KmodLayer {
            make_args: vec!["rm -rf /".into()],
            ..valid_kmod()
        };
        // An option-shaped entry is refused for the same reason a defconfig name is:
        // make parses it as an option rather than as a variable assignment, and `-f`
        // would point the build at a makefile of the author's choosing.
        let option_make_arg = KmodLayer {
            make_args: vec!["-f/tmp/evil.mk=1".into()],
            ..valid_kmod()
        };
        let escaping_fw = KmodLayer {
            firmware: Some(KmodFirmware {
                subdir: "../fw".into(),
                install: "usr/lib/firmware".into(),
            }),
            ..valid_kmod()
        };
        let absolute_fw_install = KmodLayer {
            firmware: Some(KmodFirmware {
                subdir: "fw".into(),
                install: "/usr/lib/firmware".into(),
            }),
            ..valid_kmod()
        };

        for kmod in [
            &escaping_subdir,
            &non_bare_patch,
            &escaping_local,
            &bad_make_arg,
            &option_make_arg,
            &escaping_fw,
            &absolute_fw_install,
        ] {
            match validate_kmod(kmod, "aic8800").unwrap_err() {
                ConfigError::InvalidKmod { kmod, .. } => assert_eq!(kmod, "aic8800"),
                other => panic!("expected InvalidKmod, got {other:?}"),
            }
        }

        // The name is the file stem, and it becomes a deb name — an overlay shipping
        // `kmods/Bad_Name.toml` is caught here rather than at `dpkg-deb`.
        match validate_kmod(&valid_kmod(), "Bad_Name").unwrap_err() {
            ConfigError::InvalidKmod { kmod, why } => {
                assert_eq!(kmod, "Bad_Name");
                assert!(why.contains("dpkg-package-safe"), "{why}");
            }
            other => panic!("expected InvalidKmod, got {other:?}"),
        }
    }

    /// The smallest config root that resolves a kmod: one `kmods/<name>.toml` per entry.
    fn kmod_root(files: &[(&str, &str)]) -> (tempfile::TempDir, ConfigRoot) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("kmods")).unwrap();
        for (name, body) in files {
            std::fs::write(dir.path().join(format!("kmods/{name}.toml")), body).unwrap();
        }
        let root = ConfigRoot::new(dir.path());
        (dir, root)
    }

    /// A minimal kmod layer whose only variable is the git ref, so two entries are
    /// distinguishable in the resolved order.
    fn kmod_toml(git_ref: &str) -> String {
        format!(
            "description = \"d\"\ngit = \"https://example.invalid/d.git\"\n\
             ref = \"{git_ref}\"\nsubdir = \"src\"\n"
        )
    }

    #[test]
    fn kmods_resolve_in_the_order_the_device_named_them() {
        // The device supplies only the order and the names; every field comes from the
        // kmod's own layer, and the stem lands on the resolved struct as its identity.
        let (_d, root) = kmod_root(&[("beta", &kmod_toml("v2")), ("alpha", &kmod_toml("v1"))]);
        let names = ["beta".to_string(), "alpha".to_string()];
        let resolved = resolve_kmods(&root, &names, "dev").unwrap();
        assert_eq!(
            resolved.iter().map(|k| k.name.as_str()).collect::<Vec<_>>(),
            ["beta", "alpha"]
        );
        assert_eq!(resolved[0].git_ref, "v2");
        assert_eq!(resolved[1].git_ref, "v1");
        // `patch_dir` defaults rather than being restated in every kmod file.
        assert_eq!(resolved[0].patch_dir, "debian/patches");

        // An empty list is always fine — the board carries no out-of-tree module.
        assert!(resolve_kmods(&root, &[], "dev").unwrap().is_empty());
    }

    #[test]
    fn a_kmod_named_twice_is_rejected() {
        // One kmod is one build node, one deb, and one lock pin, so a repeat has no
        // meaning to honour.
        let (_d, root) = kmod_root(&[("alpha", &kmod_toml("v1"))]);
        let names = ["alpha".to_string(), "alpha".to_string()];
        match resolve_kmods(&root, &names, "dev").unwrap_err() {
            ConfigError::DuplicateKmod { device, kmod } => {
                assert_eq!(device, "dev");
                assert_eq!(kmod, "alpha");
            }
            other => panic!("expected DuplicateKmod, got {other:?}"),
        }
    }

    #[test]
    fn an_unknown_kmod_names_the_kmod_it_could_not_find() {
        // The fault is a device naming a driver that does not exist, so the error says
        // which kmod — not which device file mentioned it.
        let (_d, root) = kmod_root(&[("alpha", &kmod_toml("v1"))]);
        match resolve_kmods(&root, &["nope".to_string()], "dev").unwrap_err() {
            ConfigError::NotFound { kind, name, .. } => {
                assert_eq!(kind, "kmod");
                assert_eq!(name, "nope");
            }
            other => panic!("expected NotFound, got {other:?}"),
        }
        // A traversing name never becomes a path join.
        match resolve_kmods(&root, &["../../etc/passwd".to_string()], "dev").unwrap_err() {
            ConfigError::InvalidName { kind, .. } => assert_eq!(kind, "kmod"),
            other => panic!("expected InvalidName, got {other:?}"),
        }
    }

    /// A slot count outside `1..=MAX_KPART_SLOTS` is a typed error, not a clamp. Zero
    /// slots would leave the firmware nothing to boot at all; past the cap is a typo,
    /// and the slots run back to back, so a large one would quietly march into the
    /// rootfs. Neither should be silently repaired into something that builds.
    #[test]
    fn a_kernel_slot_count_out_of_range_is_a_typed_error() {
        for bad in [0, crate::chromeos::MAX_KPART_SLOTS + 1] {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path();
            write_minimal_depthcharge_root(p, bad);
            match resolve_device(&ConfigRoot::new(p), "dev", &Overrides::default()).unwrap_err() {
                ConfigError::InvalidKpartSlots { value, max } => {
                    assert_eq!(value, bad);
                    assert_eq!(max, crate::chromeos::MAX_KPART_SLOTS);
                }
                other => panic!("expected InvalidKpartSlots for {bad}, got {other:?}"),
            }
        }
        // The bounds themselves resolve: one slot (no spare) and the cap both build.
        for ok in [1, crate::chromeos::MAX_KPART_SLOTS] {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path();
            write_minimal_depthcharge_root(p, ok);
            let b = resolve_device(&ConfigRoot::new(p), "dev", &Overrides::default())
                .unwrap_or_else(|e| panic!("{ok} slots should resolve, got {e:?}"));
            assert_eq!(b.depthcharge_boot().unwrap().kpart.slots, ok);
        }
    }

    /// The smallest config tree that resolves a depthcharge board, parameterized on the
    /// slot count — enough layers for `resolve_device` to reach the boot method.
    fn write_minimal_depthcharge_root(p: &std::path::Path, slots: u8) {
        for sub in ["arches", "socs", "boot-methods", "devices", "kernels"] {
            std::fs::create_dir_all(p.join(sub)).unwrap();
        }
        std::fs::write(
            p.join("arches/armv7.toml"),
            "kernel_arch = \"arm\"\n\
             kbuild_image = \"arch/arm/boot/zImage\"\ncross_compile = \"\"\n",
        )
        .unwrap();
        std::fs::write(p.join("base.toml"), "packages = []\nexclude = []\n").unwrap();
        std::fs::write(
            p.join("socs/rk3288.toml"),
            "description = \"soc\"\narch = \"armv7\"\ndt_dir = \"rockchip\"\nmodules = []\n",
        )
        .unwrap();
        std::fs::write(
            p.join("boot-methods/depthcharge.toml"),
            format!(
                "description = \"dc\"\nkpart_offset = \"12MiB\"\nkpart_size = \"16MiB\"\n\
                 kpart_slots = {slots}\nkpart_priority = 10\n\
                 kpart_tries = 5\nkpart_successful = true\ncmdline = \"ro\"\n"
            ),
        )
        .unwrap();
        std::fs::write(
            p.join("kernels/k.toml"),
            "flavor = \"distro-package\"\npackage = \"linux-image-armmp\"\n\
             supported_socs = [\"rk3288\"]\n",
        )
        .unwrap();
        std::fs::write(
            p.join("devices/dev.toml"),
            "description = \"d\"\nsoc = \"rk3288\"\nboot_method = \"depthcharge\"\n\
             supported_boot_methods = [\"depthcharge\"]\nkernel_dtb = \"rockchip/d.dtb\"\n\
             device_config_fragments = []\nsupported_kernels = [\"k\"]\ndefault_kernel = \"k\"\n\
             supported_suites = [\"*\"]\ndefault_suite = \"forky\"\ndefault_layout = \"combined\"\nhostname = \"d\"\n\
             image_size = \"4G\"\n\n[depthcharge]\nboard = \"speedy\"\n\
             supported_boards = [\"speedy\"]\n",
        )
        .unwrap();
    }

    #[test]
    fn a_boot_method_layer_rejects_another_methods_fields() {
        // The variant is chosen by the filename, so a raw-gap offset in the depthcharge
        // layer is an *unknown field* — a parse error naming the file, not a value
        // silently carried into a build with no raw gap to write it to.
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path();
        std::fs::create_dir_all(p.join("boot-methods")).unwrap();
        std::fs::write(
            p.join("boot-methods/depthcharge.toml"),
            "description = \"dc\"\nkpart_offset = \"12MiB\"\nkpart_size = \"16MiB\"\n\
             kpart_slots = 2\nkpart_priority = 10\n\
             kpart_tries = 5\nkpart_successful = true\ncmdline = \"ro\"\n\
             idbloader_offset = \"32KiB\"\n",
        )
        .unwrap();
        let root = ConfigRoot::new(p);
        let err = root.boot_method(BootMethod::Depthcharge).unwrap_err();
        assert!(
            matches!(err, ConfigError::Parse { .. })
                && err.to_string().contains("idbloader_offset"),
            "expected a parse error naming the stray field, got: {err}"
        );
    }

    #[test]
    fn base_resolution_selects_no_media_accel_sources() {
        // A plain device resolution (no recipe, hence no features) builds no
        // transcode stack, so it carries neither userspace nor ffmpeg sources even
        // though the RK3588 SoC layer supplies them — sources ride the feature, not
        // the SoC.
        let root = repo_root();
        let b = resolve_device(&root, "turing-rk1", &Overrides::default()).unwrap();
        assert!(b.features.is_empty());
        assert!(b.userspace.is_none());
        assert!(b.ffmpeg.is_none());
    }

    /// Build a feature carrying a set of apt sources, for the merge tests.
    fn feat_with_sources(sources: Vec<AptSource>) -> crate::feature::Feature {
        crate::feature::Feature {
            config_fragments: vec![],
            patch_series: vec![],
            description: "t".into(),
            packages: vec![],
            exclude: vec![],
            requires_soc: vec![],
            requires_arch: vec![],
            apt_sources: sources,
            extra_debs: vec![],
            conflicts: vec![],
            requires_media_accel: false,
            caveats: vec![],
        }
    }

    fn src(name: &str, host: &str) -> AptSource {
        AptSource {
            name: name.into(),
            uri: format!("https://{host}.example/debian"),
            suite: "trixie".into(),
            components: vec!["main".into()],
            signed_by: "k.gpg".into(),
        }
    }

    #[test]
    fn apt_sources_reject_line_structure_injection() {
        // The rendered `deb [signed-by=…] <uri> <suite> <components…>` line is
        // positional, so whitespace / brackets / newlines in a line field — or a
        // non-http(s) transport — must fail at resolve time, naming the field. The
        // two fields that become *file names* are held to a tighter rule still; see
        // `apt_sources_reject_names_that_are_not_portable_file_stems`.
        let with = |mutate: &dyn Fn(&mut AptSource)| {
            let mut s = src("vendor", "repo");
            mutate(&mut s);
            s
        };
        for (field, source) in [
            ("uri", with(&|s| s.uri = "https://repo.example/a b".into())),
            ("uri", with(&|s| s.uri = "file:///etc/apt".into())),
            ("suite", with(&|s| s.suite = "tri xie".into())),
            ("suite", with(&|s| s.suite = "trixie] [trusted=yes".into())),
            ("suite", with(&|s| s.suite = "trixie\nmain".into())),
            ("components", with(&|s| s.components = vec![])),
            ("components", with(&|s| s.components = vec!["ma in".into()])),
        ] {
            let feat = ("app".to_string(), feat_with_sources(vec![source]));
            match merge_apt_sources(&[feat]).unwrap_err() {
                ConfigError::AptSourceBadField { field: f, .. } => {
                    assert_eq!(f, field, "wrong field named");
                }
                other => panic!("{field}: expected AptSourceBadField, got {other:?}"),
            }
        }
        // An exact-path repo (suite ending in `/`) legitimately has no components.
        let exact = with(&|s| {
            s.suite = "./".into();
            s.components = vec![];
        });
        let feat = ("app".to_string(), feat_with_sources(vec![exact]));
        assert_eq!(merge_apt_sources(&[feat]).unwrap().len(), 1);
    }

    /// `name` and `signed_by` become file names, so both are held to the portable
    /// stem set rather than only to the line grammar: a separator or dot segment in
    /// either would place a `sources.list.d` entry, or read a trust anchor, outside
    /// the directory that holds it.
    #[test]
    fn apt_sources_reject_names_that_are_not_portable_file_stems() {
        let with = |mutate: &dyn Fn(&mut AptSource)| {
            let mut s = src("vendor", "repo");
            mutate(&mut s);
            s
        };
        for (field, source) in [
            ("name", with(&|s| s.name = "je/llyfin".into())),
            ("name", with(&|s| s.name = "..".into())),
            ("name", with(&|s| s.name = ".".into())),
            ("name", with(&|s| s.name = String::new())),
            // Accepted by the line grammar (no whitespace, no brackets) but not a
            // portable stem — and `a:b` is exactly the shape that a sanitizing map
            // would fold onto `a-b`.
            ("name", with(&|s| s.name = "a:b".into())),
            ("name", with(&|s| s.name = "vendor+extra".into())),
            (
                "signed_by",
                with(&|s| s.signed_by = "../../etc/host.gpg".into()),
            ),
            (
                "signed_by",
                with(&|s| s.signed_by = "/etc/ssl/host.gpg".into()),
            ),
            ("signed_by", with(&|s| s.signed_by = "sub/k.gpg".into())),
            ("signed_by", with(&|s| s.signed_by = "..".into())),
            ("signed_by", with(&|s| s.signed_by = String::new())),
        ] {
            let feat = ("app".to_string(), feat_with_sources(vec![source.clone()]));
            match merge_apt_sources(&[feat]).unwrap_err() {
                ConfigError::AptSourceBadField { field: f, .. } => {
                    assert_eq!(f, field, "wrong field named for {source:?}");
                }
                other => panic!("{field}: expected AptSourceBadField, got {other:?}"),
            }
        }
    }

    /// With `name` held to the stem set, two sources that a sanitizing map would
    /// collapse onto one file cannot both resolve — the one that is not a stem is
    /// rejected where it is authored, so the dedup key and the file it is written as
    /// stay the same string.
    #[test]
    fn apt_source_names_that_would_collide_as_file_stems_cannot_both_resolve() {
        let legal = (
            "app-a".to_string(),
            feat_with_sources(vec![src("a-b", "u1")]),
        );
        let folds_onto_it = (
            "app-b".to_string(),
            feat_with_sources(vec![src("a:b", "u2")]),
        );
        // On its own, the portable one resolves.
        assert_eq!(
            merge_apt_sources(std::slice::from_ref(&legal))
                .unwrap()
                .len(),
            1
        );
        // Its would-be twin never joins it under a different key.
        assert!(matches!(
            merge_apt_sources(&[legal, folds_onto_it]).unwrap_err(),
            ConfigError::AptSourceBadField { field: "name", .. }
        ));
    }

    #[test]
    fn apt_sources_dedup_identical_and_reject_clashes() {
        let a = (
            "app-a".to_string(),
            feat_with_sources(vec![src("jellyfin", "u1")]),
        );
        // Identical duplicate collapses to one entry.
        let a2 = (
            "app-a2".to_string(),
            feat_with_sources(vec![src("jellyfin", "u1")]),
        );
        let merged = merge_apt_sources(&[a.clone(), a2]).unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].name, "jellyfin");

        // Same name, different URI is a hard clash.
        let b = (
            "app-b".to_string(),
            feat_with_sources(vec![src("jellyfin", "u2")]),
        );
        assert!(matches!(
            merge_apt_sources(&[a, b]).unwrap_err(),
            ConfigError::ConflictingAptSource { .. }
        ));
    }

    #[test]
    fn apt_sources_union_preserves_first_appearance_order() {
        let a = (
            "app-a".to_string(),
            feat_with_sources(vec![src("one", "u1")]),
        );
        let b = (
            "app-b".to_string(),
            feat_with_sources(vec![src("two", "u2")]),
        );
        let merged = merge_apt_sources(&[a, b]).unwrap();
        assert_eq!(
            merged.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["one", "two"]
        );
    }

    /// The three scopes join in order of increasing specificity, tagged with the
    /// layer that stated each — checked against the shipped config, so the ordering
    /// rule is verified against real caveats rather than a fixture that agrees with
    /// it by construction.
    #[test]
    fn caveats_join_silicon_then_board_then_build_point() {
        let root = repo_root();
        // A libre C201: the SoC states an HDMI limit, the board a display-controller
        // one, and the recipe the radio that linux-libre puts out of reach.
        let build = resolve_recipe(&root, "asus-c201/libre-forky", &Overrides::default()).unwrap();
        let scopes: Vec<CaveatScope> = build.caveats.iter().map(|c| c.scope).collect();
        assert_eq!(
            scopes,
            [CaveatScope::Soc, CaveatScope::Device, CaveatScope::Recipe],
            "{:#?}",
            build.caveats
        );
        // Sorted by scope is the same as the order they are in, which is what makes
        // "silicon first" a property of the join and not of how these files happen to
        // be written.
        let mut sorted = scopes.clone();
        sorted.sort();
        assert_eq!(scopes, sorted);

        // The recipe's own caveat is the recipe's, and no sibling recipe on the same
        // board picks it up.
        let sibling =
            resolve_recipe(&root, "asus-c201/mainline-forky", &Overrides::default()).unwrap();
        assert!(
            sibling
                .caveats
                .iter()
                .all(|c| c.scope != CaveatScope::Recipe),
            "{:#?}",
            sibling.caveats
        );
        // ...while the hardware's carry over unchanged, because they are the board's.
        assert_eq!(
            sibling.caveats.iter().map(|c| &c.text).collect::<Vec<_>>(),
            build
                .caveats
                .iter()
                .filter(|c| c.scope != CaveatScope::Recipe)
                .map(|c| &c.text)
                .collect::<Vec<_>>()
        );
    }
}

/// Fixture-based resolution tests: a minimal config root written to a
/// tempdir so the pure merge/precedence/exclude algebra is exercised directly,
/// not through the shipped layers (whose edits would otherwise break these
/// tests). `soc = rk3588`, `arch = arm64`, `boot-method = rockchip-rkbin` are the
/// only enum-constrained names; everything else is synthetic.
#[cfg(test)]
mod fixture_tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    /// One synthetic feature: name plus its package and exclude sets.
    struct Feat {
        name: &'static str,
        packages: &'static [&'static str],
        exclude: &'static [&'static str],
    }

    /// A recipe's authored axes (each `Some` becomes a TOML key); drives the
    /// CLI-beats-recipe precedence tests through [`resolve_recipe`].
    #[derive(Default)]
    struct RecipeSpec {
        kernel: Option<&'static str>,
        suite: Option<&'static str>,
        layout: Option<&'static str>,
        features: &'static [&'static str],
        image_size: Option<&'static str>,
        /// Raw TOML appended to the recipe, for the `[[data_volumes]]` table array.
        data_volumes: &'static str,
    }

    /// A complete config tree with buildable defaults; a test sets only the axes
    /// it exercises via struct-update syntax.
    struct Tree {
        base_packages: &'static [&'static str],
        base_exclude: &'static [&'static str],
        soc_packages: &'static [&'static str],
        soc_exclude: &'static [&'static str],
        /// Nonfree firmware the SoC declares, which only a non-libre build merges.
        soc_nonfree: &'static [&'static str],
        bm_packages: &'static [&'static str],
        bm_exclude: &'static [&'static str],
        device_packages: &'static [&'static str],
        device_exclude: &'static [&'static str],
        /// Nonfree firmware the device declares, under the same gate.
        device_nonfree: &'static [&'static str],
        /// Write `libre = true` on every kernel definition this tree offers.
        libre: bool,
        supported_kernels: &'static [&'static str],
        default_kernel: &'static str,
        /// Defaults to the `*` wildcard so a test exercising some other axis need not
        /// restate the suite list; the suite-axis tests set it explicitly.
        supported_suites: &'static [&'static str],
        default_suite: &'static str,
        default_layout: &'static str,
        image_size: &'static str,
        features: Vec<Feat>,
        recipe: Option<RecipeSpec>,
    }

    impl Default for Tree {
        fn default() -> Self {
            Tree {
                base_packages: &[],
                base_exclude: &[],
                soc_packages: &[],
                soc_exclude: &[],
                soc_nonfree: &[],
                bm_packages: &[],
                bm_exclude: &[],
                device_packages: &[],
                device_exclude: &[],
                device_nonfree: &[],
                libre: false,
                supported_kernels: &["k1"],
                default_kernel: "k1",
                supported_suites: &["*"],
                default_suite: "forky",
                default_layout: "combined",
                image_size: "2G",
                features: Vec::new(),
                recipe: None,
            }
        }
    }

    /// Format string slices as a TOML array literal (`["a", "b"]`).
    fn arr(items: &[&str]) -> String {
        let inner = items
            .iter()
            .map(|s| format!("{s:?}"))
            .collect::<Vec<_>>()
            .join(", ");
        format!("[{inner}]")
    }

    impl Tree {
        /// Materialize the tree into a fresh tempdir and return it (kept alive by
        /// the caller so the files outlive resolution).
        fn write(&self) -> TempDir {
            let dir = tempfile::tempdir().unwrap();
            let p = dir.path();
            for sub in [
                "arches",
                "socs",
                "boot-methods",
                "devices",
                "kernels",
                "features",
                "recipes",
            ] {
                fs::create_dir_all(p.join(sub)).unwrap();
            }

            fs::write(
                p.join("arches/arm64.toml"),
                "kernel_arch = \"arm64\"\n\
                 kbuild_image = \"arch/arm64/boot/Image\"\ncross_compile = \"\"\n",
            )
            .unwrap();

            fs::write(
                p.join("base.toml"),
                format!(
                    "packages = {}\nexclude = {}\n",
                    arr(self.base_packages),
                    arr(self.base_exclude)
                ),
            )
            .unwrap();

            let src = "[userspace.mpp]\ngit = \"x\"\nref = \"y\"\n\
                       [userspace.librga]\ngit = \"x\"\nref = \"y\"\n\
                       [userspace.libmali]\ngit = \"x\"\nref = \"y\"\n\
                       [ffmpeg.base]\ngit = \"x\"\nref = \"y\"\n\
                       [ffmpeg.rockchip]\ngit = \"x\"\nref = \"y\"\n";
            fs::write(
                p.join("socs/rk3588.toml"),
                format!(
                    "description = \"soc\"\narch = \"arm64\"\ndt_dir = \"rockchip\"\n\
                     modules = []\npackages = {}\nexclude = {}\n\
                     nonfree_firmware_packages = {}\n{src}",
                    arr(self.soc_packages),
                    arr(self.soc_exclude),
                    arr(self.soc_nonfree)
                ),
            )
            .unwrap();

            fs::write(
                p.join("boot-methods/rockchip-rkbin.toml"),
                format!(
                    "description = \"bm\"\nuboot_source = \"x\"\nuboot_ref = \"v1\"\n\
                     idbloader_offset = \"32KiB\"\nuboot_itb_offset = \"8MiB\"\nrootfs_offset = \"16MiB\"\n\
                     packages = {}\nexclude = {}\n",
                    arr(self.bm_packages),
                    arr(self.bm_exclude)
                ),
            )
            .unwrap();

            fs::write(
                p.join("devices/dev.toml"),
                format!(
                    "description = \"dev\"\nsoc = \"rk3588\"\nboot_method = \"rockchip-rkbin\"\n\
                     supported_boot_methods = [\"rockchip-rkbin\"]\nuboot_defconfig = \"d_defconfig\"\n\
                     kernel_dtb = \"rockchip/d.dtb\"\ndevice_config_fragments = []\n\
                     supported_kernels = {}\ndefault_kernel = {:?}\n\
                     supported_suites = {}\ndefault_suite = {:?}\n\
                     default_layout = {:?}\nhostname = \"dev\"\nimage_size = {:?}\n\
                     packages = {}\nexclude = {}\nnonfree_firmware_packages = {}\n\
                     \n[rkbin]\natf = \"atf.elf\"\ntpl = \"tpl.bin\"\n",
                    arr(self.supported_kernels),
                    self.default_kernel,
                    arr(self.supported_suites),
                    self.default_suite,
                    self.default_layout,
                    self.image_size,
                    arr(self.device_packages),
                    arr(self.device_exclude),
                    arr(self.device_nonfree)
                ),
            )
            .unwrap();

            for k in self.supported_kernels {
                fs::write(
                    p.join(format!("kernels/{k}.toml")),
                    format!(
                        "flavor = \"mainline\"\nsource = \"linux-stable\"\nbase_defconfig = \"defconfig\"\n\
                         config_fragments = []\npatch_series = []\nsupported_socs = [\"rk3588\"]\n\
                         libre = {}\n",
                        self.libre
                    ),
                )
                .unwrap();
            }

            for f in &self.features {
                fs::write(
                    p.join(format!("features/{}.toml", f.name)),
                    format!(
                        "description = \"feat\"\npackages = {}\nexclude = {}\nrequires_soc = [\"rk3588\"]\n",
                        arr(f.packages),
                        arr(f.exclude)
                    ),
                )
                .unwrap();
            }

            if let Some(r) = &self.recipe {
                let mut body = String::from("device = \"dev\"\n");
                if let Some(k) = r.kernel {
                    body.push_str(&format!("kernel = {k:?}\n"));
                }
                if let Some(s) = r.suite {
                    body.push_str(&format!("suite = {s:?}\n"));
                }
                if let Some(l) = r.layout {
                    body.push_str(&format!("layout = {l:?}\n"));
                }
                if let Some(sz) = r.image_size {
                    body.push_str(&format!("image_size = {sz:?}\n"));
                }
                body.push_str(&format!("features = {}\n", arr(r.features)));
                // Appended verbatim: `[[data_volumes]]` is a table array, so a test
                // writes the TOML it means rather than the fixture modelling every
                // field of a type whose validation is what is under test.
                body.push_str(r.data_volumes);
                fs::write(p.join("recipes/rec.toml"), body).unwrap();
            }

            dir
        }
    }

    /// The nonfree-firmware axis, both ways round, on one tree.
    ///
    /// The two hardware layers declare blobs; nothing else changes between the runs
    /// but the kernel's `libre`. Asserting the *whole* package list each way is the
    /// point: it shows both that the blobs leave on a libre build and that nothing
    /// else does, which a `!contains` on the firmware names alone would not.
    #[test]
    fn a_libre_kernel_drops_the_hardware_layers_nonfree_firmware() {
        let blobbed = Tree {
            base_packages: &["base-pkg"],
            soc_packages: &["soc-pkg"],
            soc_nonfree: &["firmware-soc"],
            device_packages: &["device-pkg"],
            device_nonfree: &["firmware-board"],
            ..Default::default()
        };
        let dir = blobbed.write();
        let b = resolve_device(&ConfigRoot::new(dir.path()), "dev", &Overrides::default()).unwrap();
        assert!(!b.libre);
        // Each layer's firmware merges directly after that layer's own packages.
        assert_eq!(
            b.rootfs_packages,
            vec![
                "base-pkg",
                "soc-pkg",
                "firmware-soc",
                "device-pkg",
                "firmware-board"
            ]
        );

        let libre = Tree {
            libre: true,
            ..blobbed
        };
        let dir = libre.write();
        let b = resolve_device(&ConfigRoot::new(dir.path()), "dev", &Overrides::default()).unwrap();
        assert!(b.libre);
        assert_eq!(b.rootfs_packages, vec!["base-pkg", "soc-pkg", "device-pkg"]);
        // Dropped from the include set, not pushed onto the exclude set: the blob is
        // unreachable, not unwanted, and excluding it would also forbid apt from
        // pulling it in as some other package's dependency.
        assert!(b.rootfs_exclude.is_empty());
    }

    #[test]
    fn exclude_unions_across_layers_and_wins() {
        // base adds a/b/shared; soc drops b; device drops soc's c; the feature
        // drops base's a. The exclude set is the union (base→soc→bm→device→feat
        // order); the include set is the merge minus that union.
        let tree = Tree {
            base_packages: &["a", "b", "shared"],
            soc_packages: &["c"],
            soc_exclude: &["b"],
            bm_packages: &["d"],
            device_packages: &["e"],
            device_exclude: &["c"],
            features: vec![Feat {
                name: "f1",
                packages: &["g"],
                exclude: &["a"],
            }],
            ..Default::default()
        };
        let dir = tree.write();
        let root = ConfigRoot::new(dir.path());
        let ov = Overrides {
            features: Some(vec!["f1".into()]),
            ..Default::default()
        };
        let b = resolve_device(&root, "dev", &ov).unwrap();

        assert_eq!(b.rootfs_exclude, vec!["b", "c", "a"]);
        assert_eq!(b.rootfs_packages, vec!["shared", "d", "e", "g"]);
        // No name is both included and excluded — what the reconciliation guarantees.
        for x in &b.rootfs_exclude {
            assert!(
                !b.rootfs_packages.contains(x),
                "{x} leaked into the include set"
            );
        }
    }

    #[test]
    fn cli_override_beats_recipe_each_axis() {
        let tree = Tree {
            supported_kernels: &["k1", "k2"],
            features: vec![
                Feat {
                    name: "f1",
                    packages: &["p1"],
                    exclude: &[],
                },
                Feat {
                    name: "f2",
                    packages: &["p2"],
                    exclude: &[],
                },
            ],
            recipe: Some(RecipeSpec {
                kernel: Some("k1"),
                suite: Some("bookworm"),
                layout: Some("combined"),
                features: &["f1"],
                image_size: Some("1G"),
                data_volumes: "",
            }),
            ..Default::default()
        };
        let dir = tree.write();
        let root = ConfigRoot::new(dir.path());
        let cli = Overrides {
            kernel: Some("k2".into()),
            suite: Some("sid".into()),
            layout: Some(Layout::Split),
            features: Some(vec!["f2".into()]),
            image_size: Some("4G".into()),
            ..Default::default()
        };
        let b = resolve_recipe(&root, "rec", &cli).unwrap();
        assert_eq!(b.kernel.as_ref().unwrap().id(), "k2");
        assert_eq!(b.suite.as_deref(), Some("sid"));
        assert_eq!(b.layout, Layout::Split);
        assert_eq!(b.features, vec!["f2"]);
        assert_eq!(b.image_size, "4G");
        assert!(b.rootfs_packages.contains(&"p2".to_string()));
        assert!(!b.rootfs_packages.contains(&"p1".to_string()));
    }

    #[test]
    fn recipe_axes_apply_when_cli_unset() {
        let tree = Tree {
            supported_kernels: &["k1", "k2"],
            features: vec![Feat {
                name: "f1",
                packages: &["p1"],
                exclude: &[],
            }],
            recipe: Some(RecipeSpec {
                kernel: Some("k2"),
                suite: Some("bookworm"),
                layout: Some("split"),
                features: &["f1"],
                image_size: Some("1G"),
                data_volumes: "",
            }),
            ..Default::default()
        };
        let dir = tree.write();
        let root = ConfigRoot::new(dir.path());
        let b = resolve_recipe(&root, "rec", &Overrides::default()).unwrap();
        assert_eq!(b.kernel.as_ref().unwrap().id(), "k2");
        assert_eq!(b.suite.as_deref(), Some("bookworm"));
        assert_eq!(b.layout, Layout::Split);
        assert_eq!(b.features, vec!["f1"]);
        assert_eq!(b.image_size, "1G");
    }

    #[test]
    fn cli_some_empty_clears_recipe_features() {
        // `Some(vec![])` is an explicit "no features", distinct from `None`
        // (defer to the recipe). It must clear the recipe's feature set.
        let tree = Tree {
            features: vec![Feat {
                name: "f1",
                packages: &["p1"],
                exclude: &[],
            }],
            recipe: Some(RecipeSpec {
                features: &["f1"],
                ..Default::default()
            }),
            ..Default::default()
        };
        let dir = tree.write();
        let root = ConfigRoot::new(dir.path());
        let cli = Overrides {
            features: Some(vec![]),
            ..Default::default()
        };
        let b = resolve_recipe(&root, "rec", &cli).unwrap();
        assert!(b.features.is_empty());
        assert!(!b.rootfs_packages.contains(&"p1".to_string()));
    }

    /// A tree whose only feature is `data-volume`, with the recipe selecting the
    /// features given and appending `volumes` as raw TOML.
    fn data_volume_tree(features: &'static [&'static str], volumes: &'static str) -> Tree {
        Tree {
            features: vec![Feat {
                name: "data-volume",
                packages: &["e2fsprogs"],
                exclude: &[],
            }],
            recipe: Some(RecipeSpec {
                features,
                data_volumes: volumes,
                ..Default::default()
            }),
            ..Default::default()
        }
    }

    const ONE_VOLUME: &str = "\n[[data_volumes]]\nmatch = { kind = \"nvme\" }\n\
                              label = \"b2d-data\"\nmount = \"/srv\"\n";

    #[test]
    fn a_data_volume_resolves_with_its_feature() {
        let dir = data_volume_tree(&["data-volume"], ONE_VOLUME).write();
        let root = ConfigRoot::new(dir.path());
        let b = resolve_recipe(&root, "rec", &Overrides::default()).unwrap();
        assert_eq!(b.data_volumes.len(), 1);
        assert_eq!(b.data_volumes[0].label, "b2d-data");
        assert_eq!(b.data_volumes[0].mount, "/srv");
        // Defaults fill in rather than being restated in every recipe.
        assert_eq!(b.data_volumes[0].fstype, crate::datavolume::VolumeFs::Ext4);
        assert_eq!(
            b.data_volumes[0].create,
            crate::datavolume::CreatePolicy::IfBlank
        );
    }

    #[test]
    fn each_half_of_the_data_volume_axis_is_useless_without_the_other() {
        // Declarations with no hook to act on them, and a hook with nothing to act
        // on: both are inert, and an image that silently came up without the disk
        // its recipe names is worse than a resolution error.
        let dir = data_volume_tree(&[], ONE_VOLUME).write();
        let err =
            resolve_recipe(&ConfigRoot::new(dir.path()), "rec", &Overrides::default()).unwrap_err();
        assert!(
            matches!(err, ConfigError::DataVolumeFeatureMismatch { .. }),
            "{err}"
        );

        let dir = data_volume_tree(&["data-volume"], "").write();
        let err =
            resolve_recipe(&ConfigRoot::new(dir.path()), "rec", &Overrides::default()).unwrap_err();
        assert!(
            matches!(err, ConfigError::DataVolumeFeatureMismatch { .. }),
            "{err}"
        );
    }

    #[test]
    fn an_ill_formed_data_volume_is_refused_at_resolution() {
        // The per-entry checks run through resolution, not only in the unit tests
        // of the type — a bad mount must never reach a rootfs.
        let bad = "\n[[data_volumes]]\nmatch = { kind = \"nvme\" }\n\
                   label = \"b2d-data\"\nmount = \"/\"\n";
        let dir = data_volume_tree(&["data-volume"], bad).write();
        let err =
            resolve_recipe(&ConfigRoot::new(dir.path()), "rec", &Overrides::default()).unwrap_err();
        assert!(matches!(err, ConfigError::DataVolumeMount { .. }), "{err}");
    }

    /// A 64-char lowercase-hex sha (all `seed`) — a well-formed content pin for the
    /// resolution tests, which validate + dedup but never fetch bytes.
    fn sha(seed: char) -> String {
        seed.to_string().repeat(64)
    }

    #[test]
    fn extra_debs_union_dedups_by_sha256() {
        // base pulls deb A; feature f1 pulls deb B and a byte-identical copy of A
        // (same sha256, different locator). The union is [A, B] — A dedups by
        // content and keeps base's locator (first appearance).
        let sha_a = sha('a');
        let sha_b = sha('b');
        let tree = Tree {
            features: vec![Feat {
                name: "f1",
                packages: &["p1"],
                exclude: &[],
            }],
            ..Default::default()
        };
        let dir = tree.write();
        let p = dir.path();
        fs::write(
            p.join("base.toml"),
            format!(
                "packages = []\nexclude = []\n\
                 extra_debs = [{{ path = \"vendor/a.deb\", sha256 = \"{sha_a}\" }}]\n"
            ),
        )
        .unwrap();
        fs::write(
            p.join("features/f1.toml"),
            format!(
                "description = \"f\"\npackages = [\"p1\"]\nrequires_soc = [\"rk3588\"]\n\
                 extra_debs = [{{ path = \"vendor/b.deb\", sha256 = \"{sha_b}\" }}, \
                 {{ url = \"https://x/a.deb\", sha256 = \"{sha_a}\" }}]\n"
            ),
        )
        .unwrap();
        let root = ConfigRoot::new(p);
        let ov = Overrides {
            features: Some(vec!["f1".into()]),
            ..Default::default()
        };
        let b = resolve_device(&root, "dev", &ov).unwrap();

        assert_eq!(b.extra_debs.len(), 2, "the A-copy dedups by sha256");
        assert_eq!(b.extra_debs[0].sha256, sha_a);
        // First appearance wins: base's `path` locator, not the feature's `url` copy.
        assert_eq!(b.extra_debs[0].path.as_deref(), Some("vendor/a.deb"));
        assert!(b.extra_debs[0].url.is_none());
        assert_eq!(b.extra_debs[1].sha256, sha_b);
    }

    #[test]
    fn extra_deb_malformed_pin_is_rejected_at_resolve() {
        // A bad sha256 fails at resolve, not deep in the fetch/verify at build.
        let tree = Tree::default().write();
        let p = tree.path();
        fs::write(
            p.join("base.toml"),
            "packages = []\nexclude = []\n\
             extra_debs = [{ path = \"vendor/a.deb\", sha256 = \"not-hex\" }]\n",
        )
        .unwrap();
        let root = ConfigRoot::new(p);
        assert!(matches!(
            resolve_device(&root, "dev", &Overrides::default()).unwrap_err(),
            ConfigError::ExtraDebBadHash { .. }
        ));

        // A missing locator is likewise a resolve-time error.
        let sha_a = sha('a');
        fs::write(
            p.join("base.toml"),
            format!("packages = []\nexclude = []\nextra_debs = [{{ sha256 = \"{sha_a}\" }}]\n"),
        )
        .unwrap();
        assert!(matches!(
            resolve_device(&ConfigRoot::new(p), "dev", &Overrides::default()).unwrap_err(),
            ConfigError::ExtraDebLocator { .. }
        ));
    }

    #[test]
    fn media_accel_feature_on_a_sourceless_soc_is_rejected() {
        // A feature that builds the media-accel stack requires the SoC to supply the
        // `[userspace]`/`[ffmpeg]` sources. Rewrite the synthetic SoC to omit them
        // and mark the feature `requires_media_accel`: resolution must fail with the
        // dedicated error naming the feature, not build a stack with no
        // sources.
        let tree = Tree {
            features: vec![Feat {
                name: "accel",
                packages: &["p1"],
                exclude: &[],
            }],
            ..Default::default()
        };
        let dir = tree.write();
        let p = dir.path();
        // SoC layer with no media-accel source stanzas.
        fs::write(
            p.join("socs/rk3588.toml"),
            "description = \"soc\"\narch = \"arm64\"\ndt_dir = \"rockchip\"\nmodules = []\n",
        )
        .unwrap();
        // The feature opts into the media-accel build.
        fs::write(
            p.join("features/accel.toml"),
            "description = \"accel\"\npackages = [\"p1\"]\nrequires_soc = [\"rk3588\"]\n\
             requires_media_accel = true\n",
        )
        .unwrap();
        let root = ConfigRoot::new(p);
        let cli = Overrides {
            features: Some(vec!["accel".into()]),
            ..Default::default()
        };
        match resolve_device(&root, "dev", &cli).unwrap_err() {
            ConfigError::FeatureRequiresMediaAccel { feature, soc } => {
                assert_eq!(feature, "accel");
                assert_eq!(soc, "rk3588");
            }
            other => panic!("expected FeatureRequiresMediaAccel, got {other:?}"),
        }
    }
}
