# srtty — Plan 2 : `models`, `setup`, `compat.json` vivant

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal :** un utilisateur sans config lance `srtty setup`, choisit un fournisseur et un modèle dans une liste qui affiche prix et compatibilité, teste éventuellement ce modèle sur un extrait embarqué de 30 s, puis obtient une config utilisable. `srtty models` affiche la même liste sans interaction (`--json` possible). La liste de compatibilité se rafraîchit depuis GitHub dans `setup` et `models`, et une CI hebdomadaire la régénère avec `srtty compat probe`.

**Architecture :** on ajoute des modules autour du cœur du plan 1 sans le modifier en profondeur. `providers.rs` liste les modèles d'un fournisseur (format OpenRouter ou OpenAI). `compat.rs` gagne le téléchargement et le cache. `models.rs` fusionne la liste du fournisseur et `compat.json`. `probe.rs` classe un modèle à partir d'une vraie transcription de l'extrait embarqué. `setup` et `compat probe` l'utilisent tous les deux, avec le même `OpenAiCompatible` et le même `transcribe_with_retry` que les runs. Les prompts de `setup` passent par un trait `Prompter`, ce qui permet de tester le parcours sans terminal ; l'implémentation réelle utilise `inquire`. La config est modifiée avec `toml_edit` pour garder les commentaires et les autres fournisseurs.

**Tech Stack :** celle du plan 1 (Rust 1.99, édition 2024, tokio, reqwest 0.13, clap 4.6, serde, toml 1.1, miette, wiremock, assert_cmd), plus `inquire` 0.9 et `toml_edit` 0.25 (ajoutés avec `cargo add`, qui prend la dernière version compatible). GitHub Actions pour la CI hebdomadaire.

**Spec :** `docs/superpowers/specs/2026-10-30-srtty-design.md`, sections 2 (anti-objectifs), 5.7, 5.8, 5.14, 5.15. Mesures et prix : `docs/spikes/2026-10-30-openrouter-stt.md`.

**Découpage en plans :** 1. cœur (livré, PR #1) ; **2. ce plan** ; 3. tableau de bord Ratatui ; 4. backend local whisper-rs et modèles ggml dans `setup` ; 5. distribution (dist, binstall, Homebrew, CI des PR, licences, README).

## Global Constraints

- Tout texte destiné à l'utilisateur est en anglais : prompts de `setup`, messages, aide, tableau de `models`. Les docs du dépôt sont en français.
- La clé d'API n'apparaît nulle part : ni sortie, ni log, ni message d'erreur, ni message de validation d'un prompt, ni fichier de config. On ne cite que le **nom** de la variable.
- Préfixes de clé connus, refusés partout où un nom de variable est attendu : `sk-`, `gsk_`, `xai-` (`sk-or-` est couvert par `sk-`).
- Les seuls appels réseau hors transcription sont : la liste des modèles du fournisseur et `compat.json`, dans `setup` et `models` uniquement ; et, pour `compat probe` (commande cachée, CI), la liste et les endpoints OpenRouter. Un run de transcription ne fait toujours aucun autre appel.
- `compat.json` distant : `https://raw.githubusercontent.com/AdrienGras/srtty/main/compat.json`, timeout 3 s. La variable `SRTTY_COMPAT_URL` remplace l'URL (tests). Copie enregistrée dans `<cache>/compat.json`. Un fichier dont `schema != 1` est refusé.
- Aucune dépense sans annonce : le test de `setup` est proposé avec son coût estimé, refusé par défaut. `compat probe` saute les modèles au-delà de `0.001` $/s ou sans prix par seconde, et s'arrête avant de dépasser `0.50` $ par run.
- Codes de retour inchangés : `0` succès, `1` usage, config ou E/S, `2` réseau, API ou modèle inadapté, `3` ffmpeg, `130` interruption (y compris Échap ou Ctrl-C dans un prompt de `setup`).
- `--json` : un seul objet JSON sur stdout, rien d'autre. Ça vaut aussi pour les erreurs de clap.
- Écriture de fichiers (config, cache `compat.json`, `compat.json` du dépôt) : fichier temporaire dans le même dossier, puis renommage.
- CI locale de chaque tâche : `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` et `cargo test` passent avant le commit.

## Review Focus

1. **GitHub injoignable, lent ou qui renvoie autre chose que la liste** (pas de réseau, 404, page HTML, JSON de schéma 2) pendant `models` ou `setup` : la commande marche avec la copie en cache ou embarquée, affiche un avertissement, et la copie en cache n'est pas écrasée. → test `refresh_keeps_cache_on_bad_remote` (tâche 2).
2. **Clé collée à la place du nom de variable** (`gsk_…`, `sk-or-…`, `xai-…`), dans le prompt de `setup` ou dans `api_key_env` : refusée, sans que la valeur apparaisse dans le message ni dans le fichier écrit. → tests `key_pasted_into_api_key_env_is_rejected_for_every_prefix` (tâche 1) et `setup_rejects_a_pasted_key_without_echo` (tâche 9).
3. **`setup` relancé sur une config existante** qui contient des commentaires et d'autres fournisseurs : un refus laisse le fichier identique à l'octet près ; une acceptation garde les commentaires, les autres fournisseurs et les clés non gérées (`request_mode`). → tests `apply_keeps_comments_and_other_providers` (tâche 8) et `declining_overwrite_leaves_file_untouched` (tâche 9).
4. **Sonde interrompue par le budget ou par une erreur passagère** : les entrées déjà connues ne sont pas rétrogradées en « incompatible » ; un run sans changement réel ne réécrit pas le fichier, pour que la CI ne commite rien. → tests `budget_exhaustion_keeps_previous_entries`, `transient_failure_keeps_previous_entry`, `unchanged_content_is_not_rewritten` (tâche 6).
5. **`models` pour un fournisseur dont la liste n'est pas accessible** (OpenAI sans `OPENAI_API_KEY`, URL personnalisée sans `/models`) : erreur claire avec le code 1 qui nomme la variable, ou code 2 pour une erreur HTTP, jamais de panic ni de clé dans le message. → tests `models_without_key_names_the_variable` et `models_http_error_exits_2` (tâche 4).

---

## Structure des fichiers

```
assets/sample-30s.ogg          # extrait de 30 s, une voix (Barbe bleue), Opus 32 kbit/s mono 16 kHz
assets/speakers-45s.ogg        # extrait de 45 s, ≥ 3 voix (Précieuses ridicules, scène 10)
assets/README.md               # provenance LibriVox (domaine public), commandes ffmpeg de génération
.github/workflows/compat.yml   # CI hebdomadaire de compat.json
src/fsutil.rs                  # write_atomic
src/config.rs                  # + looks_like_api_key, redact_key_like, KeyState, deny_unknown_fields
src/compat.rs                  # + parse avec schéma, refresh (GitHub → cache), ModelStatus, dates UTC
src/providers.rs               # presets, liste des modèles (OpenRouter / OpenAI), tags d'endpoints
src/models.rs                  # ModelRow, fusion liste + compat, tableau, commande `models`
src/probe.rs                   # extraits embarqués, classement d'une transcription, sonde d'un modèle
src/compat_probe.rs            # `srtty compat probe` : orchestration, budget, fusion, écriture
src/setup/mod.rs               # parcours de `setup` sur un Prompter
src/setup/prompt.rs            # trait Prompter + InquirePrompter
src/setup/write.rs             # modification de la config avec toml_edit
src/cli.rs, src/app.rs, src/main.rs, src/error.rs   # câblage
tests/cli.rs                   # bout en bout : models, setup hors TTY, erreurs clap en JSON
tests/fixtures/openrouter_models.json, openrouter_endpoints_grok.json
docs/validation/2026-10-30-plan2.md
```

---

### Task 1 : dette du plan 1 (config, schéma, `config show`, erreurs clap en JSON)

**Files:**
- Modify: `src/config.rs`, `src/app.rs` (`show_config`, `transcribe`), `src/compat.rs` (`load`), `src/main.rs`, `src/cli.rs`
- Test: modules de test de ces fichiers, `tests/cli.rs`

**Interfaces:**
- Produces :
  - `pub fn looks_like_api_key(s: &str) -> bool` (préfixes `sk-`, `gsk_`, `xai-`, sensibles à la casse) et `pub fn is_env_var_name(name: &str) -> bool` (devient public), dans `config.rs`.
  - `pub fn redact_key_like(text: &str) -> String` : remplace par `<redacted>` chaque mot (séparé par une espace, `` ` ``, `'` ou `"`) qui `looks_like_api_key`.
  - `pub enum KeyState { Set(ApiKey), Unset { var: String }, NotNeeded }` ; `Resolved.api_key: KeyState` ; `impl Resolved { pub fn require_key(&self) -> Result<ApiKey, SrttyError> }` (erreur `Config("environment variable {var} is not set")`).
  - `pub const COMPAT_SCHEMA: u32 = 1;` et `pub fn parse_compat(text: &str) -> Result<CompatList, String>` dans `compat.rs` (refuse `schema != 1` avec `"unsupported compat.json schema {n}"`).
  - Flags `--json` et `--provider` déclarés `global = true` dans `cli.rs`.

- [ ] **Step 1 : écrire les tests qui échouent**

Dans `config.rs` :

```rust
#[test]
fn key_pasted_into_api_key_env_is_rejected_for_every_prefix() {
    for key in ["gsk_0123secret", "sk-or-v1-0123secret", "sk-proj-0123secret", "xai-0123secret"] {
        let cfg: Config = toml::from_str(&format!(
            "default_provider = \"p\"\n[providers.p]\napi_key_env = \"{key}\"\nmodel = \"m\"\n"
        )).unwrap();
        let cli = Cli::try_parse_from(["srtty", "a.mp4"]).unwrap();
        let msg = resolve(cfg, &cli, &env_with(&[])).unwrap_err().to_string();
        assert!(!msg.contains("0123secret"), "{msg}");
        assert!(msg.contains("must be the name of an environment variable"), "{msg}");
    }
}

#[test]
fn unknown_fields_are_rejected() {
    for toml in ["defualt_provider = \"p\"", "[providers.p]\nmodle = \"m\"",
                 "[subtitles]\nmax_line = 3", "[chunking]\njob = 2"] {
        assert!(toml::from_str::<Config>(toml).is_err(), "{toml}");
    }
}

#[test]
fn toml_type_error_does_not_echo_a_key_like_value() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("c.toml");
    std::fs::write(&path, "[providers.p]\nkind = \"gsk_0123secret\"\n").unwrap();
    let msg = Config::load(&path).unwrap_err().to_string();
    assert!(!msg.contains("0123secret") && msg.contains("<redacted>"), "{msg}");
}

#[test]
fn missing_key_is_reported_by_require_key_only() {
    let cfg: Config = toml::from_str(TOML).unwrap();
    let cli = Cli::try_parse_from(["srtty", "a.mp4"]).unwrap();
    let r = resolve(cfg, &cli, &env_with(&[])).unwrap();
    assert!(matches!(&r.api_key, KeyState::Unset { var } if var == "OPENROUTER_API_KEY"));
    assert!(r.require_key().unwrap_err().to_string().contains("OPENROUTER_API_KEY"));
}
```

Adapter `missing_key_names_the_variable_not_the_value` et `flags_override_env_override_toml` à `require_key()`.

Dans `compat.rs` : `load_rejects_other_schemas` (un cache `{"schema":2,"generated_at":"x","models":[]}` → `load` rend la liste embarquée, non vide) et `parse_compat_rejects_schema_2`.

Dans `tests/cli.rs` :
- `config_show_with_unset_key_prints_placeholder` : config valide, variable absente → code 0, stdout contient `api_key = "<unset: SRTTY_E2E_API_KEY>"`.
- `clap_error_with_json_prints_json` : `srtty --json a.mp4 --format both` → code 1, stdout est un JSON valide avec `exit_code == 1` et un champ `error` non vide.
- `help_with_json_is_not_an_error` : `srtty --json --help` → code 0, stdout ne se parse pas en JSON avec `error`.

- [ ] **Step 2 : lancer les tests, vérifier qu'ils échouent**

Run : `cargo test` — Expected : échec de compilation (`KeyState`, `require_key`, `parse_compat` inconnus).

- [ ] **Step 3 : implémenter**

- `resolve` : refuser `looks_like_api_key(var) || !is_env_var_name(var)` avec le message existant ; rendre `KeyState` au lieu d'échouer quand la variable est absente.
- `Config::load` : passer `e.message()` dans `redact_key_like`.
- `#[serde(deny_unknown_fields)]` sur `Config`, `ProviderConfig`, `SubtitleConfig`, `ChunkingConfig`.
- `app.rs` : `transcribe` appelle `r.require_key()?` ; `show_config` affiche `***` pour `Set`, `<unset: VAR>` pour `Unset`, et omet `api_key` pour `NotNeeded`.
- `CompatList::load` passe par `parse_compat`.
- `main.rs` : sur une erreur clap avec `err.use_stderr()` et `--json` présent dans `std::env::args_os()`, écrire en plus sur stdout `{"error": <première ligne du rendu sans le préfixe "error: ">, "exit_code": 1}`. Rendu sans couleur : `err.render().to_string()`.

- [ ] **Step 4 : lancer les tests**

Run : `cargo test` — Expected : tout passe (127 tests du plan 1 + les nouveaux).

- [ ] **Step 5 : commit**

```bash
git add -A && git commit -m "fix: harden config parsing, show unset keys, emit JSON on usage errors"
```

---

### Task 2 : `compat.json` distant, cache et statuts d'affichage

**Files:**
- Create: `src/fsutil.rs`
- Modify: `src/compat.rs`, `src/lib.rs`, `src/error.rs`, `Cargo.toml` (rien à ajouter : reqwest est déjà là)
- Test: `src/compat.rs`, `src/fsutil.rs`

**Interfaces:**
- Consumes : `parse_compat`, `COMPAT_SCHEMA` (tâche 1).
- Produces :
  - `fsutil::write_atomic(path: &Path, contents: &[u8]) -> std::io::Result<()>` : crée le dossier parent, écrit un `tempfile::NamedTempFile` dans ce dossier, puis `persist`.
  - `SrttyError::Network(String)` (code 2).
  - `pub const COMPAT_URL: &str = "https://raw.githubusercontent.com/AdrienGras/srtty/main/compat.json";`, `pub const REFRESH_TIMEOUT: Duration = Duration::from_secs(3);`, `pub fn compat_url() -> String` (`SRTTY_COMPAT_URL` non vide, sinon `COMPAT_URL`).
  - `pub enum CompatSource { Remote, Cache, Embedded }` (Serialize, minuscules).
  - `pub struct Refreshed { pub list: CompatList, pub source: CompatSource, pub warning: Option<String> }`.
  - `pub fn load_with_source(cache_dir: &Path) -> (CompatList, CompatSource)`.
  - `pub async fn refresh(cache_dir: &Path, url: &str, timeout: Duration) -> Refreshed` : GET ; statut 2xx et `parse_compat` OK → `write_atomic(<cache>/compat.json, corps)` et `Remote` ; sinon `load_with_source` et `warning = Some("could not refresh the compatibility list ({cause}); using the {cache|embedded} copy from {generated_at}")`. Une erreur d'écriture du cache devient aussi un avertissement, pas une erreur.
  - `pub enum ModelStatus { CompatibleSpeakers, Compatible, Unknown, Incompatible }` (Serialize en kebab-case : `compatible-speakers`, …), `fn label(&self) -> &'static str` (`"compatible + speakers"`, `"compatible"`, `"unknown"`, `"incompatible"`), et `pub fn status_of(entry: Option<&CompatEntry>) -> ModelStatus` (`Unusable` et `Incompatible` donnent `Incompatible` ; `Compatible` avec `diarization` non nul donne `CompatibleSpeakers`).
  - `pub fn civil_date(unix_secs: i64) -> String` (`YYYY-MM-DD`, UTC, algorithme `civil_from_days` de Howard Hinnant) et `pub fn utc_today() -> String`.

- [ ] **Step 1 : écrire les tests qui échouent**

```rust
#[tokio::test]
async fn refresh_saves_a_valid_remote_copy() {
    let server = MockServer::start().await;
    let body = r#"{"schema":1,"generated_at":"2026-11-02","models":[]}"#;
    Mock::given(method("GET")).and(path("/compat.json"))
        .respond_with(ResponseTemplate::new(200).set_body_string(body)).mount(&server).await;
    let dir = tempfile::tempdir().unwrap();
    let r = refresh(dir.path(), &format!("{}/compat.json", server.uri()), REFRESH_TIMEOUT).await;
    assert_eq!(r.source, CompatSource::Remote);
    assert!(r.warning.is_none());
    assert_eq!(std::fs::read_to_string(dir.path().join("compat.json")).unwrap(), body);
}

#[tokio::test]
async fn refresh_keeps_cache_on_bad_remote() {
    // Cas : 404, page HTML en 200, schéma 2, serveur qui répond après 5 s (timeout 1 s ici), port fermé.
    // Pour chacun : source == Cache, warning contient "could not refresh",
    // et <cache>/compat.json (pré-rempli avec generated_at "cached-copy") est inchangé.
}

#[test]
fn status_mapping() {
    let l = CompatList::embedded();
    assert_eq!(status_of(l.find("openrouter", "x-ai/grok-stt-1.0")), ModelStatus::CompatibleSpeakers);
    assert_eq!(status_of(l.find("openrouter", "openai/whisper-1")), ModelStatus::Compatible);
    assert_eq!(status_of(l.find("openrouter", "assemblyai/universal-3-6-pro")), ModelStatus::Incompatible);
    assert_eq!(status_of(l.find("openrouter", "google/chirp-3")), ModelStatus::Incompatible);
    assert_eq!(status_of(None), ModelStatus::Unknown);
}

#[test]
fn civil_dates() {
    assert_eq!(civil_date(0), "1970-01-01");
    assert_eq!(civil_date(1793235111), "2026-10-28");
    assert_eq!(civil_date(951_782_400), "2000-02-29");
}
```

Dans `fsutil.rs` : `write_atomic_creates_parents_and_replaces` (écrit deux fois, lit le second contenu, aucun fichier temporaire ne reste dans le dossier).

- [ ] **Step 2 : lancer les tests** — `cargo test compat fsutil` — Expected : échec de compilation.
- [ ] **Step 3 : implémenter les signatures ci-dessus.** Le client de `refresh` est construit avec `.timeout(timeout)` ; une erreur de construction devient un avertissement.
- [ ] **Step 4 : lancer les tests** — `cargo test` — Expected : PASS.
- [ ] **Step 5 : commit** — `git commit -m "feat: refresh compat.json from GitHub into the cache"`

---

### Task 3 : liste des modèles d'un fournisseur

**Files:**
- Create: `src/providers.rs`, `tests/fixtures/openrouter_models.json`, `tests/fixtures/openrouter_endpoints_grok.json`
- Modify: `src/lib.rs`
- Test: `src/providers.rs`

**Interfaces:**
- Consumes : `ApiKey` (config), `SrttyError::Network` (tâche 2).
- Produces :
  - `pub struct Preset { pub key: &'static str, pub label: &'static str, pub base_url: &'static str, pub api_key_env: &'static str }` et `pub const PRESETS: [Preset; 3]` :
    - `openrouter`, `"OpenRouter"`, `https://openrouter.ai/api/v1`, `OPENROUTER_API_KEY`
    - `openai`, `"OpenAI"`, `https://api.openai.com/v1`, `OPENAI_API_KEY`
    - `groq`, `"Groq"`, `https://api.groq.com/openai/v1`, `GROQ_API_KEY`
  - `pub fn preset(key: &str) -> Option<&'static Preset>`.
  - `pub struct ListedModel { pub id: String, pub usd_per_s: Option<f64>, pub token_priced: bool, pub url: Option<String> }`. `token_priced` : facturé au token (prix par seconde inconnu). `url` : page du modèle, `https://openrouter.ai/{id}` pour OpenRouter, `None` ailleurs.
  - `pub enum Listing { OpenRouter, OpenAi }` et `Listing::for_base_url(base_url: &str) -> Listing` (OpenRouter si `is_openrouter`).
  - `pub fn parse_openrouter_models(v: &serde_json::Value) -> Vec<ListedModel>` : `data[]`, garde les modèles dont `architecture.output_modalities` contient `"transcription"` ; `usd_per_s = pricing.prompt` (chaîne → f64) **seulement si** `pricing.completion` vaut 0 ; sinon `usd_per_s = None` et `token_priced = true`. `url = https://openrouter.ai/{id}`. Trié par id.
  - `pub fn parse_openai_models(v: &serde_json::Value) -> Vec<ListedModel>` : `data[].id` dont la version minuscule contient `whisper`, `transcribe`, `stt` ou `asr` ; `usd_per_s = None`, `token_priced = false`, `url = None`. Trié.
  - `pub async fn list_models(listing: Listing, base_url: &str, key: Option<&ApiKey>) -> Result<Vec<ListedModel>, SrttyError>` : OpenRouter → `GET {base}/models?output_modalities=transcription`, sans clé ; OpenAI → `GET {base}/models` avec `bearer_auth`. Timeout 15 s. HTTP 401/403 → `Network("listing models failed: authentication rejected (HTTP {code})")` ; autre non-2xx → `Network("listing models failed: HTTP {code}")` ; transport → `Network("listing models failed: {cause}")` où la cause est passée par `redact_key_like`. Le corps de réponse n'est jamais repris.
  - `pub async fn endpoint_tags(base_url: &str, model: &str) -> Result<Vec<String>, SrttyError>` : `GET {base}/models/{model}/endpoints`, renvoie `data.endpoints[].tag`.

- [ ] **Step 1 : créer les fixtures**

```bash
curl -s 'https://openrouter.ai/api/v1/models?output_modalities=transcription' \
 | jq '{data: [.data[] | select(.id == "x-ai/grok-stt-1.0" or .id == "google/gemini-3.5-transcribe" or .id == "microsoft/mai-transcribe-1.5" or .id == "openai/whisper-large-v3-turbo")]}' \
 > tests/fixtures/openrouter_models.json
curl -s 'https://openrouter.ai/api/v1/models/x-ai/grok-stt-1.0/endpoints' > tests/fixtures/openrouter_endpoints_grok.json
```

Ajouter à la main dans `openrouter_models.json` une 5ᵉ entrée `{"id": "openai/gpt-5-chat", "architecture": {"output_modalities": ["text"]}, "pricing": {"prompt": "0.000001", "completion": "0.00001"}}` pour le filtre.

- [ ] **Step 2 : écrire les tests qui échouent**

```rust
#[test]
fn openrouter_listing_prices_per_second_and_skips_token_priced() {
    let models = parse_openrouter_models(&fx("openrouter_models.json"));
    let ids: Vec<_> = models.iter().map(|m| m.id.as_str()).collect();
    assert_eq!(ids, ["google/gemini-3.5-transcribe", "microsoft/mai-transcribe-1.5",
                     "openai/whisper-large-v3-turbo", "x-ai/grok-stt-1.0"]);
    let grok = models.iter().find(|m| m.id == "x-ai/grok-stt-1.0").unwrap();
    assert!((grok.usd_per_s.unwrap() * 3600.0 - 0.10).abs() < 1e-6);
    assert_eq!(models[0].usd_per_s, None); // gemini : facturé au token
    assert!(models[0].token_priced && !grok.token_priced);
    assert_eq!(models[0].url.as_deref(), Some("https://openrouter.ai/google/gemini-3.5-transcribe"));
    assert_eq!(models[1].usd_per_s, Some(0.36));
}

#[test]
fn openai_listing_keeps_speech_models_only() {
    let v = json!({"data": [{"id": "gpt-4o"}, {"id": "whisper-1"}, {"id": "gpt-4o-mini-transcribe"},
                            {"id": "distil-whisper-large-v3-en"}, {"id": "tts-1"}]});
    let ids: Vec<_> = parse_openai_models(&v).into_iter().map(|m| m.id).collect();
    assert_eq!(ids, ["distil-whisper-large-v3-en", "gpt-4o-mini-transcribe", "whisper-1"]);
}

#[tokio::test]
async fn openai_listing_sends_the_key_and_hides_it_on_errors() {
    // wiremock : GET /v1/models exige `Authorization: Bearer sk-test-4242` → 200 avec 2 modèles ;
    // sans en-tête → 401. Vérifier le résultat avec la clé, puis l'erreur 401 sans clé :
    // Network, message contient "HTTP 401", pas "sk-test-4242".
}

#[tokio::test]
async fn http_500_and_dead_port_are_network_errors() { /* code de sortie 2 via exit_code() */ }

#[tokio::test]
async fn endpoint_tags_reads_the_fixture() {
    // wiremock sert openrouter_endpoints_grok.json sur /api/v1/models/x-ai/grok-stt-1.0/endpoints
    // → vec!["xai"]
}
```

`fx` : réutiliser `crate::test_support::fx`.

- [ ] **Step 3 : lancer** — `cargo test providers` — Expected : échec de compilation.
- [ ] **Step 4 : implémenter les signatures ci-dessus.**
- [ ] **Step 5 : lancer** — `cargo test` — Expected : PASS.
- [ ] **Step 6 : commit** — `git commit -m "feat: list transcription models from OpenRouter and OpenAI-style endpoints"`

---

### Task 4 : `srtty models`

**Files:**
- Create: `src/models.rs`
- Modify: `src/cli.rs`, `src/app.rs`, `src/lib.rs`
- Test: `src/models.rs`, `tests/cli.rs`

**Interfaces:**
- Consumes : `refresh`, `compat_url`, `status_of`, `ModelStatus`, `CompatSource` (tâche 2) ; `PRESETS`, `preset`, `Listing`, `list_models`, `ListedModel` (tâche 3) ; `provider_key` (plan 1).
- Produces :
  - `Command::Models` dans `cli.rs` (doc : `List the provider's transcription models with price and compatibility.`) ; il utilise les flags globaux `--provider` et `--json`.
  - `pub struct Target { pub name: String, pub base_url: String, pub api_key_env: Option<String> }`.
  - `pub fn listing_target(cfg: &Config, requested: Option<&str>) -> Result<Target, SrttyError>` : nom = `requested`, sinon `cfg.default_provider`, sinon `"openrouter"`. Cherché dans `cfg.providers` (un fournisseur `kind = "local"` → `Usage("provider `{name}` is a local backend and has no model list")`, un `base_url` absent → `Config`), sinon dans `PRESETS`, sinon `Config("unknown provider `{name}`; known: openrouter, openai, groq or a provider from the config file")`.
  - `#[derive(Serialize)] pub struct ModelRow { pub id: String, pub usd_per_hour: Option<f64>, pub token_priced: bool, pub url: Option<String>, pub status: ModelStatus, pub words: Option<bool>, pub fine_segments: Option<bool>, pub checked_at: Option<String> }`.
  - `pub fn rows(listed: &[ListedModel], compat: &CompatList, provider: Option<&str>) -> Vec<ModelRow>` : prix = `usd_per_s * 3600` de la liste, sinon `usd_per_hour` de l'entrée compat, sauf pour un modèle `token_priced` (prix toujours `None`) ; `token_priced` et `url` repris de la liste. Tri : les modèles `token_priced` **en fin de liste** ; à l'intérieur de chaque groupe, par statut (`CompatibleSpeakers`, `Compatible`, `Unknown`, `Incompatible`), puis prix croissant (`None` en dernier), puis id.
  - `pub fn render_table(rows: &[ModelRow]) -> String` : colonnes `MODEL`, `$/HOUR` (deux décimales, `token` pour un modèle facturé au token, `?` si inconnu, aligné à droite), `STATUS` (`label()`), puis, pour les seules lignes `token_priced`, l'URL de la page du modèle (texte brut, sans séquence OSC 8 : les terminaux la rendent cliquable et la sortie reste propre dans un pipe). Largeur de la première colonne = id le plus long.
  - `pub async fn fetch_rows(target: &Target, key: Option<&ApiKey>, compat: &CompatList) -> Result<Vec<ModelRow>, SrttyError>` : réutilisé par `setup`.
  - `pub async fn run(cli: &Cli) -> Result<(), SrttyError>`.

Comportement de `run` : charger la config (fichier absent = défauts), `listing_target`, lire la clé si `api_key_env` est défini (`looks_like_api_key` → même erreur `Config` qu'en tâche 1). Une liste au format OpenAI sans clé → `Config("environment variable {VAR} is not set; it is needed to list the models of `{name}`")`. Puis `refresh(paths::cache_dir(), &compat_url(), REFRESH_TIMEOUT)` et `fetch_rows`.
- Sans `--json` : tableau sur stdout ; sur stderr, l'éventuel avertissement de `refresh` puis `compatibility list: {generated_at} ({remote|cache|embedded})`.
- Avec `--json` : stdout = `{"provider": name, "compat": {"generated_at": …, "source": …}, "models": [ModelRow…], "warnings": […]}`.

- [ ] **Step 1 : tests unitaires qui échouent (`models.rs`)**

```rust
#[test]
fn rows_merge_listing_and_compat_and_sort() {
    let listed = vec![
        lm("new/model", Some(0.00001)),
        lm("google/chirp-3", Some(0.0002667)),
        lm("openai/whisper-1", Some(0.0001)),
        lm("x-ai/grok-stt-1.0", None),
        ListedModel { id: "google/gemini-3.5-transcribe".into(), usd_per_s: None, token_priced: true,
                      url: Some("https://openrouter.ai/google/gemini-3.5-transcribe".into()) },
    ];  // lm(id, prix) : token_priced = false, url = None
    let r = rows(&listed, &CompatList::embedded(), Some("openrouter"));
    let order: Vec<_> = r.iter().map(|m| (m.id.as_str(), m.status)).collect();
    assert_eq!(order, [("x-ai/grok-stt-1.0", ModelStatus::CompatibleSpeakers),
                       ("openai/whisper-1", ModelStatus::Compatible),
                       ("new/model", ModelStatus::Unknown),
                       ("google/chirp-3", ModelStatus::Incompatible),
                       ("google/gemini-3.5-transcribe", ModelStatus::Compatible)]);
    assert_eq!(r[0].usd_per_hour, Some(0.1)); // repli sur compat.json
    assert_eq!(r[4].usd_per_hour, None); // facturé au token : pas de repli sur compat.json
    assert_eq!(r[2].words, None);
}

#[test]
fn table_shows_unknown_price_as_question_mark() { /* ligne "new/model   ?  unknown" alignée */ }

#[test]
fn table_puts_token_priced_models_last_with_their_url() {
    // dernière ligne : id gemini, "token", "compatible", puis "https://openrouter.ai/google/gemini-3.5-transcribe" ;
    // aucune autre ligne ne contient "https://", et la sortie ne contient pas "\x1b".
}

#[test]
fn listing_target_resolution() {
    // config vide → openrouter (preset) ; requested "groq" → preset groq ;
    // config avec [providers.mine] base_url → Target mine ; "local" kind → Usage ; "nope" → Config.
}
```

- [ ] **Step 2 : tests bout en bout qui échouent (`tests/cli.rs`)**

Réutiliser `Sandbox` : la config pointe sur le `MockServer` (`base_url = "{uri}/v1"`), et `SRTTY_COMPAT_URL = "{uri}/compat.json"`. Le mock sert `/v1/models` (`{"data":[{"id":"whisper-1"},{"id":"whisper-large-v3"}]}`) et `/compat.json`.

- `models_lists_and_refreshes_cache` : code 0 ; stdout contient `whisper-1` ; `<cache>/srtty/compat.json` existe ensuite.
- `models_json_is_a_single_object` : `srtty models --json` (le flag **après** la sous-commande, pour vérifier `global = true`) → stdout se parse, `models` est un tableau de 2 éléments, `compat.source == "remote"`.
- `models_without_key_names_the_variable` : sans la variable → code 1, stderr contient `SRTTY_E2E_API_KEY`.
- `models_http_error_exits_2` : `/v1/models` répond 500 → code 2 ; le JSON d'erreur avec `--json` a `exit_code == 2`.
- `models_works_offline_with_compat_fallback` : `/compat.json` répond 404 → code 0, stderr contient `could not refresh`.

- [ ] **Step 3 : lancer** — `cargo test models` — Expected : échec.
- [ ] **Step 4 : implémenter, puis câbler `Command::Models` dans `app::main`.**
- [ ] **Step 5 : lancer** — `cargo test` — Expected : PASS.
- [ ] **Step 6 : commit** — `git commit -m "feat: add srtty models"`

---

### Task 5 : extraits embarqués et classement d'un modèle

**Files:**
- Create: `assets/sample-30s.ogg`, `assets/speakers-45s.ogg`, `assets/README.md`, `src/probe.rs`
- Modify: `src/lib.rs`, `src/transcribe/mod.rs` (ajout `has_fine_segments`)
- Test: `src/probe.rs`

**Interfaces:**
- Consumes : `Transcriber`, `Transcript`, `TranscribeError`, `TranscribeOptions`, `AudioChunk`, `transcribe_with_retry` (plan 1, `run.rs`), `Reporter`.
- Produces :
  - `pub const SAMPLE: &[u8] = include_bytes!("../assets/sample-30s.ogg");`, `pub const SAMPLE_S: f64 = 30.0;`, `pub const SPEAKERS_SAMPLE: &[u8] = include_bytes!("../assets/speakers-45s.ogg");`, `pub const SPEAKERS_SAMPLE_S: f64 = 45.0;`, `pub const SAMPLE_LANGUAGE: &str = "fr";`.
  - `impl Transcript { pub fn has_fine_segments(&self) -> bool }` : segments non vides et durée moyenne ≤ `FINE_SEGMENT_MAX_AVG_S` (réutilisé par `usability`).
  - `pub enum Verdict { Usable { words: bool, fine_segments: bool }, Unusable, Incompatible, Inconclusive(String) }`.
  - `pub struct ProbeOutcome { pub verdict: Verdict, pub cost_usd: Option<f64> }`.
  - `pub fn classify(result: Result<Transcript, TranscribeError>) -> Result<ProbeOutcome, TranscribeError>` : `Auth` → `Err` (seul cas) ; `ModelUnsupported` → `Incompatible` ; `Fatal { status: Some(400..=499) }` → `Incompatible` ; `Fatal` autre et `Retryable` → `Inconclusive(message)` ; `Ok(t)` : mots non vides ou `has_fine_segments()` → `Usable`, sinon `Unusable` ; `cost_usd = t.cost_usd`.
  - `pub fn distinct_speakers(t: &Transcript) -> usize` (sur les mots).
  - `pub struct SampleFile { pub chunk: AudioChunk, _dir: tempfile::TempDir }` et `pub fn write_sample(bytes: &[u8], duration_s: f64) -> std::io::Result<SampleFile>` (fichier `sample.ogg`, index 0).
  - `pub async fn probe_model(t: &dyn Transcriber, sample: &AudioChunk, reporter: &dyn Reporter) -> Result<ProbeOutcome, TranscribeError>` : options `language = SAMPLE_LANGUAGE`, via `transcribe_with_retry`, puis `classify`.
  - `pub async fn probe_diarization(t: &dyn Transcriber, sample: &AudioChunk, option: &serde_json::Value, reporter: &dyn Reporter) -> Result<(Option<bool>, Option<f64>), TranscribeError>` : `provider_options = Some(option)` ; `Some(true)` si `distinct_speakers ≥ 2`, `Some(false)` si la réponse est exploitable avec moins de 2 voix, `None` si non concluant ; `Err` seulement pour `Auth`.

- [ ] **Step 1 : générer les extraits**

Sources (LibriVox, domaine public), déjà dans `/tmp/srtty-spike/media*/` sur la machine d'Adrien, sinon :
`https://www.archive.org/download/contes_temps_passe_librivox/contes_du_temps_passe_04_perrault_64kb.mp3` (Barbe bleue) et `https://archive.org/download/precieusesridicules_1012_librivox/precieusesridicules_10_moliere_64kb.mp3` (scène 10).

```bash
ffmpeg -v error -ss 60 -t 30 -i barbe_bleue.mp3 -vn -ac 1 -ar 16000 -c:a libopus -b:a 32k -application voip assets/sample-30s.ogg
```

Pour `speakers-45s.ogg` : choisir dans la scène 10 une fenêtre de 45 s où au moins 3 voix alternent. Pour la trouver sans écouter : `tests/fixtures/grok_diar.json` est la pièce entière diarisée ; l'offset de la scène 10 dans la pièce est la somme des durées (`ffprobe`) des fichiers `p_00` à `p_09` de `/tmp/srtty-spike/media2/`. Prendre la fenêtre de 45 s de cette scène qui contient le plus de valeurs `speaker` distinctes dans les mots, la noter dans `assets/README.md` avec la commande ffmpeg et la provenance. Taille attendue : environ 120 Ko et 180 Ko.

- [ ] **Step 2 : tests qui échouent**

```rust
#[test]
fn classify_spike_responses() {
    let t = |name| parse_verbose_json(&fx(name)).unwrap();
    assert!(matches!(classify(Ok(t("deepgram.json"))).unwrap().verdict,
                     Verdict::Usable { words: true, fine_segments: true }));
    assert!(matches!(classify(Ok(t("scribe.json"))).unwrap().verdict,
                     Verdict::Usable { words: true, fine_segments: false }));
    assert!(matches!(classify(Ok(t("assemblyai.json"))).unwrap().verdict, Verdict::Unusable));
}

#[test]
fn classify_errors() {
    let unsupported = TranscribeError::ModelUnsupported { message: "verbose_json".into() };
    assert!(matches!(classify(Err(unsupported)).unwrap().verdict, Verdict::Incompatible));
    let bad_request = TranscribeError::Fatal { status: Some(400), message: "x".into() };
    assert!(matches!(classify(Err(bad_request)).unwrap().verdict, Verdict::Incompatible));
    let gone = TranscribeError::Retryable { status: Some(503), message: "x".into(), retry_after_s: None };
    assert!(matches!(classify(Err(gone)).unwrap().verdict, Verdict::Inconclusive(_)));
    assert!(classify(Err(TranscribeError::Auth { status: 401 })).is_err());
}

#[test]
fn grok_diarization_fixture_has_several_speakers() {
    assert!(distinct_speakers(&parse_verbose_json(&fx("grok_diar.json")).unwrap()) >= 3);
}

#[test]
fn embedded_samples_are_ogg_and_small() {
    for bytes in [SAMPLE, SPEAKERS_SAMPLE] {
        assert!(bytes.starts_with(b"OggS"));
        assert!(bytes.len() < 300_000);
    }
}

#[tokio::test]
async fn probe_model_sends_french_and_classifies() {
    // Faux Transcriber (comme dans run.rs) qui vérifie opts.language == Some("fr")
    // et renvoie le transcript de deepgram.json → Usable { words: true, .. }.
}
```

- [ ] **Step 3 : lancer** — `cargo test probe` — Expected : échec.
- [ ] **Step 4 : implémenter.**
- [ ] **Step 5 : lancer** — `cargo test` — Expected : PASS.
- [ ] **Step 6 : commit** — `git add assets src && git commit -m "feat: classify models on an embedded 30 s sample"`

---

### Task 6 : `srtty compat probe`

**Files:**
- Create: `src/compat_probe.rs`
- Modify: `src/cli.rs`, `src/app.rs`, `src/lib.rs`
- Test: `src/compat_probe.rs`, `tests/cli.rs`

**Interfaces:**
- Consumes : `list_models`, `endpoint_tags`, `ListedModel` (tâche 3) ; `probe_model`, `probe_diarization`, `write_sample`, `Verdict`, `ProbeOutcome`, `SAMPLE_S`, `SPEAKERS_SAMPLE_S` (tâche 5) ; `parse_compat`, `utc_today`, `write_atomic` (tâches 1-2) ; `OpenAiCompatible`, `RequestMode::Multipart` (plan 1).
- Produces :
  - Sous-commande cachée `srtty compat probe` (`#[command(hide = true)]`) avec `ProbeArgs` :
    `--output <PATH>` (défaut `compat.json`), `--max-usd-per-s <F64>` (défaut `0.001`), `--budget-usd <F64>` (défaut `0.50`), `--only <ID>` (répétable), `--api-key-env <NAME>` (défaut `OPENROUTER_API_KEY`), `--base-url <URL>` (caché, défaut `https://openrouter.ai/api/v1`).
  - `pub struct ProbePolicy { pub max_usd_per_s: f64, pub budget_usd: f64, pub only: Vec<String>, pub today: String }`.
  - `#[async_trait] pub trait Prober: Send + Sync { async fn probe(&self, model: &str) -> Result<ProbeOutcome, TranscribeError>; async fn endpoint_tags(&self, model: &str) -> Result<Vec<String>, SrttyError>; async fn diarize(&self, model: &str, option: &serde_json::Value) -> Result<(Option<bool>, Option<f64>), TranscribeError>; }`.
  - `pub struct ProbeReport { pub spent_usd: f64, pub probed: Vec<String>, pub kept: Vec<(String, String)>, pub budget_exhausted: bool }` (`kept` = id et raison : `"price"`, `"budget"`, `"not selected"`, ou le message non concluant).
  - `pub async fn probe_all(listed: &[ListedModel], previous: &CompatList, policy: &ProbePolicy, prober: &dyn Prober) -> Result<(CompatList, ProbeReport), SrttyError>`.
  - `pub fn same_content(a: &CompatList, b: &CompatList) -> bool` : égalité en ignorant `generated_at` et `checked_at`.

Règles de `probe_all`, dans l'ordre des ids :
1. Les entrées de `previous` dont `provider != "openrouter"` passent telles quelles.
2. Modèle hors `only` (quand `only` n'est pas vide), sans prix par seconde, ou au-delà de `max_usd_per_s` → on garde l'entrée précédente si elle existe (`kept`).
3. `estimate = usd_per_s * SAMPLE_S`. Si `spent + estimate > budget_usd` → `budget_exhausted = true`, et ce modèle comme tous les suivants gardent leur entrée précédente.
4. `prober.probe` ; `spent += cost_usd.unwrap_or(estimate)`. `Inconclusive` → entrée précédente gardée. `Auth` → erreur `SrttyError::Transcribe`, rien n'est écrit.
5. Nouvelle entrée : `provider = "openrouter"`, `status` (`Usable` → `compatible`, `Unusable` → `unusable`, `Incompatible` → `incompatible`), `words`, `fine_segments`, `usd_per_hour = usd_per_s * 3600`, `checked_at = today`, `diarization = None`.
6. Diarisation, seulement si `words` est vrai : `endpoint_tags` doit renvoyer **un seul** tag (sinon le résultat dépendrait du routage) ; `option = {<tag avant '/'>: {"diarize": true}}` ; estimation `usd_per_s * SPEAKERS_SAMPLE_S` soumise au budget comme en 3 (un dépassement met `budget_exhausted` à vrai et arrête aussi les modèles suivants) ; `spent += coût renvoyé, sinon l'estimation`. `Some(true)` → `diarization = Some(option)` ; `Some(false)` → `None` ; `None`, budget épuisé ou erreur de `endpoint_tags` → valeur de l'entrée précédente.
7. Les entrées OpenRouter précédentes absentes de `listed` disparaissent.
8. Résultat trié par `(provider, id)`, `schema = 1`, `generated_at = today`.

`run` : clé via `--api-key-env` (même contrôle `looks_like_api_key`/`is_env_var_name`, `Config` si absente) ; `list_models(Listing::OpenRouter, …)` ; `previous = parse_compat(fichier --output)` (absent → liste vide de schéma 1) ; `LiveProber` construit un `OpenAiCompatible` par modèle en multipart, écrit les deux extraits une fois avec `write_sample`, et rapporte les retries via `PlainReporter` sur stderr. Une ligne par modèle sur stderr : `{id}: {compatible|unusable|incompatible|kept (raison)}[, speakers][, ${coût:.4}]`. Fin : `spent ${:.4} of ${:.2}` puis `compat.json unchanged` ou `wrote {path}`. Écriture (`serde_json::to_string_pretty` + `\n`, `write_atomic`) seulement si `!same_content(&new, &previous)`.

- [ ] **Step 1 : tests qui échouent (`compat_probe.rs`)**, avec un `FakeProber` qui répond par id depuis une `HashMap` et compte les appels :

```rust
#[tokio::test] async fn probes_and_classifies_new_models() { /* compatible + diarisation xai ; incompatible ; unusable */ }
#[tokio::test] async fn skips_models_above_price_or_without_price() { /* aucun appel, entrée précédente gardée */ }
#[tokio::test] async fn budget_exhaustion_keeps_previous_entries() {
    // budget 0.002, trois modèles à 0.00005 $/s (0,0015 $ par sonde, réponses sans mots, coût non renvoyé) : un seul sondé,
    // les deux autres gardent leur entrée précédente "compatible", report.budget_exhausted
}
#[tokio::test] async fn transient_failure_keeps_previous_entry() { /* Inconclusive → entrée précédente intacte, checked_at compris */ }
#[tokio::test] async fn auth_failure_aborts() { /* Err, code 2 */ }
#[tokio::test] async fn diarization_needs_a_single_endpoint() { /* deux tags → pas d'appel diarize, diarization précédente gardée */ }
#[tokio::test] async fn hand_maintained_and_delisted_entries() { /* openai/groq gardés ; openrouter absent de la liste supprimé */ }
#[test] fn same_content_ignores_dates() { /* generated_at et checked_at différents → true ; un statut différent → false */ }
```

- [ ] **Step 2 : test bout en bout qui échoue (`tests/cli.rs`)**

`unchanged_content_is_not_rewritten` : MockServer qui sert `/api/v1/models?…` (un modèle `m/one` à `0.00001` $/s), `/api/v1/models/m/one/endpoints` (un tag `p`) et `/api/v1/audio/transcriptions` (réponse : `tests/fixtures/grok_diar.json`, qui a des mots et plusieurs voix ; elle sert à la sonde et à la diarisation). Premier `srtty compat probe --base-url {uri}/api/v1 --output out.json` → code 0, `out.json` contient `m/one`. On met son `generated_at` à `"old"`, on relance → stderr contient `unchanged`, le fichier garde `"old"`. Vérifier aussi que `srtty --help` ne mentionne pas `compat`.

- [ ] **Step 3 : lancer** — `cargo test compat_probe` — Expected : échec.
- [ ] **Step 4 : implémenter.**
- [ ] **Step 5 : lancer** — `cargo test` — Expected : PASS.
- [ ] **Step 6 : commit** — `git commit -m "feat: add hidden srtty compat probe to regenerate compat.json"`

---

### Task 7 : CI hebdomadaire de `compat.json`

**Files:**
- Create: `.github/workflows/compat.yml`

**Interfaces:**
- Consumes : `srtty compat probe` (tâche 6).

- [ ] **Step 1 : écrire le workflow**

- Déclencheurs : `schedule` (`cron: "17 5 * * 1"`, lundi 05:17 UTC) et `workflow_dispatch`.
- `permissions: contents: write` ; `concurrency: compat` ; `timeout-minutes: 30` ; `runs-on: ubuntu-latest`.
- Étapes : `actions/checkout` (dernière version majeure), `dtolnay/rust-toolchain@stable`, `Swatinem/rust-cache` ; `cargo run --release --locked -- compat probe --output compat.json` avec `env: OPENROUTER_API_KEY: ${{ secrets.OPENROUTER_API_KEY }}` ; puis :

```bash
if git diff --quiet -- compat.json; then echo "compat.json unchanged"; exit 0; fi
git config user.name "github-actions[bot]"
git config user.email "41898282+github-actions[bot]@users.noreply.github.com"
git add compat.json
git commit -m "chore: refresh compat.json"
git push
```

Pas de ffmpeg : les extraits sont déjà en Opus. La clé n'est passée que par `env`, jamais sur la ligne de commande.

- [ ] **Step 2 : vérifier** — `actionlint .github/workflows/compat.yml` — Expected : aucune sortie, code 0.
- [ ] **Step 3 : commit** — `git commit -m "ci: refresh compat.json weekly"`

---

### Task 8 : écriture de la config par `setup`

**Files:**
- Create: `src/setup/write.rs` (et `src/setup/mod.rs` minimal qui déclare `pub mod write;`)
- Modify: `Cargo.toml` (`cargo add toml_edit`), `src/lib.rs`
- Test: `src/setup/write.rs`

**Interfaces:**
- Produces :
  - `pub struct ProviderChoice { pub name: String, pub base_url: String, pub api_key_env: String, pub model: String }`.
  - `pub fn apply(existing: Option<&str>, choice: &ProviderChoice) -> Result<String, SrttyError>` : `existing` analysé avec `toml_edit::DocumentMut` (erreur → `Config("cannot update the config file: it is not valid TOML{position}")`, sans citer le texte) ; `default_provider = name` ; dans `[providers.<name>]` (créé s'il manque), écrit `base_url`, `api_key_env`, `model` et garde les autres clés ; tout le reste du document est intact. Le résultat doit passer `toml::from_str::<Config>` (sinon `Config`).
  - `pub fn write_config(path: &Path, text: &str) -> Result<(), SrttyError>` : `write_atomic`.

- [ ] **Step 1 : tests qui échouent**

```rust
fn choice() -> ProviderChoice {
    ProviderChoice { name: "openrouter".into(), base_url: "https://openrouter.ai/api/v1".into(),
                     api_key_env: "OPENROUTER_API_KEY".into(), model: "x-ai/grok-stt-1.0".into() }
}

#[test]
fn apply_creates_a_minimal_config() {
    let text = apply(None, &choice()).unwrap();
    let cfg: Config = toml::from_str(&text).unwrap();
    assert_eq!(cfg.default_provider.as_deref(), Some("openrouter"));
    assert_eq!(cfg.providers["openrouter"].model.as_deref(), Some("x-ai/grok-stt-1.0"));
}

#[test]
fn apply_keeps_comments_and_other_providers() {
    let existing = "# my config\ndefault_provider = \"groq\"\n\n[providers.groq]\n# fast\nbase_url = \"https://api.groq.com/openai/v1\"\napi_key_env = \"GROQ_API_KEY\"\nmodel = \"whisper-large-v3\"\n\n[providers.openrouter]\nrequest_mode = \"json-base64\"\nmodel = \"old\"\n\n[subtitles]\nmax_lines = 1\n";
    let text = apply(Some(existing), &choice()).unwrap();
    assert!(text.contains("# my config") && text.contains("# fast"));
    let cfg: Config = toml::from_str(&text).unwrap();
    assert_eq!(cfg.default_provider.as_deref(), Some("openrouter"));
    assert_eq!(cfg.providers["groq"].model.as_deref(), Some("whisper-large-v3"));
    assert_eq!(cfg.providers["openrouter"].request_mode, RequestMode::JsonBase64);
    assert_eq!(cfg.providers["openrouter"].model.as_deref(), Some("x-ai/grok-stt-1.0"));
    assert_eq!(cfg.subtitles.max_lines, 1);
}

#[test]
fn apply_refuses_invalid_toml_without_echo() {
    let err = apply(Some("api_key_env = sk-or-v1-0123secret\n"), &choice()).unwrap_err().to_string();
    assert!(!err.contains("0123secret"), "{err}");
}
```

- [ ] **Step 2 : lancer** — `cargo test setup::write` — Expected : échec.
- [ ] **Step 3 : implémenter.**
- [ ] **Step 4 : lancer** — `cargo test` — Expected : PASS.
- [ ] **Step 5 : commit** — `git commit -m "feat: update the config file without losing comments"`

---

### Task 9 : `srtty setup`

**Files:**
- Create: `src/setup/prompt.rs`
- Modify: `src/setup/mod.rs`, `src/cli.rs`, `src/app.rs`, `Cargo.toml` (`cargo add inquire`)
- Test: `src/setup/mod.rs`, `tests/cli.rs`

**Interfaces:**
- Consumes : `ProviderChoice`, `apply`, `write_config` (tâche 8) ; `PRESETS`, `Target`, `fetch_rows`, `ModelRow`, `ModelStatus` (tâches 3-4) ; `refresh`, `compat_url` (tâche 2) ; `probe_model`, `write_sample`, `SAMPLE`, `SAMPLE_S`, `Verdict` (tâche 5) ; `looks_like_api_key`, `is_env_var_name` (tâche 1).
- Produces :
  - `pub trait Prompter { fn select(&mut self, message: &str, options: &[String]) -> Result<usize, SrttyError>; fn text(&mut self, message: &str, default: Option<&str>, validate: &dyn Fn(&str) -> Result<(), String>) -> Result<String, SrttyError>; fn confirm(&mut self, message: &str, default: bool) -> Result<bool, SrttyError>; fn say(&mut self, line: &str); }`. Annulation (Échap, Ctrl-C) → `SrttyError::Interrupted`.
  - `pub struct InquirePrompter;` (implémentation `inquire` : `Select`, `Text` avec validateur, `Confirm` ; `say` écrit sur stderr).
  - `#[async_trait] pub trait SampleTester: Send + Sync { async fn test(&self, base_url: &str, key: &ApiKey, model: &str) -> Result<ProbeOutcome, TranscribeError>; }` et `pub struct LiveTester;` (`OpenAiCompatible` multipart + `probe_model` sur `SAMPLE`).
  - `pub struct SetupContext<'a> { pub config_path: PathBuf, pub cache_dir: PathBuf, pub compat_url: String, pub env: &'a dyn Fn(&str) -> Option<String>, pub tester: &'a dyn SampleTester }`.
  - `pub struct SetupDone { pub written: bool, pub path: PathBuf }`.
  - `pub async fn run_setup(p: &mut dyn Prompter, ctx: &SetupContext<'_>) -> Result<SetupDone, SrttyError>`.
  - `Command::Setup` (doc : `Configure srtty interactively.`).

Parcours de `run_setup` (textes exacts) :
1. `select("Which transcription provider?", ["OpenRouter", "OpenAI", "Groq", "Custom OpenAI-compatible URL", "Local Whisper (not available in this build)"])`. Whisper local → `say("Local Whisper needs a build with the `local` feature: cargo install srtty --features local")` et on repose la question. URL personnalisée → `text("Provider name (used in the config file):", Some("custom"), …)` qui accepte `^[a-z0-9_-]+$`, puis `text("Base URL (e.g. http://localhost:8000/v1):", None, …)` qui exige `http://` ou `https://` et une URL valide.
2. `text("Environment variable holding the API key:", Some(<variable du preset>), validate)`. Le validateur renvoie, sans jamais citer l'entrée : si `looks_like_api_key` → `"That looks like an API key. Enter the name of the variable that holds it, e.g. OPENROUTER_API_KEY."` ; si `!is_env_var_name` → `"Use letters, digits and underscores, e.g. OPENROUTER_API_KEY."`. Puis `say("✔ {VAR} is set")` ou `say("! {VAR} is not set. Export it before running srtty.")`.
3. `refresh(cache_dir, compat_url, REFRESH_TIMEOUT)` (avertissement éventuel passé à `say`), puis `fetch_rows`. Si la liste est inaccessible (clé absente pour un format OpenAI, erreur réseau) : `say("! Could not list models: {cause}")` et on se rabat sur les entrées de `compat.json` de ce fournisseur (via `provider_key`), éventuellement vides.
4. Choix du modèle : `select("Model:", …)` avec les lignes non incompatibles dans l'ordre de `rows` (les modèles facturés au token restent donc en fin de liste), au format `format!("{id}  {prix}  {label}")` (prix `"$0.10/h"`, `"price unknown"`, ou pour un modèle facturé au token `"billed per token, see {url}"`), suivies de `"Show incompatible models"` (s'il y en a) et `"Enter a model id"`. `"Show incompatible models"` → même liste avec toutes les lignes. Un modèle incompatible → `confirm("{id} is marked incompatible. Use it anyway?", false)`. Saisie manuelle → `text("Model id:", None, non vide)`.
5. Test, seulement si la clé est définie : `confirm("Test {id} on a 30 s sample? Estimated cost: ${:.4}.", false)` (ou `"Estimated cost: unknown."`, et `"Estimated cost: unknown (billed per token, see {url})."` pour un modèle facturé au token ; estimation = `usd_per_hour * 30 / 3600`). Résultats : `Usable { words: true, .. }` → `"✔ Test passed: word timestamps available."` ; `Usable { words: false, .. }` → `"✔ Test passed: fine segments, no word timestamps."` ; `Unusable` ou `Incompatible` → `"✘ This model cannot produce subtitles."` puis `confirm("Keep {id} anyway?", false)` (non → retour à l'étape 4) ; `Inconclusive(m)` → `"! Test inconclusive: {m}"` ; `Auth` → `"✘ Authentication failed; check {VAR}."`. Le coût réel, s'il est connu, est ajouté : `" (cost ${:.4})"`.
6. Écriture : fichier existant → `confirm("Update {path}? This sets default_provider = \"{name}\" and replaces [providers.{name}].", false)` ; non → `say("Nothing written.")` et `written: false`. Sinon `apply` puis `write_config`, `say("Wrote {path}.")` et `say("Next: srtty <video-or-audio-file>")`.

Entrée de la commande dans `app.rs` : `--json` → `Usage("`srtty setup` is interactive and does not support --json")` ; stdin ou stderr hors terminal → `Usage("`srtty setup` is interactive; run it in a terminal")`. Sinon `run_setup(&mut InquirePrompter, …)` avec `paths::config_file()`, `paths::cache_dir()`, `compat_url()`, l'environnement réel et `LiveTester`.

- [ ] **Step 1 : tests qui échouent (`setup/mod.rs`)**, avec un `ScriptedPrompter` qui rejoue une liste de réponses (`Select(usize)`, `Text(String)`, `Confirm(bool)`), enregistre chaque message affiché et chaque message d'erreur des validateurs, et un `FakeTester` qui compte ses appels. Le fournisseur est l'URL personnalisée, pointée sur un `MockServer` qui sert `/v1/models` et `/compat.json`.

```rust
#[tokio::test] async fn setup_writes_a_usable_config() {
    // custom → nom "mock" → URL {uri}/v1 → variable K (définie) → modèle "whisper-1" → test refusé
    // → fichier écrit ; Config::load + config::resolve avec K définie donne provider "mock", modèle "whisper-1".
}
#[tokio::test] async fn setup_rejects_a_pasted_key_without_echo() {
    // Réponse Text "gsk_0123secret" puis "K" : le validateur a refusé la première ;
    // ni les messages enregistrés, ni le fichier écrit ne contiennent "0123secret".
}
#[tokio::test] async fn declining_overwrite_leaves_file_untouched() {
    // config existante → Confirm(false) à l'étape 6 → octets identiques, written == false.
}
#[tokio::test] async fn paid_test_runs_only_after_confirmation() {
    // Confirm(false) → FakeTester jamais appelé ; Confirm(true) → appelé une fois, message "Test passed".
}
#[tokio::test] async fn failed_test_returns_to_model_choice() {
    // FakeTester → Unusable ; Confirm(false) à "Keep … anyway?" → nouveau select "Model:".
}
#[tokio::test] async fn local_whisper_is_explained_then_asked_again() { }
#[tokio::test] async fn listing_failure_falls_back_to_manual_entry() {
    // /v1/models → 500 : message "Could not list models", puis "Enter a model id".
}
```

- [ ] **Step 2 : tests bout en bout qui échouent (`tests/cli.rs`)**
- `setup_requires_a_terminal` : `srtty setup` avec stdin redirigé → code 1, stderr contient `interactive`.
- `setup_with_json_prints_json_error` : `srtty setup --json` → code 1, stdout est un JSON avec `exit_code == 1`.

- [ ] **Step 3 : lancer** — `cargo test setup` — Expected : échec.
- [ ] **Step 4 : implémenter.** Les prompts `inquire` sont bloquants : `run_setup` les appelle directement depuis la tâche async ; aucun autre travail ne tourne en parallèle pendant `setup`.
- [ ] **Step 5 : lancer** — `cargo test` puis `cargo clippy --all-targets -- -D warnings` — Expected : PASS.
- [ ] **Step 6 : vérification manuelle sans dépense** — `SRTTY_CONFIG=/tmp/srtty-setup/config.toml cargo run -- setup` dans un vrai terminal, en refusant le test : la config écrite fonctionne avec `cargo run -- config show`.
- [ ] **Step 7 : commit** — `git commit -m "feat: add interactive srtty setup"`

---

### Task 10 : validation réelle (payante, faite par le contrôleur)

**Files:**
- Create: `docs/validation/2026-10-30-plan2.md` (en français)

Clé OpenRouter demandée à Adrien via `request_secret` (delivery file), jamais affichée. Coût estimé total : **environ 0,10 $**, annoncé à Adrien avant de commencer.

- [ ] **Step 1 : `srtty models`** (gratuit) avec et sans `--json`, puis avec `SRTTY_COMPAT_URL=http://127.0.0.1:9` (repli sur le cache, avertissement). Vérifier que `~/.cache/srtty/compat.json` est la copie de `main`.
- [ ] **Step 2 : sonde ciblée** (≈ 0,01 $) : `srtty compat probe --only x-ai/grok-stt-1.0 --only openai/whisper-large-v3-turbo --only google/chirp-3 --output /tmp/srtty-p2/compat.json` (copie du `compat.json` du dépôt). Attendu : grok compatible avec diarisation `xai`, turbo compatible, chirp incompatible ; une seconde exécution dit `unchanged`.
- [ ] **Step 3 : sonde complète** (≈ 0,08 $, plafond 0,50 $) sur une copie : noter le coût réel, les modèles sautés pour le prix (`microsoft/mai-*`, modèles facturés au token) et les écarts avec la liste du spike. Si le résultat diffère, le commiter sur la branche comme premier `compat.json` régénéré.
- [ ] **Step 4 : `setup` de bout en bout** dans un pseudo-terminal (script Python `pty` lancé avec `-I`, ou Adrien à la main si l'automatisation des prompts `inquire` est trop fragile), avec le test payant accepté (≈ 0,001 $), puis un run réel court avec la config produite.
- [ ] **Step 5 : écrire le rapport** (conditions, commandes, résultats, coût réel, écarts) et le commiter : `git commit -m "docs: plan 2 validation against the real API"`.

Après la PR : Adrien ajoute le secret `OPENROUTER_API_KEY` au dépôt et lance le workflow à la main une fois (`gh workflow run compat.yml`).
