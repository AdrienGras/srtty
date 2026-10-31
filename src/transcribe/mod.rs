//! Transcription domain types and the provider-facing trait.

pub mod openai;
pub mod parse;

use std::path::PathBuf;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

/// Segments averaging at most this many seconds are usable as subtitle cues.
const FINE_SEGMENT_MAX_AVG_S: f64 = 10.0;

/// One audio chunk cut from the source, with its position in the full timeline.
#[derive(Debug, Clone, PartialEq)]
pub struct AudioChunk {
    pub index: usize,
    pub start_s: f64,
    pub end_s: f64,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TranscribeOptions {
    pub language: Option<String>,
    pub prompt: Option<String>,
    pub provider_options: Option<serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Word {
    pub start: f64,
    pub end: f64,
    pub text: String,
    pub speaker: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Segment {
    pub start: f64,
    pub end: f64,
    pub text: String,
    pub speaker: Option<u32>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Transcript {
    pub text: String,
    pub words: Vec<Word>,
    pub segments: Vec<Segment>,
    pub language: Option<String>,
    pub cost_usd: Option<f64>,
}

/// How well a transcript can be turned into subtitles.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Usability {
    /// Word-level timestamps are available.
    Words,
    /// No words, but segments are short enough to serve as cues.
    FineSegments,
    /// The audio contains no speech.
    NoSpeech,
    /// Text exists but without timing fine enough to build subtitles.
    Unusable,
}

impl Transcript {
    /// Segments exist and average at most `FINE_SEGMENT_MAX_AVG_S` seconds.
    pub fn has_fine_segments(&self) -> bool {
        if self.segments.is_empty() {
            return false;
        }
        let total: f64 = self.segments.iter().map(|s| s.end - s.start).sum();
        total / self.segments.len() as f64 <= FINE_SEGMENT_MAX_AVG_S
    }

    pub fn usability(&self) -> Usability {
        if !self.words.is_empty() {
            return Usability::Words;
        }
        if self.has_fine_segments() {
            return Usability::FineSegments;
        }
        if self.text.trim().is_empty() && self.segments.is_empty() {
            return Usability::NoSpeech;
        }
        Usability::Unusable
    }
}

/// Failure of a transcription request. Never carries an API key.
#[derive(Debug, Error)]
pub enum TranscribeError {
    #[error("model does not support verbose_json output: {message}")]
    ModelUnsupported { message: String },

    #[error("authentication failed (HTTP {status}); check your API key")]
    Auth { status: u16 },

    #[error("transient transcription failure: {message}")]
    Retryable {
        status: Option<u16>,
        message: String,
        retry_after_s: Option<f64>,
    },

    #[error("transcription failed: {message}")]
    Fatal {
        status: Option<u16>,
        message: String,
    },
}

#[async_trait]
pub trait Transcriber: Send + Sync {
    async fn transcribe(
        &self,
        chunk: &AudioChunk,
        opts: &TranscribeOptions,
    ) -> Result<Transcript, TranscribeError>;

    fn model(&self) -> &str;
}
