//! The .NET half of a [`DotnetDeb`](boot2deb_core::model::AppBuild::DotnetDeb) app.
//! It keeps the SDK and NuGet content stores, and fills the offline folder feed a build
//! restores from. It also reads a restore's packages folder into a pinned
//! [`NugetManifest`].
//!
//! Every boot2deb build command runs with no network. A .NET build restores packages
//! from a feed, so the feed is a folder this module fills before the build starts. It
//! fills it from the app's pinned [`NugetManifest`]: each `.nupkg` is fetched from
//! nuget.org, verified against its sha512, and laid beside the others. The SDK is
//! fetched and verified the same way, since Debian does not package one.
//!
//! The manifest itself comes from one networked restore at `update` time
//! ([`read_packages_folder`] turns what that restore downloaded into the pins).
//!
//! Side effects: HTTP(S) fetches through [`crate::netfetch`], the durable content
//! stores under the config root's cache, and the host `tar` that unpacks the SDK.

use crate::error::EngineError;
use crate::event::Step;
use boot2deb_core::model::{dotnet_sdk_url, DotnetSdk};
use boot2deb_core::nuget::{NugetManifest, NugetPackage};
use sha2::{Digest, Sha512};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

/// Overall timeout for fetching the SDK tarball, which is a few hundred megabytes.
const SDK_TIMEOUT: Duration = Duration::from_secs(900);

/// Body-size cap for the SDK tarball. The 10.0 SDK is about 230 MiB; 1 GiB is far
/// above any real one, so a larger body is a misbehaving server, refused rather than
/// buffered.
const MAX_SDK_BYTES: u64 = 1024 * 1024 * 1024;

/// Overall timeout for fetching one NuGet package.
const NUPKG_TIMEOUT: Duration = Duration::from_secs(300);

/// Body-size cap for one NuGet package. The largest a server restore pulls is a
/// runtime pack of under 100 MiB.
const MAX_NUPKG_BYTES: u64 = 512 * 1024 * 1024;

/// The lowercase-hex sha512 of `bytes`: the form every .NET pin is written in.
pub fn sha512_hex(bytes: &[u8]) -> String {
    let digest = Sha512::digest(bytes);
    let mut out = String::with_capacity(128);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// A content-addressed store of files under one directory, each named
/// `<sha512>.<extension>` and written only after its bytes hash to that name. A hit is
/// therefore trusted as the pinned bytes. Two pins naming identical bytes share an
/// entry.
///
/// Durable: it lives under the config root's cache and outlives any one recipe's work
/// directory, so a second build of the same pins fetches nothing.
pub struct Sha512Store {
    dir: PathBuf,
}

impl Sha512Store {
    /// Open the store rooted at `dir`, creating it if needed, and sweep the `.partial`
    /// temps an interrupted write leaves behind.
    pub fn open(dir: &Path) -> Result<Sha512Store, EngineError> {
        std::fs::create_dir_all(dir).map_err(|s| EngineError::io(dir, s))?;
        crate::gc::sweep_stale_temps(dir);
        Ok(Sha512Store {
            dir: dir.to_path_buf(),
        })
    }

    /// Where the file with hash `sha512` and `extension` lives, present or not.
    pub fn path_for(&self, sha512: &str, extension: &str) -> PathBuf {
        self.dir.join(format!("{sha512}.{extension}"))
    }

    /// Store `bytes` after verifying they hash to `expected`, returning the stored
    /// path. A mismatch stores nothing and is [`EngineError::DotnetHashMismatch`]. The
    /// write is a temp beside the destination renamed into place, so an interrupted
    /// put never leaves a truncated file at a trusted name.
    fn put_bytes(
        &self,
        bytes: &[u8],
        expected: &str,
        extension: &str,
        what: &str,
        url: &str,
    ) -> Result<PathBuf, EngineError> {
        let actual = sha512_hex(bytes);
        if actual != expected {
            return Err(EngineError::DotnetHashMismatch {
                what: what.to_string(),
                url: url.to_string(),
                expected: expected.to_string(),
                actual,
            });
        }
        let dest = self.path_for(expected, extension);
        let tmp = self
            .dir
            .join(format!(".{expected}.{}.partial", std::process::id()));
        std::fs::write(&tmp, bytes).map_err(|s| EngineError::io(&tmp, s))?;
        std::fs::rename(&tmp, &dest).map_err(|s| {
            let _ = std::fs::remove_file(&tmp);
            EngineError::io(&dest, s)
        })?;
        Ok(dest)
    }

    /// The stored file for the pin (`url`, `sha512`), fetching it on a miss.
    fn fetch_pinned(&self, pin: &Pinned, step: &Step) -> Result<PathBuf, EngineError> {
        let path = self.path_for(pin.sha512, pin.extension);
        if path.is_file() {
            return Ok(path);
        }
        step.log(format!("fetching {} from {}", pin.what, pin.url));
        let bytes =
            crate::netfetch::fetch_bounded(pin.url, pin.max_bytes, pin.timeout).map_err(|e| {
                EngineError::DotnetFetch {
                    what: pin.what.to_string(),
                    url: pin.url.to_string(),
                    detail: e.0,
                }
            })?;
        self.put_bytes(&bytes, pin.sha512, pin.extension, pin.what, pin.url)
    }
}

/// One pinned download: where it comes from, what it must hash to, and the bounds the
/// fetch runs under.
struct Pinned<'a> {
    what: &'a str,
    url: &'a str,
    sha512: &'a str,
    extension: &'a str,
    max_bytes: u64,
    timeout: Duration,
}

/// The durable caches this module keeps under the config root's cache directory.
pub struct DotnetCache {
    /// Downloaded SDK tarballs and `.nupkg` files, by sha512.
    downloads: Sha512Store,
    /// Unpacked SDKs, one directory per tarball.
    sdks: PathBuf,
}

impl DotnetCache {
    /// Open the caches under `cache_root` (`<root>/cache`), creating them if needed.
    pub fn open(cache_root: &Path) -> Result<DotnetCache, EngineError> {
        let base = cache_root.join("dotnet");
        let sdks = base.join("sdk");
        std::fs::create_dir_all(&sdks).map_err(|s| EngineError::io(&sdks, s))?;
        Ok(DotnetCache {
            downloads: Sha512Store::open(&base.join("downloads"))?,
            sdks,
        })
    }

    /// The unpacked SDK for `sdk` on a build host of Debian architecture `host_arch`,
    /// fetching and unpacking it on first use. Returns the directory holding the
    /// `dotnet` executable.
    ///
    /// The unpacked directory is named for the tarball's digest. It is published by a
    /// rename only once unpacking finished, so a directory at that name is always a
    /// complete unpacking of the pinned bytes.
    pub fn ensure_sdk(
        &self,
        app: &str,
        sdk: &DotnetSdk,
        host_arch: &str,
        step: &Step,
    ) -> Result<PathBuf, EngineError> {
        let unsupported = || EngineError::DotnetHostUnsupported {
            app: app.to_string(),
            host: host_arch.to_string(),
        };
        let sha512 = sdk.sha512.get(host_arch).ok_or_else(unsupported)?;
        let url = dotnet_sdk_url(&sdk.version, host_arch).ok_or_else(unsupported)?;
        let dir = self
            .sdks
            .join(format!("{}-{host_arch}-{}", sdk.version, &sha512[..16]));
        if dir.join("dotnet").is_file() {
            return Ok(dir);
        }
        let what = format!(".NET SDK {}", sdk.version);
        let tarball = self.downloads.fetch_pinned(
            &Pinned {
                what: &what,
                url: &url,
                sha512,
                extension: "tar.gz",
                max_bytes: MAX_SDK_BYTES,
                timeout: SDK_TIMEOUT,
            },
            step,
        )?;
        let staging = self.sdks.join(format!(
            ".{}.{}.partial",
            dir.file_name().and_then(|n| n.to_str()).unwrap_or("sdk"),
            std::process::id()
        ));
        if staging.exists() {
            std::fs::remove_dir_all(&staging).map_err(|s| EngineError::io(&staging, s))?;
        }
        std::fs::create_dir_all(&staging).map_err(|s| EngineError::io(&staging, s))?;
        step.log(format!("unpacking {what}"));
        let mut tar = Command::new("tar");
        tar.arg("-xzf")
            .arg(&tarball)
            .arg("-C")
            .arg(&staging)
            .arg("--no-same-owner");
        crate::build::run(tar, "tar", &format!("unpack {what}"), step).inspect_err(|_| {
            let _ = std::fs::remove_dir_all(&staging);
        })?;
        std::fs::rename(&staging, &dir).map_err(|s| {
            let _ = std::fs::remove_dir_all(&staging);
            EngineError::io(&dir, s)
        })?;
        Ok(dir)
    }

    /// Lay every package of `manifest` into `feed` as `<id>.<version>.nupkg`, fetching
    /// and verifying each one the store lacks. `feed` is emptied first, so it holds
    /// exactly the pinned set and a restore from it can resolve nothing else.
    pub fn materialize_feed(
        &self,
        manifest: &NugetManifest,
        feed: &Path,
        step: &Step,
    ) -> Result<(), EngineError> {
        if feed.exists() {
            std::fs::remove_dir_all(feed).map_err(|s| EngineError::io(feed, s))?;
        }
        std::fs::create_dir_all(feed).map_err(|s| EngineError::io(feed, s))?;
        let mut fetched = 0usize;
        for package in &manifest.packages {
            let what = format!("NuGet package {} {}", package.id, package.version);
            let url = package.url();
            let had = self.downloads.path_for(&package.sha512, "nupkg").is_file();
            let stored = self.downloads.fetch_pinned(
                &Pinned {
                    what: &what,
                    url: &url,
                    sha512: &package.sha512,
                    extension: "nupkg",
                    max_bytes: MAX_NUPKG_BYTES,
                    timeout: NUPKG_TIMEOUT,
                },
                step,
            )?;
            if !had {
                fetched += 1;
            }
            let dest = feed.join(package.file_name());
            // A hard link where the cache and the feed share a filesystem, which they
            // do under one config root; a copy where they do not.
            if std::fs::hard_link(&stored, &dest).is_err() {
                std::fs::copy(&stored, &dest).map_err(|s| EngineError::io(&dest, s))?;
            }
        }
        step.log(format!(
            "NuGet feed: {} package(s), {fetched} fetched, the rest from the cache",
            manifest.packages.len()
        ));
        Ok(())
    }
}

/// Read the NuGet global-packages folder a restore wrote into a [`NugetManifest`] for
/// `runtime`. Each `<id>/<version>/` directory becomes one entry, pinned by the sha512
/// of the `.nupkg` in it.
///
/// The digest is computed from the file rather than taken from the `.nupkg.sha512`
/// NuGet writes beside it. Where that file exists the two must agree, since a
/// disagreement means the folder is not what the restore left.
pub fn read_packages_folder(
    dir: &Path,
    runtime: &str,
    origin: &str,
) -> Result<NugetManifest, EngineError> {
    let unreadable = |detail: String| EngineError::NugetPackagesUnreadable {
        dir: dir.display().to_string(),
        detail,
    };
    let mut packages = Vec::new();
    for id_entry in sorted_dirs(dir)? {
        let id = file_name(&id_entry);
        for version_entry in sorted_dirs(&id_entry)? {
            let version = file_name(&version_entry);
            let nupkg = version_entry.join(format!("{id}.{version}.nupkg"));
            let bytes = std::fs::read(&nupkg)
                .map_err(|e| unreadable(format!("{}: {e}", nupkg.display())))?;
            let sha512 = sha512_hex(&bytes);
            let recorded = version_entry.join(format!("{id}.{version}.nupkg.sha512"));
            if let Ok(text) = std::fs::read_to_string(&recorded) {
                let decoded = boot2deb_core::base64::decode(text.trim())
                    .ok_or_else(|| unreadable(format!("{} is not base64", recorded.display())))?;
                let recorded_hex: String = decoded.iter().map(|b| format!("{b:02x}")).collect();
                if recorded_hex != sha512 {
                    return Err(unreadable(format!(
                        "{} does not match the digest NuGet recorded beside it",
                        nupkg.display()
                    )));
                }
            }
            packages.push(NugetPackage {
                id: id.clone(),
                version,
                sha512,
            });
        }
    }
    NugetManifest::new(runtime, packages, origin).map_err(EngineError::from)
}

/// Write `manifest` to `path` in its committed form, atomically, and return the sha256
/// of what was written — the digest a lock's [`NugetPin`](boot2deb_core::lock::NugetPin)
/// records. The directory is created if it does not exist yet, as an image's output
/// directory does not before its first artifact.
///
/// A uniquely named temp beside `path` is renamed into place. An interrupted write
/// therefore never leaves a truncated sidecar that a later build would hash, find
/// wrong, and blame on the lock.
pub fn write_manifest(path: &Path, manifest: &NugetManifest) -> Result<String, EngineError> {
    let text = manifest.to_toml_string()?;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|s| EngineError::io(dir, s))?;
    }
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("nuget.lock");
    let tmp = path.with_file_name(format!(".{name}.{}.partial", std::process::id()));
    std::fs::write(&tmp, &text).map_err(|s| EngineError::io(&tmp, s))?;
    std::fs::rename(&tmp, path).map_err(|s| {
        let _ = std::fs::remove_file(&tmp);
        EngineError::io(path, s)
    })?;
    Ok(crate::blobs::sha256_hex(text.as_bytes()))
}

/// Read the sidecar at `path`, check it hashes to `expected_sha256` — the digest the
/// lock pins — and parse it. A sidecar that drifted from its lock is
/// [`EngineError::NugetManifestMismatch`], caught before anything is fetched.
pub fn read_pinned_manifest(
    app: &str,
    path: &Path,
    expected_sha256: &str,
) -> Result<NugetManifest, EngineError> {
    let bytes = std::fs::read(path).map_err(|s| EngineError::io(path, s))?;
    let actual = crate::blobs::sha256_hex(&bytes);
    if actual != expected_sha256 {
        return Err(EngineError::NugetManifestMismatch {
            app: app.to_string(),
            manifest: path.display().to_string(),
            expected: expected_sha256.to_string(),
            actual,
        });
    }
    let text = String::from_utf8(bytes).map_err(|e| EngineError::NugetPackagesUnreadable {
        dir: path.display().to_string(),
        detail: format!("not UTF-8: {e}"),
    })?;
    Ok(NugetManifest::from_toml_str(
        &text,
        &path.display().to_string(),
    )?)
}

/// The subdirectories of `dir`, sorted by name so the walk is host-independent.
fn sorted_dirs(dir: &Path) -> Result<Vec<PathBuf>, EngineError> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir(dir).map_err(|s| EngineError::io(dir, s))? {
        let entry = entry.map_err(|s| EngineError::io(dir, s))?;
        if entry
            .file_type()
            .map_err(|s| EngineError::io(dir, s))?
            .is_dir()
        {
            out.push(entry.path());
        }
    }
    out.sort();
    Ok(out)
}

/// A path's last component as a `String`.
fn file_name(path: &Path) -> String {
    path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default()
        .to_string()
}

/// The environment a `dotnet` command runs under inside a build root.
///
/// The SDK goes on `PATH`, and its home and package folders under `scratch`. Every
/// first-run, telemetry and update check is off. The sandbox's own environment
/// ([`SANDBOX_ENV`](crate::sandbox)) is otherwise unchanged, and these entries
/// override it where they collide.
///
/// `scratch/home` stands in for the home directory the SDK writes its first-run
/// markers into, and `scratch/packages` is the global packages folder a restore
/// extracts into. Both are the caller's to create and to discard.
pub fn dotnet_env(sdk: &Path, scratch: &Path) -> Vec<(String, String)> {
    let home = scratch.join("home");
    let pair = |k: &str, v: String| (k.to_string(), v);
    vec![
        pair(
            "PATH",
            format!("{}:/usr/sbin:/usr/bin:/sbin:/bin", sdk.display()),
        ),
        pair("DOTNET_ROOT", sdk.display().to_string()),
        pair("HOME", home.display().to_string()),
        pair("DOTNET_CLI_HOME", home.display().to_string()),
        pair(
            "NUGET_PACKAGES",
            scratch.join("packages").display().to_string(),
        ),
        pair("DOTNET_CLI_TELEMETRY_OPTOUT", "1".into()),
        pair("DOTNET_NOLOGO", "1".into()),
        pair("DOTNET_SKIP_FIRST_TIME_EXPERIENCE", "1".into()),
        pair("DOTNET_CLI_WORKLOAD_UPDATE_NOTIFY_DISABLE", "1".into()),
        pair("DOTNET_GENERATE_ASPNET_CERTIFICATE", "false".into()),
        pair("DOTNET_ADD_GLOBAL_TOOLS_TO_PATH", "false".into()),
        // One MSBuild process that exits with the command, rather than build nodes
        // that outlive it inside a cage being torn down.
        pair("MSBUILDDISABLENODEREUSE", "1".into()),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;

    /// A sink that discards every event.
    fn quiet(_: Event) {}

    #[test]
    fn sha512_hex_is_128_lowercase_hex() {
        let h = sha512_hex(b"abc");
        assert!(boot2deb_core::sources::is_sha512_hex(&h));
        assert!(h.starts_with("ddaf35a193617aba"));
    }

    /// A store writes only bytes that hash to the name they are stored under, so a
    /// hit can be trusted without re-hashing it.
    #[test]
    fn a_store_refuses_bytes_that_do_not_hash_to_the_pin() {
        let tmp = tempfile::tempdir().unwrap();
        let store = Sha512Store::open(tmp.path()).unwrap();
        let pinned = sha512_hex(b"pinned");
        let err = store
            .put_bytes(b"other", &pinned, "nupkg", "pkg", "https://x/p.nupkg")
            .unwrap_err();
        assert!(matches!(err, EngineError::DotnetHashMismatch { .. }));
        assert!(!store.path_for(&pinned, "nupkg").exists());
        let stored = store
            .put_bytes(b"pinned", &pinned, "nupkg", "pkg", "https://x/p.nupkg")
            .unwrap();
        assert_eq!(std::fs::read(stored).unwrap(), b"pinned");
    }

    /// A restore's packages folder becomes a canonical manifest, and a `.nupkg` whose
    /// bytes disagree with the digest NuGet recorded beside it is refused.
    #[test]
    fn a_packages_folder_reads_into_pins_and_a_tampered_one_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let write = |id: &str, version: &str, bytes: &[u8], recorded: Option<&[u8]>| {
            let d = tmp.path().join(id).join(version);
            std::fs::create_dir_all(&d).unwrap();
            std::fs::write(d.join(format!("{id}.{version}.nupkg")), bytes).unwrap();
            if let Some(r) = recorded {
                let digest = Sha512::digest(r);
                std::fs::write(
                    d.join(format!("{id}.{version}.nupkg.sha512")),
                    boot2deb_core::base64::encode(&digest),
                )
                .unwrap();
            }
        };
        write("zlib", "1.0.0", b"z", Some(b"z"));
        write("alpha", "2.0.0", b"a", None);
        let m = read_packages_folder(tmp.path(), "linux-arm64", "t").unwrap();
        assert_eq!(m.runtime, "linux-arm64");
        let ids: Vec<&str> = m.packages.iter().map(|p| p.id.as_str()).collect();
        assert_eq!(ids, ["alpha", "zlib"]);
        assert_eq!(m.packages[1].sha512, sha512_hex(b"z"));

        write("zlib", "1.0.0", b"z", Some(b"not z"));
        assert!(matches!(
            read_packages_folder(tmp.path(), "linux-arm64", "t"),
            Err(EngineError::NugetPackagesUnreadable { .. })
        ));
    }

    /// A host with no pinned tarball is told which table entry to add rather than
    /// failing somewhere inside a download.
    #[test]
    fn an_sdk_with_no_pin_for_this_host_is_refused_up_front() {
        let tmp = tempfile::tempdir().unwrap();
        let cache = DotnetCache::open(tmp.path()).unwrap();
        let sdk = DotnetSdk {
            version: "10.0.401".into(),
            sha512: [("amd64".to_string(), "a".repeat(128))]
                .into_iter()
                .collect(),
        };
        let step = Step::start(&quiet, "test");
        assert!(matches!(
            cache.ensure_sdk("jellyfin", &sdk, "arm64", &step),
            Err(EngineError::DotnetHostUnsupported { .. })
        ));
    }

    /// What `update` writes, a build reads back only at the digest `update` returned;
    /// an edited sidecar is refused before any package is fetched.
    #[test]
    fn a_written_sidecar_reads_back_only_at_its_pinned_digest() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("x.jellyfin.nuget.lock");
        let manifest = NugetManifest::new(
            "linux-arm64",
            vec![NugetPackage {
                id: "a".into(),
                version: "1.0.0".into(),
                sha512: "b".repeat(128),
            }],
            "t",
        )
        .unwrap();
        let digest = write_manifest(&path, &manifest).unwrap();
        assert_eq!(
            read_pinned_manifest("jellyfin", &path, &digest).unwrap(),
            manifest
        );
        std::fs::write(&path, "runtime = \"linux-arm64\"\n").unwrap();
        assert!(matches!(
            read_pinned_manifest("jellyfin", &path, &digest),
            Err(EngineError::NugetManifestMismatch { .. })
        ));
    }

    #[test]
    fn the_dotnet_environment_puts_the_sdk_first_and_turns_first_run_off() {
        let env = dotnet_env(Path::new("/c/sdk"), Path::new("/w/scratch"));
        let get = |k: &str| {
            env.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
                .unwrap()
        };
        assert!(get("PATH").starts_with("/c/sdk:"));
        assert_eq!(get("NUGET_PACKAGES"), "/w/scratch/packages");
        assert_eq!(get("DOTNET_CLI_HOME"), "/w/scratch/home");
        assert_eq!(get("DOTNET_CLI_TELEMETRY_OPTOUT"), "1");
    }
}
