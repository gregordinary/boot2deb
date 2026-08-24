//! Where a build's time went: the per-step summary printed when it finishes.
//!
//! A [`Timeline`] is fed the whole [`Event`] stream and keeps the one thing the
//! stream carries that no other output surfaces — how long each build-graph node
//! ran, and whether it ran at all or restored its outputs from the artifact cache.
//! Those two together are the point: a thirty-second kernel step is a cache hit,
//! and without the second column a reader has to infer that from the first.
//!
//! Nothing here is recorded in the provenance manifest. That document is
//! reproducibility evidence, and a wall-clock duration is not reproducible.

use boot2deb_engine::event::{Event, StepOutcome};
use std::cell::RefCell;
use std::time::Instant;

/// One finished step, in the order the build started it.
struct Row {
    step: String,
    duration_ms: u64,
    outcome: StepOutcome,
}

/// Accumulates [`Event::StepFinished`] across a build and renders the summary.
///
/// Interior mutability because it sits inside the event sink, which the
/// [`EventSink`](boot2deb_engine::EventSink) contract hands only `&self`.
pub(crate) struct Timeline {
    rows: RefCell<Vec<Row>>,
    started: Instant,
}

impl Timeline {
    /// Start the clock for the whole command. Constructed before the first step, so
    /// [`total`](Self::total) covers the work outside every step too.
    pub(crate) fn new() -> Self {
        Timeline {
            rows: RefCell::new(Vec::new()),
            started: Instant::now(),
        }
    }

    /// Record `event` if it is a step finishing; ignore it otherwise.
    ///
    /// A step that fails emits [`Event::Error`] instead of finishing, so it
    /// contributes no row — the summary describes what completed, and the error is
    /// already on the stream above it.
    pub(crate) fn record(&self, event: &Event) {
        if let Event::StepFinished {
            step,
            duration_ms,
            outcome,
        } = event
        {
            self.rows.borrow_mut().push(Row {
                step: step.clone(),
                duration_ms: *duration_ms,
                outcome: *outcome,
            });
        }
    }

    /// Wall-clock time since the timeline was started.
    fn total(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }

    /// The summary as rows for [`print_columns`](crate::render::print_columns):
    /// step, duration, outcome — then a `total` line.
    ///
    /// The total is the command's own wall clock, not the sum of the rows above it.
    /// The two differ by whatever a build does outside any step (resolving the build
    /// point, writing the provenance document), and the wall clock is the number an
    /// operator can check against their own.
    ///
    /// Empty when no step finished, which is every command that builds nothing and a
    /// build that failed in its first step.
    fn rows(&self) -> Vec<Vec<String>> {
        let rows = self.rows.borrow();
        if rows.is_empty() {
            return Vec::new();
        }
        let mut out: Vec<Vec<String>> = rows
            .iter()
            .map(|r| {
                vec![
                    r.step.clone(),
                    human_duration(r.duration_ms),
                    r.outcome.as_str().to_string(),
                ]
            })
            .collect();
        out.push(vec!["total".to_string(), human_duration(self.total())]);
        out
    }

    /// Print the summary under a `timing` header, or print nothing when no step
    /// finished.
    ///
    /// Suppressed by the caller under `--json` (the durations are already on the
    /// stream, per step, as structured events) and under `--quiet` (which asks for
    /// the artifacts and nothing about how they were produced).
    pub(crate) fn print(&self) {
        let rows = self.rows();
        if rows.is_empty() {
            return;
        }
        println!("\ntiming:");
        crate::render::print_columns(&rows);
    }
}

/// Render a millisecond count as a short human duration: `1h02m04s`, `12m04s`, or
/// `3.2s` below a minute.
///
/// The seconds place is zero-padded inside a larger unit so a column of durations
/// stays aligned on the digit rather than on the unit letter.
fn human_duration(ms: u64) -> String {
    let secs = ms / 1000;
    let (h, m, s) = (secs / 3600, (secs % 3600) / 60, secs % 60);
    if h > 0 {
        format!("{h}h{m:02}m{s:02}s")
    } else if m > 0 {
        format!("{m}m{s:02}s")
    } else {
        format!("{:.1}s", ms as f64 / 1000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn finished(step: &str, duration_ms: u64, outcome: StepOutcome) -> Event {
        Event::StepFinished {
            step: step.to_string(),
            duration_ms,
            outcome,
        }
    }

    #[test]
    fn human_duration_keeps_the_seconds_place_aligned() {
        assert_eq!(human_duration(3_200), "3.2s");
        assert_eq!(human_duration(724_000), "12m04s");
        assert_eq!(human_duration(3_724_000), "1h02m04s");
        // A whole number of minutes still prints its seconds, so the column does not
        // lose its last two characters on one row.
        assert_eq!(human_duration(120_000), "2m00s");
        assert_eq!(human_duration(0), "0.0s");
    }

    #[test]
    fn the_summary_keeps_the_order_the_build_ran_and_names_the_cache_hits() {
        let t = Timeline::new();
        t.record(&finished("kernel", 724_000, StepOutcome::Built));
        t.record(&finished("uboot", 3_000, StepOutcome::Restored));
        t.record(&finished("userspace", 60_000, StepOutcome::Mixed));
        // Everything that is not a step finishing is ignored, including the progress
        // and log events that vastly outnumber it.
        t.record(&Event::StepStarted {
            step: "image".into(),
        });
        t.record(&Event::Progress {
            step: "image".into(),
            pct: 50,
        });

        let rows = t.rows();
        assert_eq!(
            rows[..3]
                .iter()
                .map(|r| (r[0].as_str(), r[1].as_str(), r[2].as_str()))
                .collect::<Vec<_>>(),
            vec![
                ("kernel", "12m04s", "built"),
                ("uboot", "3.0s", "restored"),
                ("userspace", "1m00s", "partly restored"),
            ]
        );
        // The total is the command's wall clock, so it has no third cell to align
        // against the outcome column.
        assert_eq!(rows[3][0], "total");
        assert_eq!(rows[3].len(), 2);
        assert_eq!(rows.len(), 4);
    }

    #[test]
    fn a_build_that_finished_no_step_prints_no_summary() {
        let t = Timeline::new();
        t.record(&Event::Error {
            step: "kernel".into(),
            context: "make failed".into(),
        });
        assert!(t.rows().is_empty());
    }
}
