//! Tree additions for a pressed image: the files `press --copy`, `--copy-tree`,
//! `--deb`, and `--embed-image` place into the rootfs, and the merge that lands
//! them in the image node's entry list.
//!
//! An addition belongs to a *unit or a site*, never to the recipe — a recipe
//! describes every board of a kind, `press` stamps one card — so additions ride
//! the pressed file only: the recipe's artifacts stay untouched, and the pressed
//! image records what it gained in its own `/etc/boot2deb/image.toml`
//! ([`IdentityPressed`]). Everything here is pure list manipulation over
//! [`SourceEntry`] values; a plain copy's bytes stay on the host as
//! [`FileRange`]s until the formatter places them, so an embedded
//! multi-gigabyte artifact costs no memory. A [template]
//! is the one exception, because it has to be read to be expanded — and is
//! size-capped for it.
//!
//! A whole directory is placed with [`copy_tree`](TreeAdditions::copy_tree),
//! which mirrors the target rootfs: `DIR/etc/site.conf` lands at
//! `/etc/site.conf`. The walk is
//! [`ferrosys::DirectorySource`] — the same
//! machinery the image node's own sources use — so symlinks are recorded as
//! symlinks rather than followed, and the entry list a tree walks to does not
//! depend on the order the host read its directories in.
//!
//! The merge is deliberately conservative: an addition replaces an existing
//! *file* (copying a config over the shipped one is the use case) but never a
//! directory, missing parent directories are synthesized as root-owned `0755`,
//! and a parent that exists as anything but a directory is an error rather than
//! a guess. Added entries take the rootfs's own clamped timestamp, so a pressed
//! image is deterministic in its inputs apart from the per-image password.

use crate::error::EngineError;
use crate::image::ImageIdentity;
use crate::press::template::{self, ImageFacts, Template, MAX_TEMPLATE_BYTES};
use boot2deb_core::provenance::{IdentityPressed, SystemIdentity};
use ferrosys::{EntryKind, FileContent, FileRange, Metadata, Source as _, SourceEntry};
use std::collections::HashMap;
use std::path::Path;

/// Where `--deb` packages land in the tree; the first-boot hook installs
/// everything it finds here, alphabetically.
pub const FIRSTBOOT_DEBS_DIR: &str = "/var/lib/boot2deb/firstboot-debs";

/// Where `--embed-image` places the compressed artifact;
/// `boot2deb-install-to` (a base-overlay script in every image) looks here.
pub const EMBEDDED_IMAGE_DIR: &str = "/var/lib/boot2deb/install";

/// The identity document every boot2deb rootfs carries — where the merge writes
/// the `[pressed]` table, whose mtime stamps the added entries, and which every
/// template expands against.
const IMAGE_TOML: &[u8] = b"/etc/boot2deb/image.toml";

/// Destinations an addition may not claim, each because something else owns the
/// path's content: the identity document belongs to the pressed marker, and
/// `/etc/shadow` to the per-image password splice that runs after the merge —
/// a copy there would be silently overwritten, so it is refused instead.
const RESERVED_DESTS: &[&str] = &["/etc/boot2deb/image.toml", "/etc/shadow"];

/// The additions one press applies: validated files plus the by-kind record the
/// pressed marker publishes.
///
/// Built by the CLI from `--copy`/`--copy-tree`/`--deb`/`--embed-image`,
/// consumed by the image node's re-assembly
/// ([`press_image`](crate::image::press_image)). Construction validates what can
/// be judged without the entry list (paths, kinds, source files, template
/// names); the merge validates the rest against the actual tree.
#[derive(Debug)]
pub struct TreeAdditions {
    /// The artifact stem the pressed image derives from, recorded in the marker.
    source_stem: String,
    /// The build point being pressed (`turing-rk1/forky`) — a template's
    /// `{{image.recipe}}`. The identity document states the axes a reference
    /// resolves to rather than the reference itself, so it comes from here.
    recipe: String,
    /// The `--hostname` seed key this press writes, when it names one. Held
    /// because a template's `{{image.hostname}}` must be the name the unit will
    /// answer to, and the seed — applied at first boot — supersedes the
    /// recipe's default in the identity document.
    seed_hostname: Option<String>,
    /// The identifiers the image node stamps into the GPT and the ext4
    /// superblock. Held so a template can name a PARTUUID that does not exist
    /// on any disk yet: they are derived from the build point, not drawn when
    /// the image is written.
    identity: ImageIdentity,
    /// The validated files, in insertion order; `apply` sorts by destination.
    files: Vec<AddedFile>,
    /// `--copy` and `--copy-tree` destinations, for the marker.
    copies: Vec<String>,
    /// `--deb` file names, for the marker.
    debs: Vec<String>,
    /// The `--embed-image` artifact file name, for the marker.
    embedded_image: Option<String>,
}

/// One validated addition: where it goes, what it is, and the mode it gets.
#[derive(Debug)]
struct AddedFile {
    /// Normalized absolute destination (`/etc/site.conf`), as entry-list bytes.
    dest: Vec<u8>,
    /// What lands there — host bytes, a rendered template, or a link target.
    content: AddedContent,
    /// Permission bits; ownership is always root:root, like the rest of the
    /// generated tree.
    mode: u16,
    /// How this addition is named in the plan output and the dry run.
    label: &'static str,
}

/// What an addition puts at its destination.
#[derive(Debug)]
enum AddedContent {
    /// Bytes on the host, read when the formatter places the entry.
    Host(FileContent),
    /// A parsed template, rendered against the image's identity at merge time.
    Template(Template),
    /// A symbolic link's target, recorded rather than followed.
    Symlink(Vec<u8>),
}

impl AddedFile {
    /// The entry kind this addition becomes, rendering a template against the
    /// image the merge is assembling.
    fn kind(&self, facts: &ImageFacts) -> EntryKind {
        match &self.content {
            AddedContent::Host(content) => EntryKind::File(content.clone()),
            AddedContent::Template(template) => {
                EntryKind::File(FileContent::Owned(template.render(facts)))
            }
            AddedContent::Symlink(target) => EntryKind::Symlink(target.clone()),
        }
    }

    /// One plan line: what kind of addition this is and where it lands, plus a
    /// template's reference count — an operator who expected four expansions
    /// and is told three has found a mistyped namespace, which is literal text
    /// by design.
    fn describe(&self) -> String {
        let dest = printable(&self.dest);
        match &self.content {
            AddedContent::Template(t) => {
                format!("{} -> {dest} ({} reference(s))", self.label, t.references())
            }
            _ => format!("{} -> {dest}", self.label),
        }
    }
}

impl TreeAdditions {
    /// Additions for a press of `source_stem`'s artifacts, initially empty.
    ///
    /// `recipe` is the build point being pressed and `identity` the identifiers
    /// its image will carry; both exist only so a [template]
    /// can name them, and neither is consulted by a press that adds no template.
    #[must_use]
    pub fn new(
        source_stem: impl Into<String>,
        recipe: impl Into<String>,
        identity: ImageIdentity,
    ) -> Self {
        TreeAdditions {
            source_stem: source_stem.into(),
            recipe: recipe.into(),
            seed_hostname: None,
            identity,
            files: Vec::new(),
            copies: Vec::new(),
            debs: Vec::new(),
            embedded_image: None,
        }
    }

    /// Record the `--hostname` this press seeds, so a template's
    /// `{{image.hostname}}` is the name the booted unit will answer to rather
    /// than the recipe's default. `None` — no `--hostname` — leaves the
    /// identity document's own value in place.
    #[must_use]
    pub fn seed_hostname(mut self, hostname: Option<String>) -> Self {
        self.seed_hostname = hostname;
        self
    }

    /// Whether this press adds anything to the tree at all. Empty additions never
    /// reach `apply`: a press with nothing to add streams the existing artifact
    /// instead of re-assembling.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    /// How many files the merge will place, for logs.
    #[must_use]
    pub fn file_count(&self) -> usize {
        self.files.len()
    }

    /// Add a `--copy src:dest` file: `src` on the host, placed at the absolute
    /// `dest` in the tree. Mode `0644`, or `0755` when the source is executable;
    /// ownership root:root. No option soup beyond that until a need appears.
    ///
    /// A source named `*.tmpl` is a [template]: it is
    /// read and its `{{image.…}}` references are checked now, and it is expanded
    /// when the merge runs. The destination is exactly what was named — the
    /// suffix rule decides the *destination* only where the destination is
    /// derived, which is [`copy_tree`](Self::copy_tree).
    ///
    /// # Errors
    ///
    /// [`EngineError::PressAddition`] for a destination that is not a clean
    /// absolute file path (or is reserved), for a source that is missing or not
    /// a regular file, and for a template that is oversized, not UTF-8, or names
    /// something outside the vocabulary.
    pub fn copy(&mut self, src: &Path, dest: &str) -> Result<(), EngineError> {
        let normalized = normalize_dest(dest).map_err(|detail| EngineError::PressAddition {
            dest: dest.to_string(),
            detail,
        })?;
        let is_template = src
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| template::strip_suffix(n).is_some());
        let (content, executable) = if is_template {
            let (template, executable) = host_template(src, &normalized)?;
            (AddedContent::Template(template), executable)
        } else {
            let (content, executable) = host_file(src, &normalized)?;
            (AddedContent::Host(content), executable)
        };
        self.push(
            normalized.clone(),
            content,
            file_mode(executable),
            if is_template { "template" } else { "copy" },
        );
        self.copies.push(normalized);
        Ok(())
    }

    /// Add a `--copy-tree dir` directory: every regular file and symlink under
    /// `dir` placed at its corresponding absolute path, so `dir` mirrors the
    /// target rootfs (`dir/etc/site.conf` → `/etc/site.conf`).
    ///
    /// Directories are not placed as entries of their own — the merge
    /// synthesizes the parents it needs as root-owned `0755`, so a site tree's
    /// umask never reaches the image. Files take `0644`, or `0755` when
    /// executable on the host, exactly as [`copy`](Self::copy) does; symlinks
    /// are recorded as symlinks and never followed, so a link pointing outside
    /// the tree lands as the link it is. A file named `*.tmpl` is a
    /// [template] and lands at the destination with the
    /// suffix removed — which is also the escape for shipping a literal one:
    /// `site.tmpl.tmpl` lands as `site.tmpl`.
    ///
    /// Returns the number of entries placed.
    ///
    /// # Errors
    ///
    /// [`EngineError::PressAddition`] when `dir` cannot be walked, names no
    /// files at all, holds a path that is not UTF-8 or that lands on a reserved
    /// destination, or holds an entry that is neither a regular file, a symlink,
    /// nor a directory — a device node, FIFO, socket, or hard link is refused
    /// rather than silently dropped, because a site tree carrying one means
    /// something other than what a placement can do.
    pub fn copy_tree(&mut self, dir: &Path) -> Result<usize, EngineError> {
        let tree_err = |detail: String| EngineError::PressAddition {
            dest: dir.display().to_string(),
            detail,
        };
        let entries = ferrosys::DirectorySource::from_path(dir)
            .map_err(|e| tree_err(format!("cannot walk {}: {e}", dir.display())))?
            .into_entries();
        let mut placed = 0;
        for entry in entries {
            // The walk makes `dir` itself the filesystem root, so its own entry
            // is the tree root and every parent below it is synthesized by the
            // merge — neither is an addition.
            if matches!(entry.kind, EntryKind::Directory) {
                continue;
            }
            let path = std::str::from_utf8(&entry.path)
                .map_err(|_| tree_err(format!("{} is not UTF-8", printable(&entry.path))))?
                .to_string();
            let host_path = dir.join(path.trim_start_matches('/'));
            let (dest, content, label) = match entry.kind {
                EntryKind::File(content) => match template::strip_suffix(&path) {
                    Some(dest) => {
                        let dest = dest.to_string();
                        let normalized = normalize_tree_dest(&dest, &tree_err)?;
                        let (template, _) = host_template(&host_path, &normalized)?;
                        (normalized, AddedContent::Template(template), "template")
                    }
                    None => (
                        normalize_tree_dest(&path, &tree_err)?,
                        AddedContent::Host(content),
                        "copy",
                    ),
                },
                EntryKind::Symlink(target) => (
                    normalize_tree_dest(&path, &tree_err)?,
                    AddedContent::Symlink(target),
                    "symlink",
                ),
                other => {
                    return Err(tree_err(format!(
                        "{path} is a {} — a copied tree carries regular files and \
                         symlinks only",
                        kind_name(&other)
                    )))
                }
            };
            let mode = match content {
                // A symlink's mode is not a permission in any filesystem that
                // stores one; 0777 is what every tool writes.
                AddedContent::Symlink(_) => 0o777,
                _ => file_mode(entry.meta.mode & 0o111 != 0),
            };
            self.push(dest.clone(), content, mode, label);
            self.copies.push(dest);
            placed += 1;
        }
        if placed == 0 {
            return Err(tree_err(format!(
                "{} names no files — a copied tree mirrors the target rootfs, so \
                 its contents are what get placed",
                dir.display()
            )));
        }
        Ok(placed)
    }

    /// Add a `--deb <path>` package, staged into [`FIRSTBOOT_DEBS_DIR`] under its
    /// own file name for the first-boot hook to `dpkg -i`.
    ///
    /// # Errors
    ///
    /// [`EngineError::PressAddition`] when the path does not name a readable
    /// `.deb` file.
    pub fn deb(&mut self, src: &Path) -> Result<(), EngineError> {
        let name = file_name(src)?;
        if !name.ends_with(".deb") {
            return Err(EngineError::PressAddition {
                dest: name,
                detail: format!("{} is not a .deb file", src.display()),
            });
        }
        let (content, _) = host_file(src, &name)?;
        self.push(
            format!("{FIRSTBOOT_DEBS_DIR}/{name}"),
            AddedContent::Host(content),
            0o644,
            "first-boot deb",
        );
        self.debs.push(name);
        Ok(())
    }

    /// Add the `--embed-image` payload: the recipe's own compressed artifact,
    /// carried at [`EMBEDDED_IMAGE_DIR`] for `boot2deb-install-to` to write onto
    /// the board's internal storage from the booted system.
    ///
    /// # Errors
    ///
    /// [`EngineError::PressAddition`] when the artifact is missing or not a
    /// regular file.
    pub fn embed_image(&mut self, artifact: &Path) -> Result<(), EngineError> {
        let name = file_name(artifact)?;
        let (content, _) = host_file(artifact, &name)?;
        self.push(
            format!("{EMBEDDED_IMAGE_DIR}/{name}"),
            AddedContent::Host(content),
            0o644,
            "embedded artifact",
        );
        self.embedded_image = Some(name);
        Ok(())
    }

    /// One line per addition, sorted by destination — the order the merge
    /// places them in, so the plan output and the bytes agree.
    #[must_use]
    pub fn describe(&self) -> Vec<String> {
        let mut files: Vec<&AddedFile> = self.files.iter().collect();
        files.sort_by(|a, b| a.dest.cmp(&b.dest));
        files.iter().map(|f| f.describe()).collect()
    }

    /// Record one validated addition.
    fn push(&mut self, dest: String, content: AddedContent, mode: u16, label: &'static str) {
        self.files.push(AddedFile {
            dest: dest.into_bytes(),
            content,
            mode,
            label,
        });
    }

    /// Merge the additions into the image node's entry list and write the
    /// `[pressed]` table into the tree's `/etc/boot2deb/image.toml`.
    ///
    /// Added entries and synthesized directories take the identity document's
    /// own mtime — the rootfs's clamped timestamp — so the output stays
    /// deterministic in its inputs. Templates are rendered here, against the
    /// identity read out of that same document, so a template sees what the
    /// image says about itself.
    ///
    /// # Errors
    ///
    /// [`EngineError::PressAddition`] when two additions claim one path, a
    /// destination is a directory in the image, a parent exists as a
    /// non-directory, or the tree carries no identity document (not a
    /// boot2deb-built rootfs).
    pub(crate) fn apply(&self, entries: &mut Vec<SourceEntry>) -> Result<(), EngineError> {
        // The tree's own clock, its identity, and the proof this is a boot2deb
        // rootfs in one step: its absence fails before any entry moves.
        let (identity, time) = read_identity(entries)?;
        let facts = ImageFacts::new(
            &identity,
            &self.identity,
            &self.recipe,
            self.seed_hostname.as_deref(),
        );

        // Keys are owned so the index outlives mutation of the entries it maps.
        let mut index: HashMap<Vec<u8>, usize> = HashMap::with_capacity(entries.len());
        for (i, entry) in entries.iter().enumerate() {
            index.insert(entry.path.clone(), i);
        }

        // Sorted by destination so parent synthesis is order-independent: the
        // flags may arrive in any order and press the same bytes.
        let mut files: Vec<&AddedFile> = self.files.iter().collect();
        files.sort_by(|a, b| a.dest.cmp(&b.dest));
        for pair in files.windows(2) {
            if pair[0].dest == pair[1].dest {
                return Err(EngineError::PressAddition {
                    dest: printable(&pair[0].dest),
                    detail: "two additions claim this path".into(),
                });
            }
        }

        let mut appended: Vec<SourceEntry> = Vec::new();
        // Paths that will exist once the merge lands, for parent checks across
        // the not-yet-appended entries.
        let mut planned_dirs: Vec<Vec<u8>> = Vec::new();
        for file in files {
            let dest = printable(&file.dest);
            match index.get(file.dest.as_slice()) {
                Some(&i) => {
                    // Replace anything but a directory: a config copied over the
                    // shipped one is the point; a directory swallowed by a file
                    // would orphan everything under it.
                    if matches!(entries[i].kind, EntryKind::Directory) {
                        return Err(EngineError::PressAddition {
                            dest,
                            detail: "the image holds a directory at this path".into(),
                        });
                    }
                    entries[i] = SourceEntry {
                        path: file.dest.clone(),
                        kind: file.kind(&facts),
                        meta: Metadata::new(file.mode, time),
                        xattrs: Vec::new(),
                    };
                }
                None => {
                    for parent in ancestors(&file.dest) {
                        match index.get(parent) {
                            Some(&i) => {
                                if !matches!(entries[i].kind, EntryKind::Directory) {
                                    return Err(EngineError::PressAddition {
                                        dest,
                                        detail: format!(
                                            "{} exists in the image but is not a directory",
                                            printable(parent)
                                        ),
                                    });
                                }
                            }
                            None => {
                                if !planned_dirs.iter().any(|p| p == parent) {
                                    planned_dirs.push(parent.to_vec());
                                    appended.push(SourceEntry {
                                        path: parent.to_vec(),
                                        kind: EntryKind::Directory,
                                        meta: Metadata::new(0o755, time),
                                        xattrs: Vec::new(),
                                    });
                                }
                            }
                        }
                    }
                    appended.push(SourceEntry {
                        path: file.dest.clone(),
                        kind: file.kind(&facts),
                        meta: Metadata::new(file.mode, time),
                        xattrs: Vec::new(),
                    });
                }
            }
        }
        entries.extend(appended);
        self.write_marker(entries, identity)
    }

    /// Rewrite the tree's identity document with this press's `[pressed]` table.
    fn write_marker(
        &self,
        entries: &mut [SourceEntry],
        mut identity: SystemIdentity,
    ) -> Result<(), EngineError> {
        // Sorted like the placement itself, so the marker — like the bytes — does
        // not depend on the order the flags were typed in.
        let sorted = |v: &[String]| {
            let mut v = v.to_vec();
            v.sort();
            v
        };
        identity.pressed = Some(IdentityPressed {
            source: self.source_stem.clone(),
            copies: sorted(&self.copies),
            debs: sorted(&self.debs),
            embedded_image: self.embedded_image.clone(),
        });
        let entry = entries
            .iter_mut()
            .find(|e| e.path == IMAGE_TOML)
            .expect("read_identity found it before the merge, which never moves it");
        entry.kind = EntryKind::File(FileContent::Owned(identity.to_toml_string()?.into_bytes()));
        Ok(())
    }
}

/// The tree's identity document, parsed, and its mtime — the timestamp every
/// added entry takes.
///
/// Read before anything moves: it is the proof the entry list is a boot2deb
/// rootfs at all, and no partially merged tree should exist when that fails.
fn read_identity(
    entries: &[SourceEntry],
) -> Result<(SystemIdentity, ferrosys::Timestamp), EngineError> {
    let marker_err = |detail: String| EngineError::PressAddition {
        dest: printable(IMAGE_TOML),
        detail,
    };
    let entry = entries
        .iter()
        .find(|e| e.path == IMAGE_TOML)
        .ok_or_else(|| {
            marker_err("the rootfs carries no image identity — not a boot2deb rootfs".into())
        })?;
    let EntryKind::File(content) = &entry.kind else {
        return Err(marker_err(
            "the image identity is not a regular file".into(),
        ));
    };
    let text = content
        .read()
        .map_err(|e| marker_err(format!("cannot read the image identity: {e}")))?;
    let text = std::str::from_utf8(&text)
        .map_err(|e| marker_err(format!("the image identity is not UTF-8: {e}")))?;
    let identity = SystemIdentity::from_toml_str(text, "etc/boot2deb/image.toml")?;
    Ok((identity, entry.meta.mtime))
}

/// Validate and normalize an addition destination into the entry list's path
/// form: absolute, no `.`/`..`/empty components, a file path (no trailing
/// slash), outside `/dev` (devtmpfs hides it at boot) and the reserved set.
fn normalize_dest(dest: &str) -> Result<String, String> {
    let Some(rest) = dest.strip_prefix('/') else {
        return Err("the destination must be an absolute path".into());
    };
    if rest.is_empty() {
        return Err("the destination names the root directory, not a file".into());
    }
    if dest.ends_with('/') {
        return Err("the destination must name a file, not a directory".into());
    }
    let mut parts = Vec::new();
    for part in rest.split('/') {
        match part {
            "" | "." => return Err("the destination has an empty or `.` component".into()),
            ".." => return Err("the destination must not contain `..`".into()),
            other => parts.push(other),
        }
    }
    let normalized = format!("/{}", parts.join("/"));
    if normalized == "/dev" || normalized.starts_with("/dev/") {
        return Err("everything under /dev is hidden by devtmpfs at boot".into());
    }
    if RESERVED_DESTS.contains(&normalized.as_str()) {
        return Err("this path is generated per image and cannot be replaced".into());
    }
    Ok(normalized)
}

/// [`normalize_dest`] for a destination the tree walk derived, whose failure
/// names the tree rather than a destination the operator typed.
fn normalize_tree_dest(
    dest: &str,
    tree_err: &impl Fn(String) -> EngineError,
) -> Result<String, EngineError> {
    normalize_dest(dest).map_err(|detail| tree_err(format!("{dest}: {detail}")))
}

/// The mode an addition lands with: `0755` when the host file is executable,
/// `0644` otherwise. Deliberately normalized rather than carried over, so a
/// site tree's umask is not a property of the image.
fn file_mode(executable: bool) -> u16 {
    if executable {
        0o755
    } else {
        0o644
    }
}

/// A host source file as addition content: a [`FileRange`] over the whole file
/// (read when placed, so size costs nothing now), plus whether it is executable.
fn host_file(src: &Path, dest: &str) -> Result<(FileContent, bool), EngineError> {
    let (meta, executable) = host_meta(src, dest)?;
    Ok((
        FileContent::Range(FileRange::at_path(src, 0, meta.len())),
        executable,
    ))
}

/// A host source file as a parsed [`Template`], plus whether it is executable.
///
/// Unlike [`host_file`] this reads the bytes now: a template must be parsed to
/// be checked, and checking it here is what makes a mistyped reference fail on
/// the command line rather than in the middle of a re-assembly.
fn host_template(src: &Path, dest: &str) -> Result<(Template, bool), EngineError> {
    let bad = |detail: String| EngineError::PressAddition {
        dest: dest.to_string(),
        detail,
    };
    let (meta, executable) = host_meta(src, dest)?;
    if meta.len() > MAX_TEMPLATE_BYTES {
        return Err(bad(format!(
            "{} is {} bytes; a template is a config file and is read whole, so \
             {MAX_TEMPLATE_BYTES} is the ceiling (drop the {} suffix to copy it \
             verbatim instead)",
            src.display(),
            meta.len(),
            template::TEMPLATE_SUFFIX,
        )));
    }
    let bytes =
        std::fs::read(src).map_err(|e| bad(format!("cannot read {}: {e}", src.display())))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| bad(format!("{} is a template but is not UTF-8", src.display())))?;
    Ok((Template::parse(&text, dest)?, executable))
}

/// A host source file's metadata, refusing anything but a regular file, plus
/// whether any execute bit is set.
fn host_meta(src: &Path, dest: &str) -> Result<(std::fs::Metadata, bool), EngineError> {
    let meta = std::fs::metadata(src).map_err(|e| EngineError::PressAddition {
        dest: dest.to_string(),
        detail: format!("cannot read {}: {e}", src.display()),
    })?;
    if !meta.is_file() {
        return Err(EngineError::PressAddition {
            dest: dest.to_string(),
            detail: format!("{} is not a regular file", src.display()),
        });
    }
    let executable = {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    };
    Ok((meta, executable))
}

/// The addition's file name — how a staged deb or embedded artifact is named in
/// the tree.
fn file_name(path: &Path) -> Result<String, EngineError> {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(str::to_string)
        .ok_or_else(|| EngineError::PressAddition {
            dest: path.display().to_string(),
            detail: "the path has no usable file name".into(),
        })
}

/// What an entry kind is called in the refusal that names it.
fn kind_name(kind: &EntryKind) -> &'static str {
    match kind {
        EntryKind::Directory => "directory",
        EntryKind::File(_) => "regular file",
        EntryKind::Symlink(_) => "symlink",
        EntryKind::HardLink { .. } => "hard link",
        EntryKind::CharDevice { .. } => "character device node",
        EntryKind::BlockDevice { .. } => "block device node",
        EntryKind::Fifo => "FIFO",
        EntryKind::Socket => "socket",
        // ferrosys may grow a kind; a placement still cannot make one.
        _ => "kind a placement cannot make",
    }
}

/// The proper ancestors of an absolute entry path, shallowest first —
/// `/a/b/c` yields `/a`, `/a/b`. The root itself always exists and is skipped.
fn ancestors(path: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    for (i, byte) in path.iter().enumerate().skip(1) {
        if *byte == b'/' {
            out.push(&path[..i]);
        }
    }
    out
}

/// An entry path for an error message.
fn printable(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use ferrosys::Timestamp;

    /// The build point every test presses, and the identifiers its image carries.
    const RECIPE: &str = "turing-rk1/forky";

    fn identity() -> ImageIdentity {
        ImageIdentity::derive(RECIPE, "turing-rk1")
    }

    fn additions(stem: &str) -> TreeAdditions {
        TreeAdditions::new(stem, RECIPE, identity())
    }

    /// A minimal boot2deb-shaped entry list: the directories every rootfs has,
    /// one shipped config, and the identity document the marker rewrites.
    fn tree() -> Vec<SourceEntry> {
        let t = Timestamp::from_secs(1_700_000_000);
        let dir = |p: &str| SourceEntry {
            path: p.as_bytes().to_vec(),
            kind: EntryKind::Directory,
            meta: Metadata::new(0o755, t),
            xattrs: Vec::new(),
        };
        let file = |p: &str, body: &str| SourceEntry {
            path: p.as_bytes().to_vec(),
            kind: EntryKind::File(FileContent::Owned(body.as_bytes().to_vec())),
            meta: Metadata::new(0o644, t),
            xattrs: Vec::new(),
        };
        vec![
            dir("/etc"),
            dir("/etc/boot2deb"),
            dir("/var"),
            dir("/var/lib"),
            file("/etc/motd", "shipped\n"),
            file("/etc/boot2deb/image.toml", &identity_toml()),
        ]
    }

    /// A syntactically real identity document, as the rootfs node stages it.
    fn identity_toml() -> String {
        "version = 1\n\
         [image]\n\
         device = \"turing-rk1\"\n\
         description = \"d\"\n\
         arch = \"arm64\"\n\
         soc = \"rk3588\"\n\
         boot_method = \"rockchip-rkbin\"\n\
         suite = \"forky\"\n\
         features = []\n\
         layout = \"combined\"\n\
         hostname = \"rk1\"\n\
         [kernel]\n\
         id = \"k\"\n\
         flavor = \"mainline\"\n"
            .to_string()
    }

    fn text_of(entries: &[SourceEntry], path: &str) -> String {
        let entry = entries
            .iter()
            .find(|e| e.path == path.as_bytes())
            .unwrap_or_else(|| panic!("{path} present"));
        let EntryKind::File(content) = &entry.kind else {
            panic!("{path} is a file");
        };
        String::from_utf8(content.read().unwrap().into_owned()).unwrap()
    }

    fn copy_of(tmp: &Path, name: &str, body: &str) -> std::path::PathBuf {
        let p = tmp.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    /// The merge places a new file with synthesized parents, replaces a shipped
    /// file in place, stamps the pressed marker — and leaves every untouched
    /// entry untouched.
    #[test]
    fn apply_places_replaces_and_marks() {
        let tmp = tempfile::tempdir().unwrap();
        let mut adds = additions("turing-rk1-forky");
        adds.copy(
            &copy_of(tmp.path(), "site.conf", "site\n"),
            "/opt/site/site.conf",
        )
        .unwrap();
        adds.copy(&copy_of(tmp.path(), "motd", "pressed\n"), "/etc/motd")
            .unwrap();
        adds.deb(&copy_of(tmp.path(), "app_1.0_arm64.deb", "deb"))
            .unwrap();

        let mut entries = tree();
        let before = entries.len();
        adds.apply(&mut entries).unwrap();

        // Replacement is in place; the shipped mode is superseded by the copy's.
        assert_eq!(text_of(&entries, "/etc/motd"), "pressed\n");

        // New file landed with both missing parents synthesized as directories,
        // and /var/lib was not re-created (it exists).
        for dir in ["/opt", "/opt/site", "/var/lib/boot2deb/firstboot-debs"] {
            let entry = entries
                .iter()
                .find(|e| e.path == dir.as_bytes())
                .unwrap_or_else(|| panic!("{dir} synthesized"));
            assert!(matches!(entry.kind, EntryKind::Directory), "{dir}");
            assert_eq!(entry.meta.mode, 0o755, "{dir}");
        }
        assert_eq!(
            entries.iter().filter(|e| e.path == b"/var/lib").count(),
            1,
            "an existing parent is never duplicated"
        );

        // The marker carries the by-kind record and the source stem.
        let marker = text_of(&entries, "/etc/boot2deb/image.toml");
        let identity = SystemIdentity::from_toml_str(&marker, "image.toml").unwrap();
        let pressed = identity.pressed.expect("pressed table present");
        assert_eq!(pressed.source, "turing-rk1-forky");
        assert_eq!(pressed.copies, ["/etc/motd", "/opt/site/site.conf"]);
        assert_eq!(pressed.debs, ["app_1.0_arm64.deb"]);
        assert_eq!(pressed.embedded_image, None);

        // 1 new file + 2 synthesized dirs + deb file + 2 deb parents.
        assert_eq!(entries.len(), before + 6);
    }

    /// Flag order does not reach the bytes: the same additions in a different
    /// order produce the same entry list.
    #[test]
    fn apply_is_order_independent() {
        let tmp = tempfile::tempdir().unwrap();
        let a = copy_of(tmp.path(), "a.conf", "a\n");
        let b = copy_of(tmp.path(), "b.conf", "b\n");

        let mut fwd = additions("s");
        fwd.copy(&a, "/opt/x/a.conf").unwrap();
        fwd.copy(&b, "/opt/x/b.conf").unwrap();
        let mut rev = additions("s");
        rev.copy(&b, "/opt/x/b.conf").unwrap();
        rev.copy(&a, "/opt/x/a.conf").unwrap();

        let mut left = tree();
        let mut right = tree();
        fwd.apply(&mut left).unwrap();
        rev.apply(&mut right).unwrap();
        let paths = |v: &[SourceEntry]| v.iter().map(|e| e.path.clone()).collect::<Vec<_>>();
        assert_eq!(paths(&left), paths(&right));
        assert_eq!(text_of(&left, "/opt/x/a.conf"), "a\n");
        assert_eq!(text_of(&right, "/opt/x/a.conf"), "a\n");
    }

    /// The refusals: relative/`..`/reserved/dev destinations at construction;
    /// directory destinations, non-directory parents, duplicate claims, and a
    /// tree with no identity document at merge.
    #[test]
    fn the_refusals_name_the_problem() {
        let tmp = tempfile::tempdir().unwrap();
        let src = copy_of(tmp.path(), "f", "x");

        let mut adds = additions("s");
        for bad in [
            "relative/path",
            "/etc/",
            "/../etc/x",
            "/etc/./x",
            "/dev/mmcblk0",
            "/etc/shadow",
            "/etc/boot2deb/image.toml",
            "/",
        ] {
            let err = adds.copy(&src, bad).unwrap_err();
            assert!(
                matches!(err, EngineError::PressAddition { .. }),
                "{bad}: {err}"
            );
        }
        let err = adds
            .deb(&copy_of(tmp.path(), "not-a-deb.txt", "x"))
            .unwrap_err();
        assert!(err.to_string().contains(".deb"), "{err}");

        // A destination the image holds a directory at.
        let mut dir_hit = additions("s");
        dir_hit.copy(&src, "/etc/boot2deb").unwrap();
        let err = dir_hit.apply(&mut tree()).unwrap_err();
        assert!(err.to_string().contains("directory"), "{err}");

        // A parent that exists as a file.
        let mut under_file = additions("s");
        under_file.copy(&src, "/etc/motd/x").unwrap();
        let err = under_file.apply(&mut tree()).unwrap_err();
        assert!(err.to_string().contains("not a directory"), "{err}");

        // Two additions, one path.
        let mut dup = additions("s");
        dup.copy(&src, "/opt/x").unwrap();
        dup.copy(&src, "/opt/x").unwrap();
        let err = dup.apply(&mut tree()).unwrap_err();
        assert!(err.to_string().contains("two additions"), "{err}");

        // Not a boot2deb rootfs.
        let mut fine = additions("s");
        fine.copy(&src, "/opt/x").unwrap();
        let mut bare = tree();
        bare.retain(|e| e.path != IMAGE_TOML);
        let err = fine.apply(&mut bare).unwrap_err();
        assert!(err.to_string().contains("not a boot2deb rootfs"), "{err}");
    }

    /// An executable source lands `0755`, a plain one `0644`, and both take the
    /// identity document's timestamp rather than the host file's.
    #[test]
    fn modes_and_times_are_the_trees_own() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = tempfile::tempdir().unwrap();
        let script = copy_of(tmp.path(), "run.sh", "#!/bin/sh\n");
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700)).unwrap();
        let plain = copy_of(tmp.path(), "data", "d");

        let mut adds = additions("s");
        adds.copy(&script, "/usr/local/bin/run.sh").unwrap();
        adds.copy(&plain, "/usr/local/share/data").unwrap();

        let mut entries = tree();
        adds.apply(&mut entries).unwrap();
        let by_path = |p: &str| {
            entries
                .iter()
                .find(|e| e.path == p.as_bytes())
                .unwrap_or_else(|| panic!("{p}"))
        };
        assert_eq!(by_path("/usr/local/bin/run.sh").meta.mode, 0o755);
        assert_eq!(by_path("/usr/local/share/data").meta.mode, 0o644);
        assert_eq!(
            by_path("/usr/local/share/data").meta.mtime,
            Timestamp::from_secs(1_700_000_000),
            "added entries take the tree's clock, not the host's"
        );
    }

    /// Build a site tree mirroring a rootfs: two plain files, an executable, a
    /// symlink, a template, and a nested directory.
    fn site_tree(root: &Path) {
        use std::os::unix::fs::PermissionsExt;
        std::fs::create_dir_all(root.join("etc/myapp")).unwrap();
        std::fs::create_dir_all(root.join("usr/local/bin")).unwrap();
        std::fs::write(root.join("etc/motd"), "site motd\n").unwrap();
        std::fs::write(root.join("etc/myapp/plain.conf"), "plain\n").unwrap();
        std::fs::write(
            root.join("etc/myapp/site.conf.tmpl"),
            "node = {{image.hostname}}\nroot = PARTUUID={{image.rootfs_partuuid}}\n",
        )
        .unwrap();
        let script = root.join("usr/local/bin/site");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::os::unix::fs::symlink("/etc/myapp/site.conf", root.join("etc/current.conf")).unwrap();
    }

    /// A copied tree mirrors the rootfs: every file lands at its own absolute
    /// path, the executable bit survives, a symlink stays a symlink, and a
    /// `.tmpl` lands expanded under the name without the suffix.
    #[test]
    fn a_copied_tree_places_files_symlinks_and_templates() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("site");
        site_tree(&root);

        let mut adds = additions("turing-rk1-forky");
        assert_eq!(adds.copy_tree(&root).unwrap(), 5);

        let mut entries = tree();
        adds.apply(&mut entries).unwrap();
        let by_path = |p: &str| {
            entries
                .iter()
                .find(|e| e.path == p.as_bytes())
                .unwrap_or_else(|| panic!("{p} placed"))
        };

        // Plain files, at their mirrored paths; the shipped /etc/motd replaced.
        assert_eq!(text_of(&entries, "/etc/motd"), "site motd\n");
        assert_eq!(text_of(&entries, "/etc/myapp/plain.conf"), "plain\n");
        assert_eq!(by_path("/etc/myapp/plain.conf").meta.mode, 0o644);

        // The executable bit is the one host mode carried over.
        assert_eq!(by_path("/usr/local/bin/site").meta.mode, 0o755);

        // A symlink is recorded as a symlink, not followed.
        assert!(
            matches!(
                &by_path("/etc/current.conf").kind,
                EntryKind::Symlink(t) if t == b"/etc/myapp/site.conf"
            ),
            "symlink target preserved"
        );

        // The template landed without its suffix, expanded against this image.
        assert!(
            !entries
                .iter()
                .any(|e| e.path == b"/etc/myapp/site.conf.tmpl"),
            "the suffix never reaches the image"
        );
        let expanded = text_of(&entries, "/etc/myapp/site.conf");
        assert_eq!(
            expanded,
            format!(
                "node = rk1\nroot = PARTUUID={}\n",
                identity().rootfs_partuuid.hyphenated()
            ),
            "the template expands against the tree's own identity"
        );

        // A synthesized parent that the base tree did not have.
        assert!(matches!(
            by_path("/usr/local/bin").kind,
            EntryKind::Directory
        ));

        // Every placed destination is in the marker, sorted.
        let marker = text_of(&entries, "/etc/boot2deb/image.toml");
        let pressed = SystemIdentity::from_toml_str(&marker, "image.toml")
            .unwrap()
            .pressed
            .expect("pressed table");
        assert_eq!(
            pressed.copies,
            [
                "/etc/current.conf",
                "/etc/motd",
                "/etc/myapp/plain.conf",
                "/etc/myapp/site.conf",
                "/usr/local/bin/site",
            ]
        );
    }

    /// The tree walk's own refusals: a reserved destination inside the tree, an
    /// entry kind a placement cannot make, and a tree with nothing in it.
    #[test]
    fn a_copied_tree_refuses_what_it_cannot_place() {
        let tmp = tempfile::tempdir().unwrap();

        // A reserved destination reached through the mirror rather than typed.
        let reserved = tmp.path().join("reserved");
        std::fs::create_dir_all(reserved.join("etc")).unwrap();
        std::fs::write(reserved.join("etc/shadow"), "x").unwrap();
        let err = additions("s").copy_tree(&reserved).unwrap_err();
        assert!(err.to_string().contains("generated per image"), "{err}");

        // A FIFO: a real thing to find in a tree, and not something a placement
        // can make.
        let fifo_tree = tmp.path().join("fifo");
        std::fs::create_dir_all(fifo_tree.join("run")).unwrap();
        let fifo = fifo_tree.join("run/pipe");
        let c = std::ffi::CString::new(fifo.as_os_str().as_encoded_bytes().to_vec()).unwrap();
        // SAFETY: `c` is a valid NUL-terminated path in a fresh temp directory.
        assert_eq!(unsafe { libc::mkfifo(c.as_ptr(), 0o644) }, 0);
        let err = additions("s").copy_tree(&fifo_tree).unwrap_err();
        assert!(err.to_string().contains("FIFO"), "{err}");

        // Nothing to place is operator error, not a silent no-op that would
        // make the press stream the plain artifact instead.
        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(empty.join("etc")).unwrap();
        let err = additions("s").copy_tree(&empty).unwrap_err();
        assert!(err.to_string().contains("names no files"), "{err}");

        // A directory that is not there at all.
        let err = additions("s")
            .copy_tree(&tmp.path().join("absent"))
            .unwrap_err();
        assert!(err.to_string().contains("cannot walk"), "{err}");
    }

    /// A template's names are checked when the addition is collected, so a typo
    /// fails on the command line rather than in the middle of a re-assembly.
    #[test]
    fn a_template_is_checked_before_anything_is_read() {
        let tmp = tempfile::tempdir().unwrap();
        let good = copy_of(tmp.path(), "ok.conf.tmpl", "a={{image.suite}}\n");
        let typo = copy_of(tmp.path(), "bad.conf.tmpl", "a={{image.sweet}}\n");
        let binary = tmp.path().join("bin.conf.tmpl");
        std::fs::write(&binary, [0xff, 0xfe, 0x00]).unwrap();

        let mut adds = additions("s");
        adds.copy(&good, "/etc/ok.conf").unwrap();
        // The destination is what was named: the suffix rule decides a
        // destination only where the destination is derived.
        assert_eq!(
            adds.describe(),
            ["template -> /etc/ok.conf (1 reference(s))"]
        );

        let err = additions("s").copy(&typo, "/etc/bad.conf").unwrap_err();
        assert!(err.to_string().contains("image.suite"), "{err}");
        let err = additions("s").copy(&binary, "/etc/bin.conf").unwrap_err();
        assert!(err.to_string().contains("not UTF-8"), "{err}");

        // A file that only *looks* like a template is copied verbatim.
        let plain = copy_of(tmp.path(), "keep.conf", "a={{image.sweet}}\n");
        let mut verbatim = additions("s");
        verbatim.copy(&plain, "/etc/keep.conf").unwrap();
        let mut entries = tree();
        verbatim.apply(&mut entries).unwrap();
        assert_eq!(text_of(&entries, "/etc/keep.conf"), "a={{image.sweet}}\n");
    }

    /// A press that seeds a hostname bakes *that* name into its templates: the
    /// identity document still holds the recipe's, and the unit will not.
    #[test]
    fn a_template_bakes_the_hostname_the_unit_will_answer_to() {
        let tmp = tempfile::tempdir().unwrap();
        let src = copy_of(tmp.path(), "host.conf.tmpl", "node={{image.hostname}}\n");

        let mut seeded = additions("s").seed_hostname(Some("rk1-07".into()));
        seeded.copy(&src, "/etc/node.conf").unwrap();
        let mut entries = tree();
        seeded.apply(&mut entries).unwrap();
        assert_eq!(text_of(&entries, "/etc/node.conf"), "node=rk1-07\n");

        // No --hostname: the identity document's own value stands.
        let mut plain = additions("s");
        plain.copy(&src, "/etc/node.conf").unwrap();
        let mut entries = tree();
        plain.apply(&mut entries).unwrap();
        assert_eq!(text_of(&entries, "/etc/node.conf"), "node=rk1\n");
    }

    /// An oversized template is refused rather than read into memory, and the
    /// message names the way out.
    #[test]
    fn an_oversized_template_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let big = tmp.path().join("huge.conf.tmpl");
        std::fs::write(
            &big,
            vec![b'x'; usize::try_from(MAX_TEMPLATE_BYTES).unwrap() + 1],
        )
        .unwrap();
        let err = additions("s").copy(&big, "/etc/huge.conf").unwrap_err();
        let text = err.to_string();
        assert!(text.contains("ceiling"), "{text}");
        assert!(text.contains(".tmpl"), "{text}");
    }
}
