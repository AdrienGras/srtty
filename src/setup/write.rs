//! Writes the choices made by `srtty setup` into the config file, keeping
//! comments, other providers and unmanaged keys.

use std::path::Path;

use toml_edit::{DocumentMut, Item, Table, TableLike, Value};

use crate::config::{Config, check_key_var_name, redact_key_like, toml_error_position};
use crate::error::SrttyError;
use crate::fsutil::write_atomic;

/// The provider entry `setup` manages.
pub struct ProviderChoice {
    pub name: String,
    pub base_url: String,
    pub api_key_env: String,
    pub model: String,
}

/// Checks that an existing config file can be updated: it is TOML and
/// `Config` accepts it. The reason never quotes the file, which may hold a
/// pasted key.
pub fn check_existing(text: &str) -> Result<(), String> {
    text.parse::<DocumentMut>().map_err(|e| {
        format!(
            "it is not valid TOML{}",
            toml_error_position(text, e.span())
        )
    })?;
    toml::from_str::<Config>(text).map_err(|e| {
        format!(
            "invalid config{}: {}",
            toml_error_position(text, e.span()),
            redact_key_like(e.message())
        )
    })?;
    Ok(())
}

/// Applies `choice` to the config text: sets `default_provider` and the
/// `base_url`, `api_key_env` and `model` of `[providers.<name>]`, and removes
/// its `kind` (an API provider is the default). Everything else (comments,
/// other providers, unmanaged keys) is left as it was.
pub fn apply(existing: Option<&str>, choice: &ProviderChoice) -> Result<String, SrttyError> {
    check_key_var_name(&choice.api_key_env)?;
    edit_provider(existing, &choice.name, |provider| {
        provider.remove("kind");
        set_string(provider, "base_url", &choice.base_url);
        set_string(provider, "api_key_env", &choice.api_key_env);
        set_string(provider, "model", &choice.model);
    })
}

/// Makes `[providers.<name>]` a local whisper provider and the default one:
/// sets `kind = "local"` and `model_path`, and removes the API keys
/// (`base_url`, `api_key_env`, `model`, `request_mode`). Comments and other
/// providers are left as they were.
#[cfg(feature = "local")]
pub fn apply_local(
    existing: Option<&str>,
    name: &str,
    model_path: &Path,
) -> Result<String, SrttyError> {
    let model_path = model_path
        .to_str()
        .ok_or_else(|| SrttyError::Config("the model path must be valid UTF-8".into()))?;
    edit_provider(existing, name, |provider| {
        for key in ["base_url", "api_key_env", "model", "request_mode"] {
            provider.remove(key);
        }
        set_string(provider, "kind", "local");
        set_string(provider, "model_path", model_path);
    })
}

/// Parses the config text, sets `default_provider = name`, lets `edit` change
/// `[providers.<name>]` (created if missing), then checks that `Config`
/// accepts the result.
fn edit_provider(
    existing: Option<&str>,
    name: &str,
    edit: impl FnOnce(&mut dyn TableLike),
) -> Result<String, SrttyError> {
    let text = existing.unwrap_or("");
    let mut doc = text.parse::<DocumentMut>().map_err(|e| {
        SrttyError::Config(format!(
            "cannot update the config file: it is not valid TOML{}",
            toml_error_position(text, e.span())
        ))
    })?;

    set_string(doc.as_table_mut(), "default_provider", name);

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
        .entry(name)
        .or_insert_with(|| Item::Table(Table::new()))
        .as_table_like_mut()
        .ok_or_else(|| not_a_table(&format!("providers.{name}")))?;
    edit(provider);

    let out = doc.to_string();
    toml::from_str::<Config>(&out).map_err(|e| {
        let message = redact_key_like(e.message());
        // `setup` only writes known keys: an unknown one was already there.
        if e.message().starts_with("unknown field") {
            SrttyError::Config(format!("cannot update the existing config file: {message}"))
        } else {
            SrttyError::Config(format!("the updated config would be invalid: {message}"))
        }
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
    use crate::config::{Config, ProviderKind, RequestMode};

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

    #[test]
    fn apply_resets_the_provider_kind() {
        let existing = "[providers.openrouter]\nkind = \"local\"\nmodel_path = \"/m.bin\"\n";
        let text = apply(Some(existing), &choice()).unwrap();
        assert!(!text.contains("kind"), "{text}");
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.providers["openrouter"].kind, ProviderKind::Api);
    }

    #[test]
    fn apply_refuses_a_key_as_variable_name_without_echo() {
        let bad = ProviderChoice {
            api_key_env: "sk-or-v1-0123secret".into(),
            ..choice()
        };
        let err = apply(None, &bad).unwrap_err();
        assert!(matches!(err, SrttyError::Config(_)), "{err}");
        let msg = err.to_string();
        assert!(!msg.contains("0123secret"), "{msg}");
        assert!(msg.contains("name of an environment variable"), "{msg}");
    }

    #[test]
    fn apply_blames_the_existing_file_for_an_unknown_field() {
        let err = apply(Some("[providers.groq]\nmodle = \"x\"\n"), &choice())
            .unwrap_err()
            .to_string();
        assert!(err.contains("the existing config file"), "{err}");
        assert!(err.contains("unknown field `modle`"), "{err}");
    }
    #[cfg(feature = "local")]
    #[test]
    fn apply_local_replaces_api_keys_and_keeps_comments() {
        let existing = "# my config\ndefault_provider = \"groq\"\n\n# my whisper server\n[providers.local]\nbase_url = \"http://localhost:8000/v1\"\napi_key_env = \"LOCAL_KEY\"\nmodel = \"whisper\"\nrequest_mode = \"json-base64\"\n\n[providers.groq]\napi_key_env = \"GROQ_API_KEY\"\nmodel = \"whisper-large-v3\"\n";
        let text =
            apply_local(Some(existing), "local", Path::new("/models/ggml-base.bin")).unwrap();
        assert!(text.contains("# my config"), "{text}");
        assert!(text.contains("# my whisper server"), "{text}");
        for gone in [
            "base_url",
            "LOCAL_KEY",
            "model = \"whisper\"",
            "request_mode",
        ] {
            assert!(!text.contains(gone), "{gone} left in:\n{text}");
        }
        let cfg: Config = toml::from_str(&text).unwrap();
        assert_eq!(cfg.default_provider.as_deref(), Some("local"));
        let local = &cfg.providers["local"];
        assert_eq!(local.kind, ProviderKind::Local);
        assert_eq!(
            local.model_path.as_deref(),
            Some(Path::new("/models/ggml-base.bin"))
        );
        assert_eq!(local.api_key_env, None);
        assert_eq!(local.base_url, None);
        assert_eq!(local.model, None);
        assert_eq!(local.request_mode, RequestMode::default());
        assert_eq!(
            cfg.providers["groq"].model.as_deref(),
            Some("whisper-large-v3")
        );
        assert_eq!(
            cfg.providers["groq"].api_key_env.as_deref(),
            Some("GROQ_API_KEY")
        );
    }

    #[cfg(all(feature = "local", unix))]
    #[test]
    fn apply_local_refuses_a_non_utf8_path() {
        use std::os::unix::ffi::OsStrExt;
        let path = Path::new(std::ffi::OsStr::from_bytes(b"/models/\xff.bin"));
        let err = apply_local(None, "local", path).unwrap_err();
        assert!(matches!(err, SrttyError::Config(_)), "{err}");
        assert_eq!(err.to_string(), "the model path must be valid UTF-8");
    }
}
