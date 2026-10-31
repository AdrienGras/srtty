//! `srtty setup`: choose a provider, the variable holding its key and a model,
//! optionally test the model on a short sample, then write the config file.

#[cfg(feature = "local")]
pub mod local;
pub mod prompt;
pub mod write;

use std::path::{Path, PathBuf};

use async_trait::async_trait;

use crate::compat::{CompatList, ModelStatus, REFRESH_TIMEOUT, provider_key, refresh};
use crate::config::{ApiKey, RequestMode, is_env_var_name, looks_like_api_key};
use crate::error::SrttyError;
use crate::models::{ModelRow, Target, fetch_rows, rows};
use crate::probe::{ProbeOutcome, SAMPLE, SAMPLE_S, Verdict, probe_model_once, write_sample};
use crate::providers::{ListedModel, Listing, PRESETS};
use crate::transcribe::TranscribeError;
use crate::transcribe::openai::OpenAiCompatible;

pub use prompt::{InquirePrompter, Prompter};
use write::{ProviderChoice, apply, check_existing, write_config};

/// Runs the paid sample test of a model.
#[async_trait]
pub trait SampleTester: Send + Sync {
    async fn test(
        &self,
        base_url: &str,
        key: &ApiKey,
        model: &str,
    ) -> Result<ProbeOutcome, TranscribeError>;
}

/// Transcribes the embedded 30 s sample with the provider, in one attempt.
pub struct LiveTester;

#[async_trait]
impl SampleTester for LiveTester {
    async fn test(
        &self,
        base_url: &str,
        key: &ApiKey,
        model: &str,
    ) -> Result<ProbeOutcome, TranscribeError> {
        let sample = write_sample(SAMPLE, SAMPLE_S).map_err(|e| TranscribeError::Fatal {
            status: None,
            message: format!("cannot write the sample: {e}"),
        })?;
        let t = OpenAiCompatible::new(
            base_url.to_string(),
            key.clone(),
            model.to_string(),
            RequestMode::Multipart,
        )?;
        probe_model_once(&t, &sample.chunk).await
    }
}

/// Where `setup` reads and writes, and how it reaches the outside world.
pub struct SetupContext<'a> {
    pub config_path: PathBuf,
    pub cache_dir: PathBuf,
    pub compat_url: String,
    pub env: &'a dyn Fn(&str) -> Option<String>,
    pub tester: &'a dyn SampleTester,
    /// The local whisper.cpp path.
    #[cfg(feature = "local")]
    pub local: local::LocalSetup<'a>,
}

/// Outcome of `setup`: whether the config file was written, and its path.
#[derive(Debug)]
pub struct SetupDone {
    pub written: bool,
    pub path: PathBuf,
}

const CUSTOM: &str = "Custom OpenAI-compatible URL";
#[cfg(feature = "local")]
const LOCAL_WHISPER: &str = "Local Whisper (offline, runs on this machine)";
#[cfg(not(feature = "local"))]
const LOCAL_WHISPER: &str = "Local Whisper (not available in this build)";
/// Name of the provider entry the local path writes.
#[cfg(feature = "local")]
const LOCAL_PROVIDER: &str = "local";
const SHOW_INCOMPATIBLE: &str = "Show incompatible models";
const ENTER_MODEL_ID: &str = "Enter a model id";

/// Asks for a provider, then either its key variable and a model with an
/// optional paid test, or (with the `local` feature) a whisper model with an
/// optional free test, then writes the config file. Prompts block: nothing
/// else runs while `setup` waits for an answer.
pub async fn run_setup(
    p: &mut dyn Prompter,
    ctx: &SetupContext<'_>,
) -> Result<SetupDone, SrttyError> {
    // A file `setup` cannot update is reported before any question.
    let existing = read_existing(&ctx.config_path)?;
    let (name, text) = match choose_provider(p)? {
        ProviderPick::Api {
            name,
            base_url,
            default_var,
        } => {
            let choice = api_provider(p, ctx, name, base_url, default_var).await?;
            let text = apply(existing.as_deref(), &choice)?;
            (choice.name, text)
        }
        #[cfg(feature = "local")]
        ProviderPick::Local => {
            let model_path = local::choose_local_model(p, &ctx.local).await?;
            let text = write::apply_local(existing.as_deref(), LOCAL_PROVIDER, &model_path)?;
            (LOCAL_PROVIDER.to_string(), text)
        }
    };
    finish(p, &ctx.config_path, existing.is_some(), &name, &text)
}

/// The API path: the key variable, then a model, with the optional paid test.
async fn api_provider(
    p: &mut dyn Prompter,
    ctx: &SetupContext<'_>,
    name: String,
    base_url: String,
    default_var: Option<&'static str>,
) -> Result<ProviderChoice, SrttyError> {
    let var = p.text(
        "Environment variable holding the API key:",
        default_var,
        &validate_var_name,
    )?;
    let key = (ctx.env)(&var).filter(|v| !v.is_empty()).map(ApiKey);
    if key.is_some() {
        p.say(&format!("✔ {var} is set"));
    } else {
        p.say(&format!(
            "! {var} is not set. Export it before running srtty."
        ));
    }

    let refreshed = refresh(&ctx.cache_dir, &ctx.compat_url, REFRESH_TIMEOUT).await;
    if let Some(warning) = &refreshed.warning {
        p.say(warning);
    }
    let target = Target {
        name: name.clone(),
        base_url: base_url.clone(),
        api_key_env: Some(var.clone()),
    };
    let listed = if key.is_none() && Listing::for_base_url(&base_url) == Listing::OpenAi {
        Err(format!("{var} is not set"))
    } else {
        fetch_rows(&target, key.as_ref(), &refreshed.list)
            .await
            .map_err(|e| e.to_string())
    };
    let rows = listed.unwrap_or_else(|cause| {
        p.say(&format!("! Could not list models: {cause}"));
        fallback_rows(&refreshed.list, &base_url)
    });

    let model = choose_model(p, ctx, &rows, &base_url, &var, key.as_ref()).await?;
    Ok(ProviderChoice {
        name,
        base_url,
        api_key_env: var,
        model,
    })
}

/// The provider picked in the first menu.
enum ProviderPick {
    /// Provider name, base URL and the suggested key variable.
    Api {
        name: String,
        base_url: String,
        default_var: Option<&'static str>,
    },
    /// Local whisper.cpp; never picked without the `local` feature.
    #[cfg(feature = "local")]
    Local,
}

fn choose_provider(p: &mut dyn Prompter) -> Result<ProviderPick, SrttyError> {
    let mut options: Vec<String> = PRESETS.iter().map(|pr| pr.label.to_string()).collect();
    options.push(CUSTOM.to_string());
    options.push(LOCAL_WHISPER.to_string());
    let i = loop {
        let i = p.select("Which transcription provider?", &options)?;
        // Without the feature, the local option only explains itself.
        if !cfg!(feature = "local") && options[i] == LOCAL_WHISPER {
            p.say(
                "Local Whisper needs a build with the `local` feature: cargo install srtty --features local",
            );
            continue;
        }
        break i;
    };
    if let Some(pr) = PRESETS.get(i) {
        return Ok(ProviderPick::Api {
            name: pr.key.to_string(),
            base_url: pr.base_url.to_string(),
            default_var: Some(pr.api_key_env),
        });
    }
    #[cfg(feature = "local")]
    if options[i] == LOCAL_WHISPER {
        return Ok(ProviderPick::Local);
    }
    let name = p.text(
        "Provider name (used in the config file):",
        Some("custom"),
        &validate_provider_name,
    )?;
    let base_url = p.text(
        "Base URL (e.g. http://localhost:8000/v1):",
        None,
        &validate_base_url,
    )?;
    Ok(ProviderPick::Api {
        name,
        base_url: base_url.trim().to_string(),
        default_var: None,
    })
}

/// The validators never quote the input: it may be a pasted key.
fn validate_var_name(input: &str) -> Result<(), String> {
    if looks_like_api_key(input) {
        return Err("That looks like an API key. Enter the name of the variable that holds it, e.g. OPENROUTER_API_KEY.".into());
    }
    if !is_env_var_name(input) {
        return Err("Use letters, digits and underscores, e.g. OPENROUTER_API_KEY.".into());
    }
    Ok(())
}

fn validate_provider_name(input: &str) -> Result<(), String> {
    let ok = !input.is_empty()
        && input
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-');
    if ok {
        Ok(())
    } else {
        Err("Use lowercase letters, digits, '-' and '_', e.g. custom.".into())
    }
}

fn validate_base_url(input: &str) -> Result<(), String> {
    let input = input.trim();
    let valid = (input.starts_with("http://") || input.starts_with("https://"))
        && reqwest::Url::parse(input).is_ok_and(|u| u.host_str().is_some_and(|h| !h.is_empty()));
    if valid {
        Ok(())
    } else {
        Err("Enter a URL starting with http:// or https://, e.g. http://localhost:8000/v1.".into())
    }
}

fn validate_model_id(input: &str) -> Result<(), String> {
    if input.trim().is_empty() {
        Err("Enter a model id.".into())
    } else {
        Ok(())
    }
}

/// Asks for a model until one is kept. With a key, offers the paid test first.
async fn choose_model(
    p: &mut dyn Prompter,
    ctx: &SetupContext<'_>,
    rows: &[ModelRow],
    base_url: &str,
    var: &str,
    key: Option<&ApiKey>,
) -> Result<String, SrttyError> {
    let mut show_all = false;
    loop {
        let menu = model_menu(rows, show_all);
        let i = p.select("Model:", &menu.options)?;
        let row = menu.rows.get(i).copied();
        let id = match row {
            Some(row) => {
                if row.status == ModelStatus::Incompatible
                    && !p.confirm(
                        &format!("{} is marked incompatible. Use it anyway?", row.id),
                        false,
                    )?
                {
                    continue;
                }
                row.id.clone()
            }
            None if menu.has_hidden && i == menu.rows.len() => {
                show_all = true;
                continue;
            }
            None => p
                .text("Model id:", None, &validate_model_id)?
                .trim()
                .to_string(),
        };
        let Some(key) = key else {
            return Ok(id);
        };
        if !p.confirm(&test_question(&id, row), false)? {
            return Ok(id);
        }
        let usable = match ctx.tester.test(base_url, key, &id).await {
            Ok(outcome) => {
                let (line, usable) = match &outcome.verdict {
                    Verdict::Usable { words: true, .. } => (
                        "✔ Test passed: word timestamps available.".to_string(),
                        true,
                    ),
                    Verdict::Usable { words: false, .. } => (
                        "✔ Test passed: fine segments, no word timestamps.".to_string(),
                        true,
                    ),
                    Verdict::Unusable | Verdict::Incompatible => {
                        ("✘ This model cannot produce subtitles.".to_string(), false)
                    }
                    Verdict::Inconclusive(m) => (format!("! Test inconclusive: {m}"), true),
                };
                let cost = outcome
                    .cost_usd
                    .map(|usd| format!(" (cost ${usd:.4})"))
                    .unwrap_or_default();
                p.say(&format!("{line}{cost}"));
                usable
            }
            Err(TranscribeError::Auth { .. }) => {
                p.say(&format!("✘ Authentication failed; check {var}."));
                true
            }
            Err(e) => {
                p.say(&format!("! Test inconclusive: {e}"));
                true
            }
        };
        if usable || p.confirm(&format!("Keep {id} anyway?"), false)? {
            return Ok(id);
        }
    }
}

/// The confirmation that guards the paid test, with its estimated cost.
fn test_question(id: &str, row: Option<&ModelRow>) -> String {
    let cost = match row {
        Some(ModelRow {
            usd_per_hour: Some(usd),
            ..
        }) => format!("${:.4}.", usd * SAMPLE_S / 3600.0),
        Some(ModelRow {
            token_priced: true,
            url: Some(url),
            ..
        }) => format!("unknown (billed per token, see {url})."),
        _ => "unknown.".to_string(),
    };
    format!("Test {id} on a {SAMPLE_S} s sample? Estimated cost: {cost}")
}

/// The current config file, if any, once checked that `setup` can update it.
fn read_existing(path: &Path) -> Result<Option<String>, SrttyError> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => {
            return Err(SrttyError::Config(format!(
                "cannot read {}: {e}",
                path.display()
            )));
        }
    };
    check_existing(&text).map_err(|reason| {
        SrttyError::Config(format!(
            "cannot update {}: {reason}. Fix or move this file, then run `srtty setup` again.",
            path.display()
        ))
    })?;
    Ok(Some(text))
}

/// Writes `text`, the updated config, to `path`, asking first when a file
/// exists (`update`). `name` is the provider `text` makes the default.
fn finish(
    p: &mut dyn Prompter,
    path: &Path,
    update: bool,
    name: &str,
    text: &str,
) -> Result<SetupDone, SrttyError> {
    if update
        && !p.confirm(
            &format!(
                "Update {}? This sets default_provider = \"{name}\" and updates [providers.{name}].",
                path.display(),
            ),
            false,
        )?
    {
        p.say("Nothing written.");
        return Ok(SetupDone {
            written: false,
            path: path.to_path_buf(),
        });
    }
    write_config(path, text)?;
    p.say(&format!("Wrote {}.", path.display()));
    p.say("Next: srtty <video-or-audio-file>");
    Ok(SetupDone {
        written: true,
        path: path.to_path_buf(),
    })
}

/// The model choices: rows (incompatible ones only with `show_all`), then the
/// extra entries. `rows[i]` is the row of `options[i]`.
struct ModelMenu<'r> {
    rows: Vec<&'r ModelRow>,
    options: Vec<String>,
    has_hidden: bool,
}

fn model_menu(rows: &[ModelRow], show_all: bool) -> ModelMenu<'_> {
    let visible: Vec<&ModelRow> = rows
        .iter()
        .filter(|r| show_all || r.status != ModelStatus::Incompatible)
        .collect();
    let has_hidden = visible.len() < rows.len();
    let mut options: Vec<String> = visible.iter().map(|r| model_label(r)).collect();
    if has_hidden {
        options.push(SHOW_INCOMPATIBLE.to_string());
    }
    options.push(ENTER_MODEL_ID.to_string());
    ModelMenu {
        rows: visible,
        options,
        has_hidden,
    }
}

fn model_label(row: &ModelRow) -> String {
    let price = match (row.usd_per_hour, &row.url) {
        (Some(usd), _) => format!("${usd:.2}/h"),
        (None, Some(url)) if row.token_priced => format!("billed per token, see {url}"),
        (None, _) if row.token_priced => "billed per token".to_string(),
        (None, _) => "price unknown".to_string(),
    };
    format!("{}  {price}  {}", row.id, row.status.label())
}

/// The provider's models as `compat.json` knows them, for when the listing fails.
fn fallback_rows(list: &CompatList, base_url: &str) -> Vec<ModelRow> {
    let Some(provider) = provider_key(base_url) else {
        return Vec::new();
    };
    let listed: Vec<ListedModel> = list
        .models
        .iter()
        .filter(|e| e.provider == provider)
        .map(|e| ListedModel {
            id: e.id.clone(),
            usd_per_s: None,
            token_priced: false,
            url: None,
        })
        .collect();
    rows(&listed, list, Some(provider))
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use clap::Parser;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::*;
    use crate::cli::Cli;
    use crate::compat::ModelStatus;
    use crate::config::{self, Config};
    use crate::probe::Verdict;

    #[derive(Debug)]
    enum Answer {
        Select(usize),
        Text(String),
        Confirm(bool),
    }

    fn text(s: &str) -> Answer {
        Answer::Text(s.to_string())
    }

    /// Replays scripted answers and records everything shown to the user.
    #[derive(Default)]
    struct ScriptedPrompter {
        script: VecDeque<Answer>,
        /// Every prompt message, option and `say` line, in order.
        shown: Vec<String>,
        /// Prompt messages only.
        prompts: Vec<String>,
        /// Options of each `select`, in order.
        options: Vec<Vec<String>>,
        said: Vec<String>,
        validator_errors: Vec<String>,
    }

    impl ScriptedPrompter {
        fn new(script: Vec<Answer>) -> Self {
            let mut p = Self::default();
            p.script = script.into();
            p
        }

        fn next(&mut self, message: &str) -> Answer {
            self.script
                .pop_front()
                .unwrap_or_else(|| panic!("script exhausted at prompt {message:?}"))
        }

        fn asked(&self, message: &str) -> usize {
            self.prompts.iter().filter(|m| *m == message).count()
        }

        fn everything(&self) -> String {
            let mut all = self.shown.clone();
            all.extend(self.validator_errors.iter().cloned());
            all.join("\n")
        }
    }

    /// A test that leaves answers unused did not go where it meant to.
    impl Drop for ScriptedPrompter {
        fn drop(&mut self) {
            if !std::thread::panicking() {
                assert!(
                    self.script.is_empty(),
                    "unused scripted answers: {:?}",
                    self.script
                );
            }
        }
    }

    impl Prompter for ScriptedPrompter {
        fn select(&mut self, message: &str, options: &[String]) -> Result<usize, SrttyError> {
            self.prompts.push(message.to_string());
            self.shown.push(message.to_string());
            self.shown.extend(options.iter().cloned());
            self.options.push(options.to_vec());
            match self.next(message) {
                Answer::Select(i) => {
                    assert!(
                        i < options.len(),
                        "scripted index {i} out of range for {message:?}: {options:?}"
                    );
                    Ok(i)
                }
                other => panic!("expected a Select answer for {message:?}, script has {other:?}"),
            }
        }

        fn text(
            &mut self,
            message: &str,
            default: Option<&str>,
            validate: &dyn Fn(&str) -> Result<(), String>,
        ) -> Result<String, SrttyError> {
            self.prompts.push(message.to_string());
            self.shown.push(message.to_string());
            loop {
                let input = match self.next(message) {
                    Answer::Text(s) => s,
                    other => {
                        panic!("expected a Text answer for {message:?}, script has {other:?}")
                    }
                };
                let input = match default {
                    Some(d) if input.is_empty() => d.to_string(),
                    _ => input,
                };
                match validate(&input) {
                    Ok(()) => return Ok(input),
                    // Like a terminal prompt: show the error and ask again.
                    Err(e) => self.validator_errors.push(e),
                }
            }
        }

        fn confirm(&mut self, message: &str, _default: bool) -> Result<bool, SrttyError> {
            self.prompts.push(message.to_string());
            self.shown.push(message.to_string());
            match self.next(message) {
                Answer::Confirm(b) => Ok(b),
                other => {
                    panic!("expected a Confirm answer for {message:?}, script has {other:?}")
                }
            }
        }

        fn say(&mut self, line: &str) {
            self.shown.push(line.to_string());
            self.said.push(line.to_string());
        }
    }

    /// Answers every test with `verdict` (an authentication failure when
    /// `None`) and counts its calls.
    struct FakeTester {
        calls: AtomicUsize,
        verdict: Option<Verdict>,
    }

    impl FakeTester {
        fn new(verdict: Verdict) -> Self {
            Self {
                calls: AtomicUsize::new(0),
                verdict: Some(verdict),
            }
        }

        fn auth_failure() -> Self {
            Self {
                calls: AtomicUsize::new(0),
                verdict: None,
            }
        }

        fn calls(&self) -> usize {
            self.calls.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl SampleTester for FakeTester {
        async fn test(
            &self,
            _base_url: &str,
            _key: &ApiKey,
            _model: &str,
        ) -> Result<ProbeOutcome, TranscribeError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            match &self.verdict {
                Some(verdict) => Ok(ProbeOutcome {
                    verdict: verdict.clone(),
                    cost_usd: Some(0.0008),
                }),
                None => Err(TranscribeError::Auth { status: 401 }),
            }
        }
    }

    fn usable() -> Verdict {
        Verdict::Usable {
            words: true,
            fine_segments: true,
        }
    }

    const KEY_VALUE: &str = "test-key-value-77";
    const COMPAT_BODY: &str = r#"{"schema":1,"generated_at":"2030-01-02","models":[]}"#;

    /// Serves `/v1/models` (two Whisper models) with `models_status`, and `/compat.json`.
    async fn server(models_status: u16) -> MockServer {
        let server = MockServer::start().await;
        let listing = r#"{"data":[{"id":"whisper-1"},{"id":"whisper-large-v3"}]}"#;
        Mock::given(method("GET"))
            .and(path("/v1/models"))
            .respond_with(
                ResponseTemplate::new(models_status).set_body_raw(listing, "application/json"),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/compat.json"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(COMPAT_BODY, "application/json"))
            .mount(&server)
            .await;
        server
    }

    /// `K` holds the key; nothing else is set.
    fn env_with_key(name: &str) -> Option<String> {
        (name == "K").then(|| KEY_VALUE.to_string())
    }

    fn env_empty(_: &str) -> Option<String> {
        None
    }

    fn ctx<'a>(
        dir: &tempfile::TempDir,
        server: &MockServer,
        env: &'a dyn Fn(&str) -> Option<String>,
        tester: &'a dyn SampleTester,
    ) -> SetupContext<'a> {
        SetupContext {
            config_path: dir.path().join("config.toml"),
            cache_dir: dir.path().join("cache"),
            compat_url: format!("{}/compat.json", server.uri()),
            env,
            tester,
            #[cfg(feature = "local")]
            local: local::LocalSetup {
                models_dir: dir.path().join("models"),
                catalog: &[],
                fetcher: &UnusedLocal,
                tester: &UnusedLocal,
                home: None,
            },
        }
    }

    /// Stands in for the local backend in the API flows, which never reach it.
    #[cfg(feature = "local")]
    struct UnusedLocal;

    #[cfg(feature = "local")]
    #[async_trait]
    impl local::ModelFetcher for UnusedLocal {
        async fn fetch(
            &self,
            _model: &crate::local::catalog::CatalogModel,
            _dest: &std::path::Path,
        ) -> Result<(), SrttyError> {
            panic!("unexpected download")
        }
    }

    #[cfg(feature = "local")]
    #[async_trait]
    impl local::LocalTester for UnusedLocal {
        async fn test(
            &self,
            _model_path: &std::path::Path,
        ) -> Result<local::LocalTestOutcome, SrttyError> {
            panic!("unexpected local test")
        }
    }

    /// Custom provider "mock" at the server, key variable `K`.
    fn custom_provider(server: &MockServer) -> Vec<Answer> {
        vec![
            Answer::Select(3),
            text("mock"),
            text(&format!("{}/v1", server.uri())),
            text("K"),
        ]
    }

    fn script(server: &MockServer, rest: Vec<Answer>) -> Vec<Answer> {
        let mut s = custom_provider(server);
        s.extend(rest);
        s
    }

    fn written_model(path: &std::path::Path) -> (String, String) {
        let cfg = Config::load(path).unwrap();
        let cli = Cli::try_parse_from(["srtty"]).unwrap();
        let r = config::resolve(cfg, &cli, &env_with_key).unwrap();
        (r.provider_name, r.model)
    }

    fn row(id: &str, usd_per_hour: Option<f64>, status: ModelStatus) -> ModelRow {
        ModelRow {
            id: id.into(),
            usd_per_hour,
            token_priced: false,
            url: None,
            status,
            words: None,
            fine_segments: None,
            checked_at: None,
        }
    }

    #[test]
    fn model_labels_show_price_and_status() {
        assert_eq!(
            model_label(&row("a/b", Some(0.1), ModelStatus::Compatible)),
            "a/b  $0.10/h  compatible"
        );
        assert_eq!(
            model_label(&row("a/b", None, ModelStatus::Unknown)),
            "a/b  price unknown  unknown"
        );
        let token = ModelRow {
            token_priced: true,
            url: Some("https://openrouter.ai/g/t".into()),
            ..row("g/t", None, ModelStatus::CompatibleSpeakers)
        };
        assert_eq!(
            model_label(&token),
            "g/t  billed per token, see https://openrouter.ai/g/t  compatible + speakers"
        );
    }

    #[test]
    fn incompatible_models_are_behind_an_option() {
        let rows = vec![
            row("ok", Some(0.1), ModelStatus::Compatible),
            row("bad", Some(0.01), ModelStatus::Incompatible),
        ];
        let menu = model_menu(&rows, false);
        assert_eq!(
            menu.options,
            [
                "ok  $0.10/h  compatible",
                "Show incompatible models",
                "Enter a model id"
            ]
        );
        let menu = model_menu(&rows, true);
        assert_eq!(
            menu.options,
            [
                "ok  $0.10/h  compatible",
                "bad  $0.01/h  incompatible",
                "Enter a model id"
            ]
        );
    }

    #[test]
    fn fallback_rows_come_from_compat_json() {
        let list = crate::test_support::compat_seed();
        let r = fallback_rows(&list, "https://openrouter.ai/api/v1");
        let nova = r.iter().find(|m| m.id == "deepgram/nova-3").unwrap();
        assert_eq!(nova.usd_per_hour, Some(0.26));
        assert_eq!(nova.status, ModelStatus::CompatibleSpeakers);
        assert!(fallback_rows(&list, "http://localhost:8000/v1").is_empty());
    }

    #[tokio::test]
    async fn setup_writes_a_usable_config() {
        let server = server(200).await;
        let dir = tempfile::tempdir().unwrap();
        let tester = FakeTester::new(usable());
        let ctx = ctx(&dir, &server, &env_with_key, &tester);
        let mut p = ScriptedPrompter::new(script(
            &server,
            vec![Answer::Select(0), Answer::Confirm(false)],
        ));

        let done = run_setup(&mut p, &ctx).await.unwrap();

        for line in &p.said {
            println!("say: {line}");
        }
        assert!(done.written);
        assert_eq!(done.path, ctx.config_path);
        assert_eq!(
            p.options[0],
            [
                "OpenRouter",
                "OpenAI",
                "Groq",
                "Custom OpenAI-compatible URL",
                if cfg!(feature = "local") {
                    "Local Whisper (offline, runs on this machine)"
                } else {
                    "Local Whisper (not available in this build)"
                }
            ]
        );
        assert_eq!(
            p.options[1],
            [
                "whisper-1  price unknown  unknown",
                "whisper-large-v3  price unknown  unknown",
                "Enter a model id"
            ]
        );
        assert!(p.said.contains(&"✔ K is set".to_string()), "{:?}", p.said);
        assert_eq!(tester.calls(), 0);
        let path = ctx.config_path.display().to_string();
        assert_eq!(
            p.said[p.said.len() - 2..],
            [
                format!("Wrote {path}."),
                "Next: srtty <video-or-audio-file>".to_string()
            ]
        );
        assert_eq!(
            written_model(&ctx.config_path),
            ("mock".to_string(), "whisper-1".to_string())
        );
        let text = std::fs::read_to_string(&ctx.config_path).unwrap();
        assert!(!text.contains(KEY_VALUE) && !p.everything().contains(KEY_VALUE));
    }

    #[tokio::test]
    async fn setup_rejects_a_pasted_key_without_echo() {
        let server = server(200).await;
        let dir = tempfile::tempdir().unwrap();
        let tester = FakeTester::new(usable());
        let ctx = ctx(&dir, &server, &env_with_key, &tester);
        let mut p = ScriptedPrompter::new(vec![
            Answer::Select(3),
            text("mock"),
            text(&format!("{}/v1", server.uri())),
            text("gsk_0123secret"),
            text("K"),
            Answer::Select(0),
            Answer::Confirm(false),
        ]);

        run_setup(&mut p, &ctx).await.unwrap();

        assert_eq!(
            p.validator_errors,
            [
                "That looks like an API key. Enter the name of the variable that holds it, e.g. OPENROUTER_API_KEY."
            ]
        );
        assert!(!p.everything().contains("0123secret"), "{}", p.everything());
        let text = std::fs::read_to_string(&ctx.config_path).unwrap();
        assert!(!text.contains("0123secret"), "{text}");
        assert!(text.contains("api_key_env = \"K\""), "{text}");
    }

    #[tokio::test]
    async fn env_var_validator_rejects_invalid_names() {
        let server = server(200).await;
        let dir = tempfile::tempdir().unwrap();
        let tester = FakeTester::new(usable());
        let ctx = ctx(&dir, &server, &env_with_key, &tester);
        let mut p = ScriptedPrompter::new(script_with_var(&server, &["MY-KEY", "K"]));

        run_setup(&mut p, &ctx).await.unwrap();

        assert_eq!(
            p.validator_errors,
            ["Use letters, digits and underscores, e.g. OPENROUTER_API_KEY."]
        );
    }

    fn script_with_var(server: &MockServer, vars: &[&str]) -> Vec<Answer> {
        let mut s = vec![
            Answer::Select(3),
            text("mock"),
            text(&format!("{}/v1", server.uri())),
        ];
        s.extend(vars.iter().map(|v| text(v)));
        s.extend([Answer::Select(0), Answer::Confirm(false)]);
        s
    }

    #[tokio::test]
    async fn custom_provider_name_and_url_are_validated() {
        let server = server(200).await;
        let dir = tempfile::tempdir().unwrap();
        let tester = FakeTester::new(usable());
        let ctx = ctx(&dir, &server, &env_with_key, &tester);
        let mut p = ScriptedPrompter::new(vec![
            Answer::Select(3),
            text("My Provider"),
            text("mock"),
            text("localhost:8000/v1"),
            text("http://"),
            text(&format!("{}/v1", server.uri())),
            text("K"),
            Answer::Select(0),
            Answer::Confirm(false),
        ]);

        run_setup(&mut p, &ctx).await.unwrap();

        assert_eq!(p.validator_errors.len(), 3, "{:?}", p.validator_errors);
        assert_eq!(
            p.prompts[1..3],
            [
                "Provider name (used in the config file):",
                "Base URL (e.g. http://localhost:8000/v1):"
            ]
        );
    }

    #[tokio::test]
    async fn declining_overwrite_leaves_file_untouched() {
        let server = server(200).await;
        let dir = tempfile::tempdir().unwrap();
        let tester = FakeTester::new(usable());
        let ctx = ctx(&dir, &server, &env_with_key, &tester);
        let before = "# mine\ndefault_provider = \"other\"\n";
        std::fs::write(&ctx.config_path, before).unwrap();
        let mut p = ScriptedPrompter::new(script(
            &server,
            vec![
                Answer::Select(0),
                Answer::Confirm(false),
                Answer::Confirm(false),
            ],
        ));

        let done = run_setup(&mut p, &ctx).await.unwrap();

        assert!(!done.written);
        assert_eq!(std::fs::read(&ctx.config_path).unwrap(), before.as_bytes());
        let path = ctx.config_path.display();
        assert_eq!(
            p.prompts.last().unwrap(),
            &format!(
                "Update {path}? This sets default_provider = \"mock\" and updates [providers.mock]."
            )
        );
        assert_eq!(p.said.last().unwrap(), "Nothing written.");
    }

    #[tokio::test]
    async fn accepting_overwrite_keeps_the_rest_of_the_file() {
        let server = server(200).await;
        let dir = tempfile::tempdir().unwrap();
        let tester = FakeTester::new(usable());
        let ctx = ctx(&dir, &server, &env_with_key, &tester);
        std::fs::write(&ctx.config_path, "# mine\ndefault_provider = \"other\"\n").unwrap();
        let mut p = ScriptedPrompter::new(script(
            &server,
            vec![
                Answer::Select(0),
                Answer::Confirm(false),
                Answer::Confirm(true),
            ],
        ));

        let done = run_setup(&mut p, &ctx).await.unwrap();

        assert!(done.written);
        let text = std::fs::read_to_string(&ctx.config_path).unwrap();
        assert!(text.contains("# mine"), "{text}");
        assert_eq!(written_model(&ctx.config_path).0, "mock");
    }

    #[tokio::test]
    async fn paid_test_runs_only_after_confirmation() {
        let server = server(200).await;
        let dir = tempfile::tempdir().unwrap();
        let tester = FakeTester::new(usable());
        let ctx = ctx(&dir, &server, &env_with_key, &tester);

        let mut p = ScriptedPrompter::new(script(
            &server,
            vec![Answer::Select(0), Answer::Confirm(false)],
        ));
        run_setup(&mut p, &ctx).await.unwrap();
        assert_eq!(tester.calls(), 0);
        assert!(
            p.prompts
                .contains(&"Test whisper-1 on a 30 s sample? Estimated cost: unknown.".to_string()),
            "{:?}",
            p.prompts
        );

        std::fs::remove_file(&ctx.config_path).unwrap();
        let mut p = ScriptedPrompter::new(script(
            &server,
            vec![Answer::Select(0), Answer::Confirm(true)],
        ));
        run_setup(&mut p, &ctx).await.unwrap();
        assert_eq!(tester.calls(), 1);
        assert!(
            p.said
                .contains(&"✔ Test passed: word timestamps available. (cost $0.0008)".to_string()),
            "{:?}",
            p.said
        );
    }

    #[tokio::test]
    async fn unset_key_skips_the_test_and_falls_back() {
        let server = server(200).await;
        let dir = tempfile::tempdir().unwrap();
        let tester = FakeTester::new(usable());
        let ctx = ctx(&dir, &server, &env_empty, &tester);
        let mut p =
            ScriptedPrompter::new(script(&server, vec![Answer::Select(0), text("whisper-1")]));

        let done = run_setup(&mut p, &ctx).await.unwrap();

        assert!(done.written);
        assert_eq!(tester.calls(), 0);
        assert!(
            p.said
                .contains(&"! K is not set. Export it before running srtty.".to_string()),
            "{:?}",
            p.said
        );
        assert!(
            p.said
                .iter()
                .any(|l| l.starts_with("! Could not list models:")),
            "{:?}",
            p.said
        );
        assert!(!p.prompts.iter().any(|m| m.starts_with("Test ")));
    }

    #[tokio::test]
    async fn failed_test_returns_to_model_choice() {
        let server = server(200).await;
        let dir = tempfile::tempdir().unwrap();
        let tester = FakeTester::new(Verdict::Unusable);
        let ctx = ctx(&dir, &server, &env_with_key, &tester);
        let mut p = ScriptedPrompter::new(script(
            &server,
            vec![
                Answer::Select(0),
                Answer::Confirm(true),
                Answer::Confirm(false),
                Answer::Select(1),
                Answer::Confirm(false),
            ],
        ));

        run_setup(&mut p, &ctx).await.unwrap();

        assert_eq!(p.asked("Model:"), 2);
        assert_eq!(p.asked("Keep whisper-1 anyway?"), 1);
        assert!(
            p.said
                .contains(&"✘ This model cannot produce subtitles. (cost $0.0008)".to_string()),
            "{:?}",
            p.said
        );
        assert_eq!(written_model(&ctx.config_path).1, "whisper-large-v3");
    }

    #[cfg(not(feature = "local"))]
    #[tokio::test]
    async fn local_whisper_is_explained_then_asked_again() {
        let server = server(200).await;
        let dir = tempfile::tempdir().unwrap();
        let tester = FakeTester::new(usable());
        let ctx = ctx(&dir, &server, &env_with_key, &tester);
        let mut s = vec![Answer::Select(4)];
        s.extend(script(
            &server,
            vec![Answer::Select(0), Answer::Confirm(false)],
        ));
        let mut p = ScriptedPrompter::new(s);

        run_setup(&mut p, &ctx).await.unwrap();

        assert_eq!(p.asked("Which transcription provider?"), 2);
        assert_eq!(
            p.said[0],
            "Local Whisper needs a build with the `local` feature: cargo install srtty --features local"
        );
    }

    #[tokio::test]
    async fn listing_failure_falls_back_to_manual_entry() {
        let server = server(500).await;
        let dir = tempfile::tempdir().unwrap();
        let tester = FakeTester::new(usable());
        let ctx = ctx(&dir, &server, &env_with_key, &tester);
        let mut p = ScriptedPrompter::new(script(
            &server,
            vec![Answer::Select(0), text("my-model"), Answer::Confirm(false)],
        ));

        run_setup(&mut p, &ctx).await.unwrap();

        assert!(
            p.said
                .iter()
                .any(|l| l.starts_with("! Could not list models:")),
            "{:?}",
            p.said
        );
        assert_eq!(p.options[1], ["Enter a model id"]);
        assert!(p.prompts.contains(&"Model id:".to_string()));
        assert_eq!(written_model(&ctx.config_path).1, "my-model");
    }

    #[test]
    fn test_question_states_the_cost() {
        let priced = row("a/b", Some(0.096), ModelStatus::Compatible);
        assert_eq!(
            test_question("a/b", Some(&priced)),
            "Test a/b on a 30 s sample? Estimated cost: $0.0008."
        );
        let token = ModelRow {
            token_priced: true,
            url: Some("https://openrouter.ai/g/t".into()),
            ..row("g/t", None, ModelStatus::Compatible)
        };
        assert_eq!(
            test_question("g/t", Some(&token)),
            "Test g/t on a 30 s sample? Estimated cost: unknown (billed per token, see https://openrouter.ai/g/t)."
        );
        assert_eq!(
            test_question("x", None),
            "Test x on a 30 s sample? Estimated cost: unknown."
        );
    }

    /// Picks whisper-1, accepts the paid test, and returns what was said.
    async fn tested_flow(tester: &FakeTester) -> Vec<String> {
        let server = server(200).await;
        let dir = tempfile::tempdir().unwrap();
        let ctx = ctx(&dir, &server, &env_with_key, tester);
        let mut p = ScriptedPrompter::new(script(
            &server,
            vec![Answer::Select(0), Answer::Confirm(true)],
        ));
        run_setup(&mut p, &ctx).await.unwrap();
        assert_eq!(tester.calls(), 1);
        assert_eq!(written_model(&ctx.config_path).1, "whisper-1");
        assert!(!p.prompts.iter().any(|m| m.starts_with("Keep ")));
        assert!(!p.everything().contains(KEY_VALUE));
        p.said.clone()
    }

    #[tokio::test]
    async fn inconclusive_test_keeps_the_model() {
        let tester = FakeTester::new(Verdict::Inconclusive("HTTP 503".into()));
        let said = tested_flow(&tester).await;
        assert!(
            said.contains(&"! Test inconclusive: HTTP 503 (cost $0.0008)".to_string()),
            "{said:?}"
        );
    }

    #[tokio::test]
    async fn auth_failure_names_the_variable_and_keeps_the_model() {
        let tester = FakeTester::auth_failure();
        let said = tested_flow(&tester).await;
        assert!(
            said.contains(&"✘ Authentication failed; check K.".to_string()),
            "{said:?}"
        );
    }

    #[tokio::test]
    async fn segments_without_words_pass_the_test() {
        let tester = FakeTester::new(Verdict::Usable {
            words: false,
            fine_segments: true,
        });
        let said = tested_flow(&tester).await;
        assert!(
            said.contains(
                &"✔ Test passed: fine segments, no word timestamps. (cost $0.0008)".to_string()
            ),
            "{said:?}"
        );
    }

    #[tokio::test]
    async fn invalid_existing_config_fails_before_any_prompt() {
        let server = server(200).await;
        for (before, expected) in [
            ("[providers.mock]\nmodle = \"x\"\n", "unknown field"),
            (
                "[providers.mock]\napi_key_env = sk-or-v1-0123secret\n",
                "not valid TOML at line 2",
            ),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let tester = FakeTester::new(usable());
            let ctx = ctx(&dir, &server, &env_with_key, &tester);
            std::fs::write(&ctx.config_path, before).unwrap();
            let mut p = ScriptedPrompter::new(vec![]);

            let err = run_setup(&mut p, &ctx).await.unwrap_err();

            assert!(matches!(err, SrttyError::Config(_)), "{err}");
            let msg = err.to_string();
            let path = ctx.config_path.display().to_string();
            assert!(msg.starts_with(&format!("cannot update {path}: ")), "{msg}");
            assert!(msg.contains(expected), "{msg}");
            assert!(
                msg.ends_with(". Fix or move this file, then run `srtty setup` again."),
                "{msg}"
            );
            assert!(!msg.contains("0123secret"), "{msg}");
            assert!(p.prompts.is_empty(), "{:?}", p.prompts);
            assert_eq!(tester.calls(), 0);
            assert_eq!(std::fs::read_to_string(&ctx.config_path).unwrap(), before);
        }
    }

    #[tokio::test]
    async fn live_test_makes_a_single_attempt() {
        let server = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/v1/audio/transcriptions"))
            .respond_with(ResponseTemplate::new(503).insert_header("retry-after", "0"))
            .mount(&server)
            .await;

        let outcome = LiveTester
            .test(
                &format!("{}/v1", server.uri()),
                &ApiKey(KEY_VALUE.into()),
                "m",
            )
            .await
            .unwrap();

        assert!(
            matches!(outcome.verdict, Verdict::Inconclusive(_)),
            "{outcome:?}"
        );
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }
    #[cfg(feature = "local")]
    mod local_flow {
        use std::path::Path;

        use sha2::{Digest, Sha256};

        use super::*;
        use crate::config::ProviderKind;
        use crate::local::catalog::CatalogModel;
        use crate::setup::local::{LocalSetup, LocalTestOutcome, LocalTester, ModelFetcher};

        const MODEL_BYTES: &[u8] = b"lmgg-test-model";
        const PROVIDER_MENU: &str = "Which transcription provider?";
        const MODEL_MENU: &str = "Whisper model:";
        const TEST_QUESTION: &str =
            "Test the model on a 30 s sample? It runs locally and costs nothing.";
        const LOCAL: usize = 4;
        const TINY: usize = 0;
        const OWN_FILE: usize = 1;

        /// A one-entry catalog whose model is `MODEL_BYTES`.
        fn test_catalog() -> Vec<CatalogModel> {
            let sha256 = crate::cache::hex(&Sha256::digest(MODEL_BYTES));
            vec![CatalogModel {
                name: "tiny-test",
                file: "ggml-tiny-test.bin",
                bytes: MODEL_BYTES.len() as u64,
                sha256: Box::leak(sha256.into_boxed_str()),
                note: "test model",
            }]
        }

        enum Fetch {
            Write,
            Fail,
            Interrupt,
        }

        /// Writes the expected model (or fails) and counts its calls.
        struct FakeFetcher {
            calls: AtomicUsize,
            outcome: Fetch,
        }

        impl FakeFetcher {
            fn new(outcome: Fetch) -> Self {
                Self {
                    calls: AtomicUsize::new(0),
                    outcome,
                }
            }

            fn calls(&self) -> usize {
                self.calls.load(Ordering::SeqCst)
            }
        }

        #[async_trait]
        impl ModelFetcher for FakeFetcher {
            async fn fetch(&self, model: &CatalogModel, dest: &Path) -> Result<(), SrttyError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                assert_eq!(dest.file_name().unwrap(), model.file);
                match self.outcome {
                    Fetch::Write => {
                        std::fs::create_dir_all(dest.parent().unwrap())?;
                        // The real download renames over the old file.
                        let _ = std::fs::remove_file(dest);
                        std::fs::write(dest, MODEL_BYTES)?;
                        Ok(())
                    }
                    Fetch::Fail => Err(SrttyError::Network("download failed: HTTP 404".into())),
                    Fetch::Interrupt => Err(SrttyError::Interrupted),
                }
            }
        }

        /// What every sample test gives.
        enum Tested {
            /// That many words in French.
            Words(usize),
            Fail,
            Interrupt,
        }

        /// Gives the same result to every test, and counts its calls.
        struct FakeLocalTester {
            calls: AtomicUsize,
            result: Tested,
        }

        impl FakeLocalTester {
            fn new(result: Tested) -> Self {
                Self {
                    calls: AtomicUsize::new(0),
                    result,
                }
            }
        }

        #[async_trait]
        impl LocalTester for FakeLocalTester {
            async fn test(&self, model_path: &Path) -> Result<LocalTestOutcome, SrttyError> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                assert!(model_path.is_file(), "{}", model_path.display());
                match self.result {
                    Tested::Words(words) => Ok(LocalTestOutcome {
                        words,
                        secs: 4.26,
                        language: Some("fr".into()),
                    }),
                    Tested::Fail => Err(SrttyError::Config("cannot load the whisper model".into())),
                    Tested::Interrupt => Err(SrttyError::Interrupted),
                }
            }
        }

        /// Everything a local `setup` borrows.
        struct Local {
            dir: tempfile::TempDir,
            catalog: Vec<CatalogModel>,
            fetcher: FakeFetcher,
            tester: FakeLocalTester,
            api_tester: FakeTester,
        }

        impl Local {
            fn new(fetch: Fetch, test_passes: bool) -> Self {
                Self::with_test(
                    fetch,
                    if test_passes {
                        Tested::Words(87)
                    } else {
                        Tested::Fail
                    },
                )
            }

            fn with_test(fetch: Fetch, tested: Tested) -> Self {
                Self {
                    dir: tempfile::tempdir().unwrap(),
                    catalog: test_catalog(),
                    fetcher: FakeFetcher::new(fetch),
                    tester: FakeLocalTester::new(tested),
                    api_tester: FakeTester::new(usable()),
                }
            }

            fn models_dir(&self) -> PathBuf {
                self.dir.path().join("models")
            }

            /// The home directory `~` stands for in this setup.
            fn home(&self) -> PathBuf {
                self.dir.path().join("home")
            }

            fn model_path(&self) -> PathBuf {
                self.models_dir().join("ggml-tiny-test.bin")
            }

            fn ctx(&self) -> SetupContext<'_> {
                SetupContext {
                    config_path: self.dir.path().join("config.toml"),
                    cache_dir: self.dir.path().join("cache"),
                    // Never reached: the local flow does not refresh compat.json.
                    compat_url: "http://127.0.0.1:9/compat.json".into(),
                    env: &env_empty,
                    tester: &self.api_tester,
                    local: LocalSetup {
                        models_dir: self.models_dir(),
                        catalog: &self.catalog,
                        fetcher: &self.fetcher,
                        tester: &self.tester,
                        home: Some(self.home()),
                    },
                }
            }

            fn config(&self) -> Config {
                Config::load(&self.dir.path().join("config.toml")).unwrap()
            }
        }

        fn assert_local_provider(cfg: &Config, model_path: &Path) {
            assert_eq!(cfg.default_provider.as_deref(), Some("local"));
            let provider = &cfg.providers["local"];
            assert_eq!(provider.kind, ProviderKind::Local);
            assert_eq!(provider.model_path.as_deref(), Some(model_path));
            assert_eq!(provider.api_key_env, None);
            assert_eq!(provider.base_url, None);
            assert_eq!(provider.model, None);
        }

        #[tokio::test]
        async fn local_option_is_available_with_the_feature() {
            let l = Local::new(Fetch::Write, true);
            let mut p = ScriptedPrompter::new(vec![
                Answer::Select(LOCAL),
                Answer::Select(TINY),
                Answer::Confirm(true),
                Answer::Confirm(false),
            ]);

            run_setup(&mut p, &l.ctx()).await.unwrap();

            assert_eq!(
                p.options[0][LOCAL],
                "Local Whisper (offline, runs on this machine)"
            );
            assert_eq!(p.asked(PROVIDER_MENU), 1);
        }

        #[tokio::test]
        async fn local_download_flow_writes_a_local_provider() {
            let l = Local::new(Fetch::Write, true);
            let mut p = ScriptedPrompter::new(vec![
                Answer::Select(LOCAL),
                Answer::Select(TINY),
                Answer::Confirm(true),
                Answer::Confirm(true),
            ]);

            let done = run_setup(&mut p, &l.ctx()).await.unwrap();

            assert!(done.written);
            assert_eq!(l.fetcher.calls(), 1);
            assert_eq!(l.tester.calls.load(Ordering::SeqCst), 1);
            assert_eq!(
                p.options[1],
                [
                    format!("{:<20} {:>7}  test model", "tiny-test", "0 MB"),
                    "Use a model file I already have".to_string(),
                ]
            );
            let dir = l.models_dir().display().to_string();
            let size = crate::local::catalog::human_size(15);
            assert_eq!(
                p.prompts,
                [
                    PROVIDER_MENU.to_string(),
                    MODEL_MENU.to_string(),
                    format!("Download ggml-tiny-test.bin ({size}) from Hugging Face to {dir}?"),
                    TEST_QUESTION.to_string(),
                ]
            );
            assert_local_provider(&l.config(), &l.model_path());
            assert!(
                !p.prompts.iter().any(|m| m.contains("Environment variable")),
                "{:?}",
                p.prompts
            );
            assert!(
                p.said
                    .contains(&"✔ Test passed: 87 words in 4.3 s (language: fr).".to_string()),
                "{:?}",
                p.said
            );
            let path = l.dir.path().join("config.toml").display().to_string();
            assert!(p.said.contains(&format!("Wrote {path}.")), "{:?}", p.said);
        }

        #[tokio::test]
        async fn local_already_downloaded_skips_fetch() {
            let l = Local::new(Fetch::Write, true);
            std::fs::create_dir_all(l.models_dir()).unwrap();
            std::fs::write(l.model_path(), MODEL_BYTES).unwrap();
            let mut p = ScriptedPrompter::new(vec![
                Answer::Select(LOCAL),
                Answer::Select(TINY),
                Answer::Confirm(false),
            ]);

            run_setup(&mut p, &l.ctx()).await.unwrap();

            assert!(
                p.options[1][TINY].ends_with("test model  (downloaded)"),
                "{:?}",
                p.options[1]
            );
            assert!(
                p.said
                    .contains(&"✔ ggml-tiny-test.bin is already downloaded.".to_string()),
                "{:?}",
                p.said
            );
            assert_eq!(l.fetcher.calls(), 0);
            assert!(!p.prompts.iter().any(|m| m.starts_with("Download ")));
            assert_local_provider(&l.config(), &l.model_path());
        }

        #[tokio::test]
        async fn local_declined_download_returns_to_the_menu() {
            let l = Local::new(Fetch::Write, true);
            let mut p = ScriptedPrompter::new(vec![
                Answer::Select(LOCAL),
                Answer::Select(TINY),
                Answer::Confirm(false),
                Answer::Select(TINY),
                Answer::Confirm(true),
                Answer::Confirm(false),
            ]);

            run_setup(&mut p, &l.ctx()).await.unwrap();

            assert_eq!(p.asked(MODEL_MENU), 2);
            assert_eq!(l.fetcher.calls(), 1);
            assert_local_provider(&l.config(), &l.model_path());
        }

        #[tokio::test]
        async fn local_download_failure_is_reported_and_asked_again() {
            let l = Local::new(Fetch::Fail, true);
            let own = l.dir.path().join("own.bin");
            std::fs::write(&own, MODEL_BYTES).unwrap();
            let mut p = ScriptedPrompter::new(vec![
                Answer::Select(LOCAL),
                Answer::Select(TINY),
                Answer::Confirm(true),
                Answer::Select(OWN_FILE),
                text(own.to_str().unwrap()),
                Answer::Confirm(false),
            ]);

            run_setup(&mut p, &l.ctx()).await.unwrap();

            assert!(
                p.said.contains(&"✘ Download failed: HTTP 404".to_string()),
                "{:?}",
                p.said
            );
            assert_eq!(p.asked(MODEL_MENU), 2);
            assert!(!l.model_path().exists());
            assert_local_provider(&l.config(), &own);
        }

        #[tokio::test]
        async fn local_existing_file_is_validated() {
            let l = Local::new(Fetch::Write, true);
            let mp3 = l.dir.path().join("song.mp3");
            std::fs::write(&mp3, b"ID3\x04\x00 not a model").unwrap();
            // A model under the (temporary) home directory, given as `~/…`.
            let home = l.home();
            std::fs::create_dir_all(&home).unwrap();
            let name = "my-model.bin";
            std::fs::write(home.join(name), MODEL_BYTES).unwrap();
            let mut p = ScriptedPrompter::new(vec![
                Answer::Select(LOCAL),
                Answer::Select(OWN_FILE),
                text(l.dir.path().join("missing.bin").to_str().unwrap()),
                text(mp3.to_str().unwrap()),
                text(&format!("~/{name}")),
                Answer::Confirm(true),
            ]);

            run_setup(&mut p, &l.ctx()).await.unwrap();

            assert_eq!(
                p.validator_errors,
                [
                    "No file at that path.",
                    "That file is not a ggml Whisper model."
                ]
            );
            assert!(
                p.prompts
                    .contains(&"Path to a ggml model file:".to_string())
            );
            assert_eq!(l.fetcher.calls(), 0);
            assert_eq!(l.tester.calls.load(Ordering::SeqCst), 1);
            assert_local_provider(&l.config(), &home.join(name));
        }

        #[tokio::test]
        async fn local_failed_test_can_be_rejected() {
            let l = Local::new(Fetch::Write, false);
            let mut p = ScriptedPrompter::new(vec![
                Answer::Select(LOCAL),
                Answer::Select(TINY),
                Answer::Confirm(true),
                Answer::Confirm(true),
                Answer::Confirm(false),
                Answer::Select(TINY),
                Answer::Confirm(false),
            ]);

            run_setup(&mut p, &l.ctx()).await.unwrap();

            assert!(
                p.said.contains(
                    &"✘ The model could not be used: cannot load the whisper model".to_string()
                ),
                "{:?}",
                p.said
            );
            assert_eq!(p.asked("Keep this model anyway?"), 1);
            assert_eq!(p.asked(MODEL_MENU), 2);
            assert_eq!(l.fetcher.calls(), 1);
            assert_local_provider(&l.config(), &l.model_path());
        }

        #[tokio::test]
        async fn local_test_without_words_is_a_failure() {
            let l = Local::with_test(Fetch::Write, Tested::Words(0));
            let mut p = ScriptedPrompter::new(vec![
                Answer::Select(LOCAL),
                Answer::Select(TINY),
                Answer::Confirm(true),
                Answer::Confirm(true),
                Answer::Confirm(true),
            ]);

            run_setup(&mut p, &l.ctx()).await.unwrap();

            assert!(
                p.said.contains(
                    &"✘ The model could not be used: it produced no words on the sample"
                        .to_string()
                ),
                "{:?}",
                p.said
            );
            assert!(
                !p.said.iter().any(|m| m.contains("Test passed")),
                "{:?}",
                p.said
            );
            assert_eq!(p.asked("Keep this model anyway?"), 1);
            assert_local_provider(&l.config(), &l.model_path());
        }

        #[tokio::test]
        async fn local_damaged_file_is_announced_before_the_download() {
            let l = Local::new(Fetch::Write, true);
            std::fs::create_dir_all(l.models_dir()).unwrap();
            // Right size, wrong content: listed as downloaded, then rejected.
            let damaged = vec![0u8; MODEL_BYTES.len()];
            std::fs::write(l.model_path(), damaged).unwrap();
            let mut p = ScriptedPrompter::new(vec![
                Answer::Select(LOCAL),
                Answer::Select(TINY),
                Answer::Confirm(true),
                Answer::Confirm(false),
            ]);

            run_setup(&mut p, &l.ctx()).await.unwrap();

            assert!(
                p.options[1][TINY].ends_with("(downloaded)"),
                "{:?}",
                p.options[1]
            );
            let checking = "Checking ggml-tiny-test.bin…".to_string();
            let damaged =
                "! ggml-tiny-test.bin is incomplete or damaged; it will be downloaded again."
                    .to_string();
            let at = |line: &String| p.said.iter().position(|m| m == line);
            assert!(at(&checking) < at(&damaged), "{:?}", p.said);
            assert!(at(&checking).is_some(), "{:?}", p.said);
            assert!(p.prompts.iter().any(|m| m.starts_with("Download ")));
            assert_eq!(l.fetcher.calls(), 1);
            assert_eq!(std::fs::read(l.model_path()).unwrap(), MODEL_BYTES);
        }

        #[cfg(unix)]
        #[tokio::test]
        async fn local_unreadable_file_is_reported_and_downloaded_again() {
            use std::os::unix::fs::PermissionsExt;
            let l = Local::new(Fetch::Write, true);
            std::fs::create_dir_all(l.models_dir()).unwrap();
            std::fs::write(l.model_path(), MODEL_BYTES).unwrap();
            std::fs::set_permissions(l.model_path(), std::fs::Permissions::from_mode(0o000))
                .unwrap();
            if std::fs::read(l.model_path()).is_ok() {
                eprintln!("skipped: the file stays readable (running as root)");
                return;
            }
            let mut p = ScriptedPrompter::new(vec![
                Answer::Select(LOCAL),
                Answer::Select(TINY),
                Answer::Confirm(true),
                Answer::Confirm(false),
            ]);

            run_setup(&mut p, &l.ctx()).await.unwrap();

            assert!(
                p.said
                    .iter()
                    .any(|m| m.starts_with("! Could not check ggml-tiny-test.bin: ")),
                "{:?}",
                p.said
            );
            assert!(p.prompts.iter().any(|m| m.starts_with("Download ")));
            assert_eq!(l.fetcher.calls(), 1);
            assert_local_provider(&l.config(), &l.model_path());
        }

        #[tokio::test]
        async fn local_interrupt_during_the_test_propagates() {
            let l = Local::with_test(Fetch::Write, Tested::Interrupt);
            let mut p = ScriptedPrompter::new(vec![
                Answer::Select(LOCAL),
                Answer::Select(TINY),
                Answer::Confirm(true),
                Answer::Confirm(true),
            ]);

            let err = run_setup(&mut p, &l.ctx()).await.unwrap_err();

            assert!(matches!(err, SrttyError::Interrupted), "{err}");
            assert_eq!(l.tester.calls.load(Ordering::SeqCst), 1);
            assert!(!l.dir.path().join("config.toml").exists());
        }

        #[tokio::test]
        async fn local_relative_path_is_written_absolute() {
            let l = Local::new(Fetch::Write, true);
            let own = l.dir.path().join("own.bin");
            std::fs::write(&own, MODEL_BYTES).unwrap();
            // `own` reached from the current directory through `..` components.
            let cwd = std::env::current_dir().unwrap();
            let up = "../".repeat(cwd.components().count() - 1);
            let relative = format!("{up}{}", own.strip_prefix("/").unwrap().display());
            assert!(Path::new(&relative).is_relative());
            let mut p = ScriptedPrompter::new(vec![
                Answer::Select(LOCAL),
                Answer::Select(OWN_FILE),
                text(&relative),
                Answer::Confirm(false),
            ]);

            run_setup(&mut p, &l.ctx()).await.unwrap();

            let written = l.config().providers["local"].model_path.clone().unwrap();
            assert!(written.is_absolute(), "{}", written.display());
            assert_eq!(written.canonicalize().unwrap(), own.canonicalize().unwrap());
        }

        #[tokio::test]
        async fn local_interrupt_during_download_propagates() {
            let l = Local::new(Fetch::Interrupt, true);
            let mut p = ScriptedPrompter::new(vec![
                Answer::Select(LOCAL),
                Answer::Select(TINY),
                Answer::Confirm(true),
            ]);

            let err = run_setup(&mut p, &l.ctx()).await.unwrap_err();

            assert!(matches!(err, SrttyError::Interrupted), "{err}");
            assert!(!l.dir.path().join("config.toml").exists());
        }
    }
}
