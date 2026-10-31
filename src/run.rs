//! Run orchestration: extraction, chunking, transcription, assembly and output.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures::{StreamExt, TryStreamExt, stream};
use tokio_util::sync::CancellationToken;

use crate::assemble::words::words_from_segments;
use crate::assemble::{CueRules, assemble};
use crate::cache::{CacheParams, ChunkCache, sha256_file};
use crate::error::SrttyError;
use crate::media::{
    Silence, cut_chunk, detect_silences, ensure_ffmpeg, extract_audio, mean_volume_db, plan_chunks,
    probe_duration, silence_threshold_db,
};
use crate::output::{Format, output_paths, render, write_atomic};
use crate::report::{Event, Reporter, Stage, Summary};
use crate::transcribe::{
    AudioChunk, TranscribeError, TranscribeOptions, Transcriber, Transcript, Usability,
};

/// Silences this far from a chunk target, in seconds, can still be used as cut points.
pub(crate) const SILENCE_WINDOW_S: f64 = 30.0;
/// Attempts per chunk on transient failures, the first one included.
const MAX_ATTEMPTS: u32 = 4;
/// Retries per chunk, the first attempt excluded.
pub const MAX_RETRIES: u32 = MAX_ATTEMPTS - 1;
/// Longest wait between two attempts, in seconds.
const MAX_BACKOFF_S: f64 = 30.0;

/// The extracted audio of an input and what is known about it.
pub struct Prepared {
    /// Holds the extracted audio and the chunk files; removed on drop.
    _tmp: tempfile::TempDir,
    pub input: PathBuf,
    pub input_sha256: String,
    pub audio: PathBuf,
    pub audio_bytes: u64,
    pub duration_s: f64,
    pub silences: Vec<Silence>,
}

/// Everything a run needs besides the prepared audio and the transcriber.
pub struct RunRequest {
    pub formats: Vec<Format>,
    pub output: Option<PathBuf>,
    /// Replaces the silence-based plan, e.g. one chunk for the whole audio.
    pub chunks_override: Option<Vec<(f64, f64)>>,
    pub target_s: f64,
    pub jobs: usize,
    pub opts: TranscribeOptions,
    pub rules: CueRules,
    pub speaker_label: String,
    pub cache: ChunkCache,
    pub cache_params: CacheParams,
    pub provider: String,
}

/// Extracts the audio of `input` into a temporary directory, then measures its
/// duration and finds its silences.
pub async fn prepare(input: &Path, reporter: &dyn Reporter) -> Result<Prepared, SrttyError> {
    if !input.exists() {
        return Err(SrttyError::Usage(format!(
            "input file not found: {}",
            input.display()
        )));
    }
    if !input.is_file() {
        return Err(SrttyError::Usage(format!(
            "input is not a file: {}",
            input.display()
        )));
    }
    ensure_ffmpeg()?;

    reporter.event(&Event::StageStarted(Stage::Extract));
    let input_sha256 = {
        let input = input.to_path_buf();
        tokio::task::spawn_blocking(move || sha256_file(&input))
            .await
            .map_err(std::io::Error::from)??
    };
    let tmp = tempfile::Builder::new().prefix("srtty-").tempdir()?;
    let audio = tmp.path().join("audio.ogg");
    extract_audio(input, &audio).await?;
    let audio_bytes = tokio::fs::metadata(&audio).await?.len();
    let duration_s = probe_duration(&audio).await?;
    reporter.event(&Event::StageDone {
        stage: Stage::Extract,
        detail: format!(
            "{duration_s:.1} s of audio, {:.1} MB",
            audio_bytes as f64 / 1_000_000.0
        ),
    });

    reporter.event(&Event::StageStarted(Stage::Split));
    let mean_db = mean_volume_db(&audio).await?;
    let silences = detect_silences(&audio, silence_threshold_db(mean_db)).await?;
    reporter.event(&Event::StageDone {
        stage: Stage::Split,
        detail: format!("{} silences detected", silences.len()),
    });

    Ok(Prepared {
        _tmp: tmp,
        input: input.to_path_buf(),
        input_sha256,
        audio,
        audio_bytes,
        duration_s,
        silences,
    })
}

/// Transcribes the prepared audio chunk by chunk, assembles the cues and writes
/// every requested format. Nothing is written if the run fails or is cancelled.
pub async fn run(
    prep: Prepared,
    req: RunRequest,
    transcriber: Arc<dyn Transcriber>,
    reporter: Arc<dyn Reporter>,
    cancel: CancellationToken,
) -> Result<Summary, SrttyError> {
    let started = Instant::now();
    let plan = match &req.chunks_override {
        Some(plan) => plan.clone(),
        None => plan_chunks(
            prep.duration_s,
            req.target_s,
            &prep.silences,
            SILENCE_WINDOW_S,
        ),
    };
    reporter.event(&Event::ChunksPlanned(plan.clone()));
    let chunks: Vec<AudioChunk> = plan
        .iter()
        .enumerate()
        .map(|(index, &(start_s, end_s))| AudioChunk {
            index,
            start_s,
            end_s,
            path: prep._tmp.path().join(format!("chunk-{index:03}.ogg")),
        })
        .collect();

    let ctx = ChunkContext {
        audio: &prep.audio,
        transcriber: transcriber.as_ref(),
        opts: &req.opts,
        cache: &req.cache,
        key: ChunkCache::run_key(&prep.input_sha256, &req.cache_params),
        reporter: reporter.as_ref(),
        warnings: Mutex::new(Vec::new()),
    };
    reporter.event(&Event::StageStarted(Stage::Transcribe));
    // Dropping the transcription future on cancel aborts the requests in flight.
    let outcomes = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(SrttyError::Interrupted),
        result = transcribe_all(&ctx, &chunks, req.jobs.max(1)) => result?,
    };
    // What this run spent: cached chunks were paid for by an earlier run.
    let cost_usd = outcomes
        .iter()
        .filter(|o| !o.cached)
        .filter_map(|o| o.transcript.cost_usd)
        .reduce(|a, b| a + b);
    reporter.event(&Event::StageDone {
        stage: Stage::Transcribe,
        detail: format!(
            "{} chunks{}",
            chunks.len(),
            cost_usd.map(|c| format!(", ${c:.4}")).unwrap_or_default()
        ),
    });
    if outcomes
        .iter()
        .all(|o| o.transcript.usability() == Usability::NoSpeech)
    {
        ctx.warn("no speech detected".into());
    }
    let mut transcripts: Vec<Transcript> = outcomes.into_iter().map(|o| o.transcript).collect();
    for (chunk, transcript) in chunks.iter().zip(&mut transcripts) {
        if let Some(warning) = approximate_unusable(chunk, transcript) {
            ctx.warn(warning);
        }
    }
    let warnings = ctx.warnings.into_inner().unwrap_or_else(|p| p.into_inner());

    reporter.event(&Event::StageStarted(Stage::Assemble));
    let timed: Vec<(f64, Transcript)> = chunks.iter().map(|c| c.start_s).zip(transcripts).collect();
    let cues = assemble(&timed, &req.rules);
    reporter.event(&Event::StageDone {
        stage: Stage::Assemble,
        detail: format!("{} cues", cues.len()),
    });

    if cancel.is_cancelled() {
        return Err(SrttyError::Interrupted);
    }
    reporter.event(&Event::StageStarted(Stage::Write));
    let rendered: Vec<(PathBuf, String)> =
        output_paths(&prep.input, req.output.as_deref(), &req.formats)
            .into_iter()
            .map(|(format, path)| (path, render(format, &cues, &req.speaker_label)))
            .collect();
    let files = write_outputs(rendered)?;
    reporter.event(&Event::StageDone {
        stage: Stage::Write,
        detail: files
            .iter()
            .map(|f| f.display().to_string())
            .collect::<Vec<_>>()
            .join(", "),
    });

    Ok(Summary {
        files,
        audio_duration_s: prep.duration_s,
        provider: req.provider,
        model: transcriber.model().to_string(),
        cost_usd,
        cues: cues.len(),
        chunks: chunks.len(),
        elapsed_s: started.elapsed().as_secs_f64(),
        warnings,
    })
}

/// Writes every output atomically. If one write fails, the files already
/// written are removed, so a failed run leaves no output behind.
fn write_outputs(outputs: Vec<(PathBuf, String)>) -> std::io::Result<Vec<PathBuf>> {
    let mut written: Vec<PathBuf> = Vec::with_capacity(outputs.len());
    for (path, content) in outputs {
        if let Err(e) = write_atomic(&path, &content) {
            for file in &written {
                let _ = std::fs::remove_file(file);
            }
            return Err(e);
        }
        written.push(path);
    }
    Ok(written)
}

/// A chunk's transcript and whether it came from the cache.
struct ChunkOutcome {
    transcript: Transcript,
    cached: bool,
}

/// What every chunk transcription shares within a run.
struct ChunkContext<'a> {
    audio: &'a Path,
    transcriber: &'a dyn Transcriber,
    opts: &'a TranscribeOptions,
    cache: &'a ChunkCache,
    key: String,
    reporter: &'a dyn Reporter,
    warnings: Mutex<Vec<String>>,
}

impl ChunkContext<'_> {
    fn warn(&self, message: String) {
        self.reporter.event(&Event::Warning(message.clone()));
        self.warnings
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(message);
    }
}

/// Transcribes the chunks one by one until one proves the model returns usable
/// timings, then the remaining ones `jobs` at a time. Returns transcripts in chunk order.
async fn transcribe_all(
    ctx: &ChunkContext<'_>,
    chunks: &[AudioChunk],
    jobs: usize,
) -> Result<Vec<ChunkOutcome>, SrttyError> {
    let mut outcomes = Vec::with_capacity(chunks.len());
    for chunk in chunks {
        let outcome = transcribe_chunk(ctx, chunk).await?;
        let usability = outcome.transcript.usability();
        outcomes.push(outcome);
        match usability {
            Usability::Words | Usability::FineSegments => break,
            Usability::NoSpeech => {}
            Usability::Unusable => {
                return Err(SrttyError::UnusableModel {
                    model: ctx.transcriber.model().to_string(),
                });
            }
        }
    }

    let mut rest: Vec<(usize, ChunkOutcome)> = stream::iter(&chunks[outcomes.len()..])
        .map(|chunk| async move {
            Ok::<_, SrttyError>((chunk.index, transcribe_chunk(ctx, chunk).await?))
        })
        .buffer_unordered(jobs)
        .try_collect()
        .await?;
    rest.sort_by_key(|(index, _)| *index);
    outcomes.extend(rest.into_iter().map(|(_, o)| o));
    Ok(outcomes)
}

/// Returns the cached transcript of `chunk`, or cuts and transcribes it and caches
/// the result. Unusable transcripts are not cached, so a later run retries them.
async fn transcribe_chunk(
    ctx: &ChunkContext<'_>,
    chunk: &AudioChunk,
) -> Result<ChunkOutcome, SrttyError> {
    if let Some(transcript) = ctx.cache.get(&ctx.key, chunk) {
        ctx.reporter.event(&Event::ChunkDone {
            index: chunk.index,
            words: word_count(&transcript),
            secs: 0.0,
            cost_usd: transcript.cost_usd,
            cached: true,
        });
        return Ok(ChunkOutcome {
            transcript,
            cached: true,
        });
    }
    ctx.reporter.event(&Event::ChunkStarted(chunk.index));
    cut_chunk(ctx.audio, chunk.start_s, chunk.end_s, &chunk.path).await?;
    let started = Instant::now();
    let transcript = transcribe_with_retry(ctx.transcriber, chunk, ctx.opts, ctx.reporter).await?;
    let secs = started.elapsed().as_secs_f64();
    if transcript.usability() != Usability::Unusable
        && let Err(e) = ctx.cache.put(&ctx.key, chunk, &transcript)
    {
        ctx.warn(format!("could not cache chunk {}: {e}", chunk.index + 1));
    }
    ctx.reporter.event(&Event::ChunkDone {
        index: chunk.index,
        words: word_count(&transcript),
        secs,
        cost_usd: transcript.cost_usd,
        cached: false,
    });
    Ok(ChunkOutcome {
        transcript,
        cached: false,
    })
}

/// Keeps the text of an `Unusable` transcript by spreading its words over its
/// segments. Returns the warning to show, or `None` for any other transcript.
fn approximate_unusable(chunk: &AudioChunk, t: &mut Transcript) -> Option<String> {
    if t.usability() != Usability::Unusable {
        return None;
    }
    let span = format!(
        "chunk {} ({}-{})",
        chunk.index + 1,
        mm_ss(chunk.start_s),
        mm_ss(chunk.end_s)
    );
    let words = words_from_segments(&t.segments);
    if words.is_empty() {
        return Some(format!("{span}: no word timestamps, text omitted"));
    }
    t.words = words;
    Some(format!(
        "{span}: no word timestamps, timings approximated from segments"
    ))
}

pub(crate) fn mm_ss(s: f64) -> String {
    let secs = s.max(0.0) as u64;
    format!("{:02}:{:02}", secs / 60, secs % 60)
}

fn word_count(t: &Transcript) -> usize {
    if t.words.is_empty() {
        t.text.split_whitespace().count()
    } else {
        t.words.len()
    }
}

/// Transcribes `chunk`, retrying transient failures up to 4 attempts in total.
pub async fn transcribe_with_retry(
    t: &dyn Transcriber,
    chunk: &AudioChunk,
    opts: &TranscribeOptions,
    reporter: &dyn Reporter,
) -> Result<Transcript, TranscribeError> {
    let mut attempt = 1;
    loop {
        match t.transcribe(chunk, opts).await {
            Err(TranscribeError::Retryable {
                status,
                message,
                retry_after_s,
            }) if attempt < MAX_ATTEMPTS => {
                let wait_s = backoff_s(attempt, retry_after_s);
                reporter.event(&Event::ChunkRetry {
                    index: chunk.index,
                    attempt,
                    wait_s,
                    reason: status.map_or(message, |s| format!("HTTP {s}")),
                });
                tokio::time::sleep(Duration::from_secs_f64(wait_s)).await;
                attempt += 1;
            }
            other => return other,
        }
    }
}

/// Wait before the next attempt: the server's hint, else 1, 2, 4 s, at most 30 s.
fn backoff_s(attempt: u32, retry_after_s: Option<f64>) -> f64 {
    retry_after_s
        .filter(|s| !s.is_nan())
        .unwrap_or_else(|| 2f64.powi(attempt as i32 - 1))
        .clamp(0.0, MAX_BACKOFF_S)
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use std::collections::{HashMap, VecDeque};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::media::tone_with_silences;
    use crate::test_support::fx;
    use crate::transcribe::Word;
    use crate::transcribe::parse::parse_verbose_json;

    type Reply = Result<Transcript, TranscribeError>;

    /// Scripted transcriber: replies per chunk index, counts calls, can hang after the first call.
    #[derive(Default)]
    struct Fake {
        replies: Mutex<HashMap<usize, VecDeque<Reply>>>,
        calls: AtomicUsize,
        block_after_first: bool,
    }

    impl Fake {
        fn new(replies: Vec<(usize, Vec<Reply>)>) -> Self {
            Self {
                replies: Mutex::new(
                    replies
                        .into_iter()
                        .map(|(i, r)| (i, r.into_iter().collect()))
                        .collect(),
                ),
                ..Default::default()
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl Transcriber for Fake {
        async fn transcribe(&self, chunk: &AudioChunk, _opts: &TranscribeOptions) -> Reply {
            let previous = self.calls.fetch_add(1, Ordering::SeqCst);
            if self.block_after_first && previous > 0 {
                std::future::pending::<()>().await;
            }
            let reply = self
                .replies
                .lock()
                .unwrap()
                .get_mut(&chunk.index)
                .and_then(VecDeque::pop_front);
            reply.unwrap_or_else(|| {
                Err(TranscribeError::Fatal {
                    status: None,
                    message: format!("no scripted reply for chunk {}", chunk.index),
                })
            })
        }

        fn model(&self) -> &str {
            "fake-model"
        }
    }

    /// Records every event; can cancel the run once chunk 0 is done.
    #[derive(Default)]
    struct Recorder {
        events: Mutex<Vec<Event>>,
        cancel_after_chunk_0: Option<CancellationToken>,
    }

    impl Reporter for Recorder {
        fn event(&self, e: &Event) {
            self.events.lock().unwrap().push(e.clone());
            if let (Some(token), Event::ChunkDone { index: 0, .. }) =
                (&self.cancel_after_chunk_0, e)
            {
                token.cancel();
            }
        }
    }

    impl Recorder {
        fn events(&self) -> Vec<Event> {
            self.events.lock().unwrap().clone()
        }
    }

    fn words_transcript(text: &str) -> Transcript {
        let words = text
            .split_whitespace()
            .enumerate()
            .map(|(i, w)| Word {
                start: i as f64 * 0.3,
                end: (i + 1) as f64 * 0.3,
                text: w.into(),
                speaker: None,
            })
            .collect();
        Transcript {
            text: text.into(),
            words,
            ..Default::default()
        }
    }

    fn retryable() -> Reply {
        Err(TranscribeError::Retryable {
            status: Some(503),
            message: "service unavailable".into(),
            retry_after_s: Some(0.01),
        })
    }

    fn params() -> CacheParams {
        CacheParams {
            base_url: "http://fake".into(),
            model: "fake-model".into(),
            language: None,
            prompt: None,
            provider_options: None,
        }
    }

    /// 27 s of generated audio (8 s tone + 1 s silence, three times) and a cache directory.
    struct Env {
        dir: tempfile::TempDir,
        input: PathBuf,
    }

    impl Env {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let input = tone_with_silences(dir.path(), 27);
            Self { dir, input }
        }

        fn cache(&self) -> ChunkCache {
            ChunkCache::new(&self.dir.path().join("cache"), true)
        }

        fn request(&self, formats: Vec<Format>) -> RunRequest {
            RunRequest {
                formats,
                output: None,
                chunks_override: None,
                target_s: 9.0,
                jobs: 2,
                opts: TranscribeOptions::default(),
                rules: CueRules {
                    max_chars_per_line: 42,
                    max_lines: 2,
                    max_duration_s: 7.0,
                    min_duration_s: 1.0,
                    pause_break_s: 0.8,
                    first_line_reserve: 0,
                },
                speaker_label: "Speaker".into(),
                cache: self.cache(),
                cache_params: params(),
                provider: "fake".into(),
            }
        }

        fn out(&self, ext: &str) -> PathBuf {
            self.input.with_extension(ext)
        }

        async fn run(
            &self,
            req: RunRequest,
            fake: &Arc<Fake>,
            rec: &Arc<Recorder>,
            cancel: CancellationToken,
        ) -> Result<Summary, SrttyError> {
            let prep = prepare(&self.input, &**rec).await.unwrap();
            let fut = run(prep, req, fake.clone(), rec.clone(), cancel);
            tokio::time::timeout(Duration::from_secs(60), fut)
                .await
                .expect("run did not finish")
        }
    }

    /// Chunk `index` as planned in the recorded `ChunksPlanned` event.
    fn planned_chunk(rec: &Recorder, index: usize) -> AudioChunk {
        let (start_s, end_s) = rec
            .events()
            .into_iter()
            .find_map(|e| match e {
                Event::ChunksPlanned(plan) => Some(plan[index]),
                _ => None,
            })
            .unwrap();
        AudioChunk {
            index,
            start_s,
            end_s,
            path: "unused".into(),
        }
    }

    fn three_chunks(make: impl Fn(usize) -> Reply) -> Vec<(usize, Vec<Reply>)> {
        (0..3).map(|i| (i, vec![make(i)])).collect()
    }

    #[tokio::test]
    async fn happy_path_writes_srt_and_uses_cache_on_rerun() {
        let env = Env::new();
        let fake = Arc::new(Fake::new(three_chunks(|_| {
            Ok(Transcript {
                cost_usd: Some(0.01),
                ..words_transcript("Bonjour.")
            })
        })));
        let rec = Arc::new(Recorder::default());
        let summary = env
            .run(
                env.request(vec![Format::Srt]),
                &fake,
                &rec,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(summary.chunks, 3);
        assert!((summary.cost_usd.unwrap() - 0.03).abs() < 1e-9);
        assert_eq!(summary.files, vec![env.out("srt")]);
        assert_eq!(summary.model, "fake-model");
        assert_eq!(summary.provider, "fake");
        assert!(summary.warnings.is_empty(), "{:?}", summary.warnings);
        assert!(
            std::fs::read_to_string(env.out("srt"))
                .unwrap()
                .contains("Bonjour.")
        );

        let empty = Arc::new(Fake::default());
        let rec = Arc::new(Recorder::default());
        let again = env
            .run(
                env.request(vec![Format::Srt]),
                &empty,
                &rec,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(empty.calls(), 0);
        assert_eq!(again.chunks, 3);
        assert_eq!(again.cost_usd, None, "cached chunks cost nothing this run");
        let cached = rec
            .events()
            .iter()
            .filter(|e| matches!(e, Event::ChunkDone { cached: true, .. }))
            .count();
        assert_eq!(cached, 3);
    }

    #[tokio::test]
    async fn unusable_first_chunk_stops_before_other_chunks() {
        let env = Env::new();
        let unusable = parse_verbose_json(&fx("assemblyai")).unwrap();
        let fake = Arc::new(Fake::new(vec![(0, vec![Ok(unusable)])]));
        let rec = Arc::new(Recorder::default());
        let err = env
            .run(
                env.request(vec![Format::Srt]),
                &fake,
                &rec,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(&err, SrttyError::UnusableModel { model } if model == "fake-model"),
            "{err:?}"
        );
        assert_eq!(fake.calls(), 1);
        assert!(!env.out("srt").exists());
    }

    #[tokio::test]
    async fn first_chunk_without_speech_is_inconclusive() {
        let env = Env::new();
        let fake = Arc::new(Fake::new(vec![
            (0, vec![Ok(Transcript::default())]),
            (1, vec![Ok(words_transcript("Salut tout le monde."))]),
            (2, vec![Ok(words_transcript("Au revoir."))]),
        ]));
        let rec = Arc::new(Recorder::default());
        let mut req = env.request(vec![Format::Srt]);
        req.jobs = 0; // must behave like 1, not hang
        env.run(req, &fake, &rec, CancellationToken::new())
            .await
            .unwrap();
        let srt = std::fs::read_to_string(env.out("srt")).unwrap();
        assert!(srt.contains("Salut tout le monde."), "{srt}");
        assert!(srt.contains("Au revoir."), "{srt}");
        assert_eq!(fake.calls(), 3);
    }

    #[tokio::test]
    async fn run_with_no_speech_writes_empty_files() {
        let env = Env::new();
        let fake = Arc::new(Fake::new(three_chunks(|_| Ok(Transcript::default()))));
        let rec = Arc::new(Recorder::default());
        let summary = env
            .run(
                env.request(vec![Format::Srt, Format::Vtt]),
                &fake,
                &rec,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        assert_eq!(std::fs::read_to_string(env.out("srt")).unwrap(), "");
        assert_eq!(std::fs::read_to_string(env.out("vtt")).unwrap(), "WEBVTT\n");
        assert_eq!(summary.cues, 0);
        assert!(summary.warnings.iter().any(|w| w == "no speech detected"));
        assert!(
            rec.events()
                .contains(&Event::Warning("no speech detected".into()))
        );
    }

    #[tokio::test]
    async fn retries_then_succeeds_and_reports() {
        let env = Env::new();
        let fake = Arc::new(Fake::new(vec![
            (
                0,
                vec![retryable(), retryable(), Ok(words_transcript("Bonjour."))],
            ),
            (1, vec![Ok(words_transcript("Bonjour."))]),
            (2, vec![Ok(words_transcript("Bonjour."))]),
        ]));
        let rec = Arc::new(Recorder::default());
        env.run(
            env.request(vec![Format::Srt]),
            &fake,
            &rec,
            CancellationToken::new(),
        )
        .await
        .unwrap();
        let retries: Vec<(usize, u32, f64)> = rec
            .events()
            .into_iter()
            .filter_map(|e| match e {
                Event::ChunkRetry {
                    index,
                    attempt,
                    wait_s,
                    ..
                } => Some((index, attempt, wait_s)),
                _ => None,
            })
            .collect();
        assert_eq!(retries, vec![(0, 1, 0.01), (0, 2, 0.01)]);
        assert_eq!(fake.calls(), 5);
    }

    #[tokio::test]
    async fn exhausted_retries_fail_with_code_2() {
        let env = Env::new();
        let fake = Arc::new(Fake::new(vec![(0, (0..4).map(|_| retryable()).collect())]));
        let rec = Arc::new(Recorder::default());
        let err = env
            .run(
                env.request(vec![Format::Srt]),
                &fake,
                &rec,
                CancellationToken::new(),
            )
            .await
            .unwrap_err();
        assert!(
            matches!(
                err,
                SrttyError::Transcribe(TranscribeError::Retryable { .. })
            ),
            "{err:?}"
        );
        assert_eq!(err.exit_code(), 2);
        assert_eq!(fake.calls(), 4);
        assert!(!env.out("srt").exists());
    }

    #[tokio::test]
    async fn cancel_mid_run_leaves_no_output_and_keeps_cache() {
        let env = Env::new();
        let fake = Arc::new(Fake {
            block_after_first: true,
            ..Fake::new(vec![(0, vec![Ok(words_transcript("Bonjour."))])])
        });
        let cancel = CancellationToken::new();
        let rec = Arc::new(Recorder {
            cancel_after_chunk_0: Some(cancel.clone()),
            ..Default::default()
        });
        let err = env
            .run(env.request(vec![Format::Srt]), &fake, &rec, cancel)
            .await
            .unwrap_err();
        assert!(matches!(err, SrttyError::Interrupted), "{err:?}");
        assert!(!env.out("srt").exists());

        let chunk0 = planned_chunk(&rec, 0);
        let key = ChunkCache::run_key(&sha256_file(&env.input).unwrap(), &params());
        assert!(env.cache().get(&key, &chunk0).is_some());
    }

    #[test]
    fn backoff_doubles_from_one_second_and_is_capped() {
        assert_eq!(backoff_s(1, None), 1.0);
        assert_eq!(backoff_s(2, None), 2.0);
        assert_eq!(backoff_s(3, None), 4.0);
        assert_eq!(backoff_s(1, Some(0.5)), 0.5);
        assert_eq!(backoff_s(1, Some(120.0)), MAX_BACKOFF_S);
        assert_eq!(backoff_s(1, Some(-3.0)), 0.0);
        assert_eq!(backoff_s(2, Some(f64::NAN)), 2.0);
    }

    #[tokio::test]
    async fn unusable_chunk_after_probe_keeps_its_text_with_warning() {
        let env = Env::new();
        let unusable = parse_verbose_json(&fx("assemblyai")).unwrap();
        let fake = Arc::new(Fake::new(vec![
            (0, vec![Ok(words_transcript("Bonjour."))]),
            (1, vec![Ok(unusable)]),
            (2, vec![Ok(words_transcript("Au revoir."))]),
        ]));
        let rec = Arc::new(Recorder::default());
        let summary = env
            .run(
                env.request(vec![Format::Srt]),
                &fake,
                &rec,
                CancellationToken::new(),
            )
            .await
            .unwrap();
        let srt = std::fs::read_to_string(env.out("srt")).unwrap();
        assert!(srt.contains("Barbe-Bleue"), "{srt}");
        let chunk1 = planned_chunk(&rec, 1);
        let warning = summary
            .warnings
            .iter()
            .find(|w| w.starts_with("chunk 2 ("))
            .expect("warning for chunk 2");
        assert!(
            warning.ends_with("): no word timestamps, timings approximated from segments"),
            "{warning}"
        );
        assert!(rec.events().contains(&Event::Warning(warning.clone())));
        let key = ChunkCache::run_key(&sha256_file(&env.input).unwrap(), &params());
        assert!(
            env.cache().get(&key, &chunk1).is_none(),
            "an unusable transcript must not be cached"
        );
        assert!(env.cache().get(&key, &planned_chunk(&rec, 0)).is_some());
    }

    #[test]
    fn unusable_transcript_without_segments_has_its_text_omitted() {
        let chunk = AudioChunk {
            index: 4,
            start_s: 65.0,
            end_s: 130.5,
            path: "unused".into(),
        };
        let mut t = Transcript {
            text: "Some text without any timing.".into(),
            ..Default::default()
        };
        assert_eq!(
            approximate_unusable(&chunk, &mut t),
            Some("chunk 5 (01:05-02:10): no word timestamps, text omitted".to_string())
        );
        assert!(t.words.is_empty());

        let mut usable = words_transcript("Bonjour.");
        assert_eq!(approximate_unusable(&chunk, &mut usable), None);
    }

    #[tokio::test]
    async fn failed_write_removes_outputs_of_this_run() {
        let env = Env::new();
        let fake = Arc::new(Fake::new(three_chunks(|_| {
            Ok(words_transcript("Bonjour."))
        })));
        let rec = Arc::new(Recorder::default());
        let base = env.dir.path().join("out");
        // `out.vtt` is a directory, so writing the second format fails.
        std::fs::create_dir(env.dir.path().join("out.vtt")).unwrap();
        let mut req = env.request(vec![Format::Srt, Format::Vtt]);
        req.output = Some(base);
        let err = env
            .run(req, &fake, &rec, CancellationToken::new())
            .await
            .unwrap_err();
        assert!(matches!(err, SrttyError::Io(_)), "{err:?}");
        assert!(!env.dir.path().join("out.srt").exists());
    }

    #[tokio::test]
    async fn directory_input_is_usage_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = prepare(dir.path(), &Recorder::default())
            .await
            .err()
            .unwrap();
        assert!(matches!(err, SrttyError::Usage(_)), "{err:?}");
        assert_eq!(err.exit_code(), 1);
    }

    #[tokio::test]
    async fn missing_input_is_usage_error() {
        let dir = tempfile::tempdir().unwrap();
        let err = prepare(&dir.path().join("nope.mkv"), &Recorder::default())
            .await
            .err()
            .unwrap();
        assert!(matches!(err, SrttyError::Usage(_)), "{err:?}");
        assert_eq!(err.exit_code(), 1);
    }
}
