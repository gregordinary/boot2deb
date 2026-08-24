//! Minimal ELF reader for the one property a staged build artifact must be able to
//! prove about itself: the library search path baked into it.
//!
//! Pure and side-effect-free — it parses a byte slice, so the mapping is unit-testable
//! without a compiler or a target binary. It reads only what
//! [`runpath`] needs: the program header table, the `PT_DYNAMIC` segment, and the
//! string table the dynamic entries index into.
//!
//! Why this exists rather than shelling out to `readelf`: the check runs on the host
//! over a staged tree that was built for the *target* arch, and the answer must be the
//! same whether or not a cross binutils happens to be installed.

/// The library search path baked into an ELF object — its `DT_RUNPATH`, or its
/// legacy `DT_RPATH` when no `DT_RUNPATH` is present.
///
/// Returns `None` for a file that is not an ELF, has no dynamic segment (a static
/// binary), or carries neither tag. The two tags differ in a way that matters to a
/// bundled library set: `DT_RPATH` is searched transitively for a dependency's own
/// dependencies, while `DT_RUNPATH` applies **only** to the object that carries it —
/// so a library that must find its siblings needs its own entry, and checking the
/// executable alone would prove nothing about the libraries. Modern toolchains emit
/// `DT_RUNPATH` (`--enable-new-dtags`); both are accepted here because either
/// satisfies the object being asked about.
///
/// The value is the raw path string, which may hold several `:`-separated entries
/// and may contain `$ORIGIN`; interpreting it is the caller's business.
pub fn runpath(bytes: &[u8]) -> Option<String> {
    let elf = Elf::parse(bytes)?;
    let dynamic = elf.segment(PT_DYNAMIC)?;
    // The string table is addressed by virtual address, which has to be mapped back
    // through the loadable segments to a file offset.
    let strtab = elf.vaddr_to_offset(elf.dyn_value(dynamic, DT_STRTAB)?)?;
    // DT_RUNPATH wins where both are present: a loader that honours it ignores RPATH.
    let offset = elf
        .dyn_value(dynamic, DT_RUNPATH)
        .or_else(|| elf.dyn_value(dynamic, DT_RPATH))?;
    elf.string_at(strtab.checked_add(usize::try_from(offset).ok()?)?)
}

const PT_LOAD: u32 = 1;
const PT_DYNAMIC: u32 = 2;
const DT_NULL: u64 = 0;
const DT_STRTAB: u64 = 5;
const DT_RPATH: u64 = 15;
const DT_RUNPATH: u64 = 29;

/// A parsed-enough ELF: the header fields needed to walk program headers, plus the
/// class and byte order every subsequent read depends on.
struct Elf<'a> {
    bytes: &'a [u8],
    /// 64-bit object; `false` is 32-bit (armhf).
    wide: bool,
    /// Little-endian object; `false` is big-endian.
    little: bool,
    phoff: usize,
    phentsize: usize,
    phnum: usize,
}

impl<'a> Elf<'a> {
    /// Validate the identification bytes and read the program-header table location.
    /// `None` for anything that is not a little- or big-endian ELF of a known class.
    fn parse(bytes: &'a [u8]) -> Option<Self> {
        if bytes.get(..4)? != b"\x7fELF" {
            return None;
        }
        let wide = match bytes.get(4)? {
            1 => false,
            2 => true,
            _ => return None,
        };
        let little = match bytes.get(5)? {
            1 => true,
            2 => false,
            _ => return None,
        };
        // Header field offsets differ by class; both tables are fixed by the ABI.
        let (phoff_at, phentsize_at, phnum_at) = if wide { (32, 54, 56) } else { (28, 42, 44) };
        let mut elf = Elf {
            bytes,
            wide,
            little,
            phoff: 0,
            phentsize: 0,
            phnum: 0,
        };
        elf.phoff = usize::try_from(elf.word(phoff_at)?).ok()?;
        elf.phentsize = usize::from(elf.u16(phentsize_at)?);
        elf.phnum = usize::from(elf.u16(phnum_at)?);
        // A zero entry size would make the segment walk spin on one header.
        (elf.phentsize > 0).then_some(elf)
    }

    fn u16(&self, at: usize) -> Option<u16> {
        let b: [u8; 2] = self.bytes.get(at..at + 2)?.try_into().ok()?;
        Some(if self.little {
            u16::from_le_bytes(b)
        } else {
            u16::from_be_bytes(b)
        })
    }

    fn u32(&self, at: usize) -> Option<u32> {
        let b: [u8; 4] = self.bytes.get(at..at + 4)?.try_into().ok()?;
        Some(if self.little {
            u32::from_le_bytes(b)
        } else {
            u32::from_be_bytes(b)
        })
    }

    fn u64(&self, at: usize) -> Option<u64> {
        let b: [u8; 8] = self.bytes.get(at..at + 8)?.try_into().ok()?;
        Some(if self.little {
            u64::from_le_bytes(b)
        } else {
            u64::from_be_bytes(b)
        })
    }

    /// A class-width address/offset word: 8 bytes on a 64-bit object, 4 on a 32-bit one.
    fn word(&self, at: usize) -> Option<u64> {
        if self.wide {
            self.u64(at)
        } else {
            self.u32(at).map(u64::from)
        }
    }

    /// The file offset and size of the first program header of type `want`.
    ///
    /// The two classes order the program-header fields differently — 64-bit inserts
    /// `p_flags` before `p_offset` — so the field offsets are per-class rather than shared.
    fn segment(&self, want: u32) -> Option<(usize, usize)> {
        self.segments()
            .find(|&(ty, _, _, _)| ty == want)
            .and_then(|(_, offset, _, filesz)| {
                Some((usize::try_from(offset).ok()?, usize::try_from(filesz).ok()?))
            })
    }

    /// Every program header as `(p_type, p_offset, p_vaddr, p_filesz)`.
    fn segments(&self) -> impl Iterator<Item = (u32, u64, u64, u64)> + '_ {
        let (off_at, vaddr_at, filesz_at) = if self.wide { (8, 16, 32) } else { (4, 8, 16) };
        (0..self.phnum).filter_map(move |i| {
            let base = self.phoff.checked_add(i.checked_mul(self.phentsize)?)?;
            Some((
                self.u32(base)?,
                self.word(base.checked_add(off_at)?)?,
                self.word(base.checked_add(vaddr_at)?)?,
                self.word(base.checked_add(filesz_at)?)?,
            ))
        })
    }

    /// The value of dynamic tag `want` within the dynamic segment at `(offset, size)`.
    ///
    /// Entries are `(d_tag, d_val)` pairs of class-width words, terminated by
    /// `DT_NULL`; the scan stops there so trailing padding is never read as an entry.
    fn dyn_value(&self, (offset, size): (usize, usize), want: u64) -> Option<u64> {
        let stride = if self.wide { 16 } else { 8 };
        let mut at = offset;
        let end = offset.checked_add(size)?;
        while at.checked_add(stride)? <= end {
            let tag = self.word(at)?;
            if tag == DT_NULL {
                return None;
            }
            if tag == want {
                return self.word(at + stride / 2);
            }
            at += stride;
        }
        None
    }

    /// Map a virtual address to a file offset through the loadable segments.
    fn vaddr_to_offset(&self, vaddr: u64) -> Option<usize> {
        self.segments()
            .filter(|&(ty, _, _, _)| ty == PT_LOAD)
            .find_map(|(_, offset, seg_vaddr, filesz)| {
                let delta = vaddr.checked_sub(seg_vaddr)?;
                (delta < filesz).then(|| usize::try_from(offset + delta).ok())?
            })
    }

    /// The NUL-terminated string at a file offset, as UTF-8.
    fn string_at(&self, at: usize) -> Option<String> {
        let rest = self.bytes.get(at..)?;
        let end = rest.iter().position(|&b| b == 0)?;
        std::str::from_utf8(&rest[..end]).ok().map(str::to_string)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lay out a minimal but structurally valid ELF carrying one `PT_LOAD` and one
    /// `PT_DYNAMIC`, with `tag` naming `path` through the string table. Built by hand
    /// rather than checked in as a fixture so the class/endianness matrix is a
    /// parameter rather than a binary blob.
    fn synth(wide: bool, little: bool, tag: u64, path: &str) -> Vec<u8> {
        let (ehsize, phentsize) = if wide { (64usize, 56usize) } else { (52, 32) };
        let phoff = ehsize;
        let dyn_off = phoff + 2 * phentsize;
        // Two entries plus the DT_NULL terminator.
        let stride = if wide { 16 } else { 8 };
        let dyn_len = 3 * stride;
        let str_off = dyn_off + dyn_len;
        // One leading NUL so index 0 is the empty string, as a real strtab has.
        let mut strtab = vec![0u8];
        let path_index = strtab.len() as u64;
        strtab.extend_from_slice(path.as_bytes());
        strtab.push(0);

        let mut b = vec![0u8; str_off + strtab.len()];
        b[..4].copy_from_slice(b"\x7fELF");
        b[4] = if wide { 2 } else { 1 };
        b[5] = if little { 1 } else { 2 };

        let put_u16 = |b: &mut Vec<u8>, at: usize, v: u16| {
            let x = if little {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            };
            b[at..at + 2].copy_from_slice(&x);
        };
        let put_word = |b: &mut Vec<u8>, at: usize, v: u64| {
            if wide {
                let x = if little {
                    v.to_le_bytes()
                } else {
                    v.to_be_bytes()
                };
                b[at..at + 8].copy_from_slice(&x);
            } else {
                let v = v as u32;
                let x = if little {
                    v.to_le_bytes()
                } else {
                    v.to_be_bytes()
                };
                b[at..at + 4].copy_from_slice(&x);
            }
        };
        let put_u32 = |b: &mut Vec<u8>, at: usize, v: u32| {
            let x = if little {
                v.to_le_bytes()
            } else {
                v.to_be_bytes()
            };
            b[at..at + 4].copy_from_slice(&x);
        };

        // e_phoff / e_phentsize / e_phnum at their per-class offsets.
        let (phoff_at, phentsize_at, phnum_at) = if wide { (32, 54, 56) } else { (28, 42, 44) };
        put_word(&mut b, phoff_at, phoff as u64);
        put_u16(&mut b, phentsize_at, phentsize as u16);
        put_u16(&mut b, phnum_at, 2);

        // Program headers. Identity-map vaddr to file offset so the strtab lookup has
        // a mapping to resolve through.
        let (off_at, vaddr_at, filesz_at) = if wide { (8, 16, 32) } else { (4, 8, 16) };
        put_u32(&mut b, phoff, PT_LOAD);
        put_word(&mut b, phoff + off_at, 0);
        put_word(&mut b, phoff + vaddr_at, 0);
        put_word(&mut b, phoff + filesz_at, (str_off + strtab.len()) as u64);

        let ph1 = phoff + phentsize;
        put_u32(&mut b, ph1, PT_DYNAMIC);
        put_word(&mut b, ph1 + off_at, dyn_off as u64);
        put_word(&mut b, ph1 + vaddr_at, dyn_off as u64);
        put_word(&mut b, ph1 + filesz_at, dyn_len as u64);

        // DT_STRTAB, then the requested tag, then DT_NULL.
        put_word(&mut b, dyn_off, DT_STRTAB);
        put_word(&mut b, dyn_off + stride / 2, str_off as u64);
        put_word(&mut b, dyn_off + stride, tag);
        put_word(&mut b, dyn_off + stride + stride / 2, path_index);
        put_word(&mut b, dyn_off + 2 * stride, DT_NULL);

        b[str_off..str_off + strtab.len()].copy_from_slice(&strtab);
        b
    }

    #[test]
    fn reads_runpath_across_the_class_and_endianness_matrix() {
        for wide in [true, false] {
            for little in [true, false] {
                let b = synth(wide, little, DT_RUNPATH, "/opt/ffmpeg-rk/lib");
                assert_eq!(
                    runpath(&b).as_deref(),
                    Some("/opt/ffmpeg-rk/lib"),
                    "wide={wide} little={little}"
                );
            }
        }
    }

    #[test]
    fn falls_back_to_the_legacy_rpath_tag() {
        let b = synth(true, true, DT_RPATH, "/opt/ffmpeg-rk/lib");
        assert_eq!(runpath(&b).as_deref(), Some("/opt/ffmpeg-rk/lib"));
    }

    #[test]
    fn an_object_with_neither_tag_has_no_runpath() {
        // DT_SONAME (14) occupies the slot instead, so the dynamic segment is present
        // and well-formed but names no search path.
        let b = synth(true, true, 14, "libavcodec.so.62");
        assert_eq!(runpath(&b), None);
    }

    #[test]
    fn non_elf_and_truncated_input_are_not_a_panic() {
        assert_eq!(runpath(b""), None);
        assert_eq!(runpath(b"#!/bin/sh\n"), None);
        assert_eq!(runpath(&[0x7f, b'E', b'L', b'F']), None);
        // Every prefix of a valid object must fail cleanly rather than index past
        // the end: the packaging step runs this over whatever `make install` staged.
        let full = synth(true, true, DT_RUNPATH, "/opt/ffmpeg-rk/lib");
        for n in 0..full.len() {
            let _ = runpath(&full[..n]);
        }
    }

    #[test]
    fn a_multi_entry_search_path_is_returned_verbatim() {
        let b = synth(true, true, DT_RUNPATH, "/opt/ffmpeg-rk/lib:$ORIGIN/../lib");
        assert_eq!(
            runpath(&b).as_deref(),
            Some("/opt/ffmpeg-rk/lib:$ORIGIN/../lib")
        );
    }
}
