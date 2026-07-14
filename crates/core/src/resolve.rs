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
    let device = root.device(device_name)?;
    let soc = root.soc(device.soc)?;
    let arch = root.arch(soc.arch)?;

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

    // A board carrying its own device tree must actually build the DTB it boots;
    // check that here so a filename typo is a typed error rather than a kernel that
    // builds fine and then finds no DTB at boot. §4.
    validate_device_dts(&device.device_dts, &device.kernel_dtb, device_name)?;

    let layout = overrides.layout.unwrap_or(device.default_layout);

    // The boot method's *own* requirements, enforced only for the method that has
    // them: rkbin blobs and a `uboot_defconfig` where u-boot is compiled, a board
    // profile where a signed kernel partition is written. A board is never asked for
    // a field its boot method would not read.
    let boot = resolve_boot(
        &bm,
        &device,
        &soc,
        device_name,
        layout,
        overrides.board.as_deref(),
    )?;

    let kernel = resolve_kernel(kdef, kernel_id, &device, device_name)?;

    let suite = overrides
        .suite
        .clone()
        .unwrap_or_else(|| device.default_suite.clone());
    // A bad suite otherwise fails deep in the bootstrap; reject it here (CFG-3), and
    // the shape guard also keeps a leading-`-` suite from ever reaching mmdebstrap as
    // a positional (SUB-2 backstop).
    validate_suite(&suite)?;
    let features = overrides.features.clone().unwrap_or_default();
    // Reject a feature selected twice: its overlay + packages would otherwise apply
    // twice (COR-15).
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
    let mut rootfs_packages = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for src in [
        base.packages.as_slice(),
        soc.packages.as_slice(),
        bm.packages(),
        device.packages.as_slice(),
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
    // byte/LBA `Geometry`. A zero-size image is not buildable. (The boot method's own
    // offsets are parsed the same way, in `resolve_boot`.)
    if crate::size::parse_size(&image_size)? == 0 {
        return Err(ConfigError::InvalidSize {
            value: image_size.clone(),
        });
    }

    // Localization: base-layer distro policy (locale, generated locales, timezone) plus
    // the device's keymap, each overridable. Validated here so a bad zone or a locale
    // with no codeset is a typed error at resolve, not a dangling /etc/localtime or an
    // ungenerated LANG discovered on the booted board.
    let (locale, locales_generate, timezone, keymap) = resolve_l10n(&base, &device, overrides)?;

    Ok(ResolvedBuild {
        device: device_name.to_string(),
        description: device.description,
        arch: soc.arch,
        soc: device.soc,
        boot_method,
        kernel,
        suite,
        features,
        rootfs_packages,
        rootfs_exclude,
        layout,
        image_size,
        hostname: device.hostname,
        locale,
        locales_generate,
        timezone,
        keymap,
        boot,
        kernel_dtb: device.kernel_dtb,
        device_dts: device.device_dts,
        dt_dir: soc.dt_dir,
        modules: soc.modules,
        kernel_arch: arch.kernel_arch,
        uboot_arch: arch.uboot_arch,
        cross_compile: arch.cross_compile,
        kbuild_image: arch.kbuild_image,
        // Sources ride only when a feature builds the stack; a base build drops
        // them (validated above: `build_media_accel` implies the SoC supplies both).
        userspace: build_media_accel.then_some(soc.userspace).flatten(),
        ffmpeg: build_media_accel.then_some(soc.ffmpeg).flatten(),
        apt_sources,
        extra_debs,
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
fn resolve_boot(
    bm: &BootMethodLayer,
    device: &DeviceLayer,
    soc: &SocLayer,
    device_name: &str,
    layout: Layout,
    board_override: Option<&str>,
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
            // the device overrides per field (typically just the DDR TPL). §3.6.
            let rkbin = resolve_rkbin(&soc.rkbin, &device.rkbin, device_name)?;
            for s in [&l.idbloader_offset, &l.uboot_itb_offset, &l.rootfs_offset] {
                crate::size::parse_size(s)?;
            }
            Ok(ResolvedBoot::RockchipRkbin(ResolvedRkbinBoot {
                uboot_defconfig,
                uboot_source: l.uboot_source.clone(),
                uboot_ref: l.uboot_ref.clone(),
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
            for s in [&l.kpart_offset, &l.kpart_size, &l.rootfs_offset] {
                crate::size::parse_size(s)?;
            }
            validate_depthcharge_cmdline(&l.cmdline)?;
            let flags =
                crate::chromeos::kpart_flags(l.kpart_priority, l.kpart_tries, l.kpart_successful)?;
            Ok(ResolvedBoot::Depthcharge(ResolvedDepthchargeBoot {
                board,
                kpart: Kpart {
                    offset: l.kpart_offset.clone(),
                    size: l.kpart_size.clone(),
                    priority: l.kpart_priority,
                    tries: l.kpart_tries,
                    successful: l.kpart_successful,
                    flags,
                },
                cmdline: l.cmdline.clone(),
                rootfs_offset: l.rootfs_offset.clone(),
            }))
        }
    }
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
    if cmdline.split_whitespace().any(|tok| tok.starts_with("root=")) {
        return bad(
            "it sets `root=`, which depthchargectl derives from the image's /etc/fstab and \
             strips when it disagrees — remove it and let fstab be the single source",
        );
    }
    Ok(())
}

/// Resolve the kernel axis, and reject the two device fields a distro-package
/// kernel could never act on.
///
/// A distro kernel compiles nothing, so a `device_dts` or `device_config_fragments`
/// on such a build is not merely redundant — it is a board whose device tree will
/// never be compiled and whose kconfig will never be merged. That reads as
/// configured and boots as broken, so it is a typed error instead.
fn resolve_kernel(
    kdef: KernelDef,
    kernel_id: String,
    device: &DeviceLayer,
    device_name: &str,
) -> Result<ResolvedKernel, ConfigError> {
    match kdef {
        KernelDef::Compiled(k) => {
            // Kernel-owned fragments first, then device fragments (apply order).
            let mut config_fragments = k.config_fragments;
            config_fragments.extend(device.device_config_fragments.iter().cloned());
            Ok(ResolvedKernel::Compiled(ResolvedCompiledKernel {
                id: kernel_id,
                flavor: k.flavor,
                source: k.source,
                track: k.track,
                base_defconfig: k.base_defconfig,
                // The `"none"` sentinel becomes a typed absence exactly here; nothing
                // downstream compares the authored string.
                patch_profile: crate::profile::patch_profile(&k.patch_profile).map(str::to_string),
                patches_url: k.patches_url,
                config_fragments,
            }))
        }
        KernelDef::Distro(k) => {
            for (what, declared) in [
                ("device_dts", !device.device_dts.is_empty()),
                (
                    "device_config_fragments",
                    !device.device_config_fragments.is_empty(),
                ),
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

/// Validate a device's loose device-tree sources against its `kernel_dtb` (§4).
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
        if !matches!(path.extension().and_then(|e| e.to_str()), Some("dts" | "dtsi")) {
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

/// Merge the SoC-layer rkbin defaults with the device overrides (device wins per
/// field) and validate the result: `atf` and `tpl` are required (a missing or
/// blank one is a [`ConfigError::MissingBlob`]), `bl32` stays optional. §3.6.
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
    !v.is_empty() && v.chars().all(|c| c.is_ascii_graphic() && c != '[' && c != ']')
}

/// Validate a feature's [`AptSource`] against the one-line source grammar the
/// bootstrap renders it into: every field a clean token, the URI
/// http(s) (any other transport would sidestep the mirror trust model), the
/// name additionally separator-free (it becomes a dedup key and file stem), and
/// at least one component unless the suite is an exact path (ends in `/`).
fn validate_apt_source(feature: &str, src: &AptSource) -> Result<(), ConfigError> {
    let bad = |field: &'static str, value: &str| ConfigError::AptSourceBadField {
        feature: feature.to_string(),
        name: src.name.clone(),
        field,
        value: value.to_string(),
    };
    if !apt_line_token(&src.name) || src.name.contains('/') {
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
    Ok(())
}

/// Union the selected features' [`AptSource`]s, keyed by `name`. Each source is
/// validated against the apt line grammar first ([`validate_apt_source`]).
/// Two features may legitimately reference the same repo — those
/// collapse to one entry — but a same-name pair with different settings is
/// [`ConfigError::ConflictingAptSource`], since the bootstrap solve could not tell
/// which repo to activate. Order follows first appearance across the feature list.
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

/// Resolve a named recipe: recipe fields are the base axes; `cli` overrides win.
pub fn resolve_recipe(
    root: &ConfigRoot,
    recipe_name: &str,
    cli: &Overrides,
) -> Result<ResolvedBuild, ConfigError> {
    let recipe = root.recipe(recipe_name)?;
    let merged = Overrides {
        kernel: cli.kernel.clone().or(recipe.kernel),
        suite: cli.suite.clone().or(recipe.suite),
        layout: cli.layout.or(recipe.layout),
        boot_method: cli.boot_method,
        board: cli.board.clone(),
        features: cli
            .features
            .clone()
            .or_else(|| (!recipe.features.is_empty()).then_some(recipe.features)),
        image_size: cli.image_size.clone().or(recipe.image_size),
        locale: cli.locale.clone().or(recipe.locale),
        locales_generate: cli.locales_generate.clone().or(recipe.locales_generate),
        timezone: cli.timezone.clone().or(recipe.timezone),
        keymap: cli.keymap.clone().or(recipe.keymap),
    };
    resolve_device(root, &recipe.device, &merged)
}

/// Append `src` package names to `acc`, skipping any already present, so the
/// merged rootfs set is order-preserving and de-duplicated.
fn extend_unique(acc: &mut Vec<String>, seen: &mut std::collections::HashSet<String>, src: &[String]) {
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

/// Reject a suite that is not a well-formed Debian codename (CFG-3). The
/// suite becomes an apt `sources.list` pocket (`<suite>-updates`, `<suite>-security`)
/// and an mmdebstrap positional, so it must be a bare token starting with an
/// alphanumeric and drawn from `[A-Za-z0-9._-]`. Requiring an alphanumeric first
/// character also means a suite can never be read as a `-`-prefixed option by the
/// bootstrap (the SUB-2 backstop). Pure, so it is unit-testable.
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
        let blanked = resolve_rkbin(&soc, &layer(Some("  "), Some("ddr.bin"), None), "h96").unwrap();
        assert_eq!(blanked.atf, "atf.elf");
    }

    #[test]
    fn the_none_sentinel_resolves_to_no_patch_profile() {
        // The `"none"` spelling is config-facing only; resolution turns it into a
        // typed absence so no downstream code compares against the magic string.
        assert_eq!(crate::profile::patch_profile("none"), None);
        assert_eq!(crate::profile::patch_profile("rk3588-accel"), Some("rk3588-accel"));
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
            ConfigError::KernelDtbNotInDeviceDts { device, expected, .. } => {
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
            let err = validate_device_dts(&[entry.to_string()], "rockchip/b.dtb", "h96").unwrap_err();
            assert!(
                matches!(err, ConfigError::InvalidDeviceDts { .. }),
                "expected InvalidDeviceDts for {entry:?}, got {err:?}"
            );
        };
        bad("");                                   // empty
        bad("/etc/passwd.dts");                    // absolute
        bad("../../outside/b.dts");                // escapes the config root
        bad("devices/h96/dts/../../../b.dts");     // escapes mid-path
        bad("devices/h96/dts/b.dtb");              // a blob, not a source
        bad("devices/h96/dts/b");                  // no extension
    }

    #[test]
    fn validate_suite_accepts_codenames_and_rejects_bad_shapes() {
        for s in ["forky", "trixie", "sid", "bookworm", "stable-proposed-updates"] {
            assert!(validate_suite(s).is_ok(), "{s} should be a valid suite");
        }
        for s in [
            "",                             // empty
            "-updates",                     // leading dash (option-like)
            "..",                           // traversal
            "for ky",                       // space
            "forky;rm -rf /",               // shell metacharacters
            "forky/../etc",                 // path separator
        ] {
            assert!(
                matches!(validate_suite(s), Err(ConfigError::InvalidSuite { .. })),
                "{s} should be rejected"
            );
        }
    }

    #[test]
    fn l10n_defaults_come_from_the_layer_that_determines_them() {
        let root = repo_root();

        // The RK1 is a headless server: it takes the base layer's system-wide locale
        // and timezone, and has no keymap at all — nothing is typing at its console.
        let rk1 = resolve_recipe(&root, "turing-rk1-forky", &Overrides::default()).unwrap();
        assert_eq!(rk1.locale, "C.UTF-8");
        assert_eq!(rk1.timezone, "UTC");
        assert_eq!(rk1.keymap, None, "a headless board declares no keymap");

        // The C201 is a laptop, and is the one shipped board a console keymap
        // configures anything on. It takes the same system-wide locale.
        let c201 = resolve_recipe(&root, "asus-c201-forky", &Overrides::default()).unwrap();
        assert_eq!(c201.locale, "C.UTF-8");
        let keymap = c201.keymap.expect("a board with a keyboard declares a keymap");
        assert_eq!(keymap.layout, "us");
        assert_eq!(keymap.model, "pc105", "the bare-string form takes Debian's model");
    }

    #[test]
    fn the_system_locale_is_always_generated() {
        // The invariant: LANG can never name a locale the image does not carry. It
        // holds even when the locale is one the config never listed, and even when it
        // is C.UTF-8 — which glibc builds in and *would* work ungenerated, but which
        // must still appear in /etc/locale.gen, because that file is where the `locales`
        // package builds the choice list `dpkg-reconfigure locales` offers.
        let root = repo_root();

        let base = resolve_recipe(&root, "turing-rk1-forky", &Overrides::default()).unwrap();
        assert_eq!(base.locales_generate, vec!["C.UTF-8", "en_US.UTF-8"]);
        assert!(base.locales_generate.contains(&base.locale));

        // An override the base never lists is generated anyway, and leads the set.
        let de = resolve_recipe(
            &root,
            "turing-rk1-forky",
            &Overrides {
                locale: Some("de_DE.UTF-8".into()),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(de.locales_generate, vec!["de_DE.UTF-8", "en_US.UTF-8"]);

        // Naming it in both places generates it once, not twice.
        let dup = resolve_recipe(
            &root,
            "turing-rk1-forky",
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
            "turing-rk1-forky",
            &Overrides {
                keymap: Some(Keymap::from_layout("gb")),
                ..Default::default()
            },
        )
        .unwrap();
        assert_eq!(b.keymap.unwrap().layout, "gb");
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
        assert_eq!(crate::model::locale_codeset("sr_RS.UTF-8@latin"), Some("UTF-8"));
        assert_eq!(crate::model::locale_codeset("de_DE"), None);
        assert_eq!(crate::model::locale_codeset("de_DE."), None);
    }

    #[test]
    fn validate_timezone_rejects_anything_that_escapes_the_zone_database() {
        for ok in ["UTC", "America/New_York", "Etc/GMT+5", "America/Argentina/Buenos_Aires"] {
            assert!(validate_timezone(ok).is_ok(), "{ok} should be a valid zone");
        }
        for bad in [
            "",                        // empty
            "/etc/shadow",             // absolute: escapes /usr/share/zoneinfo
            "../../../etc/shadow",     // traversal: /etc/localtime would point at it
            "America/../../etc/shadow",// traversal mid-path
            "Europe/",                 // trailing separator
            "Europe/Ber lin",          // space
            "Europe/Berlin;id",        // shell metacharacter
        ] {
            assert!(
                matches!(validate_timezone(bad), Err(ConfigError::InvalidTimezone { .. })),
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
            Err(ConfigError::InvalidKeymap { field: "layout", .. })
        ));
        let subst = Keymap {
            options: "$(id)".into(),
            ..Keymap::from_layout("us")
        };
        assert!(matches!(
            validate_keymap(&subst),
            Err(ConfigError::InvalidKeymap { field: "options", .. })
        ));
        assert!(matches!(
            validate_keymap(&Keymap::from_layout("")),
            Err(ConfigError::InvalidKeymap { field: "layout", .. })
        ));
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
    fn rk1_recipe_resolves_expected_axes() {
        let root = repo_root();
        let b = resolve_recipe(&root, "turing-rk1-forky", &Overrides::default()).unwrap();
        assert_eq!(b.arch, Arch::Arm64);
        assert_eq!(b.soc, Soc::Rk3588);
        assert_eq!(b.boot_method, BootMethod::RockchipRkbin);
        assert_eq!(b.suite, "forky");
        assert_eq!(b.layout, Layout::Combined);
        // The recipe's single capability feature resolves + passes the SoC/arch
        // gates.
        assert_eq!(b.features, vec!["media-accel-rockchip"]);
        // media-accel-rockchip declares `requires_media_accel`, so the resolved
        // build carries the SoC's userspace + ffmpeg source trees (built as a unit).
        assert!(b.userspace.is_some(), "media-accel build carries userspace sources");
        assert!(b.ffmpeg.is_some(), "media-accel build carries ffmpeg sources");
        // The shipped media-accel-rockchip feature adds no third-party apt source.
        assert!(b.apt_sources.is_empty());
        // Merged rootfs set: base packages + the feature's packages, base excludes.
        assert!(b.rootfs_packages.contains(&"openssh-server".to_string()));
        assert!(b.rootfs_packages.contains(&"ffmpeg-rk".to_string()));
        assert_eq!(b.rootfs_exclude, vec!["isc-dhcp-client"]);
        assert_eq!(b.kernel.id(), "rk3588-mainline-7.1");
        assert_eq!(b.kernel_dtb, "rockchip/rk3588-turing-rk1.dtb");
        assert!(b.modules.contains(&"rga3".to_string()));
        assert!(b.modules.contains(&"rkvenc".to_string()));
        // kernel fragments precede device fragments in apply order; the generated
        // Debian baseline is first, then the curated rockchip slices.
        let kernel = b.kernel.compiled().expect("the RK1 compiles its kernel");
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
        assert_eq!(boot.uboot_ref, "v2026.04");
        assert_eq!(boot.uboot_defconfig, "turing-rk1-rk3588_defconfig");
        assert_eq!(boot.offsets.idbloader, "32KiB");
        assert_eq!(boot.offsets.uboot_itb, "8MiB");
        assert_eq!(boot.offsets.rootfs, "16MiB");
        assert_eq!(boot.rkbin.atf, "rk3588_bl31_v1.51.elf");
        assert!(b.depthcharge_boot().is_none());
    }

    #[test]
    fn cli_override_beats_device_default() {
        let root = repo_root();
        let ov = Overrides {
            suite: Some("sid".to_string()),
            layout: Some(Layout::Split),
            ..Default::default()
        };
        let b = resolve_device(&root, "turing-rk1", &ov).unwrap();
        assert_eq!(b.suite, "sid");
        assert_eq!(b.layout, Layout::Split);
    }

    #[test]
    fn c201_recipe_resolves_a_depthcharge_board_with_a_distro_kernel() {
        let root = repo_root();
        let b = resolve_recipe(&root, "asus-c201-forky", &Overrides::default()).unwrap();
        assert_eq!(b.arch, Arch::Armv7);
        assert_eq!(b.arch.debian_arch(), "armhf");
        assert_eq!(b.soc, Soc::Rk3288);
        assert_eq!(b.boot_method, BootMethod::Depthcharge);
        assert_eq!(b.suite, "forky");

        // The kernel is Debian's: no source, no fragments, no patches — and the
        // package joins the rootfs set, which is how it gets installed and pinned.
        assert!(!b.compiles_kernel());
        assert_eq!(b.kernel.id(), "debian-armmp");
        assert_eq!(b.kernel.patch_profile(), None);
        assert!(b.kernel.compiled().is_none());
        assert!(b.rootfs_packages.contains(&"linux-image-armmp".to_string()));

        // The boot half is the kernel partition, with the bits that make the firmware
        // boot it. These are the values read back off the image that boots the unit.
        assert!(b.rkbin_boot().is_none(), "this board has no rkbin chain");
        let boot = b.depthcharge_boot().expect("a depthcharge board");
        assert_eq!(boot.board, "speedy", "the stock profile, which boots both firmwares");
        assert_eq!(boot.kpart.offset, "12MiB");
        assert_eq!(boot.kpart.size, "16MiB");
        assert_eq!(boot.kpart.flags, 0x015A_0000_0000_0000);
        assert_eq!(boot.rootfs_offset, "28MiB");
        assert!(!boot.cmdline.contains("root="), "root= is depthchargectl's to derive");

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
    fn the_trixie_recipe_differs_only_in_the_suite() {
        // One distro-kernel definition serves both releases: the *suite* picks the
        // version (forky 7.1.x, trixie 6.12.x), which is the whole point of not
        // authoring a kernel per release.
        let root = repo_root();
        let b = resolve_recipe(&root, "asus-c201-trixie", &Overrides::default()).unwrap();
        assert_eq!(b.suite, "trixie");
        assert_eq!(b.kernel.id(), "debian-armmp");
        assert_eq!(b.device, "asus-c201");
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
            ConfigError::UnknownBoardProfile { device, board, supported } => {
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
            ConfigError::UnsupportedLayout { boot_method, layout, .. } => {
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
            "kernel_arch = \"arm\"\nuboot_arch = \"arm\"\n\
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
             rootfs_offset = \"28MiB\"\nkpart_priority = 10\nkpart_tries = 5\n\
             kpart_successful = true\ncmdline = \"ro\"\n",
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
             default_suite = \"forky\"\ndefault_layout = \"combined\"\nhostname = \"d\"\n\
             image_size = \"4G\"\n",
        )
        .unwrap();
        let root = ConfigRoot::new(p);
        match resolve_device(&root, "dev", &Overrides::default()).unwrap_err() {
            ConfigError::MissingBootField { device, boot_method, what } => {
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
            "kernel_arch = \"arm\"\nuboot_arch = \"arm\"\n\
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
             rootfs_offset = \"28MiB\"\nkpart_priority = 10\nkpart_tries = 5\n\
             kpart_successful = true\ncmdline = \"ro\"\n",
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
             default_kernel = \"k\"\ndefault_suite = \"forky\"\ndefault_layout = \"combined\"\n\
             hostname = \"d\"\nimage_size = \"4G\"\n\n[depthcharge]\nboard = \"d\"\n\
             supported_boards = [\"d\"]\n",
        )
        .unwrap();
        let root = ConfigRoot::new(p);
        match resolve_device(&root, "dev", &Overrides::default()).unwrap_err() {
            ConfigError::DistroKernelCompilesNothing { device, kernel, what } => {
                assert_eq!(device, "dev");
                assert_eq!(kernel, "k");
                assert_eq!(what, "device_config_fragments");
            }
            other => panic!("expected DistroKernelCompilesNothing, got {other:?}"),
        }
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
             rootfs_offset = \"28MiB\"\nkpart_priority = 10\nkpart_tries = 5\n\
             kpart_successful = true\ncmdline = \"ro\"\nidbloader_offset = \"32KiB\"\n",
        )
        .unwrap();
        let root = ConfigRoot::new(p);
        let err = root.boot_method(BootMethod::Depthcharge).unwrap_err();
        assert!(
            matches!(err, ConfigError::Parse { .. }) && err.to_string().contains("idbloader_offset"),
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
            description: "t".into(),
            packages: vec![],
            exclude: vec![],
            requires_soc: vec![],
            requires_arch: vec![],
            apt_sources: sources,
            extra_debs: vec![],
            conflicts: vec![],
            requires_media_accel: false,
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
        // positional, so whitespace / brackets / newlines in any field — or a
        // non-http(s) transport — must fail at resolve time, naming the
        // field.
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
            ("name", with(&|s| s.name = "je/llyfin".into())),
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

    #[test]
    fn apt_sources_dedup_identical_and_reject_clashes() {
        let a = ("app-a".to_string(), feat_with_sources(vec![src("jellyfin", "u1")]));
        // Identical duplicate collapses to one entry.
        let a2 = ("app-a2".to_string(), feat_with_sources(vec![src("jellyfin", "u1")]));
        let merged = merge_apt_sources(&[a.clone(), a2]).unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].name, "jellyfin");

        // Same name, different URI is a hard clash.
        let b = ("app-b".to_string(), feat_with_sources(vec![src("jellyfin", "u2")]));
        assert!(matches!(
            merge_apt_sources(&[a, b]).unwrap_err(),
            ConfigError::ConflictingAptSource { .. }
        ));
    }

    #[test]
    fn apt_sources_union_preserves_first_appearance_order() {
        let a = ("app-a".to_string(), feat_with_sources(vec![src("one", "u1")]));
        let b = ("app-b".to_string(), feat_with_sources(vec![src("two", "u2")]));
        let merged = merge_apt_sources(&[a, b]).unwrap();
        assert_eq!(
            merged.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            vec!["one", "two"]
        );
    }
}

/// Fixture-based resolution tests (MNT-11): a minimal config root written to a
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
    }

    /// A complete config tree with buildable defaults; a test sets only the axes
    /// it exercises via struct-update syntax.
    struct Tree {
        base_packages: &'static [&'static str],
        base_exclude: &'static [&'static str],
        soc_packages: &'static [&'static str],
        soc_exclude: &'static [&'static str],
        bm_packages: &'static [&'static str],
        bm_exclude: &'static [&'static str],
        device_packages: &'static [&'static str],
        device_exclude: &'static [&'static str],
        supported_kernels: &'static [&'static str],
        default_kernel: &'static str,
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
                bm_packages: &[],
                bm_exclude: &[],
                device_packages: &[],
                device_exclude: &[],
                supported_kernels: &["k1"],
                default_kernel: "k1",
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
            for sub in ["arches", "socs", "boot-methods", "devices", "kernels", "features", "recipes"] {
                fs::create_dir_all(p.join(sub)).unwrap();
            }

            fs::write(
                p.join("arches/arm64.toml"),
                "kernel_arch = \"arm64\"\nuboot_arch = \"arm\"\n\
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
                     modules = []\npackages = {}\nexclude = {}\n{src}",
                    arr(self.soc_packages),
                    arr(self.soc_exclude)
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
                     supported_kernels = {}\ndefault_kernel = {:?}\ndefault_suite = {:?}\n\
                     default_layout = {:?}\nhostname = \"dev\"\nimage_size = {:?}\n\
                     packages = {}\nexclude = {}\n\n[rkbin]\natf = \"atf.elf\"\ntpl = \"tpl.bin\"\n",
                    arr(self.supported_kernels),
                    self.default_kernel,
                    self.default_suite,
                    self.default_layout,
                    self.image_size,
                    arr(self.device_packages),
                    arr(self.device_exclude)
                ),
            )
            .unwrap();

            for k in self.supported_kernels {
                fs::write(
                    p.join(format!("kernels/{k}.toml")),
                    "flavor = \"mainline\"\nsource = \"linux-stable\"\nbase_defconfig = \"defconfig\"\n\
                     config_fragments = []\npatch_profile = \"none\"\nsupported_socs = [\"rk3588\"]\n",
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
                fs::write(p.join("recipes/rec.toml"), body).unwrap();
            }

            dir
        }
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
        // No name is both included and excluded (the reconciliation COR-16 adds).
        for x in &b.rootfs_exclude {
            assert!(!b.rootfs_packages.contains(x), "{x} leaked into the include set");
        }
    }

    #[test]
    fn cli_override_beats_recipe_each_axis() {
        let tree = Tree {
            supported_kernels: &["k1", "k2"],
            features: vec![
                Feat { name: "f1", packages: &["p1"], exclude: &[] },
                Feat { name: "f2", packages: &["p2"], exclude: &[] },
            ],
            recipe: Some(RecipeSpec {
                kernel: Some("k1"),
                suite: Some("bookworm"),
                layout: Some("combined"),
                features: &["f1"],
                image_size: Some("1G"),
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
        assert_eq!(b.kernel.id(), "k2");
        assert_eq!(b.suite, "sid");
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
            features: vec![Feat { name: "f1", packages: &["p1"], exclude: &[] }],
            recipe: Some(RecipeSpec {
                kernel: Some("k2"),
                suite: Some("bookworm"),
                layout: Some("split"),
                features: &["f1"],
                image_size: Some("1G"),
            }),
            ..Default::default()
        };
        let dir = tree.write();
        let root = ConfigRoot::new(dir.path());
        let b = resolve_recipe(&root, "rec", &Overrides::default()).unwrap();
        assert_eq!(b.kernel.id(), "k2");
        assert_eq!(b.suite, "bookworm");
        assert_eq!(b.layout, Layout::Split);
        assert_eq!(b.features, vec!["f1"]);
        assert_eq!(b.image_size, "1G");
    }

    #[test]
    fn cli_some_empty_clears_recipe_features() {
        // `Some(vec![])` is an explicit "no features", distinct from `None`
        // (defer to the recipe). It must clear the recipe's feature set.
        let tree = Tree {
            features: vec![Feat { name: "f1", packages: &["p1"], exclude: &[] }],
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
            features: vec![Feat { name: "f1", packages: &["p1"], exclude: &[] }],
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
        let ov = Overrides { features: Some(vec!["f1".into()]), ..Default::default() };
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
            features: vec![Feat { name: "accel", packages: &["p1"], exclude: &[] }],
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
        let cli = Overrides { features: Some(vec!["accel".into()]), ..Default::default() };
        match resolve_device(&root, "dev", &cli).unwrap_err() {
            ConfigError::FeatureRequiresMediaAccel { feature, soc } => {
                assert_eq!(feature, "accel");
                assert_eq!(soc, "rk3588");
            }
            other => panic!("expected FeatureRequiresMediaAccel, got {other:?}"),
        }
    }
}
