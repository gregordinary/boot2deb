//! Read a `.deb` into the [`DebContents`] the package rules
//! ([`boot2deb_core::debcheck`]) hold it to.
//!
//! A `.deb` is an `ar` archive of three members: `debian-binary`, then `control.tar`,
//! then `data.tar`. Either tar member is compressed with `xz`, `gzip` or `zstd`, or
//! stored as it is. This reads that container in-process and streams both tar members through their
//! decompressor, so no `dpkg-deb` is needed and no member is buffered whole. Only each
//! regular file's first [`ELF_IDENT_LEN`] bytes are read, which is what the ELF rules
//! need to identify it.
//!
//! Every package boot2deb produces passes through [`check`] before the build records
//! it as local-repository input. That is the one place a produced package is admitted,
//! whether it was compiled on this run or restored from the artifact cache.

use crate::error::EngineError;
use boot2deb_core::debcheck::{
    self, elf_ident, DebContents, DebEntry, EntryKind, Finding, ELF_IDENT_LEN,
};
use boot2deb_core::model::Arch;
use std::io::{BufReader, Read};
use std::path::Path;

/// The `ar` global header every archive opens with.
const AR_MAGIC: &[u8; 8] = b"!<arch>\n";

/// The length of one `ar` member header.
const AR_HEADER_LEN: usize = 60;

/// Read the package at `path` and hold it to every rule for a build targeting `arch`.
///
/// # Errors
///
/// A file that cannot be read, or is not a `.deb` this reader understands: a malformed
/// `ar` container, or a missing tar member. A tar member in an unknown compression, or
/// whose stream does not parse, is an error too. Rule findings are not errors, since
/// they are the result.
pub fn check(path: &Path, arch: Arch) -> Result<Vec<Finding>, EngineError> {
    Ok(debcheck::check(&read(path)?, arch))
}

/// Read the control fields and data members of the package at `path`.
///
/// # Errors
///
/// As for [`check`].
pub fn read(path: &Path) -> Result<DebContents, EngineError> {
    let malformed = |why: String| EngineError::io(path, std::io::Error::other(why));
    let file = std::fs::File::open(path).map_err(|e| EngineError::io(path, e))?;
    let mut ar = BufReader::new(file);
    let mut magic = [0u8; 8];
    ar.read_exact(&mut magic)
        .map_err(|e| malformed(format!("not an ar archive ({e})")))?;
    if &magic != AR_MAGIC {
        return Err(malformed("not an ar archive".into()));
    }
    let mut contents = DebContents::default();
    let (mut saw_control, mut saw_data) = (false, false);
    loop {
        let mut header = [0u8; AR_HEADER_LEN];
        match ar.read_exact(&mut header) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => break,
            Err(e) => return Err(EngineError::io(path, e)),
        }
        if &header[58..60] != b"`\n" {
            return Err(malformed("a corrupt ar member header".into()));
        }
        // GNU ar terminates a member name with `/`; the BSD form pads with spaces.
        let name = String::from_utf8_lossy(&header[..16])
            .trim_end()
            .trim_end_matches('/')
            .to_string();
        let size: u64 = String::from_utf8_lossy(&header[48..58])
            .trim()
            .parse()
            .map_err(|_| malformed(format!("ar member {name} has no valid size")))?;
        let mut member = (&mut ar).take(size);
        if let Some(compression) = name.strip_prefix("control.tar") {
            let control = read_control(decompress(&mut member, compression, path)?)
                .map_err(|e| malformed(format!("{name}: {e}")))?;
            contents.control = debcheck::parse_control(&control);
            saw_control = true;
        } else if let Some(compression) = name.strip_prefix("data.tar") {
            contents.entries = read_data(decompress(&mut member, compression, path)?)
                .map_err(|e| malformed(format!("{name}: {e}")))?;
            saw_data = true;
        }
        // Whatever the decompressor left unread, and the member itself when it is
        // neither tar, is drained so the next header is where the archive puts it.
        std::io::copy(&mut member, &mut std::io::sink()).map_err(|e| EngineError::io(path, e))?;
        // Members are aligned to an even offset.
        if size % 2 == 1 {
            let mut pad = [0u8; 1];
            ar.read_exact(&mut pad)
                .map_err(|e| EngineError::io(path, e))?;
        }
    }
    if !saw_control || !saw_data {
        return Err(malformed(
            "a .deb contains a control.tar and a data.tar member, and this does not".into(),
        ));
    }
    Ok(contents)
}

/// Wrap one tar member in the decompressor its suffix names (`""`, `.xz`, `.gz`,
/// `.zst`).
fn decompress<'a, R: Read + 'a>(
    member: R,
    suffix: &str,
    path: &Path,
) -> Result<Box<dyn Read + 'a>, EngineError> {
    Ok(match suffix {
        "" => Box::new(member),
        ".xz" => Box::new(lzma_rust2::XzReader::new(member, true)),
        ".gz" => Box::new(flate2::read::GzDecoder::new(member)),
        ".zst" => Box::new(
            ruzstd::decoding::StreamingDecoder::new(member)
                .map_err(|e| EngineError::io(path, std::io::Error::other(e.to_string())))?,
        ),
        other => {
            return Err(EngineError::io(
                path,
                std::io::Error::other(format!("an unknown tar compression '{other}'")),
            ))
        }
    })
}

/// The text of the `./control` member of a control tarball.
fn read_control(stream: impl Read) -> std::io::Result<String> {
    let mut archive = tar::Archive::new(stream);
    for entry in archive.entries()? {
        let mut entry = entry?;
        let path = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        if path == "./control" || path == "control" {
            let mut text = String::new();
            entry.read_to_string(&mut text)?;
            return Ok(text);
        }
    }
    Err(std::io::Error::other(
        "no control file in the control archive",
    ))
}

/// Every member of a data tarball, with each regular file's ELF identity.
fn read_data(stream: impl Read) -> std::io::Result<Vec<DebEntry>> {
    let mut archive = tar::Archive::new(stream);
    let mut out = Vec::new();
    for entry in archive.entries()? {
        let mut entry = entry?;
        let header = entry.header();
        let kind = match header.entry_type() {
            tar::EntryType::Regular | tar::EntryType::Continuous => EntryKind::File,
            tar::EntryType::Directory => EntryKind::Dir,
            tar::EntryType::Symlink => EntryKind::Symlink,
            tar::EntryType::Link => EntryKind::Hardlink,
            _ => EntryKind::Other,
        };
        let mode = header.mode()?;
        let uid = header.uid()?;
        let gid = header.gid()?;
        let path = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        let elf = if kind == EntryKind::File {
            let mut prefix = Vec::with_capacity(ELF_IDENT_LEN);
            (&mut entry)
                .take(ELF_IDENT_LEN as u64)
                .read_to_end(&mut prefix)?;
            elf_ident(&prefix)
        } else {
            None
        };
        out.push(DebEntry {
            path,
            kind,
            mode,
            uid,
            gid,
            elf,
        });
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use boot2deb_core::debcheck::Rule;

    /// One tar member to assemble a fixture archive from.
    struct Member<'a> {
        path: &'a str,
        kind: tar::EntryType,
        mode: u32,
        uid: u64,
        body: &'a [u8],
    }

    /// A tar stream holding `members` under their paths exactly as given. Written
    /// through the raw name field, because the builder's own path setter drops the
    /// `./` that `dpkg-deb` stores and the reader has to see.
    fn tarball(members: &[Member]) -> Vec<u8> {
        let mut builder = tar::Builder::new(Vec::new());
        for m in members {
            let mut header = tar::Header::new_gnu();
            header.set_entry_type(m.kind);
            header.set_mode(m.mode);
            header.set_uid(m.uid);
            header.set_gid(0);
            header.set_size(m.body.len() as u64);
            if m.kind == tar::EntryType::Symlink {
                header.set_link_name("target").unwrap();
            }
            let name = &mut header.as_old_mut().name;
            name[..m.path.len()].copy_from_slice(m.path.as_bytes());
            header.set_cksum();
            builder.append(&header, m.body).unwrap();
        }
        builder.into_inner().unwrap()
    }

    fn ar(members: &[(&str, Vec<u8>)]) -> Vec<u8> {
        let mut out = AR_MAGIC.to_vec();
        for (name, body) in members {
            out.extend(
                format!(
                    "{:<16}{:<12}{:<6}{:<6}{:<8}{:<10}`\n",
                    format!("{name}/"),
                    0,
                    0,
                    0,
                    100644,
                    body.len()
                )
                .bytes(),
            );
            out.extend(body);
            if body.len() % 2 == 1 {
                out.push(b'\n');
            }
        }
        out
    }

    fn xz(bytes: &[u8]) -> Vec<u8> {
        let mut w =
            lzma_rust2::XzWriter::new(Vec::new(), lzma_rust2::XzOptions::with_preset(1)).unwrap();
        std::io::Write::write_all(&mut w, bytes).unwrap();
        w.finish().unwrap()
    }

    fn gz(bytes: &[u8]) -> Vec<u8> {
        let mut w = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        std::io::Write::write_all(&mut w, bytes).unwrap();
        w.finish().unwrap()
    }

    fn elf64_aarch64() -> Vec<u8> {
        let mut e = vec![0u8; 64];
        e[..4].copy_from_slice(b"\x7fELF");
        e[4] = 2;
        e[5] = 1;
        e[18..20].copy_from_slice(&183u16.to_le_bytes());
        e
    }

    const CONTROL: &[u8] = b"Package: x\nVersion: 1\nArchitecture: arm64\n\
        Maintainer: boot2deb <build@boot2deb>\nDescription: fixture\n";

    fn deb(data: Vec<u8>, data_name: &str, control_name: &str, control: Vec<u8>) -> Vec<u8> {
        ar(&[
            ("debian-binary", b"2.0\n".to_vec()),
            (control_name, control),
            (data_name, data),
        ])
    }

    fn control_tar() -> Vec<u8> {
        tarball(&[Member {
            path: "./control",
            kind: tar::EntryType::Regular,
            mode: 0o644,
            uid: 0,
            body: CONTROL,
        }])
    }

    #[test]
    fn reads_the_members_and_identifies_their_elf_objects() {
        let elf = elf64_aarch64();
        let data = tarball(&[
            Member {
                path: "./usr/",
                kind: tar::EntryType::Directory,
                mode: 0o755,
                uid: 0,
                body: b"",
            },
            Member {
                path: "./usr/bin/tool",
                kind: tar::EntryType::Regular,
                mode: 0o755,
                uid: 0,
                body: &elf,
            },
            Member {
                path: "./usr/bin/link",
                kind: tar::EntryType::Symlink,
                mode: 0o777,
                uid: 0,
                body: b"",
            },
            Member {
                path: "./usr/share/doc/x/README",
                kind: tar::EntryType::Regular,
                mode: 0o644,
                uid: 0,
                body: b"odd",
            },
        ]);
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x.deb");
        std::fs::write(
            &path,
            deb(
                xz(&data),
                "data.tar.xz",
                "control.tar.gz",
                gz(&control_tar()),
            ),
        )
        .unwrap();

        let contents = read(&path).unwrap();
        assert_eq!(contents.control["Package"], "x");
        let kinds: Vec<(&str, EntryKind, bool)> = contents
            .entries
            .iter()
            .map(|e| (e.path.as_str(), e.kind, e.elf.is_some()))
            .collect();
        assert_eq!(
            kinds,
            [
                ("./usr/", EntryKind::Dir, false),
                ("./usr/bin/tool", EntryKind::File, true),
                ("./usr/bin/link", EntryKind::Symlink, false),
                ("./usr/share/doc/x/README", EntryKind::File, false),
            ]
        );
        assert_eq!(check(&path, Arch::Arm64).unwrap(), []);
        // The same bytes are a foreign object in an armhf build's package.
        let findings = check(&path, Arch::Armv7).unwrap();
        assert!(
            findings.iter().any(|f| f.rule == Rule::ForeignElf),
            "{findings:?}"
        );
    }

    #[test]
    fn an_uncompressed_member_and_an_odd_sized_one_both_read() {
        // An odd-sized member is followed by one pad byte the reader has to step over.
        let data = tarball(&[Member {
            path: "./etc/x.conf",
            kind: tar::EntryType::Regular,
            mode: 0o644,
            uid: 1000,
            body: b"k=v\n",
        }]);
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x.deb");
        std::fs::write(&path, deb(data, "data.tar", "control.tar", control_tar())).unwrap();
        let findings = check(&path, Arch::Arm64).unwrap();
        assert_eq!(findings.len(), 1, "{findings:?}");
        assert_eq!(findings[0].rule, Rule::Owner);
    }

    #[test]
    fn a_file_that_is_not_a_deb_is_an_error_not_a_finding() {
        let tmp = tempfile::tempdir().unwrap();
        let junk = tmp.path().join("junk.deb");
        std::fs::write(&junk, b"definitely not an archive").unwrap();
        assert!(read(&junk).is_err());
        // An ar archive with no data member is not a package either.
        let half = tmp.path().join("half.deb");
        std::fs::write(
            &half,
            ar(&[
                ("debian-binary", b"2.0\n".to_vec()),
                ("control.tar", control_tar()),
            ]),
        )
        .unwrap();
        let err = read(&half).unwrap_err().to_string();
        assert!(err.contains("data.tar"), "{err}");
    }
}
