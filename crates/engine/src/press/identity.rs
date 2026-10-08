//! The identifiers a pressed image carries: the build's, or a set drawn for one medium.
//!
//! Every build of a recipe derives the same identifiers
//! ([`ImageIdentity::derive`]). Every medium written from its image therefore carries the
//! same GPT disk GUID, root PARTUUID, ext4 UUID, seed serial, and kernel-slot GUIDs. Two
//! such media in one machine look the same to the initramfs. It roots on the first
//! partition it finds with the PARTUUID it was given, which can be the other disk's.
//!
//! A press with [`PressIdentity::Fresh`] gives its one medium identifiers no other image
//! carries. The re-assembly stamps them into the GPT, the superblock, and the seed volume
//! as it lays those out. The tree is the rest of the work, because the build wrote the
//! root PARTUUID into it:
//!
//! | File | Who wrote the root PARTUUID there | What the press does |
//! | --- | --- | --- |
//! | `/etc/fstab` | the rootfs node | swaps the one `PARTUUID=` it names |
//! | `/boot/extlinux/extlinux.conf` | `mk_extlinux`, from fstab, on a `rockchip-rkbin` board | swaps every `root=PARTUUID=` |
//! | `/boot/depthcharge/*.img` | `depthchargectl`, from fstab, on a `depthcharge` board | replaces it with the kernel re-signed for the new root |
//!
//! The press then checks every file under `/etc` and `/boot` for any of the build's
//! identifiers, and refuses the image when one is left. A file nothing here rewrites is
//! one the press cannot vouch for.

use crate::error::EngineError;
use crate::image::ImageIdentity;
use boot2deb_core::model::ResolvedBoot;
use ferrosys::{EntryKind, FileContent, FileRange, SourceEntry};
use std::collections::HashMap;
use std::path::PathBuf;
use uuid::Uuid;

/// The identifiers a pressed image carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PressIdentity {
    /// The build's own identifiers, derived from the recipe. A streamed press carries
    /// these by construction, because it copies the build's bytes.
    Built(ImageIdentity),
    /// Identifiers drawn for this one medium ([`ImageIdentity::fresh`]), which replace
    /// the build's in the GPT, the superblock, the seed volume, and the tree. Only a
    /// re-assembly can carry them.
    Fresh {
        /// The identifiers the build derived. The kept rootfs names them, and the press
        /// replaces them.
        built: ImageIdentity,
        /// The identifiers this medium carries instead.
        fresh: ImageIdentity,
    },
}

impl PressIdentity {
    /// The identifiers the pressed image carries on disk.
    #[must_use]
    pub fn carried(&self) -> ImageIdentity {
        match self {
            PressIdentity::Built(identity) => *identity,
            PressIdentity::Fresh { fresh, .. } => *fresh,
        }
    }

    /// The identifiers the build derived, which the kept artifacts carry.
    #[must_use]
    pub fn built(&self) -> ImageIdentity {
        match self {
            PressIdentity::Built(identity)
            | PressIdentity::Fresh {
                built: identity, ..
            } => *identity,
        }
    }

    /// Whether this press draws identifiers of its own.
    #[must_use]
    pub fn is_fresh(&self) -> bool {
        matches!(self, PressIdentity::Fresh { .. })
    }
}

/// The root entry in `/etc/fstab`, which the rootfs node writes on every image.
const FSTAB: &[u8] = b"/etc/fstab";

/// The u-boot menu `mk_extlinux` writes on a `rockchip-rkbin` board.
const EXTLINUX_CONF: &[u8] = b"/boot/extlinux/extlinux.conf";

/// The tree prefixes [`TreeRekey::check`] reads. Configuration lives in the first, and
/// the boot payloads and bootloader menu in the second.
const CHECKED_PREFIXES: [&[u8]; 2] = [b"/etc/", b"/boot/"];

/// The length of a hyphenated UUID, the only spelling the build writes.
const UUID_TEXT_LEN: usize = 36;

/// How often a rewritten file names the build's root PARTUUID.
#[derive(Debug, Clone, Copy)]
enum Occurrences {
    /// Exactly once, as the one root line of `/etc/fstab`.
    Once,
    /// One or more times, one per boot entry of an extlinux menu.
    AtLeastOnce,
}

/// The rewrite a fresh identity makes to the tree a press re-assembles.
///
/// Built by [`press_image`](crate::image::press_image) and applied through the format's
/// entry list, so every change happens to the parsed tree and no file is staged on disk.
/// The one exception is the signed kernel, which the image node re-signs before the format
/// because the kernel slot needs it too.
#[derive(Debug)]
pub(crate) struct TreeRekey {
    /// The identifiers the build derived, which the tree names.
    built: ImageIdentity,
    /// The identifiers that replace them.
    fresh: ImageIdentity,
    /// Whether the boot method writes an extlinux menu that roots on the PARTUUID.
    extlinux: bool,
    /// The tree path of the signed kernel image, and the re-signed file that replaces it.
    kpart: Option<(Vec<u8>, PathBuf)>,
}

impl TreeRekey {
    /// The rewrite from `built` to `fresh` for an image booted by `boot`.
    pub(crate) fn new(built: ImageIdentity, fresh: ImageIdentity, boot: &ResolvedBoot) -> Self {
        TreeRekey {
            built,
            fresh,
            extlinux: matches!(boot, ResolvedBoot::RockchipRkbin(_)),
            kpart: None,
        }
    }

    /// Replace the tree's signed kernel image at `tree_path` with `resigned`, the file
    /// the image node re-signed for the fresh root and places in the kernel slot.
    pub(crate) fn with_kpart(mut self, tree_path: Vec<u8>, resigned: PathBuf) -> Self {
        self.kpart = Some((tree_path, resigned));
        self
    }

    /// Rewrite the tree's references to the build's root PARTUUID. Returns one log line
    /// per file changed.
    ///
    /// Each rewritten entry keeps its mode, ownership, times, and extended attributes, so
    /// only its content differs from the build's.
    ///
    /// # Errors
    ///
    /// [`EngineError::IdentityRewrite`] when a file the boot method writes is missing, is
    /// not a regular file, or does not name the build's root PARTUUID the way the build
    /// writes it.
    pub(crate) fn apply(&self, entries: &mut [SourceEntry]) -> Result<Vec<String>, EngineError> {
        let old = hyphenated(self.built.rootfs_partuuid);
        let new = hyphenated(self.fresh.rootfs_partuuid);
        let mut log = Vec::new();

        let count = rewrite_text(
            entries,
            FSTAB,
            &format!("PARTUUID={old}"),
            &format!("PARTUUID={new}"),
            Occurrences::Once,
        )?;
        log.push(format!(
            "rewrote /etc/fstab: root PARTUUID {old} is now {new} ({count} line)"
        ));
        if self.extlinux {
            let count = rewrite_text(
                entries,
                EXTLINUX_CONF,
                &format!("root=PARTUUID={old}"),
                &format!("root=PARTUUID={new}"),
                Occurrences::AtLeastOnce,
            )?;
            log.push(format!(
                "rewrote /boot/extlinux/extlinux.conf: {count} boot entry(ies) root on {new}"
            ));
        }
        if let Some((tree_path, resigned)) = &self.kpart {
            let len = std::fs::metadata(resigned)
                .map_err(|s| EngineError::io(resigned, s))?
                .len();
            let entry = regular_file(entries, tree_path)?;
            entry.kind = EntryKind::File(FileContent::Range(FileRange::at_path(
                resigned.clone(),
                0,
                len,
            )));
            log.push(format!(
                "replaced {} with the kernel partition re-signed for {new}",
                printable(tree_path)
            ));
        }
        Ok(log)
    }

    /// Refuse a tree in which a file under `/etc` or `/boot` still names one of the build's
    /// identifiers.
    ///
    /// It runs over the merged tree, additions included, so it also catches a copied file
    /// that names the build's root. It reads every regular file under the two prefixes,
    /// and matches each identifier in its hyphenated form, in either case.
    ///
    /// # Errors
    ///
    /// [`EngineError::IdentityRewrite`] naming each file found and the identifier it names.
    /// [`EngineError::Ext4Format`] when a file's content cannot be read.
    pub(crate) fn check(&self, entries: &[SourceEntry]) -> Result<(), EngineError> {
        let names = identifier_names(&self.built);
        let mut found = Vec::new();
        for entry in entries {
            if !CHECKED_PREFIXES.iter().any(|p| entry.path.starts_with(p)) {
                continue;
            }
            let EntryKind::File(content) = &entry.kind else {
                continue;
            };
            let bytes = content.read().map_err(|s| EngineError::Ext4Format {
                detail: format!("reading {}: {s}", printable(&entry.path)),
            })?;
            if let Some(name) = first_identifier(&bytes, &names) {
                found.push(format!("{} (the build's {name})", printable(&entry.path)));
            }
        }
        if found.is_empty() {
            return Ok(());
        }
        Err(EngineError::IdentityRewrite {
            detail: format!(
                "{} still name the build's identifiers, and the press does not rewrite them: {}",
                if found.len() == 1 { "a file" } else { "files" },
                found.join(", ")
            ),
        })
    }
}

/// Replace every `old` in the text file at `path` with `new`, holding the count to
/// `rule`. Returns the count.
fn rewrite_text(
    entries: &mut [SourceEntry],
    path: &[u8],
    old: &str,
    new: &str,
    rule: Occurrences,
) -> Result<usize, EngineError> {
    let entry = regular_file(entries, path)?;
    let EntryKind::File(content) = &entry.kind else {
        unreachable!("regular_file returns only regular files");
    };
    let bytes = content.read().map_err(|s| EngineError::Ext4Format {
        detail: format!("reading {}: {s}", printable(path)),
    })?;
    let text = String::from_utf8(bytes.into_owned()).map_err(|_| EngineError::IdentityRewrite {
        detail: format!("{} is not UTF-8 text", printable(path)),
    })?;
    let count = text.matches(old).count();
    let expected = match rule {
        Occurrences::Once => count == 1,
        Occurrences::AtLeastOnce => count >= 1,
    };
    if !expected {
        return Err(EngineError::IdentityRewrite {
            detail: format!(
                "{} names `{old}` {count} time(s), where the build writes it {}",
                printable(path),
                match rule {
                    Occurrences::Once => "exactly once",
                    Occurrences::AtLeastOnce => "once per boot entry",
                }
            ),
        });
    }
    entry.kind = EntryKind::File(FileContent::Owned(text.replace(old, new).into_bytes()));
    Ok(count)
}

/// The regular-file entry at `path`.
fn regular_file<'a>(
    entries: &'a mut [SourceEntry],
    path: &[u8],
) -> Result<&'a mut SourceEntry, EngineError> {
    let entry = entries.iter_mut().find(|e| e.path == path).ok_or_else(|| {
        EngineError::IdentityRewrite {
            detail: format!(
                "the rootfs carries no {}, which the build writes the root PARTUUID into",
                printable(path)
            ),
        }
    })?;
    if !matches!(entry.kind, EntryKind::File(_)) {
        return Err(EngineError::IdentityRewrite {
            detail: format!("{} in the rootfs is not a regular file", printable(path)),
        });
    }
    Ok(entry)
}

/// Each of `identity`'s identifiers in lowercase hyphenated form, mapped to its name.
fn identifier_names(identity: &ImageIdentity) -> HashMap<[u8; UUID_TEXT_LEN], &'static str> {
    let mut names = HashMap::new();
    let mut add = |uuid: Uuid, name: &'static str| {
        let mut key = [0u8; UUID_TEXT_LEN];
        key.copy_from_slice(hyphenated(uuid).as_bytes());
        names.insert(key, name);
    };
    add(identity.rootfs_partuuid, "root PARTUUID");
    add(identity.ext4_uuid, "ext4 UUID");
    add(identity.disk_guid, "GPT disk GUID");
    add(identity.seed_partuuid, "seed PARTUUID");
    for slot in identity.kpart_guids {
        add(slot, "kernel-slot GUID");
    }
    names
}

/// The name of the first identifier in `names` that `bytes` spells out, in either case.
///
/// One pass: a window is compared only where its hyphens sit where a UUID's do.
fn first_identifier(
    bytes: &[u8],
    names: &HashMap<[u8; UUID_TEXT_LEN], &'static str>,
) -> Option<&'static str> {
    bytes.windows(UUID_TEXT_LEN).find_map(|window| {
        if [8, 13, 18, 23].iter().any(|&i| window[i] != b'-') {
            return None;
        }
        let mut lower = [0u8; UUID_TEXT_LEN];
        for (out, byte) in lower.iter_mut().zip(window) {
            *out = byte.to_ascii_lowercase();
        }
        names.get(&lower).copied()
    })
}

/// A UUID in the lowercase hyphenated form the build writes everywhere it names one.
fn hyphenated(uuid: Uuid) -> String {
    uuid.hyphenated().to_string().to_ascii_lowercase()
}

/// An entry path for a message.
fn printable(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use boot2deb_core::{resolve_recipe, ConfigRoot, Overrides};
    use ferrosys::{Metadata, Timestamp};

    fn repo_root() -> ConfigRoot {
        ConfigRoot::new(
            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .ancestors()
                .nth(2)
                .unwrap()
                .to_path_buf(),
        )
    }

    fn boot_of(recipe: &str) -> ResolvedBoot {
        resolve_recipe(&repo_root(), recipe, &Overrides::default())
            .unwrap()
            .boot
    }

    fn file(path: &str, body: &str) -> SourceEntry {
        SourceEntry {
            path: path.as_bytes().to_vec(),
            kind: EntryKind::File(FileContent::Owned(body.as_bytes().to_vec())),
            meta: Metadata::new(0o644, Timestamp::from_secs(1_700_000_000)),
            xattrs: Vec::new(),
        }
    }

    fn body(entries: &[SourceEntry], path: &str) -> String {
        let entry = entries.iter().find(|e| e.path == path.as_bytes()).unwrap();
        let EntryKind::File(content) = &entry.kind else {
            panic!("{path} is a file");
        };
        String::from_utf8(content.read().unwrap().into_owned()).unwrap()
    }

    fn identities() -> (ImageIdentity, ImageIdentity) {
        (
            ImageIdentity::derive("turing-rk1/forky", "turing-rk1"),
            ImageIdentity::derive("a fresh seed", "turing-rk1"),
        )
    }

    fn fstab(root: Uuid) -> String {
        format!(
            "# regenerate extlinux.conf via /boot/mk_extlinux after editing the root entry\n\
             PARTUUID={}\t/\text4\terrors=remount-ro\t0      1\n\
             LABEL=data\t/srv\text4\tnofail\t0 2\n",
            hyphenated(root)
        )
    }

    fn extlinux(root: Uuid) -> String {
        format!(
            "default l0\nlabel l0\n\tappend root=PARTUUID={0} rw rootwait\n\
             label l1\n\tappend root=PARTUUID={0} rw rootwait single\n",
            hyphenated(root)
        )
    }

    #[test]
    fn a_fresh_identity_carries_the_fresh_ids_and_remembers_the_builds() {
        let (built, fresh) = identities();
        let id = PressIdentity::Fresh { built, fresh };
        assert_eq!(id.carried(), fresh);
        assert_eq!(id.built(), built);
        assert!(id.is_fresh());
        let id = PressIdentity::Built(built);
        assert_eq!(id.carried(), built);
        assert!(!id.is_fresh());
    }

    #[test]
    fn an_rkbin_tree_has_its_fstab_and_every_extlinux_entry_rewritten() {
        let (built, fresh) = identities();
        let mut entries = vec![
            file("/etc/fstab", &fstab(built.rootfs_partuuid)),
            file(
                "/boot/extlinux/extlinux.conf",
                &extlinux(built.rootfs_partuuid),
            ),
            file("/etc/hostname", "turing-rk1\n"),
        ];
        let rekey = TreeRekey::new(built, fresh, &boot_of("turing-rk1/forky"));
        rekey.apply(&mut entries).unwrap();
        assert_eq!(body(&entries, "/etc/fstab"), fstab(fresh.rootfs_partuuid));
        assert_eq!(
            body(&entries, "/boot/extlinux/extlinux.conf"),
            extlinux(fresh.rootfs_partuuid)
        );
        // The rest of the tree is the build's.
        assert_eq!(body(&entries, "/etc/hostname"), "turing-rk1\n");
        rekey.check(&entries).unwrap();
    }

    #[test]
    fn a_file_that_does_not_name_the_builds_root_as_the_build_writes_it_is_refused() {
        let (built, fresh) = identities();
        let rekey = TreeRekey::new(built, fresh, &boot_of("turing-rk1/forky"));

        // An fstab already naming another root: this is not the build's tree.
        let mut entries = vec![
            file("/etc/fstab", &fstab(fresh.rootfs_partuuid)),
            file(
                "/boot/extlinux/extlinux.conf",
                &extlinux(built.rootfs_partuuid),
            ),
        ];
        assert!(matches!(
            rekey.apply(&mut entries),
            Err(EngineError::IdentityRewrite { .. })
        ));

        // Two root lines, where the build writes one.
        let doubled = format!("{0}{0}", fstab(built.rootfs_partuuid));
        let mut entries = vec![
            file("/etc/fstab", &doubled),
            file(
                "/boot/extlinux/extlinux.conf",
                &extlinux(built.rootfs_partuuid),
            ),
        ];
        assert!(rekey.apply(&mut entries).is_err());

        // A rockchip-rkbin tree with no extlinux menu cannot boot the fresh root.
        let mut entries = vec![file("/etc/fstab", &fstab(built.rootfs_partuuid))];
        assert!(rekey.apply(&mut entries).is_err());
    }

    #[test]
    fn a_depthcharge_tree_has_its_fstab_rewritten_and_its_kernel_replaced() {
        let (built, fresh) = identities();
        let tmp = tempfile::tempdir().unwrap();
        let resigned = tmp.path().join("7.2.9-1-armv7.img");
        std::fs::write(
            &resigned,
            format!("signed for {}", hyphenated(fresh.rootfs_partuuid)),
        )
        .unwrap();
        let mut entries = vec![
            file("/etc/fstab", &fstab(built.rootfs_partuuid)),
            file(
                "/boot/depthcharge/7.2.9-1-armv7.img",
                &format!("signed for {}", hyphenated(built.rootfs_partuuid)),
            ),
        ];
        let rekey = TreeRekey::new(built, fresh, &boot_of("asus-c201/forky")).with_kpart(
            b"/boot/depthcharge/7.2.9-1-armv7.img".to_vec(),
            resigned.clone(),
        );
        rekey.apply(&mut entries).unwrap();
        assert_eq!(body(&entries, "/etc/fstab"), fstab(fresh.rootfs_partuuid));
        assert_eq!(
            body(&entries, "/boot/depthcharge/7.2.9-1-armv7.img"),
            std::fs::read_to_string(&resigned).unwrap()
        );
        rekey.check(&entries).unwrap();
    }

    #[test]
    fn a_file_left_naming_any_build_identifier_fails_the_check() {
        let (built, fresh) = identities();
        let rekey = TreeRekey::new(built, fresh, &boot_of("asus-c201/forky"));
        let upper = hyphenated(built.disk_guid).to_ascii_uppercase();
        let entries = vec![
            file("/etc/fstab", &fstab(fresh.rootfs_partuuid)),
            file("/etc/site.conf", &format!("disk = {upper}\n")),
            // Outside /etc and /boot, so not read: an embedded install image carries
            // the build's identity by design.
            file(
                "/var/lib/boot2deb/install/note",
                &hyphenated(built.rootfs_partuuid),
            ),
        ];
        let Err(EngineError::IdentityRewrite { detail }) = rekey.check(&entries) else {
            panic!("the check passed a tree naming the build's disk GUID");
        };
        assert!(
            detail.contains("/etc/site.conf (the build's GPT disk GUID)"),
            "{detail}"
        );
        assert!(!detail.contains("/var/lib"), "{detail}");
    }
}
