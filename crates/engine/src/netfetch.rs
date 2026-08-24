//! Bounded HTTP(S) fetch — the size-capped, redirect-bounded GET shared by
//! the `extra_debs` and `patch import` fetchers.
//!
//! Both callers pull bytes from an operator- or lock-supplied URL, so an unbounded
//! `read_to_end` is a memory-exhaustion vector and an unpinned redirect chain is a
//! transport-trust gap. This module centralizes the network policy so both fetchers
//! get the same guarantees:
//!
//! - **Size cap.** The body is read through a `take(max + 1)` limiter and refused if
//!   it would exceed `max_bytes`, so a hostile or misconfigured server cannot force
//!   an arbitrarily large allocation.
//! - **Scheme allowlist.** Only `http://` and `https://` are accepted; a redirect to
//!   any other scheme is refused.
//! - **No TLS downgrade.** Redirects are followed manually (auto-redirect off), and a
//!   hop from `https` to `http` is refused — a MITM cannot strip TLS by redirecting.
//! - **Bounded redirects.** At most `MAX_REDIRECTS` hops before giving up.
//!
//! Integrity of a fetched `extra_deb` still comes from its pinned sha256; this
//! is the transport-hardening layer beneath that pin.

use std::io::Read;
use std::time::Duration;

/// Maximum redirect hops followed before the fetch is abandoned.
const MAX_REDIRECTS: u32 = 5;

/// A bounded-fetch failure. Callers map the message into their own typed error
/// ([`ExtraDebFetch`](crate::error::EngineError::ExtraDebFetch) /
/// [`PatchImportFetch`](crate::error::EngineError::PatchImportFetch)).
#[derive(Debug)]
pub struct FetchError(pub String);

/// GET `url` over HTTP(S), following redirects manually under the module's policy,
/// and return the body — refusing anything larger than `max_bytes`, a non-HTTP(S)
/// scheme, a TLS downgrade, or more than `MAX_REDIRECTS` hops.
pub fn fetch_bounded(url: &str, max_bytes: u64, timeout: Duration) -> Result<Vec<u8>, FetchError> {
    require_http(url)?;
    // Auto-redirect off: we follow manually so each hop passes the scheme/downgrade
    // policy before it is requested.
    let agent = ureq::AgentBuilder::new()
        .timeout(timeout)
        .redirects(0)
        .build();
    let mut current = url.to_string();
    for _ in 0..=MAX_REDIRECTS {
        let resp = agent
            .get(&current)
            .call()
            .map_err(|e| FetchError(e.to_string()))?;
        // ureq returns Err for >=400, so an Ok response is 2xx or (redirects off) 3xx.
        if (300..400).contains(&resp.status()) {
            let loc = resp.header("Location").ok_or_else(|| {
                FetchError(format!(
                    "redirect {} without a Location header",
                    resp.status()
                ))
            })?;
            let next = resolve_redirect(&current, loc)?;
            require_http(&next)?;
            reject_downgrade(&current, &next)?;
            current = next;
            continue;
        }
        // 2xx: read the body under the size cap.
        let mut bytes = Vec::new();
        resp.into_reader()
            .take(max_bytes.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|e| FetchError(format!("reading response body: {e}")))?;
        if bytes.len() as u64 > max_bytes {
            return Err(FetchError(format!(
                "response body exceeds the {max_bytes}-byte cap"
            )));
        }
        return Ok(bytes);
    }
    Err(FetchError(format!(
        "too many redirects (more than {MAX_REDIRECTS})"
    )))
}

/// Refuse a URL whose scheme is not `http`/`https` — nothing else is a valid fetch
/// source, and this keeps a `file://`/`ftp://` redirect from reaching ureq.
fn require_http(url: &str) -> Result<(), FetchError> {
    if url.starts_with("https://") || url.starts_with("http://") {
        Ok(())
    } else {
        Err(FetchError(format!("refusing non-HTTP(S) URL: {url}")))
    }
}

/// Refuse a redirect that drops TLS (`https` → `http`); every other scheme pair is
/// already constrained to http(s) by [`require_http`].
fn reject_downgrade(from: &str, to: &str) -> Result<(), FetchError> {
    if from.starts_with("https://") && to.starts_with("http://") {
        Err(FetchError(format!(
            "refusing HTTPS→HTTP downgrade redirect: {from} -> {to}"
        )))
    } else {
        Ok(())
    }
}

/// Resolve a redirect `Location` against the current URL: an absolute `http(s)` URL
/// is used as-is, a network-path reference (`//host/path`) takes the base's scheme,
/// and a root-relative (`/path`) or path-relative (`sub/x`) target is joined onto
/// the current URL's authority/path. The resulting path is dot-segment-normalized by
/// [`normalize_url_path`] whichever branch produced it. Enough URL handling for the
/// deb/patch CDNs in play, without pulling in a full URL parser.
fn resolve_redirect(base: &str, loc: &str) -> Result<String, FetchError> {
    let scheme_end = base
        .find("://")
        .ok_or_else(|| FetchError(format!("malformed base URL: {base}")))?
        + 3;
    let absolute = if loc.starts_with("http://") || loc.starts_with("https://") {
        loc.to_string()
    } else if let Some(rest) = loc.strip_prefix("//") {
        // Network-path reference (RFC 3986 section 4.2): same scheme, new authority.
        format!("{}{rest}", &base[..scheme_end])
    } else {
        // A reference carrying a path never inherits the base's query or fragment
        // (RFC 3986 section 5.2.2), and a `/` inside either is not a path separator —
        // so both are cut away before anything is measured against the base.
        let hierarchical = match base[scheme_end..].find(['?', '#']) {
            Some(i) => &base[..scheme_end + i],
            None => base,
        };
        // Split scheme://authority from the path portion. No `/` after the authority
        // means the base path is empty, which merges as if it were `/` — both branches
        // below depend on knowing that, so it is the one thing measured up front.
        let path_start = hierarchical[scheme_end..].find('/').map(|i| scheme_end + i);
        let authority_end = path_start.unwrap_or(hierarchical.len());
        match (loc.starts_with('/'), path_start) {
            // Root-relative: the authority is kept and the whole path replaced.
            (true, _) => format!("{}{loc}", &hierarchical[..authority_end]),
            // Path-relative onto a base that has a path: `loc` replaces the base's
            // last segment (RFC 3986 section 5.2.3). Searching from `path_start`
            // always lands on at least the `/` that begins the path, so the cut can
            // never reach back into the authority.
            (false, Some(path_start)) => {
                let last_slash = path_start + hierarchical[path_start..].rfind('/').unwrap_or(0);
                format!("{}{loc}", &hierarchical[..last_slash + 1])
            }
            // Path-relative onto an empty base path: merge at the root, so
            // `https://h` + `c` is `https://h/c` and not the different host `hc`.
            (false, None) => format!("{}/{loc}", &hierarchical[..authority_end]),
        }
    };
    normalize_url_path(&absolute)
}

/// Apply RFC 3986 remove-dot-segments to `url`'s path, leaving the scheme,
/// authority, and query/fragment untouched: a redirect target cannot
/// smuggle `.`/`..` segments into the request path this client then sends, and
/// `..` can never climb above the path root.
fn normalize_url_path(url: &str) -> Result<String, FetchError> {
    let scheme_end = url
        .find("://")
        .ok_or_else(|| FetchError(format!("malformed URL: {url}")))?
        + 3;
    let Some(path_start) = url[scheme_end..].find('/').map(|i| scheme_end + i) else {
        return Ok(url.to_string()); // authority only, no path to normalize
    };
    let (path_end, tail) = match url[path_start..].find(['?', '#']) {
        Some(i) => (path_start + i, &url[path_start + i..]),
        None => (url.len(), ""),
    };
    let path = &url[path_start..path_end];
    Ok(format!(
        "{}{}{tail}",
        &url[..path_start],
        normalize_dot_segments(path)
    ))
}

/// Remove `.`/`..` segments from an absolute URL path (RFC 3986 section 5.2.4),
/// keeping empty segments (`a//b`) verbatim — they name a resource, not a
/// traversal. `..` at the root is dropped rather than climbing.
fn normalize_dot_segments(path: &str) -> String {
    // The leading anchor keeps the result rooted; `..` can never pop it.
    let mut out: Vec<&str> = vec![""];
    for seg in path.split('/').skip(1) {
        match seg {
            "." => {}
            ".." => {
                if out.len() > 1 {
                    out.pop();
                }
            }
            s => out.push(s),
        }
    }
    // A path ending in a dot segment refers to a directory: keep the slash.
    if matches!(path.rsplit('/').next(), Some(".") | Some("..")) {
        out.push("");
    }
    out.join("/")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serve one HTTP response (status + optional Location + body) on an ephemeral
    /// localhost port. Hermetic: no external network.
    fn serve_once(
        status_line: &'static str,
        extra_headers: String,
        body: Vec<u8>,
    ) -> (String, std::thread::JoinHandle<()>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let url = format!("http://{addr}/thing");
        let handle = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                use std::io::{Read, Write};
                let mut buf = [0u8; 2048];
                let _ = stream.read(&mut buf);
                let header = format!(
                    "HTTP/1.1 {status_line}\r\nContent-Length: {}\r\n{extra_headers}\r\n",
                    body.len()
                );
                let _ = stream.write_all(header.as_bytes());
                let _ = stream.write_all(&body);
                let _ = stream.flush();
            }
        });
        (url, handle)
    }

    #[test]
    fn fetches_a_small_body() {
        let body = b"hello-bytes".to_vec();
        let (url, handle) = serve_once("200 OK", String::new(), body.clone());
        let got = fetch_bounded(&url, 1024, Duration::from_secs(5)).unwrap();
        handle.join().unwrap();
        assert_eq!(got, body);
    }

    #[test]
    fn refuses_a_body_over_the_cap() {
        let body = vec![b'x'; 4096];
        let (url, handle) = serve_once("200 OK", String::new(), body);
        // Cap below the body size → refused rather than allocated.
        let err = fetch_bounded(&url, 1024, Duration::from_secs(5)).unwrap_err();
        handle.join().unwrap();
        assert!(err.0.contains("exceeds the 1024-byte cap"), "{}", err.0);
    }

    #[test]
    fn refuses_non_http_scheme() {
        let err = fetch_bounded("file:///etc/passwd", 1024, Duration::from_secs(5)).unwrap_err();
        assert!(err.0.contains("non-HTTP(S)"), "{}", err.0);
    }

    #[test]
    fn reject_downgrade_blocks_https_to_http() {
        assert!(reject_downgrade("https://a/x", "http://a/y").is_err());
        assert!(reject_downgrade("http://a/x", "http://a/y").is_ok());
        assert!(reject_downgrade("https://a/x", "https://a/y").is_ok());
    }

    #[test]
    fn resolve_redirect_handles_absolute_root_and_relative() {
        assert_eq!(
            resolve_redirect("https://h/a/b", "https://o/c").unwrap(),
            "https://o/c"
        );
        assert_eq!(
            resolve_redirect("https://h/a/b", "/c/d").unwrap(),
            "https://h/c/d"
        );
        assert_eq!(
            resolve_redirect("https://h/a/b", "c").unwrap(),
            "https://h/a/c"
        );
        // No path on the base → authority preserved for a root-relative target...
        assert_eq!(resolve_redirect("https://h", "/c").unwrap(), "https://h/c");
        // ...and an empty base path merges at the root for a path-relative one, so
        // the segment joins the path instead of extending the host name.
        assert_eq!(resolve_redirect("https://h", "c").unwrap(), "https://h/c");
        assert_eq!(
            resolve_redirect("https://h/", "c").unwrap(),
            "https://h/c",
            "a bare trailing slash is the same base path by another spelling"
        );
    }

    /// The base's query and fragment are not part of its path: a reference carrying a
    /// path does not inherit them (RFC 3986 section 5.2.2), and a `/` inside one is
    /// not a segment separator — so neither may be measured as though it were path.
    #[test]
    fn resolve_redirect_ignores_the_bases_query_and_fragment() {
        assert_eq!(
            resolve_redirect("https://h/a/b?x=1", "c").unwrap(),
            "https://h/a/c"
        );
        assert_eq!(
            resolve_redirect("https://h/a/b?x=/y/z", "c").unwrap(),
            "https://h/a/c",
            "a slash inside the query is not the last path segment's separator"
        );
        assert_eq!(
            resolve_redirect("https://h?x=1", "c").unwrap(),
            "https://h/c",
            "a query on an empty base path leaves the merge base at the root"
        );
        assert_eq!(
            resolve_redirect("https://h/a/b#frag", "/c").unwrap(),
            "https://h/c"
        );
    }

    #[test]
    fn resolve_redirect_normalizes_dot_segments_and_network_paths() {
        // `..` in a relative Location resolves in place and cannot climb above
        // the path root...
        assert_eq!(
            resolve_redirect("https://h/a/b/c", "../x").unwrap(),
            "https://h/a/x"
        );
        assert_eq!(
            resolve_redirect("https://h/a/b", "../../../../x").unwrap(),
            "https://h/x"
        );
        // ...including inside an absolute or root-relative target...
        assert_eq!(
            resolve_redirect("https://h/a", "https://o/a/../secret").unwrap(),
            "https://o/secret"
        );
        assert_eq!(
            resolve_redirect("https://h/a/b", "/c/./d/../e").unwrap(),
            "https://h/c/e"
        );
        // ...while the query survives untouched and empty segments stay verbatim.
        assert_eq!(
            resolve_redirect("https://h/a/b", "/c/../d?x=../y").unwrap(),
            "https://h/d?x=../y"
        );
        assert_eq!(
            resolve_redirect("https://h/a", "/c//d").unwrap(),
            "https://h/c//d"
        );
        // A network-path reference keeps the base scheme (RFC 3986 section 4.2)
        // rather than being misread as a root-relative path on the old host.
        assert_eq!(
            resolve_redirect("https://h/a/b", "//mirror.example/pool/x.deb").unwrap(),
            "https://mirror.example/pool/x.deb"
        );
    }

    #[test]
    fn follows_a_redirect_to_the_final_body() {
        // First server 302s to the second, which serves the body.
        let body = b"final".to_vec();
        let (target, h2) = serve_once("200 OK", String::new(), body.clone());
        let loc = format!("Location: {target}\r\n");
        let (start, h1) = serve_once("302 Found", loc, Vec::new());
        let got = fetch_bounded(&start, 1024, Duration::from_secs(5)).unwrap();
        h1.join().unwrap();
        h2.join().unwrap();
        assert_eq!(got, body);
    }
}
