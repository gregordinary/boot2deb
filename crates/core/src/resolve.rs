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
    if !kdef.supported_socs.contains(&device.soc) {
        return Err(ConfigError::SocMismatch {
            kernel: kernel_id,
            soc: device.soc.to_string(),
            supported: join(&kdef.supported_socs),
        });
    }

    // Required blobs present.
    if device.rkbin.atf.trim().is_empty() {
        return Err(ConfigError::MissingBlob {
            device: device_name.to_string(),
            what: "rkbin.atf".into(),
        });
    }
    if device.rkbin.tpl.trim().is_empty() {
        return Err(ConfigError::MissingBlob {
            device: device_name.to_string(),
            what: "rkbin.tpl".into(),
        });
    }

    // Kernel-owned fragments first, then device fragments (apply order).
    let mut config_fragments = kdef.config_fragments.clone();
    config_fragments.extend(device.device_config_fragments.iter().cloned());

    let suite = overrides
        .suite
        .clone()
        .unwrap_or_else(|| device.default_suite.clone());
    // A bad suite otherwise fails deep in the bootstrap; reject it here (CFG-3), and
    // the shape guard also keeps a leading-`-` suite from ever reaching mmdebstrap as
    // a positional (SUB-2 backstop).
    validate_suite(&suite)?;
    let layout = overrides.layout.unwrap_or(device.default_layout);
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

    // Union the features' third-party apt sources, de-duplicated by name.
    // Two features contributing an identical source share it; a same-name/
    // different-definition clash is a resolution error, since the bootstrap could
    // not tell which repo to activate.
    let apt_sources = merge_apt_sources(&loaded_features)?;

    // Merge the rootfs package set: base ∪ soc ∪ boot-method ∪ device ∪ Σ
    // features, de-duplicated with order preserved (base first). apt solves the
    // set, so order is not load-bearing — it only keeps the merged list stable.
    let base = root.base()?;
    let mut rootfs_packages = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for src in [
        base.packages.as_slice(),
        soc.packages.as_slice(),
        bm.packages.as_slice(),
        device.packages.as_slice(),
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
        bm.exclude.as_slice(),
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

    // Validate the authored geometry strings up front: a typo (`"2GB!"`) or
    // an unparseable offset must fail at resolve, not deep in the image stage after
    // the whole pipeline has run. The resolved build keeps the authored strings;
    // the image node re-parses them into its byte/LBA `Geometry`. A zero-size image
    // is not buildable, so it is rejected here.
    for s in [
        image_size.as_str(),
        bm.idbloader_offset.as_str(),
        bm.uboot_itb_offset.as_str(),
        bm.rootfs_offset.as_str(),
    ] {
        crate::size::parse_size(s)?;
    }
    if crate::size::parse_size(&image_size)? == 0 {
        return Err(ConfigError::InvalidSize {
            value: image_size.clone(),
        });
    }

    Ok(ResolvedBuild {
        device: device_name.to_string(),
        description: device.description,
        arch: soc.arch,
        soc: device.soc,
        boot_method,
        kernel: ResolvedKernel {
            id: kernel_id,
            flavor: kdef.flavor,
            source: kdef.source,
            track: kdef.track,
            base_defconfig: kdef.base_defconfig,
            patch_profile: kdef.patch_profile,
            patches_url: kdef.patches_url,
            config_fragments,
        },
        suite,
        features,
        rootfs_packages,
        rootfs_exclude,
        layout,
        image_size,
        hostname: device.hostname,
        uboot_defconfig: device.uboot_defconfig,
        uboot_source: bm.uboot_source,
        uboot_ref: bm.uboot_ref,
        kernel_dtb: device.kernel_dtb,
        dt_dir: soc.dt_dir,
        modules: soc.modules,
        kernel_arch: arch.kernel_arch,
        uboot_arch: arch.uboot_arch,
        cross_compile: arch.cross_compile,
        kbuild_image: arch.kbuild_image,
        rkbin: device.rkbin,
        offsets: Offsets {
            idbloader: bm.idbloader_offset,
            uboot_itb: bm.uboot_itb_offset,
            rootfs: bm.rootfs_offset,
        },
        userspace: soc.userspace,
        ffmpeg: soc.ffmpeg,
        apt_sources,
        extra_debs,
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
    absorb(&bm.extra_debs)?;
    absorb(&device.extra_debs)?;
    for (_, feat) in features {
        absorb(&feat.extra_debs)?;
    }
    Ok(merged)
}

/// Union the selected features' [`AptSource`]s, keyed by `name`. Two
/// features may legitimately reference the same repo — those collapse to one
/// entry — but a same-name pair with different settings is
/// [`ConfigError::ConflictingAptSource`], since the bootstrap solve could not tell
/// which repo to activate. Order follows first appearance across the feature list.
fn merge_apt_sources(
    features: &[(String, crate::feature::Feature)],
) -> Result<Vec<AptSource>, ConfigError> {
    let mut merged: Vec<(String, AptSource)> = Vec::new();
    for (feat_name, feat) in features {
        for src in &feat.apt_sources {
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
        features: cli
            .features
            .clone()
            .or_else(|| (!recipe.features.is_empty()).then_some(recipe.features)),
        image_size: cli.image_size.clone().or(recipe.image_size),
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
        // The shipped media-accel-rockchip feature adds no third-party apt source.
        assert!(b.apt_sources.is_empty());
        // Merged rootfs set: base packages + the feature's packages, base excludes.
        assert!(b.rootfs_packages.contains(&"openssh-server".to_string()));
        assert!(b.rootfs_packages.contains(&"ffmpeg-rk".to_string()));
        assert_eq!(b.rootfs_exclude, vec!["isc-dhcp-client"]);
        assert_eq!(b.kernel.id, "rk3588-mainline-7.1");
        assert_eq!(b.uboot_ref, "v2026.04");
        assert_eq!(b.kernel_dtb, "rockchip/rk3588-turing-rk1.dtb");
        assert_eq!(b.offsets.idbloader, "32KiB");
        assert_eq!(b.offsets.uboot_itb, "8MiB");
        assert_eq!(b.offsets.rootfs, "16MiB");
        assert!(b.modules.contains(&"rga3".to_string()));
        assert!(b.modules.contains(&"rkvenc".to_string()));
        // kernel fragments precede device fragments in apply order; the generated
        // Debian baseline is first, then the curated rockchip slices.
        assert_eq!(
            b.kernel.config_fragments,
            vec![
                "base/debian-arm64",
                "soc/rk3588",
                "accel/full",
                "device/turing-rk1"
            ]
        );
        assert_eq!(b.rkbin.atf, "rk3588_bl31_v1.51.elf");
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
        }
    }

    fn src(name: &str, uri: &str) -> AptSource {
        AptSource {
            name: name.into(),
            uri: uri.into(),
            suite: "trixie".into(),
            components: vec!["main".into()],
            signed_by: "k.gpg".into(),
        }
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
        assert_eq!(b.kernel.id, "k2");
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
        assert_eq!(b.kernel.id, "k2");
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
}
