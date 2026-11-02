//! The archive every bootstrap resolves against: the build's suite, pocket by pocket,
//! with the mirrors each pocket is fetched from.
//!
//! Each provisioned root draws on one [`Archive`]: the cross and packaging roots, the
//! target-arch sandbox, the image rootfs, and the availability queries. The archive is
//! built once from the resolved [distro](boot2deb_core::distro::ResolvedDistro) and
//! the snapshot mode. A build whose compiler and image resolved against different
//! archives therefore cannot be expressed.
//!
//! Every pocket the suite publishes is a source, not only the release pocket. An image
//! therefore ships the fixes published since its release, and the compiler that built
//! its kernel comes from the same archive state.

use crate::error::{cage_message, EngineError};
use boot2deb_core::distro::{Pocket, ResolvedDistro};
use boot2deb_core::lock::{SnapshotMode, SnapshotPin};
use ferroday_cage::provision::debian::{DebianBuilder, Repository};
use std::path::Path;

/// One pocket of an [`Archive`], with the mirrors it is fetched from in order.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PocketMirrors {
    /// The suite as the archive names the pocket: `forky`, `forky-updates`.
    pub suite: String,
    /// The pocket's mirror URLs, tried in order. Never empty. A live mirror, a
    /// point-in-time copy of it, or both, as the snapshot mode decides.
    pub mirrors: Vec<String>,
}

/// The archive a build's bootstraps resolve against: every pocket its suite
/// publishes, release pocket first, each with its mirror list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Archive {
    /// The pockets, release first. Never empty.
    pub pockets: Vec<PocketMirrors>,
    /// Whether any mirror is a point-in-time copy, whose release is past its
    /// `Valid-Until` by design. Every provisioner over this archive then accepts a
    /// signed but stale release, repository-wide.
    pub point_in_time: bool,
}

impl Archive {
    /// The archive `distro` describes, under snapshot `mode`.
    ///
    /// `snapshot` is the lock's captured pin, if any. Each pocket's mirror list comes
    /// from [`snapshot::pocket_mirrors`](crate::snapshot::pocket_mirrors), so every pocket
    /// is held to the same point in time.
    pub fn resolve(
        distro: &ResolvedDistro,
        snapshot: Option<&SnapshotPin>,
        mode: SnapshotMode,
    ) -> Result<Archive, EngineError> {
        let pockets = distro
            .pockets
            .iter()
            .map(|pocket: &Pocket| {
                Ok(PocketMirrors {
                    suite: pocket.suite.clone(),
                    mirrors: crate::snapshot::pocket_mirrors(pocket, snapshot, mode)?,
                })
            })
            .collect::<Result<Vec<_>, EngineError>>()?;
        Ok(Archive {
            pockets,
            point_in_time: mode != SnapshotMode::Off,
        })
    }

    /// The suite's release pocket, which every bootstrap is configured around.
    pub fn release(&self) -> &PocketMirrors {
        &self.pockets[0]
    }

    /// The suite the archive serves, as its release pocket names it.
    pub fn suite(&self) -> &str {
        &self.release().suite
    }

    /// One line per pocket, `<suite> <mirror> <mirror>...`, in order.
    ///
    /// The form a root's cache key and a compile node's identity fold. A root
    /// bootstrapped from a different pocket set or a different mirror is a different
    /// root, and a compiler from it a different compiler.
    pub fn identity(&self) -> Vec<String> {
        self.pockets
            .iter()
            .map(|p| format!("{} {}", p.suite, p.mirrors.join(" ")))
            .collect()
    }

    /// The files a bootstrap over this archive writes into its root for the pockets
    /// beside the release one. Each pocket gets a `sources.list.d` entry and a keyring
    /// copy.
    ///
    /// The image's own apt sources replace them, so the customize step removes each
    /// one. A build root keeps them, since it is never shipped.
    pub fn pocket_entries(&self) -> Vec<String> {
        self.pockets[1..]
            .iter()
            .flat_map(|p| {
                [
                    format!("/etc/apt/sources.list.d/{}.list", p.suite),
                    format!("/usr/share/keyrings/{}.gpg", p.suite),
                ]
            })
            .collect()
    }

    /// Point `builder`, a provisioner for [`suite`](Self::suite), at this archive.
    ///
    /// The release pocket becomes the primary repository: its mirrors, `components`,
    /// and `keyring`. Each further pocket joins as a [`Repository`] of its own, named
    /// for its suite and verified against the same keyring. A point-in-time archive
    /// relaxes the release freshness check on every one of them.
    pub(crate) fn configure(
        &self,
        builder: DebianBuilder,
        components: &[String],
        keyring: &Path,
    ) -> Result<DebianBuilder, EngineError> {
        let (primary, fallbacks) = self
            .release()
            .mirrors
            .split_first()
            .expect("a pocket's mirror list is never empty");
        let mut builder = builder
            .mirror(primary)
            .components(components.iter().cloned())
            .keyring(keyring);
        for fallback in fallbacks {
            builder = builder.mirror_fallback(fallback);
        }
        if self.point_in_time {
            builder = builder.allow_stale_release(true);
        }
        for pocket in &self.pockets[1..] {
            let (primary, fallbacks) = pocket
                .mirrors
                .split_first()
                .expect("a pocket's mirror list is never empty");
            let mut repo = Repository::builder(&pocket.suite)
                .mirror(primary)
                .components(components.iter().cloned())
                .keyring(keyring)
                .allow_stale_release(self.point_in_time)
                .name(&pocket.suite);
            for fallback in fallbacks {
                repo = repo.mirror_fallback(fallback);
            }
            let repo = repo.build().map_err(|e| EngineError::Bootstrap {
                context: format!("configure the {} pocket", pocket.suite),
                message: cage_message(&e),
            })?;
            builder = builder.repository(repo);
        }
        Ok(builder)
    }
}

/// The shipped config tree's archive for `suite`, live, with the distribution's full
/// component set: what a test standing in for a build's provisioner configures.
#[cfg(test)]
pub(crate) fn test_archive(suite: &str) -> (Archive, Vec<String>) {
    let root = boot2deb_core::ConfigRoot::new(test_root());
    let (name, layer) = root.distro_for_suite(suite).expect("a shipped suite");
    let distro = layer.resolve(&name, suite);
    let archive = Archive::resolve(&distro, None, SnapshotMode::Off).expect("a live archive");
    (archive, distro.components)
}

/// The shipped config tree's archive for `suite`, held to snapshot `timestamp` under
/// `mode`: what a test comparing a pinned build's roots with a live one's needs.
#[cfg(test)]
pub(crate) fn test_snapshot_archive(suite: &str, timestamp: &str, mode: SnapshotMode) -> Archive {
    let root = boot2deb_core::ConfigRoot::new(test_root());
    let (name, layer) = root.distro_for_suite(suite).expect("a shipped suite");
    let pin = SnapshotPin {
        timestamp: timestamp.to_string(),
        mode,
    };
    Archive::resolve(&layer.resolve(&name, suite), Some(&pin), mode).expect("a snapshot archive")
}

/// The shipped Debian archive keyring. A real one, because a provisioner parses the
/// keyring it is configured with and refuses one holding no usable key.
#[cfg(test)]
pub(crate) fn test_keyring() -> std::path::PathBuf {
    test_root().join("blobs/keyrings/debian-archive-keyring.gpg")
}

/// The shipped config root, two levels above this crate.
#[cfg(test)]
fn test_root() -> std::path::PathBuf {
    std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("the workspace root")
        .to_path_buf()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn debian() -> ResolvedDistro {
        let pocket = |suite: &str, mirror: &str, snapshot: &str| Pocket {
            suite: suite.into(),
            mirror: mirror.into(),
            snapshot: Some(snapshot.into()),
        };
        ResolvedDistro {
            name: "debian".into(),
            pockets: vec![
                pocket(
                    "forky",
                    "http://deb.debian.org/debian",
                    "https://snapshot.debian.org/archive/debian",
                ),
                pocket(
                    "forky-updates",
                    "http://deb.debian.org/debian",
                    "https://snapshot.debian.org/archive/debian",
                ),
                pocket(
                    "forky-security",
                    "http://deb.debian.org/debian-security",
                    "https://snapshot.debian.org/archive/debian-security",
                ),
            ],
            components: vec!["main".into()],
            keyring: "debian-archive-keyring.gpg".into(),
            image_keyring: "/usr/share/keyrings/debian-archive-keyring.gpg".into(),
        }
    }

    #[test]
    fn a_live_archive_carries_every_pocket_on_its_own_mirror() {
        let a = Archive::resolve(&debian(), None, SnapshotMode::Off).unwrap();
        assert!(!a.point_in_time);
        assert_eq!(a.suite(), "forky");
        assert_eq!(
            a.identity(),
            [
                "forky http://deb.debian.org/debian",
                "forky-updates http://deb.debian.org/debian",
                "forky-security http://deb.debian.org/debian-security",
            ]
        );
    }

    #[test]
    fn a_pinned_archive_holds_every_pocket_to_the_same_point_in_time() {
        let pin = SnapshotPin {
            timestamp: "20260628T083000Z".into(),
            mode: SnapshotMode::Pin,
        };
        let a = Archive::resolve(&debian(), Some(&pin), SnapshotMode::Pin).unwrap();
        assert!(a.point_in_time);
        assert_eq!(
            a.pockets[2].mirrors,
            ["https://snapshot.debian.org/archive/debian-security/20260628T083000Z/"]
        );
        assert_eq!(
            a.pockets[1].mirrors,
            ["https://snapshot.debian.org/archive/debian/20260628T083000Z/"]
        );
    }

    #[test]
    fn the_bootstrap_entries_name_every_pocket_but_the_release_one() {
        let a = Archive::resolve(&debian(), None, SnapshotMode::Off).unwrap();
        assert_eq!(
            a.pocket_entries(),
            [
                "/etc/apt/sources.list.d/forky-updates.list",
                "/usr/share/keyrings/forky-updates.gpg",
                "/etc/apt/sources.list.d/forky-security.list",
                "/usr/share/keyrings/forky-security.gpg",
            ]
        );
    }
}
