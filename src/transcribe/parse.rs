//! Parsing of OpenAI-style `verbose_json` transcription responses.

use serde::Deserialize;
use serde_json::Value;

use super::{Segment, TranscribeError, Transcript, Word};

#[derive(Deserialize)]
struct RawResponse {
    #[serde(default)]
    text: String,
    #[serde(default)]
    language: Option<String>,
    #[serde(default)]
    words: Vec<RawWord>,
    #[serde(default)]
    segments: Vec<RawSegment>,
    #[serde(default)]
    usage: Option<RawUsage>,
}

#[derive(Deserialize)]
struct RawWord {
    #[serde(default)]
    word: String,
    #[serde(default)]
    start: f64,
    #[serde(default)]
    end: f64,
    #[serde(default)]
    speaker: Option<u32>,
}

#[derive(Deserialize)]
struct RawSegment {
    #[serde(default)]
    text: String,
    #[serde(default)]
    start: f64,
    #[serde(default)]
    end: f64,
    #[serde(default)]
    speaker: Option<u32>,
}

#[derive(Deserialize)]
struct RawUsage {
    #[serde(default)]
    cost: Option<f64>,
}

/// Converts a `verbose_json` body into a [`Transcript`].
///
/// A body holding an `error` object becomes a [`TranscribeError`]. Unknown
/// fields are ignored.
pub fn parse_verbose_json(v: &Value) -> Result<Transcript, TranscribeError> {
    if let Some(err) = v.get("error") {
        let message = err
            .get("message")
            .and_then(Value::as_str)
            .map(str::to_owned)
            .unwrap_or_else(|| err.to_string());
        return Err(if message.contains("verbose_json") {
            TranscribeError::ModelUnsupported { message }
        } else {
            TranscribeError::Fatal {
                status: None,
                message,
            }
        });
    }

    let raw: RawResponse =
        serde_json::from_value(v.clone()).map_err(|e| TranscribeError::Fatal {
            status: None,
            message: format!("unexpected transcription response: {e}"),
        })?;

    Ok(Transcript {
        text: raw.text,
        words: raw
            .words
            .into_iter()
            .map(|w| Word {
                start: w.start,
                end: w.end,
                text: w.word,
                speaker: w.speaker,
            })
            .collect(),
        segments: raw
            .segments
            .into_iter()
            .map(|s| Segment {
                start: s.start,
                end: s.end,
                text: s.text,
                speaker: s.speaker,
            })
            .collect(),
        language: raw.language,
        cost_usd: raw.usage.and_then(|u| u.cost),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::fx;
    use crate::transcribe::{TranscribeError, Transcript, Usability};

    #[test]
    fn parses_words_segments_and_cost() {
        let t = parse_verbose_json(&fx("deepgram")).unwrap();
        assert_eq!(t.words.len(), 96);
        assert_eq!(t.segments.len(), 7);
        assert_eq!(t.words[0].text, "C'est");
        assert!((t.cost_usd.unwrap() - 0.00215688).abs() < 1e-6);
        assert_eq!(t.usability(), Usability::Words);
    }

    #[test]
    fn timestamps_are_read_as_floats() {
        let t = parse_verbose_json(&fx("deepgram")).unwrap();
        assert!((t.words[0].start - 0.16).abs() < 1e-6);
        assert!((t.words[0].end - 0.4).abs() < 1e-6);
        assert_eq!(t.language.as_deref(), Some("fr"));
    }

    #[test]
    fn speaker_is_read_from_words() {
        let t = parse_verbose_json(&fx("grok_diar")).unwrap();
        assert_eq!(t.words[0].speaker, Some(0));
    }

    #[test]
    fn missing_speaker_is_none() {
        let t = parse_verbose_json(&fx("deepgram")).unwrap();
        assert_eq!(t.words[0].speaker, None);
    }

    #[test]
    fn assemblyai_single_segment_no_words_is_unusable() {
        assert_eq!(
            parse_verbose_json(&fx("assemblyai")).unwrap().usability(),
            Usability::Unusable
        );
    }

    #[test]
    fn error_body_mentioning_verbose_json_is_model_unsupported() {
        assert!(matches!(
            parse_verbose_json(&fx("gpt4o_error")),
            Err(TranscribeError::ModelUnsupported { .. })
        ));
    }

    #[test]
    fn other_error_body_is_fatal() {
        let v = serde_json::json!({"error": {"message": "boom", "code": 500}});
        assert!(matches!(
            parse_verbose_json(&v),
            Err(TranscribeError::Fatal { .. })
        ));
    }

    #[test]
    fn unknown_fields_are_ignored_and_missing_ones_default() {
        let t = parse_verbose_json(&serde_json::json!({"text": "hi", "whatever": [1, 2]})).unwrap();
        assert_eq!(t.text, "hi");
        assert!(t.words.is_empty() && t.segments.is_empty());
        assert_eq!(t.cost_usd, None);
    }

    #[test]
    fn every_fixture_parses() {
        for name in ["whisper_turbo", "whisper1", "scribe", "fish"] {
            let t = parse_verbose_json(&fx(name)).unwrap();
            assert!(!t.text.is_empty(), "{name}");
        }
    }

    #[test]
    fn fine_segments_without_words() {
        let mut t = parse_verbose_json(&fx("deepgram")).unwrap();
        t.words.clear();
        assert_eq!(t.usability(), Usability::FineSegments);
    }

    #[test]
    fn empty_text_is_no_speech() {
        assert_eq!(Transcript::default().usability(), Usability::NoSpeech);
    }
}
