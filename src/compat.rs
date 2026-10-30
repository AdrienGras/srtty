//! Model compatibility list: an embedded seed, optionally replaced by a fresher
//! copy in the cache directory.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::transcribe::openai::{host_of, is_openrouter};

/// The only `compat.json` schema this build understands.
pub const COMPAT_SCHEMA: u32 = 1;

/// Where the maintained list lives.
pub const COMPAT_URL: &str = "https://raw.githubusercontent.com/AdrienGras/srtty/main/compat.json";

/// How long a refresh may take before falling back to the local copy.
pub const REFRESH_TIMEOUT: Duration = Duration::from_secs(3);

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
        load_with_source(cache_dir).0
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

/// `SRTTY_COMPAT_URL` when set and non-empty, else `COMPAT_URL`.
pub fn compat_url() -> String {
    compat_url_from(&|k| std::env::var(k).ok())
}

/// Same as `compat_url`, reading variables through `get`.
pub fn compat_url_from(get: &dyn Fn(&str) -> Option<String>) -> String {
    match get("SRTTY_COMPAT_URL") {
        Some(v) if !v.is_empty() => v,
        _ => COMPAT_URL.to_string(),
    }
}

/// Short cause for a failed request, without the URL.
fn describe(e: reqwest::Error) -> String {
    if e.is_timeout() {
        "timed out".to_string()
    } else if e.is_connect() {
        "connection failed".to_string()
    } else {
        e.without_url().to_string()
    }
}

/// Which copy of the list is in use.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CompatSource {
    Remote,
    Cache,
    Embedded,
}

impl CompatSource {
    pub fn name(self) -> &'static str {
        match self {
            CompatSource::Remote => "remote",
            CompatSource::Cache => "cache",
            CompatSource::Embedded => "embedded",
        }
    }
}

/// Result of a refresh attempt: the list to use and an optional warning.
#[derive(Debug, Clone)]
pub struct Refreshed {
    pub list: CompatList,
    pub source: CompatSource,
    pub warning: Option<String>,
}

/// The cached list when valid, else the embedded one, with its origin.
pub fn load_with_source(cache_dir: &Path) -> (CompatList, CompatSource) {
    match std::fs::read_to_string(cache_dir.join("compat.json"))
        .ok()
        .and_then(|text| parse_compat(&text).ok())
    {
        Some(list) => (list, CompatSource::Cache),
        None => (CompatList::embedded(), CompatSource::Embedded),
    }
}

async fn fetch_remote(url: &str, timeout: Duration) -> Result<(CompatList, String), String> {
    let client = reqwest::Client::builder()
        .timeout(timeout)
        .build()
        .map_err(describe)?;
    let resp = client.get(url).send().await.map_err(describe)?;
    let status = resp.status();
    if !status.is_success() {
        return Err(format!("HTTP {}", status.as_u16()));
    }
    let body = resp.text().await.map_err(describe)?;
    let list = parse_compat(&body).map_err(|e| format!("invalid list: {e}"))?;
    Ok((list, body))
}

/// Downloads `url` and stores it as `<cache_dir>/compat.json`. A failed
/// download (network, status, parse) keeps the local copy and yields a
/// warning; a failed cache write keeps the download and yields a warning.
pub async fn refresh(cache_dir: &Path, url: &str, timeout: Duration) -> Refreshed {
    let cause = match fetch_remote(url, timeout).await {
        Ok((list, body)) => {
            // A cache that cannot be written does not make the download stale.
            let warning =
                crate::fsutil::write_atomic(&cache_dir.join("compat.json"), body.as_bytes())
                    .err()
                    .map(|e| format!("could not write the cache: {e}"));
            return Refreshed {
                list,
                source: CompatSource::Remote,
                warning,
            };
        }
        Err(e) => e,
    };
    let (list, source) = load_with_source(cache_dir);
    let warning = Some(format!(
        "could not refresh the compatibility list ({cause}); using the {} copy from {}",
        source.name(),
        list.generated_at
    ));
    Refreshed {
        list,
        source,
        warning,
    }
}

/// How a model is presented in `srtty models` and `srtty setup`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum ModelStatus {
    CompatibleSpeakers,
    Compatible,
    Unknown,
    Incompatible,
}

impl ModelStatus {
    pub fn label(&self) -> &'static str {
        match self {
            ModelStatus::CompatibleSpeakers => "compatible + speakers",
            ModelStatus::Compatible => "compatible",
            ModelStatus::Unknown => "unknown",
            ModelStatus::Incompatible => "incompatible",
        }
    }
}

/// Display status of a model; `None` (not in the list) is `Unknown`.
pub fn status_of(entry: Option<&CompatEntry>) -> ModelStatus {
    match entry {
        None => ModelStatus::Unknown,
        Some(e) => match e.status {
            CompatStatus::Unusable | CompatStatus::Incompatible => ModelStatus::Incompatible,
            CompatStatus::Compatible if e.diarization.is_some() => ModelStatus::CompatibleSpeakers,
            CompatStatus::Compatible => ModelStatus::Compatible,
        },
    }
}

/// `YYYY-MM-DD` (UTC) for a Unix timestamp (Howard Hinnant's `civil_from_days`).
pub fn civil_date(unix_secs: i64) -> String {
    let z = unix_secs.div_euclid(86_400) + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Today's date in UTC as `YYYY-MM-DD`.
pub fn utc_today() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    civil_date(secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seed_list_knows_grok_diarization() {
        let l = crate::test_support::compat_seed();
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
        let l = crate::test_support::compat_seed();
        let e = l.find("openrouter", "x-ai/grok-stt-1.0");
        assert!((estimate_cost_usd(e, 1800.0).unwrap() - 0.05).abs() < 1e-9);
        assert_eq!(estimate_cost_usd(None, 1800.0), None);
    }

    #[test]
    fn embedded_list_is_well_formed() {
        // `compat.json` is regenerated weekly: assert only properties that
        // survive a regeneration, never specific models or prices.
        let l = CompatList::embedded();
        assert_eq!(l.schema, COMPAT_SCHEMA);
        assert!(!l.models.is_empty());
        let mut seen = std::collections::HashSet::new();
        for m in &l.models {
            assert!(
                seen.insert((m.provider.clone(), m.id.clone())),
                "duplicate entry {}/{}",
                m.provider,
                m.id
            );
            if m.status == CompatStatus::Compatible {
                assert!(
                    m.words || m.fine_segments,
                    "compatible entry {}/{} has neither words nor fine_segments",
                    m.provider,
                    m.id
                );
            }
            if let Some(d) = &m.diarization {
                assert!(
                    d.is_object(),
                    "diarization of {}/{} is not an object",
                    m.provider,
                    m.id
                );
            }
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

    use std::time::Duration;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[tokio::test]
    async fn refresh_saves_a_valid_remote_copy() {
        let server = MockServer::start().await;
        let body = r#"{"schema":1,"generated_at":"2026-11-02","models":[]}"#;
        Mock::given(method("GET"))
            .and(path("/compat.json"))
            .respond_with(ResponseTemplate::new(200).set_body_string(body))
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let r = refresh(
            dir.path(),
            &format!("{}/compat.json", server.uri()),
            REFRESH_TIMEOUT,
        )
        .await;
        assert_eq!(r.source, CompatSource::Remote);
        assert!(r.warning.is_none());
        assert!(r.list.models.is_empty());
        assert_eq!(
            std::fs::read_to_string(dir.path().join("compat.json")).unwrap(),
            body
        );
    }

    #[tokio::test]
    async fn refresh_keeps_cache_on_bad_remote() {
        let cached = r#"{"schema":1,"generated_at":"cached-copy","models":[]}"#;
        let slow = ResponseTemplate::new(200)
            .set_body_string(r#"{"schema":1,"generated_at":"slow","models":[]}"#)
            .set_delay(Duration::from_secs(5));
        let cases: Vec<(&str, ResponseTemplate)> = vec![
            ("404", ResponseTemplate::new(404)),
            (
                "html",
                ResponseTemplate::new(200).set_body_string("<html>nope</html>"),
            ),
            (
                "schema 2",
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"schema":2,"generated_at":"x","models":[]}"#),
            ),
            ("slow", slow),
        ];
        let expected_cause = |name: &str| match name {
            "404" => "HTTP 404",
            "slow" => "timed out",
            _ => "invalid list",
        };
        for (name, template) in cases {
            let server = MockServer::start().await;
            Mock::given(method("GET"))
                .and(path("/compat.json"))
                .respond_with(template)
                .mount(&server)
                .await;
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("compat.json"), cached).unwrap();
            let r = refresh(
                dir.path(),
                &format!("{}/compat.json", server.uri()),
                Duration::from_secs(1),
            )
            .await;
            assert_eq!(r.source, CompatSource::Cache, "{name}");
            let w = r.warning.expect(name);
            assert!(w.contains("could not refresh"), "{name}: {w}");
            assert!(w.contains("cached-copy"), "{name}: {w}");
            assert!(w.contains(expected_cause(name)), "{name}: {w}");
            assert_eq!(
                std::fs::read_to_string(dir.path().join("compat.json")).unwrap(),
                cached,
                "{name}"
            );
        }
    }

    #[tokio::test]
    async fn refresh_falls_back_when_the_port_is_closed() {
        let port = {
            let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            l.local_addr().unwrap().port()
        };
        let dir = tempfile::tempdir().unwrap();
        let cached = r#"{"schema":1,"generated_at":"cached-copy","models":[]}"#;
        std::fs::write(dir.path().join("compat.json"), cached).unwrap();
        let r = refresh(
            dir.path(),
            &format!("http://127.0.0.1:{port}/compat.json"),
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(r.source, CompatSource::Cache);
        let w = r.warning.unwrap();
        assert!(w.contains("could not refresh"), "{w}");
        assert!(w.contains("connection failed"), "{w}");
        assert!(!w.contains("127.0.0.1"), "{w}");
        assert_eq!(
            std::fs::read_to_string(dir.path().join("compat.json")).unwrap(),
            cached
        );
    }

    #[tokio::test]
    async fn refresh_without_cache_reports_the_embedded_copy() {
        let dir = tempfile::tempdir().unwrap();
        let r = refresh(
            dir.path(),
            "http://127.0.0.1:1/compat.json",
            Duration::from_secs(1),
        )
        .await;
        assert_eq!(r.source, CompatSource::Embedded);
        let w = r.warning.unwrap();
        assert!(w.contains("the embedded copy from"), "{w}");
        assert!(!dir.path().join("compat.json").exists());
    }

    #[test]
    fn load_with_source_reports_origin() {
        let d = tempfile::tempdir().unwrap();
        assert_eq!(load_with_source(d.path()).1, CompatSource::Embedded);
        std::fs::write(
            d.path().join("compat.json"),
            r#"{"schema":1,"generated_at":"x","models":[]}"#,
        )
        .unwrap();
        assert_eq!(load_with_source(d.path()).1, CompatSource::Cache);
    }

    #[test]
    fn compat_url_honours_the_override() {
        assert_eq!(compat_url_from(&|_| None), COMPAT_URL);
        assert_eq!(compat_url_from(&|_| Some(String::new())), COMPAT_URL);
        assert_eq!(
            compat_url_from(&|k| (k == "SRTTY_COMPAT_URL").then(|| "http://x/y.json".to_string())),
            "http://x/y.json"
        );
    }

    #[tokio::test]
    async fn refresh_turns_a_cache_write_failure_into_a_warning() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/compat.json"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_string(r#"{"schema":1,"generated_at":"2026-11-02","models":[]}"#),
            )
            .mount(&server)
            .await;
        let dir = tempfile::tempdir().unwrap();
        let blocker = dir.path().join("not-a-dir");
        std::fs::write(&blocker, "x").unwrap();
        let r = refresh(
            &blocker.join("cache"),
            &format!("{}/compat.json", server.uri()),
            REFRESH_TIMEOUT,
        )
        .await;
        // The downloaded list is used even though it could not be saved.
        assert_eq!(r.source, CompatSource::Remote);
        assert_eq!(r.list.generated_at, "2026-11-02");
        let w = r.warning.unwrap();
        assert!(w.starts_with("could not write the cache: "), "{w}");
        assert!(!w.contains("could not refresh"), "{w}");
    }

    #[test]
    fn status_mapping() {
        let l = crate::test_support::compat_seed();
        assert_eq!(
            status_of(l.find("openrouter", "x-ai/grok-stt-1.0")),
            ModelStatus::CompatibleSpeakers
        );
        assert_eq!(
            status_of(l.find("openrouter", "openai/whisper-1")),
            ModelStatus::Compatible
        );
        assert_eq!(
            status_of(l.find("openrouter", "assemblyai/universal-3-6-pro")),
            ModelStatus::Incompatible
        );
        assert_eq!(
            status_of(l.find("openrouter", "google/chirp-3")),
            ModelStatus::Incompatible
        );
        assert_eq!(status_of(None), ModelStatus::Unknown);
        assert_eq!(
            ModelStatus::CompatibleSpeakers.label(),
            "compatible + speakers"
        );
        assert_eq!(
            serde_json::to_string(&ModelStatus::CompatibleSpeakers).unwrap(),
            "\"compatible-speakers\""
        );
        assert_eq!(
            serde_json::to_string(&CompatSource::Remote).unwrap(),
            "\"remote\""
        );
    }

    #[test]
    fn civil_dates() {
        assert_eq!(civil_date(0), "1970-01-01");
        assert_eq!(civil_date(1_793_145_600), "2026-10-28");
        assert_eq!(civil_date(1_793_235_111), "2026-10-29");
        assert_eq!(civil_date(951_782_400), "2000-02-29");
        assert_eq!(civil_date(-86_400), "1969-12-31");
        assert_eq!(utc_today().len(), 10);
    }
}
