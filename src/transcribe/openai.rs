//! Transcriber for OpenAI-compatible `/audio/transcriptions` endpoints.

use std::time::Duration;

use async_trait::async_trait;
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use reqwest::header::RETRY_AFTER;
use reqwest::multipart::{Form, Part};
use reqwest::{Client, Response, StatusCode, Url};
use serde_json::{Value, json};

use super::parse::parse_verbose_json;
use super::{AudioChunk, TranscribeError, TranscribeOptions, Transcriber, Transcript};
use crate::config::{ApiKey, RequestMode};

const REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
const MAX_ERROR_CHARS: usize = 300;

/// Sends one chunk per call to an OpenAI-compatible transcription endpoint.
///
/// A call makes a single attempt; retries belong to the caller.
pub struct OpenAiCompatible {
    client: Client,
    endpoint: String,
    api_key: ApiKey,
    model: String,
    mode: RequestMode,
}

impl OpenAiCompatible {
    pub fn new(
        base_url: String,
        api_key: ApiKey,
        model: String,
        mode: RequestMode,
    ) -> Result<Self, TranscribeError> {
        let client = Client::builder()
            .timeout(REQUEST_TIMEOUT)
            .build()
            .map_err(|e| TranscribeError::Fatal {
                status: None,
                message: format!("cannot build the HTTP client: {e}"),
            })?;
        Ok(Self {
            client,
            endpoint: format!("{}/audio/transcriptions", base_url.trim_end_matches('/')),
            api_key,
            model,
            mode,
        })
    }

    /// Replaces every occurrence of the API key in provider-supplied text.
    fn redact(&self, text: &str) -> String {
        let key = self.api_key.expose();
        if key.is_empty() {
            text.to_owned()
        } else {
            text.replace(key, "***")
        }
    }

    fn redact_error(&self, error: TranscribeError) -> TranscribeError {
        match error {
            TranscribeError::ModelUnsupported { message } => TranscribeError::ModelUnsupported {
                message: self.redact(&message),
            },
            TranscribeError::Retryable {
                status,
                message,
                retry_after_s,
            } => TranscribeError::Retryable {
                status,
                message: self.redact(&message),
                retry_after_s,
            },
            TranscribeError::Fatal { status, message } => TranscribeError::Fatal {
                status,
                message: self.redact(&message),
            },
            auth @ TranscribeError::Auth { .. } => auth,
        }
    }

    fn classify_failure(
        &self,
        status: StatusCode,
        body: &str,
        retry_after_s: Option<f64>,
    ) -> TranscribeError {
        let code = status.as_u16();
        let message = truncate(&self.redact(&error_message(body)), MAX_ERROR_CHARS);
        match code {
            401 | 403 => TranscribeError::Auth { status: code },
            408 | 429 | 500..=599 => TranscribeError::Retryable {
                status: Some(code),
                message: format!("HTTP {code}: {message}"),
                retry_after_s,
            },
            400 if body.contains("verbose_json") => TranscribeError::ModelUnsupported { message },
            _ => TranscribeError::Fatal {
                status: Some(code),
                message,
            },
        }
    }

    fn build_request(
        &self,
        chunk: &AudioChunk,
        opts: &TranscribeOptions,
        audio: Vec<u8>,
    ) -> reqwest::RequestBuilder {
        let request = self
            .client
            .post(&self.endpoint)
            .bearer_auth(self.api_key.expose());
        let provider = opts
            .provider_options
            .as_ref()
            .map(|options| json!({ "options": options }));
        match self.mode {
            RequestMode::Multipart => {
                let file = Part::bytes(audio)
                    .file_name(format!("chunk-{:03}.ogg", chunk.index))
                    .mime_str("audio/ogg")
                    .expect("static MIME type is valid");
                let mut form = Form::new()
                    .part("file", file)
                    .text("model", self.model.clone())
                    .text("response_format", "verbose_json")
                    .text("timestamp_granularities[]", "segment")
                    .text("timestamp_granularities[]", "word");
                if let Some(language) = &opts.language {
                    form = form.text("language", language.clone());
                }
                if let Some(prompt) = &opts.prompt {
                    form = form.text("prompt", prompt.clone());
                }
                if let Some(provider) = provider {
                    form = form.text("provider", provider.to_string());
                }
                request.multipart(form)
            }
            RequestMode::JsonBase64 => {
                let mut body = json!({
                    "model": self.model,
                    "input_audio": { "data": STANDARD.encode(audio), "format": "ogg" },
                    "response_format": "verbose_json",
                    "timestamp_granularities": ["segment", "word"],
                });
                if let Some(language) = &opts.language {
                    body["language"] = json!(language);
                }
                if let Some(prompt) = &opts.prompt {
                    body["prompt"] = json!(prompt);
                }
                if let Some(provider) = provider {
                    body["provider"] = provider;
                }
                request.json(&body)
            }
        }
    }
}

#[async_trait]
impl Transcriber for OpenAiCompatible {
    async fn transcribe(
        &self,
        chunk: &AudioChunk,
        opts: &TranscribeOptions,
    ) -> Result<Transcript, TranscribeError> {
        let audio = tokio::fs::read(&chunk.path)
            .await
            .map_err(|e| TranscribeError::Fatal {
                status: None,
                message: format!("cannot read audio chunk {}: {e}", chunk.path.display()),
            })?;
        let response = self
            .build_request(chunk, opts, audio)
            .send()
            .await
            .map_err(transport_error)?;
        let status = response.status();
        let retry_after_s = retry_after_seconds(&response);
        let body = response.bytes().await.map_err(transport_error)?;

        if status.is_success() {
            let value: Value =
                serde_json::from_slice(&body).map_err(|e| TranscribeError::Fatal {
                    status: Some(status.as_u16()),
                    message: format!("response is not valid JSON: {e}"),
                })?;
            return parse_verbose_json(&value).map_err(|e| self.redact_error(e));
        }
        Err(self.classify_failure(status, &String::from_utf8_lossy(&body), retry_after_s))
    }

    fn model(&self) -> &str {
        &self.model
    }
}

/// Prefers the provider's `error.message`, falling back to the raw body.
fn error_message(body: &str) -> String {
    let from_json = serde_json::from_str::<Value>(body).ok().and_then(|v| {
        let error = v.get("error")?;
        error
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| error.as_str())
            .map(str::to_owned)
    });
    from_json.unwrap_or_else(|| body.trim().to_owned())
}

fn truncate(text: &str, max_chars: usize) -> String {
    text.chars().take(max_chars).collect()
}

fn retry_after_seconds(response: &Response) -> Option<f64> {
    response
        .headers()
        .get(RETRY_AFTER)?
        .to_str()
        .ok()?
        .trim()
        .parse::<f64>()
        .ok()
        .filter(|s| s.is_finite() && *s >= 0.0)
}

/// Timeouts, connection failures and body read errors are worth retrying.
fn transport_error(e: reqwest::Error) -> TranscribeError {
    let message = if e.is_timeout() {
        "request timed out".to_owned()
    } else if e.is_connect() {
        "could not connect to the transcription endpoint".to_owned()
    } else {
        format!("network error: {}", e.without_url())
    };
    TranscribeError::Retryable {
        status: None,
        message,
        retry_after_s: None,
    }
}

/// The host of `base_url`, or `None` when it is not a valid URL.
pub(crate) fn host_of(base_url: &str) -> Option<String> {
    Url::parse(base_url)
        .ok()
        .and_then(|url| url.host_str().map(str::to_owned))
}

/// True when the URL's host is `openrouter.ai` or one of its subdomains.
pub fn is_openrouter(base_url: &str) -> bool {
    host_of(base_url)
        .is_some_and(|host| host == "openrouter.ai" || host.ends_with(".openrouter.ai"))
}

#[cfg(test)]
mod tests {
    use tempfile::TempDir;
    use wiremock::matchers::{body_string_contains, header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::test_support::fx;

    fn fake_chunk() -> (AudioChunk, TempDir) {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("chunk.ogg");
        std::fs::write(&path, b"OggS fake audio bytes").unwrap();
        let chunk = AudioChunk {
            index: 7,
            start_s: 0.0,
            end_s: 1.0,
            path,
        };
        (chunk, dir)
    }

    /// A base URL on which nothing listens.
    fn dead_url() -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        drop(listener);
        url
    }

    #[test]
    fn new_builds_a_client() {
        let built = OpenAiCompatible::new(
            "http://127.0.0.1:1/v1".into(),
            ApiKey("sk-test".into()),
            "m".into(),
            RequestMode::Multipart,
        );
        assert!(built.is_ok());
    }

    fn dead_transcriber() -> OpenAiCompatible {
        OpenAiCompatible::new(
            dead_url(),
            ApiKey("sk-test".into()),
            "m".into(),
            RequestMode::Multipart,
        )
        .unwrap()
    }

    fn transcriber(server: &MockServer, mode: RequestMode) -> OpenAiCompatible {
        OpenAiCompatible::new(
            server.uri(),
            ApiKey("sk-test".into()),
            "x-ai/grok-stt-1.0".into(),
            mode,
        )
        .unwrap()
    }

    async fn setup(status: u16, body: serde_json::Value) -> (MockServer, OpenAiCompatible) {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/audio/transcriptions"))
            .respond_with(ResponseTemplate::new(status).set_body_json(body))
            .mount(&server)
            .await;
        let t = transcriber(&server, RequestMode::Multipart);
        (server, t)
    }

    #[tokio::test]
    async fn multipart_request_carries_required_fields() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/audio/transcriptions"))
            .and(header("authorization", "Bearer sk-test"))
            .and(body_string_contains("verbose_json"))
            .and(body_string_contains("timestamp_granularities[]"))
            .and(body_string_contains(
                "name=\"timestamp_granularities[]\"\r\n\r\nsegment",
            ))
            .and(body_string_contains(
                "name=\"timestamp_granularities[]\"\r\n\r\nword",
            ))
            .and(body_string_contains("chunk-007.ogg"))
            .and(body_string_contains("audio/ogg"))
            .and(body_string_contains(
                r#"{"options":{"xai":{"diarize":true}}}"#,
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(fx("grok_diar")))
            .expect(1)
            .mount(&server)
            .await;
        let t = transcriber(&server, RequestMode::Multipart);
        let opts = TranscribeOptions {
            provider_options: Some(serde_json::json!({"xai": {"diarize": true}})),
            ..Default::default()
        };
        let (chunk, _dir) = fake_chunk();
        let tr = t.transcribe(&chunk, &opts).await.unwrap();
        assert_eq!(tr.words[0].speaker, Some(0));
    }

    #[tokio::test]
    async fn optional_multipart_fields_are_absent_when_unset() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fx("whisper1")))
            .mount(&server)
            .await;
        let (chunk, _dir) = fake_chunk();
        transcriber(&server, RequestMode::Multipart)
            .transcribe(&chunk, &TranscribeOptions::default())
            .await
            .unwrap();
        let requests = server.received_requests().await.unwrap();
        let body = String::from_utf8_lossy(&requests[0].body).into_owned();
        for field in ["language", "prompt", "provider"] {
            assert!(
                !body.contains(&format!("name=\"{field}\"")),
                "{field} should be absent"
            );
        }
    }

    #[tokio::test]
    async fn multipart_includes_language_and_prompt_when_set() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/audio/transcriptions"))
            .and(body_string_contains("name=\"language\""))
            .and(body_string_contains("name=\"prompt\""))
            .and(body_string_contains("Bonjour"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fx("whisper1")))
            .expect(1)
            .mount(&server)
            .await;
        let t = transcriber(&server, RequestMode::Multipart);
        let opts = TranscribeOptions {
            language: Some("fr".into()),
            prompt: Some("Bonjour".into()),
            ..Default::default()
        };
        let (chunk, _dir) = fake_chunk();
        t.transcribe(&chunk, &opts).await.unwrap();
    }

    #[tokio::test]
    async fn json_base64_mode_sends_input_audio() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/audio/transcriptions"))
            .and(header("authorization", "Bearer sk-test"))
            .and(body_string_contains("\"input_audio\""))
            .and(body_string_contains("\"format\":\"ogg\""))
            .and(body_string_contains("\"response_format\":\"verbose_json\""))
            .and(body_string_contains("\"language\":\"fr\""))
            .and(body_string_contains(
                r#""timestamp_granularities":["segment","word"]"#,
            ))
            .and(body_string_contains(
                r#""provider":{"options":{"xai":{"diarize":true}}}"#,
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(fx("grok_diar")))
            .expect(1)
            .mount(&server)
            .await;
        let t = transcriber(&server, RequestMode::JsonBase64);
        let opts = TranscribeOptions {
            language: Some("fr".into()),
            provider_options: Some(serde_json::json!({"xai": {"diarize": true}})),
            ..Default::default()
        };
        let (chunk, _dir) = fake_chunk();
        t.transcribe(&chunk, &opts).await.unwrap();
    }

    #[tokio::test]
    async fn trailing_slash_in_base_url_is_ignored() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/audio/transcriptions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fx("whisper1")))
            .expect(1)
            .mount(&server)
            .await;
        let t = OpenAiCompatible::new(
            format!("{}/", server.uri()),
            ApiKey("sk-test".into()),
            "m".into(),
            RequestMode::Multipart,
        )
        .unwrap();
        let (chunk, _dir) = fake_chunk();
        t.transcribe(&chunk, &TranscribeOptions::default())
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn status_classification() {
        let (chunk, _dir) = fake_chunk();
        let opts = TranscribeOptions::default();

        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(429)
                    .insert_header("Retry-After", "3")
                    .set_body_string("slow down"),
            )
            .mount(&server)
            .await;
        let err = transcriber(&server, RequestMode::Multipart)
            .transcribe(&chunk, &opts)
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            TranscribeError::Retryable {
                status: Some(429),
                retry_after_s: Some(s),
                ..
            } if s == 3.0
        ));

        let (_s, t) = setup(503, serde_json::json!({})).await;
        let err = t.transcribe(&chunk, &opts).await.unwrap_err();
        assert!(matches!(
            err,
            TranscribeError::Retryable {
                status: Some(503),
                retry_after_s: None,
                ..
            }
        ));

        let (_s, t) = setup(408, serde_json::json!({})).await;
        let err = t.transcribe(&chunk, &opts).await.unwrap_err();
        assert!(matches!(
            err,
            TranscribeError::Retryable {
                status: Some(408),
                ..
            }
        ));

        for code in [401, 403] {
            let (_s, t) = setup(code, serde_json::json!({})).await;
            let err = t.transcribe(&chunk, &opts).await.unwrap_err();
            assert!(matches!(err, TranscribeError::Auth { status } if status == code));
        }

        let (_s, t) = setup(400, fx("gpt4o_error")).await;
        let err = t.transcribe(&chunk, &opts).await.unwrap_err();
        assert!(matches!(err, TranscribeError::ModelUnsupported { .. }));

        let (_s, t) = setup(
            404,
            serde_json::json!({"error": {"message": "no such model"}}),
        )
        .await;
        let err = t.transcribe(&chunk, &opts).await.unwrap_err();
        assert!(matches!(
            err,
            TranscribeError::Fatal {
                status: Some(404),
                ref message
            } if message.contains("no such model")
        ));
    }

    #[tokio::test]
    async fn fatal_message_is_truncated_to_300_chars() {
        let (chunk, _dir) = fake_chunk();
        let long = "x".repeat(1000);
        let (_s, t) = setup(422, serde_json::json!({"error": {"message": long}})).await;
        let err = t
            .transcribe(&chunk, &TranscribeOptions::default())
            .await
            .unwrap_err();
        match err {
            TranscribeError::Fatal { message, .. } => assert_eq!(message.chars().count(), 300),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn success_status_with_error_body_is_classified_by_parser() {
        let (chunk, _dir) = fake_chunk();
        let (_s, t) = setup(200, fx("gpt4o_error")).await;
        let err = t
            .transcribe(&chunk, &TranscribeOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(err, TranscribeError::ModelUnsupported { .. }));
    }

    #[tokio::test]
    async fn connection_failure_is_retryable_without_status() {
        let (chunk, _dir) = fake_chunk();
        let err = dead_transcriber()
            .transcribe(&chunk, &TranscribeOptions::default())
            .await
            .unwrap_err();
        assert!(matches!(
            err,
            TranscribeError::Retryable { status: None, .. }
        ));
    }

    #[tokio::test]
    async fn errors_never_contain_the_key() {
        let (chunk, _dir) = fake_chunk();
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(401)
                    .insert_header("x-echo-authorization", "Bearer sk-test")
                    .set_body_string("bad key sk-test"),
            )
            .mount(&server)
            .await;
        let err = transcriber(&server, RequestMode::Multipart)
            .transcribe(&chunk, &TranscribeOptions::default())
            .await
            .unwrap_err();
        assert!(!format!("{err}").contains("sk-test"));
        assert!(!format!("{err:?}").contains("sk-test"));

        // Provider bodies that echo the key are redacted in every error class.
        for (status, body) in [
            (503, r#"{"error":{"message":"bad header Bearer sk-test"}}"#),
            (422, r#"{"error":{"message":"bad header Bearer sk-test"}}"#),
            (500, "raw body with sk-test inside"),
            (
                400,
                r#"{"error":{"message":"verbose_json rejected for sk-test"}}"#,
            ),
            (200, r#"{"error":{"message":"echo sk-test","code":500}}"#),
        ] {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(status).set_body_string(body))
                .mount(&server)
                .await;
            let err = transcriber(&server, RequestMode::Multipart)
                .transcribe(&chunk, &TranscribeOptions::default())
                .await
                .unwrap_err();
            assert!(!format!("{err}").contains("sk-test"), "{status}: {err}");
            assert!(!format!("{err:?}").contains("sk-test"), "{status}: {err:?}");
        }
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .respond_with(
                ResponseTemplate::new(422)
                    .set_body_string(r#"{"error":{"message":"Bearer sk-test"}}"#),
            )
            .mount(&server)
            .await;
        let err = transcriber(&server, RequestMode::Multipart)
            .transcribe(&chunk, &TranscribeOptions::default())
            .await
            .unwrap_err();
        assert!(format!("{err}").contains("***"));

        // Transport failure path too.
        let err = dead_transcriber()
            .transcribe(&chunk, &TranscribeOptions::default())
            .await
            .unwrap_err();
        assert!(!format!("{err}").contains("sk-test"));
        assert!(!format!("{err:?}").contains("sk-test"));
    }

    #[test]
    fn detects_openrouter_hosts() {
        assert!(is_openrouter("https://openrouter.ai/api/v1"));
        assert!(is_openrouter("https://api.openrouter.ai/v1"));
        assert!(!is_openrouter("https://api.openai.com/v1"));
        assert!(!is_openrouter("https://notopenrouter.ai.example.com/v1"));
        assert!(!is_openrouter("not a url"));
    }
}
