use std::collections::BTreeMap;
use std::fmt;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::cli::Cli;
use crate::error::SrttyError;
use crate::paths;

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub default_provider: Option<String>,
    pub providers: BTreeMap<String, ProviderConfig>,
    pub subtitles: SubtitleConfig,
    pub chunking: ChunkingConfig,
    pub confirm_above_usd: f64,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ProviderKind {
    #[default]
    Api,
    Local,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RequestMode {
    #[default]
    Multipart,
    JsonBase64,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ProviderConfig {
    pub kind: ProviderKind,
    pub base_url: Option<String>,
    /// Name of the environment variable holding the API key.
    pub api_key_env: Option<String>,
    pub model: Option<String>,
    pub request_mode: RequestMode,
    pub model_path: Option<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SubtitleConfig {
    pub max_chars_per_line: usize,
    pub max_lines: usize,
    pub max_duration_s: f64,
    pub speaker_label: String,
}

impl Default for SubtitleConfig {
    fn default() -> Self {
        Self {
            max_chars_per_line: 42,
            max_lines: 2,
            max_duration_s: 7.0,
            speaker_label: "Speaker".into(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ChunkingConfig {
    pub target_s: f64,
    pub jobs: usize,
}

impl Default for ChunkingConfig {
    fn default() -> Self {
        Self {
            target_s: 300.0,
            jobs: 4,
        }
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            default_provider: None,
            providers: BTreeMap::new(),
            subtitles: SubtitleConfig::default(),
            chunking: ChunkingConfig::default(),
            confirm_above_usd: 1.0,
        }
    }
}

impl Config {
    /// Load the config file. A missing file yields the defaults.
    pub fn load(path: &Path) -> Result<Config, SrttyError> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Config::default()),
            Err(e) => {
                return Err(SrttyError::Config(format!(
                    "cannot read {}: {e}",
                    path.display()
                )));
            }
        };
        toml::from_str(&text).map_err(|e| {
            // The default rendering quotes the offending source line, which may
            // hold a pasted API key: report only the position and the message,
            // with any key-like word masked (type errors echo the value).
            let position = e
                .span()
                .map(|span| {
                    let before = text.get(..span.start).unwrap_or(&text);
                    let line = before.matches('\n').count() + 1;
                    let column = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
                    format!(" at line {line}, column {column}")
                })
                .unwrap_or_default();
            SrttyError::Config(format!(
                "invalid config {}{position}: {}",
                path.display(),
                redact_key_like(e.message())
            ))
        })
    }
}

/// An API key. Never printed: `Debug` and `Display` both show `***`.
#[derive(Clone)]
pub struct ApiKey(pub String);

impl ApiKey {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

impl fmt::Display for ApiKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("***")
    }
}

/// Whether the API key of the selected provider is available.
#[derive(Debug, Clone)]
pub enum KeyState {
    Set(ApiKey),
    /// The environment variable `var` is empty or absent.
    Unset {
        var: String,
    },
    /// The provider runs locally and takes no key.
    NotNeeded,
}

/// Settings after merging the config file, the environment and the CLI flags.
#[derive(Debug, Clone)]
pub struct Resolved {
    pub provider_name: String,
    pub provider: ProviderConfig,
    pub model: String,
    pub api_key: KeyState,
    pub language: Option<String>,
    pub prompt: Option<String>,
    pub target_s: f64,
    pub jobs: usize,
    pub subtitles: SubtitleConfig,
    pub confirm_above_usd: f64,
}

impl Resolved {
    /// The API key, or an error naming the variable that should hold it.
    pub fn require_key(&self) -> Result<ApiKey, SrttyError> {
        match &self.api_key {
            KeyState::Set(key) => Ok(key.clone()),
            KeyState::Unset { var } => Err(SrttyError::Config(format!(
                "environment variable {var} is not set"
            ))),
            KeyState::NotNeeded => Ok(ApiKey(String::new())),
        }
    }
}

/// Whether `s` starts with a well-known API key prefix (`sk-`, `gsk_`, `xai-`).
pub fn looks_like_api_key(s: &str) -> bool {
    ["sk-", "gsk_", "xai-"].iter().any(|p| s.starts_with(p))
}

/// Replaces every key-like word of `text` with `<redacted>`. Words are
/// separated by a space, a backtick, or a single or double quote.
pub fn redact_key_like(text: &str) -> String {
    let is_separator = |c: char| matches!(c, ' ' | '`' | '\'' | '"');
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while !rest.is_empty() {
        let sep = rest.find(is_separator).unwrap_or(rest.len());
        let (word, tail) = rest.split_at(sep);
        out.push_str(if looks_like_api_key(word) {
            "<redacted>"
        } else {
            word
        });
        let word_len = tail.chars().next().map_or(0, char::len_utf8);
        out.push_str(&tail[..word_len]);
        rest = &tail[word_len..];
    }
    out
}

/// Whether `name` matches `^[A-Za-z_][A-Za-z0-9_]*$`.
pub fn is_env_var_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Rejects an `api_key_env` that is not the name of an environment variable,
/// for instance a pasted key. The value itself is never echoed.
pub fn check_key_var_name(var: &str) -> Result<(), SrttyError> {
    if looks_like_api_key(var) || !is_env_var_name(var) {
        return Err(SrttyError::Config(
            "`api_key_env` must be the name of an environment variable (e.g. OPENROUTER_API_KEY), not the key itself".into(),
        ));
    }
    Ok(())
}

/// Merge settings. Precedence, lowest to highest: config file, environment
/// (`SRTTY_PROVIDER`, `SRTTY_MODEL`, `SRTTY_LANGUAGE`), CLI flags.
pub fn resolve(
    cfg: Config,
    cli: &Cli,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Resolved, SrttyError> {
    let env_value = |name: &str| env(name).filter(|v| !v.is_empty());

    let provider_name = cli
        .provider
        .clone()
        .or_else(|| env_value("SRTTY_PROVIDER"))
        .or_else(|| cfg.default_provider.clone())
        .ok_or_else(|| {
            SrttyError::Config(format!(
                "no provider configured; run `srtty setup` or edit {}",
                paths::config_file_from(env).display()
            ))
        })?;

    let provider = cfg.providers.get(&provider_name).cloned().ok_or_else(|| {
        SrttyError::Config(format!(
            "provider `{provider_name}` is not defined in the config file"
        ))
    })?;

    let model = cli
        .model
        .clone()
        .or_else(|| env_value("SRTTY_MODEL"))
        .or_else(|| provider.model.clone())
        .ok_or_else(|| {
            SrttyError::Config(format!(
                "no model configured for provider `{provider_name}`; pass --model or set `model` in the config file"
            ))
        })?;

    let api_key = match provider.kind {
        ProviderKind::Local => KeyState::NotNeeded,
        ProviderKind::Api => {
            let var = provider.api_key_env.as_deref().ok_or_else(|| {
                SrttyError::Config(format!(
                    "provider `{provider_name}` has no `api_key_env` in the config file"
                ))
            })?;
            check_key_var_name(var)?;
            match env_value(var) {
                Some(value) => KeyState::Set(ApiKey(value)),
                None => KeyState::Unset {
                    var: var.to_string(),
                },
            }
        }
    };

    Ok(Resolved {
        provider_name,
        provider,
        model,
        api_key,
        language: cli.language.clone().or_else(|| env_value("SRTTY_LANGUAGE")),
        prompt: cli.prompt.clone(),
        target_s: cli.chunk.unwrap_or(cfg.chunking.target_s),
        jobs: cli.jobs.unwrap_or(cfg.chunking.jobs),
        subtitles: cfg.subtitles,
        confirm_above_usd: cfg.confirm_above_usd,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::Cli;
    use clap::Parser;

    const TOML: &str = r#"
default_provider = "openrouter"
[providers.openrouter]
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
model = "x-ai/grok-stt-1.0"
[providers.groq]
base_url = "https://api.groq.com/openai/v1"
api_key_env = "GROQ_API_KEY"
model = "whisper-large-v3"
"#;

    fn env_with(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn defaults_match_spec() {
        let c = Config::default();
        assert_eq!(
            (c.subtitles.max_chars_per_line, c.subtitles.max_lines),
            (42, 2)
        );
        assert_eq!(c.subtitles.max_duration_s, 7.0);
        assert_eq!(c.subtitles.speaker_label, "Speaker");
        assert_eq!((c.chunking.target_s, c.chunking.jobs), (300.0, 4));
        assert_eq!(c.confirm_above_usd, 1.0);
    }

    #[test]
    fn flags_override_env_override_toml() {
        let cfg: Config = toml::from_str(TOML).unwrap();
        let cli = Cli::try_parse_from(["srtty", "a.mp4", "--model", "flag-model"]).unwrap();
        let env = env_with(&[
            ("SRTTY_PROVIDER", "groq"),
            ("SRTTY_MODEL", "env-model"),
            ("GROQ_API_KEY", "gsk-secret"),
        ]);
        let r = resolve(cfg, &cli, &env).unwrap();
        assert_eq!(r.provider_name, "groq");
        assert_eq!(r.model, "flag-model");
        assert_eq!(r.require_key().unwrap().expose(), "gsk-secret");
    }

    #[test]
    fn missing_key_names_the_variable_not_the_value() {
        let cfg: Config = toml::from_str(TOML).unwrap();
        let cli = Cli::try_parse_from(["srtty", "a.mp4"]).unwrap();
        let r = resolve(cfg, &cli, &env_with(&[])).unwrap();
        let err = r.require_key().unwrap_err().to_string();
        assert!(err.contains("OPENROUTER_API_KEY"));
    }

    #[test]
    fn key_pasted_into_api_key_env_is_rejected_without_echo() {
        let cfg: Config = toml::from_str(
            "default_provider = \"p\"\n[providers.p]\napi_key_env = \"sk-or-v1-0123secret\"\nmodel = \"m\"\n",
        )
        .unwrap();
        let cli = Cli::try_parse_from(["srtty", "a.mp4"]).unwrap();
        let err = resolve(cfg, &cli, &env_with(&[])).unwrap_err();
        assert!(matches!(err, SrttyError::Config(_)));
        let msg = err.to_string();
        assert!(!msg.contains("0123secret"), "{msg}");
        assert!(msg.contains("must be the name of an environment variable"));
    }

    #[test]
    fn api_key_env_accepts_variable_names_only() {
        for ok in ["OPENROUTER_API_KEY", "_x", "a1"] {
            assert!(is_env_var_name(ok), "{ok}");
        }
        for bad in ["", "1A", "sk-or-v1-abc", "A B", "É"] {
            assert!(!is_env_var_name(bad), "{bad}");
        }
    }

    #[test]
    fn toml_syntax_error_does_not_echo_the_source_line() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(
            &path,
            "[providers.p]\nmodel = \"m\"\napi_key_env = sk-or-v1-0123secret\n",
        )
        .unwrap();
        let msg = Config::load(&path).unwrap_err().to_string();
        assert!(!msg.contains("0123secret"), "{msg}");
        assert!(msg.contains("bad.toml") && msg.contains("line 3"), "{msg}");
    }

    #[test]
    fn api_key_never_formats() {
        let k = ApiKey("sk-or-v1-abc".into());
        assert_eq!(format!("{k:?} {k}"), "*** ***");
    }

    #[test]
    fn absent_file_gives_default() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(
            Config::load(&dir.path().join("none.toml"))
                .unwrap()
                .chunking
                .jobs,
            4
        );
    }

    #[test]
    fn invalid_toml_error_cites_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad.toml");
        std::fs::write(&path, "default_provider = [").unwrap();
        let err = Config::load(&path).unwrap_err();
        assert!(matches!(err, SrttyError::Config(_)));
        assert!(err.to_string().contains("bad.toml"));
    }

    #[test]
    fn provider_defaults_and_modes_parse() {
        let cfg: Config = toml::from_str(
            "[providers.p]\nrequest_mode = \"json-base64\"\n[providers.l]\nkind = \"local\"\n",
        )
        .unwrap();
        assert_eq!(cfg.providers["p"].kind, ProviderKind::Api);
        assert_eq!(cfg.providers["p"].request_mode, RequestMode::JsonBase64);
        assert_eq!(cfg.providers["l"].kind, ProviderKind::Local);
        assert_eq!(cfg.providers["l"].request_mode, RequestMode::Multipart);
    }

    #[test]
    fn no_provider_error_mentions_setup_and_config_path() {
        let cli = Cli::try_parse_from(["srtty", "a.mp4"]).unwrap();
        let env = env_with(&[("SRTTY_CONFIG", "/tmp/custom.toml")]);
        let err = resolve(Config::default(), &cli, &env)
            .unwrap_err()
            .to_string();
        assert!(err.contains("srtty setup") && err.contains("/tmp/custom.toml"));
    }

    #[test]
    fn key_pasted_into_api_key_env_is_rejected_for_every_prefix() {
        for key in [
            "gsk_0123secret",
            "sk-or-v1-0123secret",
            "sk-proj-0123secret",
            "xai-0123secret",
        ] {
            let cfg: Config = toml::from_str(&format!(
                "default_provider = \"p\"\n[providers.p]\napi_key_env = \"{key}\"\nmodel = \"m\"\n"
            ))
            .unwrap();
            let cli = Cli::try_parse_from(["srtty", "a.mp4"]).unwrap();
            let msg = resolve(cfg, &cli, &env_with(&[])).unwrap_err().to_string();
            assert!(!msg.contains("0123secret"), "{msg}");
            assert!(
                msg.contains("must be the name of an environment variable"),
                "{msg}"
            );
        }
    }

    #[test]
    fn unknown_fields_are_rejected() {
        for toml in [
            "defualt_provider = \"p\"",
            "[providers.p]\nmodle = \"m\"",
            "[subtitles]\nmax_line = 3",
            "[chunking]\njob = 2",
        ] {
            assert!(toml::from_str::<Config>(toml).is_err(), "{toml}");
        }
    }

    #[test]
    fn toml_type_error_does_not_echo_a_key_like_value() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("c.toml");
        std::fs::write(&path, "[providers.p]\nkind = \"gsk_0123secret\"\n").unwrap();
        let msg = Config::load(&path).unwrap_err().to_string();
        assert!(
            !msg.contains("0123secret") && msg.contains("<redacted>"),
            "{msg}"
        );
    }

    #[test]
    fn missing_key_is_reported_by_require_key_only() {
        let cfg: Config = toml::from_str(TOML).unwrap();
        let cli = Cli::try_parse_from(["srtty", "a.mp4"]).unwrap();
        let r = resolve(cfg, &cli, &env_with(&[])).unwrap();
        assert!(matches!(&r.api_key, KeyState::Unset { var } if var == "OPENROUTER_API_KEY"));
        assert!(
            r.require_key()
                .unwrap_err()
                .to_string()
                .contains("OPENROUTER_API_KEY")
        );
    }

    #[test]
    fn local_provider_needs_no_key() {
        let cfg: Config = toml::from_str(
            "default_provider = \"l\"\n[providers.l]\nkind = \"local\"\nmodel = \"m\"\n",
        )
        .unwrap();
        let cli = Cli::try_parse_from(["srtty", "a.mp4"]).unwrap();
        let r = resolve(cfg, &cli, &env_with(&[])).unwrap();
        assert!(matches!(r.api_key, KeyState::NotNeeded));
    }

    #[test]
    fn key_like_detection_and_redaction() {
        for key in ["sk-abc", "gsk_abc", "xai-abc"] {
            assert!(looks_like_api_key(key), "{key}");
        }
        for other in ["SK-abc", "GSK_abc", "OPENROUTER_API_KEY", "ask-x", ""] {
            assert!(!looks_like_api_key(other), "{other}");
        }
        assert_eq!(
            redact_key_like("invalid type: string \"gsk_0123\", expected kind 'sk-abc' `xai-1`"),
            "invalid type: string \"<redacted>\", expected kind '<redacted>' `<redacted>`"
        );
        assert_eq!(redact_key_like("nothing to hide"), "nothing to hide");
    }
}
