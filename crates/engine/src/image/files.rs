//! The rootfs file manifest: every file the formatted rootfs filesystem holds, read back
//! from the ext4 image and written as a UAPI.16 manifest ([`boot2deb_core::files`]).
//!
//! Read from the filesystem that ships rather than from the tar it was formatted from, so
//! the manifest describes the bytes on the partition. It lists `/lost+found`, and the
//! `/etc/shadow` the password splice rewrote. Each hard link is one shared inode. The reader is
//! ferrosys's, the same library that wrote the filesystem.
//!
//! Side effects: it reads an image file, and writes the manifest beside the image.

use crate::error::EngineError;
use crate::event::Step;
use boot2deb_core::files::{FileEntry, FileManifest, FileType, WalkedEntry};
use ferrosys::ext::ondisk::Inode;
use ferrosys::ext::Reader;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::io::{Read, Seek};
use std::path::Path;

/// The artifact name of an image's rootfs file manifest: `<stem>.rootfs.uapi16`.
pub fn manifest_name(stem: &str) -> String {
    format!("{stem}.rootfs.uapi16")
}

/// Walk the ext4 filesystem image at `ext4` into its file manifest.
///
/// # Errors
///
/// [`EngineError::Ext4Format`] when the image cannot be read, or holds a name or a time
/// the manifest format cannot carry.
pub fn manifest_of(ext4: &Path) -> Result<FileManifest, EngineError> {
    let file = std::fs::File::open(ext4).map_err(|s| EngineError::io(ext4, s))?;
    let reader = Reader::open(std::io::BufReader::new(file)).map_err(|e| read_failed(ext4, e))?;
    walk(reader, ext4)
}

/// Read the rootfs file manifest back out of a finished image artifact: the partition its
/// GPT labels `rootfs`, walked where it sits.
///
/// The walk seeks, and a compressed stream cannot. A `.img.xz` or `.img.gz` is therefore
/// decompressed first, into a sparse temporary file in `scratch`, which is removed when
/// this returns. A raw `.img` is read in place.
///
/// # Errors
///
/// [`EngineError`] when the artifact cannot be read or decompressed, has no rootfs
/// partition, or holds a tree the manifest format cannot carry.
pub fn manifest_of_image(artifact: &Path, scratch: &Path) -> Result<FileManifest, EngineError> {
    let partition = crate::image::inspect::rootfs_partition(artifact)?;
    let options = ferrosys::ext::OpenOptions::new().base(partition.start);
    if crate::press::write::Container::of(artifact)? == crate::press::write::Container::Raw {
        let file = std::fs::File::open(artifact).map_err(|s| EngineError::io(artifact, s))?;
        let reader = Reader::open_with(std::io::BufReader::new(file), &options)
            .map_err(|e| read_failed(artifact, e))?;
        return walk(reader, artifact);
    }
    let mut raw = tempfile::Builder::new()
        .prefix(".boot2deb-verify-")
        .tempfile_in(scratch)
        .map_err(|s| EngineError::io(scratch, s))?;
    decompress_sparse(artifact, raw.as_file_mut())?;
    let reader = Reader::open_with(
        std::io::BufReader::new(raw.reopen().map_err(|s| EngineError::io(raw.path(), s))?),
        &options,
    )
    .map_err(|e| read_failed(artifact, e))?;
    walk(reader, artifact)
}

/// Decompress `artifact` into `out`, seeking past every all-zero chunk rather than
/// writing it. An image is mostly unallocated filesystem, so the file stays sparse and
/// the write stays proportional to the data.
fn decompress_sparse(artifact: &Path, out: &mut std::fs::File) -> Result<(), EngineError> {
    use std::io::{SeekFrom, Write};
    let mut decoder = crate::press::write::open_decoded(artifact)?;
    let mut buf = vec![0u8; 1 << 20];
    let mut len: u64 = 0;
    loop {
        let n = decoder
            .read(&mut buf)
            .map_err(|s| EngineError::io(artifact, s))?;
        if n == 0 {
            break;
        }
        if buf[..n].iter().all(|&b| b == 0) {
            out.seek(SeekFrom::Current(n as i64))
                .map_err(|s| EngineError::io(artifact, s))?;
        } else {
            out.write_all(&buf[..n])
                .map_err(|s| EngineError::io(artifact, s))?;
        }
        len += n as u64;
    }
    // A trailing run of zeros was seeked over, not written, so the length is set.
    out.set_len(len).map_err(|s| EngineError::io(artifact, s))
}

/// Walk an open reader's tree into a manifest. `source` names the image in errors.
fn walk<R: Read + Seek>(mut reader: Reader<R>, source: &Path) -> Result<FileManifest, EngineError> {
    let root_inode = reader
        .inode(ferrosys::ext::model::ROOT_INO)
        .map_err(|e| read_failed(source, e))?;
    let root = describe(&mut reader, String::new(), &root_inode, source)?;
    let mut walked = Vec::new();
    reader
        .walk_with::<WalkError>(|r, w| {
            // A walk path is absolute; a manifest name is relative to the root.
            let name =
                String::from_utf8(w.path[1..].to_vec()).map_err(|_| EngineError::Ext4Format {
                    detail: format!(
                        "{} holds {:?}, a name the file manifest cannot carry (not UTF-8)",
                        source.display(),
                        String::from_utf8_lossy(&w.path)
                    ),
                })?;
            walked.push(WalkedEntry {
                entry: describe(r, name, &w.inode, source)?,
                inode: u64::from(w.number),
            });
            Ok(())
        })
        .map_err(|e| match e {
            WalkError::Read(e) => read_failed(source, e),
            WalkError::Entry(e) => e,
        })?;
    FileManifest::assemble(root, walked).map_err(|e| EngineError::Ext4Format {
        detail: format!("the file manifest of {}: {e}", source.display()),
    })
}

/// A walk that stopped: a read the reader failed, or an entry this module could not
/// describe.
enum WalkError {
    /// The reader's own failure.
    Read(ferrosys::ext::ReadError),
    /// An entry the manifest cannot carry, already worded.
    Entry(EngineError),
}

impl From<ferrosys::ext::ReadError> for WalkError {
    fn from(e: ferrosys::ext::ReadError) -> Self {
        WalkError::Read(e)
    }
}

impl From<EngineError> for WalkError {
    fn from(e: EngineError) -> Self {
        WalkError::Entry(e)
    }
}

/// One inode as a manifest entry: its type and ownership, its time, and what its type
/// carries — a regular file's digest, a link's target, a device's numbers.
fn describe<R: Read + Seek>(
    reader: &mut Reader<R>,
    name: String,
    inode: &Inode,
    source: &Path,
) -> Result<FileEntry, EngineError> {
    let label = if name.is_empty() {
        "/".to_string()
    } else {
        format!("/{name}")
    };
    let unencodable = |why: String| EngineError::Ext4Format {
        detail: format!("{} {label}: {why}", source.display()),
    };
    let mode = u32::from(inode.mode);
    let file_type = FileType::from_mode(mode)
        .ok_or_else(|| unencodable(format!("mode {mode:o} names no file type")))?;
    let secs = u64::try_from(inode.mtime.secs)
        .map_err(|_| unencodable("its modification time is before 1970".into()))?;
    let mtime_nsec = secs
        .checked_mul(1_000_000_000)
        .and_then(|n| n.checked_add(u64::from(inode.mtime.nanos)))
        .ok_or_else(|| unencodable("its modification time is past 2554".into()))?;
    let mut entry = FileEntry {
        name,
        file_type,
        mode: if file_type == FileType::Lnk {
            0
        } else {
            mode & 0o7777
        },
        uid: inode.uid,
        gid: inode.gid,
        mtime_nsec,
        size: 0,
        sha256: None,
        target: None,
        device: None,
        inode_token: 0,
        xattrs: BTreeMap::new(),
    };
    match file_type {
        FileType::Reg => {
            // The content streams through the hash rather than being held whole.
            let mut hasher = Sha256::new();
            let size = reader
                .read_data_to(inode, &mut hasher)
                .map_err(|e| read_failed(source, e))?;
            entry.size = size;
            entry.sha256 = (size > 0).then(|| crate::blobs::hex(&hasher.finalize()));
        }
        FileType::Lnk => {
            let target = reader
                .read_symlink(inode)
                .map_err(|e| read_failed(source, e))?;
            entry.size = target.len() as u64;
            entry.target = Some(target);
        }
        FileType::Chr | FileType::Blk => entry.device = Some(reader.device(inode)),
        FileType::Dir | FileType::Fifo | FileType::Sock => {}
    }
    for x in reader.xattrs(inode).map_err(|e| read_failed(source, e))? {
        let name = String::from_utf8(x.name)
            .map_err(|_| unencodable("an extended attribute name is not UTF-8".into()))?;
        entry.xattrs.insert(name, x.value);
    }
    Ok(entry)
}

/// Write the file manifest of the ext4 image at `ext4` to `dest`.
///
/// # Errors
///
/// [`manifest_of`]'s errors, or [`EngineError::Io`] when `dest` cannot be written.
pub fn write_manifest(ext4: &Path, dest: &Path, step: &Step) -> Result<(), EngineError> {
    let manifest = manifest_of(ext4)?;
    std::fs::write(dest, manifest.to_bytes()).map_err(|s| EngineError::io(dest, s))?;
    step.log(format!(
        "wrote the rootfs file manifest ({} entries) to {}",
        manifest.entries.len() + 1,
        dest.display()
    ));
    Ok(())
}

/// A read of the image that failed, named against the image.
fn read_failed(source: &Path, e: ferrosys::ext::ReadError) -> EngineError {
    EngineError::Ext4Format {
        detail: format!("reading {}: {e}", source.display()),
    }
}
