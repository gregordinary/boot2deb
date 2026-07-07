//! The structured build event stream — the one contract the CLI consumes
//! today and the Dioxus UI will consume later.
//!
//! Today there is a single consumer, so the stream is delivered in-process: a
//! stage emits [`Event`]s to an [`EventSink`] (a callback or trait object)
//! rather than over a wire. The enum derives `Serialize`/`Deserialize` so it
//! *can* become a JSON wire form later without a redesign, but no transport is
//! built now — designing the enum cleanly is
//! free; serializing before there is a second consumer is waste.
//!
//! Every event carries the `step` it belongs to (a build-graph node such as
//! `kernel` or `uboot`), so a flat stream stays self-describing once
//! independent nodes emit concurrently.

use serde::{Deserialize, Serialize};

/// Which subprocess stream a [`Event::Log`] line came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Stream {
    /// Standard output.
    Stdout,
    /// Standard error (where `make`/`git` write progress and diagnostics).
    Stderr,
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
    /// One line of subprocess output.
    Log {
        /// The step that produced the line.
        step: String,
        /// Whether it came from stdout or stderr.
        stream: Stream,
        /// The line, with its trailing newline stripped.
        line: String,
    },
    /// A build step finished successfully.
    StepFinished {
        /// The step that finished.
        step: String,
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
}

impl<'a> Step<'a> {
    /// Begin a step, emitting [`Event::StepStarted`].
    pub fn start(sink: &'a dyn EventSink, name: impl Into<String>) -> Self {
        let name = name.into();
        sink.emit(Event::StepStarted { step: name.clone() });
        Step { sink, name }
    }

    /// Emit an informational [`Event::Log`] line (stdout-tagged) from the stage
    /// itself, as opposed to relayed subprocess output.
    pub fn log(&self, line: impl Into<String>) {
        self.emit(Stream::Stdout, line.into());
    }

    /// Emit a coarse [`Event::Progress`] update.
    pub fn progress(&self, pct: u8) {
        self.sink.emit(Event::Progress {
            step: self.name.clone(),
            pct,
        });
    }

    /// Emit [`Event::StepFinished`]. Consumes the handle so it cannot fire twice.
    pub fn finish(self) {
        self.sink.emit(Event::StepFinished {
            step: self.name.clone(),
        });
    }

    /// Relay one line of subprocess output on `stream`. Used by the streaming
    /// runner ([`run`](crate::build::run)).
    pub(crate) fn emit(&self, stream: Stream, line: String) {
        self.sink.emit(Event::Log {
            step: self.name.clone(),
            stream,
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
    fn step_emits_started_log_progress_finished_in_order() {
        let log = RefCell::new(Vec::new());
        let sink = recorder(&log);
        let step = Step::start(&sink, "kernel");
        step.progress(10);
        step.log("configuring");
        step.finish();

        let events = log.borrow();
        assert_eq!(
            *events,
            vec![
                Event::StepStarted { step: "kernel".into() },
                Event::Progress { step: "kernel".into(), pct: 10 },
                Event::Log {
                    step: "kernel".into(),
                    stream: Stream::Stdout,
                    line: "configuring".into(),
                },
                Event::StepFinished { step: "kernel".into() },
            ]
        );
    }

    #[test]
    fn closure_is_a_sink() {
        let seen = RefCell::new(0u32);
        let sink = |_: Event| *seen.borrow_mut() += 1;
        sink.emit(Event::StepStarted { step: "x".into() });
        sink.emit(Event::StepFinished { step: "x".into() });
        assert_eq!(*seen.borrow(), 2);
    }

    #[test]
    fn event_roundtrips_through_json_shape() {
        // The enum is serializable so it can become a wire form later.
        let e = Event::Log {
            step: "uboot".into(),
            stream: Stream::Stderr,
            line: "  CC drivers/foo.o".into(),
        };
        let text = toml::to_string(&e).unwrap();
        let back: Event = toml::from_str(&text).unwrap();
        assert_eq!(e, back);
    }
}
