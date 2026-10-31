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
    /// Chunks transcribed at once; `None` picks a default from the backend and the machine.
    pub jobs: Option<usize>,
}

impl Default for ChunkingConfig {
    fn default() -> Self {
        Self {
            target_s: 300.0,
            jobs: None,
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
            SrttyError::Config(format!(
                "invalid config {}{}: {}",
                path.display(),
                toml_error_position(&text, e.span()),
                redact_key_like(e.message())
            ))
        })
    }
}

/// ` at line L, column C` for the start of `span` in `text`, or nothing
/// without a span. A TOML error is reported by its position only: the source
/// line may hold a pasted API key.
pub fn toml_error_position(text: &str, span: Option<std::ops::Range<usize>>) -> String {
    span.map(|span| {
        let before = text.get(..span.start).unwrap_or(text);
        let line = before.matches('\n').count() + 1;
        let column = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
        format!(" at line {line}, column {column}")
    })
    .unwrap_or_default()
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
    pub jobs: Option<usize>,
    /// The whisper model file of a local provider; `None` for an API provider.
    pub model_path: Option<PathBuf>,
    pub subtitles: SubtitleConfig,
    pub confirm_above_usd: f64,
}

/// What a run transcribes with, once its settings are checked.
#[derive(Debug, Clone)]
pub enum Backend {
    Api {
        base_url: String,
        key: ApiKey,
        mode: RequestMode,
    },
    Local {
        model_path: PathBuf,
    },
}

impl Resolved {
    /// The backend of the selected provider, or the config error that stops the run:
    /// an unset key variable, or a missing `base_url`.
    pub fn backend(&self) -> Result<Backend, SrttyError> {
        match (self.provider.kind, &self.api_key, &self.model_path) {
            (ProviderKind::Local, _, Some(model_path)) => Ok(Backend::Local {
                model_path: model_path.clone(),
            }),
            (ProviderKind::Local, ..) => Err(missing_model_path(&self.provider_name)),
            (ProviderKind::Api, KeyState::Unset { var }, _) => Err(SrttyError::Config(format!(
                "environment variable {var} is not set"
            ))),
            (ProviderKind::Api, KeyState::Set(key), _) => {
                let base_url = self.provider.base_url.clone().ok_or_else(|| {
                    SrttyError::Config(format!(
                        "provider `{}` has no `base_url` in the config file",
                        self.provider_name
                    ))
                })?;
                Ok(Backend::Api {
                    base_url,
                    key: key.clone(),
                    mode: self.provider.request_mode,
                })
            }
            // `resolve` gives an API provider a key state of its own.
            (ProviderKind::Api, KeyState::NotNeeded, _) => Err(SrttyError::Config(format!(
                "provider `{}` has no `api_key_env` in the config file",
                self.provider_name
            ))),
        }
    }
}

fn missing_model_path(provider_name: &str) -> SrttyError {
    SrttyError::Config(format!(
        "provider `{provider_name}` has no `model_path`; run `srtty setup` or pass --model <path to a ggml model>"
    ))
}

/// Chunks transcribed at once, and whisper threads for each of them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Parallelism {
    pub jobs: usize,
    /// Threads of one local chunk; `None` for an API provider.
    pub threads_per_job: Option<usize>,
}

/// Chunks in flight by default with an API provider.
pub const DEFAULT_API_JOBS: usize = 4;

/// Whether this build runs whisper on a GPU (Metal on macOS).
pub const GPU_BUILD: bool = cfg!(all(feature = "local", target_os = "macos"));

/// Cores available to this process, 1 when the system will not say.
pub fn available_cores() -> usize {
    std::thread::available_parallelism().map_or(1, usize::from)
}

/// How many chunks to run at once. An explicit `jobs` is kept as is.
///
/// - API: `jobs` or 4, and no threads.
/// - Local on a GPU build: `jobs` or 1 (the states queue on the same GPU),
///   with `min(4, cores)` threads.
/// - Local on the CPU: `jobs` or `clamp(cores / 4, 1, 4)`, with
///   `clamp(cores / jobs, 1, 8)` threads.
pub fn parallelism(
    kind: ProviderKind,
    jobs: Option<usize>,
    cores: usize,
    gpu: bool,
) -> Parallelism {
    let cores = cores.max(1);
    match (kind, gpu) {
        (ProviderKind::Api, _) => Parallelism {
            jobs: jobs.unwrap_or(DEFAULT_API_JOBS),
            threads_per_job: None,
        },
        (ProviderKind::Local, true) => Parallelism {
            jobs: jobs.unwrap_or(1),
            threads_per_job: Some(cores.min(4)),
        },
        (ProviderKind::Local, false) => {
            let jobs = jobs.unwrap_or_else(|| (cores / 4).clamp(1, 4));
            Parallelism {
                jobs,
                threads_per_job: Some((cores / jobs.max(1)).clamp(1, 8)),
            }
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

    let model_override = cli.model.clone().or_else(|| env_value("SRTTY_MODEL"));
    let (model, model_path) = match provider.kind {
        ProviderKind::Local => {
            let path = model_override
                .map(PathBuf::from)
                .or_else(|| provider.model_path.clone())
                .ok_or_else(|| missing_model_path(&provider_name))?;
            let path = paths::expand_tilde(&path, etcetera::home_dir().ok().as_deref());
            let name = path.file_name().map_or_else(
                || path.display().to_string(),
                |n| n.to_string_lossy().into_owned(),
            );
            (name, Some(path))
        }
        ProviderKind::Api => {
            let model = model_override.or_else(|| provider.model.clone()).ok_or_else(|| {
                SrttyError::Config(format!(
                    "no model configured for provider `{provider_name}`; pass --model or set `model` in the config file"
                ))
            })?;
            (model, None)
        }
    };

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
        jobs: cli.jobs.or(cfg.chunking.jobs),
        model_path,
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
        assert_eq!((c.chunking.target_s, c.chunking.jobs), (300.0, None));
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
        let Backend::Api { key, .. } = r.backend().unwrap() else {
            panic!("expected an API backend");
        };
        assert_eq!(key.expose(), "gsk-secret");
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
            None
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
    fn local_provider_needs_no_key() {
        let cfg: Config = toml::from_str(
            "default_provider = \"l\"\n[providers.l]\nkind = \"local\"\nmodel_path = \"/m/ggml-base.bin\"\n",
        )
        .unwrap();
        let cli = Cli::try_parse_from(["srtty", "a.mp4"]).unwrap();
        let r = resolve(cfg, &cli, &env_with(&[])).unwrap();
        assert!(matches!(r.api_key, KeyState::NotNeeded));
    }

    fn local_cfg(model_path: Option<&str>) -> Config {
        let path = model_path
            .map(|p| format!("model_path = \"{p}\"\n"))
            .unwrap_or_default();
        toml::from_str(&format!(
            "default_provider = \"l\"\n[providers.l]\nkind = \"local\"\n{path}"
        ))
        .unwrap()
    }

    #[test]
    fn parallelism_rules() {
        use ProviderKind::{Api, Local};
        let p = |jobs, threads_per_job| Parallelism {
            jobs,
            threads_per_job,
        };
        let cases = [
            ((Api, None, 16, false), p(4, None)),
            ((Local, None, 16, false), p(4, Some(4))),
            ((Local, None, 8, false), p(2, Some(4))),
            ((Local, None, 2, false), p(1, Some(2))),
            ((Local, None, 1, false), p(1, Some(1))),
            ((Local, None, 64, false), p(4, Some(8))),
            ((Local, Some(2), 16, false), p(2, Some(8))),
            ((Local, Some(32), 16, false), p(32, Some(1))),
            ((Local, None, 10, true), p(1, Some(4))),
            ((Api, Some(7), 16, false), p(7, None)),
        ];
        for ((kind, jobs, cores, gpu), want) in cases {
            assert_eq!(
                parallelism(kind, jobs, cores, gpu),
                want,
                "{kind:?} {jobs:?} {cores} {gpu}"
            );
        }
    }

    #[test]
    fn local_provider_resolves_its_model_path() {
        let cli = Cli::try_parse_from(["srtty", "a.mp4"]).unwrap();
        let r = resolve(local_cfg(Some("/m/ggml-base.bin")), &cli, &env_with(&[])).unwrap();
        assert_eq!(r.model, "ggml-base.bin");
        assert_eq!(r.model_path, Some(PathBuf::from("/m/ggml-base.bin")));
        match r.backend().unwrap() {
            Backend::Local { model_path } => {
                assert_eq!(model_path, PathBuf::from("/m/ggml-base.bin"))
            }
            other => panic!("expected a local backend, got {other:?}"),
        }
    }

    #[test]
    fn local_model_path_expands_tilde() {
        let cli = Cli::try_parse_from(["srtty", "a.mp4"]).unwrap();
        let cfg = local_cfg(Some("~/m/ggml-base.bin"));
        let r = resolve(cfg, &cli, &env_with(&[])).unwrap();
        let path = r.model_path.unwrap();
        assert!(!path.starts_with("~"), "{}", path.display());
        assert!(path.ends_with("m/ggml-base.bin"), "{}", path.display());
        assert_eq!(r.model, "ggml-base.bin");
    }

    #[test]
    fn model_flag_overrides_local_model_path() {
        let cli =
            Cli::try_parse_from(["srtty", "a.mp4", "--model", "/tmp/x/ggml-tiny.bin"]).unwrap();
        let r = resolve(local_cfg(Some("/m/ggml-base.bin")), &cli, &env_with(&[])).unwrap();
        assert_eq!(r.model_path, Some(PathBuf::from("/tmp/x/ggml-tiny.bin")));
        assert_eq!(r.model, "ggml-tiny.bin");

        let cli = Cli::try_parse_from(["srtty", "a.mp4"]).unwrap();
        let env = env_with(&[("SRTTY_MODEL", "/tmp/x/ggml-tiny.bin")]);
        let r = resolve(local_cfg(Some("/m/ggml-base.bin")), &cli, &env).unwrap();
        assert_eq!(r.model_path, Some(PathBuf::from("/tmp/x/ggml-tiny.bin")));
        assert_eq!(r.model, "ggml-tiny.bin");
    }

    #[test]
    fn local_provider_without_model_path_is_a_config_error() {
        let cli = Cli::try_parse_from(["srtty", "a.mp4"]).unwrap();
        let err = resolve(local_cfg(None), &cli, &env_with(&[])).unwrap_err();
        assert!(matches!(err, SrttyError::Config(_)));
        let msg = err.to_string();
        assert!(
            msg.contains("model_path") && msg.contains("srtty setup"),
            "{msg}"
        );
    }

    #[test]
    fn api_provider_has_no_model_path() {
        let cfg: Config = toml::from_str(TOML).unwrap();
        let cli = Cli::try_parse_from(["srtty", "a.mp4"]).unwrap();
        let r = resolve(cfg, &cli, &env_with(&[])).unwrap();
        assert_eq!(r.model_path, None);
    }

    #[test]
    fn jobs_come_from_the_flag_then_the_config() {
        let cfg: Config = toml::from_str(&format!("{TOML}[chunking]\njobs = 3\n")).unwrap();
        let cli = Cli::try_parse_from(["srtty", "a.mp4"]).unwrap();
        let r = resolve(cfg.clone(), &cli, &env_with(&[])).unwrap();
        assert_eq!(r.jobs, Some(3));
        let cli = Cli::try_parse_from(["srtty", "a.mp4", "--jobs", "5"]).unwrap();
        assert_eq!(resolve(cfg, &cli, &env_with(&[])).unwrap().jobs, Some(5));
        let cfg: Config = toml::from_str(TOML).unwrap();
        let cli = Cli::try_parse_from(["srtty", "a.mp4"]).unwrap();
        assert_eq!(resolve(cfg, &cli, &env_with(&[])).unwrap().jobs, None);
    }

    #[test]
    fn api_backend_carries_key_and_base_url() {
        let cfg: Config = toml::from_str(TOML).unwrap();
        let cli = Cli::try_parse_from(["srtty", "a.mp4"]).unwrap();

        let env = env_with(&[("OPENROUTER_API_KEY", "sk-or-secret")]);
        let r = resolve(cfg.clone(), &cli, &env).unwrap();
        match r.backend().unwrap() {
            Backend::Api {
                base_url,
                key,
                mode,
            } => {
                assert_eq!(base_url, "https://openrouter.ai/api/v1");
                assert_eq!(key.expose(), "sk-or-secret");
                assert_eq!(mode, RequestMode::Multipart);
            }
            other => panic!("expected an API backend, got {other:?}"),
        }

        let r = resolve(cfg, &cli, &env_with(&[])).unwrap();
        assert!(matches!(&r.api_key, KeyState::Unset { var } if var == "OPENROUTER_API_KEY"));
        let err = r.backend().unwrap_err().to_string();
        assert!(err.contains("OPENROUTER_API_KEY"), "{err}");

        let cfg: Config = toml::from_str(
            "default_provider = \"p\"\n[providers.p]\napi_key_env = \"K\"\nmodel = \"m\"\n",
        )
        .unwrap();
        let r = resolve(cfg, &cli, &env_with(&[("K", "v")])).unwrap();
        let err = r.backend().unwrap_err().to_string();
        assert!(err.contains("base_url"), "{err}");
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
