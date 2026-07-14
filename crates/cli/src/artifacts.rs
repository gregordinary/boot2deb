//! The output dir's artifact ledger and the kernel package it names.
//!
//! The rootfs stage stands up a `[trusted=yes]` local apt repo from the `.deb`s the
//! compile stages produced. Its input set is this explicit ledger — the artifacts the
//! build recorded — never an extension-only scan of the output dir, so a stray or
//! half-written `.deb` cannot become trusted apt input.

use std::collections::BTreeSet;
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

/// Package name of each `.deb` — its file name up to the first `_` (dpkg forbids
/// `_` in package names, so `<package>_<version>_<arch>.deb` splits unambiguously).
fn deb_package_names(debs: &[PathBuf]) -> Vec<String> {
    debs.iter()
        .filter_map(|d| d.file_name()?.to_str()?.split('_').next().map(String::from))
        .collect()
}

/// The `linux-image-*` package name(s) the rootfs stage installs on top of the
/// resolved package set. The kernel is a build artifact whose package name
/// embeds a version the static config cannot name, so it is installed by the name
/// discovered from the built `.deb`.
///
/// To keep the install reproducible — a function of the current lock, not of
/// residue in `out_dir` — the kernel built in *this* run (`kernel_image_deb`) is
/// authoritative when the kernel stage ran here. For a standalone `--stage rootfs`
/// (kernel built by a prior invocation) the name is taken from `out_dir`, but only
/// when unambiguous: exactly one distinct `linux-image-*` package. Several distinct
/// kernel packages — stale debs from builds of different kernel versions sharing an
/// `out_dir` — are a hard error rather than a silent, non-reproducible guess.
pub(crate) fn kernel_packages(
    kernel_image_deb: &Option<PathBuf>,
    repo_debs: &[PathBuf],
) -> Result<Vec<String>, Box<dyn std::error::Error>> {
    if let Some(deb) = kernel_image_deb {
        return Ok(deb_package_names(std::slice::from_ref(deb)));
    }
    let mut names: Vec<String> = deb_package_names(repo_debs)
        .into_iter()
        .filter(|p| p.starts_with("linux-image-"))
        .collect();
    names.sort();
    names.dedup();
    if names.len() > 1 {
        return Err(format!(
            "multiple kernel packages in the output dir ({}) — cannot pick one for the rootfs. \
             Rebuild the kernel this run (build --stage all) or `clean` the stale debs first.",
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
