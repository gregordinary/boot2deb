//! The output dir's artifact ledger and the kernel package it names.
//!
//! The rootfs stage stands up a `[trusted=yes]` local apt repo from the `.deb`s the
//! compile stages produced. Its input set is this explicit ledger — the artifacts the
//! build recorded — never an extension-only scan of the output dir, so a stray or
//! half-written `.deb` cannot become trusted apt input.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// Name of the artifact ledger written into `out_dir` — the explicit allowlist of
/// `.deb`s this build produced. The rootfs stage's local apt repo ingests exactly
/// the invocation's own recorded outputs, never every `*.deb` that happens to sit in
/// `out_dir`: the repo emits `[trusted=yes]`, so an unsigned stray or a
/// leftover from another build must not become trusted apt input. Persisted in
/// `out_dir` so a later `--stage rootfs` run still sees the compile stages' outputs
/// recorded by an earlier invocation.
const ARTIFACT_LEDGER: &str = ".boot2deb-artifacts";

/// Record each produced `.deb` into the `out_dir` artifact ledger,
/// idempotently: the ledger is the set of file names the build staged into
/// `out_dir`, rewritten sorted so the file is deterministic. Paths not directly
/// under `out_dir` are ignored — the ledger names local-repo inputs, which every
/// stage stages into `out_dir`.
pub(crate) fn record_artifacts(
    out_dir: &Path,
    debs: &[PathBuf],
) -> Result<(), Box<dyn std::error::Error>> {
    let ledger = out_dir.join(ARTIFACT_LEDGER);
    let mut names: BTreeSet<String> = read_ledger_names(&ledger)?;
    for deb in debs {
        // Only debs staged directly under out_dir belong in the ledger.
        let in_out_dir = deb.parent() == Some(out_dir);
        if let (true, Some(name)) = (in_out_dir, deb.file_name().and_then(|n| n.to_str())) {
            names.insert(name.to_string());
        }
    }
    let body = names.into_iter().collect::<Vec<_>>().join("\n");
    std::fs::write(&ledger, body)
        .map_err(|source| format!("cannot write artifact ledger {} ({source})", ledger.display()))?;
    Ok(())
}

/// The ledger's recorded file names, or an empty set if the ledger does not exist.
fn read_ledger_names(ledger: &Path) -> Result<BTreeSet<String>, Box<dyn std::error::Error>> {
    match std::fs::read_to_string(ledger) {
        Ok(text) => Ok(text.lines().map(str::trim).filter(|l| !l.is_empty()).map(String::from).collect()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
        Err(source) => Err(format!("cannot read artifact ledger {} ({source})", ledger.display()).into()),
    }
}

/// The `.deb`s the build recorded in the `out_dir` artifact ledger that still exist,
/// sorted — the local apt repo's trusted input set. Unlike an
/// extension-only scan, a stray or partially-written `.deb` the build did not record
/// is never ingested. A missing ledger (no compile stage staged into this `out_dir`)
/// is a hard error with the same "run the compile stages first" hint the scan gave.
///
/// Only call this for a build that **produces** `.deb`s. One that compiles nothing —
/// a distro kernel on a board whose firmware is its own — has an empty ledger as its
/// correct state, and every package, kernel included, comes from the mirror.
pub(crate) fn ledger_debs(out_dir: &Path) -> Result<Vec<PathBuf>, Box<dyn std::error::Error>> {
    let ledger = out_dir.join(ARTIFACT_LEDGER);
    let names = read_ledger_names(&ledger)?;
    let mut debs: Vec<PathBuf> = names
        .into_iter()
        .map(|n| out_dir.join(n))
        .filter(|p| p.exists())
        .collect();
    // Empty means either no ledger, or the recorded debs are all gone — either way
    // there is nothing to seed the local repo, so fail with the compile-stage hint
    // rather than bootstrap against an empty repo.
    if debs.is_empty() {
        return Err(format!(
            "no recorded build artifacts in {} — run the compile stages first \
             (e.g. `build --stage all`, or `--stage kernel/uboot/userspace/ffmpeg`)",
            out_dir.display()
        )
        .into());
    }
    debs.sort();
    Ok(debs)
}

/// The package name of a `.deb` — its file name up to the first `_` (dpkg forbids `_`
/// in package names, so `<package>_<version>_<arch>.deb` splits unambiguously). `None`
/// for a path with no file name.
fn deb_package_name(deb: impl AsRef<Path>) -> Option<String> {
    deb.as_ref()
        .file_name()?
        .to_str()?
        .split_once('_')
        .map(|(name, _)| name.to_string())
}

/// The version field of a `.deb` — the `<version>` in `<package>_<version>_<arch>.deb`
/// (dpkg forbids `_` in both the package name and the version, so the three fields split
/// unambiguously). `None` for a file name that is not a well-formed three-field `.deb`.
fn deb_version(deb: impl AsRef<Path>) -> Option<String> {
    let stem = deb.as_ref().file_name()?.to_str()?.strip_suffix(".deb")?;
    let (_name, rest) = stem.split_once('_')?;
    let (version, _arch) = rest.split_once('_')?;
    Some(version.to_string())
}

/// Scope `repo_debs` to the kernel and out-of-tree modules this build produced, dropping
/// any stale `linux-image-*` / `linux-headers-*` / `<driver>-modules-*` deb of a
/// *different* version than the one built this run.
///
/// The local apt repo is `--multiversion`, and both rootfs backends resolve a bare
/// package name highest-version-wins (apt for mmdebstrap; the provisioner's index
/// likewise). So a stale higher-versioned deb an earlier build left in `out_dir` would
/// outrank the one this build just compiled — a silent wrong-kernel install and, against
/// an out-of-tree kmod, a modversions-CRC-mismatched `.ko` that will not load. This is
/// not hypothetical: a kernel's `git describe` version *regresses* when patches are
/// dropped, so a newer build can sort *below* older residue.
///
/// When a stage ran this run its exact version is authoritative — keep only that, so the
/// current artifact is the sole candidate for both the repo index and the by-name
/// install. `linux-image-*` and `linux-headers-*` share the kernel deb's version, so the
/// kernel image scopes both; each module package pins its own. Packages this build did
/// not produce (u-boot, the mirror's own) are untouched. A no-op for a stage that did
/// not run this run (a standalone `--stage rootfs`); [`kernel_packages`] and
/// [`kmod_packages`] instead refuse an ambiguous `out_dir`.
pub(crate) fn scope_repo_to_current_artifacts(
    repo_debs: &mut Vec<PathBuf>,
    kernel_image_deb: &Option<PathBuf>,
    kmod_debs: &[PathBuf],
) {
    let kernel_ver = kernel_image_deb.as_ref().and_then(deb_version);
    let kmod_ver: BTreeMap<String, String> = kmod_debs
        .iter()
        .filter_map(|d| Some((deb_package_name(d)?, deb_version(d)?)))
        .collect();
    repo_debs.retain(|d| {
        let Some(name) = deb_package_name(d) else {
            return true;
        };
        if name.starts_with("linux-image-") || name.starts_with("linux-headers-") {
            return match &kernel_ver {
                Some(cur) => deb_version(d).as_deref() == Some(cur.as_str()),
                None => true,
            };
        }
        match kmod_ver.get(&name) {
            Some(cur) => deb_version(d).as_deref() == Some(cur.as_str()),
            None => true,
        }
    });
}

/// The `linux-image-*` package name the rootfs stage installs on top of the resolved
/// package set. The kernel is a build artifact whose package name embeds a version the
/// static config cannot name, so it is installed by the name discovered from the built
/// `.deb`. Which *version* that bare name resolves to is made deterministic upstream by
/// [`scope_repo_to_current_artifacts`], which drops stale other-version kernel debs from
/// the repo so the install cannot land on higher-versioned residue.
///
/// The kernel built in *this* run (`kernel_image_deb`) is authoritative when the kernel
/// stage ran here. For a standalone `--stage rootfs` (kernel built by a prior
/// invocation) the name is taken from `out_dir`, but only when unambiguous: a single
/// `linux-image-*` name *and* version. Several distinct names or versions — stale debs
/// from earlier builds sharing an `out_dir`, which a `--multiversion` repo would resolve
/// highest-wins — are a hard error rather than a silent, non-reproducible guess.
pub(crate) fn kernel_packages(
    kernel_image_deb: &Option<PathBuf>,
    repo_debs: &[PathBuf],
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    if let Some(deb) = kernel_image_deb {
        return Ok(deb_package_name(deb).into_iter().collect());
    }
    let kernels: Vec<&PathBuf> = repo_debs
        .iter()
        .filter(|d| deb_package_name(d).is_some_and(|n| n.starts_with("linux-image-")))
        .collect();
    let distinct: BTreeSet<(String, String)> = kernels
        .iter()
        .filter_map(|d| Some((deb_package_name(d)?, deb_version(d)?)))
        .collect();
    if distinct.len() > 1 {
        let mut listed: Vec<String> = kernels
            .iter()
            .filter_map(|d| d.file_name()?.to_str().map(String::from))
            .collect();
        listed.sort();
        return Err(format!(
            "multiple kernel packages in the output dir ({}) — cannot pick one for the rootfs. \
             Rebuild the kernel this run (build --stage all) or `clean` the stale debs first.",
            listed.join(", ")
        )
        .into());
    }
    Ok(distinct.into_iter().map(|(name, _)| name).collect())
}

/// The out-of-tree kernel-module package name(s) the rootfs stage installs on top of
/// the resolved set — the per-kernel `<driver>-modules-<kver>` deb and, when the kmod
/// ships firmware, the companion `<driver>-firmware` deb. Like the kernel, a modules
/// `.deb`'s package name embeds a kernel-release version the static config cannot name,
/// so it is installed by the name discovered from the built `.deb`;
/// [`scope_repo_to_current_artifacts`] drops stale other-version module debs from the
/// repo so the bare-name install resolves to this build's.
///
/// The debs built in *this* run (`kmod_debs`) are authoritative. For a standalone
/// `--stage rootfs` (built by a prior invocation) the names come from the ledger, matched
/// by the `-modules-` infix or `-firmware` suffix a kmod `.deb` carries. A board may
/// declare several modules, so — unlike the single kernel — a count above one is not an
/// error; but modules pinned to *two* kernel releases sharing an `out_dir` are stale
/// residue that would each pull a different `linux-image`, so that is refused, mirroring
/// [`kernel_packages`]'s stale-version guard.
pub(crate) fn kmod_packages(
    kmod_debs: &[PathBuf],
    repo_debs: &[PathBuf],
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    if !kmod_debs.is_empty() {
        return Ok(kmod_debs.iter().filter_map(deb_package_name).collect());
    }
    let mut names: Vec<String> = repo_debs
        .iter()
        .filter_map(deb_package_name)
        .filter(|n| n.contains("-modules-") || n.ends_with("-firmware"))
        .collect();
    names.sort();
    names.dedup();
    let kvers: BTreeSet<&str> = names
        .iter()
        .filter_map(|n| n.split_once("-modules-").map(|(_, kver)| kver))
        .collect();
    if kvers.len() > 1 {
        return Err(format!(
            "multiple kernel-module package versions in the output dir ({}) — cannot pick a \
             consistent set for the rootfs. Rebuild the kmods this run (build --stage all) or \
             `clean` the stale debs first.",
            names.join(", ")
        )
        .into());
    }
    Ok(names)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kernel_packages_prefers_this_runs_artifact() {
        // When the kernel stage ran this run, its exact .deb is authoritative and
        // stale linux-image debs in out_dir are ignored — no ambiguity, no scan.
        let built = PathBuf::from("/out/linux-image-6.12.0-1-arm64_1_arm64.deb");
        let repo = vec![
            built.clone(),
            PathBuf::from("/out/linux-image-6.9.0-1-arm64_1_arm64.deb"),
            PathBuf::from("/out/u-boot-turing-rk1_1_arm64.deb"),
        ];
        let pkgs = kernel_packages(&Some(built), &repo).unwrap();
        assert_eq!(pkgs, vec!["linux-image-6.12.0-1-arm64".to_string()]);
    }

    #[test]
    fn scope_repo_drops_stale_higher_versioned_kernel_and_headers() {
        // The regression: an earlier build left a *higher*-versioned kernel (+ headers)
        // of the same package name in out_dir — a daily build with a larger `git
        // describe` count than this run's, whose count dropped when patches were removed.
        // A `--multiversion` repo resolves the bare name highest-wins, so the stale deb
        // would be installed; scoping to this run's kernel version drops it.
        let built =
            PathBuf::from("/out/linux-image-7.1.3-1-arm64_7.1.3-00002-g842be0c-1_arm64.deb");
        let built_headers =
            PathBuf::from("/out/linux-headers-7.1.3-1-arm64_7.1.3-00002-g842be0c-1_arm64.deb");
        let stale_image =
            PathBuf::from("/out/linux-image-7.1.3-1-arm64_7.1.3-00008-gfcbf808-1_arm64.deb");
        let stale_headers =
            PathBuf::from("/out/linux-headers-7.1.3-1-arm64_7.1.3-00008-gfcbf808-1_arm64.deb");
        let uboot = PathBuf::from("/out/u-boot-h96-max-m9_2026.04_arm64.deb");
        let kmod = PathBuf::from("/out/aic8800-modules-7.1.3-1-arm64_0~main+gabc_arm64.deb");
        let mut repo = vec![
            built.clone(),
            built_headers.clone(),
            stale_image.clone(),
            stale_headers.clone(),
            uboot.clone(),
            kmod.clone(),
        ];
        scope_repo_to_current_artifacts(&mut repo, &Some(built.clone()), std::slice::from_ref(&kmod));
        // Only this run's kernel version survives; u-boot and the kmod are untouched.
        assert!(repo.contains(&built) && repo.contains(&built_headers));
        assert!(!repo.contains(&stale_image) && !repo.contains(&stale_headers));
        assert!(repo.contains(&uboot) && repo.contains(&kmod));
        // The by-name kernel install now resolves unambiguously to the survivor.
        let pkgs = kernel_packages(&Some(built), &repo).unwrap();
        assert_eq!(pkgs, vec!["linux-image-7.1.3-1-arm64".to_string()]);
    }

    #[test]
    fn scope_repo_drops_a_stale_module_version_but_keeps_other_packages() {
        // A kmod's version can regress the same way; scope by this run's module version.
        let built_mod = PathBuf::from("/out/aic8800-modules-7.1.3-1-arm64_0~main+g0002_arm64.deb");
        let stale_mod = PathBuf::from("/out/aic8800-modules-7.1.3-1-arm64_0~main+g0008_arm64.deb");
        let uboot = PathBuf::from("/out/u-boot-h96-max-m9_2026.04_arm64.deb");
        let mut repo = vec![built_mod.clone(), stale_mod.clone(), uboot.clone()];
        scope_repo_to_current_artifacts(&mut repo, &None, std::slice::from_ref(&built_mod));
        assert!(repo.contains(&built_mod) && repo.contains(&uboot));
        assert!(!repo.contains(&stale_mod));
    }

    #[test]
    fn scope_repo_is_a_noop_without_this_runs_artifacts() {
        // Standalone --stage rootfs: the current version is not known here, so nothing is
        // dropped; kernel_packages guards the ambiguity instead.
        let mut repo = vec![
            PathBuf::from("/out/linux-image-7.1.3-1-arm64_7.1.3-00002-gaaa-1_arm64.deb"),
            PathBuf::from("/out/linux-image-7.1.3-1-arm64_7.1.3-00008-gbbb-1_arm64.deb"),
        ];
        let before = repo.clone();
        scope_repo_to_current_artifacts(&mut repo, &None, &[]);
        assert_eq!(repo, before);
    }

    #[test]
    fn kernel_packages_standalone_errors_on_two_versions_of_one_name() {
        // Same package name, two versions (residue a --multiversion repo resolves
        // highest-wins) — refused rather than silently guessed.
        let repo = vec![
            PathBuf::from("/out/linux-image-7.1.3-1-arm64_7.1.3-00002-gaaa-1_arm64.deb"),
            PathBuf::from("/out/linux-image-7.1.3-1-arm64_7.1.3-00008-gbbb-1_arm64.deb"),
        ];
        let err = kernel_packages(&None, &repo).unwrap_err().to_string();
        assert!(err.contains("multiple kernel packages"), "{err}");
    }

    #[test]
    fn kernel_packages_standalone_uses_sole_kernel_deb() {
        // Standalone --stage rootfs: exactly one kernel deb in out_dir is unambiguous.
        let repo = vec![
            PathBuf::from("/out/linux-image-6.12.0-1-arm64_1_arm64.deb"),
            PathBuf::from("/out/u-boot-turing-rk1_1_arm64.deb"),
        ];
        let pkgs = kernel_packages(&None, &repo).unwrap();
        assert_eq!(pkgs, vec!["linux-image-6.12.0-1-arm64".to_string()]);
    }

    #[test]
    fn kernel_packages_standalone_errors_on_stale_ambiguity() {
        // Two distinct kernel versions from earlier builds sharing an out_dir must
        // not be silently guessed — the rootfs stage refuses rather than pick one.
        let repo = vec![
            PathBuf::from("/out/linux-image-6.12.0-1-arm64_1_arm64.deb"),
            PathBuf::from("/out/linux-image-6.9.0-1-arm64_1_arm64.deb"),
        ];
        let err = kernel_packages(&None, &repo).unwrap_err().to_string();
        assert!(err.contains("multiple kernel packages"), "{err}");
    }

    #[test]
    fn kernel_packages_none_when_no_kernel_deb() {
        let repo = vec![PathBuf::from("/out/u-boot-turing-rk1_1_arm64.deb")];
        assert!(kernel_packages(&None, &repo).unwrap().is_empty());
    }

    #[test]
    fn kmod_packages_prefers_this_runs_artifacts() {
        // Modules built this run are authoritative; stale kmod debs in out_dir ignored.
        let built = vec![
            PathBuf::from("/out/aic8800-modules-7.1.3-1-arm64_1_arm64.deb"),
        ];
        let repo = vec![
            built[0].clone(),
            PathBuf::from("/out/aic8800-modules-6.9.0-1-arm64_1_arm64.deb"),
            PathBuf::from("/out/linux-image-7.1.3-1-arm64_1_arm64.deb"),
        ];
        let pkgs = kmod_packages(&built, &repo).unwrap();
        assert_eq!(pkgs, vec!["aic8800-modules-7.1.3-1-arm64".to_string()]);
    }

    #[test]
    fn kmod_packages_standalone_uses_ledger_by_infix() {
        // Standalone --stage rootfs: the modules deb (by `-modules-`) and the companion
        // firmware deb (by `-firmware`) from a prior run are both picked up; the kernel
        // and other debs are not mistaken for either.
        let repo = vec![
            PathBuf::from("/out/aic8800-modules-7.1.3-1-arm64_1_arm64.deb"),
            PathBuf::from("/out/aic8800-firmware_0~main+gabc_all.deb"),
            PathBuf::from("/out/linux-image-7.1.3-1-arm64_1_arm64.deb"),
            PathBuf::from("/out/u-boot-h96-max-m9_1_arm64.deb"),
        ];
        let pkgs = kmod_packages(&[], &repo).unwrap();
        assert_eq!(
            pkgs,
            vec!["aic8800-firmware".to_string(), "aic8800-modules-7.1.3-1-arm64".to_string()]
        );
    }

    #[test]
    fn kmod_packages_standalone_errors_on_stale_kver_mix() {
        // Modules pinned to two kernel releases in one out_dir are stale residue.
        let repo = vec![
            PathBuf::from("/out/aic8800-modules-7.1.3-1-arm64_1_arm64.deb"),
            PathBuf::from("/out/aic8800-modules-6.9.0-1-arm64_1_arm64.deb"),
        ];
        let err = kmod_packages(&[], &repo).unwrap_err().to_string();
        assert!(err.contains("multiple kernel-module package versions"), "{err}");
    }

    #[test]
    fn kmod_packages_none_when_no_modules() {
        let repo = vec![PathBuf::from("/out/linux-image-7.1.3-1-arm64_1_arm64.deb")];
        assert!(kmod_packages(&[], &repo).unwrap().is_empty());
    }

    #[test]
    fn ledger_ingests_only_recorded_debs_not_strays() {
        // The local repo seed is the recorded artifact set, never an
        // extension-only scan — a stray .deb dropped into out_dir is not ingested.
        let dir = tempfile::tempdir().unwrap();
        let out = dir.path();
        let recorded = out.join("librockchip-mpp1_1.5.0-1_arm64.deb");
        std::fs::write(&recorded, b"deb").unwrap();
        record_artifacts(out, std::slice::from_ref(&recorded)).unwrap();
        // Recording is idempotent (re-recording the same deb keeps one entry).
        record_artifacts(out, std::slice::from_ref(&recorded)).unwrap();
        // A stray unsigned deb the build never recorded.
        std::fs::write(out.join("evil_1.0_arm64.deb"), b"deb").unwrap();

        let debs = ledger_debs(out).unwrap();
        assert_eq!(debs, vec![recorded.clone()], "only the recorded deb is ingested");

        // A recorded deb whose file was removed is silently skipped.
        std::fs::remove_file(&recorded).unwrap();
        assert!(ledger_debs(out).is_err(), "empty existing set is an error");
    }

    #[test]
    fn ledger_missing_is_a_clear_error() {
        // No compile stage staged into this out_dir → a hard error pointing at the
        // compile stages, not a silent empty repo.
        let dir = tempfile::tempdir().unwrap();
        let err = ledger_debs(dir.path()).unwrap_err().to_string();
        assert!(err.contains("run the compile stages first"), "{err}");
    }
}
