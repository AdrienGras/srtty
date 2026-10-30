//! Classifies a transcription model from a real transcription of an embedded sample.

use std::collections::HashSet;
use std::path::PathBuf;

use crate::report::Reporter;
use crate::run::transcribe_with_retry;
use crate::transcribe::{AudioChunk, TranscribeError, TranscribeOptions, Transcriber, Transcript};

/// 30 s of one French voice (LibriVox, public domain). See `assets/README.md`.
pub const SAMPLE: &[u8] = include_bytes!("../assets/sample-30s.ogg");
pub const SAMPLE_S: f64 = 30.0;
/// 45 s of a French play with several voices (LibriVox, public domain).
pub const SPEAKERS_SAMPLE: &[u8] = include_bytes!("../assets/speakers-45s.ogg");
pub const SPEAKERS_SAMPLE_S: f64 = 45.0;
/// Language of both samples, sent to the model.
pub const SAMPLE_LANGUAGE: &str = "fr";

/// What a model returned for the sample.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Timestamps good enough for subtitles.
    Usable { words: bool, fine_segments: bool },
    /// Answered, but without usable timestamps.
    Unusable,
    /// The model or endpoint cannot do verbose transcription.
    Incompatible,
    /// Transient or unknown failure: no conclusion.
    Inconclusive(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProbeOutcome {
    pub verdict: Verdict,
    pub cost_usd: Option<f64>,
}

/// Turns a transcription result into a verdict. Only authentication failures
/// are errors: they say nothing about the model.
pub fn classify(
    result: Result<Transcript, TranscribeError>,
) -> Result<ProbeOutcome, TranscribeError> {
    let verdict = match result {
        Ok(t) => {
            let words = !t.words.is_empty();
            let fine_segments = t.has_fine_segments();
            let verdict = if words || fine_segments {
                Verdict::Usable {
                    words,
                    fine_segments,
                }
            } else {
                Verdict::Unusable
            };
            return Ok(ProbeOutcome {
                verdict,
                cost_usd: t.cost_usd,
            });
        }
        Err(e @ TranscribeError::Auth { .. }) => return Err(e),
        Err(TranscribeError::ModelUnsupported { .. }) => Verdict::Incompatible,
        Err(TranscribeError::Fatal {
            status: Some(400..=499),
            ..
        }) => Verdict::Incompatible,
        Err(e @ (TranscribeError::Fatal { .. } | TranscribeError::Retryable { .. })) => {
            Verdict::Inconclusive(e.to_string())
        }
    };
    Ok(ProbeOutcome {
        verdict,
        cost_usd: None,
    })
}

/// Number of distinct speaker labels carried by the words.
pub fn distinct_speakers(t: &Transcript) -> usize {
    t.words
        .iter()
        .filter_map(|w| w.speaker)
        .collect::<HashSet<_>>()
        .len()
}

/// A sample written to a temporary directory, removed on drop.
pub struct SampleFile {
    pub chunk: AudioChunk,
    _dir: tempfile::TempDir,
}

pub fn write_sample(bytes: &[u8], duration_s: f64) -> std::io::Result<SampleFile> {
    let dir = tempfile::tempdir()?;
    let path: PathBuf = dir.path().join("sample.ogg");
    std::fs::write(&path, bytes)?;
    Ok(SampleFile {
        chunk: AudioChunk {
            index: 0,
            start_s: 0.0,
            end_s: duration_s,
            path,
        },
        _dir: dir,
    })
}

/// Transcribes the sample in French and classifies the answer.
pub async fn probe_model(
    t: &dyn Transcriber,
    sample: &AudioChunk,
    reporter: &dyn Reporter,
) -> Result<ProbeOutcome, TranscribeError> {
    let opts = TranscribeOptions {
        language: Some(SAMPLE_LANGUAGE.into()),
        ..Default::default()
    };
    classify(transcribe_with_retry(t, sample, &opts, reporter).await)
}

/// Asks for diarization with `option` and counts the voices in the answer.
/// `Some(true)` for two voices or more, `Some(false)` for a usable answer with
/// fewer, `None` when nothing can be concluded.
pub async fn probe_diarization(
    t: &dyn Transcriber,
    sample: &AudioChunk,
    option: &serde_json::Value,
    reporter: &dyn Reporter,
) -> Result<(Option<bool>, Option<f64>), TranscribeError> {
    let opts = TranscribeOptions {
        language: Some(SAMPLE_LANGUAGE.into()),
        provider_options: Some(option.clone()),
        ..Default::default()
    };
    match transcribe_with_retry(t, sample, &opts, reporter).await {
        Ok(transcript) => {
            let cost = transcript.cost_usd;
            let speakers = distinct_speakers(&transcript);
            let found = match classify(Ok(transcript))?.verdict {
                Verdict::Usable { .. } => Some(speakers >= 2),
                _ => None,
            };
            Ok((found, cost))
        }
        Err(e @ TranscribeError::Auth { .. }) => Err(e),
        Err(_) => Ok((None, None)),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use async_trait::async_trait;

    use super::*;
    use crate::report::PlainReporter;
    use crate::test_support::fx;
    use crate::transcribe::parse::parse_verbose_json;

    fn t(name: &str) -> Transcript {
        parse_verbose_json(&fx(name)).unwrap()
    }

    #[test]
    fn classify_spike_responses() {
        assert!(matches!(
            classify(Ok(t("deepgram"))).unwrap().verdict,
            Verdict::Usable {
                words: true,
                fine_segments: true
            }
        ));
        assert!(matches!(
            classify(Ok(t("scribe"))).unwrap().verdict,
            Verdict::Usable {
                words: true,
                fine_segments: false
            }
        ));
        assert!(matches!(
            classify(Ok(t("assemblyai"))).unwrap().verdict,
            Verdict::Unusable
        ));
    }

    #[test]
    fn classify_errors() {
        let unsupported = TranscribeError::ModelUnsupported {
            message: "verbose_json".into(),
        };
        assert!(matches!(
            classify(Err(unsupported)).unwrap().verdict,
            Verdict::Incompatible
        ));
        let bad_request = TranscribeError::Fatal {
            status: Some(400),
            message: "x".into(),
        };
        assert!(matches!(
            classify(Err(bad_request)).unwrap().verdict,
            Verdict::Incompatible
        ));
        let server = TranscribeError::Fatal {
            status: Some(500),
            message: "x".into(),
        };
        assert!(matches!(
            classify(Err(server)).unwrap().verdict,
            Verdict::Inconclusive(_)
        ));
        let gone = TranscribeError::Retryable {
            status: Some(503),
            message: "x".into(),
            retry_after_s: None,
        };
        assert!(matches!(
            classify(Err(gone)).unwrap().verdict,
            Verdict::Inconclusive(_)
        ));
        assert!(classify(Err(TranscribeError::Auth { status: 401 })).is_err());
    }

    #[test]
    fn classify_keeps_the_cost() {
        let mut transcript = t("deepgram");
        transcript.cost_usd = Some(0.002);
        assert_eq!(classify(Ok(transcript)).unwrap().cost_usd, Some(0.002));
    }

    #[test]
    fn grok_diarization_fixture_has_several_speakers() {
        assert!(distinct_speakers(&t("grok_diar")) >= 3);
        assert_eq!(distinct_speakers(&t("deepgram")), 0);
    }

    #[test]
    fn embedded_samples_are_ogg_and_small() {
        for bytes in [SAMPLE, SPEAKERS_SAMPLE] {
            assert!(bytes.starts_with(b"OggS"));
            assert!(bytes.len() < 300_000);
        }
    }

    #[test]
    fn write_sample_creates_an_ogg_chunk() {
        let sample = write_sample(SAMPLE, SAMPLE_S).unwrap();
        assert_eq!(sample.chunk.index, 0);
        assert_eq!(sample.chunk.start_s, 0.0);
        assert_eq!(sample.chunk.end_s, SAMPLE_S);
        assert_eq!(sample.chunk.path.file_name().unwrap(), "sample.ogg");
        assert_eq!(std::fs::read(&sample.chunk.path).unwrap(), SAMPLE);
    }

    /// Replies with a fixed result and records the options it receives.
    struct Fake {
        reply: Mutex<Option<Result<Transcript, TranscribeError>>>,
        seen: Mutex<Vec<TranscribeOptions>>,
    }

    impl Fake {
        fn new(reply: Result<Transcript, TranscribeError>) -> Self {
            Self {
                reply: Mutex::new(Some(reply)),
                seen: Mutex::new(Vec::new()),
            }
        }
    }

    #[async_trait]
    impl Transcriber for Fake {
        async fn transcribe(
            &self,
            _chunk: &AudioChunk,
            opts: &TranscribeOptions,
        ) -> Result<Transcript, TranscribeError> {
            self.seen.lock().unwrap().push(opts.clone());
            self.reply.lock().unwrap().take().expect("one call only")
        }

        fn model(&self) -> &str {
            "fake-model"
        }
    }

    fn reporter() -> PlainReporter {
        PlainReporter::new(Box::new(std::io::sink()))
    }

    fn chunk() -> AudioChunk {
        AudioChunk {
            index: 0,
            start_s: 0.0,
            end_s: SAMPLE_S,
            path: "sample.ogg".into(),
        }
    }

    #[tokio::test]
    async fn probe_model_sends_french_and_classifies() {
        let fake = Fake::new(Ok(t("deepgram")));
        let outcome = probe_model(&fake, &chunk(), &reporter()).await.unwrap();
        assert!(matches!(
            outcome.verdict,
            Verdict::Usable {
                words: true,
                fine_segments: true
            }
        ));
        let seen = fake.seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(seen[0].language.as_deref(), Some("fr"));
        assert_eq!(seen[0].provider_options, None);
    }

    #[tokio::test]
    async fn probe_model_propagates_auth_errors() {
        let fake = Fake::new(Err(TranscribeError::Auth { status: 401 }));
        assert!(probe_model(&fake, &chunk(), &reporter()).await.is_err());
    }

    #[tokio::test]
    async fn probe_diarization_detects_several_speakers() {
        let option = serde_json::json!({ "diarize": true });
        let mut transcript = t("grok_diar");
        transcript.cost_usd = Some(0.01);
        let fake = Fake::new(Ok(transcript));
        let got = probe_diarization(&fake, &chunk(), &option, &reporter())
            .await
            .unwrap();
        assert_eq!(got, (Some(true), Some(0.01)));
        let seen = fake.seen.lock().unwrap();
        assert_eq!(seen[0].provider_options, Some(option));
    }

    #[tokio::test]
    async fn probe_diarization_reports_a_single_voice() {
        let fake = Fake::new(Ok(t("deepgram")));
        let got = probe_diarization(&fake, &chunk(), &serde_json::json!({}), &reporter())
            .await
            .unwrap();
        assert_eq!(got.0, Some(false));
    }

    #[tokio::test]
    async fn probe_diarization_is_inconclusive_on_failure_and_errs_on_auth() {
        let fake = Fake::new(Err(TranscribeError::Fatal {
            status: Some(500),
            message: "x".into(),
        }));
        let got = probe_diarization(&fake, &chunk(), &serde_json::json!({}), &reporter())
            .await
            .unwrap();
        assert_eq!(got, (None, None));
        let fake = Fake::new(Err(TranscribeError::Auth { status: 403 }));
        assert!(
            probe_diarization(&fake, &chunk(), &serde_json::json!({}), &reporter())
                .await
                .is_err()
        );
    }
}
