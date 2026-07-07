//! Materialize pre-built `extra_debs` — obtain each pinned deb into the
//! content store, verify its sha256, and return the stored paths for the local apt
//! repo.
//!
//! Shared by `update` (fills the store, verifying every pin up front so a dead URL
//! or a wrong hash fails before the lock is written) and `build` (materializes from
//! the store, re-fetching only a miss). The build reads only the lock's pins, so a
//! locator that 404s or a hash that mismatches is a hard error, never a silently
//! dropped package. Fetched debs reach the image only through the local apt repo the
//! engine assembles ([`crate::repo`]) — their integrity comes from the pin, their
//! trust boundary from that repo, never a `dpkg -i`.

use crate::debstore::DebStore;
use crate::error::EngineError;
use crate::event::Step;
use boot2deb_core::model::{ExtraDeb, ExtraDebLocator};
use boot2deb_core::ConfigRoot;
use std::path::PathBuf;
use std::time::Duration;

/// Overall timeout for one `extra_debs` HTTP fetch.
const FETCH_TIMEOUT: Duration = Duration::from_secs(300);

/// Body-size cap for one fetched `extra_deb` (TRUST-4). A pre-built `.deb` is a
/// package, not a disk image; 512 MiB is far above any real one, so a body over the
/// cap is a hostile/misconfigured server, refused rather than buffered into memory.
const MAX_DEB_BYTES: u64 = 512 * 1024 * 1024;

/// Materialize every pinned deb in `pins` into `store`, returning their stored
/// paths in `pins` order.
///
/// A store hit (`<sha256>.deb` present) is used directly — the store is
/// content-addressed, so its bytes are already the pinned ones (and are re-verified
/// at install time against the solved manifest). On a miss the deb is obtained
/// from its locator — a `url` fetched over HTTP(S), a `path` read along the config
/// search path (`root`, so an overlay may ship it) — and [`DebStore::put_bytes`]
/// verifies it hashes to the pin before storing. A fetch/read failure is
/// [`EngineError::ExtraDebFetch`]; a hash mismatch is
/// [`EngineError::ExtraDebHashMismatch`].
pub fn materialize(
    root: &ConfigRoot,
    pins: &[ExtraDeb],
    store: &DebStore,
    step: &Step,
) -> Result<Vec<PathBuf>, EngineError> {
    let mut paths = Vec::with_capacity(pins.len());
    for pin in pins {
        // Resolve rejects a malformed pin, but a lock is hand-editable, so re-check
        // the locator/hash shape before trusting them.
        pin.validate()?;
        let locator = pin.locator_label();
        if store.has(&pin.sha256) {
            step.log(format!("extra_deb {locator} — cached ({})", short(&pin.sha256)));
            paths.push(store.path_for(&pin.sha256));
            continue;
        }
        let bytes = match pin.locator()? {
            ExtraDebLocator::Url(url) => {
                step.log(format!("extra_deb {locator} — fetching"));
                fetch_url(url)?
            }
            ExtraDebLocator::Path(rel) => {
                step.log(format!("extra_deb {locator} — reading"));
                read_path(root, rel)?
            }
        };
        let path = store.put_bytes(&bytes, &pin.sha256, &locator)?;
        step.log(format!("extra_deb {locator} — stored ({})", short(&pin.sha256)));
        paths.push(path);
    }
    Ok(paths)
}

/// Read a `path`-locator deb, resolved along the config search path (an overlay may
/// ship it). A missing/unreadable file is [`EngineError::ExtraDebFetch`].
///
/// The `rel` string was already checked for absolute/`..` escapes by
/// [`ExtraDeb::validate`](boot2deb_core::model::ExtraDeb::validate) (re-run in
/// [`materialize`]). This adds the symlink half of containment: the resolved file is
/// canonicalized and must still lie within a config root, so a symlink planted in
/// the tree cannot redirect the read to an arbitrary host location.
fn read_path(root: &ConfigRoot, rel: &str) -> Result<Vec<u8>, EngineError> {
    let path = root.find_asset(rel).unwrap_or_else(|| root.path().join(rel));
    if let Ok(canon) = path.canonicalize() {
        let contained = root.search_paths().iter().any(|base| {
            base.canonicalize().map(|b| canon.starts_with(b)).unwrap_or(false)
        });
        if !contained {
            return Err(EngineError::ExtraDebFetch {
                locator: rel.to_string(),
                detail: format!("{} resolves outside the config root", path.display()),
            });
        }
    }
    std::fs::read(&path).map_err(|source| EngineError::ExtraDebFetch {
        locator: rel.to_string(),
        detail: format!("{}: {source}", path.display()),
    })
}

/// HTTP(S) GET the full body of `url` under the shared bounded-fetch policy (size
/// cap, no TLS downgrade, bounded redirects, TRUST-4). A cap overrun, disallowed
/// redirect, non-2xx status, or transport failure is [`EngineError::ExtraDebFetch`].
fn fetch_url(url: &str) -> Result<Vec<u8>, EngineError> {
    crate::netfetch::fetch_bounded(url, MAX_DEB_BYTES, FETCH_TIMEOUT).map_err(|e| {
        EngineError::ExtraDebFetch {
            locator: url.to_string(),
            detail: e.0,
        }
    })
}

/// First 12 chars of a hash, for a compact log line.
fn short(hash: &str) -> &str {
    &hash[..hash.len().min(12)]
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Event;

    /// Serve one HTTP request with `body` on an ephemeral localhost port, returning
    /// the URL and the server thread handle. Hermetic: no external network.
    fn serve_once(body: Vec<u8>) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/vendor.deb");
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                use std::io::{Read, Write};
                // Drain the request line + headers (up to the buffer) before replying.
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let header = format!(
                    "HTTP/1.1 200 OK\r\nContent-Length: {}\r\n\
                     Content-Type: application/octet-stream\r\n\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        });
        (url, handle)
    }

    fn silent_step() -> (impl Fn(Event), &'static str) {
        (|_: Event| {}, "extra-debs")
    }

    #[test]
    fn fetch_url_downloads_the_body() {
        let body = b"pretend-deb-bytes".to_vec();
        let (url, handle) = serve_once(body.clone());
        let got = fetch_url(&url).unwrap();
        handle.join().unwrap();
        assert_eq!(got, body);
    }

    #[test]
    fn materialize_from_url_stores_verifies_and_caches() {
        let body = b"an-arm64-vendor-deb".to_vec();
        let sha = crate::blobs::sha256_hex(&body);
        let (url, handle) = serve_once(body.clone());

        let tmp = tempfile::tempdir().unwrap();
        let root = ConfigRoot::new(tmp.path().join("root"));
        std::fs::create_dir_all(root.path()).unwrap();
        let store = DebStore::open(&tmp.path().join("store")).unwrap();
        let (sink, name) = silent_step();
        let step = Step::start(&sink, name);

        let pins = vec![ExtraDeb {
            url: Some(url),
            path: None,
            sha256: sha.clone(),
        }];
        let out = materialize(&root, &pins, &store, &step).unwrap();
        handle.join().unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(std::fs::read(&out[0]).unwrap(), body);
        assert!(store.has(&sha));

        // Second call is a store hit — no server is listening now, so a re-fetch
        // would fail; it must not attempt one.
        let out2 = materialize(&root, &pins, &store, &step).unwrap();
        assert_eq!(out2, out);
    }

    #[test]
    fn materialize_from_path_resolves_and_stores() {
        let tmp = tempfile::tempdir().unwrap();
        let root_dir = tmp.path().join("root");
        std::fs::create_dir_all(root_dir.join("vendor")).unwrap();
        let bytes = b"a-file-on-disk-deb";
        std::fs::write(root_dir.join("vendor/a.deb"), bytes).unwrap();
        let sha = crate::blobs::sha256_hex(bytes);
        let root = ConfigRoot::new(root_dir);
        let store = DebStore::open(&tmp.path().join("store")).unwrap();
        let (sink, name) = silent_step();
        let step = Step::start(&sink, name);

        let pins = vec![ExtraDeb {
            url: None,
            path: Some("vendor/a.deb".into()),
            sha256: sha.clone(),
        }];
        let out = materialize(&root, &pins, &store, &step).unwrap();
        assert_eq!(std::fs::read(&out[0]).unwrap(), bytes);
        assert!(store.has(&sha));
    }

    #[test]
    fn materialize_rejects_wrong_pin_and_missing_file() {
        let tmp = tempfile::tempdir().unwrap();
        let root_dir = tmp.path().join("root");
        std::fs::create_dir_all(root_dir.join("vendor")).unwrap();
        std::fs::write(root_dir.join("vendor/a.deb"), b"actual-bytes").unwrap();
        let root = ConfigRoot::new(root_dir);
        let store = DebStore::open(&tmp.path().join("store")).unwrap();
        let (sink, name) = silent_step();
        let step = Step::start(&sink, name);

        // File exists but hashes to something else → hash mismatch.
        let wrong = crate::blobs::sha256_hex(b"different-bytes");
        let bad_pin = vec![ExtraDeb {
            url: None,
            path: Some("vendor/a.deb".into()),
            sha256: wrong,
        }];
        assert!(matches!(
            materialize(&root, &bad_pin, &store, &step).unwrap_err(),
            EngineError::ExtraDebHashMismatch { .. }
        ));

        // A path that does not exist → fetch/read error.
        let missing = vec![ExtraDeb {
            url: None,
            path: Some("vendor/nope.deb".into()),
            sha256: crate::blobs::sha256_hex(b"x"),
        }];
        assert!(matches!(
            materialize(&root, &missing, &store, &step).unwrap_err(),
            EngineError::ExtraDebFetch { .. }
        ));
    }

    #[test]
    fn materialize_rejects_out_of_root_path() {
        // A traversal/absolute path is refused by validation before any read, so a
        // crafted lock cannot pull a host file into the deb store.
        let tmp = tempfile::tempdir().unwrap();
        let root_dir = tmp.path().join("root");
        std::fs::create_dir_all(&root_dir).unwrap();
        // A real file outside the root the traversal would target, to prove the
        // refusal is by policy, not because the file is absent.
        std::fs::write(tmp.path().join("secret.deb"), b"host-secret").unwrap();
        let root = ConfigRoot::new(root_dir);
        let store = DebStore::open(&tmp.path().join("store")).unwrap();
        let (sink, name) = silent_step();
        let step = Step::start(&sink, name);

        let escaping = vec![ExtraDeb {
            url: None,
            path: Some("../secret.deb".into()),
            sha256: crate::blobs::sha256_hex(b"host-secret"),
        }];
        assert!(matches!(
            materialize(&root, &escaping, &store, &step).unwrap_err(),
            EngineError::Config(boot2deb_core::ConfigError::ExtraDebUnsafePath { .. })
        ));
        assert!(!store.has(&crate::blobs::sha256_hex(b"host-secret")));
    }
}
