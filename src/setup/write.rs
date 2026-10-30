//! Writes the choices made by `srtty setup` into the config file, keeping
//! comments, other providers and unmanaged keys.

use std::path::Path;

use toml_edit::{DocumentMut, Item, Table, TableLike, Value};

use crate::config::{Config, redact_key_like};
use crate::error::SrttyError;
use crate::fsutil::write_atomic;

/// The provider entry `setup` manages.
pub struct ProviderChoice {
    pub name: String,
    pub base_url: String,
    pub api_key_env: String,
    pub model: String,
}

/// Applies `choice` to the config text: sets `default_provider` and the
/// `base_url`, `api_key_env` and `model` of `[providers.<name>]`. Everything
/// else (comments, other providers, unmanaged keys) is left as it was.
pub fn apply(existing: Option<&str>, choice: &ProviderChoice) -> Result<String, SrttyError> {
    let text = existing.unwrap_or("");
    let mut doc = text.parse::<DocumentMut>().map_err(|e| {
        // The default rendering quotes the offending source line, which may
        // hold a pasted API key: report only the position.
        let position = e
            .span()
            .map(|span| {
                let before = text.get(..span.start).unwrap_or(text);
                let line = before.matches('\n').count() + 1;
                let column = before.rsplit('\n').next().unwrap_or("").chars().count() + 1;
                format!(" at line {line}, column {column}")
            })
            .unwrap_or_default();
        SrttyError::Config(format!(
            "cannot update the config file: it is not valid TOML{position}"
        ))
    })?;

    set_string(doc.as_table_mut(), "default_provider", &choice.name);

    let providers = doc
        .entry("providers")
        .or_insert_with(|| {
            let mut t = Table::new();
            t.set_implicit(true);
            Item::Table(t)
        })
        .as_table_like_mut()
        .ok_or_else(|| not_a_table("providers"))?;
    let provider = providers
        .entry(&choice.name)
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_like_mut()
        .ok_or_else(|| not_a_table(&format!("providers.{}", choice.name)))?;
    set_string(provider, "base_url", &choice.base_url);
    set_string(provider, "api_key_env", &choice.api_key_env);
    set_string(provider, "model", &choice.model);

    let out = doc.to_string();
    toml::from_str::<Config>(&out).map_err(|e| {
        SrttyError::Config(format!(
            "the updated config would be invalid: {}",
            redact_key_like(e.message())
        ))
    })?;
    Ok(out)
}

/// Writes the config text atomically, creating the parent directory.
pub fn write_config(path: &Path, text: &str) -> Result<(), SrttyError> {
    write_atomic(path, text.as_bytes())?;
    Ok(())
}

fn not_a_table(key: &str) -> SrttyError {
    SrttyError::Config(format!(
        "cannot update the config file: `{key}` is not a table"
    ))
}

/// Sets `key` to a string. An existing key is updated in place, so the
/// comments around it survive.
fn set_string(table: &mut dyn TableLike, key: &str, new: &str) {
    let mut value = Value::from(new);
    if let Some(item) = table.get_mut(key) {
        if let Some(old) = item.as_value() {
            *value.decor_mut() = old.decor().clone();
        }
        *item = Item::Value(value);
    } else {
        table.insert(key, Item::Value(value));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, RequestMode};

    fn choice() -> ProviderChoice {
        ProviderChoice {
            name: "openrouter".into(),
            base_url: "https://openrouter.ai/api/v1".into(),
            api_key_env: "OPENROUTER_API_KEY".into(),
            model: "x-ai/grok-stt-1.0".into(),
        }
    }

    #[test]
    fn apply_creates_a_minimal_config() {
        let text = apply(None, &choice()).unwrap();
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.default_provider.as_deref(), Some("openrouter"));
        assert_eq!(
            cfg.providers["openrouter"].model.as_deref(),
            Some("x-ai/grok-stt-1.0")
        );
    }

    #[test]
    fn apply_keeps_comments_and_other_providers() {
        let existing = "# my config\ndefault_provider = \"groq\"\n\n[providers.groq]\n# fast\nbase_url = \"https://api.groq.com/openai/v1\"\napi_key_env = \"GROQ_API_KEY\"\nmodel = \"whisper-large-v3\"\n\n[providers.openrouter]\nrequest_mode = \"json-base64\"\nmodel = \"old\"\n\n[subtitles]\nmax_lines = 1\n";
        let text = apply(Some(existing), &choice()).unwrap();
        assert!(text.contains("# my config") && text.contains("# fast"));
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.default_provider.as_deref(), Some("openrouter"));
        assert_eq!(
            cfg.providers["groq"].model.as_deref(),
            Some("whisper-large-v3")
        );
        assert_eq!(
            cfg.providers["openrouter"].request_mode,
            RequestMode::JsonBase64
        );
        assert_eq!(
            cfg.providers["openrouter"].model.as_deref(),
            Some("x-ai/grok-stt-1.0")
        );
        assert_eq!(cfg.subtitles.max_lines, 1);
    }

    #[test]
    fn apply_refuses_invalid_toml_without_echo() {
        let err = apply(Some("api_key_env = sk-or-v1-0123secret\n"), &choice())
            .unwrap_err()
            .to_string();
        assert!(!err.contains("0123secret"), "{err}");
        assert!(err.contains("not valid TOML at line 1"), "{err}");
    }

    #[test]
    fn apply_updates_an_inline_provider_table() {
        let existing = "providers = { openrouter = { request_mode = \"json-base64\", model = \"old\" }, groq = { model = \"w\" } }\n";
        let text = apply(Some(existing), &choice()).unwrap();
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(
            cfg.providers["openrouter"].request_mode,
            RequestMode::JsonBase64
        );
        assert_eq!(
            cfg.providers["openrouter"].model.as_deref(),
            Some("x-ai/grok-stt-1.0")
        );
        assert_eq!(cfg.providers["groq"].model.as_deref(), Some("w"));
    }

    #[test]
    fn apply_updates_a_dotted_key_provider() {
        let existing =
            "[providers]\nopenrouter.model = \"old\"\nopenrouter.request_mode = \"json-base64\"\n";
        let text = apply(Some(existing), &choice()).unwrap();
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(
            cfg.providers["openrouter"].request_mode,
            RequestMode::JsonBase64
        );
        assert_eq!(
            cfg.providers["openrouter"].api_key_env.as_deref(),
            Some("OPENROUTER_API_KEY")
        );
    }

    #[test]
    fn apply_refuses_a_non_table_providers_entry() {
        let err = apply(Some("providers = 3\n"), &choice()).unwrap_err();
        assert!(matches!(err, SrttyError::Config(_)), "{err}");
    }

    #[test]
    fn write_config_creates_the_file() {
        let d = tempfile::tempdir().unwrap();
        let path = d.path().join("sub/config.toml");
        write_config(&path, "a = 1\n").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "a = 1\n");
    }
}
