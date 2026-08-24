//! Stream a built image artifact into its destination file: decompress-as-written,
//! one pass, with a digest tap.
//!
//! The image artifact is `.img` raw or `.img.xz` / `.img.gz` as the
//! build compressed it ([`Container::of`] reads the extension), and the stream
//! goes straight from the decoder into the destination — no staged raw file, no
//! temp space. The SHA-256 of the *decompressed* bytes is computed on the way
//! through, so [verification](super::verify) costs one re-read rather than a
//! second decompress.
//!
//! Progress is reported from the compressed input's consumption against its file
//! length — the honest coordinate: it advances at the rate the operation actually
//! proceeds, whatever the compression ratio does locally.

use crate::error::EngineError;
use crate::event::Step;
use sha2::{Digest, Sha256};
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;

/// Chunk size for the copy loop: large enough that the write pattern is friendly
/// to eMMC/SD erase blocks, small enough to keep progress lively.
const CHUNK: usize = 4 << 20;

/// The compression container an image artifact is in, read from its extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Container {
    /// A raw `.img` — no decoder in the path.
    Raw,
    /// `.img.xz`.
    Xz,
    /// `.img.gz`.
    Gz,
}

impl Container {
    /// Read the container off the artifact's file name.
    ///
    /// # Errors
    ///
    /// [`EngineError::ArtifactMissing`]-shaped invalidity is not this function's
    /// business; an unrecognized extension is a typed error naming the accepted
    /// set, since it means the caller pointed at something a build did not write.
    pub fn of(artifact: &Path) -> Result<Container, EngineError> {
        let name = artifact.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.ends_with(".img") {
            Ok(Container::Raw)
        } else if name.ends_with(".img.xz") {
            Ok(Container::Xz)
        } else if name.ends_with(".img.gz") {
            Ok(Container::Gz)
        } else {
            Err(EngineError::ImageFileInvalid {
                target: artifact.display().to_string(),
                detail: "not an image artifact (want .img, .img.xz or .img.gz)".into(),
            })
        }
    }
}

/// What a completed write measured: the decompressed length and its SHA-256.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrittenImage {
    /// Decompressed bytes written to the destination.
    pub bytes: u64,
    /// SHA-256 of those bytes, lowercase hex — what verification re-reads against.
    pub sha256: String,
}

/// A reader that counts what has been consumed, for progress against the
/// compressed input's length. The count lives behind a shared `Rc<Cell<..>>` so
/// it stays readable while the decoder owns the reader.
struct CountingReader<R> {
    inner: R,
    consumed: std::rc::Rc<std::cell::Cell<u64>>,
}

impl<R: Read> Read for CountingReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.consumed.set(self.consumed.get() + n as u64);
        Ok(n)
    }
}

/// Decompress `artifact` into `dest`, hashing the decompressed stream, reporting
/// progress on `step`.
///
/// `dest` is any writer — in practice the output file `press` creates. The
/// caller owns durability (`fsync`); this function only moves and measures
/// bytes.
///
/// # Errors
///
/// [`EngineError`] for an unreadable artifact, a decoder error (a truncated or
/// corrupt compressed stream), or a failed write — each naming the path it was
/// reading or writing.
pub fn stream_image(
    artifact: &Path,
    dest: &mut dyn Write,
    step: &Step,
) -> Result<WrittenImage, EngineError> {
    let file = File::open(artifact).map_err(|e| EngineError::io(artifact, e))?;
    let compressed_len = file
        .metadata()
        .map_err(|e| EngineError::io(artifact, e))?
        .len();
    let container = Container::of(artifact)?;

    let consumed = std::rc::Rc::new(std::cell::Cell::new(0u64));
    let counting = CountingReader {
        inner: std::io::BufReader::with_capacity(1 << 20, file),
        consumed: std::rc::Rc::clone(&consumed),
    };
    let mut decoder: Box<dyn Read> = match container {
        Container::Raw => Box::new(counting),
        // A build writes one stream, but concatenated streams are legal xz and
        // `xz -T` produces them; accepting both means any file `xzcat` accepts.
        Container::Xz => Box::new(lzma_rust2::XzReader::new(counting, true)),
        Container::Gz => Box::new(flate2::read::MultiGzDecoder::new(counting)),
    };

    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut written: u64 = 0;
    let mut last_pct: u8 = 0;
    loop {
        let n = decoder
            .read(&mut buf)
            .map_err(|e| EngineError::io(artifact, e))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
        dest.write_all(&buf[..n])
            .map_err(|e| EngineError::io(artifact, e))?;
        written += n as u64;

        // Progress from the compressed side: it advances at the rate the input is
        // actually consumed, and holds 100 back for the completed stream. Clamped
        // because a file growing under the read could push consumed past the
        // length measured at open.
        if let Some(pct) = (consumed.get().min(compressed_len) * 100).checked_div(compressed_len) {
            let pct = pct as u8;
            if pct > last_pct {
                last_pct = pct;
                step.progress(pct.min(99));
            }
        }
    }
    drop(decoder);
    dest.flush().map_err(|e| EngineError::io(artifact, e))?;
    step.progress(100);

    Ok(WrittenImage {
        bytes: written,
        sha256: hex(&hasher.finalize()),
    })
}

/// Decompress just the first `len` bytes of `artifact` — enough to read the
/// primary GPT (34 sectors) out of a compressed image without streaming the
/// rest, which is what feeds the size report and the post-write table check.
///
/// # Errors
///
/// [`EngineError`] for an unreadable artifact or a corrupt stream; a stream
/// shorter than `len` returns what there is.
pub fn decompressed_prefix(artifact: &Path, len: usize) -> Result<Vec<u8>, EngineError> {
    let file = File::open(artifact).map_err(|e| EngineError::io(artifact, e))?;
    let reader = std::io::BufReader::with_capacity(1 << 20, file);
    let mut decoder: Box<dyn Read> = match Container::of(artifact)? {
        Container::Raw => Box::new(reader),
        Container::Xz => Box::new(lzma_rust2::XzReader::new(reader, true)),
        Container::Gz => Box::new(flate2::read::MultiGzDecoder::new(reader)),
    };
    let mut out = vec![0u8; len];
    let mut filled = 0;
    while filled < len {
        let n = decoder
            .read(&mut out[filled..])
            .map_err(|e| EngineError::io(artifact, e))?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    out.truncate(filled);
    Ok(out)
}

/// Lowercase hex of a digest.
pub(crate) fn hex(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;

    fn sink() -> impl Fn(Event) {
        |_| {}
    }

    /// A payload with recognizable structure and an incompressible tail, so a
    /// truncated decode cannot accidentally match.
    fn payload() -> Vec<u8> {
        let mut v = vec![0u8; 6 << 20];
        for (i, b) in v.iter_mut().enumerate() {
            *b = (i % 251) as u8;
        }
        v
    }

    fn xz_file(dir: &Path, name: &str, raw: &[u8]) -> std::path::PathBuf {
        let p = dir.join(name);
        let f = std::fs::File::create(&p).unwrap();
        let opts = lzma_rust2::XzOptions::with_preset(1);
        let mut w = lzma_rust2::XzWriter::new(f, opts).unwrap();
        w.write_all(raw).unwrap();
        w.finish().unwrap();
        p
    }

    fn sha256_hex(data: &[u8]) -> String {
        hex(&Sha256::digest(data))
    }

    #[test]
    fn container_reads_the_extension() {
        assert_eq!(Container::of(Path::new("a.img")).unwrap(), Container::Raw);
        assert_eq!(Container::of(Path::new("a.img.xz")).unwrap(), Container::Xz);
        assert_eq!(Container::of(Path::new("a.img.gz")).unwrap(), Container::Gz);
        assert!(Container::of(Path::new("a.tar")).is_err());
        // A container no build writes is not an image artifact.
        assert!(Container::of(Path::new("a.img.zst")).is_err());
    }

    /// The whole path: an xz artifact streams into a destination, the bytes match,
    /// and the digest is the digest of the decompressed stream.
    #[test]
    fn an_xz_image_streams_bytes_and_digest() {
        let tmp = tempfile::tempdir().unwrap();
        let raw = payload();
        let artifact = xz_file(tmp.path(), "t.img.xz", &raw);

        let sink_fn = sink();
        let step = Step::start(&sink_fn, "press");
        let mut dest = Vec::new();
        let out = stream_image(&artifact, &mut dest, &step).unwrap();
        step.finish();

        assert_eq!(dest, raw);
        assert_eq!(out.bytes, raw.len() as u64);
        assert_eq!(out.sha256, sha256_hex(&raw));
    }

    #[test]
    fn a_raw_image_streams_unchanged() {
        let tmp = tempfile::tempdir().unwrap();
        let raw = payload();
        let artifact = tmp.path().join("t.img");
        std::fs::write(&artifact, &raw).unwrap();

        let sink_fn = sink();
        let step = Step::start(&sink_fn, "press");
        let mut dest = Vec::new();
        let out = stream_image(&artifact, &mut dest, &step).unwrap();
        step.finish();
        assert_eq!(dest, raw);
        assert_eq!(out.sha256, sha256_hex(&raw));
    }

    /// A truncated compressed stream must fail the write, not silently produce a
    /// short image — this is the corrupt-download case.
    #[test]
    fn a_truncated_xz_stream_is_an_error() {
        let tmp = tempfile::tempdir().unwrap();
        let raw = payload();
        let artifact = xz_file(tmp.path(), "t.img.xz", &raw);
        let full = std::fs::read(&artifact).unwrap();
        let cut = tmp.path().join("cut.img.xz");
        std::fs::write(&cut, &full[..full.len() / 2]).unwrap();

        let sink_fn = sink();
        let step = Step::start(&sink_fn, "press");
        let mut dest = Vec::new();
        assert!(stream_image(&cut, &mut dest, &step).is_err());
        step.finish();
    }

    /// The prefix decode hands back exactly the head of the decompressed image —
    /// the 34 sectors the GPT checks need — without touching the rest.
    #[test]
    fn the_prefix_decode_reads_the_head() {
        let tmp = tempfile::tempdir().unwrap();
        let raw = payload();
        let artifact = xz_file(tmp.path(), "t.img.xz", &raw);
        let head = decompressed_prefix(&artifact, 17_408).unwrap();
        assert_eq!(head, raw[..17_408]);

        // Shorter than asked: what there is, not an error.
        let small = xz_file(tmp.path(), "s.img.xz", &raw[..1000]);
        assert_eq!(decompressed_prefix(&small, 17_408).unwrap().len(), 1000);
    }
}
