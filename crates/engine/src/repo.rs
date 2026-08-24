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
}

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
    /// Any prior contents of `dir` are removed first, so the repo reflects exactly
    /// `debs`: `Pool::publish` is incremental by design, and this repo is a
    /// per-build view of one artifact ledger rather than an accumulating pool.
    /// A `.deb` whose `Architecture` is neither `arch` nor `all` is rejected
    /// rather than indexed where nothing can resolve it. `dir` should be absolute.
    pub fn assemble(
        dir: &Path,
        debs: &[PathBuf],
        suite: &str,
        arch: &str,
        step: &Step,
    ) -> Result<LocalDistsRepo, EngineError> {
        let _ = std::fs::remove_dir_all(dir);
        step.log(format!(
            "assembling dists/ local apt repo from {} .deb(s) at {} ({suite}/{arch})",
            debs.len(),
            dir.display()
        ));
        Pool::at(dir)
            .suite(suite)
            .component("main")
            .architecture(arch)
            .publish(debs)
            .map_err(|e| EngineError::Bootstrap {
                context: format!("publish the local .deb pool at {}", dir.display()),
                message: e.to_string(),
            })?;

        Ok(LocalDistsRepo {
            dir: dir.to_path_buf(),
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
    pub fn file_url(&self) -> String {
        format!("file://{}", self.dir.display())
    }

    /// The repo directory.
    pub fn dir(&self) -> &Path {
        &self.dir
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    /// True when a host tool is runnable, so a test needing one skips cleanly
    /// where it is absent.
    fn have(tool: &str) -> bool {
        Command::new(tool)
            .arg("--version")
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    /// Build a minimal `.deb` (`<pkg>_<version>_arm64.deb`) under `dir` via
    /// `dpkg-deb --build`, returning its path.
    fn make_deb(dir: &Path, pkg: &str, version: &str) -> PathBuf {
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
        let status = Command::new("dpkg-deb")
            .args(["--build", "--nocheck"])
            .arg(&tree)
            .arg(&out)
            .status()
            .unwrap();
        assert!(status.success(), "dpkg-deb --build failed");
        out
    }

    #[test]
    fn dists_repo_has_the_mirror_layout_the_provisioner_reads() {
        // Only the fixture needs a host tool: the pool itself is written by
        // ferroday-cage, so this runs on a host with no apt archive tooling.
        if !have("dpkg-deb") {
            eprintln!("skipping dists_repo_has_the_mirror_layout: dpkg-deb unavailable");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let a = make_deb(tmp.path(), "librockchip-mpp1", "1.5.0-1");
        let b = make_deb(tmp.path(), "ffmpeg-rk", "3e53143");
        let repo_dir = tmp.path().join("localdists");
        let sink = |_: crate::event::Event| {};
        let step = Step::start(&sink, "repo");

        let repo = LocalDistsRepo::assemble(&repo_dir, &[a, b], "forky", "arm64", &step).unwrap();

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
        if !have("dpkg-deb") {
            eprintln!(
                "skipping unencoded_paths_reach_the_provisioners_fetcher: dpkg-deb unavailable"
            );
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let deb = make_deb(tmp.path(), "ffmpeg-rk", "3e53143");
        let repo_dir = tmp.path().join("My Projects/build #1/localdists");
        let sink = |_: crate::event::Event| {};
        let step = Step::start(&sink, "repo");
        let repo = LocalDistsRepo::assemble(&repo_dir, &[deb], "forky", "arm64", &step).unwrap();
        assert!(repo.file_url().contains("My Projects/build #1"));

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
