//! Model compatibility list: an embedded seed, optionally replaced by a fresher
//! copy in the cache directory.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::transcribe::openai::{host_of, is_openrouter};

/// The only `compat.json` schema this build understands.
pub const COMPAT_SCHEMA: u32 = 1;

const EMBEDDED: &str = include_str!("../compat.json");

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CompatStatus {
    Compatible,
    Incompatible,
    Unusable,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompatEntry {
    pub provider: String,
    pub id: String,
    pub status: CompatStatus,
    pub words: bool,
    pub fine_segments: bool,
    pub diarization: Option<serde_json::Value>,
    pub usd_per_hour: Option<f64>,
    pub checked_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompatList {
    pub schema: u32,
    pub generated_at: String,
    pub models: Vec<CompatEntry>,
}

impl CompatList {
    /// The list compiled into the binary.
    pub fn embedded() -> Self {
        serde_json::from_str(EMBEDDED).expect("embedded compat.json is valid")
    }

    /// `<cache_dir>/compat.json` when it exists and parses, else the embedded list.
    pub fn load(cache_dir: &Path) -> Self {
        std::fs::read_to_string(cache_dir.join("compat.json"))
            .ok()
            .and_then(|text| parse_compat(&text).ok())
            .unwrap_or_else(Self::embedded)
    }

    pub fn find(&self, provider: &str, model: &str) -> Option<&CompatEntry> {
        self.models
            .iter()
            .find(|m| m.provider == provider && m.id == model)
    }
}

/// Parses a `compat.json` document, refusing any schema but `COMPAT_SCHEMA`.
pub fn parse_compat(text: &str) -> Result<CompatList, String> {
    let list: CompatList = serde_json::from_str(text).map_err(|e| e.to_string())?;
    if list.schema != COMPAT_SCHEMA {
        return Err(format!("unsupported compat.json schema {}", list.schema));
    }
    Ok(list)
}

/// Maps a base URL to the provider key used in the compatibility list.
pub fn provider_key(base_url: &str) -> Option<&'static str> {
    if is_openrouter(base_url) {
        return Some("openrouter");
    }
    match host_of(base_url)?.as_str() {
        "api.openai.com" => Some("openai"),
        "api.groq.com" => Some("groq"),
        _ => None,
    }
}

/// Estimated price in USD for `duration_s` seconds of audio.
pub fn estimate_cost_usd(entry: Option<&CompatEntry>, duration_s: f64) -> Option<f64> {
    entry?.usd_per_hour.map(|usd| usd * duration_s / 3600.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_list_parses_and_knows_grok_diarization() {
        let l = CompatList::embedded();
        let g = l.find("openrouter", "x-ai/grok-stt-1.0").unwrap();
        assert_eq!(g.status, CompatStatus::Compatible);
        assert_eq!(
            g.diarization,
            Some(serde_json::json!({"xai": {"diarize": true}}))
        );
        assert_eq!(
            l.find("openrouter", "assemblyai/universal-3-6-pro")
                .unwrap()
                .status,
            CompatStatus::Unusable
        );
    }

    #[test]
    fn cache_copy_wins_and_corrupt_cache_falls_back() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("compat.json"),
            r#"{"schema":1,"generated_at":"x","models":[]}"#,
        )
        .unwrap();
        assert!(CompatList::load(d.path()).models.is_empty());
        std::fs::write(d.path().join("compat.json"), "{oops").unwrap();
        assert!(!CompatList::load(d.path()).models.is_empty());
    }

    #[test]
    fn provider_keys() {
        assert_eq!(
            provider_key("https://openrouter.ai/api/v1"),
            Some("openrouter")
        );
        assert_eq!(provider_key("https://api.groq.com/openai/v1"), Some("groq"));
        assert_eq!(provider_key("http://localhost:8000/v1"), None);
    }

    #[test]
    fn cost_estimate() {
        let l = CompatList::embedded();
        let e = l.find("openrouter", "x-ai/grok-stt-1.0");
        assert!((estimate_cost_usd(e, 1800.0).unwrap() - 0.05).abs() < 1e-9);
        assert_eq!(estimate_cost_usd(None, 1800.0), None);
    }

    #[test]
    fn embedded_entries_have_unique_provider_and_id() {
        let l = CompatList::embedded();
        let mut seen = std::collections::HashSet::new();
        for m in &l.models {
            assert!(
                seen.insert((m.provider.clone(), m.id.clone())),
                "duplicate entry {}/{}",
                m.provider,
                m.id
            );
        }
    }

    #[test]
    fn provider_key_handles_ports_subdomains_and_lookalikes() {
        assert_eq!(
            provider_key("https://api.openai.com:443/v1"),
            Some("openai")
        );
        assert_eq!(
            provider_key("https://api.openrouter.ai/v1"),
            Some("openrouter")
        );
        assert_eq!(provider_key("https://notopenrouter.ai/v1"), None);
        assert_eq!(provider_key("https://openrouter.ai.evil.com/v1"), None);
        assert_eq!(provider_key("not a url"), None);
    }

    #[test]
    fn load_rejects_other_schemas() {
        let d = tempfile::tempdir().unwrap();
        std::fs::write(
            d.path().join("compat.json"),
            r#"{"schema":2,"generated_at":"x","models":[]}"#,
        )
        .unwrap();
        assert!(!CompatList::load(d.path()).models.is_empty());
    }

    #[test]
    fn parse_compat_rejects_schema_2() {
        let err = parse_compat(r#"{"schema":2,"generated_at":"x","models":[]}"#).unwrap_err();
        assert_eq!(err, "unsupported compat.json schema 2");
        let ok = parse_compat(r#"{"schema":1,"generated_at":"x","models":[]}"#).unwrap();
        assert!(ok.models.is_empty());
        assert!(parse_compat("{oops").is_err());
    }
}
