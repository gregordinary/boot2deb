//! Trusted Firmware-A compile stage: the BL31 a build point packs into `u-boot.itb`
//! when its [`Bl31Provider`](boot2deb_core::model::Bl31Provider) is `tfa`.
//!
//! It clones the pinned TF-A commit and builds `make PLAT=<plat> bl31` in the cross
//! root. The u-boot stage receives the resulting `bl31.elf` and TF-A's license text,
//! which the u-boot deb ships. Neither supported SoC needs TF-A's M0
//! co-processor toolchain. The aarch64 cross compiler u-boot already uses is the whole
//! toolchain, so the stage layers no build-dependencies over the cross root.
//!
//! The output is reproducible. TF-A embeds a version string and a build timestamp,
//! and both are passed explicitly, derived from the pinned ref, commit and commit
//! date. Neither `git describe` nor the wall clock reaches the binary.
//!
//! Linux side effects, like every compile stage: a clone, a sandboxed `make`, and the
//! Tier-1 tree and Tier-2 artifact caches the other stages use.

use crate::build::{self, BuildEnv};
use crate::error::EngineError;
use crate::event::{EventSink, Step};
use crate::sandbox::{BuildRoot, BuildRootSpec, BuildSandbox, CompileRoot};
use boot2deb_core::lock::{Lock, TfaPin};
use boot2deb_core::model::TfaSource;
use boot2deb_core::ResolvedBuild;
use std::path::{Path, PathBuf};

/// The artifact-store node this stage keys its outputs under, and the label
/// [`why-rebuild`](crate::plan) predicts against. One constant, so the two cannot
/// name different entries.
pub const NODE: &str = "tfa";

/// Stage-recipe version for the TF-A tree signature: bump when the clone logic that
/// shapes the reused tree changes.
const CLONE_STAGE_VERSION: u32 = 1;

/// Stage-recipe version for the TF-A output signature: bump when the `make`
/// invocation or the collected outputs change in a way the folded inputs do not
/// already capture. The release build (`DEBUG=0`) at TF-A's default `LOG_LEVEL` is
/// part of what this version stands for.
const OUTPUT_STAGE_VERSION: u32 = 1;

/// How many hex digits of the commit the embedded version string carries.
const BUILD_STRING_ABBREV: usize = 12;

/// The TF-A source tree this stage clones and reuses under `work_dir`
/// (`<work_dir>/tf-a`). Public so a reader of the tree, such as [`crate::shell`],
/// does not restate the layout literal.
pub fn tree_dir(work_dir: &Path) -> PathBuf {
    work_dir.join("tf-a")
}

/// Where this stage leaves its outputs for the u-boot stage (`<work_dir>/tfa-out`).
/// Under the work dir, so the u-boot stage's compile root, which binds the work dir,
/// sees `bl31.elf` at the same absolute path.
fn outputs_dir(work_dir: &Path) -> PathBuf {
    work_dir.join("tfa-out")
}

/// The BL31 image, as TF-A names it and as this stage stores it.
const BL31_ELF: &str = "bl31.elf";

/// TF-A's license text, as this stage stores it. Read from `docs/license.rst`.
const LICENSE: &str = "license.rst";

/// Filesystem inputs for the TF-A stage.
pub struct TfaOptions<'a> {
    /// Git URL or local path to clone TF-A from, at the locked ref. Defaults to the
    /// SoC's `[tfa].git`. A local clone speeds the shallow clone.
    pub source: &'a str,
    /// Scratch directory holding the TF-A clone and the stage's outputs. The u-boot
    /// stage's work dir, so its compile root reaches `bl31.elf`.
    pub work_dir: &'a Path,
    /// The host-arch cross root `make` runs in
    /// ([`SandboxRole::Cross`](crate::sandbox::SandboxRole::Cross)), the same root the
    /// u-boot stage compiles in. Bootstrapped lazily, so a Tier-2 hit provisions none.
    pub cross: &'a dyn BuildSandbox,
    /// Root of the Tier-2 artifact store ([`crate::artstore`]), or `None` to disable
    /// output caching.
    pub store: Option<&'a Path>,
}

/// What the TF-A stage hands the u-boot stage.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TfaArtifacts {
    /// The built BL31 ELF, passed to u-boot as `BL31=`.
    pub bl31: PathBuf,
    /// TF-A's license text, which the u-boot deb's `copyright` reproduces. Its
    /// BSD-3-Clause terms require the notice to travel with a binary distribution.
    pub license: PathBuf,
}

/// Run the TF-A stage, emitting its [`Event`](crate::event::Event)s to `sink`.
///
/// Reads the lock's `[tfa]` pin. A fresh clone is verified against the locked commit.
/// A reused tree is `realclean`ed before every build, so an output never mixes objects
/// from two configurations.
pub fn build_tfa(
    build: &ResolvedBuild,
    lock: &Lock,
    opts: &TfaOptions,
    env: &BuildEnv,
    sink: &dyn EventSink,
) -> Result<TfaArtifacts, EngineError> {
    let src = tfa_source(build)?;
    let pin = tfa_pin(lock)?;
    let step = Step::start(sink, NODE);
    let outputs = outputs_dir(opts.work_dir);

    // Tier-2: restore `bl31.elf` and the license and skip the clone and compile. The
    // outputs dir is emptied first, so a partial restore leaves nothing a later run
    // could take for this signature's output.
    let out_man = output_manifest(src, lock, env)?;
    let _ = std::fs::remove_dir_all(&outputs);
    if let Some([bl31, license]) = build::restore_stage_outputs(
        opts.store,
        NODE,
        &out_man.signature(),
        &outputs,
        &["bl31", "license"],
        &step,
    )?
    .as_deref()
    {
        step.progress(100);
        step.finish();
        return Ok(TfaArtifacts {
            bl31: bl31.clone(),
            license: license.clone(),
        });
    }

    let tree = tree_dir(opts.work_dir);
    let man = clone_manifest(src, lock)?;
    build::reuse_or_refresh_tree(&tree, &man, "TF-A", &step, || {
        clone_verified(opts.source, pin, &src.submodules, &tree, &step)
    })?;
    step.progress(20);

    let (root, binds) = compile_root(opts, &step)?;
    let cr = CompileRoot {
        root: &root,
        binds: &binds,
    };
    let epoch = crate::git::commit_epoch(&tree, &pin.commit)?;
    let vars = cross(env);
    let clean = vec![
        "make".to_string(),
        "-C".to_string(),
        tree.display().to_string(),
        "realclean".to_string(),
    ];
    build::run_in_root(&cr, &tree, &clean, &vars, "make realclean", &step)?;
    step.progress(30);
    let argv = make_argv(&tree, src, pin, epoch, env.jobs());
    build::run_in_root(
        &cr,
        &tree,
        &argv,
        &vars,
        &format!("make PLAT={} bl31", src.plat),
        &step,
    )?;
    step.progress(90);

    let artifacts = collect(&tree, &src.plat, &outputs)?;
    step.log(format!(
        "built BL31 {} for PLAT={}",
        build_string(pin),
        src.plat
    ));
    build::store_stage_outputs(
        opts.store,
        NODE,
        &out_man.signature(),
        &[
            ("bl31", artifacts.bl31.as_path()),
            ("license", artifacts.license.as_path()),
        ],
        &step,
    )?;
    step.progress(100);
    step.finish();
    Ok(artifacts)
}

/// The resolved TF-A source, or [`EngineError::StageNotApplicable`] for a build
/// point that builds no u-boot or packs the rkbin BL31.
fn tfa_source(build: &ResolvedBuild) -> Result<&TfaSource, EngineError> {
    let boot = build.rkbin_boot().ok_or(EngineError::StageNotApplicable {
        stage: NODE,
        why: "this board's boot method builds no bootloader — its firmware is its own",
    })?;
    boot.bl31.tfa().ok_or(EngineError::StageNotApplicable {
        stage: NODE,
        why: "this build point packs the rkbin BL31, a prebuilt blob that is not compiled",
    })
}

/// The lock's `[tfa]` pin, or a typed error for a lock that has none.
pub(crate) fn tfa_pin(lock: &Lock) -> Result<&TfaPin, EngineError> {
    lock.tfa.as_ref().ok_or(EngineError::MissingPin {
        what: "tfa",
        stage: NODE,
    })
}

/// The version string TF-A embeds in place of its `git describe` default:
/// `<ref>-g<commit prefix>`, the same shape `git describe` gives for a tag with no
/// commits on top. Characters outside `[A-Za-z0-9._+-]` become `_`, since TF-A's
/// Makefile places the value inside a single-quoted shell word.
pub fn build_string(pin: &TfaPin) -> String {
    let reference: String = pin
        .reference
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '+' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let short: String = pin.commit.chars().take(BUILD_STRING_ABBREV).collect();
    format!("{reference}-g{short}")
}

/// The `make` invocation that builds BL31.
///
/// `BUILD_STRING` and `BUILD_MESSAGE_TIMESTAMP` are the two values TF-A otherwise
/// takes from the build host: `git describe --dirty` and the C preprocessor's
/// `__TIME__`/`__DATE__`. Passing both makes the binary a function of the lock. The
/// timestamp is the commit's date in RFC 3339 UTC, and is a C string literal because
/// TF-A splices it into one (`"Built : " BUILD_MESSAGE_TIMESTAMP`).
fn make_argv(tree: &Path, src: &TfaSource, pin: &TfaPin, epoch: u64, jobs: usize) -> Vec<String> {
    vec![
        "make".to_string(),
        "-C".to_string(),
        tree.display().to_string(),
        format!("-j{jobs}"),
        format!("PLAT={}", src.plat),
        format!("BUILD_STRING={}", build_string(pin)),
        format!(
            "BUILD_MESSAGE_TIMESTAMP=\"{}\"",
            boot2deb_core::datetime::format_rfc3339(epoch)
        ),
        "bl31".to_string(),
    ]
}

/// Copy `bl31.elf` and the license text out of the tree into `outputs`, so a later
/// `realclean` of a reused tree cannot remove what the u-boot stage reads.
fn collect(tree: &Path, plat: &str, outputs: &Path) -> Result<TfaArtifacts, EngineError> {
    let bl31_src = tree.join(format!("build/{plat}/release/bl31/{BL31_ELF}"));
    let license_src = tree.join("docs").join(LICENSE);
    for (what, path) in [(BL31_ELF, &bl31_src), (LICENSE, &license_src)] {
        if !path.is_file() {
            return Err(EngineError::ArtifactMissing {
                what: what.into(),
                location: tree.display().to_string(),
            });
        }
    }
    std::fs::create_dir_all(outputs).map_err(|s| EngineError::io(outputs, s))?;
    let bl31 = outputs.join(BL31_ELF);
    let license = outputs.join(LICENSE);
    for (from, to) in [(&bl31_src, &bl31), (&license_src, &license)] {
        std::fs::copy(from, to).map_err(|s| EngineError::io(from, s))?;
    }
    Ok(TfaArtifacts { bl31, license })
}

/// Shallow-clone the pinned TF-A ref, verify the clone is the locked commit, and fetch
/// the `submodules` the build reads. A failure removes the partial tree, so a later
/// run never reuses it.
///
/// Each submodule is checked out at the commit the TF-A tree records for it, so the
/// TF-A commit content-addresses the whole tree and the lock needs no pin of its own
/// for them. Fetched from the URL TF-A's `.gitmodules` names, whatever `source` is.
fn clone_verified(
    source: &str,
    pin: &TfaPin,
    submodules: &[String],
    tree: &Path,
    step: &Step,
) -> Result<(), EngineError> {
    let result = (|| {
        build::clone_shallow(source, &pin.reference, tree, step)?;
        let head = crate::git::rev_parse_head(tree)?;
        let expected = boot2deb_core::sources::normalize_ref(&pin.commit);
        if head != expected {
            return Err(EngineError::CommitMismatch {
                what: "TF-A".to_string(),
                expected,
                actual: head,
            });
        }
        if !submodules.is_empty() {
            let mut cmd = crate::git::command(Some(tree));
            build::bound_git_network(&mut cmd);
            cmd.args(["submodule", "update", "--init", "--depth", "1", "--"])
                .args(submodules);
            build::run(
                cmd,
                "git",
                &format!("fetch TF-A submodules {}", submodules.join(", ")),
                step,
            )?;
        }
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(tree);
    }
    result
}

/// The Tier-1 signature manifest of the cloned TF-A tree. It folds the commit, which
/// content-addresses the tree and every submodule it records, and the submodules
/// fetched. The source URL is excluded, since the commit names the content.
pub fn clone_manifest(
    src: &TfaSource,
    lock: &Lock,
) -> Result<crate::signature::SignatureManifest, EngineError> {
    let pin = tfa_pin(lock)?;
    let mut b = crate::signature::SignatureBuilder::new("tfa", CLONE_STAGE_VERSION);
    b.fold_scalar("tfa.commit", &pin.commit)
        .fold_set("submodules", &src.submodules);
    Ok(b.manifest())
}

/// The Tier-2 output signature manifest of `bl31.elf` and the license text.
///
/// It folds the Tier-1 tree signature and every other input that shapes the binary.
/// Those are the platform, the embedded version string, the cross prefix and the cross
/// root's identity. The timestamp is derived from the commit the tree signature already
/// folds, so it adds nothing to fold.
///
/// It is also the BL31 identity the u-boot stage folds for a `tfa` build. Folding
/// inputs rather than output bytes lets a u-boot cache hit be decided without
/// building TF-A first. Public so `why-rebuild` ([`crate::plan`]) asks the same
/// question this stage does.
pub fn output_manifest(
    src: &TfaSource,
    lock: &Lock,
    env: &BuildEnv,
) -> Result<crate::signature::SignatureManifest, EngineError> {
    let tree_sig = clone_manifest(src, lock)?.signature();
    let pin = tfa_pin(lock)?;
    let mut b = crate::signature::SignatureBuilder::new("tfa:out", OUTPUT_STAGE_VERSION);
    b.fold_dep(&tree_sig)
        .fold_scalar("plat", &src.plat)
        .fold_scalar("build_string", &build_string(pin))
        .fold_scalar("cross", env.cross_compile.as_deref().unwrap_or(""))
        .fold_scalar("toolchain", &env.toolchain_id);
    Ok(b.manifest())
}

/// Stand up the cross root this stage compiles in. TF-A needs nothing beyond the root's
/// toolchain and `make`, so no build-dependency layer is requested, and the work dir is
/// the one host path a command must see.
fn compile_root(opts: &TfaOptions, step: &Step) -> Result<(BuildRoot, Vec<PathBuf>), EngineError> {
    opts.cross.ensure_ready(step)?;
    let root = opts.cross.build_root(
        &BuildRootSpec {
            packages: &[],
            pool: None,
            stage: NODE,
        },
        step,
    )?;
    Ok((root, vec![opts.work_dir.to_path_buf()]))
}

/// `CROSS_COMPILE` as a one-entry environment, or empty for a native compile.
fn cross(env: &BuildEnv) -> Vec<(String, String)> {
    env.cross_compile
        .iter()
        .map(|prefix| ("CROSS_COMPILE".to_string(), prefix.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin() -> TfaPin {
        TfaPin {
            source: "https://example.invalid/trusted-firmware-a.git".into(),
            reference: "v2.16.0".into(),
            commit: "9347b8bbd8d56da5cfbf7d9f1af3ca4f6d6e3920".into(),
        }
    }

    fn src(plat: &str) -> TfaSource {
        TfaSource {
            git: "https://example.invalid/trusted-firmware-a.git".into(),
            git_ref: "v2.16.0".into(),
            plat: plat.into(),
            submodules: vec!["contrib/dtc".into()],
        }
    }

    fn lock_with(pin: TfaPin) -> Lock {
        Lock {
            kernel: None,
            patches: None,
            uboot: None,
            uboot_patches: None,
            tfa: Some(pin),
            userspace: vec![],
            ffmpeg: None,
            kmods: vec![],
            apps: vec![],
            rootfs: None,
            blobs: None,
            extra_debs: vec![],
            snapshot: None,
        }
    }

    #[test]
    fn the_version_string_names_the_ref_and_commit_and_nothing_shell_active() {
        assert_eq!(build_string(&pin()), "v2.16.0-g9347b8bbd8d5");
        let odd = TfaPin {
            reference: "it's/a ref".into(),
            ..pin()
        };
        assert_eq!(build_string(&odd), "it_s_a_ref-g9347b8bbd8d5");
    }

    #[test]
    fn the_make_invocation_pins_every_host_derived_value() {
        let argv = make_argv(
            Path::new("/w/tf-a"),
            &src("rk3576"),
            &pin(),
            1_767_225_600,
            8,
        );
        assert_eq!(
            argv,
            [
                "make",
                "-C",
                "/w/tf-a",
                "-j8",
                "PLAT=rk3576",
                "BUILD_STRING=v2.16.0-g9347b8bbd8d5",
                "BUILD_MESSAGE_TIMESTAMP=\"2026-01-01T00:00:00Z\"",
                "bl31",
            ]
        );
    }

    #[test]
    fn the_output_signature_moves_with_the_commit_and_the_platform_only() {
        let env = BuildEnv {
            cross_compile: Some("aarch64-linux-gnu-".into()),
            toolchain_id: "cross-a".into(),
            ..Default::default()
        };
        let sig = |s: &TfaSource, p: TfaPin, e: &BuildEnv| {
            output_manifest(s, &lock_with(p), e).unwrap().signature()
        };
        let base = sig(&src("rk3588"), pin(), &env);
        assert_eq!(base, sig(&src("rk3588"), pin(), &env), "deterministic");
        assert_ne!(base, sig(&src("rk3576"), pin(), &env), "platform");
        let moved = TfaPin {
            commit: "1".repeat(40),
            ..pin()
        };
        assert_ne!(base, sig(&src("rk3588"), moved, &env), "commit");
        let more = TfaSource {
            submodules: vec!["contrib/dtc".into(), "contrib/libtl".into()],
            ..src("rk3588")
        };
        assert_ne!(base, sig(&more, pin(), &env), "submodules");
        let other_root = BuildEnv {
            toolchain_id: "cross-b".into(),
            ..env.clone()
        };
        assert_ne!(base, sig(&src("rk3588"), pin(), &other_root), "toolchain");
        // The source URL is not an input: the commit content-addresses the tree.
        let mirror = TfaPin {
            source: "https://github.com/ARM-software/arm-trusted-firmware.git".into(),
            ..pin()
        };
        assert_eq!(base, sig(&src("rk3588"), mirror, &env));
    }

    #[test]
    fn a_lock_without_a_tfa_pin_is_a_typed_error() {
        let lock = Lock {
            tfa: None,
            ..lock_with(pin())
        };
        assert!(matches!(
            tfa_pin(&lock),
            Err(EngineError::MissingPin { what: "tfa", .. })
        ));
    }
}
