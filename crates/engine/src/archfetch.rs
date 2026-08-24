//! The transport the archive provisioner fetches through: `https://` over rustls, and
//! everything else delegated to the sandbox library's own client.
//!
//! The bundled client speaks plain `http://` and `file://`, which covers the Debian
//! mirror (integrity comes from the `Release` signature, not the transport) and the
//! build's local `file://` pool. It does not speak TLS — deliberately, since carrying a
//! TLS stack is a decision a consumer should make rather than inherit.
//!
//! boot2deb has to make it, because a feature can contribute an apt repository at any
//! URL its vendor publishes and the ones that matter are `https://` (Jellyfin's is).
//! Such a repository is verified against its own keyring exactly as the mirror is, so
//! TLS adds nothing to the *integrity* of what is installed — but without it the
//! resource cannot be fetched at all, and the recipe fails at the rootfs solve.
//!
//! The TLS half is [`ureq`] with rustls and bundled roots, which is already in the tree
//! for the `extra_debs` fetch: no system OpenSSL, no certificate store to depend on, no
//! async runtime. Everything that is not `https://` is handed to [`HttpFetch`]
//! unchanged, so the mirror and the local pool keep the library's own behaviour rather
//! than a second implementation of it.

use crate::netfetch::MAX_REDIRECTS;
use ferroday_cage::provision::debian::{Fetch, FetchError, FetchRequest, HttpFetch};
use std::io::Write;

/// The provisioner transport: rustls for `https://`, the library's client for the rest.
///
/// Constructed per bootstrap and moved into the provisioner, which owns it for the run.
pub struct ArchiveFetch {
    /// The library's own client, for `http://` and `file://`.
    plain: HttpFetch,
    /// The rustls agent, for `https://`. Held across requests so connections and the
    /// root store are reused rather than rebuilt per index.
    tls: ureq::Agent,
}

impl ArchiveFetch {
    /// A transport ready to serve any scheme the provisioner will ask for.
    pub fn new() -> ArchiveFetch {
        ArchiveFetch {
            plain: HttpFetch::new(),
            // Redirects are followed by the agent rather than by hand, and bounded for
            // the reason `netfetch` bounds them: a mirror that redirects in a loop must
            // fail rather than hang a build.
            tls: ureq::AgentBuilder::new().redirects(MAX_REDIRECTS).build(),
        }
    }
}

impl Default for ArchiveFetch {
    fn default() -> Self {
        ArchiveFetch::new()
    }
}

impl Fetch for ArchiveFetch {
    fn fetch(
        &mut self,
        request: &FetchRequest<'_>,
        sink: &mut dyn Write,
    ) -> Result<(), FetchError> {
        let url = request.url();
        if !url.starts_with("https://") {
            return self.plain.fetch(request, sink);
        }
        match self.tls.get(url).call() {
            Ok(response) => {
                let mut body = response.into_reader();
                std::io::copy(&mut body, sink)
                    .map_err(|err| FetchError::io(url, "reading the body", err))?;
                Ok(())
            }
            // A missing resource **must** be reported as `NotFound` rather than as a
            // status: the provisioner treats it as a recoverable absence and falls back
            // to another index compression or a non-by-hash path, and a plain status
            // would turn a normal probe into a hard failure.
            Err(ureq::Error::Status(404, _)) => Err(FetchError::not_found(url)),
            Err(ureq::Error::Status(code, _)) => Err(FetchError::status(url, code)),
            Err(err @ ureq::Error::Transport(_)) => Err(FetchError::io(
                url,
                "fetching over https",
                std::io::Error::other(err.to_string()),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A `file://` URL is the build's own local `.deb` pool, and it must keep going
    /// through the library's client rather than through the TLS agent — which cannot
    /// open a path at all. The delegation is the whole of this type's behaviour for
    /// every scheme but one, so this is the assertion that it is not accidentally
    /// swallowing them.
    #[test]
    fn a_non_https_url_is_served_by_the_librarys_own_client() {
        let tmp = tempfile::tempdir().unwrap();
        let path = tmp.path().join("Release");
        std::fs::write(&path, b"Suite: forky\n").unwrap();
        let url = format!("file://{}", path.display());

        let mut fetch = ArchiveFetch::new();
        let mut body = Vec::new();
        fetch.fetch(&FetchRequest::new(&url), &mut body).unwrap();
        assert_eq!(body, b"Suite: forky\n");
    }

    /// And a missing one is reported as an absence, not as a transport failure — the
    /// distinction the provisioner's index fallback turns on.
    #[test]
    fn a_missing_local_resource_is_an_absence() {
        let tmp = tempfile::tempdir().unwrap();
        let url = format!("file://{}/absent", tmp.path().display());
        let mut fetch = ArchiveFetch::new();
        let mut body = Vec::new();
        assert!(matches!(
            fetch.fetch(&FetchRequest::new(&url), &mut body),
            Err(FetchError::NotFound { .. })
        ));
    }
}
