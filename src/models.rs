//! `srtty models`: the provider's transcription models with price and
//! compatibility status.

use std::cmp::Ordering;
use std::io::Write;

use serde::Serialize;

use crate::app::print_stdout;
use crate::cli::Cli;
use crate::compat::{
    CompatList, CompatSource, ModelStatus, REFRESH_TIMEOUT, compat_url, provider_key, refresh,
    status_of,
};
use crate::config::{ApiKey, Config, ProviderKind, check_key_var_name};
use crate::error::SrttyError;
use crate::paths;
use crate::providers::{ListedModel, Listing, list_models, preset};

/// The provider whose models are listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub name: String,
    pub base_url: String,
    pub api_key_env: Option<String>,
}

/// One line of the model list.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ModelRow {
    pub id: String,
    pub usd_per_hour: Option<f64>,
    pub token_priced: bool,
    pub url: Option<String>,
    pub status: ModelStatus,
    pub words: Option<bool>,
    pub fine_segments: Option<bool>,
    pub checked_at: Option<String>,
}

/// The provider to list: `requested` (the flag), else `SRTTY_PROVIDER`, else
/// the config's default provider, else OpenRouter. A provider of the config
/// file shadows a preset of the same name.
pub fn listing_target(
    cfg: &Config,
    requested: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Target, SrttyError> {
    let name = requested
        .map(str::to_string)
        .or_else(|| env("SRTTY_PROVIDER").filter(|v| !v.is_empty()))
        .or_else(|| cfg.default_provider.clone())
        .unwrap_or_else(|| "openrouter".to_string());
    if let Some(p) = cfg.providers.get(&name) {
        if p.kind == ProviderKind::Local {
            return Err(SrttyError::Usage(format!(
                "provider `{name}` is a local backend and has no model list"
            )));
        }
        let base_url = p.base_url.clone().ok_or_else(|| {
            SrttyError::Config(format!(
                "provider `{name}` has no `base_url` in the config file"
            ))
        })?;
        return Ok(Target {
            name,
            base_url,
            api_key_env: p.api_key_env.clone(),
        });
    }
    match preset(&name) {
        Some(p) => Ok(Target {
            name,
            base_url: p.base_url.to_string(),
            api_key_env: Some(p.api_key_env.to_string()),
        }),
        None => Err(SrttyError::Config(format!(
            "unknown provider `{name}`; known: openrouter, openai, groq or a provider from the config file"
        ))),
    }
}

/// Merges a provider listing with the compatibility list. Token-priced models
/// come last; each group is sorted by status, then price (unknown last), then id.
pub fn rows(listed: &[ListedModel], compat: &CompatList, provider: Option<&str>) -> Vec<ModelRow> {
    let mut rows: Vec<ModelRow> = listed
        .iter()
        .map(|m| {
            let entry = provider.and_then(|p| compat.find(p, &m.id));
            let usd_per_hour = if m.token_priced {
                None
            } else {
                m.usd_per_s
                    .map(|per_s| per_s * 3600.0)
                    .or_else(|| entry.and_then(|e| e.usd_per_hour))
            };
            ModelRow {
                id: m.id.clone(),
                usd_per_hour,
                token_priced: m.token_priced,
                url: m.url.clone(),
                status: status_of(entry),
                words: entry.map(|e| e.words),
                fine_segments: entry.map(|e| e.fine_segments),
                checked_at: entry.and_then(|e| e.checked_at.clone()),
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        let price = match (a.usd_per_hour, b.usd_per_hour) {
            (Some(x), Some(y)) => x.total_cmp(&y),
            (Some(_), None) => Ordering::Less,
            (None, Some(_)) => Ordering::Greater,
            (None, None) => Ordering::Equal,
        };
        a.token_priced
            .cmp(&b.token_priced)
            .then(status_rank(a.status).cmp(&status_rank(b.status)))
            .then(price)
            .then_with(|| a.id.cmp(&b.id))
    });
    rows
}

fn status_rank(status: ModelStatus) -> u8 {
    match status {
        ModelStatus::CompatibleSpeakers => 0,
        ModelStatus::Compatible => 1,
        ModelStatus::Unknown => 2,
        ModelStatus::Incompatible => 3,
    }
}

fn price_cell(row: &ModelRow) -> String {
    match row.usd_per_hour {
        Some(usd) => format!("{usd:.2}"),
        None if row.token_priced => "token".to_string(),
        None => "?".to_string(),
    }
}

/// The table `srtty models` prints. Token-priced rows end with the model page
/// URL, as plain text: terminals make it clickable and pipes stay clean.
pub fn render_table(rows: &[ModelRow]) -> String {
    let prices: Vec<String> = rows.iter().map(price_cell).collect();
    let id_width = rows
        .iter()
        .map(|r| r.id.len())
        .fold("MODEL".len(), usize::max);
    let price_width = prices
        .iter()
        .map(String::len)
        .fold("$/HOUR".len(), usize::max);
    let status_width = rows
        .iter()
        .map(|r| r.status.label().len())
        .fold("STATUS".len(), usize::max);
    let mut lines = vec![format!(
        "{:<id_width$}  {:>price_width$}  STATUS",
        "MODEL", "$/HOUR"
    )];
    for (row, price) in rows.iter().zip(&prices) {
        let status = row.status.label();
        let line = match row.url.as_deref().filter(|_| row.token_priced) {
            Some(url) => format!(
                "{:<id_width$}  {price:>price_width$}  {status:<status_width$}  {url}",
                row.id
            ),
            None => format!("{:<id_width$}  {price:>price_width$}  {status}", row.id),
        };
        lines.push(line);
    }
    lines.join("\n")
}

/// Lists the models of `target` and merges them with `compat`.
pub async fn fetch_rows(
    target: &Target,
    key: Option<&ApiKey>,
    compat: &CompatList,
) -> Result<Vec<ModelRow>, SrttyError> {
    let listing = Listing::for_base_url(&target.base_url);
    let listed = list_models(listing, &target.base_url, key).await?;
    Ok(rows(&listed, compat, provider_key(&target.base_url)))
}

/// Reads the provider's key from the environment. A key is only required for
/// OpenAI-style listings; OpenRouter lists its models without one.
fn read_key(
    target: &Target,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Option<ApiKey>, SrttyError> {
    let Some(var) = target.api_key_env.as_deref() else {
        return Ok(None);
    };
    check_key_var_name(var)?;
    match env(var).filter(|v| !v.is_empty()) {
        Some(value) => Ok(Some(ApiKey(value))),
        None if Listing::for_base_url(&target.base_url) == Listing::OpenAi => {
            Err(SrttyError::Config(format!(
                "environment variable {var} is not set; it is needed to list the models of `{}`",
                target.name
            )))
        }
        None => Ok(None),
    }
}

#[derive(Serialize)]
struct CompatInfo<'a> {
    generated_at: &'a str,
    source: CompatSource,
}

#[derive(Serialize)]
struct ModelsOutput<'a> {
    provider: &'a str,
    compat: CompatInfo<'a>,
    models: &'a [ModelRow],
    warnings: &'a [String],
}

pub async fn run(cli: &Cli) -> Result<(), SrttyError> {
    let env = |name: &str| std::env::var(name).ok();
    let cfg = Config::load(&paths::config_file())?;
    let target = listing_target(&cfg, cli.provider.as_deref(), &env)?;
    let key = read_key(&target, &env)?;
    let refreshed = refresh(&paths::cache_dir(), &compat_url(), REFRESH_TIMEOUT).await;
    let rows = fetch_rows(&target, key.as_ref(), &refreshed.list).await?;
    let generated_at = refreshed.list.generated_at.as_str();
    let warnings: Vec<String> = refreshed.warning.iter().cloned().collect();

    if cli.json {
        let output = ModelsOutput {
            provider: &target.name,
            compat: CompatInfo {
                generated_at,
                source: refreshed.source,
            },
            models: &rows,
            warnings: &warnings,
        };
        return print_stdout(&serde_json::to_string(&output).map_err(std::io::Error::from)?);
    }
    print_stdout(&render_table(&rows))?;
    let mut err = std::io::stderr().lock();
    for warning in &warnings {
        writeln!(err, "{warning}")?;
    }
    writeln!(
        err,
        "compatibility list: {generated_at} ({})",
        refreshed.source.name()
    )?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lm(id: &str, usd_per_s: Option<f64>) -> ListedModel {
        ListedModel {
            id: id.into(),
            usd_per_s,
            token_priced: false,
            url: None,
        }
    }

    fn gemini() -> ListedModel {
        ListedModel {
            id: "google/gemini-3.5-transcribe".into(),
            usd_per_s: None,
            token_priced: true,
            url: Some("https://openrouter.ai/google/gemini-3.5-transcribe".into()),
        }
    }

    fn sample() -> Vec<ListedModel> {
        vec![
            lm("new/model", Some(0.00001)),
            lm("google/chirp-3", Some(0.0002667)),
            lm("openai/whisper-1", Some(0.0001)),
            lm("x-ai/grok-stt-1.0", None),
            gemini(),
        ]
    }

    #[test]
    fn rows_merge_listing_and_compat_and_sort() {
        let r = rows(&sample(), &CompatList::embedded(), Some("openrouter"));
        let order: Vec<_> = r.iter().map(|m| (m.id.as_str(), m.status)).collect();
        assert_eq!(
            order,
            [
                ("x-ai/grok-stt-1.0", ModelStatus::CompatibleSpeakers),
                ("openai/whisper-1", ModelStatus::Compatible),
                ("new/model", ModelStatus::Unknown),
                ("google/chirp-3", ModelStatus::Incompatible),
                ("google/gemini-3.5-transcribe", ModelStatus::Compatible),
            ]
        );
        assert_eq!(r[0].usd_per_hour, Some(0.1)); // falls back to compat.json
        assert_eq!(r[4].usd_per_hour, None); // token-priced: no fallback
        assert!(r[4].token_priced);
        assert_eq!(r[2].words, None);
        assert_eq!(r[0].words, Some(true));
    }

    #[test]
    fn rows_sort_by_price_then_id_within_a_status() {
        let listed = vec![
            lm("b/unknown", Some(0.0002)),
            lm("c/unknown", None),
            lm("a/unknown", Some(0.0002)),
            lm("d/unknown", Some(0.0001)),
        ];
        let r = rows(&listed, &CompatList::embedded(), Some("openrouter"));
        let ids: Vec<_> = r.iter().map(|m| m.id.as_str()).collect();
        assert_eq!(ids, ["d/unknown", "a/unknown", "b/unknown", "c/unknown"]);
    }

    #[test]
    fn rows_without_a_provider_key_have_no_compat_data() {
        let r = rows(&sample(), &CompatList::embedded(), None);
        assert!(r.iter().all(|m| m.status == ModelStatus::Unknown));
        let grok = r.iter().find(|m| m.id == "x-ai/grok-stt-1.0").unwrap();
        assert_eq!(grok.usd_per_hour, None);
    }

    fn row(id: &str, price: Option<f64>, status: ModelStatus) -> ModelRow {
        ModelRow {
            id: id.into(),
            usd_per_hour: price,
            token_priced: false,
            url: None,
            status,
            words: None,
            fine_segments: None,
            checked_at: None,
        }
    }

    #[test]
    fn table_shows_unknown_price_as_question_mark() {
        let table = render_table(&[
            row("whisper-1", Some(0.36), ModelStatus::Compatible),
            row("new/model", None, ModelStatus::Unknown),
        ]);
        let lines: Vec<_> = table.lines().collect();
        assert_eq!(lines[0], "MODEL      $/HOUR  STATUS");
        assert_eq!(lines[1], "whisper-1    0.36  compatible");
        assert_eq!(lines[2], "new/model       ?  unknown");
    }

    #[test]
    fn table_puts_token_priced_models_last_with_their_url() {
        let r = rows(&sample(), &CompatList::embedded(), Some("openrouter"));
        let table = render_table(&r);
        let last = table.lines().last().unwrap();
        assert!(last.starts_with("google/gemini-3.5-transcribe"), "{last}");
        assert!(
            last.contains("token") && last.contains("compatible"),
            "{last}"
        );
        assert!(
            last.ends_with("https://openrouter.ai/google/gemini-3.5-transcribe"),
            "{last}"
        );
        assert_eq!(table.matches("https://").count(), 1);
        assert!(!table.contains('\x1b'));
        assert!(table.lines().nth(1).unwrap().contains("0.10"));
    }

    fn cfg(toml: &str) -> Config {
        toml::from_str(toml).unwrap()
    }

    fn no_env(_: &str) -> Option<String> {
        None
    }

    #[test]
    fn listing_target_resolution() {
        let empty = Config::default();
        let t = listing_target(&empty, None, &no_env).unwrap();
        assert_eq!(t.name, "openrouter");
        assert_eq!(t.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(t.api_key_env.as_deref(), Some("OPENROUTER_API_KEY"));

        let t = listing_target(&empty, Some("groq"), &no_env).unwrap();
        assert_eq!(t.base_url, "https://api.groq.com/openai/v1");
        assert_eq!(t.api_key_env.as_deref(), Some("GROQ_API_KEY"));

        let mine = cfg(
            "default_provider = \"mine\"\n[providers.mine]\nbase_url = \"http://h/v1\"\napi_key_env = \"MINE_KEY\"\n",
        );
        let t = listing_target(&mine, None, &no_env).unwrap();
        assert_eq!(
            t,
            Target {
                name: "mine".into(),
                base_url: "http://h/v1".into(),
                api_key_env: Some("MINE_KEY".into()),
            }
        );
        // A configured provider shadows a preset of the same name.
        let shadow = cfg("[providers.groq]\nbase_url = \"http://g/v1\"\n");
        let t = listing_target(&shadow, Some("groq"), &no_env).unwrap();
        assert_eq!(t.base_url, "http://g/v1");
        assert_eq!(t.api_key_env, None);

        let local = cfg("[providers.loc]\nkind = \"local\"\n");
        let err = listing_target(&local, Some("loc"), &no_env).unwrap_err();
        assert!(matches!(&err, SrttyError::Usage(m) if m.contains("local backend")));

        let no_url = cfg("[providers.bare]\napi_key_env = \"K\"\n");
        let err = listing_target(&no_url, Some("bare"), &no_env).unwrap_err();
        assert!(matches!(&err, SrttyError::Config(m) if m.contains("base_url")));

        let err = listing_target(&empty, Some("nope"), &no_env).unwrap_err();
        assert!(matches!(&err, SrttyError::Config(m)
            if m.contains("unknown provider `nope`") && m.contains("openrouter, openai, groq")));
    }

    #[test]
    fn listing_target_precedence_is_flag_then_env_then_config() {
        let c = cfg("default_provider = \"openai\"\n");
        let env = |k: &str| (k == "SRTTY_PROVIDER").then(|| "groq".to_string());
        assert_eq!(listing_target(&c, None, &env).unwrap().name, "groq");
        assert_eq!(
            listing_target(&c, Some("openrouter"), &env).unwrap().name,
            "openrouter"
        );
        let empty_env = |k: &str| (k == "SRTTY_PROVIDER").then(String::new);
        assert_eq!(listing_target(&c, None, &empty_env).unwrap().name, "openai");
    }
}
