//! The model the dashboard draws from: run events folded into plain data.

use std::time::Duration;

use crate::report::{Event, Stage};

/// Progress of one pipeline stage.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageStatus {
    Pending,
    Running,
    Done,
}

/// Progress of one chunk.
#[derive(Debug, Clone, PartialEq)]
pub enum ChunkStatus {
    Pending,
    Running,
    /// `at`: time of the retry event since the dashboard started.
    Retry {
        attempt: u32,
        wait_s: f64,
        reason: String,
        at: Duration,
    },
    Done {
        words: usize,
        secs: f64,
        cached: bool,
    },
}

/// One chunk of the plan, with its span in the audio, in seconds.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkRow {
    pub start_s: f64,
    pub end_s: f64,
    pub status: ChunkStatus,
}

/// What the dashboard shows, updated one event at a time.
pub struct DashboardState {
    pub input_name: String,
    pub model: String,
    pub chunks: Vec<ChunkRow>,
    /// Cost reported by chunks transcribed in this run (cached ones excluded), like `Summary::cost_usd`.
    pub cost_usd: Option<f64>,
    stages: [StageStatus; Stage::ALL.len()],
    /// Time of `StageStarted(Transcribe)`, since the dashboard started.
    transcribe_started: Option<Duration>,
    /// Audio seconds of the chunks transcribed in this run (cached ones excluded).
    transcribed_audio_s: f64,
    warnings: Vec<String>,
    /// Set by `Event::Cancelling`: the run is stopping after a first Ctrl-C.
    pub cancelling: bool,
}

impl DashboardState {
    pub fn new(input_name: String, model: String) -> Self {
        Self {
            input_name,
            model,
            chunks: Vec::new(),
            cost_usd: None,
            stages: [StageStatus::Pending; Stage::ALL.len()],
            transcribe_started: None,
            transcribed_audio_s: 0.0,
            warnings: Vec::new(),
            cancelling: false,
        }
    }

    /// Folds one event; `now` is the time since the dashboard started.
    pub fn apply(&mut self, e: &Event, now: Duration) {
        match e {
            Event::StageStarted(stage) => {
                self.stages[stage_index(*stage)] = StageStatus::Running;
                if *stage == Stage::Transcribe {
                    self.transcribe_started = Some(now);
                }
            }
            Event::StageDone { stage, .. } => {
                self.stages[stage_index(*stage)] = StageStatus::Done;
            }
            Event::ChunksPlanned(plan) => {
                self.chunks = plan
                    .iter()
                    .map(|&(start_s, end_s)| ChunkRow {
                        start_s,
                        end_s,
                        status: ChunkStatus::Pending,
                    })
                    .collect();
            }
            Event::ChunkStarted(index) => {
                if let Some(row) = self.chunks.get_mut(*index) {
                    row.status = ChunkStatus::Running;
                }
            }
            Event::ChunkRetry {
                index,
                attempt,
                wait_s,
                reason,
            } => {
                if let Some(row) = self.chunks.get_mut(*index) {
                    row.status = ChunkStatus::Retry {
                        attempt: *attempt,
                        wait_s: *wait_s,
                        reason: reason.clone(),
                        at: now,
                    };
                }
            }
            Event::ChunkDone {
                index,
                words,
                secs,
                cost_usd,
                cached,
            } => {
                let Some(row) = self.chunks.get_mut(*index) else {
                    return;
                };
                row.status = ChunkStatus::Done {
                    words: *words,
                    secs: *secs,
                    cached: *cached,
                };
                if !cached {
                    self.transcribed_audio_s += row.end_s - row.start_s;
                    if let Some(cost) = cost_usd {
                        *self.cost_usd.get_or_insert(0.0) += cost;
                    }
                }
            }
            Event::Warning(message) => self.warnings.push(message.clone()),
            Event::Cancelling => self.cancelling = true,
        }
    }

    /// Status of one stage.
    pub fn stage(&self, stage: Stage) -> StageStatus {
        self.stages[stage_index(stage)]
    }

    /// The running stage, else the last finished one, else `None`.
    pub fn current_stage(&self) -> Option<Stage> {
        let find = |status| Stage::ALL.into_iter().rfind(|&s| self.stage(s) == status);
        find(StageStatus::Running).or_else(|| find(StageStatus::Done))
    }

    /// Number of chunks that are done, cached ones included.
    pub fn done_count(&self) -> usize {
        self.chunks
            .iter()
            .filter(|c| matches!(c.status, ChunkStatus::Done { .. }))
            .count()
    }

    /// Index of the first chunk that is not done yet.
    pub fn first_unfinished(&self) -> Option<usize> {
        self.chunks
            .iter()
            .position(|c| !matches!(c.status, ChunkStatus::Done { .. }))
    }

    /// Audio seconds transcribed in this run (cached chunks excluded) per second
    /// since `StageStarted(Transcribe)`; `None` before the first such chunk.
    pub fn speed(&self, now: Duration) -> Option<f64> {
        let elapsed = now.saturating_sub(self.transcribe_started?).as_secs_f64();
        (self.transcribed_audio_s > 0.0 && elapsed > 0.0)
            .then(|| self.transcribed_audio_s / elapsed)
    }

    /// Warnings received since the last call, in order.
    pub fn take_warnings(&mut self) -> Vec<String> {
        std::mem::take(&mut self.warnings)
    }
}

fn stage_index(stage: Stage) -> usize {
    Stage::ALL
        .iter()
        .position(|&s| s == stage)
        .expect("Stage::ALL lists every stage")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::report::{Event, Stage};
    use std::time::Duration;

    fn plan(n: usize) -> Event {
        Event::ChunksPlanned(
            (0..n)
                .map(|i| (i as f64 * 60.0, (i + 1) as f64 * 60.0))
                .collect(),
        )
    }

    fn done(index: usize, cost: f64, cached: bool) -> Event {
        Event::ChunkDone {
            index,
            words: 100,
            secs: 4.0,
            cost_usd: Some(cost),
            cached,
        }
    }

    const fn s(secs: u64) -> Duration {
        Duration::from_secs(secs)
    }

    #[test]
    fn folds_stages_and_chunks() {
        let mut st = DashboardState::new("talk.mkv".into(), "m".into());
        st.apply(&Event::StageStarted(Stage::Extract), s(0));
        assert_eq!(st.stage(Stage::Extract), StageStatus::Running);
        assert_eq!(st.current_stage(), Some(Stage::Extract));
        st.apply(
            &Event::StageDone {
                stage: Stage::Extract,
                detail: String::new(),
            },
            s(1),
        );
        assert_eq!(st.stage(Stage::Extract), StageStatus::Done);
        assert_eq!(st.stage(Stage::Split), StageStatus::Pending);
        st.apply(&plan(8), s(2));
        assert_eq!(st.chunks.len(), 8);
        assert_eq!(st.first_unfinished(), Some(0));
        st.apply(&Event::ChunkStarted(0), s(3));
        assert_eq!(st.chunks[0].status, ChunkStatus::Running);
        st.apply(
            &Event::ChunkRetry {
                index: 0,
                attempt: 1,
                wait_s: 2.0,
                reason: "HTTP 429".into(),
            },
            s(4),
        );
        assert!(
            matches!(st.chunks[0].status, ChunkStatus::Retry { attempt: 1, at, .. } if at == s(4))
        );
        st.apply(&done(0, 0.005, false), s(5));
        assert_eq!(st.done_count(), 1);
        assert_eq!(st.first_unfinished(), Some(1));
        st.apply(&Event::ChunkStarted(99), s(6)); // out of plan: ignored
    }

    #[test]
    fn cost_and_speed_ignore_cached_chunks() {
        let mut st = DashboardState::new("a".into(), "m".into());
        st.apply(&plan(8), s(0));
        st.apply(&Event::StageStarted(Stage::Transcribe), s(10));
        assert_eq!(st.speed(s(20)), None);
        for i in [0, 1, 2, 4] {
            st.apply(&done(i, 0.005, false), s(60));
        }
        st.apply(&done(5, 0.5, true), s(60));
        assert!((st.cost_usd.unwrap() - 0.02).abs() < 1e-9);
        assert!((st.speed(s(70)).unwrap() - 4.0).abs() < 1e-9); // 240 s of audio in 60 s
        assert_eq!(st.done_count(), 5);
    }

    #[test]
    fn warnings_are_taken_once_in_order() {
        let mut st = DashboardState::new("a".into(), "m".into());
        st.apply(&Event::Warning("one".into()), s(0));
        st.apply(&Event::Warning("two".into()), s(0));
        assert_eq!(
            st.take_warnings(),
            vec!["one".to_string(), "two".to_string()]
        );
        assert!(st.take_warnings().is_empty());
    }
}
