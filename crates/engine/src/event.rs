//! The structured build event stream — the contract the CLI consumes (human
//! rendering, or NDJSON under `--json`) and the Dioxus UI will consume later.
//!
//! The stream is delivered in-process: a stage emits [`Event`]s to an
//! [`EventSink`] (a callback or trait object). The serialized form is the
//! CLI's `--json` wire format: one event per line, each a JSON object tagged
//! by its `event` field (the serde `tag` below), e.g.
//! `{"event":"step_started","step":"kernel"}`. Variants and fields may still
//! grow; consumers should ignore unknown `event` tags.
//!
//! Every event carries the `step` it belongs to (a build-graph node such as
//! `kernel` or `uboot`), so a flat stream stays self-describing once
//! independent nodes emit concurrently.

use serde::{Deserialize, Serialize};
use std::cell::Cell;
use std::time::Instant;

/// Which subprocess stream a [`Event::Log`] line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stream {
    /// Standard output.
    Stdout,
    /// Standard error (where `make`/`git` write progress and diagnostics).
    Stderr,
}

/// Who wrote a [`Event::Log`] line.
///
/// The two are the same variant because they belong to the same step and the same
/// ordering, but they are not the same *kind* of information: a stage's own lines
/// summarize what it decided ("reusing the kernel tree", "restored from the artifact
/// cache") in tens of lines, while relayed output is the tens of thousands of lines
/// `make` emits. Without the distinction a renderer has to choose between showing
/// everything and showing nothing; with it, a default verbosity can show what the
/// build decided and leave the compile chatter behind `--verbose`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LogOrigin {
    /// The stage itself, via [`Step::log`] — a decision or a summary.
    Stage,
    /// A subprocess the stage is relaying (`make`, `git`, `dpkg-buildpackage`).
    Subprocess,
}

/// Where a finished step's outputs came from.
///
/// The companion to a step's duration, and the reason the duration is worth reading:
/// a thirty-second kernel step is a cache hit, and a reader who cannot tell the two
/// apart has to guess. Reported by the step itself rather than inferred from how long
/// it took, since that inference is exactly what this removes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum StepOutcome {
    /// This run produced the step's outputs. Also what a step with nothing to cache
    /// reports — the image node assembles a disk every time.
    Built,
    /// Every output came back from the artifact cache; nothing was compiled.
    Restored,
    /// Some outputs were restored and some were built. Reachable only from a step
    /// whose outputs are cached individually rather than as a set — the userspace
    /// stage builds several `.deb`s, each with its own signature.
    Mixed,
}

impl StepOutcome {
    /// The outcome as a short human word, for a summary column.
    pub fn as_str(self) -> &'static str {
        match self {
            StepOutcome::Built => "built",
            StepOutcome::Restored => "restored",
            StepOutcome::Mixed => "partly restored",
        }
    }
}

/// A single event in a build's structured stream.
///
/// Consumers render or forward these; they are the whole observable surface of a
/// running build. `pct` on [`Progress`](Event::Progress) is coarse and
/// phase-based (a stage reports it at sub-step boundaries), not a fine-grained
/// byte/line ratio.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    /// A build step began.
    StepStarted {
        /// Build-graph node name (e.g. `kernel`).
        step: String,
    },
    /// Coarse progress within a step, `0..=100`.
    Progress {
        /// The step this progress belongs to.
        step: String,
        /// Percent complete, phase-based.
        pct: u8,
    },
    /// One line of output attributed to a step.
    Log {
        /// The step that produced the line.
        step: String,
        /// Whether it came from stdout or stderr.
        stream: Stream,
        /// Who wrote it — the stage itself, or a subprocess it is relaying.
        origin: LogOrigin,
        /// The line, with its trailing newline stripped.
        line: String,
    },
    /// A build step finished successfully.
    StepFinished {
        /// The step that finished.
        step: String,
        /// How long it ran, measured monotonically from [`Step::start`]. Carried on
        /// the event rather than left to each consumer's own clock, so a stream read
        /// from a saved NDJSON log still says how long the build took, and a buffered
        /// consumer does not time its own delivery.
        duration_ms: u64,
        /// Where the step's outputs came from — what makes [`duration_ms`](Self::StepFinished::duration_ms)
        /// interpretable.
        outcome: StepOutcome,
    },
    /// A produced artifact's location — the structured counterpart of the CLI's
    /// human `role : path` summary lines, so a `--json` consumer gets the paths
    /// (image, `.deb`s, boot payloads) without scraping log lines.
    Artifact {
        /// The step that produced it.
        step: String,
        /// What the artifact is within its step (e.g. `image_deb`, `idbloader`).
        role: String,
        /// Its path on the build host.
        path: String,
    },
    /// A build step failed. The build stops; `context` is a human-readable
    /// summary (the typed [`EngineError`](crate::EngineError) is returned
    /// separately to the caller).
    Error {
        /// The step that failed.
        step: String,
        /// Human-readable failure summary.
        context: String,
    },
}

/// A consumer of the [`Event`] stream. Implemented in-process by the CLI (which
/// prints) and, later, by whatever bridges the stream to the UI.
///
/// A blanket impl covers any `Fn(Event)`, so a closure is a sink. `emit` takes
/// `&self`; a sink that accumulates uses interior mutability.
pub trait EventSink {
    /// Deliver one event.
    fn emit(&self, event: Event);
}

impl<F: Fn(Event)> EventSink for F {
    fn emit(&self, event: Event) {
        self(event)
    }
}

/// A handle bound to one step and the sink, so a stage emits events without
/// repeating the step name. Constructed with [`Step::start`] (which emits
/// [`Event::StepStarted`]); call [`Step::finish`] on success. On failure a stage
/// returns its error instead of finishing, and the orchestrator emits
/// [`Event::Error`].
pub struct Step<'a> {
    sink: &'a dyn EventSink,
    name: String,
    started: Instant,
    /// Whether any output was restored from the artifact cache, and whether any was
    /// produced by this run — together, the [`StepOutcome`] reported at
    /// [`finish`](Step::finish). `Cell` because a stage holds the step by shared
    /// reference while it works.
    restored: Cell<bool>,
    compiled: Cell<bool>,
}

impl<'a> Step<'a> {
    /// Begin a step, emitting [`Event::StepStarted`] and starting its clock.
    pub fn start(sink: &'a dyn EventSink, name: impl Into<String>) -> Self {
        let name = name.into();
        sink.emit(Event::StepStarted { step: name.clone() });
        Step {
            sink,
            name,
            started: Instant::now(),
            restored: Cell::new(false),
            compiled: Cell::new(false),
        }
    }

    /// Record that one of this step's outputs came back from the artifact cache
    /// instead of being produced by this run.
    ///
    /// A step that restores *some* of its outputs must also call
    /// [`compiled`](Step::compiled) for the rest, or it will claim to have restored a
    /// set it partly compiled. Stages that cache through the shared
    /// `restore_stage_outputs`/`store_stage_outputs` pair have both halves recorded for
    /// them, so only a stage driving the artifact store itself calls these.
    pub fn restored(&self) {
        self.restored.set(true);
    }

    /// Record that one of this step's outputs was produced by this run — the other
    /// half of the pair described on [`restored`](Step::restored).
    ///
    /// A step that restores nothing may skip this: with `restored` unset the outcome
    /// is [`StepOutcome::Built`] regardless, which already claims the work was done.
    pub fn compiled(&self) {
        self.compiled.set(true);
    }

    /// Emit an informational [`Event::Log`] line (stdout-tagged) from the stage
    /// itself, as opposed to relayed subprocess output.
    pub fn log(&self, line: impl Into<String>) {
        self.emit(Stream::Stdout, LogOrigin::Stage, line.into());
    }

    /// Emit a coarse [`Event::Progress`] update.
    pub fn progress(&self, pct: u8) {
        self.sink.emit(Event::Progress {
            step: self.name.clone(),
            pct,
        });
    }

    /// Emit [`Event::StepFinished`] with the elapsed time and the
    /// [`StepOutcome`]. Consumes the handle so it cannot fire twice.
    pub fn finish(self) {
        self.sink.emit(Event::StepFinished {
            step: self.name.clone(),
            duration_ms: self.started.elapsed().as_millis() as u64,
            outcome: match (self.restored.get(), self.compiled.get()) {
                (true, true) => StepOutcome::Mixed,
                (true, false) => StepOutcome::Restored,
                // Nothing was restored, so this run did whatever work there was.
                (false, _) => StepOutcome::Built,
            },
        });
    }

    /// Relay one line of subprocess output on `stream`. Used by the streaming
    /// runner ([`run`](crate::build::run)).
    pub(crate) fn relay(&self, stream: Stream, line: String) {
        self.emit(stream, LogOrigin::Subprocess, line);
    }

    /// Emit one [`Event::Log`], tagged with who wrote it.
    pub(crate) fn emit(&self, stream: Stream, origin: LogOrigin, line: String) {
        self.sink.emit(Event::Log {
            step: self.name.clone(),
            stream,
            origin,
            line,
        });
    }

    /// The step's name.
    pub fn name(&self) -> &str {
        &self.name
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    /// A sink that records every event, for asserting on the emitted sequence.
    fn recorder(log: &RefCell<Vec<Event>>) -> impl EventSink + '_ {
        move |e: Event| log.borrow_mut().push(e)
    }

    #[test]
    fn events_serialize_to_the_tagged_ndjson_shape() {
        // The serialized form is the CLI's `--json` wire format; these literals
        // are the documented schema, so a rename or retag is a breaking change
        // this test makes deliberate.
        let started = serde_json::to_string(&Event::StepStarted {
            step: "kernel".into(),
        })
        .unwrap();
        assert_eq!(started, r#"{"event":"step_started","step":"kernel"}"#);
        let finished = serde_json::to_string(&Event::StepFinished {
            step: "uboot".into(),
            duration_ms: 3_412,
            outcome: StepOutcome::Restored,
        })
        .unwrap();
        assert_eq!(
            finished,
            r#"{"event":"step_finished","step":"uboot","duration_ms":3412,"outcome":"restored"}"#
        );
        let artifact = serde_json::to_string(&Event::Artifact {
            step: "image".into(),
            role: "compressed".into(),
            path: "/out/img.xz".into(),
        })
        .unwrap();
        assert_eq!(
            artifact,
            r#"{"event":"artifact","step":"image","role":"compressed","path":"/out/img.xz"}"#
        );
        // Round-trips, for the consumer side of the same enum.
        let back: Event = serde_json::from_str(&artifact).unwrap();
        assert!(matches!(back, Event::Artifact { .. }));
    }

    #[test]
    fn step_emits_started_log_progress_finished_in_order() {
        let log = RefCell::new(Vec::new());
        let sink = recorder(&log);
        let step = Step::start(&sink, "kernel");
        step.progress(10);
        step.log("configuring");
        step.finish();

        let events = log.borrow();
        assert_eq!(
            events[..3],
            [
                Event::StepStarted {
                    step: "kernel".into()
                },
                Event::Progress {
                    step: "kernel".into(),
                    pct: 10
                },
                Event::Log {
                    step: "kernel".into(),
                    stream: Stream::Stdout,
                    // `Step::log` is the stage speaking, which is what makes it
                    // survive the default verbosity while `make` output does not.
                    origin: LogOrigin::Stage,
                    line: "configuring".into(),
                },
            ]
        );
        // The duration is the one field a test cannot assert a value for; what it can
        // assert is that the step reports one, and that a step which restored nothing
        // claims to have done the work.
        assert!(matches!(
            &events[3],
            Event::StepFinished {
                step,
                outcome: StepOutcome::Built,
                ..
            } if step == "kernel"
        ));
        assert_eq!(events.len(), 4);
    }

    /// The outcome is what makes a duration readable, so each of the three states has
    /// to come out of the calls a stage actually makes: nothing said (the step did its
    /// own work), a restore alone (the artifact cache answered), and a restore beside a
    /// compile (the userspace stage, whose `.deb`s cache one at a time).
    #[test]
    fn the_outcome_reports_what_the_step_says_it_did() {
        let outcome_of = |mark: &dyn Fn(&Step)| {
            let log = RefCell::new(Vec::new());
            {
                let sink = recorder(&log);
                let step = Step::start(&sink, "userspace");
                mark(&step);
                step.finish();
            }
            match log.into_inner().pop().expect("a finish event") {
                Event::StepFinished { outcome, .. } => outcome,
                other => panic!("expected a finish event, got {other:?}"),
            }
        };
        assert_eq!(outcome_of(&|_| {}), StepOutcome::Built);
        assert_eq!(outcome_of(&|s: &Step| s.restored()), StepOutcome::Restored);
        assert_eq!(
            outcome_of(&|s: &Step| {
                s.restored();
                s.compiled();
            }),
            StepOutcome::Mixed
        );
        // A step that only compiled says the same thing as one that said nothing:
        // `compiled` exists to qualify a restore, not to make the default correct.
        assert_eq!(outcome_of(&|s: &Step| s.compiled()), StepOutcome::Built);
    }

    #[test]
    fn closure_is_a_sink() {
        let seen = RefCell::new(0u32);
        let sink = |_: Event| *seen.borrow_mut() += 1;
        sink.emit(Event::StepStarted { step: "x".into() });
        sink.emit(Event::StepFinished {
            step: "x".into(),
            duration_ms: 0,
            outcome: StepOutcome::Built,
        });
        assert_eq!(*seen.borrow(), 2);
    }

    #[test]
    fn event_roundtrips_through_json_shape() {
        // The enum is serializable so it can become a wire form later.
        let e = Event::Log {
            step: "uboot".into(),
            stream: Stream::Stderr,
            origin: LogOrigin::Subprocess,
            line: "  CC drivers/foo.o".into(),
        };
        let text = toml::to_string(&e).unwrap();
        let back: Event = toml::from_str(&text).unwrap();
        assert_eq!(e, back);
    }
}
