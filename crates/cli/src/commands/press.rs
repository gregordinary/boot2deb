//! `press`: produce a distributable image file from a build's artifacts —
//! verified, per-unit personalized, and optionally extended with per-site files.
//!
//! boot2deb does not write devices. The pressed file is what gets handed to a
//! flasher (`dd`, pyrographer, a vendor tool). Two paths produce it, chosen per
//! output:
//!
//! - A press with nothing to add **streams** the existing compressed artifact,
//!   decompressing through a digest tap and verifying. A plain card must not cost
//!   a rebuild.
//! - A press with tree additions, or with a fresh identity, **re-assembles** the image
//!   from the kept rootfs tar, through [`boot2deb_engine::image::press_image`].
//!
//! Either way the seed keys are written last, into the finished file, and a rootfs file
//! manifest is written beside it as `<output stem>.rootfs.uapi16`. A streamed press copies
//! the build's, since its rootfs is the build's to the byte. A re-assembly formats a new
//! rootfs and writes its own, with a first-boot password of its own in
//! `<output stem>.credentials.toml`, readable by its owner only. The seed partition sits
//! outside the rootfs, so the keys leave either listing true.
//!
//! **What a press reads is the build's record, not the directory.** Every input comes
//! through the point's artifact inventory ([`boot2deb_core::inventory`]). The entry of the
//! stage that published it must be the current lock's. The file must still hold the bytes
//! the entry records.
//!
//! A streamed image is checked twice on the way through, as stored and as the disk it
//! decompresses to. Both checks are against digests the build recorded, not
//! ones the press computed. A file named like an artifact that the inventory does not
//! list is never pressed.
//!
//! **What a press writes is checked as one set** before anything is written:
//!
//! - Each output, and each file written beside it, is a regular file or absent.
//!   `--overwrite` replaces one that exists.
//! - No two of them name the same file.
//! - None of them is, or links to, anything the press reads, or one of the build's own
//!   names in its artifact directory.
//!
//! Each is produced under a temporary name in its own directory. Every one is renamed into
//! place only once all of them are produced, verified and personalized. A failed press
//! therefore leaves no partial output, and never writes into an existing file.
//!
//! What this module handles is config:
//!
//! - Which build point's artifacts these are.
//! - How the resolved roles bind to output paths: one positional for one artifact,
//!   or `--boot-out` and `--rootfs-out` for a split build.
//! - Validating the seed keys, additions and outputs before anything is written.
//!
//! The engine handles the bytes.

use crate::args::{PressArgs, SeedKeyArgs};
use crate::fsutil::absolutize;
use crate::render::{print_event_at, Verbosity};
use boot2deb_core::inventory::{role, Inventory, InventoryFile, Stage, StageEntry};
use boot2deb_core::model::{Layout, Overrides};
use boot2deb_core::press::{roles, ArtifactRole};
use boot2deb_core::{resolve_recipe, ConfigRoot, ResolvedBuild};
use boot2deb_engine::event::{Event, Step};
use boot2deb_engine::image::{press_image, BootPayload, ImageIdentity, PressOptions};
use boot2deb_engine::press::additions::TreeAdditions;
use boot2deb_engine::press::identity::PressIdentity;
use boot2deb_engine::press::seed::SeedKeys;
use boot2deb_engine::press::{seed, verify, write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

/// Run `press <recipe> [output] [...]`.
pub(crate) fn run(
    root: &ConfigRoot,
    recipe: &str,
    output: Option<PathBuf>,
    args: PressArgs,
    verbosity: Verbosity,
) -> Result<()> {
    let point = crate::config::build_point(recipe, Vec::new())?;
    let reference = point.reference();
    let recipe = reference.as_str();
    let stem = point.artifact_stem();

    let overrides = Overrides {
        layout: args.layout,
        ..Overrides::default()
    };
    let resolved = resolve_recipe(root, recipe, &overrides)?;
    let roles = roles(&resolved);

    let work_dir = crate::workdir::work_dir_for(root, recipe, args.work_dir.clone());
    let out_dir = absolutize(
        args.out_dir
            .clone()
            .unwrap_or_else(|| work_dir.join("artifacts")),
    );

    // The identifiers the build derived for this recipe, and the ones the pressed image
    // contains: the re-assembly stamps them into the GPT and the superblock, and a
    // template addition names them before any disk contains them. They are the build's
    // unless the press draws fresh ones for its medium. `--embed-image` implies that,
    // because its card stays in the machine beside the disk it installs, and two disks
    // there must not answer to one root PARTUUID.
    let built = ImageIdentity::derive(recipe, &resolved.device);
    let identity = if args.fresh_identity || args.embed_image {
        PressIdentity::Fresh {
            built,
            fresh: ImageIdentity::fresh(&resolved.device)?,
        }
    } else {
        PressIdentity::Built(built)
    };

    let keys = seed_keys(&args.keys)?;
    // What the press reads: the build's artifacts as its inventory records them, under
    // the lock the recipe has now. A dry run checks sizes; a press hashes.
    let inventory = crate::inventory::load(&out_dir, &stem)?;
    let sources = Sources {
        inventory: &inventory,
        lock_sha256: crate::inventory::lock_digest(root, recipe)?,
        out_dir: &out_dir,
        stem: &stem,
        recipe,
        layout: resolved.layout,
        image_build: resolved.produces_image(),
        hash: !args.dry_run,
    };
    let (additions, embedded) = collect_additions(
        &args,
        &resolved,
        recipe,
        &stem,
        &identity,
        keys.as_ref().and_then(|k| k.hostname.clone()),
        &sources,
    )?;

    // Seed keys and tree additions ride the rootfs; a build whose only artifact
    // is a boot image has nowhere to put either.
    if !roles.iter().any(|r| r.contains_rootfs()) {
        if keys.is_some() {
            return Err("this build presses only a boot image, which has no seed \
                        partition — seed keys do not apply"
                .into());
        }
        if !additions.is_empty() {
            return Err("this build presses only a boot image, which has no rootfs \
                        — tree additions do not apply"
                .into());
        }
        if identity.is_fresh() {
            return Err("this build presses only a boot image, which contains no \
                        partition table or rootfs — a fresh identity does not apply"
                .into());
        }
    }

    let plan: Vec<Planned> = bind_outputs(&roles, output, &args)?
        .into_iter()
        .map(|(role, out)| Planned::new(role, absolutize(out), &additions, &identity))
        .collect();
    let protected = protected_inputs(&sources, &stem, &args, embedded.as_deref())?;
    preflight(&plan, &protected, args.overwrite)?;

    if args.dry_run {
        return dry_run(&plan, &sources, &keys, &additions, &identity);
    }

    let sink = move |e: Event| print_event_at(verbosity, &e);
    let mut staged = Staged::default();
    for p in &plan {
        if p.reassemble {
            reassemble(
                &resolved,
                p,
                &sources,
                &work_dir,
                &args,
                &additions,
                identity,
                &mut staged,
                &sink,
            )?;
        } else {
            stream(p, &sources, &args, &mut staged, &sink)?;
        }
        // Personalization is applied to the finished file, whichever path made it —
        // and only to the artifact that contains the seed.
        if p.role.contains_rootfs() {
            if let Some(keys) = &keys {
                seed::rewrite_seed(staged.temp(&p.out), keys, now_secs())?;
                let step = Step::start(&sink, "press");
                step.log(format!(
                    "personalized {}: {}",
                    p.out.display(),
                    describe_keys(keys)
                ));
                step.finish();
            }
        }
    }
    // Every output is produced, verified and personalized: only now does any of it
    // take its name.
    let published = staged.commit()?;
    let step = Step::start(&sink, "press");
    for path in published {
        step.log(format!("published {}", path.display()));
    }
    step.finish();
    Ok(())
}

/// Where a press reads from: the point's artifact inventory, held to the current lock.
struct Sources<'a> {
    /// The point's inventory.
    inventory: &'a Inventory,
    /// The digest of the lock the recipe has now.
    lock_sha256: String,
    /// The build's artifact directory.
    out_dir: &'a Path,
    /// The point's artifact stem.
    stem: &'a str,
    /// The point's reference, for the remedies errors name.
    recipe: &'a str,
    /// The layout the recipe resolves to under this press's `--layout`.
    layout: Layout,
    /// Whether the build produces an image; a u-boot deliverable's one artifact is the
    /// u-boot stage's boot image.
    image_build: bool,
    /// Whether to hash each input against its record (a press), or only check its size
    /// (a dry run, which reads nothing it does not have to).
    hash: bool,
}

impl Sources<'_> {
    /// `stage`'s entry, current under the recipe's lock.
    fn entry(&self, stage: Stage) -> Result<&StageEntry> {
        crate::inventory::current(
            self.inventory,
            stage,
            &self.lock_sha256,
            self.out_dir,
            self.recipe,
        )
    }

    /// The image stage's entry, current and laid out as the press resolves.
    fn images(&self) -> Result<&StageEntry> {
        let entry = self.entry(Stage::Image)?;
        match entry.layout {
            Some(built) if built == self.layout => Ok(entry),
            built => Err(format!(
                "the build laid the image out {}, and this press resolves {} — press with \
                 the build's --layout, or rebuild",
                built.map_or_else(|| "with no recorded layout".to_string(), |l| l.to_string()),
                self.layout
            )
            .into()),
        }
    }

    /// The disk image a build published for `role`, from the stage that publishes it.
    fn disk_image(&self, role: ArtifactRole) -> Result<&InventoryFile> {
        let entry = if self.image_build {
            self.images()?
        } else {
            self.entry(Stage::Uboot)?
        };
        entry.disk_image(role).ok_or_else(|| {
            format!(
                "the build published no {} — run `boot2deb build {}`",
                role.describe(),
                self.recipe
            )
            .into()
        })
    }

    /// `row`'s path, once it holds what the inventory records: hashed for a press, sized
    /// for a dry run.
    fn take(&self, row: &InventoryFile) -> Result<PathBuf> {
        if self.hash {
            crate::inventory::verified(self.out_dir, row)
        } else {
            crate::inventory::present(self.out_dir, row)
        }
    }

    /// The file `stage` published under `name`, taken as [`take`](Self::take) does.
    fn published(&self, stage: Stage, name: &str) -> Result<PathBuf> {
        let entry = match stage {
            Stage::Image => self.images()?,
            other => self.entry(other)?,
        };
        let row = entry.file(name).ok_or_else(|| {
            format!(
                "the {} stage's inventory entry lists no {name} — rebuild it",
                stage.as_str()
            )
        })?;
        self.take(row)
    }
}

/// One output the press produces: which artifact it derives from, how, and every file it
/// writes.
struct Planned {
    /// The artifact this output derives from.
    role: ArtifactRole,
    /// The image file, as named (made absolute).
    out: PathBuf,
    /// Whether it is re-assembled rather than streamed.
    reassemble: bool,
    /// Its rootfs file manifest, `<out stem>.rootfs.uapi16`, where the role contains a
    /// rootfs.
    listing: Option<PathBuf>,
    /// Its private credential document, `<out stem>.credentials.toml`, where it is
    /// re-assembled with a password of its own.
    credential: Option<PathBuf>,
}

impl Planned {
    /// The plan for `role` written to `out`.
    fn new(
        role: ArtifactRole,
        out: PathBuf,
        additions: &TreeAdditions,
        identity: &PressIdentity,
    ) -> Self {
        let reassemble = reassembles(role, additions, identity);
        Self {
            role,
            listing: role
                .contains_rootfs()
                .then(|| out.with_extension("rootfs.uapi16")),
            credential: reassemble.then(|| out.with_extension("credentials.toml")),
            reassemble,
            out,
        }
    }

    /// Every file this output writes.
    fn destinations(&self) -> impl Iterator<Item = &Path> {
        std::iter::once(self.out.as_path())
            .chain(self.listing.as_deref())
            .chain(self.credential.as_deref())
    }
}

/// The stream path: decompress the existing artifact into the output file with a
/// digest tap, then re-read and verify — the fast path a plain card takes.
fn stream(
    p: &Planned,
    sources: &Sources,
    args: &PressArgs,
    staged: &mut Staged,
    sink: &impl Fn(Event),
) -> Result<()> {
    let row = sources.disk_image(p.role)?;
    let disk = row
        .disk
        .as_ref()
        .expect("a disk image's inventory row records its disk");
    // Sized now; its digest is taken by the tap as it streams, which spares a second read
    // of the whole container.
    let artifact = crate::inventory::present(sources.out_dir, row)?;
    // Found before anything is written, so a press that cannot give its output a
    // listing fails without leaving an unlisted image behind.
    let listing = p
        .listing
        .as_ref()
        .map(|_| sources.published(Stage::Image, role::FILES_MANIFEST))
        .transpose()?;
    let step = Step::start(sink, "press");
    step.log(format!(
        "{}: {} -> {}",
        p.role.describe(),
        artifact.display(),
        p.out.display()
    ));
    let tmp = staged.reserve(&p.out)?;
    let written = {
        let mut dest = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&tmp)
            .map_err(|e| format!("cannot create {} ({e})", tmp.display()))?;
        let written = write::stream_image(&artifact, &mut dest, &step)?;
        dest.sync_all()?;
        written
    };
    // Held to what the build recorded, not to what this press read: the file as stored,
    // and the disk it decompressed to.
    if written.source_bytes != row.size || written.source_sha256 != row.sha256 {
        return Err(format!(
            "{} is not the file its build recorded (sha256 {}, recorded {}) — it was \
             changed after the build; rebuild",
            artifact.display(),
            short(&written.source_sha256),
            short(&row.sha256)
        )
        .into());
    }
    if written.bytes != disk.bytes || written.sha256 != disk.sha256 {
        return Err(format!(
            "{} decompressed to a {}-byte disk with sha256 {}, and its build recorded {} \
             bytes with sha256 {} — rebuild",
            artifact.display(),
            written.bytes,
            short(&written.sha256),
            disk.bytes,
            short(&disk.sha256)
        )
        .into());
    }
    step.log(format!(
        "wrote {} bytes, the disk the build recorded",
        written.bytes
    ));
    if !args.no_verify {
        step.log("verifying: re-reading what was written");
        verify::verify_digest(&tmp, &written, &step)?;
        // Every artifact that contains a rootfs contains a table, and an unreadable one
        // is a failure. Only the boot image has none to compare.
        if p.role.contains_rootfs() {
            let planned = verify::planned_table(&artifact)?;
            let read_back = verify::read_back_table(&tmp)?;
            verify::compare_tables(&p.out.display().to_string(), &planned, &read_back)?;
            step.log(format!(
                "verified: digest matches, partition table matches ({} entries)",
                planned.len()
            ));
        } else {
            step.log("verified: digest matches (a boot image has no partition table)");
        }
    }
    if let (Some(source), Some(dest)) = (listing, &p.listing) {
        let tmp = staged.reserve(dest)?;
        std::fs::copy(&source, &tmp)
            .map_err(|e| format!("cannot copy {} ({e})", source.display()))?;
        step.log(format!(
            "listed: the build's rootfs file manifest, published as {}",
            dest.display()
        ));
    }
    if p.role.contains_rootfs() {
        // The stream is the build's image to the byte, password included, and the
        // build's credential document is the only place that password is.
        step.log(format!(
            "first-boot pw: the build's, in {}",
            sources
                .out_dir
                .join(boot2deb_core::credential::credential_name(sources.stem))
                .display()
        ));
    }
    step.finish();
    Ok(())
}

/// Whether this output is re-assembled rather than streamed: it contains a rootfs, and
/// the press either adds to that rootfs or gives the image a fresh identity. Either one
/// changes bytes the build's artifact fixes, so neither can stream.
fn reassembles(role: ArtifactRole, additions: &TreeAdditions, identity: &PressIdentity) -> bool {
    role.contains_rootfs() && (!additions.is_empty() || identity.is_fresh())
}

/// The re-assembly path: rebuild the image from the kept rootfs tar with the additions
/// merged in, and the identity it contains stamped, writing the output's temporary file
/// directly.
#[allow(clippy::too_many_arguments)] // one call site; the args are the press itself
fn reassemble(
    resolved: &ResolvedBuild,
    p: &Planned,
    sources: &Sources,
    work_dir: &Path,
    args: &PressArgs,
    additions: &TreeAdditions,
    identity: PressIdentity,
    staged: &mut Staged,
    sink: &impl Fn(Event),
) -> Result<()> {
    let rootfs_tar = sources.published(Stage::Rootfs, role::ROOTFS_TAR)?;
    boot2deb_engine::rootfs::validate_tar(&rootfs_tar)?;

    // The boot payload, only where this output places one: a combined image contains
    // it, a split rootfs image is bootloader-agnostic. The u-boot stage's, through its
    // entry, under the same lock as the tar.
    let payloads = match (p.role, &resolved.boot) {
        (ArtifactRole::Combined, boot2deb_core::model::ResolvedBoot::RockchipRkbin(_)) => Some((
            sources.published(Stage::Uboot, role::IDBLOADER)?,
            sources.published(Stage::Uboot, role::UBOOT_ITB)?,
        )),
        _ => None,
    };
    let boot = match (p.role, &resolved.boot, &payloads) {
        (ArtifactRole::Combined, _, Some((idbloader, uboot_itb))) => {
            Some(BootPayload::RockchipRkbin {
                idbloader,
                uboot_itb,
            })
        }
        (ArtifactRole::Combined, boot2deb_core::model::ResolvedBoot::Depthcharge(_), None) => {
            Some(BootPayload::Depthcharge)
        }
        _ => None,
    };

    // A re-assembly rebuilds a rootfs into a new image, so it needs the image axis; a
    // u-boot deliverable never reaches here — `roles` gives it the boot role alone, and
    // this path is only entered for a role that contains a rootfs.
    let image = resolved.as_image().ok_or(
        "a u-boot deliverable has no rootfs to re-assemble; press streams its boot artifact",
    )?;
    let out = staged.reserve(&p.out)?;
    let listing = staged.reserve(p.listing.as_ref().expect("a rootfs-bearing output"))?;
    let pressed = press_image(
        image,
        &PressOptions {
            rootfs_tar: &rootfs_tar,
            boot,
            role: p.role,
            output: &out,
            files_manifest: &listing,
            work_dir: &work_dir.join("press"),
            rootfs_label: &args.rootfs_label,
            identity,
            additions,
        },
        sink,
    )?;
    let step = Step::start(sink, "press");
    let root = identity.pressed().rootfs_partuuid.hyphenated().to_string();
    if !args.no_verify {
        // The rootfs was already scan-verified inside the assembly; what is left
        // to hold is the disk around it, whose table must read back whole and root
        // on the PARTUUID this press gave it.
        let table = verify::read_back_table(&out)?;
        if !table.iter().any(|entry| entry.part_guid == root) {
            return Err(format!(
                "the pressed image's partition table contains no partition with the root \
                 PARTUUID {root} it was pressed with"
            )
            .into());
        }
        step.log(format!(
            "verified: the pressed image contains a readable GPT ({} entries) rooting on {root}",
            table.len()
        ));
    }
    if let PressIdentity::Fresh { built, .. } = identity {
        step.log(format!(
            "identity: drawn for this medium, root PARTUUID {root} (the build's is {})",
            built.rootfs_partuuid.hyphenated()
        ));
    }
    // The pressed file's own credential exists nowhere else, so it goes to its own
    // private file, and the log names the file rather than the secret.
    let credential = p.credential.as_ref().expect("a re-assembled output");
    let tmp = staged.reserve(credential)?;
    boot2deb_engine::secret::write_private(
        &tmp,
        boot2deb_core::credential::FirstBootCredential::new(
            &file_name(&p.out),
            &image.image.user,
            &pressed.password,
        )
        .to_toml()
        .as_bytes(),
    )?;
    step.log(format!(
        "first-boot pw: {} (user {}, readable by you only; the password is expired — \
         change it at first login)",
        credential.display(),
        image.image.user
    ));
    step.finish();
    Ok(())
}

/// Something a press reads or the build owns, which no output may be or link to.
enum Protected {
    /// One file, held by path and by inode.
    File {
        /// The file.
        path: PathBuf,
        /// What it is, for the refusal.
        what: String,
    },
    /// Every file under a directory (`--copy-tree`).
    Tree {
        /// The directory.
        dir: PathBuf,
    },
    /// The build's own files in its artifact directory: every name the point's stem
    /// starts.
    Artifacts {
        /// The artifact directory.
        dir: PathBuf,
        /// The point's artifact stem.
        stem: String,
    },
}

/// What the press reads and the build owns: every file the inventory lists, the build's
/// own names in its artifact directory, the embedded artifact, and every `--copy`,
/// `--deb` and `--copy-tree` source.
fn protected_inputs(
    sources: &Sources,
    stem: &str,
    args: &PressArgs,
    embedded: Option<&Path>,
) -> Result<Vec<Protected>> {
    let mut protected = vec![Protected::Artifacts {
        dir: sources.out_dir.to_path_buf(),
        stem: stem.to_string(),
    }];
    for name in sources.inventory.artifact_names() {
        protected.push(Protected::File {
            path: sources.out_dir.join(name),
            what: format!("artifact {name}"),
        });
    }
    if let Some(path) = embedded {
        protected.push(Protected::File {
            path: path.to_path_buf(),
            what: "embedded artifact".into(),
        });
    }
    for spec in &args.copy {
        if let Some((src, _)) = spec.split_once(':') {
            protected.push(Protected::File {
                path: absolutize(PathBuf::from(src)),
                what: format!("--copy source {src}"),
            });
        }
    }
    for deb in &args.debs {
        protected.push(Protected::File {
            path: absolutize(deb.clone()),
            what: format!("--deb {}", deb.display()),
        });
    }
    for dir in &args.copy_tree {
        protected.push(Protected::Tree {
            dir: absolutize(dir.clone()),
        });
    }
    Ok(protected)
}

/// Hold every file the plan writes, as one set, to what a press may write, before any of
/// it is written.
///
/// A destination must be a regular file or absent: a symlink is not written through, and
/// a device, directory or FIFO is not a file to press to. One that exists is replaced
/// only under `overwrite`. No two destinations may name the same file, by path or by
/// link. None may be, link to, or sit under anything in `protected`.
fn preflight(plan: &[Planned], protected: &[Protected], overwrite: bool) -> Result<()> {
    let mut seen: Vec<(PathBuf, &Path)> = Vec::new();
    for dest in plan.iter().flat_map(Planned::destinations) {
        let canonical = canonical_destination(dest)?;
        let existing = match std::fs::symlink_metadata(dest) {
            Ok(meta) => Some(meta),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(e) => return Err(format!("cannot inspect {} ({e})", dest.display()).into()),
        };
        if let Some(meta) = &existing {
            let kind = meta.file_type();
            if kind.is_symlink() {
                return Err(format!(
                    "{} is a symlink — a press writes files of its own and does not write \
                     through links; name the file itself",
                    dest.display()
                )
                .into());
            }
            if !kind.is_file() {
                return Err(format!(
                    "{} is {}, not a regular file. boot2deb does not write devices — press \
                     to a file and hand that to your flasher",
                    dest.display(),
                    describe_kind(meta)
                )
                .into());
            }
            if !overwrite {
                return Err(format!(
                    "{} already exists — pass --overwrite to replace it (it is replaced \
                     whole once the new file is verified, never written into)",
                    dest.display()
                )
                .into());
            }
        }
        if let Some((_, other)) = seen.iter().find(|(c, _)| c == &canonical) {
            return Err(format!(
                "{} and {} name the same file — every output needs a path of its own",
                dest.display(),
                other.display()
            )
            .into());
        }
        for p in protected {
            if let Some(what) = collides(p, dest, &canonical, existing.as_ref()) {
                return Err(format!(
                    "{} would replace {what} — press to a path of its own",
                    dest.display()
                )
                .into());
            }
        }
        seen.push((canonical, dest));
    }
    Ok(())
}

/// What `dest` would replace of `p`, or `None` when it is clear of it. `canonical` is
/// its canonical path, and `existing` its metadata when it exists.
fn collides(
    p: &Protected,
    dest: &Path,
    canonical: &Path,
    existing: Option<&std::fs::Metadata>,
) -> Option<String> {
    match p {
        Protected::File { path, what } => {
            let same_path = std::fs::canonicalize(path).is_ok_and(|c| c == canonical);
            let same_inode = match (existing, std::fs::metadata(path)) {
                (Some(d), Ok(m)) => d.dev() == m.dev() && d.ino() == m.ino(),
                _ => false,
            };
            (same_path || same_inode).then(|| format!("the press's {what} ({})", path.display()))
        }
        Protected::Tree { dir } => std::fs::canonicalize(dir)
            .is_ok_and(|d| canonical.starts_with(&d))
            .then(|| format!("a file under the --copy-tree source {}", dir.display())),
        Protected::Artifacts { dir, stem } => {
            let in_dir =
                std::fs::canonicalize(dir).is_ok_and(|d| canonical.parent() == Some(d.as_path()));
            let name = dest.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let owned =
                name.starts_with(&format!("{stem}.")) || name.starts_with(&format!("{stem}-"));
            (in_dir && owned).then(|| {
                format!(
                    "a name the build owns in its artifact directory {}",
                    dir.display()
                )
            })
        }
    }
}

/// `dest`'s canonical path: its nearest existing ancestor resolved through every link,
/// with the components below it appended. What the file will be once it exists, for
/// comparing two destinations, or a destination with an input, by what they name rather
/// than how they are spelled.
fn canonical_destination(dest: &Path) -> Result<PathBuf> {
    let mut tail = Vec::new();
    let mut base = dest.to_path_buf();
    loop {
        // The leaf itself is compared by its own name: resolving a link there would
        // compare its target, and a link at a destination is refused regardless.
        if tail.is_empty() {
            if let Some(name) = base.file_name() {
                tail.push(name.to_os_string());
                base.pop();
                continue;
            }
        }
        match std::fs::canonicalize(&base) {
            Ok(resolved) => {
                let mut out = resolved;
                for part in tail.iter().rev() {
                    out.push(part);
                }
                return Ok(out);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = base.file_name() else {
                    return Err(format!("cannot resolve {} ({e})", dest.display()).into());
                };
                tail.push(name.to_os_string());
                base.pop();
            }
            Err(e) => return Err(format!("cannot resolve {} ({e})", dest.display()).into()),
        }
    }
}

/// A file type that is not a regular file, for a refusal.
fn describe_kind(meta: &std::fs::Metadata) -> &'static str {
    use std::os::unix::fs::FileTypeExt;
    let kind = meta.file_type();
    if kind.is_dir() {
        "a directory"
    } else if kind.is_block_device() {
        "a block device"
    } else if kind.is_char_device() {
        "a character device"
    } else if kind.is_fifo() {
        "a FIFO"
    } else if kind.is_socket() {
        "a socket"
    } else {
        "not a regular file"
    }
}

/// The temporary files a press writes its outputs to, and the names they take on commit.
///
/// Each temporary is in its destination's own directory, so the commit is a rename, and
/// is named `.<name>.<pid>.<random>.press.partial`, which the artifact directory's
/// stale-temporary sweep recognizes. Dropped without a commit, every temporary is
/// removed: a failed press leaves no partial output behind.
#[derive(Default)]
struct Staged {
    /// `(temporary, destination)`, in the order reserved.
    pending: Vec<(PathBuf, PathBuf)>,
}

impl Staged {
    /// A fresh temporary name for `dest`, in its directory, which is created if missing.
    fn reserve(&mut self, dest: &Path) -> Result<PathBuf> {
        let dir = dest.parent().unwrap_or_else(|| Path::new("."));
        std::fs::create_dir_all(dir)
            .map_err(|e| format!("cannot create {} ({e})", dir.display()))?;
        let mut nonce = [0u8; 6];
        std::io::Read::read_exact(&mut std::fs::File::open("/dev/urandom")?, &mut nonce)?;
        let nonce: String = nonce.iter().map(|b| format!("{b:02x}")).collect();
        let tmp = dir.join(format!(
            ".{}.{}.{nonce}.press.partial",
            file_name(dest),
            std::process::id()
        ));
        self.pending.push((tmp.clone(), dest.to_path_buf()));
        Ok(tmp)
    }

    /// The temporary reserved for `dest`.
    fn temp(&self, dest: &Path) -> &Path {
        self.pending
            .iter()
            .find(|(_, d)| d == dest)
            .map(|(t, _)| t.as_path())
            .expect("a destination is reserved before it is personalized")
    }

    /// Rename every temporary to its destination: the files written beside an image
    /// first, then the images, so an image never takes its name ahead of its listing.
    fn commit(mut self) -> Result<Vec<PathBuf>> {
        let pending = std::mem::take(&mut self.pending);
        let (images, beside): (Vec<_>, Vec<_>) = pending.into_iter().partition(|(_, d)| {
            !d.to_string_lossy().ends_with(".rootfs.uapi16")
                && !d.to_string_lossy().ends_with(".credentials.toml")
        });
        let mut published = Vec::new();
        let mut left: Vec<(PathBuf, PathBuf)> = beside.into_iter().chain(images).collect();
        while !left.is_empty() {
            let (tmp, dest) = left.remove(0);
            if let Err(e) = std::fs::rename(&tmp, &dest) {
                self.pending = std::iter::once((tmp, dest.clone())).chain(left).collect();
                return Err(format!("cannot publish {} ({e})", dest.display()).into());
            }
            published.push(dest);
        }
        Ok(published)
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        for (tmp, _) in &self.pending {
            let _ = std::fs::remove_file(tmp);
        }
    }
}

/// Bind each resolved role to the output path that names it: the positional for
/// a single-artifact build, `--boot-out` + `--rootfs-out` for a split build —
/// refusing the mixed forms so a half-specified press fails whole.
fn bind_outputs(
    roles: &[ArtifactRole],
    output: Option<PathBuf>,
    args: &PressArgs,
) -> Result<Vec<(ArtifactRole, PathBuf)>> {
    if roles.len() == 1 {
        if args.boot_out.is_some() || args.rootfs_out.is_some() {
            return Err(format!(
                "this build presses one file (the {}) — name it as the positional \
                 output, not with --boot-out/--rootfs-out",
                roles[0].describe()
            )
            .into());
        }
        let out = output
            .ok_or_else(|| format!("press needs an output path for the {}", roles[0].describe()))?;
        return Ok(vec![(roles[0], out)]);
    }
    if output.is_some() {
        return Err(
            "this build is a split layout: two artifacts, two files. Name both \
                    outputs with --boot-out and --rootfs-out instead of one positional"
                .into(),
        );
    }
    let boot_out = args
        .boot_out
        .clone()
        .ok_or("the boot image needs --boot-out <file>")?;
    let rootfs_out = args
        .rootfs_out
        .clone()
        .ok_or("the rootfs image needs --rootfs-out <file>")?;
    Ok(vec![
        (ArtifactRole::Boot, boot_out),
        (ArtifactRole::Rootfs, rootfs_out),
    ])
}

/// The dry run: everything the press would produce, nothing written.
fn dry_run(
    plan: &[Planned],
    sources: &Sources,
    keys: &Option<SeedKeys>,
    additions: &TreeAdditions,
    identity: &PressIdentity,
) -> Result<()> {
    for p in plan {
        if p.reassemble {
            println!(
                "{}: re-assemble from {} -> {}",
                p.role.describe(),
                sources
                    .published(Stage::Rootfs, role::ROOTFS_TAR)?
                    .display(),
                p.out.display()
            );
            for line in additions.describe() {
                println!("  {line}");
            }
            if let PressIdentity::Fresh { built, .. } = identity {
                println!(
                    "  identity: fresh, drawn when the image is written (the build's root \
                     PARTUUID is {})",
                    built.rootfs_partuuid.hyphenated()
                );
            }
        } else {
            let row = sources.disk_image(p.role)?;
            println!(
                "{}: stream {} -> {}",
                p.role.describe(),
                sources.take(row)?.display(),
                p.out.display()
            );
            if let Some(disk) = &row.disk {
                println!("  needs a medium of at least {} bytes", disk.bytes);
            }
        }
        for dest in p.destinations().skip(1) {
            println!("  beside it: {}", dest.display());
        }
        if p.role.contains_rootfs() {
            println!(
                "  seed: {}",
                keys.as_ref().map_or_else(
                    || "the empty template (no keys named)".into(),
                    describe_keys
                )
            );
        }
    }
    println!("dry run: nothing was written");
    Ok(())
}

/// Validate and collect the seed keys named on the command line. `None` when no
/// key was named at all — the pressed file then keeps its built-in empty seed.
pub(crate) fn seed_keys(args: &SeedKeyArgs) -> Result<Option<SeedKeys>> {
    if args.is_empty() {
        return Ok(None);
    }
    if let Some(hostname) = &args.hostname {
        boot2deb_core::hostname::check(hostname)
            .map_err(|why| format!("--hostname {hostname:?}: {why}"))?;
    }
    for key in &args.ssh_keys {
        boot2deb_core::authkeys::check_authorized_key(key)
            .map_err(|why| format!("--ssh-key: {why}"))?;
    }
    if let Some(ssid) = &args.wifi_ssid {
        boot2deb_core::wifi::check_ssid(ssid).map_err(|why| format!("--wifi-ssid: {why}"))?;
    }
    if let Some(psk) = &args.wifi_psk {
        boot2deb_core::wifi::check_psk(psk).map_err(|why| format!("--wifi-psk: {why}"))?;
    }
    if let Some(ip) = &args.static_ip {
        boot2deb_core::staticip::check(ip).map_err(|why| format!("--static-ip: {why}"))?;
    }
    Ok(Some(SeedKeys {
        hostname: args.hostname.clone(),
        authorized_keys: args.ssh_keys.clone(),
        wifi_ssid: args.wifi_ssid.clone(),
        wifi_psk: args.wifi_psk.clone(),
        static_ip: args.static_ip.clone(),
    }))
}

/// Collect the tree additions named on the command line, resolving `--embed-image` to
/// the build's own compressed artifact through its inventory. Returns the additions,
/// and the embedded artifact's path when there is one.
fn collect_additions(
    args: &PressArgs,
    resolved: &ResolvedBuild,
    recipe: &str,
    stem: &str,
    identity: &PressIdentity,
    seed_hostname: Option<String>,
    sources: &Sources,
) -> Result<(TreeAdditions, Option<PathBuf>)> {
    let mut additions = TreeAdditions::new(stem, recipe, identity).seed_hostname(seed_hostname);
    for spec in &args.copy {
        let Some((src, dest)) = spec.split_once(':') else {
            return Err(format!("--copy takes SRC:DEST, got {spec:?}").into());
        };
        additions.copy(Path::new(src), dest)?;
    }
    for dir in &args.copy_tree {
        additions.copy_tree(dir)?;
    }
    for deb in &args.debs {
        additions.deb(deb)?;
    }
    let mut embedded = None;
    if args.embed_image {
        if resolved.layout != Layout::Combined || !resolved.produces_image() {
            return Err(
                "--embed-image includes the recipe's own installable artifact, \
                        which only a combined-layout image build has"
                    .into(),
            );
        }
        // Compressed only, because the embedded copy is decompressed by the board at
        // install time and a raw multi-gigabyte payload would double the pressed image
        // for nothing. The first container in the build's own preference order.
        let row = sources
            .images()?
            .containers(ArtifactRole::Combined)
            .next()
            .ok_or(
                "--embed-image includes the build's compressed image, and the build kept \
                 none — build with compression on (the default)",
            )?;
        let path = sources.take(row)?;
        additions.embed_image(
            &path,
            &row.sha256,
            row.disk
                .as_ref()
                .expect("a disk image's inventory row records its disk"),
        )?;
        embedded = Some(path);
    }
    Ok((additions, embedded))
}

/// What the seed will say, for logs and the dry run.
fn describe_keys(keys: &SeedKeys) -> String {
    if keys.is_empty() {
        return "reset to the empty template".into();
    }
    let mut parts = Vec::new();
    if let Some(h) = &keys.hostname {
        parts.push(format!("hostname={h}"));
    }
    if !keys.authorized_keys.is_empty() {
        parts.push(format!("{} ssh key(s)", keys.authorized_keys.len()));
    }
    if let Some(ssid) = &keys.wifi_ssid {
        parts.push(format!(
            "wifi {ssid:?} ({})",
            if keys.wifi_psk.is_some() {
                "wpa"
            } else {
                "open"
            }
        ));
    }
    if let Some(ip) = &keys.static_ip {
        parts.push(format!("static ip {ip}"));
    }
    parts.join(", ")
}

/// The final component of `path`, for names derived from it.
fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The first 12 hex digits of a digest, for messages.
fn short(hex: &str) -> &str {
    hex.get(..12).unwrap_or(hex)
}

/// The wall clock, for the seed FAT's timestamp — per-unit data, not a
/// reproducible artifact.
pub(crate) fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(role: ArtifactRole, out: &Path) -> Planned {
        Planned {
            role,
            out: out.to_path_buf(),
            reassemble: false,
            listing: role
                .contains_rootfs()
                .then(|| out.with_extension("rootfs.uapi16")),
            credential: None,
        }
    }

    /// A press never writes onto the build's image, a compressed copy of it, a name the
    /// build owns, or, through its listing, the build's file manifest. A path of its own,
    /// even in the same directory, is fine.
    #[test]
    fn a_press_output_never_overwrites_the_builds_artifacts() {
        let tmp = tempfile::tempdir().unwrap();
        let out_dir = tmp.path().join("artifacts");
        std::fs::create_dir(&out_dir).unwrap();
        std::fs::write(out_dir.join("board-forky.img.xz"), b"xz").unwrap();
        let protected = vec![
            Protected::Artifacts {
                dir: out_dir.clone(),
                stem: "board-forky".into(),
            },
            Protected::File {
                path: out_dir.join("board-forky.img.xz"),
                what: "artifact board-forky.img.xz".into(),
            },
        ];
        let press = |role, out: &Path| preflight(&[plan(role, out)], &protected, true);
        assert!(press(ArtifactRole::Combined, &out_dir.join("board-forky.img")).is_err());
        assert!(press(ArtifactRole::Combined, &out_dir.join("board-forky.img.xz")).is_err());
        assert!(press(
            ArtifactRole::Rootfs,
            &out_dir.join("board-forky-rootfs.img")
        )
        .is_err());
        assert!(press(ArtifactRole::Combined, &out_dir.join("card.img")).is_ok());
        assert!(press(ArtifactRole::Combined, &tmp.path().join("board-forky.img")).is_ok());
    }

    /// An alias of a protected input is caught by what it names, not how it is spelled:
    /// a symlinked directory leading to the input, and a hard link to it.
    #[test]
    fn an_alias_of_an_input_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir(&src).unwrap();
        let input = src.join("site.conf");
        std::fs::write(&input, b"site").unwrap();
        let protected = vec![Protected::File {
            path: input.clone(),
            what: "--copy source".into(),
        }];
        std::os::unix::fs::symlink(&src, tmp.path().join("via")).unwrap();
        let err = preflight(
            &[plan(ArtifactRole::Boot, &tmp.path().join("via/site.conf"))],
            &protected,
            true,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("would replace"), "{err}");

        let hard = tmp.path().join("hard.img");
        std::fs::hard_link(&input, &hard).unwrap();
        let err = preflight(&[plan(ArtifactRole::Boot, &hard)], &protected, true)
            .unwrap_err()
            .to_string();
        assert!(err.contains("would replace"), "{err}");

        let tree = vec![Protected::Tree { dir: src.clone() }];
        assert!(preflight(
            &[plan(ArtifactRole::Boot, &src.join("new.img"))],
            &tree,
            true
        )
        .is_err());
    }

    /// Two outputs that name one file are refused before anything is written: the two
    /// split roles on one path, and an output on another output's listing.
    #[test]
    fn outputs_that_collide_are_refused_as_a_set() {
        let tmp = tempfile::tempdir().unwrap();
        let same = tmp.path().join("disk.img");
        let err = preflight(
            &[
                plan(ArtifactRole::Boot, &same),
                plan(ArtifactRole::Rootfs, &same),
            ],
            &[],
            false,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("name the same file"), "{err}");

        let card = tmp.path().join("card.img");
        let on_listing = tmp.path().join("card.rootfs.uapi16");
        assert!(preflight(
            &[
                plan(ArtifactRole::Rootfs, &card),
                plan(ArtifactRole::Boot, &on_listing)
            ],
            &[],
            false
        )
        .is_err());
        assert!(!card.exists() && !on_listing.exists());
    }

    /// A destination that is not a regular file is refused before it is opened: a
    /// symlink, a directory, a FIFO, and a character device. An existing regular file
    /// needs `--overwrite`.
    #[test]
    fn a_destination_must_be_a_regular_file_or_absent() {
        let tmp = tempfile::tempdir().unwrap();
        let target = tmp.path().join("target");
        std::fs::write(&target, b"keep").unwrap();
        let link = tmp.path().join("link.img");
        std::os::unix::fs::symlink(&target, &link).unwrap();
        let dir = tmp.path().join("dir.img");
        std::fs::create_dir(&dir).unwrap();
        let fifo = tmp.path().join("fifo.img");
        assert!(std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .is_ok_and(|s| s.success()));
        let check = |p: &Path, overwrite| preflight(&[plan(ArtifactRole::Boot, p)], &[], overwrite);
        assert!(check(&link, true)
            .unwrap_err()
            .to_string()
            .contains("symlink"));
        assert!(check(&dir, true)
            .unwrap_err()
            .to_string()
            .contains("a directory"));
        assert!(check(&fifo, true)
            .unwrap_err()
            .to_string()
            .contains("a FIFO"));
        assert!(check(Path::new("/dev/null"), true)
            .unwrap_err()
            .to_string()
            .contains("character device"));
        assert!(check(&target, false)
            .unwrap_err()
            .to_string()
            .contains("--overwrite"));
        assert!(check(&target, true).is_ok());
        assert_eq!(std::fs::read(&target).unwrap(), b"keep");
    }

    /// A staged output takes its name only on commit; dropped, it leaves nothing.
    #[test]
    fn staged_outputs_publish_on_commit_and_vanish_on_failure() {
        let tmp = tempfile::tempdir().unwrap();
        let dest = tmp.path().join("out/card.img");
        {
            let mut staged = Staged::default();
            let t = staged.reserve(&dest).unwrap();
            std::fs::write(&t, b"half").unwrap();
        }
        assert!(!dest.exists());
        assert_eq!(
            std::fs::read_dir(tmp.path().join("out")).unwrap().count(),
            0
        );

        let mut staged = Staged::default();
        let t = staged.reserve(&dest).unwrap();
        std::fs::write(&t, b"whole").unwrap();
        let l = staged
            .reserve(&dest.with_extension("rootfs.uapi16"))
            .unwrap();
        std::fs::write(&l, b"list").unwrap();
        let published = staged.commit().unwrap();
        assert_eq!(
            published.last().unwrap(),
            &dest,
            "the image is published last"
        );
        assert_eq!(std::fs::read(&dest).unwrap(), b"whole");
    }

    /// An output re-assembles when the press adds to its rootfs or draws a fresh identity,
    /// and only an output that contains a rootfs can. Everything else streams the build's
    /// bytes.
    #[test]
    fn an_addition_or_a_fresh_identity_re_assembles_and_nothing_else_does() {
        let built = ImageIdentity::derive("turing-rk1/forky", "turing-rk1");
        let kept = PressIdentity::Built(built);
        let fresh = PressIdentity::Fresh {
            built,
            fresh: ImageIdentity::derive("a fresh seed", "turing-rk1"),
        };
        let empty = TreeAdditions::new("turing-rk1-forky", "turing-rk1/forky", &kept);
        assert!(!reassembles(ArtifactRole::Combined, &empty, &kept));
        assert!(reassembles(ArtifactRole::Combined, &empty, &fresh));
        assert!(reassembles(ArtifactRole::Rootfs, &empty, &fresh));
        assert!(!reassembles(ArtifactRole::Boot, &empty, &fresh));

        let tmp = tempfile::tempdir().unwrap();
        let site = tmp.path().join("site.conf");
        std::fs::write(&site, b"site\n").unwrap();
        let mut added = TreeAdditions::new("turing-rk1-forky", "turing-rk1/forky", &kept);
        added.copy(&site, "/etc/site.conf").unwrap();
        assert!(reassembles(ArtifactRole::Combined, &added, &kept));
        assert!(!reassembles(ArtifactRole::Boot, &added, &kept));
        // A re-assembled output writes a credential beside it; a streamed one does not.
        let card = Planned::new(
            ArtifactRole::Combined,
            tmp.path().join("c.img"),
            &added,
            &kept,
        );
        assert_eq!(
            card.credential.as_deref(),
            Some(tmp.path().join("c.credentials.toml").as_path())
        );
        let streamed = Planned::new(
            ArtifactRole::Combined,
            tmp.path().join("c.img"),
            &empty,
            &kept,
        );
        assert!(streamed.credential.is_none());
    }
}
