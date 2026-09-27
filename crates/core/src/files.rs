//! Image file manifests — every file an image's root filesystem holds, in the UAPI.16
//! "File Manifest" format, and the comparison of two of them.
//!
//! Pure: the model, the writer, a strict reader for the manifests boot2deb writes, and the
//! comparison. Walking a formatted filesystem to fill the model is the image stage's work.
//!
//! # The format
//!
//! UAPI.16 is a draft, not a published specification: uapi-group/specifications pull
//! request 213, version 0.1, at head `8c732b1` (2026-09-23). This module implements that
//! revision, named by [`DRAFT`]. The draft is still moving, so read the pull request again
//! before changing anything here.
//!
//! A manifest is an RFC 7464 JSON text sequence: each object is preceded by an ASCII record
//! separator (`0x1e`) and followed by a line feed. The sequence is:
//!
//! 1. The root object, for the filesystem's root directory. It carries no `name`, and its
//!    `mediaType` is [`MEDIA_TYPE`].
//! 2. One object per inode name, in pre-order: a directory's entries follow it directly.
//! 3. A trailer object whose `mediaType` is [`TRAILER_MEDIA_TYPE`].
//!
//! # What boot2deb writes
//!
//! Each object is compact JSON with its keys sorted by byte value, so one tree always writes
//! the same bytes. Siblings are sorted by the raw bytes of their names, which the draft
//! recommends and which ferrosys does not promise. An entry carries `name`, `type`, `mode`
//! (not on a symlink), `uid`, `gid` and `mTimeNSec`, then by type:
//!
//! - A regular file carries `size`, and `sha256` when it is not empty.
//! - A symbolic link carries `size`, the target's length, and its target as one `literal`
//!   entry in `contents`, base64-encoded.
//! - A character or block device carries `major` and `minor`.
//! - Every name of a hard-linked inode carries the same `inodeToken`, counted from 1 in
//!   manifest order.
//! - Extended attributes go in [`XATTRS_FIELD`], an object from attribute name to base64
//!   value. The draft names attributes only as future work, and its extension rule is what
//!   this field follows. File capabilities (`security.capability`) ship in an image, and a
//!   comparison has to see them.

use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

/// The draft revision this module implements.
pub const DRAFT: &str = "UAPI.16 version 0.1, uapi-group/specifications#213 at 8c732b1";

/// The root object's `mediaType`, which identifies a sequence as a UAPI.16 manifest.
pub const MEDIA_TYPE: &str = "application/vnd.uapi.16.manifest";

/// The trailer object's `mediaType`. The trailer ends the sequence, and anything after it
/// makes the manifest invalid.
pub const TRAILER_MEDIA_TYPE: &str = "application/vnd.uapi.16.trailer";

/// The vendor field carrying an entry's extended attributes, named by the draft's
/// `x<Vendor><Field>` extension rule.
pub const XATTRS_FIELD: &str = "xBoot2debXattrs";

/// A name the draft reserves for the manifest itself. The writer refuses an entry with
/// this name, or with this name followed by a dot.
pub const RESERVED_NAME: &str = "Uapi16Manifest";

/// The paths whose content differs between two images built from one lock by design.
///
/// `/etc/shadow` holds the per-image first-boot password, generated fresh for every image
/// and spliced in after the rootfs is cached. Two builds of one lock that agree everywhere
/// else have reproduced each other. A comparison of their manifests sets this set aside
/// to say so. It is a constant rather than a flag, because what the password
/// splice touches is a property of the build, not of the question.
///
/// Only the content is set aside, the [`PER_IMAGE_FIELDS`]. The splice rewrites the
/// file's bytes and nothing else. Such a path that appears or disappears is still a
/// difference, as is one whose type, mode, owner or attributes change.
pub const PER_IMAGE_PATHS: &[&str] = &["/etc/shadow"];

/// The fields of a [`PER_IMAGE_PATHS`] entry the password splice can change: the file's
/// bytes, its length, and the time it was written.
pub const PER_IMAGE_FIELDS: &[&str] = &["mtime", "size", "sha256"];

/// The record separator RFC 7464 puts before each object.
const RS: u8 = 0x1e;

/// An inode's type, spelled the way the draft's `type` field spells it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FileType {
    /// A regular file (`reg`).
    Reg,
    /// A directory (`dir`).
    Dir,
    /// A symbolic link (`lnk`).
    Lnk,
    /// A named pipe (`fifo`).
    Fifo,
    /// A character device (`chr`).
    Chr,
    /// A block device (`blk`).
    Blk,
    /// A socket (`sock`).
    Sock,
}

impl FileType {
    /// The draft's spelling.
    pub fn as_str(self) -> &'static str {
        match self {
            FileType::Reg => "reg",
            FileType::Dir => "dir",
            FileType::Lnk => "lnk",
            FileType::Fifo => "fifo",
            FileType::Chr => "chr",
            FileType::Blk => "blk",
            FileType::Sock => "sock",
        }
    }

    /// The type a POSIX `st_mode` encodes in its format bits, or `None` for bits naming
    /// no type.
    pub fn from_mode(mode: u32) -> Option<Self> {
        Some(match mode & 0o170000 {
            0o100000 => FileType::Reg,
            0o040000 => FileType::Dir,
            0o120000 => FileType::Lnk,
            0o010000 => FileType::Fifo,
            0o020000 => FileType::Chr,
            0o060000 => FileType::Blk,
            0o140000 => FileType::Sock,
            _ => return None,
        })
    }

    /// The type a `type` field names, or `None` for any other string.
    fn parse(s: &str) -> Option<Self> {
        [
            FileType::Reg,
            FileType::Dir,
            FileType::Lnk,
            FileType::Fifo,
            FileType::Chr,
            FileType::Blk,
            FileType::Sock,
        ]
        .into_iter()
        .find(|t| t.as_str() == s)
    }
}

/// One inode name in a manifest, or the root directory.
///
/// A resolved entry: what it carries is what the manifest states, and a field the draft
/// does not allow on its type is empty here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileEntry {
    /// The path relative to the filesystem root, `/`-separated, with no leading or trailing
    /// slash and no `.` or `..` component. Empty for the root.
    pub name: String,
    /// The inode's type.
    pub file_type: FileType,
    /// The permission bits, `0..=0o7777`. Not written for a symbolic link, whose mode the
    /// draft excludes.
    pub mode: u32,
    /// The owning user id.
    pub uid: u32,
    /// The owning group id.
    pub gid: u32,
    /// The modification time, in nanoseconds since the Unix epoch.
    pub mtime_nsec: u64,
    /// For a regular file its length, for a symbolic link its target's length, and zero
    /// for every other type.
    pub size: u64,
    /// The lowercase-hex sha256 of a non-empty regular file's content. `None` for every
    /// other entry, including an empty file, which the draft asks to leave unhashed.
    pub sha256: Option<String>,
    /// A symbolic link's target bytes. `None` for every other type.
    pub target: Option<Vec<u8>>,
    /// A device's `(major, minor)` numbers. `None` for every other type.
    pub device: Option<(u32, u32)>,
    /// The hard-link group this name belongs to: every name of one multiply-linked inode
    /// carries the same token, counted from 1 in manifest order. Zero for an inode with one
    /// name, and always zero for a directory.
    pub inode_token: u64,
    /// Extended attributes, by name.
    pub xattrs: BTreeMap<String, Vec<u8>>,
}

impl FileEntry {
    /// The entry's path as the booted system names it: `/` for the root, `/<name>`
    /// otherwise. The form a report and [`PER_IMAGE_PATHS`] use.
    pub fn path(&self) -> String {
        format!("/{}", self.name)
    }
}

/// A filesystem's manifest: its root directory and every name below it.
///
/// Built from a walk with [`FileManifest::assemble`] or read back with
/// [`FileManifest::parse`]. Either way the entries are in the order the manifest writes
/// them, which is pre-order with siblings sorted by name bytes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileManifest {
    /// The root directory. Its `name` is empty.
    pub root: FileEntry,
    /// Every other entry, in manifest order.
    pub entries: Vec<FileEntry>,
}

/// Why a tree cannot be written as a manifest, or a manifest cannot be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum FilesError {
    /// A name the draft cannot encode: not UTF-8, carrying a control character, a `.` or
    /// `..` component, an empty component, or the reserved manifest name.
    #[error("the manifest cannot name {name:?}: {why}")]
    InvalidName {
        /// The offending name, lossily decoded where it was not UTF-8.
        name: String,
        /// What is wrong with it.
        why: &'static str,
    },
    /// Two entries name the same path.
    #[error("{name:?} appears twice")]
    Duplicate {
        /// The repeated name.
        name: String,
    },
    /// An entry whose parent is not a directory the manifest lists.
    #[error("{name:?} has no parent directory in the manifest")]
    Orphan {
        /// The entry.
        name: String,
    },
    /// An entry's metadata falls outside what the draft allows for its type.
    #[error("{name:?}: {why}")]
    InvalidEntry {
        /// The entry, or `/` for the root.
        name: String,
        /// What is wrong with it.
        why: String,
    },
    /// The byte stream is not a manifest boot2deb wrote.
    #[error("not a manifest this reader accepts: {0}")]
    Format(String),
}

/// A walked inode name, before the manifest orders it and assigns link tokens.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WalkedEntry {
    /// The entry, whose `inode_token` [`FileManifest::assemble`] overwrites.
    pub entry: FileEntry,
    /// The inode number the name points at. Two names with one number are one file.
    pub inode: u64,
}

impl FileManifest {
    /// Order a walked tree into a manifest.
    ///
    /// The entries are sorted into pre-order by their names' component bytes, which puts a
    /// directory directly before its contents whatever the characters of its siblings.
    /// Every name of an inode reached more than once receives an [`inode_token`], counted
    /// from 1 in that order.
    ///
    /// # Errors
    ///
    /// [`FilesError`] when an entry breaks the draft's rules:
    ///
    /// - A name cannot be encoded.
    /// - Two entries share a name.
    /// - An entry's parent is not a listed directory.
    /// - An entry's metadata is not what the draft allows for its type.
    ///
    /// [`inode_token`]: FileEntry::inode_token
    pub fn assemble(root: FileEntry, mut walked: Vec<WalkedEntry>) -> Result<Self, FilesError> {
        walked.sort_by(|a, b| order(&a.entry.name, &b.entry.name));
        let mut links: BTreeMap<u64, usize> = BTreeMap::new();
        for w in &walked {
            if w.entry.file_type != FileType::Dir {
                *links.entry(w.inode).or_default() += 1;
            }
        }
        let mut tokens: BTreeMap<u64, u64> = BTreeMap::new();
        let mut entries = Vec::with_capacity(walked.len());
        for w in walked {
            let mut entry = w.entry;
            entry.inode_token = if entry.file_type != FileType::Dir && links[&w.inode] > 1 {
                let next = tokens.len() as u64 + 1;
                *tokens.entry(w.inode).or_insert(next)
            } else {
                0
            };
            entries.push(entry);
        }
        let manifest = FileManifest { root, entries };
        manifest.validate()?;
        Ok(manifest)
    }

    /// Every rule [`parse`](Self::parse) enforces on a manifest it reads, so the writer
    /// never emits one the reader refuses.
    fn validate(&self) -> Result<(), FilesError> {
        check_entry(&self.root, true)?;
        let mut dirs: BTreeSet<&str> = BTreeSet::from([""]);
        let mut previous: Option<&str> = None;
        let mut next_token = 1;
        let mut groups: BTreeMap<u64, &FileEntry> = BTreeMap::new();
        for entry in &self.entries {
            check_name(&entry.name)?;
            check_entry(entry, false)?;
            if let Some(prev) = previous {
                match order(prev, &entry.name) {
                    std::cmp::Ordering::Less => {}
                    std::cmp::Ordering::Equal => {
                        return Err(FilesError::Duplicate {
                            name: entry.name.clone(),
                        })
                    }
                    std::cmp::Ordering::Greater => {
                        return Err(FilesError::Format(format!(
                            "{:?} comes after {prev:?}, out of pre-order name order",
                            entry.name
                        )))
                    }
                }
            }
            let parent = entry.name.rsplit_once('/').map_or("", |(p, _)| p);
            if !dirs.contains(parent) {
                return Err(FilesError::Orphan {
                    name: entry.name.clone(),
                });
            }
            if entry.file_type == FileType::Dir {
                dirs.insert(&entry.name);
            }
            if entry.inode_token != 0 {
                match groups.get(&entry.inode_token) {
                    None if entry.inode_token == next_token => {
                        groups.insert(entry.inode_token, entry);
                        next_token += 1;
                    }
                    None => {
                        return Err(FilesError::InvalidEntry {
                            name: entry.name.clone(),
                            why: format!(
                                "inodeToken {} out of sequence, {next_token} expected",
                                entry.inode_token
                            ),
                        })
                    }
                    Some(first) if !same_inode(first, entry) => {
                        return Err(FilesError::InvalidEntry {
                            name: entry.name.clone(),
                            why: format!(
                                "shares inodeToken {} with {:?} but not its metadata",
                                entry.inode_token, first.name
                            ),
                        })
                    }
                    Some(_) => {}
                }
            }
            previous = Some(&entry.name);
        }
        Ok(())
    }

    /// Write the manifest: the root object, every entry, and the trailer, each as one
    /// RFC 7464 record. One manifest always writes the same bytes.
    pub fn to_bytes(&self) -> Vec<u8> {
        let mut out = Vec::new();
        let mut record = |body: String| {
            out.push(RS);
            out.extend_from_slice(body.as_bytes());
            out.push(b'\n');
        };
        record(object(&self.root, true));
        for entry in &self.entries {
            record(object(entry, false));
        }
        record(trailer());
        out
    }

    /// Read a manifest boot2deb wrote.
    ///
    /// Strict where the draft is lenient, because what it reads back is its own output. A
    /// difference there is a defect to report, not a variation to tolerate. Every record
    /// must be the exact bytes the writer emits for the entry it describes. That refuses
    /// a field it does not write, a default written out, a duplicate key, and any other
    /// key order or spacing. It also refuses a missing trailer, anything after the
    /// trailer, and entries out of the writer's order.
    ///
    /// # Errors
    ///
    /// [`FilesError`] naming the first record or entry that breaks a rule.
    pub fn parse(bytes: &[u8]) -> Result<Self, FilesError> {
        let mut records = Vec::new();
        let mut rest = bytes;
        while !rest.is_empty() {
            let Some(body) = rest.strip_prefix(&[RS]) else {
                return Err(FilesError::Format(
                    "a record does not begin with the RS byte".into(),
                ));
            };
            let end = body.iter().position(|&b| b == b'\n').ok_or_else(|| {
                FilesError::Format("a record is not terminated by a line feed".into())
            })?;
            records.push(&body[..end]);
            rest = &body[end + 1..];
        }
        let (first, tail) = records
            .split_first()
            .ok_or_else(|| FilesError::Format("the manifest is empty".into()))?;
        let (last, middle) = tail
            .split_last()
            .ok_or_else(|| FilesError::Format("the manifest has no trailer".into()))?;
        if *last != trailer().as_bytes() {
            return Err(FilesError::Format(
                "the last record is not the trailer, or carries more than its media type".into(),
            ));
        }
        let root = read_canonical(first, true)?;
        let entries = middle
            .iter()
            .map(|r| read_canonical(r, false))
            .collect::<Result<Vec<_>, _>>()?;
        let manifest = FileManifest { root, entries };
        manifest.validate()?;
        Ok(manifest)
    }

    /// Every entry, the root first.
    pub fn all(&self) -> impl Iterator<Item = &FileEntry> {
        std::iter::once(&self.root).chain(&self.entries)
    }
}

/// The manifest order of two names: by path component, each compared by its bytes. A
/// directory therefore sorts directly before everything under it, which a plain byte sort
/// does not do when a sibling's name holds a byte below `/`.
fn order(a: &str, b: &str) -> std::cmp::Ordering {
    components(a).cmp(&components(b))
}

/// A name's path components as bytes, none for the root's empty name.
fn components(name: &str) -> Vec<&[u8]> {
    if name.is_empty() {
        Vec::new()
    } else {
        name.split('/').map(str::as_bytes).collect()
    }
}

/// Refuse a name the draft cannot encode.
fn check_name(name: &str) -> Result<(), FilesError> {
    let bad = |why| {
        Err(FilesError::InvalidName {
            name: name.to_string(),
            why,
        })
    };
    if name.is_empty() {
        return bad("only the root has an empty name");
    }
    if name.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7f) {
        return bad("it holds a control character");
    }
    if name
        .split('/')
        .any(|c| c.is_empty() || c == "." || c == "..")
    {
        return bad("it has an empty, `.` or `..` component, or a leading or trailing slash");
    }
    if name == RESERVED_NAME || name.starts_with(&format!("{RESERVED_NAME}.")) {
        return bad("the draft reserves it for the manifest file");
    }
    Ok(())
}

/// Refuse metadata the draft does not allow for the entry's type.
fn check_entry(e: &FileEntry, is_root: bool) -> Result<(), FilesError> {
    let bad = |why: String| {
        Err(FilesError::InvalidEntry {
            name: e.path(),
            why,
        })
    };
    if is_root && (e.file_type != FileType::Dir || !e.name.is_empty()) {
        return bad("the root must be an unnamed directory".into());
    }
    if e.mode > 0o7777 {
        return bad(format!("mode {:o} is past 0o7777", e.mode));
    }
    if e.file_type == FileType::Lnk && e.mode != 0 {
        return bad("a symbolic link carries no mode".into());
    }
    let sized = matches!(e.file_type, FileType::Reg | FileType::Lnk);
    if !sized && e.size != 0 {
        return bad(format!("a {} carries no size", e.file_type.as_str()));
    }
    match (&e.sha256, e.file_type, e.size) {
        (Some(h), FileType::Reg, s) if s > 0 => {
            if h.len() != 64 || !h.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f')) {
                return bad(format!("sha256 {h:?} is not 64 lowercase hex characters"));
            }
        }
        (None, FileType::Reg, 0) | (None, FileType::Lnk, _) => {}
        (None, FileType::Reg, _) => return bad("a non-empty regular file carries a sha256".into()),
        (Some(_), _, _) => return bad("only a non-empty regular file carries a sha256".into()),
        (None, _, _) => {}
    }
    match (&e.target, e.file_type) {
        (Some(t), FileType::Lnk) if t.len() as u64 == e.size && !t.is_empty() => {}
        (Some(_), FileType::Lnk) => {
            return bad("a link's size must be its non-empty target's length".into())
        }
        (None, FileType::Lnk) => return bad("a symbolic link carries its target".into()),
        (Some(_), _) => return bad("only a symbolic link carries a target".into()),
        (None, _) => {}
    }
    match (e.device, e.file_type) {
        (Some(_), FileType::Chr | FileType::Blk) | (None, _) => {}
        (Some(_), _) => return bad("only a device carries major and minor numbers".into()),
    }
    if matches!(e.file_type, FileType::Chr | FileType::Blk) && e.device.is_none() {
        return bad("a device carries its major and minor numbers".into());
    }
    if e.file_type == FileType::Dir && e.inode_token != 0 {
        return bad("a directory carries no inodeToken".into());
    }
    if let Some(name) = e
        .xattrs
        .keys()
        .find(|n| n.is_empty() || n.chars().any(|c| (c as u32) < 0x20 || c as u32 == 0x7f))
    {
        return bad(format!(
            "extended attribute name {name:?} is empty or holds a control character"
        ));
    }
    Ok(())
}

/// Whether two names of one hard-link group describe one inode.
fn same_inode(a: &FileEntry, b: &FileEntry) -> bool {
    (
        a.file_type,
        a.mode,
        a.uid,
        a.gid,
        a.mtime_nsec,
        a.size,
        &a.sha256,
        &a.target,
        a.device,
        &a.xattrs,
    ) == (
        b.file_type,
        b.mode,
        b.uid,
        b.gid,
        b.mtime_nsec,
        b.size,
        &b.sha256,
        &b.target,
        b.device,
        &b.xattrs,
    )
}

/// One compact JSON object with its keys in byte order, built field by field. Keys are
/// held sorted, so the order fields are added in does not reach the output.
#[derive(Default)]
struct Json {
    fields: BTreeMap<&'static str, String>,
}

impl Json {
    fn number(&mut self, key: &'static str, value: u64) {
        self.fields.insert(key, value.to_string());
    }

    fn string(&mut self, key: &'static str, value: &str) {
        self.fields.insert(key, quote(value));
    }

    fn raw(&mut self, key: &'static str, value: String) {
        self.fields.insert(key, value);
    }

    fn finish(self) -> String {
        let mut out = String::from("{");
        for (i, (key, value)) in self.fields.into_iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&quote(key));
            out.push(':');
            out.push_str(&value);
        }
        out.push('}');
        out
    }
}

/// A JSON string literal for `s`: quoted, with `"`, `\` and every control character
/// escaped as RFC 8259 requires, and everything else as UTF-8.
fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if (c as u32) < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c as u32);
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The JSON object for one entry, with the fields its type carries.
fn object(e: &FileEntry, is_root: bool) -> String {
    let mut o = Json::default();
    if is_root {
        o.string("mediaType", MEDIA_TYPE);
    } else {
        o.string("name", &e.name);
    }
    o.string("type", e.file_type.as_str());
    if e.file_type != FileType::Lnk {
        o.number("mode", u64::from(e.mode));
    }
    o.number("uid", u64::from(e.uid));
    o.number("gid", u64::from(e.gid));
    o.number("mTimeNSec", e.mtime_nsec);
    if matches!(e.file_type, FileType::Reg | FileType::Lnk) {
        o.number("size", e.size);
    }
    if let Some(hash) = &e.sha256 {
        o.string("sha256", hash);
    }
    if let Some(target) = &e.target {
        o.raw(
            "contents",
            format!(
                "[{{\"literal\":{}}}]",
                quote(&crate::base64::encode(target))
            ),
        );
    }
    if let Some((major, minor)) = e.device {
        o.number("major", u64::from(major));
        o.number("minor", u64::from(minor));
    }
    if e.inode_token != 0 {
        o.number("inodeToken", e.inode_token);
    }
    if !e.xattrs.is_empty() {
        let mut attrs = String::from("{");
        for (i, (name, value)) in e.xattrs.iter().enumerate() {
            if i > 0 {
                attrs.push(',');
            }
            let _ = write!(
                attrs,
                "{}:{}",
                quote(name),
                quote(&crate::base64::encode(value))
            );
        }
        attrs.push('}');
        o.raw(XATTRS_FIELD, attrs);
    }
    o.finish()
}

/// The closing record.
fn trailer() -> String {
    let mut trailer = Json::default();
    trailer.string("mediaType", TRAILER_MEDIA_TYPE);
    trailer.finish()
}

/// Read one entry record, and hold it to the bytes the writer emits for that entry.
fn read_canonical(record: &[u8], is_root: bool) -> Result<FileEntry, FilesError> {
    let entry = read_entry(&json_object(record)?, is_root)?;
    if object(&entry, is_root).as_bytes() != record {
        return Err(FilesError::InvalidEntry {
            name: entry.path(),
            why: "the record is not in the form the writer emits (a key out of order, a \
                  duplicate key, extra spacing, or a default written out)"
                .into(),
        });
    }
    Ok(entry)
}

/// Parse one record's JSON text into an object.
fn json_object(record: &[u8]) -> Result<serde_json::Map<String, serde_json::Value>, FilesError> {
    match serde_json::from_slice::<serde_json::Value>(record) {
        Ok(serde_json::Value::Object(map)) => Ok(map),
        Ok(_) => Err(FilesError::Format("a record is not a JSON object".into())),
        Err(e) => Err(FilesError::Format(format!("a record is not JSON: {e}"))),
    }
}

/// Read one entry object back, refusing any field boot2deb does not write.
fn read_entry(
    map: &serde_json::Map<String, serde_json::Value>,
    is_root: bool,
) -> Result<FileEntry, FilesError> {
    let label = map
        .get("name")
        .and_then(|v| v.as_str())
        .map_or_else(|| "/".to_string(), |n| format!("/{n}"));
    let bad = |why: String| FilesError::InvalidEntry {
        name: label.clone(),
        why,
    };
    const KNOWN: &[&str] = &[
        "contents",
        "gid",
        "inodeToken",
        "major",
        "mediaType",
        "minor",
        "mode",
        "mTimeNSec",
        "name",
        "sha256",
        "size",
        "type",
        "uid",
        XATTRS_FIELD,
    ];
    if let Some(key) = map.keys().find(|k| !KNOWN.contains(&k.as_str())) {
        return Err(bad(format!("unknown field {key:?}")));
    }
    let number = |key: &str| -> Result<Option<u64>, FilesError> {
        match map.get(key) {
            None => Ok(None),
            Some(v) => v
                .as_u64()
                .map(Some)
                .ok_or_else(|| bad(format!("{key} is not an unsigned integer"))),
        }
    };
    let small = |key: &str| -> Result<Option<u32>, FilesError> {
        number(key)?
            .map(|n| u32::try_from(n).map_err(|_| bad(format!("{key} {n} is past 32 bits"))))
            .transpose()
    };
    let string = |key: &str| -> Result<Option<&str>, FilesError> {
        match map.get(key) {
            None => Ok(None),
            Some(v) => v
                .as_str()
                .map(Some)
                .ok_or_else(|| bad(format!("{key} is not a string"))),
        }
    };
    let media = string("mediaType")?;
    let name = string("name")?;
    match (is_root, media, name) {
        (true, Some(MEDIA_TYPE), None) | (false, None, Some(_)) => {}
        (true, _, _) => {
            return Err(FilesError::Format(format!(
                "the first record is not a root object with mediaType {MEDIA_TYPE}"
            )))
        }
        (false, _, _) => return Err(bad("an entry must carry a name and no mediaType".into())),
    }
    let file_type = string("type")?
        .and_then(FileType::parse)
        .ok_or_else(|| bad("type is missing or not one the draft names".into()))?;
    let required = |key: &str, v: Option<u64>| v.ok_or_else(|| bad(format!("{key} is missing")));
    let target = match map.get("contents") {
        None => None,
        Some(serde_json::Value::Array(items)) if items.len() == 1 => {
            let literal = items[0]
                .as_object()
                .filter(|o| o.len() == 1)
                .and_then(|o| o.get("literal"))
                .and_then(|l| l.as_str())
                .ok_or_else(|| bad("contents is not one literal entry".into()))?;
            Some(
                crate::base64::decode(literal)
                    .ok_or_else(|| bad("a contents literal is not base64".into()))?,
            )
        }
        Some(_) => return Err(bad("contents is not one literal entry".into())),
    };
    let mut xattrs = BTreeMap::new();
    if let Some(value) = map.get(XATTRS_FIELD) {
        let attrs = value
            .as_object()
            .ok_or_else(|| bad(format!("{XATTRS_FIELD} is not an object")))?;
        for (k, v) in attrs {
            let bytes = v
                .as_str()
                .and_then(crate::base64::decode)
                .ok_or_else(|| bad(format!("extended attribute {k:?} is not base64")))?;
            xattrs.insert(k.clone(), bytes);
        }
    }
    let device = match (small("major")?, small("minor")?) {
        (Some(major), Some(minor)) => Some((major, minor)),
        (None, None) => None,
        _ => return Err(bad("major and minor come together".into())),
    };
    Ok(FileEntry {
        name: name.unwrap_or_default().to_string(),
        file_type,
        mode: if file_type == FileType::Lnk {
            if map.contains_key("mode") {
                return Err(bad("a symbolic link carries no mode".into()));
            }
            0
        } else {
            required("mode", small("mode")?.map(u64::from))? as u32
        },
        uid: required("uid", small("uid")?.map(u64::from))? as u32,
        gid: required("gid", small("gid")?.map(u64::from))? as u32,
        mtime_nsec: required("mTimeNSec", number("mTimeNSec")?)?,
        size: match file_type {
            FileType::Reg | FileType::Lnk => required("size", number("size")?)?,
            _ if map.contains_key("size") => {
                return Err(bad(format!("a {} carries no size", file_type.as_str())))
            }
            _ => 0,
        },
        sha256: string("sha256")?.map(str::to_string),
        target,
        device,
        inode_token: number("inodeToken")?.unwrap_or(0),
        xattrs,
    })
}

/// How two manifests differ, path by path.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct FilesDiff {
    /// Paths only the right-hand manifest lists, in manifest order.
    pub added: Vec<String>,
    /// Paths only the left-hand manifest lists, in manifest order.
    pub removed: Vec<String>,
    /// Paths both list with different metadata or content, in the left's order.
    pub changed: Vec<ChangedFile>,
}

/// One path whose entry differs between two manifests, and what differs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChangedFile {
    /// The path, as the booted system names it.
    pub path: String,
    /// The fields that differ, in a fixed order: `type`, `mode`, `uid`, `gid`, `mtime`,
    /// `size`, `sha256`, `target`, `device`, `hardlinks`, `xattrs`.
    pub fields: Vec<&'static str>,
}

impl FilesDiff {
    /// Whether the two manifests list the same paths with the same metadata.
    pub fn is_empty(&self) -> bool {
        self.added.is_empty() && self.removed.is_empty() && self.changed.is_empty()
    }

    /// Set aside the content changes of the [`PER_IMAGE_PATHS`], returning what remains
    /// and the paths whose content was set aside.
    ///
    /// Only the [`PER_IMAGE_FIELDS`] of those paths are set aside. A per-image path added
    /// or removed stays a difference, and so does any other field of one that changed.
    /// The remainder is empty exactly when the two manifests agree everywhere but in the
    /// per-image content.
    pub fn without_per_image(mut self) -> (FilesDiff, Vec<String>) {
        let mut set_aside = Vec::new();
        self.changed.retain_mut(|c| {
            if !PER_IMAGE_PATHS.contains(&c.path.as_str()) {
                return true;
            }
            let before = c.fields.len();
            c.fields.retain(|f| !PER_IMAGE_FIELDS.contains(f));
            if c.fields.len() < before {
                set_aside.push(c.path.clone());
            }
            !c.fields.is_empty()
        });
        (self, set_aside)
    }
}

/// A manifest's entries by path, the root under `/`.
fn by_path(m: &FileManifest) -> BTreeMap<String, &FileEntry> {
    m.all().map(|e| (e.path(), e)).collect()
}

/// Compare two manifests path by path.
///
/// Hard links are compared by group rather than by token, because a token is a counter.
/// One new link earlier in the tree renumbers every group after it. A name's `hardlinks`
/// field differs when the set of other names sharing its inode does.
pub fn compare(left: &FileManifest, right: &FileManifest) -> FilesDiff {
    let groups = |m: &FileManifest| {
        let mut by_token: BTreeMap<u64, Vec<String>> = BTreeMap::new();
        for e in m.all().filter(|e| e.inode_token != 0) {
            by_token.entry(e.inode_token).or_default().push(e.path());
        }
        by_token
    };
    let (l, r) = (by_path(left), by_path(right));
    let (lg, rg) = (groups(left), groups(right));
    let siblings = |groups: &BTreeMap<u64, Vec<String>>, e: &FileEntry| -> Vec<String> {
        groups
            .get(&e.inode_token)
            .map(|names| names.iter().filter(|n| **n != e.path()).cloned().collect())
            .unwrap_or_default()
    };
    let mut diff = FilesDiff::default();
    for e in left.all() {
        let path = e.path();
        let Some(o) = r.get(&path) else {
            diff.removed.push(path);
            continue;
        };
        let mut fields = Vec::new();
        for (field, differs) in [
            ("type", e.file_type != o.file_type),
            ("mode", e.mode != o.mode),
            ("uid", e.uid != o.uid),
            ("gid", e.gid != o.gid),
            ("mtime", e.mtime_nsec != o.mtime_nsec),
            ("size", e.size != o.size),
            ("sha256", e.sha256 != o.sha256),
            ("target", e.target != o.target),
            ("device", e.device != o.device),
            ("hardlinks", siblings(&lg, e) != siblings(&rg, o)),
            ("xattrs", e.xattrs != o.xattrs),
        ] {
            if differs {
                fields.push(field);
            }
        }
        if !fields.is_empty() {
            diff.changed.push(ChangedFile { path, fields });
        }
    }
    diff.added = right
        .all()
        .map(FileEntry::path)
        .filter(|p| !l.contains_key(p))
        .collect();
    diff
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> FileEntry {
        FileEntry {
            name: name.into(),
            file_type: FileType::Dir,
            mode: 0o755,
            uid: 0,
            gid: 0,
            mtime_nsec: 1_790_347_034_000_000_000,
            size: 0,
            sha256: None,
            target: None,
            device: None,
            inode_token: 0,
            xattrs: BTreeMap::new(),
        }
    }

    fn file(name: &str, content: &[u8]) -> FileEntry {
        use std::fmt::Write;
        // A stand-in digest: the tests need a well-formed, content-dependent hex string,
        // not SHA-256 itself, which core does not link.
        let mut hex = String::new();
        for i in 0..32 {
            let _ = write!(hex, "{:02x}", content.get(i).copied().unwrap_or(i as u8));
        }
        FileEntry {
            file_type: FileType::Reg,
            mode: 0o644,
            size: content.len() as u64,
            sha256: (!content.is_empty()).then_some(hex),
            ..dir(name)
        }
    }

    fn link(name: &str, target: &str) -> FileEntry {
        FileEntry {
            file_type: FileType::Lnk,
            mode: 0,
            size: target.len() as u64,
            target: Some(target.as_bytes().to_vec()),
            ..dir(name)
        }
    }

    fn walked(entry: FileEntry, inode: u64) -> WalkedEntry {
        WalkedEntry { entry, inode }
    }

    /// A small tree with every shape the writer handles: nested directories, a sibling
    /// whose name sorts below `/`, a symlink, an empty file, a hard link and an attribute.
    fn sample() -> FileManifest {
        let mut cap = file("usr/bin/ping", b"elf");
        cap.xattrs
            .insert("security.capability".into(), vec![1, 0, 0, 2, 0, 0x20]);
        FileManifest::assemble(
            dir(""),
            vec![
                walked(dir("usr"), 10),
                walked(dir("usr/bin"), 11),
                walked(cap, 12),
                walked(file("usr/bin/perl", b"perl"), 13),
                walked(file("usr/bin/perl5.40", b"perl"), 13),
                walked(dir("etc"), 20),
                walked(file("etc/shadow", b"root:!:"), 21),
                walked(file("etc/empty", b""), 22),
                walked(link("etc/mtab", "../proc/self/mounts"), 23),
                walked(dir("usr-local"), 30),
            ],
        )
        .unwrap()
    }

    /// The draft's structure: the root object first with the media type and no name, the
    /// trailer last and alone, and one RS-framed, LF-terminated record per object.
    #[test]
    fn the_manifest_is_a_json_sequence_framed_by_the_draft() {
        let bytes = sample().to_bytes();
        let records: Vec<&[u8]> = bytes
            .split(|&b| b == RS)
            .filter(|r| !r.is_empty())
            .collect();
        assert!(records.iter().all(|r| r.ends_with(b"\n")));
        let first = std::str::from_utf8(records[0]).unwrap();
        assert!(first.contains(r#""mediaType":"application/vnd.uapi.16.manifest""#));
        assert!(!first.contains("\"name\""));
        let last = std::str::from_utf8(records.last().unwrap()).unwrap();
        assert_eq!(
            last,
            "{\"mediaType\":\"application/vnd.uapi.16.trailer\"}\n"
        );
        // 1 root + 10 entries + 1 trailer.
        assert_eq!(records.len(), 12);
    }

    /// Pre-order with siblings sorted by component bytes: `usr` is followed by what is
    /// under it before `usr-local`, although `-` sorts below `/` as a byte.
    #[test]
    fn entries_are_in_pre_order_with_sorted_siblings() {
        let m = sample();
        let names: Vec<&str> = m.entries.iter().map(|e| e.name.as_str()).collect();
        assert_eq!(
            names,
            [
                "etc",
                "etc/empty",
                "etc/mtab",
                "etc/shadow",
                "usr",
                "usr/bin",
                "usr/bin/perl",
                "usr/bin/perl5.40",
                "usr/bin/ping",
                "usr-local",
            ]
        );
    }

    /// Each object's keys are sorted by byte and the output carries no whitespace, so one
    /// tree always writes the same bytes; the per-type fields follow the draft.
    #[test]
    fn objects_carry_sorted_keys_and_the_fields_their_type_allows() {
        let text = String::from_utf8(sample().to_bytes()).unwrap();
        let line = |name: &str| {
            text.split(['\u{1e}', '\n'])
                .find(|l| l.contains(&format!("\"name\":\"{name}\"")))
                .unwrap()
                .to_string()
        };
        assert_eq!(
            line("etc/empty"),
            r#"{"gid":0,"mTimeNSec":1790347034000000000,"mode":420,"name":"etc/empty","size":0,"type":"reg","uid":0}"#
        );
        // A link: no mode, its length as size, its target as a literal.
        let mtab = line("etc/mtab");
        assert!(mtab.contains(r#""contents":[{"literal":"Li4vcHJvYy9zZWxmL21vdW50cw=="}]"#));
        assert!(mtab.contains(r#""size":19"#) && !mtab.contains("\"mode\""));
        // A directory: no size, no hash.
        assert!(!line("usr").contains("\"size\""));
        // Both names of the hard link share token 1, the first link group in order.
        assert!(line("usr/bin/perl").contains(r#""inodeToken":1"#));
        assert!(line("usr/bin/perl5.40").contains(r#""inodeToken":1"#));
        assert!(!line("usr/bin/ping").contains("inodeToken"));
        // The capability, under the vendor field.
        assert!(line("usr/bin/ping")
            .contains(r#""xBoot2debXattrs":{"security.capability":"AQAAAgAg"}"#));
        assert!(!text.contains(": ") && !text.contains(", "));
    }

    /// What the writer emits, the reader accepts and reproduces exactly.
    #[test]
    fn a_manifest_round_trips_through_the_reader() {
        let m = sample();
        let bytes = m.to_bytes();
        let back = FileManifest::parse(&bytes).unwrap();
        assert_eq!(back, m);
        assert_eq!(back.to_bytes(), bytes);
    }

    /// The reader holds boot2deb's output to every rule it was written under.
    #[test]
    fn the_reader_refuses_what_the_writer_never_emits() {
        let good = String::from_utf8(sample().to_bytes()).unwrap();
        let cases: Vec<(String, &str)> = vec![
            // No trailer.
            (
                good.rsplit_once('\u{1e}').unwrap().0.to_string(),
                "the last record is not the trailer",
            ),
            // Data after the trailer.
            (
                format!("{good}\u{1e}{{}}\n"),
                "the last record is not the trailer",
            ),
            // A root without the media type.
            (
                good.replacen("\"mediaType\":\"application/vnd.uapi.16.manifest\",", "", 1),
                "first record",
            ),
            // An unknown field.
            (
                good.replacen("\"uid\":0}", "\"uid\":0,\"xOther\":1}", 1),
                "unknown field",
            ),
            // Entries out of order.
            (swap(&good, "etc/empty", "etc/mtab"), "pre-order"),
            // A `..` component.
            (
                good.replace("\"name\":\"usr-local\"", "\"name\":\"usr/../x\""),
                "component",
            ),
            // A default the writer omits, written out.
            (
                good.replacen("\"uid\":0}", "\"inodeToken\":0,\"uid\":0}", 1),
                "the form the writer emits",
            ),
            // Spacing the writer never puts in.
            (
                good.replacen("\"uid\":0}", "\"uid\": 0}", 1),
                "the form the writer emits",
            ),
            // A duplicate key, which a JSON parser keeps the last of.
            (
                good.replacen("\"uid\":0}", "\"uid\":0,\"uid\":0}", 1),
                "the form the writer emits",
            ),
        ];
        for (text, why) in cases {
            let err = FileManifest::parse(text.as_bytes())
                .unwrap_err()
                .to_string();
            assert!(err.contains(why), "expected {why:?} in {err}");
        }
    }

    /// Swap the records naming `a` and `b` in a written manifest.
    fn swap(text: &str, a: &str, b: &str) -> String {
        let mut records: Vec<String> = text.split('\u{1e}').map(str::to_string).collect();
        let find = |records: &[String], n: &str| {
            records
                .iter()
                .position(|r| r.contains(&format!("\"name\":\"{n}\"")))
                .unwrap()
        };
        let (i, j) = (find(&records, a), find(&records, b));
        records.swap(i, j);
        records.join("\u{1e}")
    }

    /// A name the draft cannot carry is refused where the tree is assembled, not written
    /// into a manifest no reader accepts.
    #[test]
    fn a_name_the_draft_cannot_carry_is_refused() {
        for name in [
            "",
            "a/./b",
            "a//b",
            "/a",
            "a/",
            "Uapi16Manifest",
            "Uapi16Manifest.gz",
            "a\nb",
        ] {
            let err = FileManifest::assemble(dir(""), vec![walked(file(name, b"x"), 1)]);
            assert!(err.is_err(), "{name:?} was accepted");
        }
        // Deeper in the tree the reserved name is an ordinary one.
        assert!(FileManifest::assemble(
            dir(""),
            vec![
                walked(dir("srv"), 1),
                walked(file("srv/Uapi16Manifest", b"x"), 2)
            ]
        )
        .is_ok());
        // An entry whose parent directory is not listed.
        assert!(matches!(
            FileManifest::assemble(dir(""), vec![walked(file("a/b", b"x"), 1)]),
            Err(FilesError::Orphan { .. })
        ));
    }

    /// Two builds that differ only in the per-image password agree once its path is set
    /// aside, and the comparison says which fields moved where anything else differs.
    #[test]
    fn the_comparison_names_fields_and_sets_aside_the_per_image_password() {
        let a = sample();
        let mut b = sample();
        let shadow = b
            .entries
            .iter_mut()
            .find(|e| e.name == "etc/shadow")
            .unwrap();
        *shadow = file("etc/shadow", b"root:$6$other");
        let diff = compare(&a, &b);
        assert_eq!(diff.changed.len(), 1);
        assert_eq!(diff.changed[0].path, "/etc/shadow");
        assert_eq!(diff.changed[0].fields, ["size", "sha256"]);
        let (rest, set_aside) = diff.without_per_image();
        assert!(rest.is_empty());
        assert_eq!(set_aside, ["/etc/shadow"]);

        // The splice rewrites content only. A per-image path whose mode moved, or that is
        // gone, is still a difference, with only the content fields set aside.
        let mut loose = b.clone();
        let shadow = loose
            .entries
            .iter_mut()
            .find(|e| e.name == "etc/shadow")
            .unwrap();
        shadow.mode = 0o600;
        let (rest, set_aside) = compare(&a, &loose).without_per_image();
        assert_eq!(rest.changed[0].path, "/etc/shadow");
        assert_eq!(rest.changed[0].fields, ["mode"]);
        assert_eq!(set_aside, ["/etc/shadow"]);
        let mut gone = a.clone();
        gone.entries.retain(|e| e.name != "etc/shadow");
        let (rest, _) = compare(&a, &gone).without_per_image();
        assert_eq!(rest.removed, ["/etc/shadow"]);

        // A moved mtime elsewhere is a real difference and survives the exception.
        let mut c = sample();
        c.entries
            .iter_mut()
            .find(|e| e.name == "usr/bin/ping")
            .unwrap()
            .mtime_nsec += 1;
        let (rest, _) = compare(&a, &c).without_per_image();
        assert_eq!(rest.changed[0].path, "/usr/bin/ping");
        assert_eq!(rest.changed[0].fields, ["mtime"]);
    }

    /// Hard links compare by group. A new link group earlier in the tree renumbers every
    /// token after it, and that alone is not a change to the names it renumbers.
    #[test]
    fn hard_links_compare_by_group_not_by_token() {
        let a = sample();
        let mut walked_b = vec![
            walked(dir("usr"), 10),
            walked(dir("usr/bin"), 11),
            walked(file("usr/bin/perl", b"perl"), 13),
            walked(file("usr/bin/perl5.40", b"perl"), 13),
            walked(dir("etc"), 20),
            walked(file("etc/shadow", b"root:!:"), 21),
            walked(file("etc/empty", b""), 22),
            walked(link("etc/mtab", "../proc/self/mounts"), 23),
            walked(dir("usr-local"), 30),
        ];
        let mut cap = file("usr/bin/ping", b"elf");
        cap.xattrs
            .insert("security.capability".into(), vec![1, 0, 0, 2, 0, 0x20]);
        walked_b.push(walked(cap, 12));
        // A second name for etc/empty: token 1 now, which moves perl's group to token 2.
        walked_b.push(walked(file("etc/empty.bak", b""), 22));
        let b = FileManifest::assemble(dir(""), walked_b).unwrap();
        assert_eq!(
            b.entries
                .iter()
                .find(|e| e.name == "usr/bin/perl")
                .unwrap()
                .inode_token,
            2
        );
        let diff = compare(&a, &b);
        assert_eq!(diff.added, ["/etc/empty.bak"]);
        let paths: Vec<&str> = diff.changed.iter().map(|c| c.path.as_str()).collect();
        assert_eq!(paths, ["/etc/empty"], "only the newly linked name changed");
        assert_eq!(diff.changed[0].fields, ["hardlinks"]);
    }

    /// A file type is read from a mode's format bits, and bits naming no type are refused.
    #[test]
    fn a_file_type_comes_from_the_mode_format_bits() {
        assert_eq!(FileType::from_mode(0o100644), Some(FileType::Reg));
        assert_eq!(FileType::from_mode(0o040755), Some(FileType::Dir));
        assert_eq!(FileType::from_mode(0o120777), Some(FileType::Lnk));
        assert_eq!(FileType::from_mode(0o020600), Some(FileType::Chr));
        assert_eq!(FileType::from_mode(0o000644), None);
    }
}
