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
    DtwMode, DtwModelPreset, DtwParameters, FullParams, SamplingStrategy, WhisperContext,
    WhisperContextParameters, WhisperError,
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
/// Least probability of a detected language for it to be pinned: a chunk that
/// opens with music gives a doubtful detection, which must not spread.
const PIN_MIN_PROB: f32 = 0.8;
/// Memory for the DTW computation (whisper-rs's default, 128 MiB).
const DTW_MEM_SIZE: usize = 128 * 1024 * 1024;

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
    // whisper-rs panics on a NUL byte; no language code contains one.
    let short = if code.contains('\0') {
        None
    } else {
        whisper_rs::get_lang_id(code).and_then(whisper_rs::get_lang_str)
    };
    match short {
        Some(short) if short == code => Ok(()),
        _ => Err(SrttyError::Usage(format!(
            "unknown language `{code}` for the local backend; use an ISO 639-1 code such as fr or en"
        ))),
    }
}

/// Routes whisper.cpp and ggml logs through whisper-rs hooks that, without its
/// `log_backend` or `tracing_backend` feature, drop them, so they never reach
/// stderr. Only the first call has an effect.
fn quiet_whisper() {
    whisper_rs::install_logging_hooks();
}

/// The short code of language `id` when whisper gave it a probability of at
/// least `PIN_MIN_PROB`; `None` for a doubtful detection or an unknown id.
fn confident_language(id: i32, probs: &[f32]) -> Option<&'static str> {
    let prob = *probs.get(usize::try_from(id).ok()?)?;
    if prob >= PIN_MIN_PROB {
        whisper_rs::get_lang_str(id)
    } else {
        None
    }
}

/// Called with the language code once a detected language is pinned.
pub type LanguageNotifier = Box<dyn Fn(&str) + Send + Sync>;

/// Keeps a segment unless it is probably silence (`no_speech_prob > 0.6`) and
/// its tokens are unlikely (mean log probability below -1), as OpenAI Whisper
/// does against hallucinations.
pub fn keep_segment(no_speech_prob: f32, mean_plog: f32) -> bool {
    !(no_speech_prob > NO_SPEECH_THRESHOLD && mean_plog < LOGPROB_THRESHOLD)
}

/// The DTW alignment preset of a ggml model, from its file name
/// (`ggml-<name>[-q<d>_<d>].bin`); `None` for a name that is not a known
/// whisper model, which then keeps whisper.cpp's own token times.
fn dtw_preset(path: &Path) -> Option<DtwModelPreset> {
    let file = path.file_name()?.to_str()?;
    let name = file.strip_prefix("ggml-").unwrap_or(file);
    let name = name.strip_suffix(".bin").unwrap_or(name);
    let name = strip_quantization(name);
    Some(match name {
        "tiny" => DtwModelPreset::Tiny,
        "tiny.en" => DtwModelPreset::TinyEn,
        "base" => DtwModelPreset::Base,
        "base.en" => DtwModelPreset::BaseEn,
        "small" => DtwModelPreset::Small,
        "small.en" => DtwModelPreset::SmallEn,
        "medium" => DtwModelPreset::Medium,
        "medium.en" => DtwModelPreset::MediumEn,
        "large-v1" => DtwModelPreset::LargeV1,
        "large-v2" => DtwModelPreset::LargeV2,
        "large-v3" => DtwModelPreset::LargeV3,
        "large-v3-turbo" => DtwModelPreset::LargeV3Turbo,
        _ => return None,
    })
}

/// `name` without a trailing `-q<digit>_<digit>` quantization suffix.
fn strip_quantization(name: &str) -> &str {
    if let Some((head, tail)) = name.rsplit_once("-q")
        && let [major, b'_', minor] = tail.as_bytes()
        && major.is_ascii_digit()
        && minor.is_ascii_digit()
    {
        return head;
    }
    name
}

/// The local whisper.cpp backend.
pub struct LocalWhisper {
    ctx: Arc<WhisperContext>,
    /// Whether the context was built with DTW token timestamps.
    dtw: bool,
    name: String,
    threads: usize,
    /// Language confidently detected on the first chunk with words, given to
    /// the next ones.
    pinned_language: OnceLock<String>,
    /// Told once when `pinned_language` is set.
    on_pinned: Option<LanguageNotifier>,
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
        let preset = dtw_preset(path);
        let dtw = preset.is_some();
        let ctx = tokio::task::spawn_blocking(move || {
            let mut params = WhisperContextParameters::default();
            if let Some(model_preset) = preset {
                // DTW places word onsets after pauses where whisper.cpp's own
                // token times are contiguous. It needs flash attention off,
                // which is the default.
                params.dtw_parameters(DtwParameters {
                    mode: DtwMode::ModelPreset { model_preset },
                    dtw_mem_size: DTW_MEM_SIZE,
                });
            }
            WhisperContext::new_with_params(&owned, params)
        })
        .await
        .map_err(|e| cannot_load(&e))?
        .map_err(|e| cannot_load(&e))?;
        Ok(LocalWhisper {
            ctx: Arc::new(ctx),
            dtw,
            name: path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into(),
            ),
            threads,
            pinned_language: OnceLock::new(),
            on_pinned: None,
            running: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// Calls `notify` with the language code when a detected language gets
    /// pinned, which happens at most once.
    pub fn on_language_pinned(&mut self, notify: LanguageNotifier) {
        self.on_pinned = Some(notify);
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
        // `None`: the blocking job detects the language itself.
        let language = opts
            .language
            .clone()
            .or_else(|| self.pinned_language.get().cloned());

        let abort = Arc::new(AtomicBool::new(false));
        let _abort_on_drop = AbortOnDrop(Arc::clone(&abort));
        let job = Job {
            ctx: Arc::clone(&self.ctx),
            dtw: self.dtw,
            samples,
            language,
            prompt: opts.prompt.clone(),
            threads: self.threads,
            abort,
            running: Arc::clone(&self.running),
        };
        let (transcript, detected) = tokio::task::spawn_blocking(move || run_full(job))
            .await
            .map_err(|e| fatal(&e))?
            .map_err(|e| fatal(&e))?;

        if !transcript.words.is_empty()
            && let Some(language) = detected
            && self.pinned_language.set(language.to_string()).is_ok()
            && let Some(notify) = &self.on_pinned
        {
            notify(language);
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
    /// Whether to read the DTW token timestamps.
    dtw: bool,
    samples: Vec<f32>,
    /// The language to decode in, or `None` to detect it.
    language: Option<String>,
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
///
/// Without a language, it is detected first on the opening 30 s: a confident
/// detection is used for this chunk and returned so that it can be pinned; a
/// doubtful one is left to whisper.cpp (`auto`) and not returned.
fn run_full(job: Job) -> Result<(Transcript, Option<&'static str>), WhisperError> {
    let mut state = job.ctx.create_state()?;
    let detected = match job.language {
        Some(_) => None,
        None => {
            state.pcm_to_mel(&job.samples, job.threads)?;
            let (id, probs) = state.lang_detect(0, job.threads)?;
            confident_language(id, &probs)
        }
    };
    let language = job.language.as_deref().or(detected).unwrap_or("auto");
    let mut params = FullParams::new(SamplingStrategy::BeamSearch {
        beam_size: 5,
        patience: -1.0,
    });
    params.set_n_threads(c_int::try_from(job.threads).unwrap_or(c_int::MAX));
    params.set_translate(false);
    params.set_language(Some(language));
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
        // Counts only `full`, the part the abort flag can stop; the mel and the
        // language detection above cannot be aborted.
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
                t_dtw_cs: (job.dtw && data.t_dtw >= 0).then_some(data.t_dtw),
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

    let transcript = Transcript {
        text: text.join(" "),
        words: words_from_tokens(&tokens),
        segments,
        language: whisper_rs::get_lang_str(state.full_lang_id_from_state()).map(String::from),
        cost_usd: None,
    };
    Ok((transcript, detected))
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
    fn dtw_preset_from_file_name() {
        let name = |file: &str| match dtw_preset(Path::new(file)) {
            Some(preset) => format!("{preset:?}"),
            None => "None".to_string(),
        };
        for (file, expected) in [
            ("ggml-base.bin", "Base"),
            ("/models/ggml-large-v3-turbo-q5_0.bin", "LargeV3Turbo"),
            ("ggml-large-v3-turbo.bin", "LargeV3Turbo"),
            ("ggml-small.en-q8_0.bin", "SmallEn"),
            ("ggml-tiny.bin", "Tiny"),
            ("ggml-tiny.en.bin", "TinyEn"),
            ("ggml-base.en-q5_1.bin", "BaseEn"),
            ("ggml-medium.bin", "Medium"),
            ("ggml-medium.en.bin", "MediumEn"),
            ("ggml-large-v1.bin", "LargeV1"),
            ("ggml-large-v2-q5_0.bin", "LargeV2"),
            ("ggml-large-v3.bin", "LargeV3"),
            ("ggml-small.bin", "Small"),
            ("my-model.bin", "None"),
            ("ggml-model.bin", "None"),
            ("ggml-large.bin", "None"),
            ("ggml-base-q5.bin", "None"),
            ("", "None"),
        ] {
            assert_eq!(name(file), expected, "{file}");
        }
    }

    #[test]
    fn check_language_cases() {
        check_language("fr").unwrap();
        check_language("en").unwrap();
        for code in ["french", "xx", "f\0r"] {
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
    fn only_a_confident_detection_is_pinned() {
        // whisper.cpp logs an unknown id to stderr unless the hooks are installed.
        quiet_whisper();
        let fr = whisper_rs::get_lang_id("fr").unwrap();
        let mut probs = vec![0.0f32; whisper_rs::get_lang_max_id() as usize + 1];
        probs[fr as usize] = 0.95;
        assert_eq!(confident_language(fr, &probs), Some("fr"));
        probs[fr as usize] = 0.5;
        assert_eq!(confident_language(fr, &probs), None);
        let past_the_end = i32::try_from(probs.len()).unwrap();
        assert_eq!(confident_language(past_the_end, &[1.0; 200]), None);
        assert_eq!(confident_language(past_the_end, &probs), None);
        assert_eq!(confident_language(-1, &probs), None);
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
        // `running` counts `state.full` only, so the drop lands in the part
        // the abort flag can stop.
        task.abort();

        // `full` starts with the mel of the whole 300 s, which cannot be
        // aborted and may take a few seconds on a small CI machine. A run that
        // is not aborted takes far longer than 10 s, so the bound still tells
        // them apart.
        let aborted = Instant::now();
        while whisper.running() > 0 {
            assert!(
                aborted.elapsed() < Duration::from_secs(10),
                "whisper still running 10 s after the drop"
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

    #[tokio::test]
    async fn pinning_the_language_is_notified_once() {
        let Some(model) = test_model() else { return };
        let mut whisper = LocalWhisper::load(&model, TEST_THREADS).await.unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        whisper.on_language_pinned(Box::new({
            let seen = Arc::clone(&seen);
            move |language: &str| seen.lock().unwrap().push(language.to_string())
        }));
        let sample = write_sample(SAMPLE, SAMPLE_S).unwrap();
        for _ in 0..2 {
            whisper
                .transcribe(&sample.chunk, &TranscribeOptions::default())
                .await
                .unwrap();
        }
        assert_eq!(*seen.lock().unwrap(), ["fr"]);
    }
}
