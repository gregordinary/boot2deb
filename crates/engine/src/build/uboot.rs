//! u-boot compile stage. It clones the pinned u-boot and applies the locked u-boot
//! patch series where the resolved device selects one. It then builds the board
//! defconfig with the sha256-verified rkbin DDR TPL and the selected BL31, and stages
//! the raw-gap payloads (`idbloader.img`, `u-boot.itb`).
//!
//! The BL31 is the rkbin blob, verified like the TPL, or the `bl31.elf` the
//! [`tfa`](crate::build::tfa) stage builds. This stage runs that one itself, after
//! its own artifact-cache probe misses, so a cached u-boot never builds TF-A. Either
//! way the BL31 reaches `make` as `BL31=`.
//!
//! RK3588 u-boot builds with the aarch64 toolchain (`CROSS_COMPILE` on a
//! non-arm64 host) and `CONFIG_ARM64=y` from the defconfig, so no `ARCH=` is
//! passed. The defconfig contains it. The blobs are verified against the lock's
//! hashes ([`crate::blobs`]) before `make` consumes them.

use crate::blobs;
use crate::build::{
    self, stage_artifact, BuildEnv, CloneMode, ClonePinned, PatchScope, PatchSource, SeriesIdentity,
};
use crate::error::EngineError;
use crate::event::{EventSink, Step};
use crate::sandbox::{BuildRoot, BuildRootSpec, BuildSandbox, CompileRoot, PackagingSandbox};
use boot2deb_core::lock::{Lock, TfaPin};
use boot2deb_core::model::{Bl31, ResolvedRkbinBoot};
use boot2deb_core::size::parse_size;
use boot2deb_core::ResolvedBuild;
use std::path::{Path, PathBuf};

/// The artifact-store node this stage keys its outputs under, and the label
/// [`why-rebuild`](crate::plan) predicts against.
///
/// A constant because the two have to be the *same string*. The store is keyed by
/// `(node, signature)`. A prediction computed under a different node name would
/// therefore answer a question about an entry no build ever wrote. It is the
/// counterpart of the
/// path helper above: one names where the tree is, this names where the artifacts are.
pub const NODE: &str = "uboot";

/// Stage-recipe version for the u-boot tree signature: bump when the
/// clone/patch logic that shapes the reused tree changes.
const CLONE_STAGE_VERSION: u32 = 1;

/// The u-boot source tree the [`build_uboot`] stage clones and reuses under `work_dir`
/// (`<work_dir>/u-boot`). Exposed for the same reason
/// [`kernel::tree_dir`](crate::build::kernel::tree_dir) is. A reader of the tree, such
/// as [`crate::shell`], which starts an interactive session in it, does not restate
/// the layout literal.
pub fn tree_dir(work_dir: &Path) -> PathBuf {
    work_dir.join("u-boot")
}

/// Build-dependencies this stage layers over the cross root's base. These are what a
/// u-boot build wants that the toolchain, `make`, `bc`, `bison`, `flex` and
/// `libssl-dev` already in that base do not supply.
///
/// Four of them serve one step. u-boot generates Python bindings for libfdt and then
/// runs `binman` to assemble the image. `swig` and the Python dev headers compile the
/// `pylibfdt` extension, `setuptools` builds it, and `pyelftools` is what `binman`
/// imports. Absent, the failure is a mid-build Python traceback rather than a missing
/// dependency. That is exactly why they are declared here rather than left to be
/// present by accident.
///
/// `libgnutls28-dev` is the fifth, and serves a different one. `tools/mkeficapsule`
/// includes `<gnutls/gnutls.h>` and is built whenever the board's defconfig sets
/// `CONFIG_TOOLS_MKEFICAPSULE`, which every EFI-loader configuration does. It is a host
/// tool rather than something the payloads link. `make tools` is on the path to them,
/// so its absence stops the build outright.
///
/// Read by the [`BuildRootSpec`] that stages the layer, and by [`crate::shell`], which
/// stages the same layer for an interactive session in this stage's root.
pub const UBOOT_BUILD_DEPS: &[&str] = &[
    "swig",
    "python3-dev",
    "python3-setuptools",
    "python3-pyelftools",
    "libgnutls28-dev",
];

/// Stage-recipe version for the u-boot **output** signature (Tier-2 artifact cache):
/// this stage's own logic, folded in as an input.
///
/// An entry stored under a different version is never restored. Bump it when the
/// compile or package logic changes the produced payloads or `.deb` in a way the
/// folded inputs do not already capture. That covers a defconfig generated with
/// different `make` variables, a change to which artifacts the stage emits, and a
/// different archive compressor.
const OUTPUT_STAGE_VERSION: u32 = 8;

/// Where the u-boot stage reads the lock's rkbin blobs from. Either way each file is
/// verified against its sha256 pin and the verified bytes are what `make` reads.
#[derive(Debug, Clone, Copy)]
pub enum BlobSource<'a> {
    /// Fetch the pinned files from the rkbin repository at the locked commit into this
    /// durable, commit-addressed cache ([`rkbin::ensure_files`](crate::rkbin::ensure_files)).
    Fetch(&'a Path),
    /// Read them from this local directory (`--blobs-dir`), for an offline build or a
    /// blob under test. The files are named as the lock names them.
    Dir(&'a Path),
}

/// Filesystem inputs for the u-boot stage.
pub struct UbootOptions<'a> {
    /// Git URL or local path to clone u-boot from, at the locked ref. Defaults to
    /// the boot method's `uboot_source`. A local clone speeds the shallow clone.
    pub source: &'a str,
    /// The patch series to apply, or `None` when the resolved kernel names no patch
    /// series. u-boot is then compiled exactly as cloned.
    pub patches: Option<PatchSource<'a>>,
    /// Where the rkbin blobs the lock pins come from. Read only after the Tier-2
    /// probe misses, so a build that restores u-boot from the artifact cache fetches
    /// nothing.
    pub blobs: BlobSource<'a>,
    /// Git URL or local path to clone Trusted Firmware-A from, for a build point whose
    /// BL31 provider is `tfa`. Defaults to the SoC's `[tfa].git`. Unread under the
    /// rkbin provider.
    pub tfa_source: &'a str,
    /// Scratch directory holding the u-boot clone (`<work>/u-boot`).
    pub work_dir: &'a Path,
    /// The host-arch cross root every `make` in this stage runs in
    /// ([`SandboxRole::Cross`](crate::sandbox::SandboxRole::Cross)).
    ///
    /// u-boot's build compiles host tools (`mkimage`, `dtc`) as well as target ones,
    /// generates its `pylibfdt` device-tree bindings, and runs `binman`. The
    /// toolchain, `swig`, the Python dev headers and `pyelftools` therefore all come
    /// from this root, and the host has none of them. Bootstrapped lazily, like
    /// [`packaging`](Self::packaging) and for the same reason.
    pub cross: &'a dyn BuildSandbox,
    /// Directory the produced boot payloads are staged into.
    pub out_dir: &'a Path,
    /// The root the `u-boot-<device>` `.deb` is archived in.
    ///
    /// Provisioned lazily by this stage rather than by the caller. A build that
    /// restores its u-boot artifacts from the cache never archives anything, and does
    /// not pay for a bootstrap. Its identity is folded into [`output_manifest`] for the
    /// same reason the toolchain's is: it decides the output bytes.
    pub packaging: &'a PackagingSandbox,
    /// The build point's
    /// [artifact stem](boot2deb_core::buildpoint::BuildPoint::artifact_stem) — every
    /// payload is published as `<stem>-<name>`. Two recipes on one board can pin
    /// different u-boot series. Payloads named for the board alone would therefore let
    /// the second build's bootloader be folded into the first's image, silently.
    pub stem: &'a str,
    /// Root of the Tier-2 artifact store ([`crate::artstore`]), or `None` to
    /// disable output caching. On a hit the payloads + deb are restored. On a miss
    /// they are stored after the build.
    pub store: Option<&'a Path>,
}

/// The raw-gap payloads as the u-boot build names them in its tree. Published under
/// the build point's stem (`<stem>-idbloader.img`) by [`publish`], and stored in the
/// artifact cache under these canonical names so one entry serves every point.
const IDBLOADER: &str = "idbloader.img";
const UBOOT_ITB: &str = "u-boot.itb";

/// binman's maskrom USB download payload filenames, emitted only when the u-boot
/// build enables `CONFIG_ROCKCHIP_MASKROM_IMAGE` (the `rk3576-loader` series does).
const MASKROM_USB471: &str = "u-boot-rockchip-usb471.bin";
const MASKROM_USB472: &str = "u-boot-rockchip-usb472.bin";
/// The merged RKBOOT loader packed from the two payloads by [`crate::build::rkboot`]:
/// the single file `rkdeveloptool db` (and other rockusb hosts) stream to the
/// BootROM to RAM-boot this u-boot. Staged alongside the raw payloads (pyrographer
/// sends the raw pair; rkdeveloptool takes the merged file).
const MASKROM_LOADER: &str = "u-boot-rockchip-maskrom.bin";

/// The Tier-2 restore's private staging dir, removed when it goes out of scope —
/// including on the `?` of a publish that fails partway through it.
struct RestoreDir(PathBuf);

impl Drop for RestoreDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// The maskrom USB boot images: the CODE471/CODE472 payloads the BootROM download
/// protocol takes. They run this u-boot from RAM over USB, with nothing written to
/// storage. Present only when the build enables `CONFIG_ROCKCHIP_MASKROM_IMAGE`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MaskromImages {
    /// `u-boot-rockchip-usb471.bin` — CODE471, the external DDR TPL (the rkbin blob).
    pub usb471: PathBuf,
    /// `u-boot-rockchip-usb472.bin` — CODE472, SPL + the FIT, laid out so the FIT
    /// sits at `SPL_LOAD_FIT_ADDRESS`. The SPL advertises `SPL_TEXT_BASE` as its load
    /// address (patch `0005`), which the RK3576 BootROM honors to place the download.
    pub usb472: PathBuf,
    /// `u-boot-rockchip-maskrom.bin` — the two payloads packed into the RKBOOT
    /// container `rkdeveloptool db` consumes (see [`crate::build::rkboot`]). The
    /// directly-flashable single-file loader. The raw pair above is what pyrographer
    /// streams.
    pub loader: PathBuf,
}

/// The raw-gap boot payloads produced by [`build_uboot`], plus the packaged deb.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UbootArtifacts {
    /// `idbloader.img` (SPL + TPL), written at the boot method's idbloader offset.
    pub idbloader: PathBuf,
    /// `u-boot.itb` (FIT: u-boot proper + BL31 + DT), written at its offset.
    pub uboot_itb: PathBuf,
    /// The `u-boot-<device>` `.deb` staging the payloads under `/usr/lib/u-boot`.
    /// The image build still writes the raw payloads to the gap directly. This deb is
    /// the package-centric artifact and on-board reference, rather than the
    /// bootloader-write path (it never auto-flashes).
    pub deb: PathBuf,
    /// The maskrom USB boot images, when the build produced them (see
    /// [`MaskromImages`]). `None` for boards whose u-boot does not enable
    /// `CONFIG_ROCKCHIP_MASKROM_IMAGE`. Cached alongside the payloads, so a Tier-2
    /// restore reproduces them.
    pub maskrom: Option<MaskromImages>,
}

/// Run the u-boot stage, emitting its [`Event`](crate::event::Event)s to `sink`.
///
/// Reads the [`Lock`] for the u-boot ref/commit and the blob hashes. A fresh
/// clone is verified against the locked commit before any patches. A reused tree
/// is `distclean`ed and reconfigured.
pub fn build_uboot(
    build: &ResolvedBuild,
    lock: &Lock,
    opts: &UbootOptions,
    env: &BuildEnv,
    sink: &dyn EventSink,
) -> Result<UbootArtifacts, EngineError> {
    // Narrow the build once: everything below reads the u-boot defconfig, the rkbin
    // blob set, and the raw-gap offsets, which only this boot method has.
    let boot = build.rkbin_boot().ok_or(EngineError::StageNotApplicable {
        stage: "uboot",
        why: "this board's boot method builds no bootloader — its firmware is its own",
    })?;
    let uboot = build::uboot_pin(lock)?;
    let blob_pins = build::blob_pins(lock)?;
    let step = Step::start(sink, "uboot");
    let tree = tree_dir(opts.work_dir);

    // The applied patch series' identity for the Tier-1/Tier-2 signatures:
    // pinned by `patches.commit`, or the live-series fingerprint in co-dev mode so an
    // edited u-boot patch restamps the tree. `series_fp` outlives `patches`.
    let series_fp = build::dev_series_fingerprint(opts.patches, PatchScope::Uboot);
    let patches = build::series_identity(opts.patches, &series_fp);

    // Tier-2 output cache: restore the payloads + deb and skip the whole
    // clone/blob-verify/configure/compile when the output signature is stored. The
    // signature folds the blob hashes, so a hit implies the same verified blobs.
    //
    // Restored into a private staging dir rather than straight into `out_dir`: the
    // store holds the payloads under the canonical names the build tree produces (so
    // one entry serves every build point whose inputs match), while `out_dir` names
    // them for this point. Staging also keeps [`maskrom_in`]'s discovery honest — it
    // sees only what this signature restored, never a leftover from another build.
    let out_man = output_manifest(build, boot, lock, env, patches)?;
    // Emptied before the probe and dropped after it, hit or miss, so a partial restore
    // leaves nothing a later run could read as this signature's output.
    let restored = RestoreDir(opts.work_dir.join("uboot-restore"));
    let _ = std::fs::remove_dir_all(&restored.0);
    if let Some([idbloader, uboot_itb, deb]) = build::restore_stage_outputs(
        opts.store,
        NODE,
        &out_man.signature(),
        &restored.0,
        &["idbloader", "uboot_itb", "deb"],
        &step,
    )?
    .as_deref()
    {
        let idbloader = publish(opts, idbloader)?;
        let uboot_itb = publish(opts, uboot_itb)?;
        // The deb contains a package name and version of its own, so it publishes
        // unrenamed — the artifact ledger, not the file name, scopes a `.deb`.
        let deb = stage_artifact(opts.out_dir, deb)?;
        let maskrom = match maskrom_in(&restored.0) {
            Some(m) => Some(MaskromImages {
                usb471: publish(opts, &m.usb471)?,
                usb472: publish(opts, &m.usb472)?,
                loader: publish(opts, &m.loader)?,
            }),
            None => None,
        };
        step.progress(100);
        step.finish();
        return Ok(UbootArtifacts {
            idbloader,
            uboot_itb,
            deb,
            maskrom,
        });
    }
    // The probe missed, or restored an entry missing a role: discard the staging dir
    // rather than keep it through the compile below.
    drop(restored);

    // Tier-1 reuse of the cloned+patched tree: a lock bump rebuilds it.
    // configure() distcleans + reconfigures a *reused* tree (keyed on the returned
    // flag) and compile() re-runs regardless.
    let man = clone_manifest(lock, patches)?;
    let reused = build::reuse_or_refresh_tree(&tree, &man, "u-boot", &step, || {
        clone_and_patch(lock, opts, &tree, &step)
    })?;
    step.progress(20);

    // Verify blobs against the lock and stage the verified bytes into a private
    // dir the build consumes, so `make` reads exactly what was hashed. The BL31 is
    // the rkbin blob, verified the same way, or the TF-A stage's `bl31.elf`.
    let blobs_dir = match opts.blobs {
        BlobSource::Fetch(cache) => {
            crate::rkbin::ensure_files(blob_pins, &boot.rkbin.upstream, cache, &step)?
        }
        BlobSource::Dir(dir) => dir.to_path_buf(),
    };
    let blob_stage = opts.work_dir.join("blobs");
    let (atf, deb_bl31) = match &boot.bl31 {
        Bl31::Rkbin { .. } => {
            let pin = blob_pins.atf.as_deref().ok_or(EngineError::MissingPin {
                what: "blobs.atf",
                stage: NODE,
            })?;
            let path = absolute(blobs::verify_to(&blobs_dir, pin, &blob_stage)?)?;
            (
                path,
                DebBl31::Rkbin {
                    file: pin_file(pin),
                },
            )
        }
        Bl31::Tfa(_) => {
            let pin = crate::build::tfa::tfa_pin(lock)?;
            // Built into this stage's work dir, which its compile root binds, so `make`
            // reads `bl31.elf` at the path returned here.
            let tfa = crate::build::tfa::build_tfa(
                build,
                lock,
                &crate::build::tfa::TfaOptions {
                    source: opts.tfa_source,
                    work_dir: opts.work_dir,
                    cross: opts.cross,
                    store: opts.store,
                },
                env,
                sink,
            )?;
            let license = std::fs::read_to_string(&tfa.license)
                .map_err(|s| EngineError::io(&tfa.license, s))?;
            (absolute(tfa.bl31.clone())?, DebBl31::Tfa { pin, license })
        }
    };
    let tpl = absolute(blobs::verify_to(&blobs_dir, &blob_pins.tpl, &blob_stage)?)?;
    // BL32/OP-TEE only where the boot chain has one (RK3576); BL31-only SoCs
    // (RK3588/RK1) pin no bl32, so nothing is verified or passed.
    let bl32 = match &blob_pins.bl32 {
        Some(pin) => Some(absolute(blobs::verify_to(&blobs_dir, pin, &blob_stage)?)?),
        None => None,
    };
    step.log(match (&deb_bl31, bl32.is_some()) {
        (DebBl31::Rkbin { .. }, true) => "verified rkbin BL31 + TPL + BL32 against the lock",
        (DebBl31::Rkbin { .. }, false) => "verified rkbin BL31 + TPL against the lock",
        (DebBl31::Tfa { .. }, true) => "verified rkbin TPL + BL32 against the lock; BL31 is TF-A",
        (DebBl31::Tfa { .. }, false) => "verified rkbin TPL against the lock; BL31 is TF-A",
    });
    step.progress(30);

    let blobs = BlobPaths { atf, tpl, bl32 };
    // Everything from here compiles, so the cross root is stood up here rather than at
    // the top of the stage: a Tier-2 hit returns without ever provisioning one.
    let (root, binds) = compile_root(opts, &step)?;
    let cr = CompileRoot {
        root: &root,
        binds: &binds,
    };
    configure(boot, env, &cr, &tree, &blobs, reused, &step)?;
    // Under the efi loader u-boot hands the kernel the DTB it finds on the EFI system
    // partition under its own `fdtfile`, so the name the image places it under has to
    // be that one. Said rather than refused: a board's code can set `fdtfile` at boot
    // (the Radxa ZERO 3 picks its tree by a resistor it reads), which no build can see.
    if let Some(efi) = build
        .image
        .as_ref()
        .and_then(|i| i.loader.as_ref())
        .and_then(|l| l.efi())
    {
        if let Some(line) = fdtfile_mismatch(&tree, &efi.dtb) {
            step.log(line);
        }
    }
    step.progress(40);

    // Deterministic build timestamp from the locked commit, so `u-boot.itb` does
    // not embed wall-clock time.
    let epoch = crate::git::commit_epoch(&tree, &uboot.commit).ok();
    compile(env, &cr, &tree, &blobs, epoch, &step)?;

    let (idbloader, uboot_itb) = collect(opts, &tree, &step)?;
    let maskrom = collect_maskrom(opts, &tree, &step)?;
    step.progress(90);

    let deb = package_deb(
        build,
        boot,
        lock,
        opts,
        epoch,
        Payloads {
            idbloader: &idbloader,
            uboot_itb: &uboot_itb,
        },
        &deb_bl31,
        &step,
    )?;

    // Store the payloads + deb under the output signature, plus the maskrom images
    // when this build produced them, so a Tier-2 restore reproduces the full set.
    //
    // Stored from the *tree*, not from the published copies: the signature keys on the
    // inputs that shape the bytes, and the build point is not one of them, so an entry
    // must contain the canonical names to be servable to any point whose inputs match.
    let mut outputs = vec![
        ("idbloader", tree.join(IDBLOADER)),
        ("uboot_itb", tree.join(UBOOT_ITB)),
        ("deb", deb.clone()),
    ];
    if maskrom.is_some() {
        outputs.push(("usb471", tree.join(MASKROM_USB471)));
        outputs.push(("usb472", tree.join(MASKROM_USB472)));
        outputs.push(("maskrom_loader", tree.join(MASKROM_LOADER)));
    }
    let outputs: Vec<(&str, &Path)> = outputs.iter().map(|(r, p)| (*r, p.as_path())).collect();
    build::store_stage_outputs(opts.store, NODE, &out_man.signature(), &outputs, &step)?;
    step.progress(100);
    step.finish();
    Ok(UbootArtifacts {
        idbloader,
        uboot_itb,
        deb,
        maskrom,
    })
}

/// The Tier-2 output signature manifest of the u-boot payloads + deb.
///
/// It folds the Tier-1 tree signature ([`clone_manifest`]) as a dependency, then every
/// other input that shapes the output:
///
/// - The board defconfig.
/// - The sha256-pinned rkbin blob hashes. A blob change means a new signature and so a
///   rebuild, which is why a hit implies the same verified blobs.
/// - The BL31: the provider, then the rkbin blob's hash or the
///   [`tfa`](crate::build::tfa::output_manifest) stage's input signature. Inputs
///   rather than `bl31.elf`'s bytes, so a hit is decided without building TF-A.
/// - The deb's packaged fields: device, description, SoC, arch, the raw offsets, and
///   the deb version ([`Lock::uboot_deb_version`]). Its revision is a lock value of
///   its own, so two locks with every other input equal still archive two debs when
///   their revisions differ.
/// - Whether the build is cross, and the host toolchain identity.
/// - The identity of the packaging root that archives the deb.
///
/// On a signature hit the artifact store restores the payloads + deb rather than
/// rebuilding, so the key must cover everything that can change them.
/// Public so `why-rebuild` ([`crate::plan`]) asks the artifact store the same
/// question this stage does, rather than reimplementing the key it is asking under.
pub fn output_manifest(
    build: &ResolvedBuild,
    boot: &ResolvedRkbinBoot,
    lock: &Lock,
    env: &BuildEnv,
    patches: SeriesIdentity,
) -> Result<crate::signature::SignatureManifest, EngineError> {
    // Fold the Tier-1 tree signature (containing the co-dev series fingerprint, if any),
    // so a co-dev build never shares an output entry with a pinned one and an edited
    // patch invalidates the cached deb.
    let tree_sig = clone_manifest(lock, patches)?.signature();
    let pins = build::blob_pins(lock)?;
    let mut b = crate::signature::SignatureBuilder::new("uboot:out", OUTPUT_STAGE_VERSION);
    b.fold_dep(&tree_sig)
        .fold_scalar("uboot_defconfig", &boot.uboot_defconfig)
        .fold_scalar("bl31", boot.bl31.provider().as_str());
    match &boot.bl31 {
        Bl31::Rkbin { .. } => {
            let atf = pins.atf.as_deref().ok_or(EngineError::MissingPin {
                what: "blobs.atf",
                stage: NODE,
            })?;
            b.fold_scalar("blob.atf", atf);
        }
        Bl31::Tfa(src) => {
            b.fold_dep(&crate::build::tfa::output_manifest(src, lock, env)?.signature());
        }
    }
    b.fold_scalar("blob.tpl", &pins.tpl)
        .fold_scalar("blob.bl32", pins.bl32.as_deref().unwrap_or(""))
        .fold_scalar("device", &build.device)
        .fold_scalar("description", &build.description)
        .fold_scalar("soc", build.soc.as_str())
        .fold_scalar("boot_method", build.boot_method.as_str())
        .fold_scalar("arch", build.arch.as_str())
        .fold_scalar("offset.idbloader", &boot.offsets.idbloader)
        .fold_scalar("offset.uboot_itb", &boot.offsets.uboot_itb)
        .fold_scalar("deb_version", &deb_version(lock)?)
        .fold_scalar("cross", env.cross_compile.as_deref().unwrap_or(""))
        .fold_scalar("toolchain", &env.toolchain_id)
        // The base root's identity above covers what the toolchain is; this covers what
        // was layered over it. Both reach the compile, so both reach the key — u-boot's
        // makefiles probe for what is present (`pkg-config --cflags gnutls` decides how
        // `mkeficapsule` is built), so a package added to or removed from this list is a
        // different build.
        .fold_set("build_deps", UBOOT_BUILD_DEPS)
        // The root that archives the deb decides its bytes as surely as the compiler
        // decides the payloads' — a different `dpkg-deb`, or the same one resolved from
        // a different point in the archive, is a different output.
        .fold_scalar("packaging", &env.packaging_id);
    Ok(b.manifest())
}

/// The Tier-1 signature manifest of the cloned and patched u-boot tree. It folds the
/// pinned inputs that determine its content: the u-boot commit and the patch series
/// (`build::fold_patch_series`). The source URL is excluded, since the commit
/// content-addresses the tree.
///
/// The [`SeriesIdentity`] fold covers the pinned patch commit and, in co-dev mode, the
/// live-series fingerprint. A co-dev build therefore never shares a stamp with a
/// pinned one, and an edited patch restamps.
///
/// Blobs and the defconfig are not folded here. They gate compile, which re-runs on
/// every invocation, rather than the tree reuse. Public so `why-rebuild`
/// ([`crate::plan`]) recomputes the same signature it stamps here.
pub fn clone_manifest(
    lock: &Lock,
    patches: SeriesIdentity,
) -> Result<crate::signature::SignatureManifest, EngineError> {
    let pin = build::uboot_pin(lock)?;
    let mut b = crate::signature::SignatureBuilder::new("uboot", CLONE_STAGE_VERSION);
    b.fold_scalar("uboot.commit", &pin.commit);
    // The u-boot tree is patched by the *u-boot* series (`[uboot_patches]`), an
    // independent axis from the kernel's `[patches]`. Folding the wrong pin here would
    // make every u-boot series on one u-boot commit share a signature — the display,
    // util, and loader builds would collide in the artifact cache.
    build::fold_patch_series(&mut b, lock.uboot_patches.as_ref(), patches);
    Ok(b.manifest())
}

/// Shallow-clone the pinned u-boot, verify the commit, enforce the patches pin,
/// and apply any locked u-boot patches. A failure removes the partial tree so
/// a resume never reuses a half-patched u-boot (via [`build::clone_pinned`]).
///
/// The declared-intent gate runs against the u-boot ref, not the kernel's: u-boot
/// is its own axis, so a series that claims `applies_to_uboot = ">=2026.01"` is
/// making a claim about `pin.reference` and nothing else.
fn clone_and_patch(
    lock: &Lock,
    opts: &UbootOptions,
    tree: &Path,
    step: &Step,
) -> Result<(), EngineError> {
    let pin = build::uboot_pin(lock)?;
    let target = format!("u-boot @ {}", pin.reference);
    let spec = ClonePinned {
        source: opts.source,
        reference: &pin.reference,
        commit: &pin.commit,
        mode: CloneMode::Shallow,
        tree,
        what: "u-boot",
        patches: opts.patches,
        scope: PatchScope::Uboot,
        target: &target,
        gate_reference: Some(&pin.reference),
    };
    let n = build::clone_pinned(&spec, step)?;
    if let (Some(p), 1..) = (opts.patches, n) {
        step.log(format!(
            "applied {n} u-boot patches ({})",
            p.pin.series.join(", ")
        ));
    }
    Ok(())
}

/// Configure the board defconfig (`make <defconfig>`), `distclean`ing first only
/// when reusing a tree. RK3588 u-boot takes no `ARCH=` (the defconfig sets it).
fn configure(
    boot: &ResolvedRkbinBoot,
    env: &BuildEnv,
    cr: &CompileRoot,
    tree: &Path,
    blobs: &BlobPaths,
    reused: bool,
    step: &Step,
) -> Result<(), EngineError> {
    if reused {
        let argv = vec![
            "make".to_string(),
            "-C".to_string(),
            tree.display().to_string(),
            "distclean".to_string(),
        ];
        build::run_in_root(cr, tree, &argv, &cross(env), "make distclean", step)?;
    }
    // The defconfig comes from config; validate it and pass it after `--` so make
    // cannot read it as an option or a `FOO=bar` variable assignment.
    build::reject_unsafe_make_target("uboot_defconfig", &boot.uboot_defconfig)?;
    let mut argv = vec![
        "make".to_string(),
        "-C".to_string(),
        tree.display().to_string(),
    ];
    blob_vars(&mut argv, blobs);
    argv.push("--".to_string());
    argv.push(boot.uboot_defconfig.clone());
    build::run_in_root(
        cr,
        tree,
        &argv,
        &cross(env),
        &format!("make {}", boot.uboot_defconfig),
        step,
    )
}

/// Build u-boot with the verified blobs passed as make variables.
/// `source_date_epoch` is the locked commit's committer date.
///
/// `bl32` is the OP-TEE payload, passed only when the boot chain needs one. It is
/// passed as `TEE=` — the variable mainline u-boot's binman FIT assembly reads for
/// the OP-TEE image; the vendor tree's `BL32=` name is not used here.
fn compile(
    env: &BuildEnv,
    cr: &CompileRoot,
    tree: &Path,
    blobs: &BlobPaths,
    source_date_epoch: Option<u64>,
    step: &Step,
) -> Result<(), EngineError> {
    let mut argv = vec![
        "make".to_string(),
        "-C".to_string(),
        tree.display().to_string(),
        format!("-j{}", env.jobs()),
    ];
    blob_vars(&mut argv, blobs);
    let mut vars = cross(env);
    if let Some(epoch) = source_date_epoch {
        vars.push(("SOURCE_DATE_EPOCH".to_string(), epoch.to_string()));
    }
    build::run_in_root(cr, tree, &argv, &vars, "make u-boot", step)
}

/// The verified payloads a u-boot build consumes, as absolute paths.
pub struct BlobPaths {
    /// BL31 image (`BL31=`): the rkbin blob or TF-A's `bl31.elf`.
    pub atf: PathBuf,
    /// DDR init TPL (`ROCKCHIP_TPL=`).
    pub tpl: PathBuf,
    /// OP-TEE secure payload (`TEE=`), on SoCs whose boot chain has one.
    pub bl32: Option<PathBuf>,
}

/// Add the blob payload paths as `make` variables.
///
/// Passed to **both** [`configure`] and [`compile`], because u-boot's Kconfig reads
/// them out of the build environment — `HAS_TEE_IN_BUILD_ENV` is
/// `def_bool $(success, test -n "$(TEE)")` and `select`s `OPTEE_LIB`, which in turn
/// exposes `OPTEE_TZDRAM_SIZE`. Generating the `.config` without `TEE` and then
/// compiling with it leaves those symbols unset, so the compile's `syncconfig` finds a
/// stale `.config` and stops for interactive input — a build that hangs on stdin.
/// The two invocations must therefore see one environment. (`TEE` is also what makes
/// u-boot copy OP-TEE's reserved-memory nodes into the FDT it hands the kernel, so
/// having it at config time is the correct behavior, not a workaround.)
fn blob_vars(argv: &mut Vec<String>, blobs: &BlobPaths) {
    argv.push(format!("BL31={}", blobs.atf.display()));
    argv.push(format!("ROCKCHIP_TPL={}", blobs.tpl.display()));
    if let Some(bl32) = &blobs.bl32 {
        argv.push(format!("TEE={}", bl32.display()));
    }
}

/// Publish one payload into `out_dir` as `<stem>-<its own name>`.
///
/// The one place a raw payload's published name is formed, so the build path and the
/// cache-restore path cannot disagree about what the image node will look for.
fn publish(opts: &UbootOptions, src: &Path) -> Result<PathBuf, EngineError> {
    let name = src
        .file_name()
        .expect("a payload path has a file name")
        .to_string_lossy();
    build::stage_artifact_as(opts.out_dir, src, &format!("{}-{name}", opts.stem))
}

/// Stage the produced boot payloads out of the tree, returning the published
/// `(<stem>-idbloader.img, <stem>-u-boot.itb)`.
fn collect(
    opts: &UbootOptions,
    tree: &Path,
    step: &Step,
) -> Result<(PathBuf, PathBuf), EngineError> {
    let idb_src = tree.join(IDBLOADER);
    let itb_src = tree.join(UBOOT_ITB);
    for (what, path) in [(IDBLOADER, &idb_src), (UBOOT_ITB, &itb_src)] {
        if !path.exists() {
            return Err(EngineError::ArtifactMissing {
                what: what.into(),
                location: tree.display().to_string(),
            });
        }
    }
    let idbloader = publish(opts, &idb_src)?;
    let uboot_itb = publish(opts, &itb_src)?;
    step.log(format!(
        "staged {} and {}",
        idbloader.display(),
        uboot_itb.display()
    ));
    Ok((idbloader, uboot_itb))
}

/// Stage the maskrom USB boot images from a freshly built tree, when binman emitted
/// them (`CONFIG_ROCKCHIP_MASKROM_IMAGE`). Both files must be present or the build
/// produced neither — a lone one is a binman contract break, reported as missing.
fn collect_maskrom(
    opts: &UbootOptions,
    tree: &Path,
    step: &Step,
) -> Result<Option<MaskromImages>, EngineError> {
    let src471 = tree.join(MASKROM_USB471);
    let src472 = tree.join(MASKROM_USB472);
    match (src471.exists(), src472.exists()) {
        (false, false) => Ok(None),
        (true, true) => {
            // The merged loader is packed into the tree beside the two payloads it is
            // made of, so all three reach the artifact cache under canonical names and
            // publish through the same [`publish`] rename.
            let src_loader = pack_maskrom_loader(tree, &src471, &src472)?;
            let usb471 = publish(opts, &src471)?;
            let usb472 = publish(opts, &src472)?;
            let loader = publish(opts, &src_loader)?;
            step.log("staged maskrom USB boot images (usb471 + usb472 + merged loader)");
            Ok(Some(MaskromImages {
                usb471,
                usb472,
                loader,
            }))
        }
        (has471, _) => Err(EngineError::ArtifactMissing {
            what: if has471 {
                MASKROM_USB472
            } else {
                MASKROM_USB471
            }
            .into(),
            location: tree.display().to_string(),
        }),
    }
}

/// Pack the two payloads into the merged RKBOOT loader
/// ([`crate::build::rkboot`]) and write it into `tree`, beside them. The SoC's
/// four-digit code (for the container `chipType`) is read from the built u-boot
/// `.config` (`CONFIG_ROCKCHIP_RK<code>=y`), so the packer stays chip-agnostic.
fn pack_maskrom_loader(tree: &Path, usb471: &Path, usb472: &Path) -> Result<PathBuf, EngineError> {
    let chip = rk_chip_code(tree).ok_or_else(|| EngineError::ArtifactMissing {
        what: "CONFIG_ROCKCHIP_RK<code> in .config".into(),
        location: tree.display().to_string(),
    })?;
    let b471 = std::fs::read(usb471).map_err(|source| EngineError::io(usb471, source))?;
    let b472 = std::fs::read(usb472).map_err(|source| EngineError::io(usb472, source))?;
    let bytes = crate::build::rkboot::write_maskrom_loader(chip, &b471, &b472);
    let dest = tree.join(MASKROM_LOADER);
    let tmp = tree.join(format!(".{MASKROM_LOADER}.{}.partial", std::process::id()));
    std::fs::write(&tmp, &bytes).map_err(|source| {
        let _ = std::fs::remove_file(&tmp);
        EngineError::io(&tmp, source)
    })?;
    std::fs::rename(&tmp, &dest).map_err(|source| {
        let _ = std::fs::remove_file(&tmp);
        EngineError::io(&dest, source)
    })?;
    Ok(dest)
}

/// The warning to log when the configured u-boot's default `fdtfile` is not `esp_dtb`,
/// the name the image places the kernel's DTB under on the EFI system partition.
/// `None` when they agree, or when the `.config` sets no default.
fn fdtfile_mismatch(tree: &Path, esp_dtb: &str) -> Option<String> {
    let config = std::fs::read_to_string(tree.join(".config")).ok()?;
    let fdtfile = config.lines().find_map(|line| {
        line.strip_prefix("CONFIG_DEFAULT_FDT_FILE=\"")?
            .strip_suffix('"')
    })?;
    (fdtfile != esp_dtb).then(|| {
        format!(
            "warning: u-boot's default fdtfile is {fdtfile}, and the image places the \
             kernel's DTB at dtb/{esp_dtb} on the EFI system partition. Unless the board's \
             u-boot sets fdtfile to that name at boot, it boots its own device tree: set \
             the device's uboot_fdtfile to the name u-boot loads by"
        )
    })
}

/// The SoC's four-character code from the built u-boot `.config`, e.g. `b"3576"`
/// from `CONFIG_ROCKCHIP_RK3576=y`. `None` if no such symbol is set.
fn rk_chip_code(tree: &Path) -> Option<[u8; 4]> {
    let config = std::fs::read_to_string(tree.join(".config")).ok()?;
    config.lines().find_map(|line| {
        let code = line
            .strip_prefix("CONFIG_ROCKCHIP_RK")?
            .strip_suffix("=y")?;
        let bytes = code.as_bytes();
        (bytes.len() == 4 && bytes.iter().all(u8::is_ascii_digit))
            .then(|| bytes.try_into().unwrap())
    })
}

/// The maskrom images sitting in `dir` under their canonical names, or `None` when
/// this board's build produced none.
///
/// Read from the Tier-2 restore's private staging dir, which holds exactly the
/// artifacts one signature stored — so "this entry has no maskrom images" cannot be
/// answered by another build's leftovers.
fn maskrom_in(dir: &Path) -> Option<MaskromImages> {
    let usb471 = dir.join(MASKROM_USB471);
    let usb472 = dir.join(MASKROM_USB472);
    let loader = dir.join(MASKROM_LOADER);
    (usb471.is_file() && usb472.is_file() && loader.is_file()).then_some(MaskromImages {
        usb471,
        usb472,
        loader,
    })
}

/// Stand up the build root this stage compiles in, and the host paths its commands must
/// see: the work dir (the tree and everything the build writes beside it) and the
/// verified blob payloads, which `make` reads by absolute path from a staging directory
/// under that same work dir.
fn compile_root(
    opts: &UbootOptions,
    step: &Step,
) -> Result<(BuildRoot, Vec<PathBuf>), EngineError> {
    opts.cross.ensure_ready(step)?;
    let root = opts.cross.build_root(
        &BuildRootSpec {
            packages: UBOOT_BUILD_DEPS,
            // Nothing this build produced: u-boot build-depends only on the archive.
            pool: None,
            stage: "uboot",
        },
        step,
    )?;
    Ok((root, vec![opts.work_dir.to_path_buf()]))
}

/// `CROSS_COMPILE` as a one-entry environment, or empty where the cross root is already
/// the target's architecture and the compile is native.
fn cross(env: &BuildEnv) -> Vec<(String, String)> {
    env.cross_compile
        .iter()
        .map(|prefix| ("CROSS_COMPILE".to_string(), prefix.clone()))
        .collect()
}

/// Canonicalize a blob path so `make -C <tree>` (which changes directory) still
/// resolves it. The file exists (verify just read it), so this cannot 404.
fn absolute(path: PathBuf) -> Result<PathBuf, EngineError> {
    std::fs::canonicalize(&path).map_err(|source| EngineError::io(&path, source))
}

/// Debian package name for a device's u-boot payloads, e.g. `u-boot-turing-rk1`.
fn package_name(device: &str) -> String {
    format!("u-boot-{device}")
}

/// The two blobs a u-boot build produces, as one value.
///
/// They travel together everywhere: both are written into the deb's payload directory,
/// both are named by `install.conf`, and both are placed at their own raw-gap offset.
/// A struct because they are two `&Path`s of the same shape — a swapped pair would
/// write each blob at the other's offset and produce an image that does not boot.
struct Payloads<'a> {
    /// The SPL + TPL loader image, written at the boot method's `idbloader` offset.
    idbloader: &'a Path,
    /// The u-boot FIT, written at the `uboot_itb` offset.
    uboot_itb: &'a Path,
}

/// What names the produced `.deb`: its package name, its version, its architecture.
///
/// A struct for the same reason [`Payloads`] is one: three `&str`s in a row that a
/// swap would silently reorder into a package nobody can install.
struct DebIdentity<'a> {
    /// `u-boot-<device>`.
    pkg: &'a str,
    /// The Debian version, the lock's [`uboot_deb_version`](Lock::uboot_deb_version).
    version: &'a str,
    /// The Debian architecture the payloads are for.
    arch: &'a str,
}

/// The BL31 inside `u-boot.itb`, as the u-boot deb describes it: its `README.Debian`
/// line and its `copyright` entry. The deb version names it too, read from the lock
/// ([`Lock::uboot_deb_upstream`]).
enum DebBl31<'a> {
    /// Rockchip's prebuilt BL31.
    Rkbin {
        /// The vendored file's name.
        file: &'a str,
    },
    /// Trusted Firmware-A built from source.
    Tfa {
        /// The lock's `[tfa]` pin.
        pin: &'a TfaPin,
        /// TF-A's license text, from the built tree's `docs/license.rst`.
        license: String,
    },
}

/// The file name of a `"<file>@sha256:<hex>"` blob pin.
fn pin_file(pin: &str) -> &str {
    pin.rsplit_once("@sha256:").map_or(pin, |(file, _)| file)
}

/// Package the staged raw payloads into the `u-boot-<device>` `.deb`.
///
/// The deb stages `idbloader.img` + `u-boot.itb` under `/usr/lib/u-boot/<device>/`
/// with an `install.conf` recording their raw byte offsets and BL31 provider,
/// documents the manual `dd` in `README.Debian`, and names every component the two
/// payloads contain, with its license, in `copyright`. It contains **no** maintainer
/// script: the bootloader lives in a raw gap outside any filesystem, so flashing is
/// the image build's job (or a documented manual step), never an `apt` side effect
/// that could brick a board by writing to the wrong device.
///
/// The tree is staged on the host, since a data-only archive resolves no
/// dependencies, so nothing here needs a target-arch root. It is archived by
/// [`build::archive_deb`] in the build's host-arch packaging root, which is what
/// makes the `.deb` a function of the lock rather than of the build host's `dpkg`.
// One argument per thing the deb is made of; each is its own type, so a swap does
// not compile.
#[allow(clippy::too_many_arguments)]
fn package_deb(
    build: &ResolvedBuild,
    boot: &ResolvedRkbinBoot,
    lock: &Lock,
    opts: &UbootOptions,
    source_date_epoch: Option<u64>,
    payloads: Payloads,
    bl31: &DebBl31,
    step: &Step,
) -> Result<PathBuf, EngineError> {
    let pkg = package_name(&build.device);
    let version = deb_version(lock)?;
    let arch = build.arch.debian_arch();

    // Assemble under a clean pkg-stage (a stale tree would ship leftover files).
    let pkg_stage = opts.work_dir.join("uboot-deb");
    let _ = std::fs::remove_dir_all(&pkg_stage);
    stage_tree(
        &pkg_stage,
        build,
        boot,
        &DebIdentity {
            pkg: &pkg,
            version: &version,
            arch,
        },
        payloads,
        &copyright_text(&pkg, lock, bl31)?,
        bl31,
    )?;
    // Force uniform data modes (dirs 0755, files 0644) so the host umask does not leak
    // into the packaged tree — the u-boot deb is data-only, so this is byte-safe and
    // makes the .deb reproducible across hosts.
    build::normalize_data_tree(&pkg_stage)?;

    let deb_name = format!("{pkg}_{version}_{arch}.deb");
    let deb_in_stage = opts.work_dir.join(&deb_name);
    opts.packaging.ensure_ready(step)?;
    // One bind covers both ends: the staged tree and the archive are both under the
    // stage's work dir, exposed inside at their host path.
    build::archive_deb(
        opts.packaging,
        &pkg_stage,
        &deb_in_stage,
        &[opts.work_dir.to_path_buf()],
        source_date_epoch,
        "dpkg-deb --build u-boot",
        step,
    )?;

    let deb = stage_artifact(opts.out_dir, &deb_in_stage)?;
    step.log(format!("staged {deb_name}"));
    Ok(deb)
}

/// Lay out the u-boot deb's file tree under `pkg_stage`: the two payloads and
/// their `install.conf` under `/usr/lib/u-boot/<device>/`, the `README.Debian`
/// and `copyright` under `/usr/share/doc/<pkg>/`, and the `DEBIAN/control`. Split
/// from [`package_deb`] so the layout is testable without `dpkg-deb`. The offsets are
/// parsed from the build's authored strings, so a malformed offset is a
/// typed [`ConfigError`](boot2deb_core::ConfigError) here rather than a bad deb.
fn stage_tree(
    pkg_stage: &Path,
    build: &ResolvedBuild,
    boot: &ResolvedRkbinBoot,
    id: &DebIdentity,
    payloads: Payloads,
    copyright: &str,
    bl31: &DebBl31,
) -> Result<(), EngineError> {
    let &DebIdentity { pkg, version, arch } = id;
    let Payloads {
        idbloader,
        uboot_itb,
    } = payloads;
    let idb_off = parse_size(&boot.offsets.idbloader)?;
    let itb_off = parse_size(&boot.offsets.uboot_itb)?;

    let lib_dir = pkg_stage.join(format!("usr/lib/u-boot/{}", build.device));
    std::fs::create_dir_all(&lib_dir).map_err(|s| EngineError::io(&lib_dir, s))?;
    copy_into(idbloader, &lib_dir.join("idbloader.img"))?;
    copy_into(uboot_itb, &lib_dir.join("u-boot.itb"))?;
    let conf = install_conf_text(
        &build.device,
        build.boot_method.as_str(),
        boot.bl31.provider().as_str(),
        idb_off,
        itb_off,
    );
    write_file(&lib_dir.join("install.conf"), &conf)?;

    let doc_dir = pkg_stage.join(format!("usr/share/doc/{pkg}"));
    std::fs::create_dir_all(&doc_dir).map_err(|s| EngineError::io(&doc_dir, s))?;
    let readme = readme_text(
        &build.device,
        &build.description,
        &boot.offsets.idbloader,
        idb_off,
        &boot.offsets.uboot_itb,
        itb_off,
        &bl31_line(bl31),
    );
    write_file(&doc_dir.join("README.Debian"), &readme)?;
    write_file(&doc_dir.join("copyright"), copyright)?;

    let debian = pkg_stage.join("DEBIAN");
    std::fs::create_dir_all(&debian).map_err(|s| EngineError::io(&debian, s))?;
    let control = control_text(pkg, version, arch, &build.description, build.soc.as_str());
    write_file(&debian.join("control"), &control)?;
    Ok(())
}

/// Copy `src` to `dst` (its parent already created), mapping I/O errors.
fn copy_into(src: &Path, dst: &Path) -> Result<(), EngineError> {
    std::fs::copy(src, dst)
        .map(|_| ())
        .map_err(|s| EngineError::io(src, s))
}

/// Write `contents` to `path`, mapping I/O errors.
fn write_file(path: &Path, contents: &str) -> Result<(), EngineError> {
    std::fs::write(path, contents).map_err(|s| EngineError::io(path, s))
}

/// The `u-boot-<device>` package's Debian version, the lock's
/// [`uboot_deb_version`](Lock::uboot_deb_version): `<upstream>-<revision>`, as in
/// `2026.10+rkbin1.56-1`.
///
/// The lock rather than the resolved build is what it is read from, so `update`, which
/// moves the revision, and this stage, which writes it into the package, read one
/// version. A lock that reaches here pins its u-boot and its BL31, both of which the
/// stage has already read. The BL31 pin is the one the error names, since a lock with
/// no u-boot pin fails on that first.
fn deb_version(lock: &Lock) -> Result<String, EngineError> {
    build::uboot_pin(lock)?;
    lock.uboot_deb_version().ok_or(EngineError::MissingPin {
        what: "blobs.atf",
        stage: NODE,
    })
}

/// The `README.Debian` sentence naming the BL31 inside `u-boot.itb`.
fn bl31_line(bl31: &DebBl31) -> String {
    match bl31 {
        DebBl31::Rkbin { file } => {
            format!("The BL31 inside u-boot.itb is Rockchip's prebuilt {file} from rkbin.")
        }
        DebBl31::Tfa { pin, .. } => format!(
            "The BL31 inside u-boot.itb is Trusted Firmware-A {} ({}), built from source.",
            pin.reference, pin.commit
        ),
    }
}

/// Rockchip's terms for the rkbin binaries, as `THIRD-PARTY-NOTICES.md` states them,
/// wrapped for a terminal.
const RKBIN_TERMS: &str =
    "Rockchip's license grants a non-exclusive right to use, copy, and distribute
the binaries and to distribute modifications, on the condition that copyright,
patent, and trademark notices are preserved. The binaries are provided as-is
with no warranty.";

/// The deb's `copyright`: one entry per component the payloads contain, each with its
/// source, its exact version and its license, and TF-A's full license text where the
/// BL31 is TF-A, since its BSD-3-Clause terms require the notice to travel with the
/// binary. Pure, so it is testable.
fn copyright_text(pkg: &str, lock: &Lock, bl31: &DebBl31) -> Result<String, EngineError> {
    let uboot = build::uboot_pin(lock)?;
    let blobs = build::blob_pins(lock)?;
    let mut s = format!(
        "{pkg}\n{}\n\n\
         The payloads in this package contain the components below. Each keeps its\n\
         upstream license.\n\n\
         U-Boot\n\
        \x20 Source:  {}\n\
        \x20 Version: {} ({})\n\
        \x20 License: GPL-2.0+\n\n\
         DDR init TPL, in idbloader.img\n\
        \x20 File:    {}\n\
        \x20 Source:  Rockchip rkbin, https://github.com/rockchip-linux/rkbin\n\
        \x20 License: rkbin (below)\n\n",
        "=".repeat(pkg.len()),
        uboot.source,
        uboot.reference,
        uboot.commit,
        pin_file(&blobs.tpl),
    );
    if let Some(bl32) = &blobs.bl32 {
        s.push_str(&format!(
            "OP-TEE BL32, in u-boot.itb\n\
            \x20 File:    {}\n\
            \x20 Source:  Rockchip rkbin, https://github.com/rockchip-linux/rkbin\n\
            \x20 License: rkbin (below)\n\n",
            pin_file(bl32)
        ));
    }
    match bl31 {
        DebBl31::Rkbin { file } => s.push_str(&format!(
            "BL31, in u-boot.itb\n\
            \x20 File:    {file}\n\
            \x20 Source:  Rockchip rkbin, https://github.com/rockchip-linux/rkbin\n\
            \x20 License: rkbin (below)\n\n"
        )),
        DebBl31::Tfa { pin, .. } => s.push_str(&format!(
            "BL31, in u-boot.itb: Trusted Firmware-A\n\
            \x20 Source:    {}\n\
            \x20 Version:   {} ({})\n\
            \x20 Copyright: Arm Limited and Contributors, Rockchip Electronics Co., Ltd.,\n\
            \x20            and the further holders each source file names\n\
            \x20 License:   BSD-3-Clause (below), with the bundled code it lists\n\n",
            pin.source, pin.reference, pin.commit
        )),
    }
    s.push_str(&format!("License: rkbin\n\n{RKBIN_TERMS}\n"));
    if let DebBl31::Tfa { license, .. } = bl31 {
        s.push_str("\nLicense: BSD-3-Clause (Trusted Firmware-A, docs/license.rst)\n\n");
        s.push_str(license.trim_end());
        s.push('\n');
    }
    Ok(s)
}

/// The `DEBIAN/control` stanza. Pure, so it is testable. No `Depends:` — the
/// package ships only data files — and no maintainer script.
fn control_text(pkg: &str, version: &str, arch: &str, description: &str, soc: &str) -> String {
    format!(
        "Package: {pkg}\n\
         Version: {version}\n\
         Section: admin\n\
         Priority: optional\n\
         Architecture: {arch}\n\
         Maintainer: boot2deb <build@boot2deb>\n\
         Description: U-Boot bootloader payloads for {description}\n\
        \x20Stages the SPL (idbloader.img) and U-Boot FIT (u-boot.itb) for the\n\
        \x20{soc} under /usr/lib/u-boot, with the raw offsets recorded in\n\
        \x20install.conf. It does NOT flash the bootloader: it lives in a raw gap\n\
        \x20outside any filesystem, so writing it is left to the image build or the\n\
        \x20documented manual dd (see /usr/share/doc/{pkg}/README.Debian).\n"
    )
}

/// The `install.conf` recording the payloads' raw byte offsets and the BL31
/// provider, so an on-device updater can read where each payload is written and
/// which firmware it carries. Pure/testable.
fn install_conf_text(
    device: &str,
    boot_method: &str,
    bl31: &str,
    idb_off: u64,
    itb_off: u64,
) -> String {
    format!(
        "# boot2deb u-boot install offsets for {device}\n\
         # raw byte offsets from the start of the boot medium (outside any filesystem)\n\
         device={device}\n\
         boot_method={boot_method}\n\
         bl31={bl31}\n\
         idbloader=/usr/lib/u-boot/{device}/idbloader.img\n\
         idbloader_offset={idb_off}\n\
         uboot_itb=/usr/lib/u-boot/{device}/u-boot.itb\n\
         uboot_itb_offset={itb_off}\n"
    )
}

/// The `README.Debian` documenting the manual flash. Pure/testable. Shows each
/// payload's offset both in bytes (from the parsed value) and in the authored
/// unit string, plus a ready-to-run `dd` per payload.
fn readme_text(
    device: &str,
    description: &str,
    idb_str: &str,
    idb_off: u64,
    itb_str: &str,
    itb_off: u64,
    bl31_line: &str,
) -> String {
    let title = package_name(device);
    let underline = "=".repeat(title.len());
    format!(
        "{title}\n{underline}\n\n\
         This package stages the U-Boot bootloader for {description} under\n\
         /usr/lib/u-boot/{device}/. It deliberately does not write the bootloader\n\
         to any device: the payloads live in a raw gap outside any filesystem, so\n\
         an automatic flash on apt install/upgrade could brick a board by writing\n\
         to the wrong disk. The boot2deb image build writes them for you; to flash\n\
         a device by hand, write each payload to its fixed byte offset from the\n\
         start of the medium:\n\n\
        \x20 idbloader.img  ->  offset {idb_off} bytes ({idb_str})\n\
        \x20 u-boot.itb     ->  offset {itb_off} bytes ({itb_str})\n\n\
        \x20 {}\n\
        \x20 {}\n\
        \x20 sync\n\n\
         Replace /dev/DISK with the target boot medium (e.g. /dev/mmcblk0 or\n\
         /dev/sdX). Writing to the wrong device will destroy its contents --\n\
         double-check the device name first.\n\n\
         {bl31_line} The components of both payloads and their licenses are listed\n\
         in /usr/share/doc/{title}/copyright.\n",
        dd_command(&format!("/usr/lib/u-boot/{device}/idbloader.img"), idb_off),
        dd_command(&format!("/usr/lib/u-boot/{device}/u-boot.itb"), itb_off),
    )
}

/// A `dd` command writing `payload` to `/dev/DISK` at `offset` bytes, choosing
/// the largest of 4096 / 512 / 1 that divides the offset as the block size (RK
/// raw-gap offsets are 4 KiB-aligned, so this uses `bs=4K`). Pure, so the
/// block-size choice is testable.
fn dd_command(payload: &str, offset: u64) -> String {
    let (bs, seek) = if offset.is_multiple_of(4096) {
        (4096, offset / 4096)
    } else if offset.is_multiple_of(512) {
        (512, offset / 512)
    } else {
        (1, offset)
    };
    format!("dd if={payload} of=/dev/DISK bs={bs} seek={seek} conv=notrunc")
}

#[cfg(test)]
mod tests {

    /// A [`UserspacePin`] from a name and a [`GitPin`]-shaped fixture.
    fn named_pin(name: &str, p: GitPin) -> boot2deb_core::lock::UserspacePin {
        boot2deb_core::lock::UserspacePin {
            name: name.into(),
            source: p.source,
            reference: p.reference,
            commit: p.commit,
        }
    }
    use super::*;
    use crate::sandbox::{SandboxRun, SandboxSpec};
    use crate::test_support::{rk1_build, UnusedSandbox};
    use boot2deb_core::lock::{
        BlobsPin, FfmpegPins, GitPin, KernelPin, PatchesPin, RootfsPin, UbootPin,
    };
    use std::cell::RefCell;

    /// An [`EventSink`] retaining every log line, so a command run inside the packaging
    /// root can be asserted on — `dpkg-deb`'s own report of the archive it built is the
    /// only place that answer exists, and it arrives as events rather than as a
    /// captured stdout.
    #[derive(Default)]
    struct Recorder(RefCell<Vec<String>>);

    impl Recorder {
        /// Drain the retained lines, so consecutive commands are read apart.
        fn take(&self) -> Vec<String> {
            std::mem::take(&mut self.0.borrow_mut())
        }
    }

    impl EventSink for Recorder {
        fn emit(&self, event: crate::event::Event) {
            if let crate::event::Event::Log { line, .. } = event {
                self.0.borrow_mut().push(line);
            }
        }
    }

    /// A packaging root for the tests that never archive anything — every stage
    /// fixture needs one, and only the end-to-end test provisions a real one.
    ///
    /// Pointed at a path that does not exist, deliberately: a test that reached
    /// `ensure_ready` through this would fail rather than quietly bootstrap a tree.
    fn unused_packaging() -> PackagingSandbox {
        let (archive, components) = crate::bootstrap::test_archive("forky");
        PackagingSandbox::new(SandboxSpec {
            rootfs: PathBuf::from("/nonexistent/packaging-root"),
            arch: "amd64".into(),
            archive,
            components,
            keyring: crate::bootstrap::test_keyring(),
            cache_dir: None,
        })
    }

    // The `patches_commit` names the *u-boot* patch pin, since that is what the u-boot
    // tree signature tracks. The kernel `[patches]` pin is held fixed so the tests can
    // also assert the u-boot signature is independent of it.
    fn lock_with(uboot_commit: &str, patches_commit: &str) -> Lock {
        let git = |c: &str| GitPin {
            source: "s".into(),
            reference: "r".into(),
            commit: c.into(),
        };
        Lock {
            kernel: Some(KernelPin {
                id: "k".into(),
                source: "ks".into(),
                reference: "v".into(),
                commit: "kc".into(),
                revision: 1,
                config_sha256: "d".repeat(64),
            }),
            patches: Some(PatchesPin {
                series: vec!["rk3588-accel".into()],
                source: "ps".into(),
                reference: "main".into(),
                commit: "kernel-pc".into(),
            }),
            uboot: Some(UbootPin {
                source: "us".into(),
                reference: "v2026.04".into(),
                commit: uboot_commit.into(),
                revision: 1,
                config_sha256: "9".repeat(64),
            }),
            uboot_patches: Some(PatchesPin {
                series: vec!["rk3576-util".into()],
                source: "ps".into(),
                reference: "main".into(),
                commit: patches_commit.into(),
            }),
            tfa: None,
            userspace: vec![
                named_pin("mpp", git("m")),
                named_pin("librga", git("r")),
                named_pin("libmali", git("l")),
            ],
            ffmpeg: Some(FfmpegPins {
                base: git("b"),
                rockchip: Some(git("rk")),
            }),
            rootfs: Some(RootfsPin {
                distro: "debian".into(),
                suite: "forky".into(),
                manifest: "m".into(),
                manifest_sha256: None,
                source_date_epoch: 1_790_347_034,
            }),
            blobs: Some(BlobsPin {
                source: "https://example.invalid/rkbin.git".into(),
                reference: "master".into(),
                commit: "c".repeat(40),
                atf: Some(format!("rk3588_bl31_v1.56.elf@sha256:{}", "0".repeat(64))),
                tpl: "t".into(),
                bl32: None,
            }),
            kmods: vec![],
            apps: vec![],
            extra_debs: vec![],
            snapshot: None,
        }
    }

    #[test]
    fn blob_vars_are_identical_for_configure_and_compile() {
        // u-boot's Kconfig reads `TEE` from the environment, so a `.config` generated
        // without it lacks OPTEE_LIB/OPTEE_TZDRAM_SIZE and the compile's `syncconfig`
        // stops for interactive input. Both invocations must pass the same variables.
        let vars = |bl32: Option<&str>| {
            let blobs = BlobPaths {
                atf: PathBuf::from("/b/bl31.elf"),
                tpl: PathBuf::from("/b/ddr.bin"),
                bl32: bl32.map(PathBuf::from),
            };
            let mut argv = Vec::new();
            blob_vars(&mut argv, &blobs);
            argv
        };
        // A BL31-only SoC (RK3588/RK1) passes no `TEE`, so its Kconfig sees none.
        assert_eq!(vars(None), ["BL31=/b/bl31.elf", "ROCKCHIP_TPL=/b/ddr.bin"]);
        // A SoC with OP-TEE (RK3576) passes it, at both config and compile time.
        assert_eq!(
            vars(Some("/b/bl32.bin")),
            [
                "BL31=/b/bl31.elf",
                "ROCKCHIP_TPL=/b/ddr.bin",
                "TEE=/b/bl32.bin"
            ]
        );
    }

    #[test]
    fn clone_manifest_tracks_pin_and_dev_inputs() {
        let sig = |uc, pc, patches| {
            clone_manifest(&lock_with(uc, pc), patches)
                .unwrap()
                .signature
        };
        let base = sig("uc1", "pc1", SeriesIdentity::Pinned);
        assert_eq!(base, sig("uc1", "pc1", SeriesIdentity::Pinned));
        // A u-boot bump or a u-boot-patches-pin bump each invalidate the reused tree.
        assert_ne!(base, sig("uc2", "pc1", SeriesIdentity::Pinned));
        assert_ne!(base, sig("uc1", "pc2", SeriesIdentity::Pinned));
        // The u-boot tree signature tracks `[uboot_patches]`, not the kernel `[patches]`
        // pin: bumping the kernel patch commit must NOT move it (regression guard —
        // folding the wrong pin collided every u-boot series in the artifact cache).
        let mut kernel_bumped = lock_with("uc1", "pc1");
        kernel_bumped.patches.as_mut().unwrap().commit = "kernel-pc-2".into();
        assert_eq!(
            base,
            clone_manifest(&kernel_bumped, SeriesIdentity::Pinned)
                .unwrap()
                .signature
        );
        // And two different u-boot series at the same commit stay distinct.
        let mut util = lock_with("uc1", "pc1");
        util.uboot_patches.as_mut().unwrap().series = vec!["rk3576-util".into()];
        let mut util_net = lock_with("uc1", "pc1");
        util_net.uboot_patches.as_mut().unwrap().series = vec!["rk3576-util-net".into()];
        assert_ne!(
            clone_manifest(&util, SeriesIdentity::Pinned)
                .unwrap()
                .signature,
            clone_manifest(&util_net, SeriesIdentity::Pinned)
                .unwrap()
                .signature
        );
        // Co-dev mode splits the key; a co-dev content change restamps.
        let empty: Vec<String> = vec![];
        assert_ne!(base, sig("uc1", "pc1", SeriesIdentity::Dev(&empty)));
        let fp1 = vec!["uboot/010.patch=aaa".to_string()];
        let fp2 = vec!["uboot/010.patch=bbb".to_string()];
        assert_ne!(
            sig("uc1", "pc1", SeriesIdentity::Dev(&fp1)),
            sig("uc1", "pc1", SeriesIdentity::Dev(&fp2))
        );
    }

    #[test]
    fn output_manifest_covers_blobs_defconfig_and_toolchain() {
        let build = rk1_build();
        let env = |tc: &str| BuildEnv {
            cross_compile: None,
            jobs: None,
            toolchain_id: tc.to_string(),
            sandbox_id: String::new(),
            packaging_id: String::new(),
        };
        let boot = build.rkbin_boot().unwrap();
        let man = |lock: &Lock, env: &BuildEnv, patches| {
            output_manifest(&build, boot, lock, env, patches)
                .unwrap()
                .signature
        };
        let base = man(
            &lock_with("uc1", "pc1"),
            &env("gcc-1"),
            SeriesIdentity::Pinned,
        );
        // Stable under identical inputs.
        assert_eq!(
            base,
            man(
                &lock_with("uc1", "pc1"),
                &env("gcc-1"),
                SeriesIdentity::Pinned
            )
        );
        // A u-boot pin bump reaches the output signature through the tree dependency.
        assert_ne!(
            base,
            man(
                &lock_with("uc2", "pc1"),
                &env("gcc-1"),
                SeriesIdentity::Pinned
            )
        );
        // A blob change → new signature (a hit must imply the same verified blobs).
        let mut lock_blob = lock_with("uc1", "pc1");
        lock_blob.blobs.as_mut().unwrap().atf = Some("different-atf-hash".into());
        assert_ne!(base, man(&lock_blob, &env("gcc-1"), SeriesIdentity::Pinned));
        // A revision that moved with every other input equal is another deb version.
        let mut lock_rev = lock_with("uc1", "pc1");
        lock_rev.uboot.as_mut().unwrap().revision = 2;
        assert_ne!(base, man(&lock_rev, &env("gcc-1"), SeriesIdentity::Pinned));
        // Adding/altering the BL32 blob also restamps (the OP-TEE payload is folded).
        let mut lock_bl32 = lock_with("uc1", "pc1");
        lock_bl32.blobs.as_mut().unwrap().bl32 = Some("rk3576_bl32@sha256:cd".into());
        assert_ne!(base, man(&lock_bl32, &env("gcc-1"), SeriesIdentity::Pinned));
        // Toolchain and co-dev mode each split the key.
        assert_ne!(
            base,
            man(
                &lock_with("uc1", "pc1"),
                &env("gcc-2"),
                SeriesIdentity::Pinned
            )
        );
        let empty: Vec<String> = vec![];
        assert_ne!(
            base,
            man(
                &lock_with("uc1", "pc1"),
                &env("gcc-1"),
                SeriesIdentity::Dev(&empty)
            )
        );
    }

    /// A TF-A build point and its lock: the resolved RK1 with `bl31 = tfa`, and
    /// [`lock_with`]'s pins with the rkbin BL31 traded for a `[tfa]` table.
    fn tfa_point(tfa_commit: &str) -> (ResolvedBuild, Lock) {
        let build = boot2deb_core::resolve::resolve_recipe(
            &crate::test_support::repo_root(),
            crate::test_support::RK1_MEDIA_ACCEL,
            &boot2deb_core::model::Overrides {
                bl31: Some(boot2deb_core::model::Bl31Provider::Tfa),
                ..Default::default()
            },
        )
        .unwrap();
        let mut lock = lock_with("uc1", "pc1");
        lock.blobs.as_mut().unwrap().atf = None;
        lock.tfa = Some(TfaPin {
            source: "https://example.invalid/trusted-firmware-a.git".into(),
            reference: "v2.16.0".into(),
            commit: tfa_commit.repeat(40),
        });
        (build, lock)
    }

    #[test]
    fn the_output_signature_folds_the_bl31_provider_and_its_identity() {
        let env = BuildEnv {
            toolchain_id: "gcc-1".into(),
            ..Default::default()
        };
        let rkbin = rk1_build();
        let rkbin_sig = output_manifest(
            &rkbin,
            rkbin.rkbin_boot().unwrap(),
            &lock_with("uc1", "pc1"),
            &env,
            SeriesIdentity::Pinned,
        )
        .unwrap()
        .signature;
        let tfa_sig = |commit: &str| {
            let (build, lock) = tfa_point(commit);
            output_manifest(
                &build,
                build.rkbin_boot().unwrap(),
                &lock,
                &env,
                SeriesIdentity::Pinned,
            )
            .unwrap()
            .signature
        };
        // The same u-boot with the other BL31 is a different output.
        assert_ne!(rkbin_sig, tfa_sig("e"));
        // A TF-A re-pin reaches u-boot's key through the TF-A node's input signature.
        assert_eq!(tfa_sig("e"), tfa_sig("e"));
        assert_ne!(tfa_sig("e"), tfa_sig("f"));
        // A `tfa` point whose lock still pins the rkbin BL31 (no `[tfa]`) is refused
        // rather than keyed as something it is not.
        let (build, _) = tfa_point("e");
        assert!(output_manifest(
            &build,
            build.rkbin_boot().unwrap(),
            &lock_with("uc1", "pc1"),
            &env,
            SeriesIdentity::Pinned,
        )
        .is_err());
    }

    fn tfa_pin() -> TfaPin {
        TfaPin {
            source: "https://example.invalid/trusted-firmware-a.git".into(),
            reference: "v2.16-rc0".into(),
            commit: "9347b8bbd8d56da5cfbf7d9f1af3ca4f6d6e3920".into(),
        }
    }

    /// The package is versioned by the lock, revision included. A lock with no BL31 to
    /// name has no version, and one with no u-boot fails on that first.
    #[test]
    fn the_deb_version_is_the_locks() {
        let mut lock = lock_with("uc1", "pc1");
        assert_eq!(deb_version(&lock).unwrap(), "2026.04+rkbin1.56-1");
        lock.uboot.as_mut().unwrap().revision = 4;
        assert_eq!(deb_version(&lock).unwrap(), "2026.04+rkbin1.56-4");
        lock.blobs.as_mut().unwrap().atf = None;
        assert!(matches!(
            deb_version(&lock),
            Err(EngineError::MissingPin {
                what: "blobs.atf",
                ..
            })
        ));
        lock.uboot = None;
        assert!(matches!(
            deb_version(&lock),
            Err(EngineError::MissingPin { what: "uboot", .. })
        ));
    }

    #[test]
    fn the_copyright_names_every_component_and_carries_tfas_license() {
        let lock = lock_with("uc1", "pc1");
        let rkbin = copyright_text(
            "u-boot-turing-rk1",
            &lock,
            &DebBl31::Rkbin {
                file: "rk3588_bl31_v1.56.elf",
            },
        )
        .unwrap();
        assert!(rkbin.contains("Version: v2026.04 (uc1)"), "{rkbin}");
        assert!(rkbin.contains("License: GPL-2.0+"));
        assert!(rkbin.contains("File:    rk3588_bl31_v1.56.elf"));
        assert!(rkbin.contains("License: rkbin"));
        assert!(!rkbin.contains("BSD-3-Clause"));
        let pin = tfa_pin();
        let tfa = copyright_text(
            "u-boot-turing-rk1",
            &lock,
            &DebBl31::Tfa {
                pin: &pin,
                license: "License\n=======\n\nBSD-3-Clause terms.\n".into(),
            },
        )
        .unwrap();
        assert!(tfa.contains("Trusted Firmware-A"));
        assert!(tfa.contains("Version:   v2.16-rc0 (9347b8bbd8d56da5cfbf7d9f1af3ca4f6d6e3920)"));
        assert!(tfa.contains("Copyright: Arm Limited and Contributors"));
        assert!(tfa.contains("License:   BSD-3-Clause"));
        // Every line a reader sees is terminal-width, the rkbin terms included.
        assert!(tfa.lines().all(|l| l.len() <= 80), "{tfa}");
        assert!(tfa.ends_with("BSD-3-Clause terms.\n"), "{tfa}");
        // The DDR TPL is rkbin's under either provider.
        assert!(tfa.contains("DDR init TPL") && tfa.contains("License: rkbin"));
        assert!(!tfa.contains("rk3588_bl31"));
    }

    #[test]
    fn dd_command_picks_block_size_by_alignment() {
        // 4 KiB-aligned RK offsets use bs=4K seeks.
        assert_eq!(
            dd_command("/p/idbloader.img", 32768),
            "dd if=/p/idbloader.img of=/dev/DISK bs=4096 seek=8 conv=notrunc"
        );
        assert!(dd_command("/p/u-boot.itb", 8 * 1024 * 1024).contains("bs=4096 seek=2048"));
        // 512-aligned but not 4 KiB → bs=512; unaligned → bs=1.
        assert!(dd_command("x", 512).contains("bs=512 seek=1"));
        assert!(dd_command("x", 513).contains("bs=1 seek=513"));
    }

    #[test]
    fn control_text_has_fields_and_no_depends() {
        let c = control_text(
            "u-boot-turing-rk1",
            "2026.04",
            "arm64",
            "Turing RK1 (RK3588)",
            "rk3588",
        );
        assert!(c.contains("Package: u-boot-turing-rk1"));
        assert!(c.contains("Version: 2026.04"));
        assert!(c.contains("Architecture: arm64"));
        // Data-only package: no runtime deps, and it must never auto-flash.
        assert!(!c.contains("Depends:"));
        assert!(c.contains("does NOT flash"));
        // Description continuation lines are space-prefixed per deb-control.
        assert!(c.lines().any(|l| l.starts_with(" Stages the SPL")));
    }

    #[test]
    fn install_conf_records_offsets() {
        let conf = install_conf_text("turing-rk1", "rockchip-rkbin", "tfa", 32768, 8_388_608);
        assert!(conf.contains("device=turing-rk1"));
        assert!(conf.contains("boot_method=rockchip-rkbin"));
        assert!(conf.contains("bl31=tfa"));
        assert!(conf.contains("idbloader_offset=32768"));
        assert!(conf.contains("uboot_itb_offset=8388608"));
    }

    #[test]
    fn readme_documents_offsets_and_dd() {
        let r = readme_text(
            "turing-rk1",
            "Turing RK1 (RK3588)",
            "32KiB",
            32768,
            "8MiB",
            8_388_608,
            "The BL31 inside u-boot.itb is X.",
        );
        assert!(r.contains("The BL31 inside u-boot.itb is X."));
        assert!(r.contains("/usr/share/doc/u-boot-turing-rk1/copyright"));
        assert!(r.contains("offset 32768 bytes (32KiB)"));
        assert!(r.contains("offset 8388608 bytes (8MiB)"));
        assert!(r.contains("bs=4096 seek=8"));
        assert!(r.contains("bs=4096 seek=2048"));
        assert!(r.contains("/dev/DISK"));
    }

    #[test]
    fn stage_tree_lays_out_the_package() {
        let tmp = tempfile::tempdir().unwrap();
        let build = rk1_build();
        let payloads = tmp.path().join("payloads");
        std::fs::create_dir_all(&payloads).unwrap();
        let idb = payloads.join("idbloader.img");
        let itb = payloads.join("u-boot.itb");
        std::fs::write(&idb, b"IDBLOADER").unwrap();
        std::fs::write(&itb, b"UBOOTITB").unwrap();

        let pkg_stage = tmp.path().join("pkg-stage");
        let boot = build.rkbin_boot().unwrap();
        stage_tree(
            &pkg_stage,
            &build,
            boot,
            &DebIdentity {
                pkg: "u-boot-turing-rk1",
                version: "2026.04",
                arch: "arm64",
            },
            Payloads {
                idbloader: &idb,
                uboot_itb: &itb,
            },
            "COPYRIGHT",
            &DebBl31::Rkbin {
                file: "rk3588_bl31_v1.56.elf",
            },
        )
        .unwrap();

        // Payloads + install.conf land under /usr/lib/u-boot/<device>/.
        let libd = pkg_stage.join("usr/lib/u-boot/turing-rk1");
        assert_eq!(
            std::fs::read(libd.join("idbloader.img")).unwrap(),
            b"IDBLOADER"
        );
        assert_eq!(std::fs::read(libd.join("u-boot.itb")).unwrap(), b"UBOOTITB");
        let conf = std::fs::read_to_string(libd.join("install.conf")).unwrap();
        assert!(conf.contains("idbloader_offset=32768")); // rk1: idbloader @ 32KiB

        // control + doc present; no maintainer scripts (never auto-flash).
        assert!(pkg_stage.join("DEBIAN/control").exists());
        assert!(pkg_stage
            .join("usr/share/doc/u-boot-turing-rk1/README.Debian")
            .exists());
        assert_eq!(
            std::fs::read_to_string(pkg_stage.join("usr/share/doc/u-boot-turing-rk1/copyright"))
                .unwrap(),
            "COPYRIGHT"
        );
        assert!(conf.contains("bl31=rkbin"));
        assert!(!pkg_stage.join("DEBIAN/postinst").exists());
    }

    /// End-to-end through a real packaging root: the staged tree becomes a `.deb`, that
    /// root's own `dpkg` reads it back and accepts the control stanza, and two
    /// packagings at one epoch produce identical bytes.
    ///
    /// One test rather than three, because the expensive part is provisioning the root
    /// and all three assertions want the same one.
    ///
    /// Nothing here touches a host `dpkg-deb`: the archive is built *and* inspected
    /// inside the root, which is what makes this an assertion about the tool a build
    /// actually uses rather than about whatever the test machine has installed.
    #[test]
    fn the_packaging_root_archives_a_deb_dpkg_accepts_and_reproduces() {
        let sink = Recorder::default();
        let step = Step::start(&sink, "uboot");
        let Some(packaging) = crate::sandbox::packaging_root_for_tests(&step) else {
            return;
        };

        let tmp = tempfile::tempdir().unwrap();
        let build = rk1_build();
        let idb = tmp.path().join("idbloader.img");
        let itb = tmp.path().join("u-boot.itb");
        std::fs::write(&idb, b"IDB").unwrap();
        std::fs::write(&itb, b"ITB").unwrap();
        let dummy = tmp.path().join("dummy");

        // Two independent packagings of the same inputs at the same epoch. `dpkg-deb`
        // clamps the staged files' build-clock mtimes to it, so no wall clock reaches
        // the archive and the two runs must agree byte for byte — no sleep needed,
        // since both stage far newer than the 2020 epoch and both clamp to it.
        let package = |tag: &str| -> PathBuf {
            let work = tmp.path().join(tag);
            std::fs::create_dir_all(&work).unwrap();
            let opts = UbootOptions {
                source: "unused",
                patches: None,
                tfa_source: "unused",
                blobs: BlobSource::Dir(&dummy),
                work_dir: &work,
                cross: &UnusedSandbox,
                out_dir: &work,
                packaging: &packaging,
                stem: "turing-rk1-forky",
                store: None,
            };
            let boot = build.rkbin_boot().unwrap();
            package_deb(
                &build,
                boot,
                &lock_with("uc1", "pc1"),
                &opts,
                Some(1_600_000_000),
                Payloads {
                    idbloader: &idb,
                    uboot_itb: &itb,
                },
                &DebBl31::Rkbin {
                    file: "rk3588_bl31_v1.56.elf",
                },
                &step,
            )
            .unwrap()
        };
        let (a, b) = (package("run-a"), package("run-b"));
        assert_eq!(
            std::fs::read(&a).unwrap(),
            std::fs::read(&b).unwrap(),
            "two packagings at one epoch differ"
        );
        assert!(
            a.file_name().unwrap().to_str().unwrap()
                == "u-boot-turing-rk1_2026.04+rkbin1.56-1_arm64.deb",
            "unexpected deb name: {}",
            a.display()
        );

        // Read the archive back with the root's own dpkg. `-I` reports the control
        // stanza it parsed, `-c` the data members with their ownership and paths.
        let inspect = |flag: &str| -> String {
            sink.take();
            let argv = vec![
                "dpkg-deb".to_string(),
                flag.to_string(),
                a.to_string_lossy().into_owned(),
            ];
            packaging
                .run(
                    &SandboxRun {
                        work: a.parent().unwrap(),
                        binds: &[tmp.path().to_path_buf()],
                        env: &[],
                        argv: &argv,
                        context: "inspect the packaged deb",
                        probe: None,
                    },
                    &step,
                )
                .unwrap();
            sink.take().join("\n")
        };

        let info = inspect("-I");
        assert!(
            info.contains("Package: u-boot-turing-rk1"),
            "info was: {info}"
        );
        assert!(
            info.contains("Version: 2026.04+rkbin1.56-1"),
            "info was: {info}"
        );
        assert!(info.contains("Architecture: arm64"), "info was: {info}");

        let contents = inspect("-c");
        for path in [
            "/usr/lib/u-boot/turing-rk1/idbloader.img",
            "/usr/lib/u-boot/turing-rk1/u-boot.itb",
            "/usr/lib/u-boot/turing-rk1/install.conf",
            "/usr/share/doc/u-boot-turing-rk1/copyright",
        ] {
            assert!(contents.contains(path), "{path} missing from: {contents}");
        }
        // The `fakeroot`-less claim, asserted rather than assumed: the root maps the
        // caller to uid 0, so a tree the build user staged on the host is archived with
        // the ownership a `.deb` must have. A member owned by anyone else would install
        // files owned by whoever happened to run the build.
        for line in contents.lines().filter(|l| l.contains("/usr/lib/u-boot/")) {
            assert!(
                line.contains("root/root"),
                "archive member is not root-owned: {line}"
            );
        }
    }

    #[test]
    fn maskrom_images_stage_only_when_binman_emitted_both() {
        let tmp = tempfile::tempdir().unwrap();
        let tree = tmp.path().join("u-boot");
        let out = tmp.path().join("out");
        std::fs::create_dir_all(&tree).unwrap();
        std::fs::create_dir_all(&out).unwrap();
        let sink = |_: crate::event::Event| {};
        let step = Step::start(&sink, "uboot");
        let opts = UbootOptions {
            source: "unused",
            patches: None,
            tfa_source: "unused",
            blobs: BlobSource::Dir(tmp.path()),
            work_dir: tmp.path(),
            cross: &UnusedSandbox,
            out_dir: &out,
            packaging: &unused_packaging(),
            stem: "turing-rk1-forky",
            store: None,
        };

        // A board without CONFIG_ROCKCHIP_MASKROM_IMAGE emits neither file: no images,
        // no error, and nothing staged.
        assert_eq!(collect_maskrom(&opts, &tree, &step).unwrap(), None);
        assert_eq!(maskrom_in(&out), None);

        // A lone file is a binman contract break, reported rather than half-staged.
        std::fs::write(tree.join(MASKROM_USB471), b"471").unwrap();
        assert!(matches!(
            collect_maskrom(&opts, &tree, &step),
            Err(EngineError::ArtifactMissing { what, .. }) if what == MASKROM_USB472
        ));

        // Both present (plus a .config naming the SoC): the pair is staged and the
        // merged RKBOOT loader is packed from them. Published under the build point's
        // stem, so a second recipe on this board does not overwrite them...
        std::fs::write(tree.join(MASKROM_USB472), b"472").unwrap();
        std::fs::write(tree.join(".config"), "CONFIG_ROCKCHIP_RK3576=y\n").unwrap();
        let m = collect_maskrom(&opts, &tree, &step).unwrap().unwrap();
        assert_eq!(
            m.usb471,
            out.join(format!("turing-rk1-forky-{MASKROM_USB471}"))
        );
        assert_eq!(
            m.usb472,
            out.join(format!("turing-rk1-forky-{MASKROM_USB472}"))
        );
        assert_eq!(
            m.loader,
            out.join(format!("turing-rk1-forky-{MASKROM_LOADER}"))
        );
        assert_eq!(std::fs::read(&m.usb472).unwrap(), b"472");
        // The loader is a real RKBOOT container ("LDR " tag) built from the payloads.
        assert_eq!(&std::fs::read(&m.loader).unwrap()[..4], b"LDR ");
        // ...while all three keep their canonical names in the tree, which is what the
        // artifact cache stores and what `maskrom_in` reads back on a restore.
        assert_eq!(
            maskrom_in(&tree),
            Some(MaskromImages {
                usb471: tree.join(MASKROM_USB471),
                usb472: tree.join(MASKROM_USB472),
                loader: tree.join(MASKROM_LOADER),
            })
        );

        // Without a chip code in .config, the payloads exist but the loader cannot
        // be packed — surfaced rather than silently skipped.
        std::fs::write(tree.join(".config"), "CONFIG_FOO=y\n").unwrap();
        std::fs::remove_file(tree.join(MASKROM_LOADER)).unwrap();
        assert!(matches!(
            collect_maskrom(&opts, &tree, &step),
            Err(EngineError::ArtifactMissing { .. })
        ));
    }

    /// The u-boot stage names a disagreement between u-boot's default fdtfile and the
    /// DTB name the efi image uses, and says nothing when they agree.
    #[test]
    fn a_default_fdtfile_other_than_the_esp_dtb_is_named() {
        let tree = tempfile::tempdir().unwrap();
        std::fs::write(
            tree.path().join(".config"),
            "CONFIG_FOO=y\nCONFIG_DEFAULT_FDT_FILE=\"rockchip/rk3576-generic.dtb\"\n",
        )
        .unwrap();
        let line = fdtfile_mismatch(tree.path(), "rockchip/rk3576-h96-max-m9-npu.dtb").unwrap();
        assert!(line.contains("rockchip/rk3576-generic.dtb"), "{line}");
        assert!(line.contains("uboot_fdtfile"), "{line}");
        assert_eq!(
            fdtfile_mismatch(tree.path(), "rockchip/rk3576-generic.dtb"),
            None
        );
    }
}
