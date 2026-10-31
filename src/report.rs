//! Progress events emitted during a run, the reporter that displays them and the run summary.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Mutex;

use serde::Serialize;

/// A phase of a run.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stage {
    Extract,
    Split,
    Transcribe,
    Assemble,
    Write,
}

impl Stage {
    /// Every stage, in pipeline order.
    pub const ALL: [Stage; 5] = [
        Stage::Extract,
        Stage::Split,
        Stage::Transcribe,
        Stage::Assemble,
        Stage::Write,
    ];

    /// Capitalized name shown in the dashboard.
    pub fn label(&self) -> &'static str {
        match self {
            Stage::Extract => "Extract",
            Stage::Split => "Split",
            Stage::Transcribe => "Transcribe",
            Stage::Assemble => "Assemble",
            Stage::Write => "Write",
        }
    }

    /// Lowercase name used as a line prefix.
    pub fn name(&self) -> &'static str {
        match self {
            Stage::Extract => "extract",
            Stage::Split => "split",
            Stage::Transcribe => "transcribe",
            Stage::Assemble => "assemble",
            Stage::Write => "write",
        }
    }
}

/// Something that happened during a run. Chunk indexes start at 0.
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    StageStarted(Stage),
    StageDone {
        stage: Stage,
        detail: String,
    },
    /// Start and end of every chunk, in seconds.
    ChunksPlanned(Vec<(f64, f64)>),
    ChunkStarted(usize),
    ChunkRetry {
        index: usize,
        attempt: u32,
        wait_s: f64,
        reason: String,
    },
    ChunkDone {
        index: usize,
        words: usize,
        secs: f64,
        cost_usd: Option<f64>,
        cached: bool,
    },
    Warning(String),
}

/// Receives run events.
pub trait Reporter: Send + Sync {
    fn event(&self, e: &Event);
}

/// Writes one plain line per event, without colors or cursor control.
pub struct PlainReporter {
    state: Mutex<PlainState>,
}

struct PlainState {
    out: Box<dyn Write + Send>,
    chunks: usize,
}

impl PlainReporter {
    pub fn new(writer: Box<dyn Write + Send>) -> Self {
        Self {
            state: Mutex::new(PlainState {
                out: writer,
                chunks: 0,
            }),
        }
    }
}

impl Reporter for PlainReporter {
    fn event(&self, e: &Event) {
        let mut state = self.state.lock().unwrap_or_else(|p| p.into_inner());
        if let Event::ChunksPlanned(plan) = e {
            state.chunks = plan.len();
        }
        let line = plain_line(e, state.chunks);
        // Progress output is best effort: a closed stream must not fail the run.
        let _ = writeln!(state.out, "{line}");
        let _ = state.out.flush();
    }
}

fn plain_line(e: &Event, chunks: usize) -> String {
    let transcribe = Stage::Transcribe.name();
    match e {
        Event::StageStarted(stage) => format!("{}: started", stage.name()),
        Event::StageDone { stage, detail } => format!("{}: done ({detail})", stage.name()),
        Event::ChunksPlanned(plan) => {
            let s = if plan.len() == 1 { "" } else { "s" };
            format!("{}: {} chunk{s} planned", Stage::Split.name(), plan.len())
        }
        Event::ChunkStarted(index) => {
            format!("{transcribe}: chunk {}/{chunks} started", index + 1)
        }
        Event::ChunkRetry {
            index,
            attempt,
            wait_s,
            reason,
        } => format!(
            "{transcribe}: chunk {} retry {attempt} in {wait_s:.1} s ({reason})",
            index + 1
        ),
        Event::ChunkDone {
            index,
            words,
            cached: true,
            ..
        } => format!(
            "{transcribe}: chunk {}/{chunks} cached ({words} words)",
            index + 1
        ),
        Event::ChunkDone {
            index,
            words,
            secs,
            cost_usd,
            cached: false,
        } => {
            let cost = cost_usd.map(|c| format!(", ${c:.4}")).unwrap_or_default();
            format!(
                "{transcribe}: chunk {}/{chunks} done ({words} words, {secs:.1} s{cost})",
                index + 1
            )
        }
        Event::Warning(message) => format!("warning: {message}"),
    }
}

/// Outcome of a successful run.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Summary {
    pub files: Vec<PathBuf>,
    pub audio_duration_s: f64,
    pub provider: String,
    pub model: String,
    pub cost_usd: Option<f64>,
    pub cues: usize,
    pub chunks: usize,
    pub elapsed_s: f64,
    pub warnings: Vec<String>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[derive(Clone, Default)]
    struct Buf(Arc<Mutex<Vec<u8>>>);

    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    #[test]
    fn plain_reporter_writes_one_line_per_event() {
        let buf = Buf::default();
        let r = PlainReporter::new(Box::new(buf.clone()));
        let events = [
            Event::StageStarted(Stage::Extract),
            Event::StageDone {
                stage: Stage::Extract,
                detail: "27.0 s of audio".into(),
            },
            Event::ChunksPlanned((0..8).map(|i| (i as f64, i as f64 + 1.0)).collect()),
            Event::ChunkStarted(2),
            Event::ChunkRetry {
                index: 1,
                attempt: 1,
                wait_s: 2.0,
                reason: "HTTP 503".into(),
            },
            Event::ChunkDone {
                index: 2,
                words: 142,
                secs: 4.07,
                cost_usd: Some(0.0043),
                cached: false,
            },
            Event::ChunkDone {
                index: 3,
                words: 12,
                secs: 0.0,
                cost_usd: None,
                cached: true,
            },
            Event::Warning("no speech detected".into()),
        ];
        for e in &events {
            r.event(e);
        }
        let out = String::from_utf8(buf.0.lock().unwrap().clone()).unwrap();
        assert_eq!(
            out,
            "extract: started\n\
             extract: done (27.0 s of audio)\n\
             split: 8 chunks planned\n\
             transcribe: chunk 3/8 started\n\
             transcribe: chunk 2 retry 1 in 2.0 s (HTTP 503)\n\
             transcribe: chunk 3/8 done (142 words, 4.1 s, $0.0043)\n\
             transcribe: chunk 4/8 cached (12 words)\n\
             warning: no speech detected\n"
        );
        assert!(!out.contains('\x1b'));
    }

    #[test]
    fn stage_labels() {
        assert_eq!(
            Stage::ALL.map(|s| s.label()),
            ["Extract", "Split", "Transcribe", "Assemble", "Write"]
        );
    }
}
