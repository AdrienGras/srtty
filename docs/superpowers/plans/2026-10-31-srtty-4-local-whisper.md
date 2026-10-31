# srtty plan 4 : backend local whisper-rs — plan d'implémentation

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Transcrire hors ligne avec whisper.cpp (crate `whisper-rs`) derrière la feature Cargo `local`, avec des timestamps au mot tirés des tokens, les mêmes `Event` que le backend API, et un `setup` qui télécharge un modèle ggml vérifié depuis Hugging Face. Sans la feature, `cargo build` reste sans toolchain C++.

**Architecture:** Un module `src/local/`, compilé seulement avec la feature `local`. `words.rs` convertit les tokens de whisper.cpp en `Word` (fonction pure). `mod.rs` porte `LocalWhisper`, qui implémente `Transcriber` : un seul `WhisperContext` partagé, un `WhisperState` par morceau dans `spawn_blocking`, et un drapeau d'abandon levé quand le future est abandonné. `catalog.rs` et `download.rs` portent la liste figée des modèles et le téléchargement atomique. Hors feature, `config.rs` résout un `Backend` (API ou local) et le chemin du modèle, `app.rs` refuse proprement le local quand la feature manque, et `setup` garde l'option « not available in this build ». Le pipeline (`run.rs`) ne change pas : les morceaux restent en Opus et `LocalWhisper` les décode en PCM avec ffmpeg.

**Tech Stack:** Rust 1.99 (édition 2024), whisper-rs 0.16.0 (whisper.cpp 1.8.3, licence Unlicense), tokio, reqwest 0.13 (déjà présent), sha2, tempfile, inquire.

**Spec:** `docs/superpowers/specs/2026-10-30-srtty-design.md`, surtout §5.3 (trait `Transcriber`), §5.5 (le local refuse `--speakers`), §5.6 (`[providers.local]`), §5.7 et §5.12 (`setup`, distribution, Metal), §5.15 (chemins XDG), §6 (critère « `cargo build` sans toolchain C++ ; avec `--features local`, le même fichier se transcrit hors ligne »), §8 (timestamps de tokens) et §2 (anti-objectifs).

## Global Constraints

- `cargo build` et `cargo test` **sans** feature compilent sans toolchain C++ : `cargo tree -i whisper-rs-sys` doit échouer (« did not match any packages »). Tout ce qui dépend de whisper-rs est sous `#[cfg(feature = "local")]`.
- Dépendances : `whisper-rs = { version = "0.16.0", optional = true }` dans `[dependencies]`, et la même ligne avec `features = ["metal"]` dans `[target.'cfg(target_os = "macos")'.dependencies]`. La feature devient `local = ["dep:whisper-rs"]`. Aucune autre dépendance nouvelle.
- Prérequis du build avec `local` : CMake, un compilateur C/C++ et libclang (bindgen de whisper-rs-sys). Vérifiés sur la machine : cmake 3.28.3, clang 18.1.3, gcc 13.3 ; build à froid de whisper-rs-sys en 28 s.
- Tout texte affiché est en anglais (§5.14).
- Aucun appel réseau pendant un run (§2, §5.8). Le seul nouvel appel est le téléchargement d'un modèle dans `setup`, après confirmation explicite, avec la taille annoncée.
- Le backend local ne coûte rien : `Transcript::cost_usd = None`, aucune question de coût.
- Aucun modèle n'est figé comme défaut d'un run : le run lit `model_path` dans la config. Le catalogue ne sert qu'à `setup`.
- Codes de retour inchangés : `0`, `1`, `2`, `3`, `130`.
- Un seul bloc `unsafe` autorisé : le callback d'abandon de whisper.cpp (décision 6), avec un commentaire `// SAFETY:`.
- Avant chaque commit, **dans les deux configurations** :
  - `cargo fmt --check` ;
  - `cargo clippy --all-targets -- -D warnings` et `cargo clippy --all-targets --features local -- -D warnings` ;
  - `cargo test` et `cargo test --features local` (sortie brute : `rtk proxy cargo test …`).
- Les tests qui demandent un vrai modèle lisent son chemin dans `SRTTY_TEST_WHISPER_MODEL`. Sans cette variable, ils affichent `skipped: SRTTY_TEST_WHISPER_MODEL is not set` sur stderr et passent. Pendant l'exécution, la variable pointe vers `ggml-tiny.bin` (78 Mo).
- Commits terminés par `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Décisions de conception propres à ce plan

1. **Un modèle chargé une fois, un état par morceau.** `WhisperContext` (poids du modèle) est `Send + Sync` et partagé dans un `Arc`. Chaque appel à `transcribe` crée un `WhisperState` (cache KV, tampons de calcul) dans `spawn_blocking`. Les morceaux tournent donc en parallèle sans recharger le modèle.
2. **Parallélisme local** (`config::parallelism`), sur `cores = available_parallelism()` :
   - CPU : `jobs = clamp(cores / 4, 1, 4)` par défaut, puis `threads = clamp(cores / jobs, 1, 8)`. Sur 16 cœurs : 4 morceaux × 4 threads ; sur 8 : 2 × 4 ; sur 2 : 1 × 2.
   - Justification :
     - whisper.cpp prend lui-même `min(4, cœurs)` threads par défaut, et son gain au-delà de 4 à 8 threads par état est faible (calcul limité par la bande passante mémoire) ;
     - des états indépendants sur des morceaux différents se cumulent presque linéairement ;
     - le plafond de 4 morceaux borne la mémoire : un état de large-v3-turbo pèse quelques centaines de Mo en plus des poids partagés.
   - Build Metal (macOS avec `local`) : `jobs = 1` par défaut et `threads = min(4, cores)`. Les états se partagent le même GPU et s'y sérialisent.
   - `--jobs` ou `chunking.jobs` explicites sont respectés, et `threads` suit la même formule. Pour l'API, rien ne change : 4 par défaut.
   - La validation mesure 1 × 16, 4 × 4 et 2 × 8 sur « La Barbe bleue ». Le défaut est corrigé si la mesure le contredit.
3. **Le pipeline ne change pas.** Les morceaux restent en Opus 32 kbit/s, mono, 16 kHz. `LocalWhisper` décode chaque morceau en PCM f32 avec ffmpeg (`-f f32le -ac 1 -ar 16000`), ce qui fait 19 Mo pour 300 s. Whisper travaille de toute façon à 16 kHz, et ffmpeg est déjà requis. La validation compare la qualité au SRT OpenRouter du plan 3.
4. **Mots à partir des tokens** (`words_from_tokens`) :
   - On ignore les tokens spéciaux (`id >= token_eot()` : horodatages, langue, `[_BEG_]`).
   - On travaille sur les **octets** des tokens, parce qu'un caractère accentué peut être coupé entre deux tokens.
   - Un token qui commence par une espace ouvre un mot, les autres se collent au mot courant. Le premier token d'un segment ouvre toujours un mot.
   - Le texte d'un mot est décodé en UTF-8 avec remplacement (`from_utf8_lossy`) et rogné.
   - Temps : `t0` et `t1` sont en centisecondes, on divise par 100. Un mot va du `t0` de son premier token au `t1` de son dernier. Un `end` inférieur à `start` est ramené à `start`, et le début d'un mot n'est jamais avant le début du précédent.
   - On ne passe pas par DTW (`t_dtw`), qui demande un préréglage d'alignement par modèle. Si la validation dépasse 0,5 s d'écart (§6), on l'ajoutera pour les modèles du catalogue, et ce sera consigné en écart.
5. **Décodage** :
   - Recherche en faisceau (`BeamSearch { beam_size: 5, patience: -1.0 }`), le défaut de whisper-cli ;
   - `translate = false`, `token_timestamps = true`, `suppress_nst = true` ;
   - tous les `print_*` à `false`, car `print_realtime` écrit sur **stdout** ;
   - `whisper_rs::install_logging_hooks()` une fois avant le premier chargement : sans backend `log`, les logs de whisper.cpp et ggml sont jetés. Sinon ils iraient sur stderr, sous le tableau de bord ;
   - `--prompt` devient `set_initial_prompt` (le local l'honore, sans avertissement) ;
   - filtre anti-hallucination, sur la règle d'OpenAI Whisper : un segment est jeté si `no_speech_probability > 0.6` **et** la moyenne des `plog` de ses tokens non spéciaux est `< -1.0` ;
   - un morceau de moins de 0,1 s d'échantillons rend un `Transcript` vide (`NoSpeech`), parce que whisper-rs refuse un tableau vide.
6. **Annulation.**
   - Le run abandonne déjà les futures au premier Ctrl-C (`select!` dans `run.rs`), puis `main` sort par `process::exit`. Les threads bloqués ne retardent donc pas la sortie.
   - Pour libérer le CPU tout de suite, et pour les erreurs qui abandonnent les autres morceaux (`try_collect`), `transcribe` crée un `Arc<AtomicBool>` et un garde qui le lève dans son `Drop`. whisper.cpp interroge ce drapeau entre deux passes de l'encodeur ou du décodeur, et dans le calcul ggml.
   - On ne prend pas `set_abort_callback_safe` de whisper-rs 0.16 : son trampoline caste les données en `*mut F` alors qu'il a stocké un `Box<dyn FnMut>`, ce qui est un comportement indéfini. Le callback de progression, lui, est correct.
   - On utilise donc `set_abort_callback` et `set_abort_callback_user_data` avec une `extern "C" fn` qui lit l'`AtomicBool`. L'`Arc` est gardé vivant pendant tout `state.full`.
7. **Chargement et vérifications.**
   - Avant d'ouvrir le tableau de bord : existence du fichier modèle, octets magiques ggml (`6c 6d 67 67`, `0x67676d6c` en little-endian), code de langue reconnu par whisper (`get_lang_id`), refus de `--speakers`. Toutes ces erreurs sont des erreurs d'usage ou de config, code 1.
   - Le modèle est chargé après le dry run et avant `run`, dans `spawn_blocking`, en concurrence avec l'annulation. Un échec de chargement donne une erreur de config (code 1).
   - Un échec pendant la transcription, y compris le décodage ffmpeg d'un morceau, devient `TranscribeError::Fatal` (code 2).
8. **Coût.**
   - `cost_usd: None` dans chaque `Transcript`. Pas d'entrée `compat.json`, donc pas d'estimation ni de question de coût.
   - Le dry run affiche `no API cost: the local backend runs on this machine`, et `estimated_cost_usd: null` en JSON, comme le résumé.
   - Le tableau de bord n'affiche pas de coût quand il est inconnu, et le récapitulatif affiche `cost n/a` : rien ne change dans `src/dashboard/`.
9. **Cache.** `CacheParams { base_url: "local", model: "<nom du fichier>#<taille en octets>", … }`. Remplacer le fichier du modèle invalide le cache sans hacher 1,6 Go à chaque run.
10. **`Backend` remplace `require_key`** (point reporté du plan 2). `Resolved::backend()` rend soit `Backend::Api { base_url, key, mode }`, soit `Backend::Local { model_path }`, avec les erreurs actuelles (clé absente, `base_url` absent). `require_key` disparaît, et un fournisseur local ne fabrique plus de clé vide.
11. **Chemin du modèle.**
    - Pour un fournisseur local, `--model` et `SRTTY_MODEL` donnent un **chemin** qui remplace `model_path`.
    - Un `~/` en tête est développé.
    - `Resolved::model` devient le nom du fichier (`ggml-base.bin`), affiché dans le titre du tableau de bord et dans le résumé.
12. **Catalogue figé.**
    - Les modèles multilingues du dépôt `ggerganov/whisper.cpp` sont figés à la révision `5359861c739e955e79d9a303bcbc70fb988958b1`, avec leur taille et leur SHA-256 (API Hugging Face, relevé du 2026-10-31). URL : `https://huggingface.co/ggerganov/whisper.cpp/resolve/<révision>/<fichier>`.
    - Les variantes `.en` sont exclues : elles ignorent `--language`.
    - Le téléchargement passe par un fichier temporaire dans le dossier de destination, hache au fil de l'eau, vérifie la taille et le SHA-256, synchronise le fichier sur disque, puis le renomme.
    - En cas d'échec, d'annulation ou de disque plein, le fichier temporaire est supprimé et le chemin final n'existe pas.
13. **Langue** (question ouverte, voir la fin du plan) : proposition par défaut, **détecter sur le premier morceau, puis figer**.
    - Sans `--language`, le premier morceau qui contient des mots fixe la langue détectée (`OnceLock` dans `LocalWhisper`), et les morceaux suivants la reçoivent.
    - Raison : whisper détecte la langue sur les 30 premières secondes de chaque morceau, et un morceau qui commence par de la musique peut être mal détecté.
    - Limite connue : si le premier morceau vient du cache, les suivants détectent chacun leur langue.
14. **Sans la feature** :
    - un run sur un fournisseur local s'arrête avec le code 1 et le message `the local backend is not available in this build; reinstall with: cargo install srtty --features local` ;
    - `setup` garde l'option `Local Whisper (not available in this build)` et son message actuel.

## Review Focus

1. **whisper.cpp ou ggml qui écrit sur stdout ou stderr** (logs d'initialisation, `print_realtime`). Ça casserait `--json` et le tableau de bord. Attendu : stdout ne contient que l'objet JSON, et stderr ne contient aucune ligne `whisper_` ou `ggml_`. Testé par `e2e_local_json_run_is_offline_and_quiet` (Task 4).
2. **Accents français coupés entre deux tokens** (`é` = `C3 A9` sur deux tokens). Attendu : des mots intacts, sans `�`. Testé par `utf8_split_across_tokens_is_rejoined` (Task 2).
3. **Silence, musique ou morceau très court.** Attendu : un `Transcript` vide ou des mots réels, jamais une erreur et jamais la phrase hallucinée « Sous-titres réalisés par la communauté d'Amara.org ». Testé par `silence_is_no_speech` et `segment_filter_follows_openai_rule` (Task 3).
4. **Ctrl-C pendant un long morceau sur CPU.** Attendu : code 130 sans attendre la fin du morceau, et un CPU libéré peu après. Testé par `dropping_transcribe_aborts_whisper` (Task 3), puis dans le pty à la validation.
5. **Téléchargement interrompu, corrompu ou refusé** (coupure réseau, mauvais SHA-256, 404, Ctrl-C, disque plein). Attendu : aucun fichier au chemin final, aucun `.part` laissé, et une relance de `setup` qui repart de zéro. Testé par `hash_mismatch_leaves_nothing`, `short_body_leaves_nothing` et `dropping_the_download_leaves_no_file` (Task 5).

---

## Structure des fichiers

| Fichier | Rôle |
| --- | --- |
| `Cargo.toml` | feature `local = ["dep:whisper-rs"]`, whisper-rs optionnel, Metal sur macOS |
| `src/config.rs` | `ChunkingConfig::jobs: Option<usize>`, `Resolved::{jobs, model_path}`, `Backend`, `Resolved::backend`, `Parallelism`, `parallelism`, `available_cores`, `GPU_BUILD` ; suppression de `require_key` |
| `src/paths.rs` | `expand_tilde`, `data_dir`, `models_dir` |
| `src/local/mod.rs` (créé, feature) | `LocalWhisper` (impl `Transcriber`), `check_ggml_magic`, `check_language`, `keep_segment` |
| `src/local/words.rs` (créé, feature) | `TokenPiece`, `words_from_tokens` |
| `src/local/catalog.rs` (créé, feature) | `CatalogModel`, `CATALOG`, `HF_REVISION`, `model_url`, `human_size` |
| `src/local/download.rs` (créé, feature) | `http_client`, `download`, `is_complete` |
| `src/media.rs` | `decode_pcm_f32` (feature) ; `run` rend les octets bruts de stdout |
| `src/run.rs` | `check_input`, appelée par `prepare` et par `app::transcribe` |
| `src/app.rs` | `Backend` dans `Job`, refus du local sans feature, refus de `--speakers`, vérifications avant le tableau de bord, dry run local, chargement du modèle, `config show` (`model_path`) |
| `src/setup/mod.rs`, `src/setup/local.rs` (créé, feature), `src/setup/write.rs` | parcours local de `setup`, `ModelFetcher`, `LocalTester`, `apply_local` |
| `src/lib.rs` | `#[cfg(feature = "local")] pub mod local;` |
| `tests/cli.rs` | e2e sans feature ; `tests/local.rs` (créé, `#![cfg(feature = "local")]`) |

---

### Task 1 : `Backend`, chemin du modèle et parallélisme dans la config

**Files:**
- Modify: `src/config.rs` (`ChunkingConfig`, `Resolved`, `resolve`, tests)
- Modify: `src/paths.rs`
- Modify: `src/app.rs` (`check_settings`, `ConfigView`, `transcribe` : passer de `require_key` à `backend()`, sans encore brancher le local)

**Interfaces:**
- Produces :
  - `paths::expand_tilde(path: &Path, home: Option<&Path>) -> PathBuf` : remplace un `~` seul ou un `~/` en tête ; sinon, le chemin est rendu tel quel ;
  - `paths::data_dir() -> PathBuf` (`$XDG_DATA_HOME/srtty`, par défaut `~/.local/share/srtty`) et `paths::models_dir() -> PathBuf` (`data_dir().join("models")`) ;
  - `ChunkingConfig { target_s: f64, jobs: Option<usize> }` (défaut `None`) ;
  - `Resolved { jobs: Option<usize>, model_path: Option<PathBuf>, .. }` ;
  - `pub enum Backend { Api { base_url: String, key: ApiKey, mode: RequestMode }, Local { model_path: PathBuf } }` et `Resolved::backend(&self) -> Result<Backend, SrttyError>` ;
  - `pub struct Parallelism { pub jobs: usize, pub threads_per_job: Option<usize> }` et `pub fn parallelism(kind: ProviderKind, jobs: Option<usize>, cores: usize, gpu: bool) -> Parallelism` (règles de la décision 2 ; `threads_per_job` vaut `None` pour l'API) ;
  - `pub const DEFAULT_API_JOBS: usize = 4` ;
  - `pub const GPU_BUILD: bool = cfg!(all(feature = "local", target_os = "macos"))` ;
  - `pub fn available_cores() -> usize` (`available_parallelism`, 1 en cas d'échec).

- [ ] **Step 1 : écrire les tests qui échouent** dans `config.rs` et `paths.rs`.
  - `parallelism_rules`, avec ces cas :
    - `(Api, None, 16, false)` → `{4, None}` ;
    - `(Local, None, 16, false)` → `{4, Some(4)}` ;
    - `(Local, None, 8, false)` → `{2, Some(4)}` ;
    - `(Local, None, 2, false)` → `{1, Some(2)}` ;
    - `(Local, None, 1, false)` → `{1, Some(1)}` ;
    - `(Local, None, 64, false)` → `{4, Some(8)}` ;
    - `(Local, Some(2), 16, false)` → `{2, Some(8)}` ;
    - `(Local, Some(32), 16, false)` → `{32, Some(1)}` ;
    - `(Local, None, 10, true)` → `{1, Some(4)}` ;
    - `(Api, Some(7), 16, false)` → `{7, None}`.
  - `local_provider_resolves_its_model_path` :
    - config `[providers.l] kind = "local"`, `model_path = "/m/ggml-base.bin"` ;
    - `r.model == "ggml-base.bin"` ;
    - `r.backend()` rend `Backend::Local { model_path: "/m/ggml-base.bin" }`.
  - `local_model_path_expands_tilde` : `model_path = "~/m/ggml-base.bin"` donne un chemin qui ne commence plus par `~`. `resolve` passe `etcetera::home_dir()` à `expand_tilde`, et les cas précis sont testés dans `paths.rs`.
  - `model_flag_overrides_local_model_path` : `--model /tmp/x/ggml-tiny.bin` → `model_path == /tmp/x/ggml-tiny.bin` et `model == "ggml-tiny.bin"`. Même résultat avec `SRTTY_MODEL`.
  - `local_provider_without_model_path_is_a_config_error` : le message contient `model_path` et `srtty setup`.
  - `api_backend_carries_key_and_base_url` remplace les tests de `require_key` : clé présente → `Backend::Api` avec la clé ; clé absente → l'erreur cite `OPENROUTER_API_KEY` ; `base_url` absent → l'erreur cite `base_url`.
  - `expand_tilde_cases` dans `paths.rs` :
    - `~/a` → `<home>/a` ;
    - `~` → `<home>` ;
    - `~x/a` reste tel quel ;
    - `/abs` reste tel quel ;
    - sans home, `~/a` reste tel quel.
  - `defaults_match_spec` : `chunking.jobs == None`.
- [ ] **Step 2 : lancer `cargo test --lib config paths`.** Attendu : échec de compilation (`Backend`, `parallelism` et `expand_tilde` absents).
- [ ] **Step 3 : implémenter.**
  - `resolve` :
    - pour `ProviderKind::Local`, le chemin vient de `cli.model`, puis `SRTTY_MODEL`, puis `provider.model_path`, passe par `expand_tilde`, et `model` prend le nom du fichier ;
    - pour l'API, rien ne change, et `model_path` vaut `None` ;
    - `jobs = cli.jobs.or(cfg.chunking.jobs)`.
  - Le message d'erreur sans chemin : `provider \`{name}\` has no \`model_path\`; run \`srtty setup\` or pass --model <path to a ggml model>`.
  - `check_settings` refuse `jobs == Some(0)`, avec le même message qu'aujourd'hui.
  - `ConfigView` :
    - ajoute `model_path` (omis si absent) ;
    - affiche `jobs = parallelism(kind, r.jobs, available_cores(), GPU_BUILD).jobs` ;
    - ajoute `threads_per_job` pour le local.
  - Dans `transcribe`, `r.require_key()` et la lecture de `base_url` sont remplacés par `r.backend()?`. Un `Backend::Local` y garde pour l'instant l'erreur `local backend is not available in this build`, et la Task 4 le branche.
  - `run_job` reçoit `parallelism(...).jobs` à la place de `r.jobs`.
- [ ] **Step 4 : lancer `cargo test` puis `cargo clippy --all-targets -- -D warnings`.** Attendu : tout passe. `grep -rn require_key src` ne trouve plus rien.
- [ ] **Step 5 : commit** `refactor: resolve a Backend instead of a possibly empty key`.

---

### Task 2 : la feature `local` et les mots tirés des tokens

**Files:**
- Modify: `Cargo.toml`, `src/lib.rs`
- Create: `src/local/mod.rs` (pour l'instant : la doc du module et `pub mod words;`), `src/local/words.rs`

**Interfaces:**
- Produces :
  - `pub struct TokenPiece { pub bytes: Vec<u8>, pub t0_cs: i64, pub t1_cs: i64 }` : un token non spécial ;
  - `pub fn words_from_tokens(segments: &[Vec<TokenPiece>]) -> Vec<Word>` (règles de la décision 4, `speaker: None`).

- [ ] **Step 1 : déclarer la feature** (Global Constraints), puis vérifier que la frontière tient.
  - `cargo tree -i whisper-rs-sys` → erreur « did not match any packages ».
  - `cargo tree --features local -i whisper-rs-sys` → l'arbre s'affiche.
  - `cargo tree --target aarch64-apple-darwin --features local -e features -i whisper-rs-sys | grep metal` → `whisper-rs-sys feature "metal"`.
- [ ] **Step 2 : écrire les tests qui échouent** dans `words.rs`. Un helper `tp(text_or_bytes, t0, t1)` construit les `TokenPiece`.
  - `leading_space_opens_a_word` : `" La"(0,20) " Bar"(20,40) "be"(40,55) " bleue"(55,90) ","(90,92)` → `["La" 0.0–0.2, "Barbe" 0.2–0.55, "bleue," 0.55–0.92]`.
  - `utf8_split_across_tokens_is_rejoined` : `" caf"(0,30)`, puis les octets `[0xC3]`(30,35), puis `[0xA9]`(35,40) → un seul mot, `"café"`, de 0.0 à 0.4.
  - `segment_boundary_starts_a_word` : segment 1 `" fin"`, segment 2 `"suite"` sans espace → deux mots.
  - `inverted_and_backward_times_are_clamped` : un token avec `t1 < t0` donne `end == start`. Un mot dont `t0` précède le début du mot précédent commence au début du précédent.
  - `blank_tokens_are_dropped` : `" "` seul ne produit aucun mot.
  - `invalid_utf8_is_replaced_not_panicking` : `" a"` suivi de `[0xFF]` donne `"a\u{FFFD}"`.
- [ ] **Step 3 : lancer `cargo test --features local local::words`.** Attendu : échec (fonction absente).
- [ ] **Step 4 : implémenter `words_from_tokens`.** On accumule les octets et les temps du mot courant. On ferme le mot sur un token qui commence par `b' '` ou sur un changement de segment. À la fermeture, `from_utf8_lossy` puis `trim`, et le mot est jeté s'il est vide.
- [ ] **Step 5 : lancer les tests et les vérifications** dans les deux configurations (Global Constraints). Attendu : tout passe. `cargo build` sans feature ne compile pas `whisper-rs-sys`.
- [ ] **Step 6 : commit** `feat: add the local feature and build words from whisper tokens`.

---

### Task 3 : `LocalWhisper`

**Files:**
- Modify: `src/media.rs` (`decode_pcm_f32`, et `run` qui rend les octets de stdout)
- Modify: `src/local/mod.rs`

**Interfaces:**
- Consumes : `words_from_tokens`, `TokenPiece` (Task 2).
- Produces :
  - `media::decode_pcm_f32(path: &Path) -> Result<Vec<f32>, SrttyError>` (feature `local`) : `ffmpeg -nostdin -hide_banner -v error -i <path> -f f32le -ac 1 -ar 16000 -`, avec le même `run` que les autres appels (groupe de processus, `kill_on_drop`) ;
  - `pub fn check_ggml_magic(path: &Path) -> Result<(), SrttyError>` : `Usage("model file not found: <p>; run \`srtty setup\` to download one")` si le fichier n'existe pas, `Config("<p> is not a ggml whisper model")` si les 4 premiers octets ne sont pas `6c 6d 67 67` ;
  - `pub fn check_language(code: &str) -> Result<(), SrttyError>` : `Usage("unknown language \`<code>\` for the local backend; use an ISO 639-1 code such as fr or en")` quand `whisper_rs::get_lang_id` rend `None` ;
  - `pub fn keep_segment(no_speech_prob: f32, mean_plog: f32) -> bool` (décision 5) ;
  - `pub struct LocalWhisper` et `pub async fn LocalWhisper::load(path: &Path, threads: usize) -> Result<LocalWhisper, SrttyError>` :
    - `check_ggml_magic`, puis `install_logging_hooks`, puis `WhisperContext::new_with_params` dans `spawn_blocking` ;
    - un échec donne `Config("cannot load the whisper model <p>: <e>")` ;
    - `model()` rend le nom du fichier ;
  - `impl Transcriber for LocalWhisper` (décisions 3 à 6 et 13) ;
  - `#[cfg(test)] fn running(&self) -> usize` : le nombre d'appels à `state.full` en cours (`Arc<AtomicUsize>`, incrémenté et décrémenté dans le thread bloquant) ;
  - `#[cfg(test)] fn pinned_language(&self) -> Option<String>`.

- [ ] **Step 1 : écrire les tests qui échouent.**
  - Sans modèle :
    - `decode_pcm_f32_reads_16k_mono` (dans `media.rs`, feature) : un ton de 1 s généré par ffmpeg en `.ogg` libopus → entre 15 800 et 16 200 échantillons, avec un maximum en valeur absolue au-dessus de 0,1 ;
    - `check_ggml_magic_cases` : un fichier qui commence par `lmgg` passe ; un faux mp3 (`ID3…`) donne une erreur de code 1 qui contient `not a ggml whisper model` ; un chemin absent donne une erreur qui contient `srtty setup` ;
    - `check_language_cases` : `fr` et `en` passent, `french` échoue avec un message qui contient `ISO 639-1` ;
    - `segment_filter_follows_openai_rule` : `keep_segment(0.9, -1.5) == false`, `keep_segment(0.9, -0.5) == true`, `keep_segment(0.2, -1.5) == true` ;
    - `load_rejects_a_non_ggml_file` : `LocalWhisper::load` sur le faux mp3 → `exit_code() == 1`.
  - Avec `SRTTY_TEST_WHISPER_MODEL` :
    - `transcribes_the_embedded_sample_offline` : `probe::SAMPLE` écrit avec `write_sample`, puis `transcribe` sans langue →
      - `usability() == Usability::Words` ;
      - au moins 40 mots ;
      - des `start` croissants au sens large, tous dans `[0, 30.5]` ;
      - `language == Some("fr")` ;
      - `cost_usd == None`.
    - `silence_is_no_speech` : 10 s d'`anullsrc` encodées en Opus → `usability() == NoSpeech`.
    - `dropping_transcribe_aborts_whisper` :
      - l'échantillon bouclé 10 fois avec ffmpeg (`-stream_loop 9`, 300 s) ;
      - `tokio::time::timeout(300 ms, transcribe)` expire ;
      - `running()` revient à 0 en moins de 2 s (sondé toutes les 50 ms).
    - `language_is_pinned_after_the_first_chunk` : après le premier `transcribe` sans langue, `pinned_language() == Some("fr")`.
- [ ] **Step 2 : lancer `cargo test --features local local media`.** Attendu : échec de compilation.
- [ ] **Step 3 : implémenter.**
  - Le corps de `transcribe` suit cet ordre :
    1. décoder le PCM ;
    2. passer en court-circuit sous 0,1 s ;
    3. choisir la langue : `opts.language`, sinon la langue figée, sinon `"auto"` ;
    4. armer le garde d'abandon ;
    5. lancer `spawn_blocking` : création de l'état, `FullParams`, puis `state.full` ;
    6. relire les segments et les tokens : un token est ignoré si `token_id() >= ctx.token_eot()`, `to_bytes()` et `token_data().t0/t1/plog` sinon ;
    7. appliquer `keep_segment` ;
    8. construire le `Transcript` :
       - `text` : les textes des segments gardés, rognés et joints par une espace ;
       - `words` : la sortie de `words_from_tokens` ;
       - `segments` : en secondes ;
       - `language` : `get_lang_str(full_lang_id_from_state())` ;
       - `cost_usd` : `None` ;
    9. figer la langue si `opts.language` est absent et qu'il y a des mots.
  - Le callback d'abandon :

```rust
unsafe extern "C" fn abort_requested(data: *mut std::ffi::c_void) -> bool {
    // SAFETY: `data` points into the `Arc<AtomicBool>` that `run_full` keeps alive
    // until `state.full` returns; whisper.cpp only calls this during `full`.
    unsafe { (*(data as *const AtomicBool)).load(Ordering::Relaxed) }
}
```

  - Les erreurs de whisper et de ffmpeg pendant `transcribe` deviennent `TranscribeError::Fatal { status: None, message: format!("chunk {}: {e}", index + 1) }`.
- [ ] **Step 4 : lancer les tests avec et sans `SRTTY_TEST_WHISPER_MODEL`, puis les vérifications des deux configurations.** Attendu : tout passe. Sans la variable, les tests de modèle affichent `skipped`.
- [ ] **Step 5 : commit** `feat: transcribe chunks offline with whisper.cpp`.

---

### Task 4 : brancher le backend local dans un run

**Files:**
- Modify: `src/app.rs`, `src/run.rs` (`check_input`)
- Modify: `tests/cli.rs` ; Create: `tests/local.rs` (`#![cfg(feature = "local")]`)

**Interfaces:**
- Consumes : `Backend`, `parallelism`, `available_cores`, `GPU_BUILD` (Task 1) ; `LocalWhisper::load`, `check_ggml_magic`, `check_language` (Task 3).
- Produces :
  - `run::check_input(input: &Path) -> Result<(), SrttyError>` : les deux erreurs actuelles de `prepare` (`input file not found`, `input is not a file`), que `prepare` appelle aussi ;
  - `pub(crate) const LOCAL_UNAVAILABLE: &str = "the local backend is not available in this build; reinstall with: cargo install srtty --features local"` ;
  - `pub(crate) const LOCAL_NO_SPEAKERS: &str = "--speakers is not supported by the local backend; use an API provider with a diarization model (see \`srtty models\`)"`.

- [ ] **Step 1 : écrire les tests qui échouent.**
  - Dans `tests/cli.rs`, sans feature (`#[cfg(not(feature = "local"))]`) : `e2e_local_provider_needs_the_feature`.
    - Config `[providers.local] kind = "local"`, `model_path = "/nonexistent/ggml-base.bin"`.
    - Attendu : code 1, stderr contient `cargo install srtty --features local`, stdout vide.
  - Dans `tests/local.rs`, avec un `Sandbox` local (même forme que celui de `tests/cli.rs` : `SRTTY_CONFIG`, `XDG_CACHE_HOME`, config `[providers.local]`) :
    - `e2e_local_speakers_refused_before_anything` : `--speakers` avec un `model_path` absent → code 1, et le message contient `--speakers is not supported by the local backend`, pas `model file not found`.
    - `e2e_local_missing_input_comes_first` : entrée et modèle absents → code 1, message `input file not found`.
    - `e2e_local_missing_model_exit_1` : entrée présente (ton de `tone_mkv`), modèle absent → code 1, message `model file not found` qui contient `srtty setup`.
    - `e2e_local_unknown_language_exit_1` : `--language french` avec un faux modèle aux bons octets magiques (fichier de 4 octets `lmgg`) → code 1, et le message contient `ISO 639-1`. La vérification passe avant le chargement.
    - `e2e_local_dry_run_has_no_cost` : avec le faux modèle aux bons octets magiques →
      - `--dry-run` donne `no API cost` sur stderr ;
      - `--dry-run --json` donne `"estimated_cost_usd":null` ;
      - aucun chargement : le faux modèle ferait échouer whisper.
    - Avec `SRTTY_TEST_WHISPER_MODEL` : `e2e_local_json_run_is_offline_and_quiet`.
      - Entrée : `assets/sample-30s.ogg` copiée dans le bac à sable.
      - Environnement : `HTTPS_PROXY`, `HTTP_PROXY` et `ALL_PROXY` à `http://127.0.0.1:9`, pour qu'un appel réseau éventuel échoue.
      - Commande : `--json --format srt,txt`.
      - Attendu :
        - code 0 ;
        - stdout est **un seul** objet JSON, avec `cost_usd` à `null` et `provider` à `"local"` ;
        - le `.srt` existe et contient au moins 5 sous-titres ;
        - stderr ne contient ni `whisper_` ni `ggml_`.
- [ ] **Step 2 : lancer `cargo test --test cli` et `cargo test --features local --test local`.** Attendu : échecs (messages et ordre des vérifications).
- [ ] **Step 3 : implémenter dans `app::transcribe`**, avant `stop_signals` et le tableau de bord, dans cet ordre :
  1. l'entrée donnée ;
  2. `load_settings` ;
  3. `backend()` ;
  4. si le backend est local :
     - sans la feature : `Config(LOCAL_UNAVAILABLE)` ;
     - avec `--speakers` : `Usage(LOCAL_NO_SPEAKERS)` ;
  5. `check_input` ;
  6. si le backend est local (feature) : `check_ggml_magic(model_path)`, puis `check_language` quand une langue est donnée.

  Ensuite :
  - `Job` porte `backend: Backend` à la place de `api_key` et `base_url`.
  - Dans `run_job` :
    - `entry` et l'avertissement `--prompt` ne concernent que l'API ;
    - `print_dry_run` reçoit `local: bool` (décision 8) ;
    - `CacheParams` suit la décision 9 ; la taille vient de `std::fs::metadata` ;
    - `jobs` et `threads_per_job` viennent de `parallelism`.
  - Pour l'API, `OpenAiCompatible::new` comme aujourd'hui. Pour le local, `LocalWhisper::load(&model_path, threads)` dans un `tokio::select!` avec `cancel.cancelled()` (→ `Interrupted`), placé après la question de coût et avant `run`.
- [ ] **Step 4 : lancer les tests et les vérifications des deux configurations, avec et sans `SRTTY_TEST_WHISPER_MODEL`.** Attendu : tout passe.
- [ ] **Step 5 : commit** `feat: run transcriptions with the local whisper backend`.

---

### Task 5 : catalogue des modèles et téléchargement vérifié

**Files:**
- Create: `src/local/catalog.rs`, `src/local/download.rs` ; Modify: `src/local/mod.rs` (`pub mod catalog; pub mod download;`)

**Interfaces:**
- Produces :
  - `pub const HF_REVISION: &str = "5359861c739e955e79d9a303bcbc70fb988958b1"` ;
  - `pub struct CatalogModel { pub name: &'static str, pub file: &'static str, pub bytes: u64, pub sha256: &'static str, pub note: &'static str }` ;
  - `pub const CATALOG: &[CatalogModel]`, dans cet ordre (premier = proposé, voir la question 1) :

| name | file | bytes | sha256 | note |
| --- | --- | --- | --- | --- |
| `large-v3-turbo-q5_0` | `ggml-large-v3-turbo-q5_0.bin` | 574041195 | `394221709cd5ad1f40c46e6031ca61bce88931e6e088c188294c6d5a55ffa7e2` | `best accuracy for its size (recommended)` |
| `large-v3-turbo` | `ggml-large-v3-turbo.bin` | 1624555275 | `1fc70f774d38eb169993ac391eea357ef47c88757ef72ee5943879b7e8e2bc69` | `full precision, slowest on CPU` |
| `small` | `ggml-small.bin` | 487601967 | `1be3a9b2063867b937e64e2ec7483364a79917e157fa98c5d94b5c1fffea987b` | `balanced` |
| `base` | `ggml-base.bin` | 147951465 | `60ed5bc3dd14eea856493d334349b405782ddcaf0028d4b5df4088345fba2efe` | `fast, lower accuracy` |
| `tiny` | `ggml-tiny.bin` | 77691713 | `be07e048e1e599ad46341c8d2a135645097a538221678b7acdd1b1919c6e1b21` | `fastest, for quick tests` |

  - `pub fn model_url(file: &str) -> String` : `https://huggingface.co/ggerganov/whisper.cpp/resolve/{HF_REVISION}/{file}` ;
  - `pub fn human_size(bytes: u64) -> String` : `78 MB`, `574 MB`, `1.6 GB` (unités décimales ; Go au-delà de 999 Mo, avec une décimale) ;
  - `pub fn http_client() -> Result<reqwest::Client, SrttyError>` : `connect_timeout` 10 s, `read_timeout` 30 s, pas de timeout total, `user_agent("srtty/<version>")` ;
  - `pub async fn download(client: &reqwest::Client, url: &str, dest: &Path, bytes: u64, sha256: &str, progress: &mut (dyn FnMut(u64) + Send)) -> Result<(), SrttyError>` :
    - crée le dossier, puis le fichier temporaire `tempfile::Builder::new().prefix(".srtty-").suffix(".part").tempfile_in(dir)` **avant** la requête ;
    - lit le corps avec `Response::chunk()`, en hachant au fil de l'eau, et appelle `progress(total_reçu)` ;
    - s'arrête dès que le corps dépasse `bytes` ;
    - à la fin : vérifie la taille et le SHA-256, appelle `sync_all`, puis `persist(dest)` ;
    - erreurs :
      - un statut HTTP d'erreur donne `Network("download failed: HTTP <s> for <url>")` ;
      - une taille ou une empreinte inattendue donne `Network("downloaded file does not match the expected size or checksum; nothing was saved")` ;
      - le reste donne `Io` ;
  - `pub fn is_complete(path: &Path, bytes: u64, sha256: &str) -> std::io::Result<bool>` : compare la taille, et ne hache que si elle correspond.

- [ ] **Step 1 : écrire les tests qui échouent.**
  - `catalog.rs` :
    - `catalog_is_pinned_and_well_formed` : chaque `sha256` fait 64 caractères hexadécimaux minuscules ; chaque `file` vaut `ggml-<name>.bin` ; `model_url` contient `HF_REVISION` ; aucun nom ne finit par `.en` ;
    - `human_size_cases` : 77691713 → `78 MB`, 574041195 → `574 MB`, 1624555275 → `1.6 GB`.
  - `download.rs`, avec wiremock et un corps de 64 Kio dont le SHA-256 est calculé dans le test :
    - `download_verifies_and_renames` : le contenu de `dest` est identique ; le dossier ne contient que `dest` ; la dernière valeur de `progress` vaut 65536 ;
    - `hash_mismatch_leaves_nothing` : erreur, `exit_code() == 2`, dossier vide ;
    - `short_body_leaves_nothing` : `bytes` annoncé à 65537 → erreur, dossier vide ;
    - `long_body_is_cut_short` : `bytes` annoncé à 1000 → erreur, dossier vide ;
    - `http_error_is_a_network_error` : un 404 donne une erreur de code 2 dont le message contient `HTTP 404`, et le dossier reste vide ;
    - `dropping_the_download_leaves_no_file` : réponse retardée de 2 s (`set_delay`), `timeout(200 ms)` expire, et le dossier reste vide ;
    - `is_complete_checks_size_then_hash` : bonne taille et bon hash → `true` ; bonne taille et mauvais contenu → `false` ; mauvaise taille → `false` ; fichier absent → `false`.
- [ ] **Step 2 : lancer `cargo test --features local local::catalog local::download`.** Attendu : échec de compilation.
- [ ] **Step 3 : implémenter.**
- [ ] **Step 4 : lancer les tests et les vérifications des deux configurations.** Attendu : tout passe.
- [ ] **Step 5 : commit** `feat: download verified ggml models from a pinned Hugging Face revision`.

---

### Task 6 : le parcours local de `setup`

**Files:**
- Create: `src/setup/local.rs` (feature)
- Modify: `src/setup/mod.rs` (`choose_provider`, `run_setup`, `SetupContext`, `finish`, tests), `src/setup/write.rs` (`apply_local`), `src/app.rs` (`setup`)

**Interfaces:**
- Consumes : `CATALOG`, `CatalogModel`, `human_size`, `model_url`, `http_client`, `download`, `is_complete` (Task 5) ; `LocalWhisper`, `check_ggml_magic` (Task 3) ; `paths::models_dir`, `paths::expand_tilde` (Task 1) ; `probe::{SAMPLE, SAMPLE_S, write_sample}`.
- Produces :
  - les traits et types :
    - `#[async_trait] pub trait ModelFetcher: Send + Sync { async fn fetch(&self, model: &CatalogModel, dest: &Path) -> Result<(), SrttyError>; }` ;
    - `#[async_trait] pub trait LocalTester: Send + Sync { async fn test(&self, model_path: &Path) -> Result<LocalTestOutcome, SrttyError>; }` ;
    - `pub struct LocalTestOutcome { pub words: usize, pub secs: f64, pub language: Option<String> }` ;
    - `pub struct LocalSetup<'a> { pub models_dir: PathBuf, pub catalog: &'a [CatalogModel], pub fetcher: &'a dyn ModelFetcher, pub tester: &'a dyn LocalTester }` ;
  - `pub async fn choose_local_model(p: &mut dyn Prompter, local: &LocalSetup<'_>) -> Result<PathBuf, SrttyError>` ;
  - les implémentations réelles :
    - `pub struct LiveFetcher` : `download` vers `models_dir/<file>` ; une ligne de progression sur stderr, `\rDownloading <file>: 45% (258 MB of 574 MB)`, au plus toutes les 250 ms, suivie d'un saut de ligne ; un `tokio::select!` avec `tokio::signal::ctrl_c()` qui donne `Interrupted` ;
    - `pub struct LiveLocalTester` : `LocalWhisper::load(path, available_cores().min(8))`, puis `transcribe` de l'extrait embarqué sans langue, le tout dans un `select!` avec `ctrl_c()` ;
  - `SetupContext` gagne `#[cfg(feature = "local")] pub local: LocalSetup<'a>` ;
  - `write::apply_local(existing: Option<&str>, name: &str, model_path: &Path) -> Result<String, SrttyError>` (feature) :
    - met `default_provider = name`, `kind = "local"` et `model_path` ;
    - retire `base_url`, `api_key_env`, `model` et `request_mode` ;
    - garde les commentaires et les autres fournisseurs ;
    - un chemin non UTF-8 donne `Config("the model path must be valid UTF-8")`.

- [ ] **Step 1 : écrire les tests qui échouent.** Dans `setup/mod.rs`, réutiliser `ScriptedPrompter`, avec ces fakes :
  - `FakeFetcher` : il écrit le contenu attendu et compte ses appels ; une variante échoue ;
  - `FakeTester` : succès ou échec, au choix ;
  - un catalogue de test d'une entrée, `tiny-test`, dont le contenu (`b"lmgg-test-model"`) a sa taille et son SHA-256 calculés dans le test.

  Sans feature, `local_whisper_is_explained_then_asked_again` reste tel quel, sous `#[cfg(not(feature = "local"))]`. Le helper de contexte des tests ajoute le champ `local` sous `#[cfg(feature = "local")]`.

  Avec la feature :
  - `local_option_is_available_with_the_feature` : l'option affichée est `Local Whisper (offline, runs on this machine)`.
  - `local_download_flow_writes_a_local_provider` : choisir Local, `tiny-test`, confirmer le téléchargement, accepter le test, puis écriture. Attendu :
    - le fetcher est appelé une fois ;
    - le config parsée a `default_provider = "local"`, `kind = Local` et `model_path = <models_dir>/ggml-tiny-test.bin` ;
    - aucun `api_key_env`, aucune question sur une variable d'environnement ;
    - les messages comprennent `Test passed` et `Wrote`.
  - `local_already_downloaded_skips_fetch` : le fichier est déjà là et complet → `✔ ggml-tiny-test.bin is already downloaded.`, et le fetcher n'est pas appelé.
  - `local_declined_download_returns_to_the_menu` : le menu des modèles est demandé deux fois.
  - `local_download_failure_is_reported_and_asked_again` : `✘ Download failed: …`, puis le menu de nouveau.
  - `local_existing_file_is_validated` : le validateur du chemin refuse un chemin absent (`No file at that path.`) et un faux mp3 (`That file is not a ggml Whisper model.`). Il accepte un fichier qui commence par `lmgg` ; le chemin avec `~/` est développé avant l'écriture.
  - `local_failed_test_can_be_rejected` : le tester échoue, `Keep this model anyway?` reçoit non, et le menu revient.
  - `local_interrupt_during_download_propagates` : le fetcher rend `Interrupted` → `run_setup` rend `Interrupted`.

  Dans `write.rs` : `apply_local_replaces_api_keys_and_keeps_comments`. Partir d'un `[providers.local]` d'API commenté, avec un autre fournisseur à côté. Attendu : `kind = "local"` et `model_path` sont posés ; `base_url`, `api_key_env` et `model` sont retirés ; le commentaire et l'autre fournisseur restent.
- [ ] **Step 2 : lancer `cargo test --features local setup` et `cargo test setup`.** Attendu : échecs avec la feature, succès sans.
- [ ] **Step 3 : implémenter.**
  - `choose_provider` rend `enum ProviderPick { Api { name, base_url, default_var }, Local }`. Sans la feature, `Local` n'est jamais rendu : le message actuel s'affiche, puis le menu revient.
  - Textes du parcours local, dans l'ordre :
    - `select("Whisper model:", …)` avec une option par entrée, `"{name:<20} {size:>7}  {note}"`, suivie de `  (downloaded)` quand le fichier existe à la bonne taille, puis l'option `Use a model file I already have`.
    - Modèle du catalogue :
      - s'il est déjà complet : `✔ <file> is already downloaded.` ;
      - sinon : `confirm("Download <file> (<size>) from Hugging Face to <dir>?", true)`.
    - Fichier existant : `text("Path to a ggml model file:", None, validator)`.
    - Ensuite : `confirm("Test the model on a 30 s sample? It runs locally and costs nothing.", true)` → `✔ Test passed: <n> words in <s:.1> s (language: <lang>).` ou `✘ The model could not be used: <e>`, puis `confirm("Keep this model anyway?", false)`.
  - `finish` est généralisé : il prend le texte de config déjà produit et le nom du fournisseur. La question de mise à jour et les lignes `Wrote …` et `Next: …` ne changent pas.
- [ ] **Step 4 : lancer les tests et les vérifications des deux configurations.** Attendu : tout passe.
- [ ] **Step 5 : commit** `feat: set up the local backend with a verified model download`.

---

## Validation réelle et clôture (après les tâches, par le contrôleur)

Aucune dépense d'API : la comparaison se fait avec le SRT OpenRouter du plan 3 (`/tmp/srtty-plan3/out/barbe_bleue.srt`, Grok). Les téléchargements sont annoncés avant d'être lancés. Config, cache et données sont isolés dans `/tmp/srtty-plan4` (`SRTTY_CONFIG`, `XDG_CACHE_HOME`, `XDG_DATA_HOME`). Binaire : `cargo build --release --features local`. Le tableau de bord passe par le harnais pty du plan 3 (`/tmp/srtty-plan3/pty_harness.py`, `python3` sans `-I`).

0. **Frontière du build** :
   - `cargo tree -i whisper-rs-sys` échoue ;
   - `cargo build --release` sans feature réussit ;
   - on relève la durée du build `--features local` à froid.
1. **`setup` local de bout en bout dans le pty** : téléchargement réel de `base` (148 Mo), avec la progression, puis un SHA-256 vérifié avec `sha256sum`. Le test de l'extrait doit passer, et la config écrite doit fonctionner.
2. **« La Barbe bleue » hors ligne** (664 s, 3 morceaux), `--format srt,vtt,txt`, dans le pty :
   - tableau de bord sans changement et récapitulatif `cost n/a` ;
   - le réseau est coupé, avec `unshare -rn` si l'espace de noms utilisateur est permis, sinon avec les variables de proxy.
3. **Jonctions (§6)** : on compare les mots autour des 2 coupes avec un run local en un seul morceau (`--chunk 10000`) et avec le SRT OpenRouter. Attendu : aucun mot perdu ni dupliqué.
4. **Timestamps (§6)** : 5 points de contrôle répartis dans le fichier, en comparant le début des sous-titres local et OpenRouter. Attendu : un écart sous 0,5 s. Sinon, on passe à DTW pour les modèles du catalogue (décision 4) et on le consigne en écart.
5. **Parallélisme** : temps réel et RSS maximal (`/usr/bin/time -v`) pour 1 × 16, 4 × 4 (défaut) et 2 × 8. On ajuste la décision 2 si la mesure la contredit.
6. **Ctrl-C** pendant la transcription, dans le pty : code 130 en moins d'1 s, curseur rendu, plus aucun thread whisper actif peu après (`top -H` ou `/proc/<pid>/task`).
7. **`--json` redirigé et `NO_COLOR`** : stdout ne contient que le JSON, et stderr ne contient aucune ligne `whisper_` ni `ggml_`.
8. **Optionnel, si Adrien l'accepte** : `large-v3-turbo-q5_0` (574 Mo) sur le même fichier, pour la qualité et les timestamps.

Ensuite :
- écrire `docs/validation/2026-10-31-plan4.md` ;
- mettre à jour `docs/HANDOFF.md` : état courant, historique, décisions, points reportés, et ce que la CI du plan 5 doit installer :
  - Linux : `cmake`, `clang` et `libclang-dev`, `build-essential` ;
  - macOS : Xcode CLT et `cmake` ;
  - un cache cargo, parce que whisper.cpp se recompile sinon ;
  - en option, `ggml-tiny.bin` (78 Mo, SHA-256 figé) en cache, pour lancer les tests de modèle avec `SRTTY_TEST_WHISPER_MODEL` ;
  - la vérification du build Metal sur le runner macOS, qui n'a pas pu être testé sur cette machine Linux ;
- ouvrir la PR en français (Résumé, Tests, Limites connues), la lier et la surveiller. Ne pas la merger.

## Décisions d'Adrien (2026-10-31)

Adrien a validé le plan avec les options recommandées :

1. **Modèle proposé en premier** : `large-v3-turbo-q5_0` (574 Mo). Le catalogue de 5 modèles multilingues est gardé.
2. **`--language` en local** : détection sur le premier morceau, puis langue figée (décision 13).
3. **Bloc `unsafe`** du callback d'abandon : accepté (décision 6).
4. **Téléchargements** : `tiny` (78 Mo) pour les tests de modèle et `base` (148 Mo) pour la validation. `large-v3-turbo-q5_0` (574 Mo) reste optionnel : à redemander au moment de la validation.

## Écarts décidés pendant l'exécution

Ces écarts ont été décidés pendant l'exécution, à partir des relectures par tâche, de la revue finale de la branche et de la validation réelle. Ils priment sur le texte des tâches ci-dessus.

- **Timestamps DTW** (décision 4, validation) :
  - Les timestamps de tokens de whisper.cpp sont jointifs et absorbent les pauses : sur « La Barbe bleue », un mot qui suit une pause de 0,7 s commençait 0,69 s trop tôt, au-delà du critère du §6.
  - On passe donc par DTW (`t_dtw`) quand le préréglage d'alignement se déduit du nom du fichier : `ggml-<nom>[-qN_N].bin` pour tiny, base, small, medium, large-v1/v2/v3 et large-v3-turbo, avec ou sans `.en`. Ça couvre tout le catalogue. Un autre fichier reprend les timestamps de tokens, sans message.
  - Un mot commence au `t_dtw` de son premier token, et garde la durée estimée par `t0`/`t1`.
  - Résultat : médiane d'écart de 0,07 s, 99 % des débuts sous 0,5 s, les 5 points de contrôle entre +0,04 et +0,12 s.
  - Coût : environ 10 à 16 % de temps en plus et 35 Mo par état.
- **Langue figée** (décision 13, revue finale) :
  - On ne fige qu'une détection sûre. Sans `--language`, une détection explicite (`lang_detect`) précède `full` ; la langue n'est passée à whisper et figée que si sa probabilité atteint 0,8. Sinon, le morceau part en `auto` et le suivant réessaie.
  - Quand une langue est figée, l'utilisateur le voit : avertissement `language detected as <code>; pass --language to force another one`, dans le flux d'événements et dans `warnings` du JSON.
- **Abandon** (décision 6) :
  - whisper.cpp 1.8.3 ne lit le drapeau qu'entre deux passes de l'encodeur ou du décodeur, jamais pendant le calcul ggml, le calcul du spectrogramme ou la détection de langue.
  - Le CPU est donc libéré environ 1 s après l'abandon avec tiny, plus tard avec un gros modèle. La sortie sur Ctrl-C reste immédiate (70 ms mesurées), puisque `main` sort par `process::exit`.
  - Le test d'abandon tolère 10 s.
- **Deux blocs `unsafe`** et non un seul : le corps du callback, et l'appel aux deux setters `unsafe fn` de whisper-rs qui l'enregistrent.
- **`check_language`** :
  - n'accepte que les codes courts de whisper (`get_lang_id` accepte aussi `french`) ;
  - refuse un octet NUL, sur lequel whisper-rs paniquait ;
  - installe les hooks de log, sans quoi whisper.cpp écrivait `unknown language` sur stderr.
- **Transcription** : on retire les octets NUL du prompt (panique de whisper-rs), et on ignore les segments sans texte, pour qu'un silence reste `NoSpeech`.
- **Téléchargement** :
  - `setup` supprime les `.srtty-*.part` restés dans le dossier des modèles avant de télécharger. Un kill, une fermeture du terminal ou une coupure de courant laissaient un fichier caché, jusqu'à 1,6 Go.
  - Le modèle téléchargé garde le mode 0600 du fichier temporaire.
- **Cache** : l'identité d'un modèle local est `<nom>#<taille>#<mtime>`, et non `<nom>#<taille>` (deux modèles affinés peuvent avoir le même nom et la même taille).
- **`setup` local** :
  - `LocalSetup` reçoit le répertoire home, pour que les tests n'écrivent pas dans le vrai home ;
  - un chemin relatif devient absolu avant d'être écrit ;
  - un test de l'extrait sans aucun mot compte comme un échec ;
  - un fichier présent mais abîmé est annoncé avant d'être retéléchargé ;
  - la vérification du hash affiche `Checking <file>…`, tourne dans `spawn_blocking` et reste interruptible par Ctrl-C ;
  - une erreur de lecture pendant cette vérification est signalée, et le fichier est traité comme incomplet.
- **Sans la feature** : un fournisseur local sans `model_path` donne aussi le message `the local backend is not available in this build; …`, au lieu de conseiller `srtty setup`.
- **`config show`** n'affiche plus `request_mode` pour un fournisseur local. L'aide de `--model` dit qu'il s'agit d'un chemin pour un fournisseur local.
- **Parallélisme** : la décision 2 est gardée, mesures à l'appui (validation). Le premier morceau, transcrit seul, n'utilise que `threads_per_job` threads. Une amélioration possible est reportée.
