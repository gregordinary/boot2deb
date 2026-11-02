//! The record of the newest build of one build point: which stages it ran, when, and
//! how it ended.
//!
//! An artifact directory outlives the builds that write into it. A build that fails
//! half way leaves the previous build's image, provenance and packages where they
//! were. Nothing in those files says a newer attempt happened. A check of the directory
//! would then vouch for an image the newest build did not produce. The record is what
//! says it.
//!
//! `build` writes the record beside the artifacts as soon as it is admitted, marked
//! [`Outcome::Running`]. When the run ends the record changes to
//! [`Succeeded`](Outcome::Succeeded) or [`Failed`](Outcome::Failed). A run that never
//! ends, killed or crashed, leaves it `running`, which is the truth.
//!
//! `verify-image` reads it first ([`BuildRecord::vouches_for_image`]) and verifies
//! nothing the newest whole build did not write.
//!
//! Pure: parsing, formatting and the verdict. The CLI reads the clock and writes the
//! file.

use crate::error::ConfigError;
use serde::{Deserialize, Serialize};

/// The file name of a build point's record, beside its other artifacts.
pub fn record_name(stem: &str) -> String {
    format!("{stem}.build.toml")
}

/// How the newest build of a point ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum Outcome {
    /// The build was admitted and has not ended, or ended without settling its record.
    Running,
    /// Every stage the build ran finished.
    Succeeded,
    /// A stage failed, or the build was refused after it was admitted.
    Failed,
}

/// The newest build of one build point.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BuildRecord {
    /// The build point's reference (`turing-rk1/forky+jellyfin`).
    pub point: String,
    /// The `--stage` it ran, as typed (`all` for the whole pipeline).
    pub stage: String,
    /// How it ended.
    pub outcome: Outcome,
    /// When it was admitted, RFC 3339 UTC.
    pub started: String,
    /// When it ended, RFC 3339 UTC. Absent while [`Running`](Outcome::Running).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub finished: Option<String>,
    /// The error it ended with. Present only for [`Failed`](Outcome::Failed).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The stage name of a whole build: the only stage whose success vouches for every
/// artifact the directory holds.
pub const WHOLE_BUILD: &str = "all";

impl BuildRecord {
    /// A record of a build admitted at `started`, not yet ended.
    pub fn started(point: &str, stage: &str, started: String) -> Self {
        Self {
            point: point.to_string(),
            stage: stage.to_string(),
            outcome: Outcome::Running,
            started,
            finished: None,
            error: None,
        }
    }

    /// This record, ended at `finished`: succeeded when `error` is `None`, failed with
    /// it otherwise.
    pub fn ended(mut self, finished: String, error: Option<String>) -> Self {
        self.outcome = match error {
            None => Outcome::Succeeded,
            Some(_) => Outcome::Failed,
        };
        self.finished = Some(finished);
        self.error = error;
        self
    }

    /// Whether the artifacts beside this record are the ones the newest build wrote,
    /// for a check that holds them all at once.
    ///
    /// Only a whole build that succeeded vouches for them. A failed or unfinished build
    /// can have replaced some files and not others. A succeeded partial build
    /// (`--stage kernel`) leaves an image older than the packages beside it.
    ///
    /// # Errors
    ///
    /// Why the record does not vouch, phrased for an operator.
    pub fn vouches_for_image(&self) -> Result<(), String> {
        match self.outcome {
            Outcome::Running => Err(format!(
                "the newest build of {} (started {}) never finished, so these artifacts \
                 may be part old and part new",
                self.point, self.started
            )),
            Outcome::Failed => Err(format!(
                "the newest build of {} failed ({}), so these artifacts are from an earlier \
                 one: {}",
                self.point,
                self.finished.as_deref().unwrap_or("unknown time"),
                self.error.as_deref().unwrap_or("no error recorded")
            )),
            Outcome::Succeeded if self.stage != WHOLE_BUILD => Err(format!(
                "the newest build of {} ran only --stage {}, so the image beside it is from \
                 an earlier whole build",
                self.point, self.stage
            )),
            Outcome::Succeeded => Ok(()),
        }
    }

    /// The record as the TOML document `build` writes.
    pub fn to_toml(&self) -> String {
        let body = toml::to_string(self).expect("a build record always serializes");
        format!("# Written by `boot2deb build`: how the newest build of this point ended.\n{body}")
    }

    /// Parse a record. `origin` names the file in an error.
    ///
    /// # Errors
    ///
    /// [`ConfigError::Parse`] for a document that is not a build record.
    pub fn from_toml_str(text: &str, origin: &str) -> Result<Self, ConfigError> {
        toml::from_str(text).map_err(|source| ConfigError::Parse {
            path: origin.to_string(),
            source,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(stage: &str) -> BuildRecord {
        BuildRecord::started("turing-rk1/forky", stage, "2026-10-08T12:00:00Z".into())
    }

    #[test]
    fn only_a_whole_build_that_succeeded_vouches() {
        let ok = record("all").ended("2026-10-08T13:00:00Z".into(), None);
        assert_eq!(ok.outcome, Outcome::Succeeded);
        assert_eq!(ok.vouches_for_image(), Ok(()));

        let running = record("all");
        assert!(running
            .vouches_for_image()
            .unwrap_err()
            .contains("never finished"));

        let failed = record("all").ended(
            "2026-10-08T12:05:00Z".into(),
            Some("rootfs: solve failed".into()),
        );
        assert_eq!(failed.outcome, Outcome::Failed);
        let why = failed.vouches_for_image().unwrap_err();
        assert!(why.contains("rootfs: solve failed"), "{why}");

        let partial = record("kernel").ended("2026-10-08T12:30:00Z".into(), None);
        let why = partial.vouches_for_image().unwrap_err();
        assert!(why.contains("--stage kernel"), "{why}");
    }

    #[test]
    fn the_document_round_trips_and_omits_what_a_running_build_lacks() {
        let running = record("all");
        let text = running.to_toml();
        assert!(text.contains("outcome = \"running\""), "{text}");
        assert!(!text.contains("finished"), "{text}");
        assert_eq!(BuildRecord::from_toml_str(&text, "t").unwrap(), running);

        let failed = running.ended("2026-10-08T12:05:00Z".into(), Some("boom".into()));
        assert_eq!(
            BuildRecord::from_toml_str(&failed.to_toml(), "t").unwrap(),
            failed
        );
        assert!(BuildRecord::from_toml_str("outcome = \"maybe\"\n", "t").is_err());
    }

    #[test]
    fn the_record_is_named_for_the_point_stem() {
        assert_eq!(
            record_name("turing-rk1-forky"),
            "turing-rk1-forky.build.toml"
        );
    }
}
