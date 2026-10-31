//! Provider presets and listing of their transcription models.

use std::time::Duration;

use serde_json::Value;

use crate::config::{ApiKey, redact_key_like};
use crate::error::SrttyError;
use crate::transcribe::openai::is_openrouter;

const LIST_TIMEOUT: Duration = Duration::from_secs(15);

/// A known provider, offered by `srtty setup`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Preset {
    pub key: &'static str,
    pub label: &'static str,
    pub base_url: &'static str,
    pub api_key_env: &'static str,
}

pub const PRESETS: [Preset; 3] = [
    Preset {
        key: "openrouter",
        label: "OpenRouter",
        base_url: "https://openrouter.ai/api/v1",
        api_key_env: "OPENROUTER_API_KEY",
    },
    Preset {
        key: "openai",
        label: "OpenAI",
        base_url: "https://api.openai.com/v1",
        api_key_env: "OPENAI_API_KEY",
    },
    Preset {
        key: "groq",
        label: "Groq",
        base_url: "https://api.groq.com/openai/v1",
        api_key_env: "GROQ_API_KEY",
    },
];

/// The preset named `key`, if any.
pub fn preset(key: &str) -> Option<&'static Preset> {
    PRESETS.iter().find(|p| p.key == key)
}

/// A transcription model found in a provider listing.
#[derive(Debug, Clone, PartialEq)]
pub struct ListedModel {
    pub id: String,
    /// Price per second of audio, when the listing states it.
    pub usd_per_s: Option<f64>,
    /// Billed per token, so the price per second is unknown.
    pub token_priced: bool,
    /// Model page, when the provider has one.
    pub url: Option<String>,
}

/// Format of a provider's `/models` listing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Listing {
    OpenRouter,
    OpenAi,
}

impl Listing {
    pub fn for_base_url(base_url: &str) -> Listing {
        if is_openrouter(base_url) {
            Listing::OpenRouter
        } else {
            Listing::OpenAi
        }
    }
}

fn price(v: &Value) -> Option<f64> {
    match v {
        Value::String(s) => s.trim().parse().ok(),
        Value::Number(n) => n.as_f64(),
        _ => None,
    }
}

/// Transcription models of an OpenRouter listing, sorted by id.
pub fn parse_openrouter_models(v: &Value) -> Vec<ListedModel> {
    let mut models: Vec<ListedModel> = v["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let id = entry["id"].as_str()?;
            let transcribes = entry["architecture"]["output_modalities"]
                .as_array()?
                .iter()
                .any(|m| m.as_str() == Some("transcription"));
            if !transcribes {
                return None;
            }
            let pricing = &entry["pricing"];
            let per_second = price(&pricing["completion"]) == Some(0.0);
            let usd_per_s = if per_second {
                price(&pricing["prompt"]).filter(|p| p.is_finite() && *p >= 0.0)
            } else {
                None
            };
            Some(ListedModel {
                id: id.to_string(),
                usd_per_s,
                token_priced: !per_second,
                url: Some(format!("https://openrouter.ai/{id}")),
            })
        })
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models
}

/// Speech models of an OpenAI-style listing, sorted by id.
pub fn parse_openai_models(v: &Value) -> Vec<ListedModel> {
    let mut models: Vec<ListedModel> = v["data"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|entry| entry["id"].as_str())
        .filter(|id| {
            let lower = id.to_lowercase();
            ["whisper", "transcribe", "stt", "asr"]
                .iter()
                .any(|needle| lower.contains(needle))
        })
        .map(|id| ListedModel {
            id: id.to_string(),
            usd_per_s: None,
            token_priced: false,
            url: None,
        })
        .collect();
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models
}

/// GETs `url` and parses the JSON body. Errors never include the response body.
async fn get_json(url: &str, key: Option<&ApiKey>, what: &str) -> Result<Value, SrttyError> {
    let fail = |detail: String| SrttyError::Network(format!("{what} failed: {detail}"));
    let transport = |e: reqwest::Error| fail(redact_key_like(&e.without_url().to_string()));
    let client = reqwest::Client::builder()
        .timeout(LIST_TIMEOUT)
        .build()
        .map_err(transport)?;
    let mut request = client.get(url);
    if let Some(key) = key {
        request = request.bearer_auth(key.expose());
    }
    let resp = request.send().await.map_err(transport)?;
    let status = resp.status();
    if matches!(status.as_u16(), 401 | 403) {
        return Err(fail(format!(
            "authentication rejected (HTTP {})",
            status.as_u16()
        )));
    }
    if !status.is_success() {
        return Err(fail(format!("HTTP {}", status.as_u16())));
    }
    resp.json::<Value>()
        .await
        .map_err(|_| fail("response is not valid JSON".to_string()))
}

/// Lists the transcription models of the provider at `base_url`.
pub async fn list_models(
    listing: Listing,
    base_url: &str,
    key: Option<&ApiKey>,
) -> Result<Vec<ListedModel>, SrttyError> {
    let base = base_url.trim_end_matches('/');
    match listing {
        Listing::OpenRouter => {
            let url = format!("{base}/models?output_modalities=transcription");
            let v = get_json(&url, None, "listing models").await?;
            Ok(parse_openrouter_models(&v))
        }
        Listing::OpenAi => {
            let v = get_json(&format!("{base}/models"), key, "listing models").await?;
            Ok(parse_openai_models(&v))
        }
    }
}

/// Provider tags serving `model` on OpenRouter.
pub async fn endpoint_tags(base_url: &str, model: &str) -> Result<Vec<String>, SrttyError> {
    let base = base_url.trim_end_matches('/');
    let v = get_json(
        &format!("{base}/models/{model}/endpoints"),
        None,
        "listing endpoints",
    )
    .await?;
    Ok(v["data"]["endpoints"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["tag"].as_str().map(str::to_owned))
        .collect())
}

#[cfg(test)]
mod tests {
    use serde_json::json;
    use wiremock::matchers::{header, method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::test_support::fx;

    #[test]
    fn presets_are_found_by_key() {
        assert_eq!(PRESETS.len(), 3);
        let groq = preset("groq").unwrap();
        assert_eq!(groq.base_url, "https://api.groq.com/openai/v1");
        assert_eq!(groq.api_key_env, "GROQ_API_KEY");
        assert_eq!(preset("openrouter").unwrap().label, "OpenRouter");
        assert!(preset("nope").is_none());
    }

    #[test]
    fn listing_follows_the_base_url() {
        assert_eq!(
            Listing::for_base_url("https://openrouter.ai/api/v1"),
            Listing::OpenRouter
        );
        assert_eq!(
            Listing::for_base_url("https://api.openai.com/v1"),
            Listing::OpenAi
        );
    }

    #[test]
    fn openrouter_listing_prices_per_second_and_skips_token_priced() {
        let models = parse_openrouter_models(&fx("openrouter_models"));
        let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "google/gemini-3.5-transcribe",
                "microsoft/mai-transcribe-1.5",
                "openai/whisper-large-v3-turbo",
                "x-ai/grok-stt-1.0"
            ]
        );
        let grok = models.iter().find(|m| m.id == "x-ai/grok-stt-1.0").unwrap();
        assert!((grok.usd_per_s.unwrap() * 3600.0 - 0.10).abs() < 1e-6);
        assert_eq!(models[0].usd_per_s, None); // gemini: billed per token
        assert!(models[0].token_priced && !grok.token_priced);
        assert_eq!(
            models[0].url.as_deref(),
            Some("https://openrouter.ai/google/gemini-3.5-transcribe")
        );
        // "0.36" $/s is upstream data (OpenRouter), kept as is.
        assert_eq!(models[1].usd_per_s, Some(0.36));
    }

    #[test]
    fn invalid_per_second_prices_are_dropped() {
        let entry = |id: &str, prompt: &str| {
            json!({"id": id, "architecture": {"output_modalities": ["transcription"]},
                   "pricing": {"prompt": prompt, "completion": "0"}})
        };
        let v = json!({"data": [entry("a/neg", "-1"), entry("a/nan", "nan"),
                                entry("a/inf", "inf"), entry("a/abc", "abc"),
                                entry("a/ok", "0.001")]});
        let models = parse_openrouter_models(&v);
        let by_id = |id: &str| models.iter().find(|m| m.id == id).unwrap();
        for id in ["a/neg", "a/nan", "a/inf", "a/abc"] {
            assert_eq!(by_id(id).usd_per_s, None, "{id}");
            assert!(!by_id(id).token_priced, "{id}");
        }
        assert_eq!(by_id("a/ok").usd_per_s, Some(0.001));
    }

    #[test]
    fn openai_listing_keeps_speech_models_only() {
        let v = json!({"data": [{"id": "gpt-4o"}, {"id": "whisper-1"}, {"id": "gpt-4o-mini-transcribe"},
                                {"id": "distil-whisper-large-v3-en"}, {"id": "tts-1"}]});
        let models = parse_openai_models(&v);
        assert!(
            models
                .iter()
                .all(|m| m.usd_per_s.is_none() && !m.token_priced && m.url.is_none())
        );
        let ids: Vec<_> = models.into_iter().map(|m| m.id).collect();
        assert_eq!(
            ids,
            [
                "distil-whisper-large-v3-en",
                "gpt-4o-mini-transcribe",
                "whisper-1"
            ]
        );
    }

    #[tokio::test]
    async fn openai_listing_sends_the_key_and_hides_it_on_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .and(header("authorization", "Bearer sk-test-4242"))
            .respond_with(ResponseTemplate::new(200).set_body_json(
                json!({"data": [{"id": "whisper-1"}, {"id": "gpt-4o"}, {"id": "gsk-asr-1"}]}),
            ))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(ResponseTemplate::new(401).set_body_string("bad key sk-test-4242"))
            .mount(&server)
            .await;
        let base = format!("{}/v1", server.uri());

        let key = ApiKey("sk-test-4242".to_string());
        let models = list_models(Listing::OpenAi, &base, Some(&key))
            .await
            .unwrap();
        let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["gsk-asr-1", "whisper-1"]);

        let err = list_models(Listing::OpenAi, &base, None).await.unwrap_err();
        let SrttyError::Network(msg) = &err else {
            panic!("expected Network, got {err:?}");
        };
        assert_eq!(
            msg,
            "listing models failed: authentication rejected (HTTP 401)"
        );
        assert!(!msg.contains("sk-test-4242"));
    }

    #[tokio::test]
    async fn openrouter_listing_needs_no_key_and_asks_for_transcription() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/models"))
            .and(query_param("output_modalities", "transcription"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fx("openrouter_models")))
            .mount(&server)
            .await;
        let base = format!("{}/api/v1", server.uri());
        let key = ApiKey("sk-test-4242".to_string());
        let models = list_models(Listing::OpenRouter, &base, Some(&key))
            .await
            .unwrap();
        assert_eq!(models.len(), 4);
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests.len(), 1);
        assert!(!requests[0].headers.contains_key("authorization"));
    }

    #[tokio::test]
    async fn http_500_and_dead_port_are_network_errors() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(500).set_body_string("secret body"))
            .mount(&server)
            .await;
        let base = format!("{}/v1", server.uri());
        let err = list_models(Listing::OpenAi, &base, None).await.unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert_eq!(err.to_string(), "listing models failed: HTTP 500");

        let dead = list_models(Listing::OpenAi, "http://127.0.0.1:1/v1", None)
            .await
            .unwrap_err();
        assert_eq!(dead.exit_code(), 2);
        assert!(dead.to_string().starts_with("listing models failed: "));
    }

    #[tokio::test]
    async fn endpoint_tags_reads_the_fixture() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/v1/models/x-ai/grok-stt-1.0/endpoints"))
            .respond_with(ResponseTemplate::new(200).set_body_json(fx("openrouter_endpoints_grok")))
            .mount(&server)
            .await;
        let base = format!("{}/api/v1", server.uri());
        let tags = endpoint_tags(&base, "x-ai/grok-stt-1.0").await.unwrap();
        assert_eq!(tags, vec!["xai".to_string()]);
    }
}
