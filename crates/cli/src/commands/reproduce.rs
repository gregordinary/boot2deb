//! `reproduce`: rebuild an image from a published plan document rather than from a
//! fresh archive resolve.
//!
//! A recipe's `.lock` pins sources, patches, and the builder. It does not pin *which
//! package versions the archive served*, so the same lock a month later resolves a
//! different userland. The plan document a build publishes beside its image pins exactly
//! that — every package name, version and sha256, plus the state of each repository they
//! were selected from — and this command replays it.
//!
//! It is [`build`](super::build) with one substitution: the rootfs installs the plan it
//! is given instead of resolving one. Every other input, flag and stage is the same, so
//! the two commands share one pipeline. What differs is the trust model, and that is why
//! this is its own command rather than a flag: a pinned install reads neither a release
//! nor a package index, so the plan document — not an archive signature — is what the
//! package digests chain to. See [`RootfsOptions::pinned_plan`](boot2deb_engine::rootfs::RootfsOptions::pinned_plan).
//!
//! The builder is the third reproducibility axis and is not enforced here. The published
//! provenance manifest records which boot2deb produced the image, so this reports how the
//! running checkout compares and leaves the decision to the operator — a stamped commit
//! is a floor, not a ceiling, and a newer builder usually reproduces the image and may
//! carry fixes.

use crate::args::BuildArgs;
use crate::render::{note, Verbosity};
use boot2deb_core::provenance::BuiltWithProvenance;
use boot2deb_core::ConfigRoot;
use std::path::{Path, PathBuf};

/// Run `reproduce <recipe>`.
pub(crate) fn run(
    root: &ConfigRoot,
    recipe: &str,
    from: Option<PathBuf>,
    args: BuildArgs,
    json: bool,
    verbosity: Verbosity,
) -> Result<(), Box<dyn std::error::Error>> {
    // The same point resolution `build` performs, for the same reason: every published
    // artifact is named for the build point, so a feature variant's plan sits beside a
    // variant-named image and never beside the base recipe's.
    let point = crate::config::build_point(recipe, args.features.clone())?;
    let reference = point.reference();
    let stem = point.artifact_stem();

    // Where the earlier build published. Defaulted to where this build point's own
    // artifacts land, so reproducing in place — the common case while checking that a
    // build is reproducible at all — needs no flag.
    let published = from.unwrap_or_else(|| {
        crate::fsutil::absolutize(args.out_dir.clone().unwrap_or_else(|| {
            crate::workdir::work_dir_for(root, reference.as_str(), args.work_dir.clone())
                .join("artifacts")
        }))
    });

    let plan = published.join(format!("{stem}.plan"));
    if !plan.exists() {
        return Err(format!(
            "no plan document at {} — `reproduce` replays the one a build publishes \
             beside its image. Point --from at the directory holding {stem}.plan (it \
             ships with the image and its provenance manifest).",
            plan.display()
        )
        .into());
    }

    // The build's own event stream does not exist yet — `build::run` owns it — so these
    // two lines go out on the same stdout contract, rendered for a human or as NDJSON,
    // and are then followed by the build's own stream on the same terminal.
    let sink = move |e: boot2deb_engine::event::Event| {
        if json {
            crate::render::print_event_json(&e)
        } else {
            crate::render::print_event_at(verbosity, &e)
        }
    };
    note(
        json,
        verbosity,
        &sink,
        "reproduce",
        format!(
            "replaying {} — the rootfs installs this plan and consults no archive index",
            plan.display()
        ),
    );
    // The builder advisory: what produced the image, against what is running now. Read
    // from the provenance manifest beside the plan when one is there, and skipped
    // quietly when it is not — a plan alone is enough to replay, and refusing for a
    // missing advisory would make the record a requirement it was never meant to be.
    let provenance = published.join(format!("{stem}.provenance.toml"));
    let line = match builder_stamp(&provenance)? {
        Some(stamp) => advice(&stamp),
        None => format!(
            "no provenance manifest at {} — replaying the plan without a builder \
             comparison",
            provenance.display()
        ),
    };
    note(json, verbosity, &sink, "reproduce", line);

    super::build::run(root, recipe, args, Some(&plan), json, verbosity)
}

/// One line comparing the builder that produced the image with the running one.
///
/// Advisory in both directions. A match is worth stating because it is the case that
/// needs no action; a mismatch names the checkout to step back to without claiming the
/// replay will fail, because a stamp is the commit at which the build *worked* and never
/// the commit past which it breaks — that change is in the future and unknowable at
/// build time.
fn advice(stamp: &BuiltWithProvenance) -> String {
    let running_version = env!("CARGO_PKG_VERSION");
    let running_commit = option_env!("BOOT2DEB_GIT_COMMIT").filter(|s| !s.is_empty());
    let built = match (&stamp.commit, stamp.dirty) {
        (Some(commit), true) => format!("{} ({commit}, dirty)", stamp.version),
        (Some(commit), false) => format!("{} ({commit})", stamp.version),
        (None, _) => stamp.version.clone(),
    };
    let running = match running_commit {
        Some(commit) => format!("{running_version} ({commit})"),
        None => running_version.to_string(),
    };
    if stamp.dirty {
        return format!(
            "built with boot2deb {built}; running {running}. The stamped checkout had \
             uncommitted changes, so no commit identifies the builder that produced this \
             image."
        );
    }
    match (&stamp.commit, running_commit) {
        (Some(built_commit), Some(running_commit)) if built_commit == running_commit => {
            format!("built with boot2deb {built}; running the same checkout.")
        }
        (Some(built_commit), _) => format!(
            "built with boot2deb {built}; running {running}. A newer builder usually \
             reproduces the image and may carry fixes — step back with \
             `git checkout {built_commit}` only if it diverges."
        ),
        (None, _) => format!(
            "built with boot2deb {built}; running {running}. The image was built outside \
             a git checkout, so only the version identifies its builder."
        ),
    }
}

/// Read the builder stamp from a provenance manifest, or `None` when there is no
/// manifest to read.
///
/// A manifest that exists and cannot be parsed *is* an error: it names the file the
/// operator pointed at, and silently treating a corrupt record as an absent one would
/// report "no builder comparison" for a document that has one.
fn builder_stamp(path: &Path) -> Result<Option<BuiltWithProvenance>, Box<dyn std::error::Error>> {
    if !path.exists() {
        return Ok(None);
    }
    let text =
        std::fs::read_to_string(path).map_err(|e| format!("read {}: {e}", path.display()))?;
    Ok(Some(boot2deb_core::provenance::builder_stamp(
        &text,
        &path.display().to_string(),
    )?))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(version: &str, commit: Option<&str>, dirty: bool) -> BuiltWithProvenance {
        BuiltWithProvenance {
            version: version.to_string(),
            commit: commit.map(str::to_string),
            dirty,
        }
    }

    /// A dirty stamp is the one case where the commit says nothing, so the advice must
    /// not offer it as somewhere to step back to.
    #[test]
    fn a_dirty_stamp_does_not_offer_a_checkout_to_step_back_to() {
        let advice = advice(&stamp("0.1.0", Some("abc1234"), true));
        assert!(
            advice.contains("uncommitted changes") && !advice.contains("git checkout"),
            "a dirty stamp must not name a commit to return to: {advice}"
        );
    }

    /// The mismatch case is the one an operator acts on, so it names the commit and
    /// frames it as advice rather than as a requirement.
    #[test]
    fn a_differing_commit_names_it_and_stays_advisory() {
        let advice = advice(&stamp("0.1.0", Some("abc1234"), false));
        assert!(
            advice.contains("git checkout abc1234"),
            "the advice must name the stamped commit: {advice}"
        );
        assert!(
            advice.contains("only if it diverges"),
            "the advice must stay advisory: {advice}"
        );
    }

    /// A manifest that is present but unreadable is an error rather than a missing
    /// advisory — the difference between "there is nothing to compare" and "the record
    /// is broken" is exactly what an operator reproducing an image needs told.
    #[test]
    fn a_corrupt_manifest_is_an_error_not_an_absent_stamp() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.provenance.toml");
        std::fs::write(&path, "this is not toml\x00").unwrap();
        assert!(builder_stamp(&path).is_err());
        assert!(builder_stamp(&dir.path().join("absent.toml"))
            .unwrap()
            .is_none());
    }

    /// The stamp is read out of a whole provenance manifest, and the manifest carries a
    /// banner of TOML comments plus sections this struct does not name — including the
    /// first-boot credential, which must not have to be parsed to read the builder.
    #[test]
    fn the_stamp_is_read_from_a_full_manifest_without_its_other_sections() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("x.provenance.toml");
        std::fs::write(
            &path,
            "# boot2deb provenance manifest\n\
             [image]\n\
             device = \"turing-rk1\"\n\
             \n\
             [credentials]\n\
             password = \"secret\"\n\
             \n\
             [built_with]\n\
             version = \"0.1.0\"\n\
             commit = \"abc1234\"\n\
             dirty = false\n",
        )
        .unwrap();
        let stamp = builder_stamp(&path).unwrap().expect("the stamp is present");
        assert_eq!(stamp.version, "0.1.0");
        assert_eq!(stamp.commit.as_deref(), Some("abc1234"));
    }
}
