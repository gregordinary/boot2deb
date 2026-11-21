//! Rootfs node — bootstraps the device's Debian userland and emits the
//! `tar` archive the [image node](crate::image) formats into ext4, plus a
//! content-pinned solved package manifest.
//!
//! The bootstrap is in-process and rootless: no `sudo`, no persistent chroot, and no
//! external bootstrap binary. [`build_rootfs`] drives ferroday-cage's Debian
//! provisioner, which does four things:
//!
//! 1. Install the merged rootfs package set ([`ResolvedImage::rootfs_packages`]) from
//!    the build's own [`LocalDistsRepo`](crate::repo::LocalDistsRepo) plus the suite
//!    mirror, so each `.deb`'s dependencies resolve together.
//! 2. Lay the staged overlay in.
//! 3. Run the target config (user account, ssh first-boot, group membership).
//! 4. Re-run the kernel `postinst.d` hooks, so `/boot` gains the initrd, board dtb,
//!    and `extlinux.conf` u-boot loads.
//!
//! The kernel configured before the overlay's hooks existed, so its own postinst
//! produced none of those boot files. The overlay is the layered substrate merged in
//! Rust beforehand. It is each layer's `overlay/` tree followed by the *generated*
//! config this module produces from the resolved values (fstab, hostname, apt
//! sources, locale, machine-id).
//!
//! This module is responsible for the node's inputs, the two staged overlay trees, and the
//! generated config. The bootstrap itself is the `provisioner` submodule.
//!
//! The manifest is fully content-pinned, `name + version + arch + sha256` per
//! package, from the archive-recorded digests in the resolved plan. Each row
//! therefore names the exact `.deb` the image contains.
//!
//! Board firmware (rockchip display, panthor Mali, Realtek NIC) is staged as Debian
//! `non-free-firmware` packages declared on the SoC layer. For RK3588 that is
//! `firmware-misc-nonfree` + `firmware-realtek` in `socs/rk3588.toml`. It is not
//! extracted from a linux-firmware tarball, so it is snapshot-pinned like every
//! other package and needs no separate fetch step.

mod provisioner;
pub use provisioner::{
    build_rootfs, read_plan_record, read_plan_weights, sweep_provisioned, PlanRecord, PlanWeights,
};
pub(crate) use provisioner::{feature_repositories, provisioned_dir, ProvisionedRoot};

use crate::error::EngineError;
use crate::event::Step;
use boot2deb_core::distro::{InitramfsGenerator, TimeClient};
use boot2deb_core::model::{AptSource, ImageBuild, InitramfsCompress, ResolvedImage};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Filesystem inputs for the rootfs node.
pub struct RootfsOptions<'a> {
    /// The build's `.deb`s (kernel, u-boot, MPP/RGA, ffmpeg-rk, …) — assembled
    /// into a [`LocalDistsRepo`](crate::repo::LocalDistsRepo) the bootstrap
    /// installs the accel packages from.
    pub repo_debs: &'a [PathBuf],
    /// Layer `overlay/` directories to lay into the rootfs **after** every package,
    /// in merge order (base → soc → boot-method → device → features). Non-existent
    /// entries are skipped, and later entries win. That includes winning over the
    /// packages' own files, which is the point of laying them in last.
    pub overlay_dirs: &'a [PathBuf],
    /// Layer `overlay-pre/` directories to lay into the rootfs **before** any package
    /// is installed, same merge order.
    ///
    /// This is for config a package's maintainer scripts must see *while they run*,
    /// where winning afterwards is too late. That covers the initramfs settings that
    /// decide how big the first initramfs is. On a depthcharge board it also covers
    /// the `depthcharge-tools` config. That stops its kernel hook from hunting the
    /// build host's disks for a ChromeOS kernel partition to write to. Empty for a
    /// board that needs none.
    pub preinstall_overlay_dirs: &'a [PathBuf],
    /// The boot-method-specific configuration written into the pre-install overlay
    /// ([`BootConfig`]), or `None` where the boot method generates none.
    pub boot_config: Option<BootConfig<'a>>,
    /// The image's account of itself
    /// ([`SystemIdentity`](boot2deb_core::provenance::SystemIdentity)), written into the
    /// rootfs at `/etc/boot2deb/image.toml`.
    ///
    /// It ships *inside* the image because its whole purpose is to be readable when the
    /// image is all you have. That means from other media, without mounting it, and
    /// possibly on a machine that is not this board. That last case is the one that
    /// decides its contents. `depthchargectl` recovers a board profile from the
    /// *running* board's HWID. A tool re-signing this image's kernel from somewhere
    /// else has no way to learn it except by being told here.
    ///
    /// Written as part of the staged overlay, so it folds into the rootfs cache key with
    /// every other generated file ([`crate::rootcache::dir_fingerprints`]). A cached
    /// tree can never be reused under an identity that disagrees with it.
    pub image_identity: &'a boot2deb_core::provenance::SystemIdentity,
    /// The partitions the image node will write, as the rootfs names them
    /// ([`Partitions`]).
    ///
    /// They are *inputs* to the rootfs, not outputs of the partition table.
    /// `depthchargectl` derives the signed kernel's `root=` from the image's own
    /// `/etc/fstab`. Under the `efi` loader, update-grub's menu and GRUB's core image are
    /// written before the partitions exist. Each has to name them first. Every value
    /// reaches the rootfs cache key, through the staged fstab or explicitly. A cached
    /// tree can therefore never name a partition the table does not.
    pub partitions: Partitions,
    /// Directory the rootfs tarball is written to.
    pub out_dir: &'a Path,
    /// The build point's
    /// [artifact stem](boot2deb_core::buildpoint::BuildPoint::artifact_stem) — the
    /// tarball is published as `<stem>-rootfs.tar`. A board has several recipes
    /// with different package sets. A tarball named for the board alone would let
    /// one recipe's userland be formatted into another's image.
    pub stem: &'a str,
    /// The build's scratch tree — where this node stages the overlays and provisions
    /// the rootfs before exporting it.
    ///
    /// Deliberately **not** `TMPDIR`, for the same reason as
    /// [`build_root_uppers`](crate::sandbox::build_root_uppers). The provisioned tree
    /// is the whole target userland (multiple GB) containing `security.capability`
    /// xattrs and subordinate-mapped ownership. On most desktop distros `/tmp` is a
    /// RAM-backed `tmpfs` sized to a fraction of physical memory. `TMPDIR` would
    /// therefore make the build's success a property of the host's mount table. A
    /// `nodev`/`nosuid` `TMPDIR` would change what the tree can hold.
    ///
    /// The work dir is where the build's other large intermediates already live, and it
    /// is the directory [`doctor`](crate::checks) probes. Its capabilities are
    /// therefore a *checked* host requirement rather than an inherited one.
    pub scratch_dir: &'a Path,
    /// The archive keyring verifying every pocket's `Release`, as the sandboxes use.
    pub keyring: &'a Path,
    /// Identity of the `qemu-user` interpreter that executes the target's maintainer
    /// scripts
    /// ([`qemu_identity`](crate::toolchain::HostToolchain::qemu_identity)). It is
    /// `None` on a native host, where target binaries run directly and nothing is
    /// interpreted.
    ///
    /// It is an input to the *tree*, not merely a fact about the host. On a cross host
    /// every maintainer script runs under it, and so does everything they invoke. That
    /// covers the initramfs generator writing the initrd this image boots, `ldconfig`,
    /// `locale-gen`, and on a depthcharge board the kernel signing. So it is folded
    /// into the rootfs cache key ([`crate::rootcache::CacheKeyInputs`]), and a tree
    /// configured under one interpreter is never restored for a build using another.
    ///
    /// One residual remains. The binfmt handler is registered `F`-flagged, so the
    /// kernel pins an fd to the interpreter at registration time. This identity is
    /// probed from `PATH` instead. Debian's `qemu-user-static` re-registers as it
    /// upgrades, so the two agree. A host that registers its handler by hand has a
    /// window where they do not.
    pub interpreter_id: Option<&'a str>,
    /// Where the solved package manifest is written (the lock's `[rootfs].manifest`
    /// points at it).
    pub manifest_out: &'a Path,
    /// A plan document from an earlier build, replayed instead of resolving one
    /// (`reproduce`). `None` — every ordinary build — resolves against the mirrors.
    ///
    /// Set, the archive is not consulted at all. No release is fetched and no package
    /// index is. The exact versions the document names are installed by the digests it
    /// records.
    ///
    /// **That moves the trust anchor.** An ordinary build's package digests chain to
    /// the archive signature that vouched for the index they came from. A replay's
    /// chain to this document, because the index that would have contained the signature
    /// is never read. Each `.deb` is still verified against the digest here, so a
    /// mirror serving different bytes is caught. What is no longer checked is that the
    /// document itself describes a set the archive ever offered.
    ///
    /// That trade is right for reproducing a published image and wrong for a routine
    /// build. It is therefore reachable only through the command that names it.
    pub pinned_plan: Option<&'a Path>,
    /// The archive the bootstrap resolves against: every pocket of the image's suite.
    /// Each pocket is at the live mirror, plus the distribution's point-in-time copy
    /// when a captured snapshot is activated ([`Archive::resolve`](crate::Archive::resolve)).
    /// Each pocket's mirrors are tried in order, so a `fallback` snapshot backfills the
    /// live mirror's 404s.
    pub archive: &'a crate::Archive,
    /// Extra package names to install beyond [`ResolvedImage::rootfs_packages`].
    /// They are build artifacts whose names are version-specific and thus unknowable
    /// to the static config, above all the freshly built kernel image.
    pub extra_packages: &'a [String],
    /// Root of the content-addressed rootfs cache. `Some(dir)` enables the
    /// early-cutoff cache. The resolved plan keys a store under `<dir>/rootfs/`,
    /// and an unchanged solved set restores a stored tree instead of
    /// re-bootstrapping ([`crate::rootcache`]). It also roots the persistent
    /// `.deb` download cache the provisioner reuses across builds. `None` always
    /// bootstraps.
    pub cache_dir: Option<&'a Path>,
    /// Ignore a cache hit and re-bootstrap, refreshing the stored entry
    /// (`--refresh-rootfs`). The solve still runs and the fresh tree is re-stored.
    pub refresh: bool,
    /// Third-party apt repositories the selected features contribute
    /// ([`ResolvedImage::apt_sources`]), each paired with the host
    /// path its `signed_by` keyring resolved to. Rendered into signed `deb` positionals
    /// so apt resolves an out-of-mirror app (e.g. Jellyfin) during the bootstrap solve.
    /// Empty when no feature adds one.
    pub apt_sources: &'a [AptRepo<'a>],
    /// The rootfs build's `SOURCE_DATE_EPOCH`: the lock's
    /// [`source_date_epoch`](boot2deb_core::lock::RootfsPin::source_date_epoch).
    ///
    /// It is the upper bound the tar export clamps every member's mtime to, and the time
    /// the staged overlays are stamped with. The bootstrap's maintainer scripts and the
    /// target-side customize step both run under it. What the account tools,
    /// `mkinitramfs` and `useradd` write therefore contains it rather than the wall clock.
    /// It dates the local `.deb` pool's `Release` too, and it is folded into the rootfs
    /// cache key.
    pub source_date_epoch: u64,
}

/// What the rootfs names of the partitions the image node will write, before they
/// exist. The caller derives every value from the image's identity and geometry
/// ([`crate::image::partitions`]), the same ones the image node then writes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Partitions {
    /// The rootfs partition's PARTUUID, which `/etc/fstab` mounts root by and every
    /// loader's menu roots on.
    pub rootfs_partuuid: uuid::Uuid,
    /// The rootfs filesystem's UUID, which update-grub's menu searches for under the
    /// `efi` loader.
    pub rootfs_fs_uuid: uuid::Uuid,
    /// The rootfs partition's GPT entry number. GRUB's core image finds the rootfs as
    /// this partition of the disk the firmware loaded it from.
    pub rootfs_index: u32,
    /// The EFI system partition's PARTUUID, which `/etc/fstab` mounts it by, under the
    /// `efi` loader. `None` under a loader with no partition.
    pub esp_partuuid: Option<uuid::Uuid>,
}

/// The boot-method configuration the rootfs generates, for a method that needs the
/// *rootfs itself* to participate in booting.
///
/// `rockchip-rkbin` needs none: its bootloader is written to the raw gap by the image
/// node, and the in-rootfs half (the extlinux hooks) is static overlay files.
///
/// `depthcharge` is different in kind. The boot payload **is** the kernel, signed, and
/// it is built inside the rootfs by `depthchargectl`. The rootfs therefore has to be
/// told which board profile to sign for and what command line to bake in. It has to be
/// told *before* the kernel package installs and triggers the whole chain.
#[derive(Debug, Clone, Copy)]
pub enum BootConfig<'a> {
    /// Configure `depthcharge-tools` inside the rootfs.
    Depthcharge {
        /// The board profile to sign for (`depthchargectl`'s `board` codename).
        board: &'a str,
        /// The kernel command line to bake into the signature, **without** `root=` —
        /// which `depthchargectl` derives from `/etc/fstab`, here and again on every
        /// on-device kernel upgrade.
        cmdline: &'a str,
        /// The compressor `mkinitramfs` builds this board's initramfs with, resolved
        /// from the kernel slot's size. It has to be in place before the kernel package
        /// is, or the first initramfs is built with the wrong one and thrown away.
        initramfs_compress: InitramfsCompress,
    },
}

/// A resolved third-party apt repository activated during the rootfs bootstrap solve:
/// a feature's [`AptSource`] paired with the host path its `signed_by`
/// keyring resolved to. The CLI resolves and existence-checks the keyring, a vendored
/// build-host prerequisite. The rootfs stage turns the pair into a signed provisioner
/// repository. The repo's `Release` signature is therefore verified against that
/// keyring during the solve, rather than trusted blindly.
pub struct AptRepo<'a> {
    /// The resolved feature source (uri / suite / components / name / signed_by).
    pub source: &'a AptSource,
    /// Host path the source's `signed_by` keyring resolved to (CLI-verified to exist).
    pub keyring: PathBuf,
}

/// What [`build_rootfs`] produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootfsArtifacts {
    /// The rootfs `tar` archive (device nodes excluded), consumed by the image node's
    /// ext4 formatter. The image's account ([`ResolvedImage::user`]) is **locked** in
    /// it. The unique per-image first-boot password is spliced into the staged tree at
    /// image assembly, not into this shared, cacheable tarball.
    pub tar: PathBuf,
    /// The solved package manifest (`name version arch sha256` per line).
    pub manifest: PathBuf,
    /// The plan document: the install set *and* the archive state it resolved against,
    /// in the archive's own deb822 control format.
    ///
    /// The manifest beside it answers "which package bytes shipped". This additionally
    /// answers "selected from what", and it is the form a later
    /// [`pinned_plan`](RootfsOptions::pinned_plan) replays. A `reproduce` run republishes
    /// the document it was given, byte for byte, rather than re-rendering the plan it
    /// parsed — see [`RootfsProvenance::plan_sha256`](boot2deb_core::provenance::RootfsProvenance::plan_sha256).
    ///
    /// What it contributes to the provenance manifest is read back off this file by
    /// [`read_plan_record`], not passed alongside it. The manifest describes the
    /// published document, so the digest it records must be the digest of what was
    /// published.
    pub plan: PathBuf,
}

/// Copy the layer overlay trees into `staging` (each existing dir, in order, via
/// `cp -a` so symlinks and the executable bit survive). Later layers win. The
/// caller normalizes the merged tree's modes with [`normalize_overlay_modes`].
///
/// `--remove-destination` is what makes "later layers win" hold for every file a
/// layer may legitimately ship. `cp` opens an existing destination for writing, so
/// overriding a file an earlier layer shipped without the owner-write bit — a key, a
/// `sudoers.d` drop-in — would fail with `Permission denied` instead of replacing it;
/// unlinking first sidesteps the destination's mode entirely.
///
/// A layer's symlink is copied as a symlink and never resolved: its target is a path on
/// the target system, and the build host is not that system. Unlinking first also means
/// a later layer's file replaces an earlier layer's link rather than writing through
/// it, and `cp` refuses to merge a later layer's directory into an earlier layer's link
/// (`cannot overwrite non-directory`), so the merge itself never leaves the staging
/// tree. Everything the rootfs stage then generates into the merged tree goes through
/// [`place_staged`], which holds the same line.
fn copy_overlay_trees(
    staging: &Path,
    overlay_dirs: &[PathBuf],
    step: &Step,
) -> Result<(), EngineError> {
    let _ = std::fs::remove_dir_all(staging);
    std::fs::create_dir_all(staging).map_err(|s| EngineError::io(staging, s))?;
    for dir in overlay_dirs {
        if dir.exists() {
            let mut cmd = Command::new("cp");
            cmd.arg("-a")
                .arg("--remove-destination")
                .arg(format!("{}/.", dir.display()))
                .arg(staging);
            crate::build::run(cmd, "cp", &format!("stage overlay {}", dir.display()), step)?;
        }
    }
    Ok(())
}

/// Force every mode in a staged overlay tree to the mode the image must ship, so the
/// build host's umask cannot reach the rootfs: each directory to `0755`, each file to
/// `0755` if any execute bit is set and `0644` otherwise, symlinks left alone.
///
/// The rule is **git's own file model**, and that is what makes it lossless here. Git
/// records exactly two file modes (`100644` and `100755`) and records directory modes
/// not at all, so an overlay tree's checked-out modes are not authored data — they are
/// the developer's umask applied to git's two bits. Restating those bits is therefore
/// the identity function on everything the repository actually expresses, while
/// discarding the one thing it does not: under a `002` umask a checkout materializes
/// `0664`/`0775` files and `0775` directories, and a `cp -a` into the staging tree
/// followed by a mode-preserving copy into the rootfs would stamp a group-writable
/// `/etc`, `/usr`, and `/boot` into every image. Under `077` it would stamp `0700`
/// `/etc` and the image would not work at all.
///
/// It also covers the generated config: [`place_staged`] sets each file's own mode but
/// creates missing parent directories with `mkdirat`, which applies the umask.
///
/// An overlay that needs a mode outside git's two — a `0440` sudoers drop-in, a `0700`
/// home — cannot express it in a git tree in the first place, and so belongs in the
/// customize script, which runs as root inside the cage. Applying this after the
/// merge is what keeps that boundary honest rather than accidental.
fn normalize_overlay_modes(root: &Path) -> Result<(), EngineError> {
    let meta = std::fs::symlink_metadata(root).map_err(|s| EngineError::io(root, s))?;
    if meta.file_type().is_symlink() {
        // A symlink's own mode is not consulted by the kernel, and `chmod` would
        // follow it and rewrite the target instead.
        return Ok(());
    }
    if meta.is_dir() {
        set_mode(root, 0o755)?;
        let mut children: Vec<PathBuf> = std::fs::read_dir(root)
            .map_err(|s| EngineError::io(root, s))?
            .map(|e| e.map(|e| e.path()).map_err(|s| EngineError::io(root, s)))
            .collect::<Result<_, _>>()?;
        // Deterministic recursion order (cosmetic; modes are order-independent).
        children.sort();
        for child in children {
            normalize_overlay_modes(&child)?;
        }
    } else {
        use std::os::unix::fs::PermissionsExt;
        // Any execute bit means git recorded `100755` — the hooks and first-boot
        // scripts, whose `+x` is required (the runner refuses a non-executable
        // hook). Everything else is `100644`.
        let exec = meta.permissions().mode() & 0o111 != 0;
        set_mode(root, if exec { 0o755 } else { 0o644 })?;
    }
    Ok(())
}

/// Staged directories whose files the image keeps readable by root alone, which no mode
/// git records can express. netplan warns on every run about a configuration file
/// anyone else can read, since one may contain a Wi-Fi passphrase.
const ROOT_ONLY_DIRS: &[&str] = &["etc/netplan"];

/// Narrow every regular file under a [`ROOT_ONLY_DIRS`] entry of `staging` to `0600`.
/// Run after [`normalize_overlay_modes`], which gives every file the mode git recorded
/// for it.
///
/// Only files the staging tree itself contains are narrowed. The walk starts only when
/// every component of the entry is a real directory in the tree, and descends only into
/// real directories. A layer's symlink there names a directory on the target, so
/// following it on the build host would re-mode host files with the builder's
/// permissions.
fn restrict_root_only_files(staging: &Path) -> Result<(), EngineError> {
    fn walk(dir: &Path) -> Result<(), EngineError> {
        let entries = std::fs::read_dir(dir).map_err(|s| EngineError::io(dir, s))?;
        for entry in entries {
            let path = entry.map_err(|s| EngineError::io(dir, s))?.path();
            let meta = std::fs::symlink_metadata(&path).map_err(|s| EngineError::io(&path, s))?;
            if meta.is_dir() {
                walk(&path)?;
            } else if meta.is_file() {
                set_mode(&path, 0o600)?;
            }
        }
        Ok(())
    }
    for rel in ROOT_ONLY_DIRS {
        let mut dir = staging.to_path_buf();
        // `symlink_metadata` describes a link itself, which is never `is_dir`. A missing
        // component means no layer staged this directory, which is the common case.
        let real = Path::new(rel).components().all(|component| {
            dir.push(component);
            std::fs::symlink_metadata(&dir).is_ok_and(|meta| meta.is_dir())
        });
        if real {
            walk(&dir)?;
        }
    }
    Ok(())
}

/// Force every modification time in a staged overlay tree to `epoch`, so the build
/// host's *checkout* time cannot reach the rootfs any more than its umask can
/// ([`normalize_overlay_modes`] is the same pass for the same reason).
///
/// A layer's `overlay/` files are merged with `cp -a`, which preserves their times, and
/// those times say when this working copy was cloned or last edited — a property of the
/// machine, not of the build. They matter because the tree is laid into the rootfs by
/// [`provision::CopyIn`](ferroday_cage::provision::CopyIn), which *does* preserve times,
/// and the tar export's clamp is `min(mtime, SOURCE_DATE_EPOCH)` — so a checkout older
/// than the epoch would ship its own mtime and two hosts would export different bytes.
/// Stamping the epoch here makes the clamp a no-op and the tree a function of the lock.
///
/// Applied to symlinks too — `tar` records a symlink's own mtime — which is why this
/// does not go through `std::fs`: `filetime`-style helpers there follow the link and
/// would stamp its target instead.
fn normalize_overlay_times(root: &Path, epoch: u64) -> Result<(), EngineError> {
    let secs = i64::try_from(epoch).unwrap_or(i64::MAX);
    stamp_times(root, secs)
}

/// Recursive worker for [`normalize_overlay_times`]: stamp `path`, then descend into it
/// when it is a real directory (never through a symlink, which would leave the tree and
/// stamp whatever it points at).
fn stamp_times(path: &Path, secs: i64) -> Result<(), EngineError> {
    let meta = std::fs::symlink_metadata(path).map_err(|s| EngineError::io(path, s))?;
    if meta.is_dir() && !meta.file_type().is_symlink() {
        for entry in std::fs::read_dir(path).map_err(|s| EngineError::io(path, s))? {
            let entry = entry.map_err(|s| EngineError::io(path, s))?;
            stamp_times(&entry.path(), secs)?;
        }
    }
    // The directory itself is stamped after its children, whose creation would
    // otherwise move its mtime back to now.
    set_times(path, secs)
}

/// Set one path's access and modification times to `secs`, without following a symlink.
///
/// `utimensat` with `AT_SYMLINK_NOFOLLOW` stamps a symlink itself. Anything that opens
/// the path first stamps the link's target, which can lie outside the tree.
fn set_times(path: &Path, secs: i64) -> Result<(), EngineError> {
    use rustix::fs::{AtFlags, Timespec, Timestamps, CWD};
    let at = Timespec {
        tv_sec: secs,
        tv_nsec: 0,
    };
    let times = Timestamps {
        last_access: at,
        last_modification: at,
    };
    rustix::fs::utimensat(CWD, path, &times, AtFlags::SYMLINK_NOFOLLOW)
        .map_err(|errno| EngineError::io(path, errno.into()))
}

/// Merge the layer `overlay-pre/` trees into `staging` and write the boot method's
/// generated pre-install config into it — the tree the essential hook lays into the
/// rootfs **before any package is configured**.
///
/// Only config that a package's own maintainer scripts must *see while they run*
/// belongs here. On a depthcharge board that is `/etc/depthcharge-tools/config` and
/// `/etc/kernel/cmdline`, and the reason is not tidiness:
///
///  - `depthcharge-tools`' kernel hook re-signs and re-writes a ChromeOS kernel
///    partition. With no config present it runs at its defaults and goes looking for
///    that partition on **the build host's** block devices. `enable-system-hooks =
///    False` must therefore be in place before the package is.
///  - `/etc/kernel/cmdline` — not `kernel-cmdline` in the depthcharge config, which is
///    read through a `ConfigParser` whose interpolation rejects a raw `%`. This file is
///    read with a plain `read_text()` + `shlex.split()`, so it is the one place a
///    cmdline can safely live.
///
/// The localization config is here for the same reason, and it is the difference
/// between an image that merely *has* a locale and one that can be *re-configured* to
/// another. Debian's `locales`, `keyboard-configuration`, and `tzdata` all seed their
/// debconf answers from these exact files when they configure: `locales` reads
/// `/etc/locale.gen` (and `/etc/locale.conf` for the current `LANG`) and runs
/// `locale-gen` over what it finds; `keyboard-configuration` reads
/// `/etc/default/keyboard`; `tzdata` `readlink`s `/etc/localtime`. Laid in *first*,
/// they are adopted — the installed files, the debconf database, and the `console-setup`
/// cached keymap all agree, so `dpkg-reconfigure` on the running board opens on the
/// image's real values. Written *after* the packages instead, the files would be right
/// and debconf would still hold Debian's defaults.
fn stage_preinstall_overlay(
    staging: &Path,
    overlay_dirs: &[PathBuf],
    image: &ResolvedImage,
    boot: Option<BootConfig>,
    source_date_epoch: u64,
    step: &Step,
) -> Result<(), EngineError> {
    copy_overlay_trees(staging, overlay_dirs, step)?;
    if let Some(BootConfig::Depthcharge {
        board,
        cmdline,
        initramfs_compress,
    }) = boot
    {
        write_staged(
            staging,
            "etc/depthcharge-tools/config",
            &config::depthcharge_config(board, false),
        )?;
        write_staged(staging, "etc/kernel/cmdline", &format!("{cmdline}\n"))?;
        write_staged(
            staging,
            "etc/initramfs-tools/conf.d/depthcharge.conf",
            &config::depthcharge_initramfs_conf(initramfs_compress),
        )?;
    }

    write_staged(
        staging,
        "etc/locale.conf",
        &config::locale_conf(&image.locale),
    )?;
    write_staged(
        staging,
        "etc/locale.gen",
        &config::locale_gen(&image.locales_generate),
    )?;
    // The zone is the symlink and nothing else — see `config::localtime_target`.
    symlink_staged(
        staging,
        "etc/localtime",
        &config::localtime_target(&image.timezone),
    )?;
    // Only a board with a keyboard writes one. On a headless board the file is absent
    // and `keyboard-configuration` installs its own default, which is the honest
    // outcome: nothing here knows better than Debian what layout an unused console has.
    if let Some(keymap) = &image.keymap {
        write_staged(staging, "etc/default/keyboard", &config::keyboard(keymap))?;
    }
    // Same rule as the keymap: configure nothing when nothing is configured. An empty
    // list is not "no time source" — it is the distribution's own servers, and writing
    // an empty `NTP=` would say the opposite.
    if !image.ntp_servers.is_empty() {
        write_staged(
            staging,
            ntp_conf_path(image.time_client),
            &config::ntp_conf(image.time_client, &image.ntp_servers),
        )?;
    }

    // Only a build that names modules writes the list. With none, the generator
    // selects modules on its own and the build holds it to nothing.
    let generator = image.initramfs_generator;
    if !image.initramfs_modules.is_empty() {
        write_staged(
            staging,
            initramfs_modules_drop_in(generator),
            &config::initramfs_modules(generator, &image.initramfs_modules),
        )?;
    }

    // The placeholders that keep the install from building an initrd it cannot
    // build correctly — see `initramfs_stubs`. Executable, and the only staged files
    // that are.
    for stub in initramfs_stubs(generator) {
        write_staged_exec(
            staging,
            stub.trim_start_matches('/'),
            &config::initramfs_stub(generator, stub),
        )?;
    }

    normalize_overlay_modes(staging)?;
    restrict_root_only_files(staging)?;
    normalize_overlay_times(staging, source_date_epoch)?;
    step.log(format!(
        "staged pre-install overlay at {} (laid in before any package)",
        staging.display()
    ));
    Ok(())
}

/// Merge the layer `overlay/` trees into `staging` (each existing dir, in order,
/// via `cp -a` so symlinks and the executable bit survive), then write the
/// *generated* config into it and normalize every mode and modification time
/// ([`normalize_overlay_modes`], [`normalize_overlay_times`]) so neither the build
/// host's umask nor its checkout time reaches the image. Later layers and the generated
/// config win.
fn stage_overlay(
    staging: &Path,
    overlay_dirs: &[PathBuf],
    ib: ImageBuild,
    partitions: Partitions,
    image_identity: &boot2deb_core::provenance::SystemIdentity,
    source_date_epoch: u64,
    step: &Step,
) -> Result<(), EngineError> {
    let ImageBuild { build, image } = ib;
    copy_overlay_trees(staging, overlay_dirs, step)?;
    // Generated config, mirroring rootfs paths.
    write_staged(staging, "etc/fstab", &config::fstab(ib, &partitions))?;
    // Only written when the build declares volumes: an image with none should not
    // ship a config file suggesting the mechanism is live.
    if !image.data_volumes.is_empty() {
        write_staged(
            staging,
            boot2deb_core::datavolume::CONFIG_PATH,
            &boot2deb_core::datavolume::render_config(&image.data_volumes),
        )?;
    }
    write_staged(staging, "etc/hostname", &config::hostname(&image.hostname))?;
    write_staged(staging, "etc/hosts", &config::hosts(&image.hostname))?;
    write_staged(
        staging,
        &format!("etc/apt/sources.list.d/{}.sources", build.distro.name),
        &config::apt_sources(&build.distro, &image.components),
    )?;
    write_staged(staging, "etc/kernel-img.conf", &config::kernel_img_conf())?;
    // A file that exists because the build host is not the board. The initramfs
    // generator — running here, against a disk that does not exist yet — otherwise
    // sizes the initrd to this host by probing it. See each generator for what goes
    // wrong.
    write_staged(
        staging,
        initramfs_conf_path(image.initramfs_generator),
        // From the image node's own declaration of what it formats, so the helper the
        // initramfs contains is the one the filesystem the image node writes needs.
        &config::initramfs_conf(image.initramfs_generator, crate::image::ROOTFS_FS_KIND),
    )?;
    // initramfs-tools installs the firmware of built-in drivers on its own. dracut needs
    // the module, or a built-in driver that probes from the initrd finds none.
    if image.initramfs_generator == InitramfsGenerator::Dracut {
        write_staged_exec(
            staging,
            DRACUT_BUILTIN_FIRMWARE_MODULE,
            DRACUT_BUILTIN_FIRMWARE,
        )?;
    }
    // Board boot params the kernel postinst.d/postrm.d hooks + mk_extlinux source,
    // so the boot-method overlay scripts stay board-agnostic (driven by the
    // device's kernel_dtb) rather than hardcoding one board's dtb name.
    write_staged(
        staging,
        "etc/boot2deb/board.conf",
        &config::board_conf(
            &build.kernel_dtb,
            &build.kernel_cmdline,
            image
                .loader
                .as_ref()
                .and_then(|l| l.efi())
                .map(|e| e.dtb.as_str()),
        ),
    )?;
    // The image's own account of what it is, beside the board config, for a tool that
    // reads this disk from outside — from other media, without mounting it, on a machine
    // that may not be this board. Everything in it but the depthcharge board profile is a
    // cross-check against what such a reader can already infer from the disk; the board
    // profile is the part it cannot.
    write_staged(
        staging,
        "etc/boot2deb/image.toml",
        &image_identity.to_toml_string()?,
    )?;
    // The image's expectations of itself, read on the device by
    // `boot2deb-selftest`: one derived file cross-checking the identity above
    // against the running system, then one file per config layer that declared
    // `[[expect]]` checks. Written here — pre-flattened by the build, which has
    // the TOML parser — so the on-device runner reads plain lines and nothing
    // richer.
    write_staged(
        staging,
        &format!("{}/identity.checks", boot2deb_core::expect::CHECKS_DIR),
        &config::selftest_identity(ib, image_identity),
    )?;
    for group in &image.expectations {
        write_staged(
            staging,
            &format!(
                "{}/{}",
                boot2deb_core::expect::CHECKS_DIR,
                group.file_name()
            ),
            &group.render(),
        )?;
    }
    // The boot-time selftest unit ships in the base overlay on every image;
    // the enable symlink is laid in only when the recipe asked for it, so a
    // disabled unit costs its bytes and nothing else. Same mechanism as the
    // first-boot unit's shipped symlink, generated instead of checked in
    // because it is conditional.
    if image.selftest_on_boot {
        symlink_staged(
            staging,
            "etc/systemd/system/multi-user.target.wants/boot2deb-selftest.service",
            "../boot2deb-selftest.service",
        )?;
    }
    // The bounded wait for the first clock synchronization. The customize step enables
    // the unit, once the package that ships it is installed.
    let wait = clock_wait(image.time_client);
    write_staged(
        staging,
        &format!("etc/systemd/system/{}.d/bounded.conf", wait.unit),
        &config::clock_wait_drop_in(&wait),
    )?;
    // Empty machine-id → systemd regenerates it on first boot.
    write_staged(staging, "etc/machine-id", "")?;
    // The sudoers drop-in is written by the customize-hook directly into the chroot,
    // not staged here: at mode 0440 it would be unreadable to the hook's mapped uid
    // when it copies the overlay in.
    normalize_overlay_modes(staging)?;
    restrict_root_only_files(staging)?;
    normalize_overlay_times(staging, source_date_epoch)?;
    step.log(format!(
        "staged overlay + generated config at {}",
        staging.display()
    ));
    Ok(())
}

/// Write `content` to `staging/<rel>` at mode `0644`, replacing a file or symlink a
/// layer placed there ([`place_staged`] states how the path is resolved). The explicit
/// mode keeps generated `/etc` config reproducible regardless of the build host's
/// umask (a `002` umask would otherwise leave it group-writable); the parent
/// directories it creates are covered by the staging tree's closing
/// [`normalize_overlay_modes`] pass, which the umask reaches the same way.
fn write_staged(staging: &Path, rel: &str, content: &str) -> Result<(), EngineError> {
    place_staged(
        staging,
        rel,
        StagedEntry::File {
            content,
            mode: 0o644,
        },
    )
}

/// [`write_staged`] for a file that must be executable — a hook or, in the
/// pre-install overlay, an [`initramfs_stubs`] placeholder. Mode `0755` rather
/// than `0644`, which `normalize_overlay_modes` then preserves.
fn write_staged_exec(staging: &Path, rel: &str, content: &str) -> Result<(), EngineError> {
    place_staged(
        staging,
        rel,
        StagedEntry::File {
            content,
            mode: 0o755,
        },
    )
}

/// Create `staging/<rel>` as a symlink to `target`, replacing a file or symlink a layer
/// placed there ([`place_staged`] states how the path is resolved).
///
/// The overlay copy preserves symlinks (`cp -a`), so the link — not a copy of what it
/// points at — is what goes into the rootfs; the cache key fingerprints it by its target
/// ([`crate::rootcache::dir_fingerprints`]), so re-pointing it rebuilds.
fn symlink_staged(staging: &Path, rel: &str, target: &str) -> Result<(), EngineError> {
    place_staged(staging, rel, StagedEntry::Symlink { target })
}

/// What [`place_staged`] creates at a generated path.
enum StagedEntry<'a> {
    /// A regular file containing `content`, at `mode`.
    File {
        /// The file's bytes.
        content: &'a str,
        /// The file's permission bits, applied whatever the umask.
        mode: u32,
    },
    /// A symlink to `target`, a path on the target system. It is stored, never
    /// resolved on the build host.
    Symlink {
        /// The link's target.
        target: &'a str,
    },
}

/// Place a generated entry at `staging/<rel>`, resolving `rel` beneath `staging` one
/// component at a time without following a symlink at any of them.
///
/// The merged layer overlays keep their symlinks as symlinks ([`copy_overlay_trees`]),
/// and each one names a path on the *target*. Resolved on the build host instead, a
/// legitimate absolute link (`etc/hostname -> /run/hostname`, or a whole `etc`) leads
/// out of the staging tree, and writing through it would overwrite or re-mode a host
/// file with the builder's permissions. So:
///
///  - Each ancestor of `rel` is opened relative to its parent's descriptor with
///    `O_DIRECTORY | O_NOFOLLOW`, so no path is ever re-resolved from the top. A
///    missing one is created as a real directory. A symlink or any other non-directory
///    there is [`StagedPathBlocked`](EngineError::StagedPathBlocked). Following it is
///    what this exists to prevent, and replacing it would silently rewrite the layer's
///    tree under every other path that goes through it.
///  - The entry itself is replaced, since generated config wins over the layers. An
///    existing file or symlink is unlinked, which removes the name and never follows
///    it, and the new entry is created in its place. A file is opened `O_CREAT |
///    O_EXCL`, which refuses to open anything already at that name. A directory there
///    is [`StagedPathBlocked`](EngineError::StagedPathBlocked) too, since replacing it
///    would discard what the layers placed beneath it.
///
/// A file's mode is requested at creation and restated with `fchmod` on the new
/// descriptor, so the umask cannot narrow it and no path is resolved again to apply it.
///
/// `rel` is a relative path of plain components. One with a root, `.`, or `..`
/// component is refused before anything is touched.
fn place_staged(staging: &Path, rel: &str, entry: StagedEntry) -> Result<(), EngineError> {
    use crate::error::StagedConflict;
    use rustix::fd::OwnedFd;
    use rustix::fs::{AtFlags, FileType, Mode, OFlags};
    use rustix::io::Errno;
    use std::ffi::OsStr;
    use std::path::Component;

    let names: Option<Vec<&OsStr>> = Path::new(rel)
        .components()
        .map(|component| match component {
            Component::Normal(name) => Some(name),
            _ => None,
        })
        .collect();
    let Some((name, ancestors)) = names.as_deref().and_then(<[_]>::split_last) else {
        return Err(EngineError::io(
            &staging.join(rel),
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "a generated path is relative and has no root, `.`, or `..` component",
            ),
        ));
    };

    let failed = |prefix: &Path, errno: Errno| EngineError::io(&staging.join(prefix), errno.into());
    let blocked = |prefix: &Path, conflict| EngineError::StagedPathBlocked {
        generated: rel.to_string(),
        entry: prefix.display().to_string(),
        conflict,
    };
    // What is at `name` in `dir`, without following it; `None` when nothing is.
    let kind = |dir: &OwnedFd, name: &OsStr, prefix: &Path| match rustix::fs::statat(
        dir,
        name,
        AtFlags::SYMLINK_NOFOLLOW,
    ) {
        Ok(stat) => Ok(Some(FileType::from_raw_mode(stat.st_mode))),
        Err(Errno::NOENT) => Ok(None),
        Err(errno) => Err(failed(prefix, errno)),
    };

    // The staging root is the builder's own directory; only what lies beneath it came
    // from the layers.
    let root_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::CLOEXEC;
    let dir_flags = root_flags | OFlags::NOFOLLOW;
    let mut dir = rustix::fs::open(staging, root_flags, Mode::empty())
        .map_err(|errno| EngineError::io(staging, errno.into()))?;
    let mut prefix = PathBuf::new();
    for ancestor in ancestors {
        prefix.push(ancestor);
        match kind(&dir, ancestor, &prefix)? {
            Some(FileType::Directory) => {}
            Some(FileType::Symlink) => {
                return Err(blocked(&prefix, StagedConflict::SymlinkAncestor));
            }
            Some(_) => return Err(blocked(&prefix, StagedConflict::NonDirectoryAncestor)),
            None => rustix::fs::mkdirat(&dir, *ancestor, Mode::from_raw_mode(0o755))
                .map_err(|errno| failed(&prefix, errno))?,
        }
        // `O_NOFOLLOW` is what holds the line; the check above names the conflict.
        dir = rustix::fs::openat(&dir, *ancestor, dir_flags, Mode::empty())
            .map_err(|errno| failed(&prefix, errno))?;
    }

    prefix.push(name);
    match kind(&dir, name, &prefix)? {
        Some(FileType::Directory) => return Err(blocked(&prefix, StagedConflict::Directory)),
        Some(_) => rustix::fs::unlinkat(&dir, *name, AtFlags::empty())
            .map_err(|errno| failed(&prefix, errno))?,
        None => {}
    }
    match entry {
        StagedEntry::File { content, mode } => {
            use std::io::Write;
            let mode = Mode::from_raw_mode(mode);
            let create =
                OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
            let fd = rustix::fs::openat(&dir, *name, create, mode)
                .map_err(|errno| failed(&prefix, errno))?;
            rustix::fs::fchmod(&fd, mode).map_err(|errno| failed(&prefix, errno))?;
            std::fs::File::from(fd)
                .write_all(content.as_bytes())
                .map_err(|s| EngineError::io(&staging.join(&prefix), s))
        }
        StagedEntry::Symlink { target } => {
            rustix::fs::symlinkat(target, &dir, *name).map_err(|errno| failed(&prefix, errno))
        }
    }
}

/// Set the unix mode of `path`.
fn set_mode(path: &Path, mode: u32) -> Result<(), EngineError> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|s| EngineError::io(path, s))
}

/// Where the pre-install overlay lays the placeholders that shadow `generator`'s
/// initramfs builders for the length of the package install.
///
/// `/usr/local/sbin` is first on the `PATH` every maintainer script runs with,
/// so these are what the kernel package's `postinst.d` hook and the generator's
/// trigger reach — and neither of them can build a usable initrd at the point they
/// run. The overlay this build ships is laid in *after* the packages, so an initrd
/// built during the install has neither the image's initramfs configuration nor the
/// `modules.dep` the `depmod` hook writes; the rootfs stage discards it and builds
/// the real one afterwards, in
/// [`customize_script`](crate::rootfs::provisioner). Suppressing them costs
/// nothing and saves every install-time initramfs build, which on a cross host runs
/// under `qemu-user` like everything else a maintainer script invokes.
///
/// Which programs that takes:
///
///  - `initramfs-tools`: `update-initramfs`, which its kernel hook and its trigger
///    both call.
///  - `dracut`: `dracut`, which its kernel hook calls by name, and `update-initramfs`,
///    the shim the package ships for its own trigger and for other packages' calls.
///    The shim reaches `dracut` by `PATH` as well, but it is shadowed in its own
///    right: once `dracut` succeeds it `sync`s the initrd `dracut` was to write, and
///    a placeholder writes none.
///
/// Removed by the same customize step that builds the real initrd, so none reaches
/// the image — and each is a file in the staging tree only, not a `dpkg-divert`, so a
/// build that dies before that leaves nothing to undo: the staging tree is discarded,
/// and no cached artifact ever holds one.
pub(crate) fn initramfs_stubs(generator: InitramfsGenerator) -> &'static [&'static str] {
    match generator {
        InitramfsGenerator::InitramfsTools => &["/usr/local/sbin/update-initramfs"],
        InitramfsGenerator::Dracut => {
            &["/usr/local/sbin/dracut", "/usr/local/sbin/update-initramfs"]
        }
    }
}

/// Where the placeholders record the calls they absorbed, so the build can report
/// how many initramfs builds it suppressed rather than claiming a saving it did
/// not make. Deleted with the placeholders.
pub(crate) const INITRAMFS_STUB_LOG: &str = "/var/lib/boot2deb/initramfs-stub.log";

/// Where the pre-install overlay writes the image's initramfs module list
/// ([`ResolvedImage::initramfs_modules`]) for `generator`, in the form
/// [`config::initramfs_modules`] renders.
///
/// Both are files no package owns, so dpkg has no conffile to prompt about, and both
/// ship in the image, so an on-device kernel upgrade regenerates the initrd from the
/// same list:
///
///  - `initramfs-tools`: a `modules.d` drop-in under `/usr/share` rather than
///    `/etc/initramfs-tools/modules`, which is a conffile of the package. `mkinitramfs`
///    reads every file in this directory whatever `MODULES=` says.
///  - `dracut`: a drop-in in the distribution's `dracut.conf.d`, beside the one
///    [`initramfs_conf_path`] names. An administrator's file of the same name under
///    `/etc/dracut.conf.d` replaces it.
pub(crate) fn initramfs_modules_drop_in(generator: InitramfsGenerator) -> &'static str {
    match generator {
        InitramfsGenerator::InitramfsTools => "usr/share/initramfs-tools/modules.d/boot2deb",
        InitramfsGenerator::Dracut => "usr/lib/dracut/dracut.conf.d/90-boot2deb-modules.conf",
    }
}

/// Where the overlay writes `generator`'s build settings, [`config::initramfs_conf`].
///
/// A drop-in either way. `initramfs-tools` reads `conf.d/*` unconditionally, and
/// `/etc/initramfs-tools/initramfs.conf` is a conffile. For dracut, its distribution
/// directory and `/etc/dracut.conf.d` are read together, ordered by file name, and a
/// file of the same name under `/etc` replaces this one.
pub(crate) fn initramfs_conf_path(generator: InitramfsGenerator) -> &'static str {
    match generator {
        InitramfsGenerator::InitramfsTools => "etc/initramfs-tools/conf.d/boot2deb.conf",
        InitramfsGenerator::Dracut => "usr/lib/dracut/dracut.conf.d/90-boot2deb.conf",
    }
}

/// Where a dracut image's overlay installs [`DRACUT_BUILTIN_FIRMWARE`], the dracut
/// module that puts the firmware of the kernel's built-in drivers into the initrd.
///
/// dracut installs a driver's firmware with the driver's module, and a built-in driver
/// has no module, so an initrd would contain no firmware for one. A built-in driver
/// probes from the initrd once what it waits on is there, and reads its firmware from
/// it: the RK3588's panthor does, after the PMIC modules the initrd loads bring up its
/// regulator. `initramfs-tools` installs these files itself (`add_builtin_firmware`),
/// so an image it builds the initrd for needs nothing.
///
/// In the distribution's `modules.d` beside dracut's own modules. No package owns the
/// file, and it ships in the image, so an on-device kernel upgrade builds the next
/// initrd with it.
pub(crate) const DRACUT_BUILTIN_FIRMWARE_MODULE: &str =
    "usr/lib/dracut/modules.d/90boot2deb-builtin-firmware/module-setup.sh";

/// The dracut module [`DRACUT_BUILTIN_FIRMWARE_MODULE`] names: one committed bash file,
/// byte-identical for every build, which dracut sources. It reads the kernel's
/// `modules.builtin.modinfo` and installs each listed file in the first spelling the
/// kernel's loader opens.
const DRACUT_BUILTIN_FIRMWARE: &str = include_str!("dracut/builtin-firmware.sh");

/// Where the pre-install overlay writes the image's configured NTP servers
/// ([`ResolvedImage::ntp_servers`]) for `client`, in the form [`config::ntp_conf`]
/// renders.
///
/// A file beside the distribution's own configuration either way, never an edit of it:
///
///  - `systemd-timesyncd`: a `timesyncd.conf.d` drop-in. `timesyncd.conf` is a
///    `systemd` conffile, and rewriting it would make every later systemd upgrade
///    prompt about a modified conffile on a running board.
///  - `chrony`: a file in `/etc/chrony/sources.d`, which `chrony.conf` reads through
///    `sourcedir` beside the distribution's own sources. `chronyc reload sources`
///    applies an edit to it without a restart.
pub(crate) fn ntp_conf_path(client: TimeClient) -> &'static str {
    match client {
        TimeClient::SystemdTimesyncd => "etc/systemd/timesyncd.conf.d/10-boot2deb.conf",
        TimeClient::Chrony => "etc/chrony/sources.d/boot2deb.sources",
    }
}

/// The unit that holds `time-sync.target` until the time client has synchronized the
/// clock, and the bounded command the image runs it with. See [`clock_wait`].
pub(crate) struct ClockWait {
    /// The unit's name, which is also the stem of its drop-in directory.
    pub unit: &'static str,
    /// The target whose `.wants` directory enabling the unit links it into, as the
    /// unit's own `[Install]` section names it.
    pub wanted_by: &'static str,
    /// The command the drop-in runs in place of the unit's own. Its leading `-` makes
    /// systemd record a failure without failing the unit.
    pub exec_start: &'static str,
    /// The program [`exec_start`](Self::exec_start) runs. The customize step refuses an
    /// image without it, since the drop-in would replace the unit's command with one
    /// that cannot run.
    pub program: &'static str,
}

/// The [`ClockWait`] for `client`.
///
/// The maintenance jobs of both distributions (apt-daily, logrotate, man-db, fstrim,
/// e2scrub_all, dpkg-db-backup) order themselves `After=time-sync.target`. Nothing
/// reaches that target until one of these units is enabled, and neither distribution
/// enables one, so on a stock image the ordering is inert and the jobs check dates
/// against whatever the clock says at boot. A board with no RTC boots with a stale clock,
/// and apt then rejects Release files and TLS certificates as not valid yet.
///
/// The image enables the unit, and bounds it. Stock, `systemd-time-wait-sync` ships
/// `TimeoutStartSec=infinity`, and `chrony-wait` waits three minutes and then fails. On
/// a board that never reaches a time server that holds back everything ordered after
/// the target, and Debian's `anacron.service` is both `After=time-sync.target` and
/// `Before=multi-user.target`, so it holds back `multi-user.target` too. Logins still
/// work, which makes the failure quiet. The bounded command releases the target after
/// 45 seconds and succeeds either way: ordering is satisfied when a job finishes rather
/// than when it succeeds, and a failed unit would leave `systemctl is-system-running` at
/// `degraded` on every offline boot.
///
///  - `systemd-timesyncd`: `systemd-time-wait-sync` under `timeout(1)`.
///  - `chrony`: `chronyc waitsync` with its own bound, 45 tries one second apart and the
///    stock unit's correction limit. The stock unit runs `chronyc` under a system-call
///    filter written for it, so the bounded command keeps that program and changes only
///    its try count.
pub(crate) fn clock_wait(client: TimeClient) -> ClockWait {
    match client {
        TimeClient::SystemdTimesyncd => ClockWait {
            unit: "systemd-time-wait-sync.service",
            wanted_by: "sysinit.target",
            exec_start: "-/usr/bin/timeout 45 /usr/lib/systemd/systemd-time-wait-sync",
            program: "/usr/bin/timeout",
        },
        TimeClient::Chrony => ClockWait {
            unit: "chrony-wait.service",
            wanted_by: "multi-user.target",
            exec_start: "-/usr/bin/chronyc -h 127.0.0.1,::1 waitsync 45 0.1 0.0 1",
            program: "/usr/bin/chronyc",
        },
    }
}

/// Members every finished rootfs tarball must contain, checked by [`validate_tar`].
/// `etc/shadow` is written late (the account is created there), so its presence
/// proves the archive is readable through to its end (not truncated);
/// `etc/os-release` and `etc/fstab` confirm the archive is a real Debian rootfs
/// rather than an unrelated or empty tar. Named without a leading `./` and matched
/// against a listing normalized the same way, so the check states the paths as the
/// rootfs sees them rather than as one tar writer happens to spell them.
const REQUIRED_TAR_MEMBERS: &[&str] = &["etc/os-release", "etc/fstab", "etc/shadow"];

/// Validate that `tarball` is a complete, readable rootfs archive before the image
/// stage formats it into ext4. Lists the archive end-to-end (`tar tf`, which
/// errors on a truncated archive) and confirms every `REQUIRED_TAR_MEMBERS` entry is
/// present (including the still-locked `./etc/shadow`). A partial tar left by an
/// interrupted bootstrap fails here with a "re-run rootfs" pointer. It is not silently
/// formatted into a broken image by an `--stage image` retry whose gate only checked
/// that the file existed.
pub fn validate_tar(tarball: &Path) -> Result<(), EngineError> {
    let out = Command::new("tar")
        .arg("-tf")
        .arg(tarball)
        .output()
        .map_err(|source| EngineError::CommandSpawn {
            command: "tar".into(),
            context: "validate rootfs tarball".into(),
            source,
        })?;
    if !out.status.success() {
        return Err(EngineError::CommandFailed {
            command: "tar".into(),
            context: format!("validate rootfs tarball {}", tarball.display()),
            status: out.status.code(),
            stderr: String::from_utf8_lossy(&out.stderr).trim().to_string(),
        });
    }
    let listing = String::from_utf8_lossy(&out.stdout);
    // The export writes `./`-prefixed members (`./etc/os-release`); strip that prefix
    // so the comparison is against the path the rootfs sees, whatever the archive's
    // spelling.
    let members: std::collections::HashSet<&str> = listing
        .lines()
        .map(|l| l.trim().trim_start_matches("./"))
        .collect();
    for required in REQUIRED_TAR_MEMBERS {
        if !members.contains(required) {
            return Err(EngineError::ArtifactMissing {
                what: format!("member {required} in rootfs tarball (incomplete tar?)"),
                location: tarball.display().to_string(),
            });
        }
    }
    Ok(())
}

/// Generated rootfs config files, rendered from resolved values. Pure —
/// each returns the exact file content — so the config is unit-testable.
mod config {
    use boot2deb_core::distro::{InitramfsGenerator, ResolvedDistro, TimeClient};
    use boot2deb_core::model::{
        ImageBuild, InitramfsCompress, Keymap, Loader, ResolvedBoot, ResolvedLoader,
        CONSOLE_LOGLEVEL_ARG,
    };
    use std::fmt::Write;

    /// `/etc/fstab`. Every image roots on the **PARTUUID** the image node stamps
    /// into the GPT ([`ImageIdentity::rootfs_partuuid`](crate::image::ImageIdentity)),
    /// derived up front and threaded through both nodes so this entry always names the
    /// partition the image will actually write. The value is the device's boot
    /// identity for its whole life: nothing on the device rewrites it (first-boot
    /// grows the partition but keeps its PARTUUID), so the references that contain it —
    /// this file, the loader's menu, the signed depthcharge kernel — never go
    /// stale. A press that draws a fresh identity rewrites each of them in the image
    /// file, before any medium contains it ([`press::identity`](crate::press::identity)).
    /// The ext4 label still exists (it is handy for rescue mounts) but nothing boots by
    /// it.
    ///
    /// Per-boot notes:
    ///
    ///  - `extlinux`: the `mk_extlinux` hook reads root back out of here to build the
    ///    bootloader's cmdline on every kernel install.
    ///
    ///  - `efi`: a second entry mounts the EFI system partition at
    ///    [`ESP_MOUNT_POINT`](boot2deb_core::model::ESP_MOUNT_POINT), by its PARTUUID,
    ///    where `grub-install` and the DTB hook write. `umask=0077` keeps the loader
    ///    root's alone, as Debian's installer mounts it, and pass 2 checks it at boot
    ///    with the `fsck.vfat` the loader's layer installs. `nofail`, because the running
    ///    system does not need the partition: a board whose ESP will not mount still
    ///    boots to the network, where the failed mount, the selftest and the next
    ///    kernel install each report it, rather than stopping in emergency mode.
    ///
    ///  - `depthcharge`: the boot itself depends on this file —
    ///    `depthchargectl` reads it to derive the `root=` it bakes into the kernel's
    ///    *signature*, at image build and again on every on-device kernel upgrade. A
    ///    label would work for the kernel but not for the tool: it resolves root from
    ///    fstab and strips any `root=` that disagrees, so whatever is here is what
    ///    gets signed.
    pub fn fstab(ib: ImageBuild, partitions: &super::Partitions) -> String {
        // One root entry everywhere, because every image roots by PARTUUID. Only the
        // note differs: it tells an editor what else has to be regenerated, and each
        // boot keeps that value in a different place.
        let source = format!("PARTUUID={}", partitions.rootfs_partuuid.hyphenated());
        let loader = ib.image.loader.as_ref().map(|l| l.loader());
        let note = match (&ib.build.boot, loader) {
            (ResolvedBoot::Depthcharge(_), _) => {
                "# depthchargectl derives the signed kernel's root= from this line; after\n\
                 # editing it, re-sign with `depthchargectl write --allow-current`"
            }
            (_, Some(Loader::Efi)) => {
                "# update-grub writes the root= of /boot/grub/grub.cfg from the mounted root;\n\
                 # run it after moving root"
            }
            _ => "# regenerate extlinux.conf via /boot/mk_extlinux after editing the root entry",
        };
        let esp: String = partitions
            .esp_partuuid
            .map(|esp| {
                format!(
                    "PARTUUID={}\t{}\tvfat\tumask=0077,nofail\t0      2\n",
                    esp.hyphenated(),
                    boot2deb_core::model::ESP_MOUNT_POINT
                )
            })
            .unwrap_or_default();
        // Data volumes follow root, mounted by LABEL and with `nofail`, so a board
        // whose data disk is absent or slow to enumerate still boots. The
        // first-boot hook creates or adopts the volume these lines name; the lines
        // themselves are here at build time so the mount survives every later boot
        // without the hook, which runs exactly once.
        let data: String = ib
            .image
            .data_volumes
            .iter()
            .map(|v| format!("{}\n", v.fstab_line()))
            .collect();
        format!(
            "{note}\n\
             # <device>\t<mount>\t<type>\t<options>\t\t<dump> <pass>\n\
             {source}\t/\text4\terrors=remount-ro\t0      1\n\
             {esp}\
             {data}"
        )
    }

    /// The placeholder the pre-install overlay lays at `path`, one of
    /// [`initramfs_stubs`](super::initramfs_stubs) for `generator`.
    ///
    /// It answers `--version` with the generator's name on purpose: initramfs-tools'
    /// `postinst.d` hook checks that `update-initramfs` names `initramfs-tools` before
    /// calling it, and a placeholder that failed the check would be skipped silently
    /// instead of taking the call and recording it. Every other invocation is a no-op
    /// that appends the program's name and its arguments to
    /// [`INITRAMFS_STUB_LOG`](super::INITRAMFS_STUB_LOG), which is what lets the
    /// customize step report the suppression as a number rather than an assumption.
    pub fn initramfs_stub(generator: InitramfsGenerator, path: &str) -> String {
        let program = path.rsplit('/').next().unwrap_or(path);
        format!(
            "#!/bin/sh\n\
             # Placeholder for {generator}'s {program}, in place only while\n\
             # boot2deb's package install runs. An initrd built here would predate\n\
             # this image's initramfs configuration and the modules.dep the depmod\n\
             # hook writes, so the build discards these calls and builds the real\n\
             # initrd afterwards. Removed by the customize step that does.\n\
             case \"$1\" in\n\
             \x20 --version) echo '{generator} boot2deb-placeholder'; exit 0 ;;\n\
             esac\n\
             mkdir -p \"$(dirname '{log}')\"\n\
             echo \"{program} $*\" >> '{log}'\n\
             exit 0\n",
            log = super::INITRAMFS_STUB_LOG,
        )
    }

    /// `/etc/boot2deb/selftest.d/identity.checks` — the checks derived from the
    /// build itself, as opposed to the authored `[[expect]]` files written beside
    /// it. Derived rather than authored because each restates something the lock
    /// or the device layer already defines, and a layer restating it could drift:
    ///
    ///  - `kernel-release` — from the pinned kernel ref, for a compiled kernel
    ///    whose ref reads as a version tag, normalized to the release the kernel
    ///    itself reports (`v7.1.6` → `7.1.6`, `v7.2` → `7.2.0`) and matched as a
    ///    prefix of `uname -r`. A distro kernel's version belongs to the suite,
    ///    and a vendor branch or bare commit names no version — neither emits
    ///    the line.
    ///  - `kernel-flavor` — the `uname -r` suffix: the arch tail `bindeb-pkg`
    ///    bakes into a compiled kernel's `LOCALVERSION`, or the distro package's
    ///    flavor (`linux-image-armmp` → `armmp`). The runner rejects an `-rt-`
    ///    variant of the same flavor — the check that catches an accidental
    ///    PREEMPT_RT kernel.
    ///  - `dtb` — the resolved `kernel_dtb`, under the `extlinux` loader, which reads
    ///    the DTB from `/boot`. A depthcharge board's DTB is inside the signed FIT,
    ///    where no file check can see it.
    ///  - `esp-file` — under the `efi` loader, the two things the firmware reads from
    ///    the EFI system partition: the architecture's loader at the removable-media
    ///    path, and the kernel's DTB under `dtb/`, at the name u-boot loads by. The build has already
    ///    refused an image missing either; on the board the lines re-check them after
    ///    every GRUB upgrade and kernel install has rewritten the partition.
    ///  - `single-kernel` — unconditional, and the one line here that restates
    ///    nothing: every image installs one solved plan with one `linux-image`
    ///    in it and never dist-upgrades mid-build, so a second version on `/boot`
    ///    means a feature or a `--deb` addition pulled one in. It is a check
    ///    rather than a sweep because losing a kernel is worse than shipping two,
    ///    so the build fails and says which versions it found.
    ///  - `unique-partuuid` — unconditional, and also restating nothing: every
    ///    PARTUUID the running system mounts by names one attached partition. It
    ///    fails when a second disk written from the same image is attached, which
    ///    `boot2deb-partuuid-check.service` also reports on every boot.
    ///  - `initramfs-module` — one per name in the resolved
    ///    [`initramfs_modules`](boot2deb_core::model::ResolvedImage::initramfs_modules).
    ///    The build already checked each against the initrd it built. The line is
    ///    that same check re-run on the device, where an on-device kernel upgrade
    ///    regenerates the initrd with no build to check it.
    pub fn selftest_identity(
        ib: ImageBuild,
        identity: &boot2deb_core::provenance::SystemIdentity,
    ) -> String {
        let ImageBuild { build, image } = ib;
        let mut out = String::from(
            "# Checks derived from this image's identity (etc/boot2deb/image.toml).\n\
             # Run by boot2deb-selftest; see the boot2deb manual (reference/self-test).\n",
        );
        if let Some(release) = identity
            .kernel
            .reference
            .as_deref()
            .and_then(kernel_release_from_ref)
        {
            let _ = writeln!(
                out,
                "{}",
                boot2deb_core::expect::render_line("kernel-release", &release)
            );
        }
        if let Some(flavor) = kernel_flavor(identity) {
            let _ = writeln!(
                out,
                "{}",
                boot2deb_core::expect::render_line("kernel-flavor", &flavor)
            );
        }
        match &image.loader {
            Some(ResolvedLoader::Extlinux) => {
                let _ = writeln!(
                    out,
                    "{}",
                    boot2deb_core::expect::render_line("dtb", &build.kernel_dtb)
                );
            }
            Some(ResolvedLoader::Efi(efi)) => {
                for path in [
                    format!("/EFI/BOOT/{}", efi.arch.removable_file),
                    format!("/dtb/{}", efi.dtb),
                ] {
                    let _ = writeln!(
                        out,
                        "{}",
                        boot2deb_core::expect::render_line("esp-file", &path)
                    );
                }
            }
            None => {}
        }
        for kind in ["single-kernel", "unique-partuuid"] {
            let _ = writeln!(out, "{}", boot2deb_core::expect::render_line(kind, ""));
        }
        for module in &image.initramfs_modules {
            let _ = writeln!(
                out,
                "{}",
                boot2deb_core::expect::render_line("initramfs-module", module)
            );
        }
        out
    }

    /// The [`initramfs_modules_drop_in`](super::initramfs_modules_drop_in) content for
    /// `generator`: a header, then one module per line, in resolved order.
    ///
    /// For initramfs-tools that is the bare name. For dracut it is a `force_drivers+=`
    /// line per name, its form of the same instruction: install the module and load it in the
    /// initramfs, as initramfs-tools loads every module its list names. The spaces
    /// around each name are dracut's own convention for appending to a list, and
    /// resolution holds a name to letters, digits, `-` and `_`, so none can end the
    /// quoted string.
    ///
    /// The reason each module is listed lives beside it in the layer's TOML, which is
    /// where the next editor looks. This file names its source instead.
    pub fn initramfs_modules(generator: InitramfsGenerator, modules: &[String]) -> String {
        let mut out = String::from(
            "# Generated by boot2deb from the `[initramfs] modules` lists of this image's\n\
             # SoC and device layers. Every name here must reach the initramfs: the build\n\
             # fails when one is neither in the built initrd nor built into the kernel.\n",
        );
        for module in modules {
            let _ = match generator {
                InitramfsGenerator::InitramfsTools => writeln!(out, "{module}"),
                InitramfsGenerator::Dracut => writeln!(out, "force_drivers+=\" {module} \""),
            };
        }
        out
    }

    /// The kernel release a pinned ref names, spelled the way the built kernel
    /// will spell it — which is what the runner prefix-matches `uname -r`
    /// against.
    ///
    /// [`parse_tag`](boot2deb_core::version::parse_tag) is the only thing that
    /// decides what a tag means, and its normalization *is* the kernel's own:
    /// upstream drops the patch component on a `.0` release (`v7.2`), and the
    /// Makefile's `SUBLEVEL` is 0 there, so the kernel reports `7.2.0`. A ref
    /// read literally would be compared against a release it can never equal.
    /// The same call handles the Linux-libre namespace (`sources/v7.1.6-gnu` →
    /// `7.1.6`, its `-gnu` kept in `uname -r`'s suffix) and preserves a
    /// prerelease (`v7.2-rc3` → `7.2.0-rc3`).
    ///
    /// A branch name or a bare commit parses as no version and yields no
    /// `kernel-release` check — a prefix match against `uname -r` would be
    /// meaningless for either.
    pub(super) fn kernel_release_from_ref(reference: &str) -> Option<String> {
        boot2deb_core::version::parse_tag(reference)
            .ok()
            .map(|v| v.to_string())
    }

    /// The `uname -r` flavor suffix this image's kernel boots with: the arch
    /// tail of a compiled kernel's `LOCALVERSION`
    /// ([`localversion`](crate::build::kernel)), or a distro package's flavor.
    fn kernel_flavor(identity: &boot2deb_core::provenance::SystemIdentity) -> Option<String> {
        match identity.kernel.package.as_deref() {
            // `linux-image-armmp` → `armmp`; a package spelled without the
            // conventional prefix names no derivable flavor.
            Some(package) => package.strip_prefix("linux-image-").map(str::to_string),
            // Compiled: `LOCALVERSION=-<rev>-<arch>` makes the arch the suffix.
            None => Some(identity.image.arch.clone()),
        }
    }

    /// `/etc/depthcharge-tools/config` — which board profile to sign for, and whether
    /// the package's kernel hooks are live.
    ///
    /// `enable-system-hooks` is the safety switch, and it is written **False at build
    /// time** and flipped True at the end. The hook it gates runs `depthchargectl
    /// write`, which hunts the *host's* block devices for a ChromeOS kernel partition
    /// — harmless when there is none, and not harmless at all when the operator
    /// happens to have the target SD card plugged in while building.
    ///
    /// In the shipped image it must be `True`, or an `apt` kernel upgrade would leave
    /// the signed partition holding the old kernel while the filesystem moved on.
    pub fn depthcharge_config(board: &str, enable_system_hooks: bool) -> String {
        let enabled = if enable_system_hooks { "True" } else { "False" };
        format!(
            "# Generated by boot2deb.\n\
             [depthcharge-tools]\n\
             enable-system-hooks = {enabled}\n\
             \n\
             [depthchargectl]\n\
             board = {board}\n"
        )
    }

    /// `/etc/initramfs-tools/conf.d/depthcharge.conf` — the two initramfs settings a
    /// signed-payload board has to state outright.
    ///
    /// Generated rather than shipped as an overlay file because the compressor is not a
    /// free choice: it follows from the kernel slot's size
    /// ([`InitramfsCompress`](boot2deb_core::model::InitramfsCompress)), and a static
    /// file would make every board take the narrowest board's answer.
    ///
    /// A drop-in, not `/etc/initramfs-tools/initramfs.conf`, which is a conffile:
    /// writing that before the package unpacks makes dpkg prompt about a
    /// locally-modified file. `mkinitramfs` reads `conf.d/*` unconditionally, so a
    /// drop-in has the same effect and no packaging conflict.
    ///
    /// `MODULES=list`, not `dep`, for two reasons and the second is the sharp edge:
    ///
    ///  - Budget. A `MODULES=most` initramfs does not fit under the payload limit once
    ///    the kernel image has taken its share.
    ///  - Correctness under cross-build. `MODULES=dep` guesses from the *building*
    ///    system's mounted `/sys` and `/`, so in an armhf rootfs on an x86_64 host it
    ///    resolves the host's root device and produces an initramfs with none of the
    ///    drivers the board needs. It is only meaningful when built on the target; an
    ///    explicit list is deterministic and works in both places.
    ///
    /// The list itself is initramfs-tools'
    /// [`initramfs_modules_drop_in`](super::initramfs_modules_drop_in), generated from
    /// the `[initramfs] modules` of the board's SoC and device layers, so which modules
    /// a board includes stays with the board. Resolution refuses any other generator on
    /// a depthcharge board.
    pub fn depthcharge_initramfs_conf(compress: InitramfsCompress) -> String {
        format!(
            "# Generated by boot2deb.\n\
             MODULES=list\n\
             COMPRESS={}\n",
            compress.as_str()
        )
    }

    /// `/etc/hostname`.
    pub fn hostname(h: &str) -> String {
        format!("{h}\n")
    }

    /// `/etc/hosts` with the loopback + a `127.0.1.1` entry for the hostname.
    pub fn hosts(h: &str) -> String {
        format!(
            "127.0.0.1\tlocalhost\n127.0.1.1\t{h}\n\
             ::1\tlocalhost ip6-localhost ip6-loopback\nff02::1\tip6-allnodes\nff02::2\tip6-allrouters\n"
        )
    }

    /// The image's apt sources, in deb822 form: one stanza per archive the suite's
    /// pockets are served from, written to
    /// `/etc/apt/sources.list.d/<distro>.sources`.
    ///
    /// The pockets are the [distro layer's](boot2deb_core::distro::DistroLayer) for
    /// the suite, so a rolling suite offers no pocket its archive never serves. Pockets
    /// on one archive share a stanza, in the order the layer lists them, which is the
    /// layout a stock install of either distribution writes.
    ///
    /// `components` is the set this image was *provisioned* from, so what the booted
    /// board can install matches what the build could: a libre image, whose kernel
    /// loads no blob, does not offer nonfree firmware on the console either.
    ///
    /// Always the live mirror. A snapshot is a fetch-time concern of the build, and a
    /// device updates from the distribution's own archive whichever copy built it.
    /// `Signed-By` names the keyring the distribution's keyring package installs, so the
    /// board keeps verifying as the archive rotates keys.
    pub fn apt_sources(distro: &ResolvedDistro, components: &[String]) -> String {
        let mut archives: Vec<(&str, Vec<&str>)> = Vec::new();
        for pocket in &distro.pockets {
            match archives.iter_mut().find(|(uri, _)| *uri == pocket.mirror) {
                Some((_, suites)) => suites.push(&pocket.suite),
                None => archives.push((&pocket.mirror, vec![&pocket.suite])),
            }
        }
        archives
            .iter()
            .map(|(uri, suites)| {
                format!(
                    "Types: deb\nURIs: {uri}\nSuites: {}\nComponents: {}\nSigned-By: {}\n",
                    suites.join(" "),
                    components.join(" "),
                    distro.image_keyring
                )
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// `/etc/locale.conf` — the system `LANG`.
    ///
    /// **Not** `/etc/default/locale`, which Debian makes a symlink to this file and
    /// `systemd-tmpfiles` re-asserts with a *forcing* rule (`L+`) on every boot: a
    /// regular file written there is deleted and replaced by the symlink, so an image
    /// that put its `LANG` in `/etc/default/locale` would come up with no locale at
    /// all. Writing the symlink's target instead satisfies every reader — `pam_env`
    /// through the symlink, `systemd`/`localectl` directly, and the `locales` package,
    /// whose config script reads this exact path to learn the current default.
    pub fn locale_conf(locale: &str) -> String {
        format!("LANG={locale}\n")
    }

    /// `/etc/locale.gen` — one `<name> <codeset>` line per locale the image compiles.
    ///
    /// Laid in *before* `locales` installs, so its config script adopts these as the
    /// selected set and its postinst runs `locale-gen` over them as part of the normal
    /// install. The package then appends the full commented catalogue below them, which
    /// is what `dpkg-reconfigure locales` offers on the running board.
    pub fn locale_gen(locales: &[String]) -> String {
        let mut s = String::new();
        for locale in locales {
            // Resolution rejects a locale with no codeset, so this cannot be `None`.
            let codeset = boot2deb_core::locale_codeset(locale).unwrap_or("UTF-8");
            let _ = writeln!(s, "{locale} {codeset}");
        }
        s
    }

    /// `/etc/default/keyboard` — the console keyboard layout.
    ///
    /// Laid in before `keyboard-configuration` installs, which is what makes it stick:
    /// that package's config script seeds its debconf answers from this file, so the
    /// installed system, its debconf state, and the `console-setup` cached keymap all
    /// agree — and a later `dpkg-reconfigure keyboard-configuration` opens on the right
    /// current values rather than Debian's defaults.
    pub fn keyboard(keymap: &Keymap) -> String {
        format!(
            "# Generated by boot2deb. Consult the keyboard(5) manual page.\n\
             XKBMODEL=\"{model}\"\n\
             XKBLAYOUT=\"{layout}\"\n\
             XKBVARIANT=\"{variant}\"\n\
             XKBOPTIONS=\"{options}\"\n\
             \n\
             BACKSPACE=\"guess\"\n",
            model = keymap.model,
            layout = keymap.layout,
            variant = keymap.variant,
            options = keymap.options,
        )
    }

    /// The `/etc/localtime` symlink target for `timezone`.
    ///
    /// The symlink is the whole interface: forky's `tzdata` **deletes** `/etc/timezone`
    /// on configure ("Removing now unused /etc/timezone"), and reads the zone back by
    /// `readlink`-ing this symlink — so writing that file would be worse than
    /// redundant. `systemd`/`timedatectl` read the symlink too.
    pub fn localtime_target(timezone: &str) -> String {
        format!("/usr/share/zoneinfo/{timezone}")
    }

    /// The [`ntp_conf_path`](super::ntp_conf_path) content: the NTP servers the image
    /// reaches for first, in the form `client` reads.
    ///
    /// Neither form replaces the distribution's own servers, so a configured server is
    /// a preference that still degrades to them, where replacing them would strand an
    /// image the moment it booted anywhere else:
    ///
    ///  - `systemd-timesyncd`: `NTP=`, leaving `FallbackNTP=` alone. Debian compiles its
    ///    pool into that fallback, and `timesyncd` consults it whenever the primary list
    ///    yields nothing.
    ///  - `chrony`: one `server` line per entry, beside the distribution's sources. Each
    ///    is `prefer`, as Ubuntu's own pools are, and `require trust`. Ubuntu's pools use
    ///    NTS, and in chrony's default `authselectmode mix` an authenticated source makes
    ///    every unauthenticated one usable only while an authenticated one is selectable
    ///    and agrees with it. A server named here without `require trust` would then
    ///    never set the clock on a network that cannot reach those pools, which is the
    ///    network it is most often named for. With them, chrony takes the operator's word
    ///    for it.
    ///
    /// Resolution guarantees each entry is whitespace-free, which is what makes the
    /// space-separated join and the one-server-per-line form unambiguous.
    pub fn ntp_conf(client: TimeClient, servers: &[String]) -> String {
        match client {
            TimeClient::SystemdTimesyncd => format!(
                "# Generated by boot2deb. Consult the timesyncd.conf(5) manual page.\n\
                 #\n\
                 # FallbackNTP is deliberately not set here: Debian compiles its pool in as\n\
                 # the fallback, so the servers below are tried first and the pool still\n\
                 # covers this image on a network where they cannot be reached.\n\
                 [Time]\n\
                 NTP={servers}\n",
                servers = servers.join(" ")
            ),
            TimeClient::Chrony => {
                let mut out = String::from(
                    "# Generated by boot2deb. Consult the chrony.conf(5) manual page.\n\
                     #\n\
                     # The distribution's own sources stay configured beside these. Each\n\
                     # server here is preferred, as those are, and is also required and\n\
                     # trusted, so it sets the clock on a network where they cannot be\n\
                     # reached. Apply an edit with `chronyc reload sources`.\n",
                );
                for server in servers {
                    let _ = writeln!(out, "server {server} iburst prefer require trust");
                }
                out
            }
        }
    }

    /// The bounded drop-in for `wait`, written to
    /// `/etc/systemd/system/<unit>.d/bounded.conf`. See
    /// [`clock_wait`](super::clock_wait) for why the image bounds the wait.
    ///
    /// The empty `ExecStart=` clears the unit's own command, which a drop-in otherwise
    /// adds to rather than replaces.
    pub fn clock_wait_drop_in(wait: &super::ClockWait) -> String {
        format!(
            "# Generated by boot2deb.\n\
             #\n\
             # Hold time-sync.target until the clock is synchronized, but give up after\n\
             # 45 seconds. A board with no RTC boots with a stale clock, and the jobs\n\
             # ordered after time-sync.target check dates against it. The stock unit\n\
             # waits far longer, which on a board that never reaches a time server holds\n\
             # back every unit ordered after the target. The `-` prefix keeps a wait that\n\
             # runs out from failing the unit, so an offline boot is not `degraded`.\n\
             [Service]\n\
             ExecStart=\n\
             ExecStart={exec_start}\n",
            exec_start = wait.exec_start,
        )
    }

    /// `/etc/kernel-img.conf` — install kernels into `/boot` without symlinks, as
    /// the extlinux boot flow expects.
    pub fn kernel_img_conf() -> String {
        "link_in_boot = 1\ndo_symlinks = 0\n".to_string()
    }

    /// The [`initramfs_conf_path`](super::initramfs_conf_path) content: the initramfs
    /// settings an image has to state outright, because the generator runs on the build
    /// host while the hardware it is building for is absent, and its defaults resolve by
    /// probing whatever is in front of them.
    ///
    /// For `dracut` that is one setting and one for the lock's sake:
    ///
    ///  - `hostonly="no"`. Unless told otherwise, dracut builds host-only, sizing the
    ///    initrd to the drivers, filesystems and swap of the machine it runs on, which
    ///    here is the build host. A generic initrd also leaves `fsck` helpers and
    ///    `resume=` unprobed: every helper the image has is installed, and no host swap
    ///    device is written into the initrd's command line. `fstype` is not needed and
    ///    not written. The `filesystems` setting is an exclusive list, and setting it would
    ///    drop every other filesystem module from the initrd.
    ///  - `reproducible="yes"`, so dracut dates and orders the archive from the
    ///    `SOURCE_DATE_EPOCH` the customize step runs under, as `mkinitramfs` does
    ///    unasked. The modes it records follow the umask of the process that runs it,
    ///    and the customize step reaches it through `run-parts`, which runs every
    ///    hook under `022`, so the build host's umask never reaches the archive.
    ///
    /// For `initramfs-tools`, two:
    ///
    /// `RESUME=none`: the image is one ext4 root partition with no swap, so there is
    /// nothing to resume from. Left unset, the resume hook auto-detects a swap device and
    /// finds the **build host's**, writing that host's device path into the image; the
    /// board then retries it for about thirty seconds every boot before giving up.
    ///
    /// `FSTYPE`: the fsck hook reads `FSTYPE=auto` as an instruction to resolve the root
    /// device named in `/etc/fstab` and probe it for its type — and that is a `PARTUUID`
    /// belonging to a disk the image has not been written to yet, so the probe finds
    /// nothing and the initramfs ships with no `fsck` at all, leaving the root filesystem
    /// unchecked on every boot. Naming the type skips the probe. It selects which helper
    /// is installed and nothing more: at boot the root filesystem's type is read from the
    /// device itself.
    pub fn initramfs_conf(generator: InitramfsGenerator, fstype: &str) -> String {
        match generator {
            InitramfsGenerator::InitramfsTools => format!("RESUME=none\nFSTYPE={fstype}\n"),
            InitramfsGenerator::Dracut => "hostonly=\"no\"\nreproducible=\"yes\"\n".to_string(),
        }
    }

    /// `/etc/boot2deb/board.conf` — board boot parameters the loader's overlay
    /// scripts source, so those stay board-agnostic instead of hardcoding one board's
    /// dtb: the `/etc/kernel/postinst.d` + `postrm.d` hooks and `/boot/mk_extlinux`
    /// under `extlinux`, the DTB hook and `/etc/default/grub.d/boot2deb.cfg` under
    /// `efi`. Derived from the device's resolved `kernel_dtb`: `DTB_PATH` (relative to
    /// the DT output dir) locates the dtb inside the installed `linux-image` package,
    /// names its copy under the EFI system partition's `dtb/`, and `DTB_NAME` (its
    /// basename) names the copy placed in `/boot`. Under the `efi` loader `ESP_DTB_PATH`
    /// names the copy on the EFI system partition, under its `dtb/`: the name the
    /// board's u-boot loads by ([`ResolvedEfi::dtb`](boot2deb_core::model::ResolvedEfi::dtb)).
    ///
    /// `KERNEL_CMDLINE` contains the arguments either loader boots the kernel with
    /// after `root=`: `rootwait`, then the base console gate
    /// ([`CONSOLE_LOGLEVEL_ARG`]), then any extra arguments the board asked for
    /// ([`ResolvedImage::kernel_cmdline`]). It replaces `mk_extlinux`'s own
    /// `rootwait` default, so the assignment contains `rootwait` itself. The board's
    /// arguments come last so a board can override the gate. Resolution guarantees the
    /// value is safe to embed in this double-quoted, shell-sourced assignment.
    pub fn board_conf(kernel_dtb: &str, kernel_cmdline: &str, esp_dtb: Option<&str>) -> String {
        let name = kernel_dtb.rsplit('/').next().unwrap_or(kernel_dtb);
        let mut args = format!("rootwait {CONSOLE_LOGLEVEL_ARG}");
        if !kernel_cmdline.is_empty() {
            args.push(' ');
            args.push_str(kernel_cmdline);
        }
        let esp = esp_dtb
            .map(|dtb| format!("ESP_DTB_PATH=\"{dtb}\"\n"))
            .unwrap_or_default();
        format!(
            "# Generated by boot2deb -- board boot parameters. Do not edit.\n\
             # Sourced by the loader's kernel hooks and its configuration.\n\
             DTB_PATH=\"{kernel_dtb}\"\n\
             DTB_NAME=\"{name}\"\n\
             {esp}\
             KERNEL_CMDLINE=\"{args}\"\n"
        )
    }
}

#[cfg(test)]
mod tests {
    use boot2deb_core::model::ResolvedBuild;

    /// The image half of a fixture build. Every fixture here resolves a shipped image
    /// recipe, so the axis is there; the unwrap states that rather than threading an
    /// `Option` through every assertion.
    fn image_of(build: &boot2deb_core::ResolvedBuild) -> &boot2deb_core::ResolvedImage {
        pair_of(build).image
    }

    /// The same fixture build as an [`ImageBuild`] pair, for the stages that take one.
    fn pair_of(build: &boot2deb_core::ResolvedBuild) -> boot2deb_core::ImageBuild<'_> {
        build.as_image().expect("the fixture recipes build images")
    }
    use super::*;
    use boot2deb_core::{resolve_recipe, ConfigRoot, Overrides};

    fn repo_root() -> ConfigRoot {
        ConfigRoot::new(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(2)
                .unwrap()
                .to_path_buf(),
        )
    }

    fn rk1() -> ResolvedBuild {
        // The media-accel build: these rootfs tests assert the bootstrap includes the
        // feature packages (ffmpeg-rk et al.), so they need the recipe that includes userspace.
        resolve_recipe(
            &repo_root(),
            "turing-rk1/media-accel-forky",
            &Overrides::default(),
        )
        .unwrap()
    }

    /// A lock with nothing pinned. These tests exercise how the rootfs node *stages* the
    /// identity document, not what it contains — the contents are unit-tested where they
    /// are assembled, in `core::provenance`.
    fn empty_lock() -> boot2deb_core::lock::Lock {
        boot2deb_core::lock::Lock {
            kernel: None,
            patches: None,
            uboot: None,
            uboot_patches: None,
            tfa: None,
            userspace: Vec::new(),
            ffmpeg: None,
            rootfs: Some(boot2deb_core::lock::RootfsPin {
                distro: "debian".into(),
                suite: "forky".into(),
                manifest: "m.pkgs.lock".into(),
                manifest_sha256: None,
                source_date_epoch: 1_790_347_034,
            }),
            blobs: None,
            kmods: vec![],
            apps: vec![],
            extra_debs: vec![],
            snapshot: None,
        }
    }

    /// The `/etc/boot2deb/image.toml` document for a build, for the `RootfsOptions`
    /// fixtures below.
    fn ident(build: &ResolvedBuild) -> boot2deb_core::provenance::SystemIdentity {
        boot2deb_core::provenance::system_identity(pair_of(build), &empty_lock())
    }

    /// The staged tree's modes are the image's modes, and they must not depend on the
    /// umask the build ran under.
    ///
    /// Built at a `002` umask — the Ubuntu/Pop!_OS default, where a git checkout
    /// materializes `0664` files and `0775` directories — the tree normalizes to the
    /// two modes git can express, with the executable bit and the symlink intact. The
    /// `077` case is the one that would ship an unusable image (`/etc` at `0700`, so
    /// no non-root process could read `/etc/passwd`), so it is asserted too.
    #[test]
    fn staged_modes_do_not_follow_the_build_host_umask() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("overlay");
        std::fs::create_dir_all(root.join("etc/boot2deb/first-boot.d")).unwrap();
        // Group-writable throughout, as `cp -a` from a umask-002 checkout leaves it.
        let plain = root.join("etc/hostname");
        std::fs::write(&plain, "board\n").unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o664)).unwrap();
        let hook = root.join("etc/boot2deb/first-boot.d/20-network");
        std::fs::write(&hook, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&hook, std::fs::Permissions::from_mode(0o775)).unwrap();
        let link = root.join("etc/localtime");
        std::os::unix::fs::symlink("/usr/share/zoneinfo/UTC", &link).unwrap();
        for dir in ["etc", "etc/boot2deb", "etc/boot2deb/first-boot.d"] {
            std::fs::set_permissions(root.join(dir), std::fs::Permissions::from_mode(0o775))
                .unwrap();
        }
        // The `077` shape, on one directory: `/etc` unreadable to every non-root process.
        std::fs::create_dir(root.join("usr")).unwrap();
        std::fs::set_permissions(root.join("usr"), std::fs::Permissions::from_mode(0o700)).unwrap();

        normalize_overlay_modes(&root).unwrap();

        let mode = |p: &Path| p.symlink_metadata().unwrap().permissions().mode() & 0o7777;
        for dir in [
            "",
            "etc",
            "etc/boot2deb",
            "etc/boot2deb/first-boot.d",
            "usr",
        ] {
            assert_eq!(mode(&root.join(dir)), 0o755, "directory {dir:?}");
        }
        assert_eq!(mode(&plain), 0o644, "a non-executable file is 0644");
        assert_eq!(mode(&hook), 0o755, "an executable hook keeps its +x");
        // The link's own mode is untouched, and it still points where it did — a
        // `chmod` through the link would have rewritten its target instead.
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new("/usr/share/zoneinfo/UTC")
        );
    }

    /// A staged overlay's modification times are the lock's, not the checkout's.
    ///
    /// This is required rather than cosmetic. The tree is laid into the rootfs by a
    /// copy that *preserves* times, and the tar export clamps each member to
    /// `min(mtime, SOURCE_DATE_EPOCH)` — so a working copy older than the epoch would
    /// ship its own mtimes and two hosts building one lock would export different bytes.
    /// The symlink is the case that needs the raw `utimensat`: a helper that followed the
    /// link would stamp its target and leave the link's own mtime — which `tar` records —
    /// at whatever the checkout gave it.
    #[test]
    fn a_staged_overlay_is_stamped_with_the_lock_epoch() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("overlay");
        std::fs::create_dir_all(root.join("etc/kernel")).unwrap();
        let file = root.join("etc/hostname");
        std::fs::write(&file, "board\n").unwrap();
        let link = root.join("etc/localtime");
        std::os::unix::fs::symlink("/usr/share/zoneinfo/UTC", &link).unwrap();

        const EPOCH: u64 = 1_700_000_000;
        normalize_overlay_times(&root, EPOCH).unwrap();

        let mtime = |p: &Path| {
            use std::os::unix::fs::MetadataExt;
            p.symlink_metadata().unwrap().mtime()
        };
        for path in [
            root.as_path(),
            &root.join("etc"),
            &root.join("etc/kernel"),
            &file,
            &link,
        ] {
            assert_eq!(mtime(path), EPOCH as i64, "{}", path.display());
        }
        // The link's target was not touched — the stamp went to the link itself.
        assert!(link.symlink_metadata().unwrap().file_type().is_symlink());
    }

    /// "Later layers win" has to hold for every file a layer may legitimately ship,
    /// including one with no owner-write bit — a key, a `sudoers.d` drop-in. `cp` opens
    /// an existing destination for writing, so without `--remove-destination` a device
    /// overriding such a file from its SoC layer fails the whole build with
    /// `Permission denied`, and the layering contract silently has an exception in it.
    #[test]
    fn a_later_layer_overrides_a_read_only_file() {
        use std::os::unix::fs::PermissionsExt;

        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().join("base");
        let device = tmp.path().join("device");
        std::fs::create_dir_all(base.join("etc/ssh")).unwrap();
        std::fs::create_dir_all(device.join("etc/ssh")).unwrap();
        let key = base.join("etc/ssh/ssh_host_ed25519_key");
        std::fs::write(&key, "base key\n").unwrap();
        std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o400)).unwrap();
        std::fs::write(device.join("etc/ssh/ssh_host_ed25519_key"), "device key\n").unwrap();

        let staging = tmp.path().join("staging");
        let sink = |_: crate::event::Event| {};
        let step = Step::start(&sink, "overlay");
        copy_overlay_trees(&staging, &[base, device], &step).unwrap();

        assert_eq!(
            std::fs::read_to_string(staging.join("etc/ssh/ssh_host_ed25519_key")).unwrap(),
            "device key\n"
        );
    }

    /// Every first-boot hook any layer ships must be executable.
    ///
    /// The overlay trees are copied with `cp -a` and normalized by git's two file modes
    /// ([`normalize_overlay_modes`]), so a hook's executable bit in the repo is its
    /// executable bit in the image, and
    /// [the runner](../../../base/overlay/usr/lib/boot2deb/first-boot)
    /// refuses to run a non-executable one. The hooks are essential — depthcharge's
    /// re-signs the kernel partition against the UUID first-boot has just changed — so a
    /// lost `+x` bit is the difference between a board that boots and one that boots
    /// exactly once. Nothing else in the build would notice, hence this test.
    #[test]
    fn shipped_first_boot_hooks_are_executable() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .ancestors()
            .nth(2)
            .unwrap()
            .to_path_buf();
        let mut checked: Vec<std::path::PathBuf> = Vec::new();
        // Every layer kind that can contain an overlay tree, which is every kind the
        // rootfs stage merges: base, then distro, soc, boot method, loader, device,
        // feature. This list must stay in step with the CLI's `overlay_dirs` — a kind
        // missing here is not a failing test but a *vacuous* one, silently exempting
        // whatever it holds.
        let layer_dirs = std::iter::once(root.join("base")).chain(
            [
                "distros",
                "socs",
                "boot-methods",
                "loaders",
                "devices",
                "features",
            ]
            .iter()
            .flat_map(|kind| {
                std::fs::read_dir(root.join(kind))
                    .into_iter()
                    .flatten()
                    .flatten()
                    .map(|e| e.path())
                    .filter(|p| p.is_dir())
            }),
        );

        for layer in layer_dirs {
            for overlay in ["overlay", "overlay-pre", "overlay-nonfree"] {
                let hooks = layer.join(overlay).join("etc/boot2deb/first-boot.d");
                let Ok(entries) = std::fs::read_dir(&hooks) else {
                    continue; // this layer ships no hooks, which is the common case
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    let mode = path.metadata().unwrap().permissions().mode();
                    assert!(
                        mode & 0o111 != 0,
                        "first-boot hook {} is mode {:o}; it would be copied into the image \
                         non-executable and the runner would refuse to boot past it",
                        path.display(),
                        mode & 0o7777,
                    );
                    checked.push(path);
                }
            }
        }
        // A path typo that found no hooks at all would make the assertions above vacuous,
        // and so would a layer kind missing from the list above. Name one hook from each
        // kind that ships one, so losing a whole kind fails here instead of quietly
        // shrinking the test's reach. (No boot-method layer ships a hook: boot identity
        // is fixed at build time, so there is nothing bootloader-side to redo on-device.)
        for expected in [
            "socs/rk3288/overlay/etc/boot2deb/first-boot.d/20-audio",
            "devices/turing-rk1/overlay/etc/boot2deb/first-boot.d/20-network",
        ] {
            assert!(
                checked.contains(&root.join(expected)),
                "{expected} was never checked — the layer-kind list above has a hole in it",
            );
        }
    }

    /// The other shape: a depthcharge board with a distro kernel.
    fn c201() -> ResolvedBuild {
        resolve_recipe(&repo_root(), "asus-c201/forky", &Overrides::default()).unwrap()
    }

    #[test]
    fn board_conf_derives_dtb_path_and_name() {
        let c = config::board_conf("rockchip/rk3576-h96-max-m9.dtb", "", None);
        assert!(c.contains("DTB_PATH=\"rockchip/rk3576-h96-max-m9.dtb\""));
        assert!(c.contains("DTB_NAME=\"rk3576-h96-max-m9.dtb\""));
        // A bare filename (no dir) uses itself as the name.
        assert!(config::board_conf("board.dtb", "", None).contains("DTB_NAME=\"board.dtb\""));
    }

    #[test]
    fn board_conf_names_the_esp_dtb_under_the_efi_loader_alone() {
        let efi = config::board_conf(
            "rockchip/rk3576-h96-max-m9.dtb",
            "",
            Some("rockchip/rk3576-generic.dtb"),
        );
        assert!(
            efi.contains("ESP_DTB_PATH=\"rockchip/rk3576-generic.dtb\"\n"),
            "{efi}"
        );
        assert!(
            efi.contains("DTB_PATH=\"rockchip/rk3576-h96-max-m9.dtb\"\n"),
            "{efi}"
        );
        let plain = config::board_conf("rockchip/rk3576-h96-max-m9.dtb", "", None);
        assert!(!plain.contains("ESP_DTB_PATH"), "{plain}");
    }

    #[test]
    fn board_conf_gates_the_console_on_every_board_and_lets_one_override_it() {
        // Every board gets the assignment, because every board gets the console gate.
        // It re-states `mk_extlinux`'s `rootwait` default, which it replaces.
        let plain = config::board_conf("rockchip/rk3588-turing-rk1.dtb", "", None);
        assert!(plain.contains("KERNEL_CMDLINE=\"rootwait loglevel=4\"\n"));

        // A board's own arguments are appended *after* the gate, so a board that wants
        // a louder console can say so and the kernel takes its last `loglevel=`.
        let extra = config::board_conf(
            "rockchip/rk3576-h96-max-m9.dtb",
            "video=HDMI-A-1:d cpuidle.off=1",
            None,
        );
        assert!(extra
            .contains("KERNEL_CMDLINE=\"rootwait loglevel=4 video=HDMI-A-1:d cpuidle.off=1\"\n"));
        let louder = config::board_conf("rockchip/rk3576-h96-max-m9.dtb", "loglevel=7", None);
        assert!(louder.contains("KERNEL_CMDLINE=\"rootwait loglevel=4 loglevel=7\"\n"));
    }

    /// The rootfs PARTUUID the image node will write — a fixed value for the tests.
    const PARTUUID: uuid::Uuid = uuid::Uuid::from_bytes([0xb2; 16]);

    /// The ESP PARTUUID the image node will write under the efi loader.
    const ESP_PARTUUID: uuid::Uuid = uuid::Uuid::from_bytes([0xe5; 16]);

    /// The partitions a raw-gap or depthcharge image names: root alone.
    const PARTITIONS: Partitions = Partitions {
        rootfs_partuuid: PARTUUID,
        rootfs_fs_uuid: uuid::Uuid::from_bytes([0xe4; 16]),
        rootfs_index: 2,
        esp_partuuid: None,
    };

    #[test]
    fn a_raw_gap_boot_mounts_root_by_the_partuuid_the_image_will_write() {
        // mk_extlinux reads root back out of fstab to build the cmdline, so this line
        // must name the partition the image node stamps into the GPT. It is the
        // device's boot identity for life — first-boot grows the partition but never
        // rewrites its PARTUUID — so no label and no filesystem UUID appears here.
        let f = {
            let b = rk1();
            config::fstab(pair_of(&b), &PARTITIONS)
        };
        let expected = format!("PARTUUID={}\t/\text4", PARTUUID.hyphenated());
        assert!(
            f.contains(&expected),
            "fstab must root on the derived PARTUUID:\n{f}"
        );
        assert!(!f.contains("LABEL="));
        assert!(!f.contains("\tUUID="));
        assert!(f.contains("mk_extlinux"));
    }

    #[test]
    fn an_efi_boot_mounts_the_esp_beside_root_by_its_partuuid() {
        // grub-install and the DTB hook write to the ESP's mount point, so fstab must
        // mount the partition the image node writes there, by the PARTUUID it stamps.
        let b = resolve_recipe(
            &repo_root(),
            "turing-rk1/forky",
            &Overrides {
                loader: Some(boot2deb_core::model::Loader::Efi),
                ..Default::default()
            },
        )
        .unwrap();
        let f = config::fstab(
            pair_of(&b),
            &Partitions {
                rootfs_index: 3,
                esp_partuuid: Some(ESP_PARTUUID),
                ..PARTITIONS
            },
        );
        let lines: Vec<&str> = f.lines().filter(|l| !l.starts_with('#')).collect();
        assert!(lines[0].starts_with(&format!("PARTUUID={}\t/\t", PARTUUID.hyphenated())));
        assert_eq!(
            lines[1],
            format!(
                "PARTUUID={}\t/boot/efi\tvfat\tumask=0077,nofail\t0      2",
                ESP_PARTUUID.hyphenated()
            )
        );
        assert!(f.contains("update-grub"), "{f}");
        assert!(!f.contains("mk_extlinux"), "{f}");
    }

    #[test]
    fn a_depthcharge_boot_mounts_root_by_the_partuuid_the_image_will_write() {
        // This line is what depthchargectl signs into the kernel, so it must name the
        // partition the image node is about to create — not a label, which the tool
        // would not resolve, and not a filesystem UUID, which does not exist yet.
        let f = {
            let b = c201();
            config::fstab(pair_of(&b), &PARTITIONS)
        };
        let expected = format!("PARTUUID={}\t/\text4", PARTUUID.hyphenated());
        assert!(
            f.contains(&expected),
            "fstab must root on the derived PARTUUID:\n{f}"
        );
        assert!(!f.contains("LABEL="));
        // And it says how to re-sign, because editing this line without doing so
        // produces a system that boots once and then does not.
        assert!(f.contains("depthchargectl write"));
    }

    #[test]
    fn a_data_volume_follows_root_by_label_and_cannot_block_the_boot() {
        use boot2deb_core::datavolume::{
            CreatePolicy, DataVolume, DiskKind, VolumeFs, VolumeMatch,
        };
        let mut b = rk1();
        b.image.as_mut().unwrap().data_volumes = vec![DataVolume {
            match_: VolumeMatch::Kind(DiskKind::Nvme),
            label: "b2d-data".into(),
            fstype: VolumeFs::Ext4,
            mount: "/srv".into(),
            create: CreatePolicy::IfBlank,
        }];
        let f = config::fstab(pair_of(&b), &PARTITIONS);
        let lines: Vec<&str> = f.lines().filter(|l| !l.starts_with('#')).collect();

        // Root still comes first and still roots on the PARTUUID.
        assert!(lines[0].starts_with(&format!("PARTUUID={}", PARTUUID.hyphenated())));
        // The data volume mounts by LABEL: the one identifier that survives the disk
        // moving slots, and the whole point of a volume that outlives its image.
        assert_eq!(
            lines[1],
            "LABEL=b2d-data\t/srv\text4\tdefaults,nofail,x-systemd.device-timeout=10s\t0 2"
        );
        // A missing or slow data disk must never be why a system will not boot.
        assert!(lines[1].contains("nofail"));
    }

    #[test]
    fn a_build_with_no_data_volumes_writes_the_fstab_it_always_did() {
        // The feature is opt-in, so an image that declares none must not gain a line,
        // a blank, or a comment about an axis it does not use.
        let f = {
            let b = rk1();
            config::fstab(pair_of(&b), &PARTITIONS)
        };
        assert_eq!(f.lines().filter(|l| !l.starts_with('#')).count(), 1);
        assert!(!f.contains("LABEL="));
    }

    #[test]
    fn the_build_time_depthcharge_config_disarms_the_kernel_hooks() {
        // False during the build: the hook it gates runs `depthchargectl write`, which
        // hunts the *host's* block devices for a ChromeOS kernel partition. With the
        // target SD card plugged in while building, that is a real write to a real disk.
        let build = config::depthcharge_config("speedy", false);
        assert!(build.contains("enable-system-hooks = False"));
        assert!(build.contains("board = speedy"));
        // True in the shipped image, or an apt kernel upgrade would leave the signed
        // partition holding the old kernel while the filesystem moved on.
        assert!(config::depthcharge_config("speedy", true).contains("enable-system-hooks = True"));
    }

    #[test]
    fn the_depthcharge_initramfs_conf_states_the_list_and_the_slot_s_compressor() {
        // Pinned rather than probed: this file is what every `update-initramfs` on the
        // board reads, at build time and for the life of the image.
        assert_eq!(
            config::depthcharge_initramfs_conf(InitramfsCompress::Xz),
            "# Generated by boot2deb.\nMODULES=list\nCOMPRESS=xz\n"
        );
        assert_eq!(
            config::depthcharge_initramfs_conf(InitramfsCompress::Zstd),
            "# Generated by boot2deb.\nMODULES=list\nCOMPRESS=zstd\n"
        );
    }

    #[test]
    fn a_depthcharge_board_is_told_its_compressor_before_a_kernel_package_arrives() {
        // The whole reason this is pre-install config: `MODULES=list` and `COMPRESS=`
        // have to be readable when the kernel package's own hooks build the first
        // initramfs, or that one is built wrong and thrown away.
        let tmp = tempfile::tempdir().unwrap();
        let sink = |_e: crate::event::Event| {};
        let step = Step::start(&sink, "test");
        stage_preinstall_overlay(
            tmp.path(),
            &[],
            image_of(&c201()),
            Some(BootConfig::Depthcharge {
                board: "speedy-libreboot",
                cmdline: "console=tty1 ro",
                initramfs_compress: InitramfsCompress::Zstd,
            }),
            1_790_347_034,
            &step,
        )
        .unwrap();
        let conf = std::fs::read_to_string(
            tmp.path()
                .join("etc/initramfs-tools/conf.d/depthcharge.conf"),
        )
        .unwrap();
        assert!(conf.contains("COMPRESS=zstd"), "{conf}");
        assert!(conf.contains("MODULES=list"), "{conf}");

        // A raw-gap board has no signed payload and generates none of this.
        let rkbin = tempfile::tempdir().unwrap();
        stage_preinstall_overlay(
            rkbin.path(),
            &[],
            image_of(&rk1()),
            None,
            1_790_347_034,
            &step,
        )
        .unwrap();
        assert!(!rkbin
            .path()
            .join("etc/initramfs-tools/conf.d/depthcharge.conf")
            .exists());
    }

    /// The shipped distro layer serving `suite`, resolved, and the image components a
    /// build of it resolves against.
    fn shipped_distro(
        suite: &str,
        libre: bool,
    ) -> (boot2deb_core::distro::ResolvedDistro, Vec<String>) {
        let root = repo_root();
        let (name, layer) = root.distro_for_suite(suite).unwrap();
        let components = layer.image_components(libre).unwrap().to_vec();
        (layer.resolve(&name, suite), components)
    }

    #[test]
    fn apt_sources_has_suite_pockets_and_components() {
        // The emitted text is pinned rather than probed for substrings: this file is
        // what every `apt update` on the shipped board reads, and the component list
        // and archive-per-pocket split are both part of the contract.
        let (distro, components) = shipped_distro("forky", false);
        assert_eq!(
            config::apt_sources(&distro, &components),
            "Types: deb\n\
             URIs: http://deb.debian.org/debian\n\
             Suites: forky forky-updates\n\
             Components: main contrib non-free non-free-firmware\n\
             Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg\n\
             \n\
             Types: deb\n\
             URIs: http://deb.debian.org/debian-security\n\
             Suites: forky-security\n\
             Components: main contrib non-free non-free-firmware\n\
             Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg\n"
        );
    }

    #[test]
    fn a_libre_image_offers_only_the_free_archive() {
        // The whole point of the libre axis reaching this file: an image whose kernel
        // cannot load a blob does not offer `non-free-firmware` to the person at its
        // console either. Every pocket narrows, not just the first.
        let (distro, components) = shipped_distro("forky", true);
        let sources = config::apt_sources(&distro, &components);
        assert_eq!(
            sources.matches("Components: main\n").count(),
            2,
            "{sources}"
        );
        assert!(!sources.contains("non-free"), "{sources}");
    }

    #[test]
    fn apt_sources_omits_the_pockets_unstable_does_not_publish() {
        // Debian publishes neither `sid-security` nor `sid-updates` — both 404, so an
        // image emitting them reports two dead sources on every `apt update`. `sid` is a
        // documented, deliberate suite value, so the pocket set belongs to the distro
        // layer and not to suite validation.
        for suite in ["sid", "unstable"] {
            let (distro, components) = shipped_distro(suite, false);
            assert_eq!(
                config::apt_sources(&distro, &components),
                format!(
                    "Types: deb\nURIs: http://deb.debian.org/debian\nSuites: {suite}\n\
                     Components: main contrib non-free non-free-firmware\n\
                     Signed-By: /usr/share/keyrings/debian-archive-keyring.gpg\n"
                )
            );
        }
    }

    #[test]
    fn ubuntu_sources_are_the_stock_layout() {
        // What a stock 26.04 install writes: release, updates and backports on the
        // archive, security on its own host, all four components, the package keyring.
        let (distro, components) = shipped_distro("resolute", false);
        assert_eq!(distro.name, "ubuntu");
        assert_eq!(
            config::apt_sources(&distro, &components),
            "Types: deb\n\
             URIs: http://archive.ubuntu.com/ubuntu\n\
             Suites: resolute resolute-updates resolute-backports\n\
             Components: main restricted universe multiverse\n\
             Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg\n\
             \n\
             Types: deb\n\
             URIs: http://security.ubuntu.com/ubuntu\n\
             Suites: resolute-security\n\
             Components: main restricted universe multiverse\n\
             Signed-By: /usr/share/keyrings/ubuntu-archive-keyring.gpg\n"
        );
    }

    #[test]
    fn hosts_names_the_hostname() {
        assert!(config::hosts("turing-rk1").contains("127.0.1.1\tturing-rk1"));
    }

    /// Build a small rootfs-shaped tarball (`./`-prefixed, as the export writes) at
    /// `tarball` from the given `members` (`archive-path`, `contents`). Returns false
    /// if `tar` is unavailable so the caller can skip.
    #[cfg(test)]
    fn build_test_tar(tarball: &Path, root: &Path, members: &[(&str, &str)]) -> bool {
        for (path, contents) in members {
            let full = root.join(path.trim_start_matches("./"));
            std::fs::create_dir_all(full.parent().unwrap()).unwrap();
            std::fs::write(&full, contents).unwrap();
        }
        let mut cmd = Command::new("tar");
        cmd.arg("-cf").arg(tarball).arg("-C").arg(root);
        for (path, _) in members {
            cmd.arg(path);
        }
        cmd.status().map(|s| s.success()).unwrap_or(false)
    }

    #[test]
    fn validate_tar_rejects_a_tar_missing_a_required_member() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("r");
        let tarball = tmp.path().join("rootfs.tar");
        // Missing `./etc/shadow` — the completeness sentinel — stands in for a tar
        // truncated by an interrupted bootstrap.
        let members = [
            ("./etc/os-release", "ID=debian\n"),
            ("./etc/fstab", "LABEL=rootfs / ext4 defaults 0 1\n"),
        ];
        if !build_test_tar(&tarball, &root, &members) {
            eprintln!("skipping: tar unavailable");
            return;
        }
        let err = validate_tar(&tarball).unwrap_err();
        assert!(
            matches!(err, EngineError::ArtifactMissing { what, .. } if what.contains("etc/shadow")),
            "expected a missing-member error naming shadow"
        );
    }

    #[test]
    fn locale_gen_pairs_each_locale_with_its_codeset() {
        // locale-gen reads `<name> <codeset>` pairs, and the codeset is not separate
        // config — it is contained inside the name.
        let g = config::locale_gen(&[
            "C.UTF-8".to_string(),
            "en_US.UTF-8".to_string(),
            "sr_RS.UTF-8@latin".to_string(),
        ]);
        assert_eq!(
            g, "C.UTF-8 UTF-8\nen_US.UTF-8 UTF-8\nsr_RS.UTF-8@latin UTF-8\n",
            "a modifier goes after the codeset, not inside it"
        );
    }

    #[test]
    fn the_system_locale_is_written_to_locale_conf_not_default_locale() {
        // This is the whole point of the path choice. Debian makes /etc/default/locale
        // a symlink to /etc/locale.conf and systemd-tmpfiles re-asserts it with a
        // *forcing* rule (`L+ /etc/default/locale ... ../locale.conf`) — so a regular
        // file written at /etc/default/locale is deleted and replaced by the symlink on
        // the next boot, leaving the image with no locale at all. Write the target.
        assert_eq!(config::locale_conf("C.UTF-8"), "LANG=C.UTF-8\n");

        let tmp = tempfile::tempdir().unwrap();
        let sink = |_e: crate::event::Event| {};
        let step = Step::start(&sink, "test");
        stage_preinstall_overlay(
            tmp.path(),
            &[],
            image_of(&rk1()),
            None,
            1_790_347_034,
            &step,
        )
        .unwrap();

        assert!(tmp.path().join("etc/locale.conf").is_file());
        assert!(
            !tmp.path().join("etc/default/locale").exists(),
            "must not write /etc/default/locale — tmpfiles force-replaces it with a symlink"
        );
    }

    /// The identity document reaches the image, at the path a tool reading this disk
    /// from outside looks for it.
    ///
    /// The two assertions are the two halves of why the file exists. The board profile
    /// is the value that is **not** recoverable from the disk — `depthchargectl` gets it
    /// from the *running* board's HWID, so a tool re-signing this image's kernel from
    /// other media, or from a machine that is not this board, can only be told. And the
    /// document ships *inside* the image, so it must never contain the per-image first-boot
    /// password that its sibling — the provenance manifest, which stays with the build —
    /// exists to record.
    #[test]
    fn the_staged_overlay_contains_the_image_identity() {
        let build = c201();
        let identity = ident(&build);
        let tmp = tempfile::tempdir().unwrap();
        let sink = |_e: crate::event::Event| {};
        let step = Step::start(&sink, "test");
        stage_overlay(
            tmp.path(),
            &[],
            pair_of(&build),
            PARTITIONS,
            &identity,
            1_790_347_034,
            &step,
        )
        .unwrap();

        let path = tmp.path().join("etc/boot2deb/image.toml");
        let text = std::fs::read_to_string(&path).expect("identity staged into the rootfs");

        assert!(
            text.contains("board = \"speedy\""),
            "the depthcharge board profile is the one value an outside tool cannot \
             derive from the disk; without it the file has no reason to exist:\n{text}"
        );
        assert!(text.contains("device = \"asus-c201\""));
        assert!(
            text.contains("version = 1"),
            "a wire format read by other programs"
        );

        let data: String = text
            .lines()
            .filter(|l| !l.trim_start().starts_with('#'))
            .collect();
        assert!(
            !data.contains("password"),
            "the first-boot secret must never ship inside an image:\n{text}"
        );

        // A rockchip-rkbin board has no board profile, so it records none rather than an
        // empty string a reader would have to special-case.
        let rk = rk1();
        let tmp2 = tempfile::tempdir().unwrap();
        stage_overlay(
            tmp2.path(),
            &[],
            pair_of(&rk),
            PARTITIONS,
            &ident(&rk),
            1_790_347_034,
            &step,
        )
        .unwrap();
        let rk_text = std::fs::read_to_string(tmp2.path().join("etc/boot2deb/image.toml")).unwrap();
        assert!(rk_text.contains("boot_method = \"rockchip-rkbin\""));
        assert!(!rk_text.contains("board ="));
    }

    /// A pinned ref becomes the release the *kernel* reports, not the ref read
    /// literally — the two differ on exactly the tags upstream publishes for a
    /// `.0` release.
    ///
    /// `v7.2` is the whole reason this test exists. The on-image runner matches
    /// the emitted value against `uname -r` as a prefix ending at a `-`, so a
    /// literal `7.2` is compared against `7.2.0-1-arm64`, finds `.` where it
    /// needs `-`, and fails a correct kernel. Padding to the Makefile's
    /// `SUBLEVEL` is what makes the prefix rule true, and it belongs to
    /// `parse_tag` rather than being re-decided here.
    #[test]
    fn a_pinned_ref_becomes_the_release_the_kernel_reports() {
        for (reference, want, unamer) in [
            // A `.0` release: upstream drops the patch component, the kernel does not.
            ("v7.2", "7.2.0", "7.2.0-1-arm64"),
            // A point release already spells all three.
            ("v7.1.6", "7.1.6", "7.1.6-1-arm64"),
            // Linux-libre: namespaced tag, and `-gnu` goes into the release suffix.
            ("sources/v7.1.6-gnu", "7.1.6", "7.1.6-gnu-1-arm64"),
            // A prerelease survives, and still pads.
            ("v7.2-rc3", "7.2.0-rc3", "7.2.0-rc3-1-arm64"),
        ] {
            let got = config::kernel_release_from_ref(reference)
                .unwrap_or_else(|| panic!("{reference} names a version"));
            assert_eq!(got, want, "{reference}");
            // The property the runner actually relies on: equal, or a prefix
            // ending at a `-`. Asserting it here is what keeps the emitted
            // spelling and the matcher from drifting apart again.
            assert!(
                unamer == got
                    || unamer
                        .strip_prefix(&got)
                        .is_some_and(|t| t.starts_with('-')),
                "{reference}: {got:?} is not a `-`-bounded prefix of {unamer:?}"
            );
        }

        // Naming no version emits no check: a prefix match would be meaningless,
        // and a check that cannot be right is worse than one that is absent.
        for reference in [
            "master",
            "linux-7.1.y",
            "8d3ae59288f1e7d58d76558a6ee96d533bc5019f",
        ] {
            assert_eq!(
                config::kernel_release_from_ref(reference),
                None,
                "{reference} names no version"
            );
        }
    }

    /// The selftest check files reach the image: the derived `identity.checks`
    /// plus one file per layer-declared expectation group, all under the
    /// directory the on-device runner globs.
    ///
    /// The derived file's contents differ by what the build is, and both shapes
    /// are asserted: a compiled-kernel rkbin board states its flavor and its DTB;
    /// a distro-kernel depthcharge board states the package's flavor and no DTB
    /// (its DTB is inside the signed FIT, invisible to a file check). Neither
    /// states a `kernel-release` here because the fixture lock pins no kernel —
    /// exactly the distro case, and the compiled case is covered by
    /// [`a_pinned_ref_becomes_the_release_the_kernel_reports`].
    #[test]
    fn the_staged_overlay_contains_the_selftest_checks() {
        let rk = rk1();
        let tmp = tempfile::tempdir().unwrap();
        let sink = |_e: crate::event::Event| {};
        let step = Step::start(&sink, "test");
        stage_overlay(
            tmp.path(),
            &[],
            pair_of(&rk),
            PARTITIONS,
            &ident(&rk),
            1_790_347_034,
            &step,
        )
        .unwrap();
        let dir = tmp.path().join(boot2deb_core::expect::CHECKS_DIR);
        let identity = std::fs::read_to_string(dir.join("identity.checks")).unwrap();
        assert!(identity.contains("kernel-flavor     arm64\n"), "{identity}");
        assert!(
            identity.contains("dtb               rockchip/rk3588-turing-rk1.dtb\n"),
            "{identity}"
        );
        // The two unconditional lines, which take no argument — so each must render
        // as the bare word rather than a word and the padding of an argument that is
        // not there.
        assert!(identity.contains("\nsingle-kernel\n"), "{identity}");
        assert!(identity.contains("\nunique-partuuid\n"), "{identity}");
        // Every group the build resolved is a file in the directory, named for
        // its declaring layer.
        for group in &image_of(&rk).expectations {
            assert!(
                dir.join(group.file_name()).is_file(),
                "missing {}",
                group.file_name()
            );
        }

        let c2 = c201();
        let tmp2 = tempfile::tempdir().unwrap();
        stage_overlay(
            tmp2.path(),
            &[],
            pair_of(&c2),
            PARTITIONS,
            &ident(&c2),
            1_790_347_034,
            &step,
        )
        .unwrap();
        let identity = std::fs::read_to_string(
            tmp2.path()
                .join(boot2deb_core::expect::CHECKS_DIR)
                .join("identity.checks"),
        )
        .unwrap();
        assert!(identity.contains("kernel-flavor     armmp\n"), "{identity}");
        assert!(!identity.contains("dtb "), "{identity}");
        // Every name in the board's initramfs list is re-checked on the device, where
        // an on-device kernel upgrade regenerates the initrd with no build watching.
        for module in &image_of(&c2).initramfs_modules {
            assert!(
                identity.contains(&format!("\ninitramfs-module  {module}\n")),
                "{module} missing from {identity}"
            );
        }
    }

    /// The initramfs module list reaches the image as a generated `modules.d` drop-in,
    /// staged before any package so the first initramfs the install builds already has
    /// it. A board that names no module gets no drop-in, and `initramfs-tools` selects on
    /// its own.
    #[test]
    fn the_initramfs_list_is_staged_as_a_generated_drop_in() {
        let sink = |_e: crate::event::Event| {};
        let step = Step::start(&sink, "test");

        let c2 = c201();
        let laptop = tempfile::tempdir().unwrap();
        stage_preinstall_overlay(
            laptop.path(),
            &[],
            image_of(&c2),
            None,
            1_790_347_034,
            &step,
        )
        .unwrap();
        let tools = InitramfsGenerator::InitramfsTools;
        let dropin = std::fs::read_to_string(laptop.path().join(initramfs_modules_drop_in(tools)))
            .expect("the drop-in is staged");
        let names: Vec<&str> = dropin
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .collect();
        assert_eq!(names, image_of(&c2).initramfs_modules);
        assert!(dropin.starts_with("# Generated by boot2deb"), "{dropin}");

        // The same list under dracut: one `force_drivers+=` line per name, in its own
        // drop-in, and nothing where initramfs-tools would read. The board is a
        // depthcharge one, which resolution keeps on initramfs-tools, so the generator
        // is set on the resolved image directly: what is under test is the staging.
        let mut dracut = c201();
        dracut.image.as_mut().unwrap().initramfs_generator = InitramfsGenerator::Dracut;
        let staged = tempfile::tempdir().unwrap();
        stage_preinstall_overlay(
            staged.path(),
            &[],
            image_of(&dracut),
            None,
            1_790_347_034,
            &step,
        )
        .unwrap();
        let dropin = std::fs::read_to_string(
            staged
                .path()
                .join(initramfs_modules_drop_in(InitramfsGenerator::Dracut)),
        )
        .expect("the dracut drop-in is staged");
        let lines: Vec<String> = dropin
            .lines()
            .filter(|l| !l.starts_with('#') && !l.is_empty())
            .map(str::to_string)
            .collect();
        let expected: Vec<String> = image_of(&c2)
            .initramfs_modules
            .iter()
            .map(|m| format!("force_drivers+=\" {m} \""))
            .collect();
        assert_eq!(lines, expected);
        assert!(!staged
            .path()
            .join(initramfs_modules_drop_in(tools))
            .exists());

        let headless = tempfile::tempdir().unwrap();
        stage_preinstall_overlay(
            headless.path(),
            &[],
            image_of(&rk1()),
            None,
            1_790_347_034,
            &step,
        )
        .unwrap();
        assert!(!headless
            .path()
            .join(initramfs_modules_drop_in(tools))
            .exists());
    }

    /// The boot-time selftest service is enabled by the recipe flag and only by
    /// it: without the flag no `.wants` symlink is staged (the unit itself ships
    /// from the base overlay, which these bare-overlay fixtures do not copy).
    #[test]
    fn the_selftest_service_symlink_follows_the_recipe_flag() {
        let mut build = rk1();
        let tmp = tempfile::tempdir().unwrap();
        let sink = |_e: crate::event::Event| {};
        let step = Step::start(&sink, "test");
        let link = "etc/systemd/system/multi-user.target.wants/boot2deb-selftest.service";
        stage_overlay(
            tmp.path(),
            &[],
            pair_of(&build),
            PARTITIONS,
            &ident(&build),
            1_790_347_034,
            &step,
        )
        .unwrap();
        assert!(
            !tmp.path().join(link).symlink_metadata().is_ok(),
            "no symlink unless the recipe opts in"
        );

        build.image.as_mut().unwrap().selftest_on_boot = true;
        let tmp2 = tempfile::tempdir().unwrap();
        stage_overlay(
            tmp2.path(),
            &[],
            pair_of(&build),
            PARTITIONS,
            &ident(&build),
            1_790_347_034,
            &step,
        )
        .unwrap();
        let target = std::fs::read_link(tmp2.path().join(link)).expect("enable symlink staged");
        assert_eq!(target, Path::new("../boot2deb-selftest.service"));
    }

    /// The `kernel-release` line is derived only from a ref that names a version.
    #[test]
    fn a_kernel_release_check_needs_a_version_shaped_ref() {
        let mut build = rk1();
        let mut lock = empty_lock();
        lock.kernel = Some(boot2deb_core::lock::KernelPin {
            id: "rk3588-mainline-7.1".into(),
            source: "https://git.example/linux.git".into(),
            reference: "v7.1.6".into(),
            commit: "0".repeat(40),
            revision: 1,
            config_sha256: "d".repeat(64),
        });
        let identity = boot2deb_core::provenance::system_identity(pair_of(&build), &lock);
        let checks = config::selftest_identity(pair_of(&build), &identity);
        assert!(checks.contains("kernel-release    7.1.6\n"), "{checks}");

        // A bare commit pins the tree but names no version to compare uname against.
        lock.kernel.as_mut().unwrap().reference = "95a6c488".into();
        let identity = boot2deb_core::provenance::system_identity(pair_of(&build), &lock);
        let checks = config::selftest_identity(pair_of(&build), &identity);
        assert!(!checks.contains("kernel-release"), "{checks}");

        // The dtb line follows the boot method, already asserted both ways above;
        // here only that a split-layout rkbin build still emits it (the rootfs
        // half is the one being checked, wherever the bootloader lives).
        build.layout = boot2deb_core::model::Layout::Split;
        let identity = boot2deb_core::provenance::system_identity(pair_of(&build), &lock);
        assert!(config::selftest_identity(pair_of(&build), &identity).contains("dtb "));
    }

    /// The settings the image needs because it is built on a machine that is not the
    /// board. The failure is silent at build time and only surfaces on hardware: an
    /// initramfs that waits for the *builder's* swap and then skips the root filesystem
    /// check.
    #[test]
    fn the_staged_overlay_states_what_the_build_host_would_otherwise_answer() {
        let build = rk1();
        let tmp = tempfile::tempdir().unwrap();
        let sink = |_e: crate::event::Event| {};
        let step = Step::start(&sink, "test");
        stage_overlay(
            tmp.path(),
            &[],
            pair_of(&build),
            PARTITIONS,
            &ident(&build),
            1_790_347_034,
            &step,
        )
        .unwrap();

        let conf =
            std::fs::read_to_string(tmp.path().join("etc/initramfs-tools/conf.d/boot2deb.conf"))
                .expect("the initramfs drop-in is staged");
        // Unset, the resume hook bakes the build host's swap device into every image.
        assert!(conf.contains("RESUME=none"), "{conf}");
        // Unset, the fsck hook probes a PARTUUID that no disk has yet, finds nothing,
        // and ships an initramfs with no fsck — so root goes unchecked at every boot.
        // The type is the formatter's, not a literal, so the helper matches the image.
        assert!(
            conf.contains(&format!("FSTYPE={}", crate::image::ROOTFS_FS_KIND)),
            "{conf}"
        );
        assert!(!tmp
            .path()
            .join(initramfs_conf_path(InitramfsGenerator::Dracut))
            .exists());
        assert!(!tmp.path().join(DRACUT_BUILTIN_FIRMWARE_MODULE).exists());
    }

    /// The dracut counterpart of the settings above: a generic initrd, which is what
    /// keeps dracut from sizing it to the build host, dated from the lock's epoch. Its
    /// `filesystems` list is exclusive, so the root's type must not be written there.
    #[test]
    fn a_dracut_image_states_a_generic_reproducible_initrd() {
        let mut build = rk1();
        build.image.as_mut().unwrap().initramfs_generator = InitramfsGenerator::Dracut;
        let tmp = tempfile::tempdir().unwrap();
        let sink = |_e: crate::event::Event| {};
        let step = Step::start(&sink, "test");
        stage_overlay(
            tmp.path(),
            &[],
            pair_of(&build),
            PARTITIONS,
            &ident(&build),
            1_790_347_034,
            &step,
        )
        .unwrap();
        let conf = std::fs::read_to_string(
            tmp.path()
                .join(initramfs_conf_path(InitramfsGenerator::Dracut)),
        )
        .expect("the dracut drop-in is staged");
        assert_eq!(conf, "hostonly=\"no\"\nreproducible=\"yes\"\n");
        assert!(!tmp
            .path()
            .join(initramfs_conf_path(InitramfsGenerator::InitramfsTools))
            .exists());
        // The built-in drivers' firmware module, executable like dracut's own.
        let module = tmp.path().join(DRACUT_BUILTIN_FIRMWARE_MODULE);
        assert_eq!(
            std::fs::read_to_string(&module).expect("the firmware module is staged"),
            DRACUT_BUILTIN_FIRMWARE
        );
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&module).unwrap().permissions().mode() & 0o7777,
            0o755
        );
    }

    /// The dracut module installs each firmware file a built-in driver lists, in the
    /// first spelling the kernel's loader opens. The loader tries the bare name in every
    /// directory before any `.zst`, so a bare file under `/lib/firmware` wins over a
    /// `.zst` under `updates/`. A listed file the root does not have is a warning, and a
    /// failed install fails the module. dracut's two helpers are stand-ins that print
    /// their arguments.
    #[test]
    fn the_dracut_module_installs_the_firmware_built_in_drivers_list() {
        if !crate::hosttool::require(&["bash"]) {
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let sysroot = tmp.path().join("root");
        let kver = "7.3.1-1-arm64";
        let srcmods = sysroot.join("lib/modules").join(kver);
        std::fs::create_dir_all(&srcmods).unwrap();
        std::fs::write(
            srcmods.join("modules.builtin.modinfo"),
            "panthor.description=Panthor DRM driver\0\
             panthor.firmware=arm/mali/arch14.8/mali_csffw.bin\0\
             panthor.firmware=arm/mali/arch10.8/mali_csffw.bin\0\
             realtek.firmware=rtl_nic/rtl8261c.bin\0",
        )
        .unwrap();
        for file in [
            "lib/firmware/arm/mali/arch10.8/mali_csffw.bin.zst",
            "lib/firmware/arm/mali/arch10.8/mali_csffw.bin.xz",
            "lib/firmware/updates/rtl_nic/rtl8261c.bin.zst",
            "lib/firmware/rtl_nic/rtl8261c.bin",
        ] {
            let path = sysroot.join(file);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, "").unwrap();
        }
        let module = tmp.path().join("module-setup.sh");
        std::fs::write(&module, DRACUT_BUILTIN_FIRMWARE).unwrap();
        let run = |inst_status: u8| {
            let script = format!(
                "inst_simple() {{ echo \"inst $*\"; return {inst_status}; }}\n\
                 dwarn() {{ echo \"warn $*\"; }}\n\
                 . \"$1\"\n\
                 check || exit 9\n\
                 installkernel\n"
            );
            let out = Command::new("bash")
                .args(["-c", &script, "bash"])
                .arg(&module)
                .env("srcmods", &srcmods)
                .env("kernel", kver)
                .env("dracutsysrootdir", &sysroot)
                .output()
                .unwrap();
            (
                out.status.code(),
                String::from_utf8_lossy(&out.stdout).into_owned(),
            )
        };

        let (code, out) = run(0);
        assert_eq!(code, Some(0), "{out}");
        assert_eq!(
            out,
            "warn no firmware arm/mali/arch14.8/mali_csffw.bin for the built-in driver panthor\n\
             inst /lib/firmware/arm/mali/arch10.8/mali_csffw.bin.zst\n\
             inst /lib/firmware/rtl_nic/rtl8261c.bin\n"
        );

        let (code, out) = run(1);
        assert_eq!(code, Some(1), "{out}");
        assert!(!out.contains("rtl8261c"), "{out}");

        // A kernel without the list installs nothing and says so.
        std::fs::remove_file(srcmods.join("modules.builtin.modinfo")).unwrap();
        let (code, out) = run(0);
        assert_eq!(code, Some(0), "{out}");
        assert!(
            out.starts_with("warn no ") && out.contains("modules.builtin.modinfo"),
            "{out}"
        );
        assert!(!out.contains("inst "), "{out}");
    }

    /// Each time client's wait unit gets its bounded command, in the drop-in directory
    /// systemd reads for that unit. A drop-in for the other client's unit would
    /// configure a unit the image does not have and leave its own one unbounded.
    #[test]
    fn the_clock_wait_drop_in_bounds_the_time_clients_own_unit() {
        let sink = |_e: crate::event::Event| {};
        for (client, unit, command) in [
            (
                TimeClient::SystemdTimesyncd,
                "systemd-time-wait-sync.service",
                "ExecStart=-/usr/bin/timeout 45 /usr/lib/systemd/systemd-time-wait-sync\n",
            ),
            (
                TimeClient::Chrony,
                "chrony-wait.service",
                "ExecStart=-/usr/bin/chronyc -h 127.0.0.1,::1 waitsync 45 0.1 0.0 1\n",
            ),
        ] {
            let mut build = rk1();
            build.image.as_mut().unwrap().time_client = client;
            let tmp = tempfile::tempdir().unwrap();
            let step = Step::start(&sink, "test");
            stage_overlay(
                tmp.path(),
                &[],
                pair_of(&build),
                PARTITIONS,
                &ident(&build),
                1_790_347_034,
                &step,
            )
            .unwrap();
            let dropin = std::fs::read_to_string(
                tmp.path()
                    .join(format!("etc/systemd/system/{unit}.d/bounded.conf")),
            )
            .unwrap_or_else(|e| panic!("{client}: no drop-in for {unit}: {e}"));
            // The empty line clears the unit's own command; without it the drop-in's
            // would run after it rather than instead of it.
            assert!(dropin.contains("[Service]\nExecStart=\n"), "{dropin}");
            assert!(dropin.ends_with(command), "{client}: {dropin}");
            assert_eq!(clock_wait(client).unit, unit);
            assert!(command.contains(clock_wait(client).program), "{client}");
        }
    }

    /// netplan warns on every run about a configuration file anyone else can read, and
    /// git records no mode narrower than `0644`.
    #[test]
    fn netplan_files_are_staged_readable_by_root_alone() {
        use std::os::unix::fs::PermissionsExt;
        let layer = tempfile::tempdir().unwrap();
        let netplan = layer.path().join("etc/netplan");
        std::fs::create_dir_all(&netplan).unwrap();
        std::fs::write(netplan.join("50-wired.yaml"), "network: {version: 2}\n").unwrap();
        std::fs::write(layer.path().join("etc/issue"), "x\n").unwrap();
        let staging = tempfile::tempdir().unwrap();
        let sink = |_e: crate::event::Event| {};
        let step = Step::start(&sink, "test");
        let build = rk1();
        stage_overlay(
            staging.path(),
            &[layer.path().to_path_buf()],
            pair_of(&build),
            PARTITIONS,
            &ident(&build),
            1_790_347_034,
            &step,
        )
        .unwrap();
        let mode = |rel: &str| {
            std::fs::metadata(staging.path().join(rel))
                .unwrap()
                .permissions()
                .mode()
                & 0o7777
        };
        assert_eq!(mode("etc/netplan/50-wired.yaml"), 0o600);
        // The directory stays traversable, and nothing outside it is narrowed.
        assert_eq!(mode("etc/netplan"), 0o755);
        assert_eq!(mode("etc/issue"), 0o644);
    }

    #[test]
    fn the_timezone_is_the_localtime_symlink_and_nothing_else() {
        // forky's tzdata *deletes* /etc/timezone on configure ("Removing now unused
        // /etc/timezone") and reads the zone back by readlink-ing /etc/localtime, so
        // the symlink is the entire interface. Writing the file would be worse than
        // redundant: it would be removed, and it would be the thing a reader trusted.
        assert_eq!(
            config::localtime_target("America/New_York"),
            "/usr/share/zoneinfo/America/New_York"
        );

        let tmp = tempfile::tempdir().unwrap();
        let sink = |_e: crate::event::Event| {};
        let step = Step::start(&sink, "test");
        stage_preinstall_overlay(
            tmp.path(),
            &[],
            image_of(&rk1()),
            None,
            1_790_347_034,
            &step,
        )
        .unwrap();

        let link = tmp.path().join("etc/localtime");
        assert_eq!(
            std::fs::read_link(&link).unwrap(),
            Path::new("/usr/share/zoneinfo/UTC"),
            "staged as a symlink, which `cp -dR` copies as a link"
        );
        assert!(!tmp.path().join("etc/timezone").exists());
    }

    #[test]
    fn configured_ntp_servers_set_the_primary_list_and_leave_the_fallback_pool_alone() {
        // Setting FallbackNTP too would strand an image the moment it booted on a
        // network its configured server is not on. Debian compiles its pool into that
        // fallback, and timesyncd consults it whenever the primary yields nothing — so
        // writing only NTP= makes a configured server a preference that degrades.
        let conf = config::ntp_conf(
            TimeClient::SystemdTimesyncd,
            &["ntp.lan".to_string(), "192.168.1.1".to_string()],
        );
        assert!(
            conf.contains("NTP=ntp.lan 192.168.1.1\n"),
            "servers join with the space timesyncd splits on: {conf}"
        );
        // No *directive* sets it. Checked against the uncommented lines rather than the
        // whole file, since the drop-in explains in a comment why it leaves it alone.
        let directives: Vec<&str> = conf
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        assert!(
            !directives.iter().any(|l| l.starts_with("FallbackNTP")),
            "the compiled-in pool must survive as the fallback: {directives:?}"
        );
        assert!(
            conf.contains("[Time]"),
            "timesyncd reads the [Time] section"
        );
    }

    #[test]
    fn configured_ntp_servers_join_the_chrony_sources_as_trusted_preferences() {
        // Ubuntu's own pools use NTS, and in chrony's default `authselectmode mix` that
        // gives them `require trust`: an unauthenticated server then moves the clock only
        // while one of them is selectable. `require trust` on the configured server lets
        // it set the clock on its own, which is what a LAN server is named for.
        let conf = config::ntp_conf(
            TimeClient::Chrony,
            &["ntp.lan".to_string(), "fd00::1".to_string()],
        );
        let directives: Vec<&str> = conf
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#'))
            .collect();
        assert_eq!(
            directives,
            [
                "server ntp.lan iburst prefer require trust",
                "server fd00::1 iburst prefer require trust",
            ]
        );
        // A sources.d file may contain only source directives, each line ended.
        assert!(conf.ends_with('\n'), "{conf:?}");
        assert!(
            !directives.iter().any(|l| l.starts_with("authselectmode")),
            "the distribution's policy for its own sources stays as shipped"
        );
    }

    #[test]
    fn an_image_with_no_configured_ntp_server_stages_no_drop_in() {
        let sink = |_e: crate::event::Event| {};

        // Empty is not "no time source" — it is the distribution's own servers. Writing
        // an empty `NTP=` would say the opposite, so the file is absent instead.
        let bare = tempfile::tempdir().unwrap();
        let step = Step::start(&sink, "test");
        stage_preinstall_overlay(
            bare.path(),
            &[],
            image_of(&rk1()),
            None,
            1_790_347_034,
            &step,
        )
        .unwrap();
        assert!(
            !bare
                .path()
                .join("etc/systemd/timesyncd.conf.d/10-boot2deb.conf")
                .exists(),
            "an unconfigured image contains no timesyncd drop-in"
        );

        // Configured, the drop-in is written where timesyncd reads drop-ins — not as an
        // edit of timesyncd.conf, which is a systemd conffile a rewrite would make
        // every future upgrade prompt about.
        let configured = resolve_recipe(
            &repo_root(),
            "turing-rk1/media-accel-forky",
            &Overrides {
                ntp_servers: Some(vec!["ntp.lan".to_string()]),
                ..Overrides::default()
            },
        )
        .unwrap();
        let lan = tempfile::tempdir().unwrap();
        let step = Step::start(&sink, "test");
        stage_preinstall_overlay(
            lan.path(),
            &[],
            image_of(&configured),
            None,
            1_790_347_034,
            &step,
        )
        .unwrap();
        let conf = std::fs::read_to_string(
            lan.path()
                .join("etc/systemd/timesyncd.conf.d/10-boot2deb.conf"),
        )
        .expect("a configured image contains the drop-in");
        assert!(conf.contains("NTP=ntp.lan\n"), "{conf}");
    }

    #[test]
    fn only_a_board_with_a_keyboard_gets_a_keyboard_file() {
        let sink = |_e: crate::event::Event| {};

        // The C201 is a laptop: its layout is configured, and the file it is written to is
        // the one keyboard-configuration seeds its debconf answers from.
        let laptop = tempfile::tempdir().unwrap();
        let step = Step::start(&sink, "test");
        stage_preinstall_overlay(
            laptop.path(),
            &[],
            image_of(&c201()),
            None,
            1_790_347_034,
            &step,
        )
        .unwrap();
        let kb = std::fs::read_to_string(laptop.path().join("etc/default/keyboard")).unwrap();
        assert!(kb.contains("XKBLAYOUT=\"us\""));
        assert!(kb.contains("XKBMODEL=\"pc105\""));

        // The RK1 is headless. Writing a layout for a console nobody types at would be
        // a claim we cannot back; Debian's own default stands instead.
        let headless = tempfile::tempdir().unwrap();
        let step = Step::start(&sink, "test");
        stage_preinstall_overlay(
            headless.path(),
            &[],
            image_of(&rk1()),
            None,
            1_790_347_034,
            &step,
        )
        .unwrap();
        assert!(!headless.path().join("etc/default/keyboard").exists());
    }

    /// The pre-install overlay contains an executable placeholder for every program
    /// the generator's kernel hook and trigger reach, each a real shell script that
    /// logs its own name and answers `--version` with the generator's.
    #[test]
    fn the_preinstall_overlay_shadows_the_generator_s_programs() {
        use std::os::unix::fs::PermissionsExt;
        let sink = |_e: crate::event::Event| {};
        let step = Step::start(&sink, "test");
        let mut build = rk1();
        for (generator, programs) in [
            (
                InitramfsGenerator::InitramfsTools,
                &["update-initramfs"][..],
            ),
            (
                InitramfsGenerator::Dracut,
                &["dracut", "update-initramfs"][..],
            ),
        ] {
            build.image.as_mut().unwrap().initramfs_generator = generator;
            let tmp = tempfile::tempdir().unwrap();
            stage_preinstall_overlay(
                tmp.path(),
                &[],
                image_of(&build),
                None,
                1_700_000_000,
                &step,
            )
            .unwrap();
            let stubs = initramfs_stubs(generator);
            assert_eq!(stubs.len(), programs.len(), "{generator}");
            for (stub, program) in stubs.iter().zip(programs) {
                assert_eq!(*stub, format!("/usr/local/sbin/{program}"));
                let path = tmp.path().join(stub.trim_start_matches('/'));
                let text = std::fs::read_to_string(&path).unwrap();
                assert!(text.starts_with("#!/bin/sh\n"), "{text}");
                assert!(
                    text.contains(&format!("echo '{generator} boot2deb-placeholder'")),
                    "initramfs-tools' kernel hook skips an update-initramfs whose --version \
                     does not name initramfs-tools, which would suppress the call without \
                     recording it:\n{text}"
                );
                assert!(text.contains(&format!("echo \"{program} $*\"")), "{text}");
                assert!(text.contains(INITRAMFS_STUB_LOG), "{text}");
                assert_eq!(
                    std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                    0o755,
                    "a placeholder that is not executable is not on the PATH at all"
                );
            }
        }
    }

    #[test]
    fn write_staged_forces_0644_regardless_of_umask() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        write_staged(tmp.path(), "etc/hostname", "host\n").unwrap();
        let mode = std::fs::metadata(tmp.path().join("etc/hostname"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o644);
    }

    /// A directory beside the staging tree that a layer's symlink can point into,
    /// holding one file at a mode no staged file is given. A write or `chmod` that
    /// followed a link out of the tree shows up as a changed sentinel or a new entry.
    struct Outside {
        dir: PathBuf,
    }

    impl Outside {
        const CONTENT: &'static str = "outside\n";
        const MODE: u32 = 0o600;

        fn new(tmp: &Path) -> Self {
            use std::os::unix::fs::PermissionsExt;
            let dir = tmp.join("outside");
            std::fs::create_dir(&dir).unwrap();
            let sentinel = dir.join("sentinel");
            std::fs::write(&sentinel, Self::CONTENT).unwrap();
            std::fs::set_permissions(&sentinel, std::fs::Permissions::from_mode(Self::MODE))
                .unwrap();
            Outside { dir }
        }

        fn sentinel(&self) -> PathBuf {
            self.dir.join("sentinel")
        }

        fn assert_untouched(&self) {
            let names: Vec<_> = std::fs::read_dir(&self.dir)
                .unwrap()
                .map(|e| e.unwrap().file_name())
                .collect();
            assert_eq!(names, ["sentinel"], "nothing new appears outside the tree");
            let sentinel = self.sentinel();
            assert_eq!(std::fs::read_to_string(&sentinel).unwrap(), Self::CONTENT);
            assert_eq!(
                staged_mode(&sentinel),
                Self::MODE,
                "the sentinel's mode is its own"
            );
        }
    }

    fn staged_mode(path: &Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        path.symlink_metadata().unwrap().permissions().mode() & 0o7777
    }

    /// A layer that ships `etc` as a symlink — to a directory outside the tree, as a
    /// legitimate target-side absolute link would resolve on the build host — makes every
    /// generated path beneath it an error. Nothing is written, linked, or re-moded through
    /// the link, and the layer's link itself stays as the layer shipped it.
    #[test]
    fn a_generated_path_never_resolves_through_a_layer_s_symlink() {
        use crate::error::StagedConflict;
        let tmp = tempfile::tempdir().unwrap();
        let outside = Outside::new(tmp.path());
        let staging = tmp.path().join("staging");
        std::fs::create_dir(&staging).unwrap();
        std::os::unix::fs::symlink(&outside.dir, staging.join("etc")).unwrap();

        let attempts: [(&str, Result<(), EngineError>); 4] = [
            (
                "etc/sentinel",
                write_staged(&staging, "etc/sentinel", "generated\n"),
            ),
            (
                "etc/boot2deb/board.conf",
                write_staged(&staging, "etc/boot2deb/board.conf", "generated\n"),
            ),
            (
                "etc/sentinel",
                write_staged_exec(&staging, "etc/sentinel", "#!/bin/sh\n"),
            ),
            (
                "etc/localtime",
                symlink_staged(&staging, "etc/localtime", "/usr/share/zoneinfo/UTC"),
            ),
        ];
        for (rel, result) in attempts {
            match result {
                Err(EngineError::StagedPathBlocked {
                    generated,
                    entry,
                    conflict: StagedConflict::SymlinkAncestor,
                }) => {
                    assert_eq!(generated, rel);
                    assert_eq!(entry, "etc", "the error names the layer's link");
                }
                other => panic!("{rel}: expected a blocked path, got {other:?}"),
            }
        }
        outside.assert_untouched();
        assert_eq!(
            std::fs::read_link(staging.join("etc")).unwrap(),
            outside.dir
        );

        let message = write_staged(&staging, "etc/hostname", "board\n")
            .unwrap_err()
            .to_string();
        assert_eq!(
            message,
            "cannot stage the generated /etc/hostname: /etc in the layer overlays is a \
             symlink, which a generated path is never resolved through"
        );
    }

    /// Generated config wins over the layers, so a layer's symlink at the generated path
    /// itself is replaced — by a regular file at the generated mode, inside the tree —
    /// rather than written through. A dangling link is the sharpest case: a write that
    /// followed it would create its target outside the tree.
    #[test]
    fn a_generated_file_replaces_a_layer_s_symlink_without_following_it() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = Outside::new(tmp.path());
        let staging = tmp.path().join("staging");
        std::fs::create_dir_all(staging.join("etc")).unwrap();
        std::fs::create_dir_all(staging.join("usr/local/sbin")).unwrap();
        std::os::unix::fs::symlink(outside.sentinel(), staging.join("etc/hostname")).unwrap();
        std::os::unix::fs::symlink(
            outside.dir.join("dangling"),
            staging.join("etc/kernel-img.conf"),
        )
        .unwrap();
        std::os::unix::fs::symlink(
            outside.sentinel(),
            staging.join("usr/local/sbin/update-initramfs"),
        )
        .unwrap();

        write_staged(&staging, "etc/hostname", "board\n").unwrap();
        write_staged(&staging, "etc/kernel-img.conf", "do_symlinks = no\n").unwrap();
        write_staged_exec(&staging, "usr/local/sbin/update-initramfs", "#!/bin/sh\n").unwrap();

        for (rel, content, mode) in [
            ("etc/hostname", "board\n", 0o644),
            ("etc/kernel-img.conf", "do_symlinks = no\n", 0o644),
            ("usr/local/sbin/update-initramfs", "#!/bin/sh\n", 0o755),
        ] {
            let path = staging.join(rel);
            assert!(
                path.symlink_metadata().unwrap().is_file(),
                "{rel} is a file"
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), content, "{rel}");
            assert_eq!(staged_mode(&path), mode, "{rel}");
        }
        outside.assert_untouched();
    }

    /// The same rule for a generated symlink: a layer's symlink or file at its path is
    /// replaced by the generated link, and the old link's target is never touched.
    #[test]
    fn a_generated_symlink_replaces_a_layer_s_entry_without_following_it() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = Outside::new(tmp.path());
        let staging = tmp.path().join("staging");
        let wants = staging.join("etc/systemd/system/multi-user.target.wants");
        std::fs::create_dir_all(&wants).unwrap();
        std::os::unix::fs::symlink(outside.sentinel(), staging.join("etc/localtime")).unwrap();
        std::fs::write(wants.join("boot2deb-selftest.service"), "a layer's file\n").unwrap();

        symlink_staged(&staging, "etc/localtime", "/usr/share/zoneinfo/UTC").unwrap();
        symlink_staged(
            &staging,
            "etc/systemd/system/multi-user.target.wants/boot2deb-selftest.service",
            "../boot2deb-selftest.service",
        )
        .unwrap();

        assert_eq!(
            std::fs::read_link(staging.join("etc/localtime")).unwrap(),
            Path::new("/usr/share/zoneinfo/UTC")
        );
        assert_eq!(
            std::fs::read_link(wants.join("boot2deb-selftest.service")).unwrap(),
            Path::new("../boot2deb-selftest.service")
        );
        outside.assert_untouched();
    }

    /// What a generated entry cannot win over is refused, naming the overlay entry in its
    /// way: a directory at the generated path (replacing it would discard what the layers
    /// placed beneath it), a file where the path needs a directory, and a path that would
    /// leave the tree on its own.
    #[test]
    fn a_generated_path_blocked_by_the_layers_is_refused() {
        use crate::error::StagedConflict;
        let tmp = tempfile::tempdir().unwrap();
        let staging = tmp.path().join("staging");
        std::fs::create_dir_all(staging.join("etc/hostname")).unwrap();
        std::fs::write(staging.join("etc/hostname/kept"), "a layer's file\n").unwrap();
        std::fs::write(staging.join("etc/boot2deb"), "a layer's file\n").unwrap();

        let blocked = |result: Result<(), EngineError>| match result {
            Err(EngineError::StagedPathBlocked {
                entry, conflict, ..
            }) => (entry, conflict),
            other => panic!("expected a blocked path, got {other:?}"),
        };
        assert_eq!(
            blocked(write_staged(&staging, "etc/hostname", "board\n")),
            ("etc/hostname".to_string(), StagedConflict::Directory)
        );
        assert_eq!(
            blocked(symlink_staged(&staging, "etc/hostname", "/run/hostname")),
            ("etc/hostname".to_string(), StagedConflict::Directory)
        );
        assert_eq!(
            blocked(write_staged(&staging, "etc/boot2deb/board.conf", "x\n")),
            (
                "etc/boot2deb".to_string(),
                StagedConflict::NonDirectoryAncestor
            )
        );
        assert_eq!(
            std::fs::read_to_string(staging.join("etc/hostname/kept")).unwrap(),
            "a layer's file\n"
        );
        assert_eq!(
            std::fs::read_to_string(staging.join("etc/boot2deb")).unwrap(),
            "a layer's file\n"
        );

        for rel in ["../escape", "/etc/escape", "etc/../escape", ""] {
            assert!(
                matches!(
                    write_staged(&staging, rel, "x\n"),
                    Err(EngineError::Io { .. })
                ),
                "{rel:?} is refused"
            );
        }
        assert!(!tmp.path().join("escape").exists());
        assert!(!staging.join("escape").exists());
    }

    /// Narrowing `/etc/netplan` to root-only touches only files the staging tree
    /// contains. A layer's symlink at the directory, or at an ancestor of it, names a
    /// directory on the target, so the files it reaches on the build host keep their mode.
    #[test]
    fn root_only_narrowing_never_follows_a_layer_s_symlink() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let outside = Outside::new(tmp.path());
        let open = std::fs::Permissions::from_mode(0o644);
        let narrowed = |link: &str| {
            let staging = tmp.path().join(link.replace('/', "-"));
            std::fs::create_dir_all(staging.join("etc")).unwrap();
            std::os::unix::fs::symlink(&outside.dir, staging.join(link)).unwrap();
            restrict_root_only_files(&staging).unwrap();
        };

        std::fs::set_permissions(outside.sentinel(), open.clone()).unwrap();
        narrowed("etc/netplan");
        assert_eq!(staged_mode(&outside.sentinel()), 0o644);

        std::fs::create_dir(outside.dir.join("netplan")).unwrap();
        let reached = outside.dir.join("netplan/10-board.yaml");
        std::fs::write(&reached, "network: {}\n").unwrap();
        std::fs::set_permissions(&reached, open).unwrap();
        let staging = tmp.path().join("linked-etc");
        std::fs::create_dir(&staging).unwrap();
        std::os::unix::fs::symlink(&outside.dir, staging.join("etc")).unwrap();
        restrict_root_only_files(&staging).unwrap();
        assert_eq!(staged_mode(&reached), 0o644);

        // The staged file itself is still narrowed.
        let staging = tmp.path().join("real");
        std::fs::create_dir_all(staging.join("etc/netplan")).unwrap();
        let staged = staging.join("etc/netplan/10-board.yaml");
        std::fs::write(&staged, "network: {}\n").unwrap();
        restrict_root_only_files(&staging).unwrap();
        assert_eq!(staged_mode(&staged), 0o600);
    }

    /// The overlay merge never writes through an earlier layer's symlink. A later layer's
    /// file replaces the link (`--remove-destination` unlinks it first), and a later
    /// layer's directory over the link makes `cp` refuse, so the merge fails rather than
    /// writing into whatever the link reaches on the build host.
    #[test]
    fn the_overlay_merge_never_writes_through_an_earlier_layer_s_symlink() {
        let tmp = tempfile::tempdir().unwrap();
        let outside = Outside::new(tmp.path());
        let sink = |_: crate::event::Event| {};
        let step = Step::start(&sink, "overlay");
        let layer = |name: &str| {
            let dir = tmp.path().join(name);
            std::fs::create_dir_all(dir.join("etc")).unwrap();
            dir
        };

        let (base, device) = (layer("file-base"), layer("file-device"));
        std::os::unix::fs::symlink(outside.sentinel(), base.join("etc/hostname")).unwrap();
        std::fs::write(device.join("etc/hostname"), "device\n").unwrap();
        let staging = tmp.path().join("file-staging");
        copy_overlay_trees(&staging, &[base, device], &step).unwrap();
        let merged = staging.join("etc/hostname");
        assert!(merged.symlink_metadata().unwrap().is_file());
        assert_eq!(std::fs::read_to_string(&merged).unwrap(), "device\n");
        outside.assert_untouched();

        let (base, device) = (tmp.path().join("dir-base"), layer("dir-device"));
        std::fs::create_dir(&base).unwrap();
        std::os::unix::fs::symlink(&outside.dir, base.join("etc")).unwrap();
        std::fs::write(device.join("etc/sentinel"), "device\n").unwrap();
        let staging = tmp.path().join("dir-staging");
        assert!(copy_overlay_trees(&staging, &[base, device], &step).is_err());
        outside.assert_untouched();
    }
}
