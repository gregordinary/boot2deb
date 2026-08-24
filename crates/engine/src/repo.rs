//! Local apt repo — assembles the build's own `.deb`s into a small apt
//! repository the rootfs node installs from, so the provisioner resolves our
//! packages together with their dependencies (against this repo plus the suite
//! mirror).
//!
//! The build makes every artifact a `.deb` (kernel, u-boot, MPP/RGA, ffmpeg-rk);
//! dropping them behind a trusted apt source lets the rootfs install a package
//! *list* — the substrate base set plus the selected features' packages — and
//! have the solver pull each deb's transitive deps from the mirror.
//!
//! The repo is a **`dists/`-structured** trusted `file://` mirror
//! ([`LocalDistsRepo`]): the provisioner speaks the standard
//! `dists/<suite>/…/Release` + `Packages` mirror layout and nothing else. It is
//! written by [`ferroday_cage::provision::debian::Pool`], the inverse of the
//! provisioner's own index reader, so this side needs no external tool and the
//! writer and the reader cannot disagree about the layout.

use crate::error::EngineError;
use crate::event::Step;
use ferroday_cage::provision::debian::Pool;
use std::path::{Path, PathBuf};

/// A `dists/`-structured trusted local apt repository — the shape the
/// ferroday-cage provisioner backend reads.
///
/// The provisioner speaks only the standard mirror layout: a
/// `dists/<suite>/Release` indexing a per-component `Packages` file, whose
/// packages carry a pool-relative `Filename`. So the build's own `.deb`s reach
/// the provisioner's resolution as one more `file://` mirror, trusted without a
/// signature (apt's `[trusted=yes]` / ferroday's `trust_unsigned`). The layout:
///
/// ```text
/// <dir>/pool/main/<p>/<pkg>/<pkg>_<ver>_<arch>.deb
/// <dir>/dists/<suite>/main/binary-<arch>/Packages
/// <dir>/dists/<suite>/Release
/// ```
///
/// The debs are the build's own freshly-generated output, so trusting the repo
/// unsigned is apt's own `file://` `[trusted=yes]` case — the provisioner refuses
/// `trust_unsigned` only over `http://`, never a local path.
pub struct LocalDistsRepo {
    /// Absolute repo root — the `file://` mirror URL base the provisioner fetches
    /// `dists/<suite>/…` and `pool/…` under.
    dir: PathBuf,
    /// The mirror URL naming [`dir`](Self::dir), as the [`Pool`] that owns the layout
    /// spells it. Taken from the pool rather than composed here, so the base the
    /// provisioner is pointed at and the tree the pool wrote cannot disagree.
    mirror_url: String,
}

/// The emitted `Release`'s `Origin`: who produced the archive.
///
/// It is the field apt's own documentation leads with for pinning
/// (`Pin: release o=boot2deb`) and one of the two an `apt policy` renders a repository
/// under, so leaving it unset would show the build's own packages under blanks on the
/// running board.
const POOL_ORIGIN: &str = "boot2deb";

/// The emitted `Release`'s `Label`: a short name for the archive itself.
///
/// Where [`POOL_ORIGIN`] names who produced the archive, this names the archive, and it
/// pins as `Pin: release l=boot2deb-local`.
///
/// It is also the name the rootfs registers this repository under — see
/// `rootfs`'s `LOCAL_REPO_NAME`, which is this constant — so the
/// `sources.list.d` entry, `apt policy`'s rendering, and a release pin all name one
/// thing by construction rather than by two constants staying in step.
pub(crate) const POOL_LABEL: &str = "boot2deb-local";

/// The emitted `Release`'s `Description`: one line, purely informational — nothing
/// resolves or pins on it — for a reader who asks what this repository is.
const POOL_DESCRIPTION: &str = "Packages boot2deb built for this image";

impl LocalDistsRepo {
    /// Assemble a `dists/`-structured trusted repo at `dir` from `debs` for
    /// `suite`/`arch`, emitting progress to `step`.
    ///
    /// [`Pool`] copies each `.deb` to its archive pool path, writes the component
    /// `Packages` index (with pool-relative `Filename`s the provisioner resolves
    /// against the `file://` base) and the `Release` that checksums it, stamped
    /// with the suite, the `main` component, and `<arch>` so the provisioner's
    /// release check accepts it — `trust_unsigned` skips the signature and the
    /// freshness bound, so no `Valid-Until` is needed.
    ///
    /// `source_date_epoch` pins the `Date` the `Release` carries, and pinning it is what
    /// makes the publish byte-reproducible: the indexes are a function of the package
    /// set, and the release is then a function of the indexes and this number — without
    /// it the release takes the wall clock and no two publishes of one package set agree.
    /// It is the same value the rootfs tar export clamps member mtimes to, so one
    /// lock-derived number dates everything the build emits. `None` is a build with no
    /// kernel tree to take an epoch from, where the release falls back to the publish
    /// time and this repository is simply not reproducible — the same amount of
    /// determinism the rest of that build has.
    ///
    /// Any prior contents of `dir` are removed first, so the repo reflects exactly
    /// `debs`: `Pool::publish` is incremental by design, and this repo is a
    /// per-build view of one artifact ledger rather than an accumulating pool.
    /// A `.deb` whose `Architecture` is neither `arch` nor `all` is rejected
    /// rather than indexed where nothing can resolve it. A relative `dir` is made
    /// absolute for the mirror URL, which cannot spell a relative path.
    pub fn assemble(
        dir: &Path,
        debs: &[PathBuf],
        suite: &str,
        arch: &str,
        source_date_epoch: Option<u64>,
        step: &Step,
    ) -> Result<LocalDistsRepo, EngineError> {
        let _ = std::fs::remove_dir_all(dir);
        step.log(format!(
            "assembling dists/ local apt repo from {} .deb(s) at {} ({suite}/{arch})",
            debs.len(),
            dir.display()
        ));
        let mut pool = Pool::at(dir)
            .suite(suite)
            .component("main")
            .architecture(arch)
            .origin(POOL_ORIGIN)
            .label(POOL_LABEL)
            .description(POOL_DESCRIPTION);
        if let Some(epoch) = source_date_epoch {
            // `Pool::date` takes signed Unix seconds; a lock epoch past 2038-in-u64 is
            // not a thing any kernel commit carries, and saturating is the only sane
            // reading of one that did.
            pool = pool.date(i64::try_from(epoch).unwrap_or(i64::MAX));
        }
        let fail = |e: ferroday_cage::provision::debian::DebianError| EngineError::Bootstrap {
            context: format!("publish the local .deb pool at {}", dir.display()),
            message: e.to_string(),
        };
        pool.publish(debs).map_err(fail)?;
        let mirror_url = pool.mirror_url().map_err(fail)?;

        Ok(LocalDistsRepo {
            dir: dir.to_path_buf(),
            mirror_url,
        })
    }

    /// The `file://` mirror URL for this repo — the base the provisioner fetches
    /// `dists/<suite>/…` and each package's pool `Filename` under. Built for a
    /// [`ferroday_cage`](crate::sandbox) `Repository::builder(...).mirror(...)`.
    ///
    /// The path is carried **verbatim, not percent-encoded**. The mirror base is
    /// concatenated with a pool-relative suffix and the result is handed to
    /// `HttpFetch`, which strips the `file://` scheme and opens what remains as a
    /// filesystem path — it does no percent-decoding, so an encoded byte would be
    /// looked up literally and a build under `/home/me/My Projects/…` would fail to
    /// find its own `.deb`s. The repo dir is boot2deb's own (`<cache>/…`), never
    /// attacker-supplied, and nothing downstream parses a query or fragment out of
    /// it. `unencoded_paths_reach_the_provisioners_fetcher` holds this to the
    /// fetcher's actual behavior rather than to an assumption about URL syntax.
    pub fn file_url(&self) -> &str {
        &self.mirror_url
    }

    /// The repo directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sandbox::{PackagingSandbox, SandboxRun};

    /// A stand-in `SOURCE_DATE_EPOCH`, as a lock's kernel commit date would supply.
    const EPOCH: u64 = 1_700_000_000;

    /// Build a minimal `.deb` (`<pkg>_<version>_arm64.deb`) under `dir` in the build's
    /// own packaging root, returning its path.
    ///
    /// Through the root rather than a host `dpkg-deb`, so the whole of this module's
    /// coverage runs on a host with no dpkg installed — which is the same thing the
    /// build itself now claims. `--nocheck` because the fixture's control stanza names
    /// only the fields the pool layout reads.
    fn make_deb(
        root: &PackagingSandbox,
        dir: &Path,
        pkg: &str,
        version: &str,
        step: &Step,
    ) -> PathBuf {
        let tree = dir.join(format!("{pkg}-tree"));
        std::fs::create_dir_all(tree.join("DEBIAN")).unwrap();
        std::fs::write(
            tree.join("DEBIAN/control"),
            format!(
                "Package: {pkg}\nVersion: {version}\nArchitecture: arm64\n\
                 Maintainer: test <t@example.invalid>\nDescription: test package\n"
            ),
        )
        .unwrap();
        let out = dir.join(format!("{pkg}_{version}_arm64.deb"));
        let argv = vec![
            "dpkg-deb".to_string(),
            "--build".to_string(),
            "--nocheck".to_string(),
            tree.to_string_lossy().into_owned(),
            out.to_string_lossy().into_owned(),
        ];
        root.run(
            &SandboxRun {
                work: &tree,
                binds: &[dir.to_path_buf()],
                env: &[],
                argv: &argv,
                context: "build a fixture deb",
            },
            step,
        )
        .expect("the packaging root builds the fixture deb");
        out
    }

    #[test]
    fn dists_repo_has_the_mirror_layout_the_provisioner_reads() {
        // Only the fixture needs a `.deb` at all: the pool itself is written by
        // ferroday-cage, and the fixture's dpkg comes from the packaging root — so no
        // part of this needs apt archive tooling on the host.
        let sink = |_: crate::event::Event| {};
        let step = Step::start(&sink, "repo");
        let Some(root) = crate::sandbox::packaging_root_for_tests(&step) else {
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let a = make_deb(&root, tmp.path(), "librockchip-mpp1", "1.5.0-1", &step);
        let b = make_deb(&root, tmp.path(), "ffmpeg-rk", "3e53143", &step);
        let repo_dir = tmp.path().join("localdists");

        let repo =
            LocalDistsRepo::assemble(&repo_dir, &[a, b], "forky", "arm64", Some(EPOCH), &step)
                .unwrap();

        // The debs live at their archive pool paths (a `lib*` package under its
        // `lib` + fourth letter), and the file:// URL bases at the repo root.
        assert!(repo_dir
            .join("pool/main/libr/librockchip-mpp1/librockchip-mpp1_1.5.0-1_arm64.deb")
            .exists());
        assert!(repo_dir
            .join("pool/main/f/ffmpeg-rk/ffmpeg-rk_3e53143_arm64.deb")
            .exists());
        assert_eq!(repo.file_url(), format!("file://{}", repo_dir.display()));

        // The Packages index sits at the standard dists path, and each Filename is
        // pool-relative to the mirror base (how the provisioner resolves the bytes).
        let packages =
            std::fs::read_to_string(repo_dir.join("dists/forky/main/binary-arm64/Packages"))
                .unwrap();
        assert!(packages.contains("Package: ffmpeg-rk"));
        assert!(packages.contains("Filename: pool/main/f/ffmpeg-rk/ffmpeg-rk_3e53143_arm64.deb"));

        // The Release declares the suite, component, and architecture the
        // provisioner's release check requires, and checksums the index.
        let release = std::fs::read_to_string(repo_dir.join("dists/forky/Release")).unwrap();
        assert!(release.contains("Suite: forky"));
        assert!(release.contains("Codename: forky"));
        assert!(release.contains("Components: main"));
        assert!(release.contains("Architectures: arm64"));
        assert!(release.contains("SHA256:"));
        assert!(release.contains("main/binary-arm64/Packages"));

        // And it declares who produced it. Without these an in-image `apt policy` shows
        // the build's own packages under blanks, and `Pin: release o=…` — the pinning
        // form apt's documentation leads with — has nothing to name.
        assert!(release.contains(&format!("Origin: {POOL_ORIGIN}")));
        assert!(release.contains(&format!("Label: {POOL_LABEL}")));
        assert!(release.contains(&format!("Description: {POOL_DESCRIPTION}")));
    }

    /// A publish of one package set is byte-identical however many times it runs.
    ///
    /// The indexes are a function of the package set and the release is a function of
    /// the indexes and the `Date`, so the date is the only thing between this repository
    /// and reproducibility — unpinned it takes the wall clock, and two builds of one
    /// lock disagree on a file that ships nothing but still gets recorded.
    ///
    /// Published with **no** `.deb`s, deliberately: the property under test belongs to
    /// the release rather than to the packages, and the fixture `.deb` the layout tests
    /// use needs a provisioned packaging root that a host without one skips. This
    /// assertion is the one that must run everywhere.
    #[test]
    fn a_pinned_date_makes_the_publish_byte_reproducible() {
        let sink = |_: crate::event::Event| {};
        let step = Step::start(&sink, "repo");
        let tmp = tempfile::tempdir().unwrap();

        let publish = |dir: &Path, epoch: Option<u64>| {
            LocalDistsRepo::assemble(dir, &[], "forky", "arm64", epoch, &step).unwrap();
            std::fs::read(dir.join("dists/forky/Release")).unwrap()
        };
        let first = publish(&tmp.path().join("a"), Some(EPOCH));
        let second = publish(&tmp.path().join("b"), Some(EPOCH));
        assert_eq!(first, second, "a pinned date publishes the same bytes");

        // The pinned value is the one that lands, not merely *a* fixed one — so the
        // release dates with the lock rather than with whatever this run inherited.
        let text = String::from_utf8(first).unwrap();
        let date = text
            .lines()
            .find_map(|l| l.strip_prefix("Date: "))
            .expect("the release carries a Date");
        assert!(
            date.contains("2023"),
            "the pinned epoch is what dates the release, got {date:?}"
        );
    }

    #[test]
    fn a_relative_repo_dir_still_yields_an_absolute_mirror_url() {
        // The work dir is whatever the caller passed `--work-dir`, and a relative one
        // is ordinary for a build run from its own tree. A `file://` URL names an
        // absolute path, so a relative repo dir has to be resolved before it can be
        // handed to the provisioner — otherwise the base is `file://build/…` and the
        // fetcher opens a path relative to whatever directory it happens to run in.
        let sink = |_: crate::event::Event| {};
        let step = Step::start(&sink, "repo");
        let Some(root) = crate::sandbox::packaging_root_for_tests(&step) else {
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let deb = make_deb(&root, tmp.path(), "ffmpeg-rk", "3e53143", &step);
        // Relative to the process's own directory, which is where the fetcher would
        // otherwise resolve the un-absolutized base from.
        let repo_dir = std::env::current_dir()
            .unwrap()
            .join(tmp.path().file_name().unwrap());
        let relative = pathdiff(&repo_dir);

        let repo =
            LocalDistsRepo::assemble(&relative, &[deb], "forky", "arm64", Some(EPOCH), &step)
                .unwrap();
        assert_eq!(repo.file_url(), format!("file://{}", repo_dir.display()));
        let _ = std::fs::remove_dir_all(&repo_dir);
    }

    /// `path` expressed relative to the current directory, which it must be under.
    fn pathdiff(path: &Path) -> PathBuf {
        path.strip_prefix(std::env::current_dir().unwrap())
            .expect("a path under the current directory")
            .to_path_buf()
    }

    #[test]
    fn unencoded_paths_reach_the_provisioners_fetcher() {
        // The work dir — and so the repo dir under it — is a user-chosen path, and a
        // space or a `#` in it is ordinary on a desktop. This asserts the whole join
        // through the *real* consumer: the URL is concatenated with a pool-relative
        // suffix and handed to the provisioner's fetcher, which strips `file://` and
        // opens the remainder as a path. Percent-encoding the base would turn a
        // working path into a literal `%20` lookup, so this is what stops that from
        // ever looking like a fix.
        use ferroday_cage::provision::debian::{Fetch, FetchRequest, HttpFetch};
        let sink = |_: crate::event::Event| {};
        let step = Step::start(&sink, "repo");
        let Some(root) = crate::sandbox::packaging_root_for_tests(&step) else {
            return;
        };
        let tmp = tempfile::tempdir().unwrap();
        let deb = make_deb(&root, tmp.path(), "ffmpeg-rk", "3e53143", &step);
        let repo_dir = tmp.path().join("My Projects/build #1/localdists");
        let repo =
            LocalDistsRepo::assemble(&repo_dir, &[deb], "forky", "arm64", Some(EPOCH), &step)
                .unwrap();
        assert!(repo.file_url().contains("My Projects/build #1"));
        assert!(!repo.file_url().contains('%'));

        let mut fetch = HttpFetch::new();
        for suffix in [
            "/dists/forky/Release",
            "/pool/main/f/ffmpeg-rk/ffmpeg-rk_3e53143_arm64.deb",
        ] {
            let url = format!("{}{suffix}", repo.file_url());
            let mut body = Vec::new();
            fetch
                .fetch(&FetchRequest::new(&url), &mut body)
                .unwrap_or_else(|e| panic!("fetching {url}: {e}"));
            assert!(!body.is_empty(), "empty body for {url}");
        }
    }
}
