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
//!
//! Both halves batch. [`Fetch::fetch_all`]'s default body would serve one job at a time,
//! and the provisioner's package prefetch chunks in plan order across every configured
//! repository — so a chunk holds whichever schemes those repositories happen to use.
//! [`ArchiveFetch::fetch_all`] therefore partitions a batch by scheme, passes the plain
//! half to [`HttpFetch`] as a batch of its own, serves the `https://` half here through a
//! bounded pool of its own, and runs the two halves at once.

use crate::netfetch::MAX_REDIRECTS;
use ferroday_cage::provision::{Fetch, FetchError, FetchJob, FetchRequest, HttpFetch};
use std::io::Write;
use std::sync::Mutex;
use std::thread;

/// How many `https://` bodies of one batch are in flight at once.
///
/// The bound the library's own client defaults to, for the same reason it has one: a
/// vendor repository serving a single build should see a handful of connections, not one
/// per package the plan happens to draw from it.
const TLS_AT_ONCE: usize = 4;

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

/// Whether `request` names the one scheme this transport serves itself.
///
/// Compared case-insensitively, because a scheme is (RFC 3986 §3.1) and the library's
/// client reads its own that way: routing `HTTPS://` to the plain client would have it
/// refused with advice to supply a TLS transport — which this is.
fn names_tls(request: &FetchRequest<'_>) -> bool {
    request
        .scheme()
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("https"))
}

/// Fetches one `https://` body through `agent`.
///
/// Free rather than a method on [`ArchiveFetch`] because a batch serves several at once
/// from threads of its own, each holding `&ureq::Agent`, while [`Fetch::fetch`] holds
/// `&mut self` and could hand out only one such borrow.
fn fetch_tls(
    agent: &ureq::Agent,
    request: &FetchRequest<'_>,
    sink: &mut dyn Write,
) -> Result<(), FetchError> {
    let url = request.url();
    match agent.get(url).call() {
        Ok(response) => {
            let mut body = response.into_reader();
            std::io::copy(&mut body, sink).map_err(FetchError::at("reading the body", url))?;
            Ok(())
        }
        // A missing resource **must** be reported as `NotFound` rather than as a
        // status: the provisioner treats it as a recoverable absence and falls back
        // to another index compression or a non-by-hash path, and a plain status
        // would turn a normal probe into a hard failure.
        Err(ureq::Error::Status(404, _)) => Err(FetchError::not_found(url)),
        Err(ureq::Error::Status(code, _)) => Err(FetchError::status(url, code)),
        Err(err @ ureq::Error::Transport(_)) => Err(FetchError::io(
            "fetching over https",
            url,
            std::io::Error::other(err.to_string()),
        )),
    }
}

/// Serves a whole batch of `https://` jobs, [`TLS_AT_ONCE`] at a time.
///
/// Answers are returned by position, as [`Fetch::fetch_all`] requires: a worker writes
/// into the slot handed out with the job it took, so no two ever hold the same one and
/// the order jobs finish in does not reach the caller.
fn fetch_all_tls(agent: &ureq::Agent, jobs: &mut [FetchJob<'_>]) -> Vec<Result<(), FetchError>> {
    let workers = TLS_AT_ONCE.min(jobs.len());
    if workers <= 1 {
        return jobs
            .iter_mut()
            .map(|job| {
                let (request, sink) = job.parts();
                fetch_tls(agent, request, sink)
            })
            .collect();
    }

    let mut outcomes: Vec<Option<Result<(), FetchError>>> = jobs.iter().map(|_| None).collect();
    let queue = Mutex::new(
        jobs.iter_mut()
            .zip(outcomes.iter_mut())
            .collect::<Vec<_>>()
            .into_iter(),
    );

    thread::scope(|scope| {
        for _ in 0..workers {
            let queue = &queue;
            scope.spawn(move || loop {
                // The lock is held only long enough to take the next job, never across
                // a fetch — a body arriving is what this is meant to overlap.
                let next = queue
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .next();
                let Some((job, outcome)) = next else {
                    return;
                };
                let (request, sink) = job.parts();
                *outcome = Some(fetch_tls(agent, request, sink));
            });
        }
    });

    outcomes
        .into_iter()
        .map(|outcome| outcome.expect("every job was handed to a worker"))
        .collect()
}

impl Fetch for ArchiveFetch {
    fn fetch(
        &mut self,
        request: &FetchRequest<'_>,
        sink: &mut dyn Write,
    ) -> Result<(), FetchError> {
        if names_tls(request) {
            fetch_tls(&self.tls, request, sink)
        } else {
            self.plain.fetch(request, sink)
        }
    }

    fn fetch_all(&mut self, jobs: &mut [FetchJob<'_>]) -> Vec<Result<(), FetchError>> {
        // An override answers every job in the slice — there is no way to call the
        // default body it replaces — so each half's answers are scattered back to the
        // positions its jobs came from, and a `None` left at the end is a partition that
        // lost one.
        let mut outcomes: Vec<Option<Result<(), FetchError>>> = jobs.iter().map(|_| None).collect();

        let (mut tls_half, mut plain_half) = (Vec::new(), Vec::new());
        for (index, job) in jobs.iter_mut().enumerate() {
            let (request, sink) = job.parts();
            let half = if names_tls(request) {
                &mut tls_half
            } else {
                &mut plain_half
            };
            // Rebuilt at the shorter borrow `parts` handed back, so what each half
            // receives is a batch its client can overlap rather than a run of single
            // requests.
            half.push((index, FetchJob::new(*request, sink)));
        }

        let (plain_at, mut plain_batch): (Vec<usize>, Vec<FetchJob<'_>>) =
            plain_half.into_iter().unzip();
        let (tls_at, mut tls_batch): (Vec<usize>, Vec<FetchJob<'_>>) = tls_half.into_iter().unzip();

        // Borrowed apart so one half can run on a thread of its own while the other runs
        // here.
        let ArchiveFetch { plain, tls } = self;
        let (plain_outcomes, tls_outcomes) = if plain_batch.is_empty() || tls_batch.is_empty() {
            // A homogeneous batch is the common one, since a repository's packages sit
            // together in the plan's order. It reaches the client that speaks for it
            // whole, with no thread to hand an empty half to.
            (
                plain.fetch_all(&mut plain_batch),
                fetch_all_tls(tls, &mut tls_batch),
            )
        } else {
            // A mixed batch is the case this override exists for, so the halves overlap:
            // serving one after the other would leave a build waiting out the vendor
            // repository's packages with the mirror idle.
            thread::scope(|scope| {
                let plain_worker = scope.spawn(|| plain.fetch_all(&mut plain_batch));
                let tls_outcomes = fetch_all_tls(tls, &mut tls_batch);
                let plain_outcomes = plain_worker
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic));
                (plain_outcomes, tls_outcomes)
            })
        };

        for (index, outcome) in plain_at.into_iter().zip(plain_outcomes) {
            outcomes[index] = Some(outcome);
        }
        for (index, outcome) in tls_at.into_iter().zip(tls_outcomes) {
            outcomes[index] = Some(outcome);
        }
        outcomes
            .into_iter()
            .map(|outcome| outcome.expect("every job went to one half or the other"))
            .collect()
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

    /// A scheme is case-insensitive (RFC 3986), so `HTTPS://` must reach the TLS agent —
    /// the plain client would refuse it with advice to supply the transport this is. The
    /// unservable loopback port keeps the test off the network: reaching the agent means
    /// failing there, as the transport failure the `https` arm reports.
    #[test]
    fn an_uppercase_https_scheme_still_reaches_the_tls_agent() {
        let mut fetch = ArchiveFetch::new();
        let mut body = Vec::new();
        assert!(matches!(
            fetch.fetch(&FetchRequest::new("HTTPS://127.0.0.1:1/x"), &mut body),
            Err(FetchError::Io {
                op: "fetching over https",
                ..
            })
        ));
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

    /// The batch a build actually produces: the mirror's packages and a vendor
    /// repository's in one chunk. Partitioning serves them out of the order they were
    /// asked for, and the caller reads the answers by position — so this pins that each
    /// one lands at its own job's index, and that a body written by one half is not
    /// written into another job's sink.
    #[test]
    fn a_mixed_batch_answers_every_job_at_its_own_index() {
        let tmp = tempfile::tempdir().unwrap();
        let present = tmp.path().join("Release");
        std::fs::write(&present, b"Suite: forky\n").unwrap();
        let present_url = format!("file://{}", present.display());
        let absent_url = format!("file://{}/absent", tmp.path().display());

        let (mut first_tls, mut body, mut missing, mut second_tls) =
            (Vec::new(), Vec::new(), Vec::new(), Vec::new());
        let mut jobs = vec![
            FetchJob::new(FetchRequest::new("https://127.0.0.1:1/a"), &mut first_tls),
            FetchJob::new(FetchRequest::new(&present_url), &mut body),
            FetchJob::new(FetchRequest::new(&absent_url), &mut missing),
            FetchJob::new(FetchRequest::new("https://127.0.0.1:1/b"), &mut second_tls),
        ];
        let outcomes = ArchiveFetch::new().fetch_all(&mut jobs);
        drop(jobs);

        assert_eq!(outcomes.len(), 4);
        assert!(matches!(
            outcomes[0],
            Err(FetchError::Io {
                op: "fetching over https",
                ..
            })
        ));
        assert!(outcomes[1].is_ok());
        assert!(matches!(outcomes[2], Err(FetchError::NotFound { .. })));
        assert!(matches!(
            outcomes[3],
            Err(FetchError::Io {
                op: "fetching over https",
                ..
            })
        ));
        assert_eq!(body, b"Suite: forky\n");
        assert!(first_tls.is_empty() && missing.is_empty() && second_tls.is_empty());
    }

    /// More `https://` jobs than [`TLS_AT_ONCE`], so the batch is served by a pool of
    /// workers rather than in a straight line: every job is still answered exactly once,
    /// which is what a slot handed out with its job buys.
    #[test]
    fn every_job_of_an_all_https_batch_is_answered() {
        let urls: Vec<String> = (0..TLS_AT_ONCE + 1)
            .map(|n| format!("https://127.0.0.1:1/{n}"))
            .collect();
        let mut bodies: Vec<Vec<u8>> = urls.iter().map(|_| Vec::new()).collect();
        let mut jobs: Vec<FetchJob<'_>> = bodies
            .iter_mut()
            .zip(&urls)
            .map(|(body, url)| FetchJob::new(FetchRequest::new(url), body))
            .collect();

        let outcomes = ArchiveFetch::new().fetch_all(&mut jobs);

        assert_eq!(outcomes.len(), urls.len());
        assert!(outcomes.iter().all(|outcome| matches!(
            outcome,
            Err(FetchError::Io {
                op: "fetching over https",
                ..
            })
        )));
    }

    /// The common batch: one repository's packages sit together in the plan's order, so
    /// nothing in the chunk names TLS and the whole slice reaches the library's client
    /// as a batch — which is the point of overriding the seam at all.
    #[test]
    fn a_batch_naming_no_tls_is_served_whole_by_the_librarys_own_client() {
        let tmp = tempfile::tempdir().unwrap();
        let urls: Vec<String> = (0..3)
            .map(|n| {
                let path = tmp.path().join(format!("deb-{n}"));
                std::fs::write(&path, format!("body {n}")).unwrap();
                format!("file://{}", path.display())
            })
            .collect();
        let mut bodies: Vec<Vec<u8>> = urls.iter().map(|_| Vec::new()).collect();
        let mut jobs: Vec<FetchJob<'_>> = bodies
            .iter_mut()
            .zip(&urls)
            .map(|(body, url)| FetchJob::new(FetchRequest::new(url), body))
            .collect();

        let outcomes = ArchiveFetch::new().fetch_all(&mut jobs);
        drop(jobs);

        assert!(outcomes.iter().all(Result::is_ok));
        assert_eq!(bodies[0], b"body 0");
        assert_eq!(bodies[2], b"body 2");
    }
}
