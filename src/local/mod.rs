//! Local transcription with whisper.cpp, compiled only with the `local` feature.
//!
//! One `WhisperContext` (the model weights) is loaded once and shared; each
//! chunk gets its own `WhisperState` on a blocking thread, so chunks can run in
//! parallel. Dropping a `transcribe` future raises an abort flag that
//! whisper.cpp polls during the encoder and decoder passes.

pub mod catalog;
pub mod download;
pub mod words;

use std::ffi::{c_int, c_void};
use std::io::Read;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use whisper_rs::{
    FullParams, SamplingStrategy, WhisperContext, WhisperContextParameters, WhisperError,
};

use crate::error::SrttyError;
use crate::media;
use crate::transcribe::{
    AudioChunk, Segment, TranscribeError, TranscribeOptions, Transcriber, Transcript,
};
use words::{TokenPiece, words_from_tokens};

/// First bytes of a ggml whisper model (`0x67676d6c` in little-endian).
const GGML_MAGIC: [u8; 4] = [0x6c, 0x6d, 0x67, 0x67];
/// whisper.cpp works on 16 kHz mono samples.
const SAMPLE_RATE: usize = 16_000;
/// Chunks shorter than this (0.1 s) give an empty transcript.
const MIN_SAMPLES: usize = SAMPLE_RATE / 10;
/// OpenAI Whisper's rule: a segment is dropped when it is probably silence…
const NO_SPEECH_THRESHOLD: f32 = 0.6;
/// …and its tokens are unlikely.
const LOGPROB_THRESHOLD: f32 = -1.0;

/// Checks that `path` exists and starts with the ggml magic bytes.
pub fn check_ggml_magic(path: &Path) -> Result<(), SrttyError> {
    let mut file = std::fs::File::open(path).map_err(|e| {
        if e.kind() == std::io::ErrorKind::NotFound {
            SrttyError::Usage(format!(
                "model file not found: {}; run `srtty setup` to download one",
                path.display()
            ))
        } else {
            SrttyError::Io(e)
        }
    })?;
    let mut magic = [0u8; 4];
    match file.read_exact(&mut magic) {
        Ok(()) if magic == GGML_MAGIC => Ok(()),
        Ok(()) => Err(not_ggml(path)),
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Err(not_ggml(path)),
        Err(e) => Err(SrttyError::Io(e)),
    }
}

fn not_ggml(path: &Path) -> SrttyError {
    SrttyError::Config(format!("{} is not a ggml whisper model", path.display()))
}

/// Checks that whisper.cpp knows the language code `code`. whisper.cpp also
/// accepts full names (`french`), which are refused: only its short codes pass.
pub fn check_language(code: &str) -> Result<(), SrttyError> {
    quiet_whisper();
    let short = whisper_rs::get_lang_id(code).and_then(whisper_rs::get_lang_str);
    match short {
        Some(short) if short == code => Ok(()),
        _ => Err(SrttyError::Usage(format!(
            "unknown language `{code}` for the local backend; use an ISO 639-1 code such as fr or en"
        ))),
    }
}

/// Sends whisper.cpp and ggml logs to the `log` crate, which has no backend
/// here, so they never reach stderr. Only the first call has an effect.
fn quiet_whisper() {
    whisper_rs::install_logging_hooks();
}

/// Keeps a segment unless it is probably silence (`no_speech_prob > 0.6`) and
/// its tokens are unlikely (mean log probability below -1), as OpenAI Whisper
/// does against hallucinations.
pub fn keep_segment(no_speech_prob: f32, mean_plog: f32) -> bool {
    !(no_speech_prob > NO_SPEECH_THRESHOLD && mean_plog < LOGPROB_THRESHOLD)
}

/// The local whisper.cpp backend.
pub struct LocalWhisper {
    ctx: Arc<WhisperContext>,
    name: String,
    threads: usize,
    /// Language detected on the first chunk with words, given to the next ones.
    pinned_language: OnceLock<String>,
    /// Number of `state.full` calls in progress.
    running: Arc<AtomicUsize>,
}

impl LocalWhisper {
    /// Loads the model at `path` on a blocking thread. Each chunk then uses
    /// `threads` CPU threads.
    pub async fn load(path: &Path, threads: usize) -> Result<LocalWhisper, SrttyError> {
        check_ggml_magic(path)?;
        quiet_whisper();
        let cannot_load = |e: &dyn std::fmt::Display| {
            SrttyError::Config(format!(
                "cannot load the whisper model {}: {e}",
                path.display()
            ))
        };
        let owned = path.to_path_buf();
        let ctx = tokio::task::spawn_blocking(move || {
            WhisperContext::new_with_params(&owned, WhisperContextParameters::default())
        })
        .await
        .map_err(|e| cannot_load(&e))?
        .map_err(|e| cannot_load(&e))?;
        Ok(LocalWhisper {
            ctx: Arc::new(ctx),
            name: path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into(),
            ),
            threads,
            pinned_language: OnceLock::new(),
            running: Arc::new(AtomicUsize::new(0)),
        })
    }

    #[cfg(test)]
    fn running(&self) -> usize {
        self.running.load(Ordering::SeqCst)
    }

    #[cfg(test)]
    fn pinned_language(&self) -> Option<String> {
        self.pinned_language.get().cloned()
    }
}

#[async_trait]
impl Transcriber for LocalWhisper {
    async fn transcribe(
        &self,
        chunk: &AudioChunk,
        opts: &TranscribeOptions,
    ) -> Result<Transcript, TranscribeError> {
        let fatal = |e: &dyn std::fmt::Display| TranscribeError::Fatal {
            status: None,
            message: format!("chunk {}: {e}", chunk.index + 1),
        };
        let samples = media::decode_pcm_f32(&chunk.path)
            .await
            .map_err(|e| fatal(&e))?;
        if samples.len() < MIN_SAMPLES {
            return Ok(Transcript::default());
        }
        let language = opts
            .language
            .clone()
            .or_else(|| self.pinned_language.get().cloned())
            .unwrap_or_else(|| "auto".into());

        let abort = Arc::new(AtomicBool::new(false));
        let _abort_on_drop = AbortOnDrop(Arc::clone(&abort));
        let job = Job {
            ctx: Arc::clone(&self.ctx),
            samples,
            language,
            prompt: opts.prompt.clone(),
            threads: self.threads,
            abort,
            running: Arc::clone(&self.running),
        };
        let transcript = tokio::task::spawn_blocking(move || run_full(job))
            .await
            .map_err(|e| fatal(&e))?
            .map_err(|e| fatal(&e))?;

        if opts.language.is_none()
            && !transcript.words.is_empty()
            && let Some(language) = &transcript.language
        {
            let _ = self.pinned_language.set(language.clone());
        }
        Ok(transcript)
    }

    fn model(&self) -> &str {
        &self.name
    }
}

/// Raises the abort flag when the `transcribe` future ends or is dropped.
struct AbortOnDrop(Arc<AtomicBool>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Relaxed);
    }
}

/// Counts a `state.full` call in progress, even if it panics.
struct Running(Arc<AtomicUsize>);

impl Running {
    fn start(counter: &Arc<AtomicUsize>) -> Running {
        counter.fetch_add(1, Ordering::SeqCst);
        Running(Arc::clone(counter))
    }
}

impl Drop for Running {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Everything the blocking thread needs for one chunk.
struct Job {
    ctx: Arc<WhisperContext>,
    samples: Vec<f32>,
    language: String,
    prompt: Option<String>,
    threads: usize,
    abort: Arc<AtomicBool>,
    running: Arc<AtomicUsize>,
}

unsafe extern "C" fn abort_requested(data: *mut c_void) -> bool {
    // SAFETY: `data` points into the `Arc<AtomicBool>` that `run_full` keeps alive
    // until `state.full` returns; whisper.cpp only calls this during `full`.
    unsafe { (*(data as *const AtomicBool)).load(Ordering::Relaxed) }
}

/// Runs whisper.cpp on one chunk and reads its segments and tokens back.
fn run_full(job: Job) -> Result<Transcript, WhisperError> {
    let mut state = job.ctx.create_state()?;
    let mut params = FullParams::new(SamplingStrategy::BeamSearch {
        beam_size: 5,
        patience: -1.0,
    });
    params.set_n_threads(c_int::try_from(job.threads).unwrap_or(c_int::MAX));
    params.set_translate(false);
    params.set_language(Some(&job.language));
    params.set_token_timestamps(true);
    params.set_suppress_nst(true);
    // print_realtime writes to stdout; the others would clutter the dashboard.
    params.set_print_special(false);
    params.set_print_progress(false);
    params.set_print_realtime(false);
    params.set_print_timestamps(false);
    if let Some(prompt) = &job.prompt {
        // set_initial_prompt panics on a NUL byte.
        params.set_initial_prompt(&prompt.replace('\0', ""));
    }
    // SAFETY: `abort_requested` only reads the `AtomicBool` behind `job.abort`,
    // which this function owns until after `state.full` returns.
    unsafe {
        params.set_abort_callback(Some(abort_requested));
        params.set_abort_callback_user_data(Arc::as_ptr(&job.abort) as *mut c_void);
    }
    {
        let _running = Running::start(&job.running);
        state.full(params, &job.samples)?;
    }

    let eot = job.ctx.token_eot();
    let mut text = Vec::new();
    let mut segments = Vec::new();
    let mut tokens = Vec::new();
    for segment in state.as_iter() {
        let mut pieces = Vec::new();
        let mut plog_sum = 0.0f32;
        for i in 0..segment.n_tokens() {
            let Some(token) = segment.get_token(i) else {
                continue;
            };
            if token.token_id() >= eot {
                continue;
            }
            let data = token.token_data();
            plog_sum += data.plog;
            pieces.push(TokenPiece {
                bytes: token.to_bytes()?.to_vec(),
                t0_cs: data.t0,
                t1_cs: data.t1,
            });
        }
        let segment_text = String::from_utf8_lossy(segment.to_bytes()?)
            .trim()
            .to_string();
        if pieces.is_empty() || segment_text.is_empty() {
            continue;
        }
        let mean_plog = plog_sum / pieces.len() as f32;
        if !keep_segment(segment.no_speech_probability(), mean_plog) {
            continue;
        }
        segments.push(Segment {
            start: segment.start_timestamp() as f64 / 100.0,
            end: segment.end_timestamp() as f64 / 100.0,
            text: segment_text.clone(),
            speaker: None,
        });
        text.push(segment_text);
        tokens.push(pieces);
    }

    Ok(Transcript {
        text: text.join(" "),
        words: words_from_tokens(&tokens),
        segments,
        language: whisper_rs::get_lang_str(state.full_lang_id_from_state()).map(String::from),
        cost_usd: None,
    })
}

#[cfg(test)]
mod tests {
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use super::*;
    use crate::probe::{SAMPLE, SAMPLE_S, write_sample};
    use crate::transcribe::{AudioChunk, TranscribeOptions, Transcriber, Usability};

    const TEST_THREADS: usize = 4;

    /// Path of the model for tests that need one, or `None` (with a note) to skip.
    fn test_model() -> Option<PathBuf> {
        let path = std::env::var_os("SRTTY_TEST_WHISPER_MODEL").map(PathBuf::from);
        if path.is_none() {
            eprintln!("skipped: SRTTY_TEST_WHISPER_MODEL is not set");
        }
        path
    }

    fn fake_mp3(dir: &Path) -> PathBuf {
        let path = dir.join("model.bin");
        std::fs::write(&path, b"ID3\x04\x00\x00\x00\x00\x00\x00 not a model").unwrap();
        path
    }

    fn ffmpeg(args: &[&str], out: &Path) {
        let status = std::process::Command::new("ffmpeg")
            .args(["-v", "error", "-y"])
            .args(args)
            .arg(out)
            .status()
            .unwrap();
        assert!(status.success(), "ffmpeg could not generate the test input");
    }

    fn chunk(path: PathBuf, end_s: f64) -> AudioChunk {
        AudioChunk {
            index: 0,
            start_s: 0.0,
            end_s,
            path,
        }
    }

    #[test]
    fn check_ggml_magic_cases() {
        let d = tempfile::tempdir().unwrap();
        let good = d.path().join("ggml.bin");
        std::fs::write(&good, b"lmgg\x00\x01\x02\x03").unwrap();
        check_ggml_magic(&good).unwrap();

        let err = check_ggml_magic(&fake_mp3(d.path())).unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(
            err.to_string().contains("not a ggml whisper model"),
            "{err}"
        );

        let err = check_ggml_magic(&d.path().join("missing.bin")).unwrap_err();
        assert_eq!(err.exit_code(), 1);
        assert!(err.to_string().contains("srtty setup"), "{err}");
    }

    #[test]
    fn check_language_cases() {
        check_language("fr").unwrap();
        check_language("en").unwrap();
        for code in ["french", "xx"] {
            let err = check_language(code).unwrap_err();
            assert_eq!(err.exit_code(), 1);
            assert!(err.to_string().contains("ISO 639-1"), "{err}");
        }
    }

    /// whisper.cpp logs unknown codes to stderr unless the hooks are installed:
    /// rerun `check_language_cases` alone in a child process and read its stderr.
    #[test]
    fn check_language_is_quiet() {
        let out = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "local::tests::check_language_cases",
                "--nocapture",
            ])
            .output()
            .unwrap();
        assert!(out.status.success());
        let stdout = String::from_utf8_lossy(&out.stdout);
        assert!(stdout.contains("1 passed"), "{stdout}");
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(!stderr.contains("whisper_"), "{stderr}");
    }

    #[test]
    fn segment_filter_follows_openai_rule() {
        assert!(!keep_segment(0.9, -1.5));
        assert!(keep_segment(0.9, -0.5));
        assert!(keep_segment(0.2, -1.5));
    }

    #[tokio::test]
    async fn load_rejects_a_non_ggml_file() {
        let d = tempfile::tempdir().unwrap();
        let err = LocalWhisper::load(&fake_mp3(d.path()), TEST_THREADS)
            .await
            .err()
            .unwrap();
        assert_eq!(err.exit_code(), 1);
    }

    #[tokio::test]
    async fn transcribes_the_embedded_sample_offline() {
        let Some(model) = test_model() else { return };
        let whisper = LocalWhisper::load(&model, TEST_THREADS).await.unwrap();
        assert_eq!(
            whisper.model(),
            model.file_name().unwrap().to_str().unwrap()
        );
        let sample = write_sample(SAMPLE, SAMPLE_S).unwrap();
        let t = whisper
            .transcribe(&sample.chunk, &TranscribeOptions::default())
            .await
            .unwrap();
        assert_eq!(t.usability(), Usability::Words);
        assert!(t.words.len() >= 40, "{} words: {}", t.words.len(), t.text);
        assert!(
            t.words.windows(2).all(|w| w[0].start <= w[1].start),
            "{:?}",
            t.words
        );
        assert!(
            t.words.iter().all(|w| (0.0..=30.5).contains(&w.start)),
            "{:?}",
            t.words
        );
        assert_eq!(t.language.as_deref(), Some("fr"));
        assert_eq!(t.cost_usd, None);
    }

    #[tokio::test]
    async fn silence_is_no_speech() {
        let Some(model) = test_model() else { return };
        let d = tempfile::tempdir().unwrap();
        let silence = d.path().join("silence.ogg");
        ffmpeg(
            &[
                "-f",
                "lavfi",
                "-i",
                "anullsrc=r=16000:cl=mono",
                "-t",
                "10",
                "-c:a",
                "libopus",
            ],
            &silence,
        );
        let whisper = LocalWhisper::load(&model, TEST_THREADS).await.unwrap();
        let t = whisper
            .transcribe(&chunk(silence, 10.0), &TranscribeOptions::default())
            .await
            .unwrap();
        assert_eq!(t.usability(), Usability::NoSpeech, "{t:?}");
    }

    #[tokio::test]
    async fn dropping_transcribe_aborts_whisper() {
        let Some(model) = test_model() else { return };
        let sample = write_sample(SAMPLE, SAMPLE_S).unwrap();
        let d = tempfile::tempdir().unwrap();
        let long = d.path().join("long.ogg");
        let sample_path = sample.chunk.path.to_str().unwrap().to_string();
        ffmpeg(
            &[
                "-stream_loop",
                "9",
                "-i",
                &sample_path,
                "-ac",
                "1",
                "-ar",
                "16000",
                "-c:a",
                "libopus",
                "-b:a",
                "32k",
            ],
            &long,
        );
        let whisper = Arc::new(LocalWhisper::load(&model, TEST_THREADS).await.unwrap());
        let task = tokio::spawn({
            let whisper = Arc::clone(&whisper);
            async move {
                whisper
                    .transcribe(&chunk(long, 300.0), &TranscribeOptions::default())
                    .await
            }
        });

        let started = Instant::now();
        while whisper.running() == 0 {
            assert!(
                started.elapsed() < Duration::from_secs(30),
                "whisper never started"
            );
            assert!(!task.is_finished(), "transcribe ended before whisper ran");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        task.abort();

        let aborted = Instant::now();
        while whisper.running() > 0 {
            assert!(
                aborted.elapsed() < Duration::from_secs(2),
                "whisper still running 2 s after the drop"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        assert!(task.await.unwrap_err().is_cancelled());
    }

    #[tokio::test]
    async fn language_is_pinned_after_the_first_chunk() {
        let Some(model) = test_model() else { return };
        let whisper = LocalWhisper::load(&model, TEST_THREADS).await.unwrap();
        assert_eq!(whisper.pinned_language(), None);
        let sample = write_sample(SAMPLE, SAMPLE_S).unwrap();
        whisper
            .transcribe(&sample.chunk, &TranscribeOptions::default())
            .await
            .unwrap();
        assert_eq!(whisper.pinned_language().as_deref(), Some("fr"));
    }
}
