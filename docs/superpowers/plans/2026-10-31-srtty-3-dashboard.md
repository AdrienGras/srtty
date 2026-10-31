# srtty plan 3 : tableau de bord Ratatui — plan d'implémentation

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Afficher un run interactif dans un tableau de bord Ratatui en viewport inline (maquette B), derrière le trait `Reporter`, sans rien changer aux sorties hors TTY, `--quiet`, `NO_COLOR` et `--json`. Le plan traite aussi les deux points reportés : le second Ctrl-C avalé et le `.expect` sur `Client::builder`. Il ajoute enfin la décision d'Adrien du 2026-10-31 : `compat probe` ne rétrograde un modèle compatible qu'après deux sondes concluantes ratées de suite. Le seuil de diarisation reste à 2 voix.

**Architecture:** Un module `src/dashboard/` en quatre fichiers. `state.rs` replie les `Event` dans un modèle pur. `view.rs` calcule la disposition selon la taille du terminal et dessine une `Frame`, sans effet de bord. `mod.rs` porte le runtime : le `Reporter` qui pilote un `Terminal` inline, avec un thread de rafraîchissement à 10 Hz. `recap.rs` produit le récapitulatif final. Le runtime est générique sur le `Backend` de Ratatui : `CrosstermBackend<Stderr>` en production, `TestBackend` dans les tests. `app.rs` choisit le mode d'affichage, appelle `finish()` sur le reporter avant toute sortie finale (succès, dry run, erreur), et `suspend()` avant la question de confirmation du coût.

**Tech Stack:** Rust 1.99 (édition 2024), ratatui 0.30 (backend crossterm 0.29, déjà présent dans `Cargo.lock` via inquire), tokio, futures.

**Spec:** `docs/superpowers/specs/2026-10-30-srtty-design.md`, surtout §5.9 (affichage, maquette B), §5.14 (tout en anglais) et §2 (anti-objectifs).

## Global Constraints

- Tout texte affiché est en anglais (§5.14). La maquette de la conception est en français : elle fixe la mise en page, pas les libellés.
- Hors TTY, avec `--quiet` ou `NO_COLOR` : `PlainReporter`, une ligne par événement sur stderr, **aucun code ANSI** (§5.9).
- `--json` : un seul objet JSON sur stdout, rien d'autre (§5.9).
- `NO_COLOR` compte s'il est défini **et non vide** (convention no-color.org).
- Aucune nouvelle requête réseau (§2, anti-objectifs). Le tableau de bord n'affiche jamais la clé.
- Codes de retour inchangés : `0`, `1`, `2`, `3`, `130`.
- Dépendance : `ratatui = { version = "0.30", default-features = false, features = ["crossterm_0_29"] }`. Utiliser crossterm via `ratatui::crossterm`, sans dépendance directe. `cargo tree -i crossterm@0.28` doit échouer (une seule version de crossterm).
- Avant chaque commit : `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` (sortie brute : `rtk proxy cargo test`).
- Commits terminés par `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Décisions de conception propres à ce plan

1. **Le tableau de bord exige que stdout ET stderr soient des terminaux, et il est coupé avec `--json`.** Raison : crossterm 0.29 écrit la requête de position du curseur (`ESC [ 6 n`) sur **stdout** (`crossterm-0.29.0/src/cursor/sys/unix.rs`, `read_position_raw`), et le viewport inline de Ratatui l'utilise à la création, à l'effacement et au redimensionnement. Avec `srtty in.mkv --json | jq`, cette requête partirait dans le tube, sans réponse : JSON corrompu, puis deux secondes d'attente. Règle complète : `--quiet` → silencieux ; sinon `--json`, stdout ou stderr hors TTY, `NO_COLOR` non vide ou `TERM=dumb` → `PlainReporter` ; sinon tableau de bord. Si l'ouverture du viewport échoue (terminal qui ne répond pas à la requête), on retombe sur `PlainReporter`.
2. **Cinq étapes dans l'en-tête** (Extract, Split, Transcribe, Assemble, Write), et pas quatre comme la maquette : ce sont les cinq `Stage` émises par le pipeline. La ligne tient en 60 colonnes.
3. **Compteur de mots** dans les lignes de morceaux, pas de segments : `ChunkDone` porte `words`, et srtty construit les sous-titres à partir des mots (§5.4).
4. **Retries affichés sur 3** (`retry 1/3`) : `MAX_ATTEMPTS = 4` dans `run.rs`, donc trois nouvelles tentatives au plus. Le délai affiché est un compte à rebours.
5. **Liste longue** : au plus 8 lignes de morceaux (`MAX_CHUNK_ROWS`). Au-delà, la liste défile : la fenêtre commence un morceau avant le premier morceau non terminé, et sa dernière ligne résume ce qui est caché (`… 9 above, 4 below`). Avec `--jobs 4`, les morceaux en cours restent visibles.
6. **Bande de progression** : une case de 4 caractères par morceau (`████`, avec un espace entre deux cases) si tout tient sur une ligne. Sinon, une case d'un caractère sur 3 lignes au plus. Au-delà, une case regroupe plusieurs morceaux et prend le statut le moins avancé (retry > en cours > en attente > terminé). Chaque statut a son glyphe (`█` terminé, `▓` en cours, `▒` retry, `░` en attente) : la bande se lit sans les couleurs.
7. **Terminal trop petit** : on ne dessine jamais de cadre tronqué. Le cadre complet demande au moins 40 colonnes, et une hauteur suffisante pour l'en-tête, la bande de progression et au moins `min(n, 2)` lignes de morceaux, en gardant une ligne libre sous le viewport. Sinon, le viewport passe à **une seule ligne compacte** sans cadre (`⠋ Transcribe · 5/8 chunks · 4.0× realtime · $0.0200 · 1:10`), tronquée à la largeur. La disposition est recalculée à chaque image : si la hauteur voulue change (plan de découpage connu, terminal redimensionné), l'ancien viewport est effacé et un nouveau est ouvert au même endroit.
8. **Fin de run** : succès, dry run, erreur ou interruption, le viewport est toujours effacé et le curseur réaffiché avant toute autre sortie. En cas de succès hors `--json`, srtty affiche le récapitulatif (fichiers, durée audio, sous-titres, modèle, coût, durée). En cas d'erreur, le diagnostic miette suit, comme aujourd'hui.
9. **Avertissements** : en mode tableau de bord, chaque avertissement est imprimé au-dessus du viewport (`Terminal::insert_before`) et reste dans l'historique du terminal.
10. **Pas de mode raw** : Ctrl-C doit continuer à envoyer SIGINT. Conséquence connue : une touche frappée pendant le run s'affiche et peut décaler le viewport.

## Review Focus

1. **stdout redirigé alors que stderr est un terminal** (`srtty in.mkv --json | jq`, `srtty in.mkv > log`) : aucun octet d'échappement sur stdout, le JSON reste valide. Testé par `display_mode_rules` (Task 6).
2. **Terminal trop petit ou réduit pendant le run** : la ligne compacte, jamais un cadre tronqué. Testé par `layout_degrades_to_compact` (Task 3) et `viewport_height_follows_terminal_size` (Task 5).
3. **Beaucoup de morceaux** (10 h de vidéo, 120 morceaux et plus) : fenêtre qui défile, ligne de résumé, cases regroupées sans débordement. Testé par `visible_window_follows_first_unfinished` et `cell_geometry_cases` (Task 3).
4. **Compte à rebours de retry qui passe sous zéro** : `retrying`, jamais un délai négatif. Testé par `retry_countdown_never_goes_negative` (Task 4).
5. **Avertissement émis avant la première image ou pendant la suspension** (avertissement `--prompt` émis avant `prepare`, question de coût) : imprimé une fois, pas perdu. Testé par `warnings_queued_while_suspended_are_printed_once` (Task 5).

---

## Structure des fichiers

| Fichier | Rôle |
| --- | --- |
| `src/dashboard/mod.rs` (créé) | `DisplayMode`, `display_mode`, `Screen`, `stderr_screen`, `Dashboard<B>` (impl `Reporter`) |
| `src/dashboard/state.rs` (créé) | `DashboardState`, `StageStatus`, `ChunkStatus`, `ChunkRow` : le modèle replié des événements |
| `src/dashboard/view.rs` (créé) | `Layout`, `CellGeometry`, `plan_layout`, `cell_geometry`, `visible_window`, `render`, `fmt_clock`, `SPINNER` |
| `src/dashboard/recap.rs` (créé) | `recap_lines` |
| `src/report.rs` | `Stage::ALL`, `Stage::label`, `Reporter::suspend` et `Reporter::finish`, `Event::Cancelling` |
| `src/run.rs` | `pub const MAX_RETRIES`, `mm_ss` passe en `pub(crate)` |
| `src/app.rs` | choix du mode, `Outcome`, `finish()` avant toute sortie, `suspend()` avant la question, `on_interrupts` |
| `src/transcribe/openai.rs`, `src/setup/mod.rs`, `src/compat_probe.rs` | `OpenAiCompatible::new` renvoie un `Result` |
| `src/lib.rs`, `Cargo.toml` | `pub mod dashboard;`, dépendance ratatui |
| `tests/cli.rs` | e2e : sortie sans ANSI hors TTY |
| `src/compat.rs`, `src/compat_probe.rs` | `FailedProbe`, mise à l'épreuve avant rétrogradation |

---

### Task 1 : `OpenAiCompatible::new` renvoie une erreur au lieu de paniquer

**Files:**
- Modify: `src/transcribe/openai.rs:31-43` (et les constructions dans ses tests)
- Modify: `src/app.rs` (`transcribe`), `src/setup/mod.rs:49`, `src/compat_probe.rs:286` (`LiveProber::transcriber`)

**Interfaces:**
- Produces: `pub fn new(base_url: String, api_key: ApiKey, model: String, mode: RequestMode) -> Result<Self, TranscribeError>`. L'échec de `Client::builder().build()` devient `TranscribeError::Fatal { status: None, message: format!("cannot build the HTTP client: {e}") }`. Dans `app.rs`, il remonte par `?` en `SrttyError::Transcribe` (code 2). `LiveProber::transcriber` renvoie aussi `Result<OpenAiCompatible, TranscribeError>`, et ses appelants propagent l'erreur comme une sonde ratée.

- [ ] **Step 1 : écrire le test qui échoue.** Dans les tests de `openai.rs` : `fn new_builds_a_client()` qui appelle `OpenAiCompatible::new("http://127.0.0.1:1/v1".into(), ApiKey("sk-test".into()), "m".into(), RequestMode::Multipart)` et vérifie `.is_ok()`. L'échec du builder ne se provoque pas en test ; le test fixe la signature.
- [ ] **Step 2 : lancer `cargo test --lib transcribe::openai`.** Attendu : erreur de compilation (`is_ok` n'existe pas sur `OpenAiCompatible`).
- [ ] **Step 3 : changer la signature et mettre à jour les appelants** (`?` dans `app.rs` et `setup/mod.rs` ; `.unwrap()` dans les helpers de test `dead_transcriber` et `transcriber`).
- [ ] **Step 4 : lancer `cargo test`.** Attendu : tout passe. `grep -n 'expect("HTTP client' src` ne trouve plus rien.
- [ ] **Step 5 : commit** `fix: return an error instead of panicking when the HTTP client cannot be built`.

---

### Task 2 : le modèle du tableau de bord (`DashboardState`)

**Files:**
- Create: `src/dashboard/mod.rs` (pour l'instant `pub mod state;`), `src/dashboard/state.rs`
- Modify: `src/lib.rs` (`pub mod dashboard;`), `src/report.rs` (`Stage`), `src/run.rs` (`MAX_RETRIES`)

**Interfaces:**
- Produces, dans `report.rs` : `impl Stage { pub const ALL: [Stage; 5] = [Extract, Split, Transcribe, Assemble, Write]; pub fn label(&self) -> &'static str }`, avec les libellés `"Extract"`, `"Split"`, `"Transcribe"`, `"Assemble"`, `"Write"`.
- Produces, dans `run.rs` : `pub const MAX_RETRIES: u32 = MAX_ATTEMPTS - 1;`.
- Produces, dans `state.rs` :

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StageStatus { Pending, Running, Done }

#[derive(Debug, Clone, PartialEq)]
pub enum ChunkStatus {
    Pending,
    Running,
    /// `at`: time of the retry event since the dashboard started.
    Retry { attempt: u32, wait_s: f64, reason: String, at: Duration },
    Done { words: usize, secs: f64, cached: bool },
}

#[derive(Debug, Clone, PartialEq)]
pub struct ChunkRow { pub start_s: f64, pub end_s: f64, pub status: ChunkStatus }

pub struct DashboardState {
    pub input_name: String,
    pub model: String,
    pub chunks: Vec<ChunkRow>,
    /// Cost reported by chunks transcribed in this run (cached ones excluded), like `Summary::cost_usd`.
    pub cost_usd: Option<f64>,
    // private: stage statuses, transcribe start time, queued warnings
}

impl DashboardState {
    pub fn new(input_name: String, model: String) -> Self;
    /// Folds one event; `now` is the time since the dashboard started.
    pub fn apply(&mut self, e: &Event, now: Duration);
    pub fn stage(&self, stage: Stage) -> StageStatus;
    /// The running stage, else the last finished one, else `None`.
    pub fn current_stage(&self) -> Option<Stage>;
    pub fn done_count(&self) -> usize;
    pub fn first_unfinished(&self) -> Option<usize>;
    /// Audio seconds transcribed in this run (cached chunks excluded) per second
    /// since `StageStarted(Transcribe)`; `None` before the first such chunk.
    pub fn speed(&self, now: Duration) -> Option<f64>;
    /// Warnings received since the last call, in order.
    pub fn take_warnings(&mut self) -> Vec<String>;
}
```

Règles de `apply` : `StageStarted` → `Running` ; `StageDone` → `Done` ; `ChunksPlanned` remplace `chunks` (tous `Pending`) ; `ChunkStarted(i)` → `Running` ; `ChunkRetry` → `Retry { at: now, .. }` ; `ChunkDone` → `Done`, et ajoute `cost_usd` si `!cached` ; `Warning(m)` → file d'attente. Un index hors plan est ignoré, sans panique.

- [ ] **Step 1 : écrire les tests qui échouent** dans `state.rs` :

```rust
fn plan(n: usize) -> Event { Event::ChunksPlanned((0..n).map(|i| (i as f64 * 60.0, (i + 1) as f64 * 60.0)).collect()) }
fn done(index: usize, cost: f64, cached: bool) -> Event {
    Event::ChunkDone { index, words: 100, secs: 4.0, cost_usd: Some(cost), cached }
}
const fn s(secs: u64) -> Duration { Duration::from_secs(secs) }

#[test]
fn folds_stages_and_chunks() {
    let mut st = DashboardState::new("talk.mkv".into(), "m".into());
    st.apply(&Event::StageStarted(Stage::Extract), s(0));
    assert_eq!(st.stage(Stage::Extract), StageStatus::Running);
    assert_eq!(st.current_stage(), Some(Stage::Extract));
    st.apply(&Event::StageDone { stage: Stage::Extract, detail: String::new() }, s(1));
    assert_eq!(st.stage(Stage::Extract), StageStatus::Done);
    assert_eq!(st.stage(Stage::Split), StageStatus::Pending);
    st.apply(&plan(8), s(2));
    assert_eq!(st.chunks.len(), 8);
    assert_eq!(st.first_unfinished(), Some(0));
    st.apply(&Event::ChunkStarted(0), s(3));
    assert_eq!(st.chunks[0].status, ChunkStatus::Running);
    st.apply(&Event::ChunkRetry { index: 0, attempt: 1, wait_s: 2.0, reason: "HTTP 429".into() }, s(4));
    assert!(matches!(st.chunks[0].status, ChunkStatus::Retry { attempt: 1, at, .. } if at == s(4)));
    st.apply(&done(0, 0.005, false), s(5));
    assert_eq!(st.done_count(), 1);
    assert_eq!(st.first_unfinished(), Some(1));
    st.apply(&Event::ChunkStarted(99), s(6)); // out of plan: ignored
}

#[test]
fn cost_and_speed_ignore_cached_chunks() {
    let mut st = DashboardState::new("a".into(), "m".into());
    st.apply(&plan(8), s(0));
    st.apply(&Event::StageStarted(Stage::Transcribe), s(10));
    assert_eq!(st.speed(s(20)), None);
    for i in [0, 1, 2, 4] { st.apply(&done(i, 0.005, false), s(60)); }
    st.apply(&done(5, 0.5, true), s(60));
    assert!((st.cost_usd.unwrap() - 0.02).abs() < 1e-9);
    assert!((st.speed(s(70)).unwrap() - 4.0).abs() < 1e-9); // 240 s of audio in 60 s
    assert_eq!(st.done_count(), 5);
}

#[test]
fn warnings_are_taken_once_in_order() {
    let mut st = DashboardState::new("a".into(), "m".into());
    st.apply(&Event::Warning("one".into()), s(0));
    st.apply(&Event::Warning("two".into()), s(0));
    assert_eq!(st.take_warnings(), vec!["one".to_string(), "two".to_string()]);
    assert!(st.take_warnings().is_empty());
}
```

Et dans `report.rs` : `fn stage_labels()` qui vérifie `Stage::ALL.map(|s| s.label()) == ["Extract", "Split", "Transcribe", "Assemble", "Write"]`.

- [ ] **Step 2 : lancer `cargo test --lib dashboard::state report::`.** Attendu : erreur de compilation (`DashboardState` n'existe pas).
- [ ] **Step 3 : implémenter** `state.rs`, `Stage::ALL`, `Stage::label` et `MAX_RETRIES`.
- [ ] **Step 4 : lancer `cargo test --lib dashboard::state report::`.** Attendu : PASS.
- [ ] **Step 5 : commit** `feat: fold run events into a dashboard state`.

---

### Task 3 : la disposition selon la taille du terminal

**Files:**
- Create: `src/dashboard/view.rs` (géométrie seulement ; le dessin vient en Task 4)
- Modify: `src/dashboard/mod.rs` (`pub mod view;`), `Cargo.toml` (ratatui, voir Global Constraints)

**Interfaces:**
- Consumes: rien des tâches précédentes.
- Produces :

```rust
pub const MAX_CHUNK_ROWS: u16 = 8;
pub const MAX_CELL_ROWS: u16 = 3;
pub const MIN_WIDTH: u16 = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CellGeometry { pub width: u16, pub gap: u16, pub per_cell: usize, pub rows: u16 }

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Layout { Full { chunk_rows: u16, cells: CellGeometry }, Compact }

impl Layout { pub fn height(&self) -> u16 }
/// `inner_width`: columns inside the progress box (terminal width - 4).
pub fn cell_geometry(chunks: usize, inner_width: u16) -> CellGeometry;
pub fn plan_layout(chunks: usize, size: ratatui::layout::Size) -> Layout;
/// Chunks to list and whether the last row summarizes the hidden ones.
pub fn visible_window(chunks: usize, first_unfinished: Option<usize>, rows: u16) -> (std::ops::Range<usize>, bool);
```

Règles (voir les décisions 5 à 7) :
- `cell_geometry` : 0 morceau → `rows: 0`. Si `chunks * 5 - 1 <= inner_width` → `{width: 4, gap: 1, per_cell: 1, rows: 1}`. Sinon `rows = ceil(chunks / inner_width)` ; si `rows <= MAX_CELL_ROWS` → `{1, 0, 1, rows}` ; sinon `per_cell = ceil(chunks / (inner_width * MAX_CELL_ROWS))` et `rows = ceil(ceil(chunks / per_cell) / inner_width)`.
- `Layout::height` : `Compact` → 1 ; `Full` → `3 + (si chunks > 0 : 2 + chunk_rows) + 2 + cells.rows + 1`.
- `plan_layout` : `available = size.height - 1`. Si `size.width < MIN_WIDTH` → `Compact`. `cells = cell_geometry(chunks, size.width - 4)`. Partie fixe : `3 + 2 + cells.rows + 1`, plus 2 si `chunks > 0`. `chunk_rows = min(chunks, MAX_CHUNK_ROWS, available.saturating_sub(fixe))` (toutes les soustractions sont saturantes). `Full` seulement si `fixe <= available` et `chunk_rows >= min(chunks, 2)` ; sinon `Compact`.
- `visible_window` : si `chunks <= rows` → `(0..chunks, false)`. Sinon `shown = rows - 1`, `start = first_unfinished.unwrap_or(chunks).saturating_sub(1).min(chunks - shown)` → `(start..start + shown, true)`.

- [ ] **Step 1 : écrire les tests qui échouent** :

```rust
fn size(w: u16, h: u16) -> Size { Size::new(w, h) }

#[test]
fn cell_geometry_cases() {
    assert_eq!(cell_geometry(0, 76).rows, 0);
    assert_eq!(cell_geometry(8, 66), CellGeometry { width: 4, gap: 1, per_cell: 1, rows: 1 });
    assert_eq!(cell_geometry(20, 76), CellGeometry { width: 1, gap: 0, per_cell: 1, rows: 1 });
    assert_eq!(cell_geometry(200, 76), CellGeometry { width: 1, gap: 0, per_cell: 1, rows: 3 });
    assert_eq!(cell_geometry(500, 76), CellGeometry { width: 1, gap: 0, per_cell: 3, rows: 3 });
}

#[test]
fn full_layout_heights() {
    assert_eq!(plan_layout(0, size(80, 40)).height(), 6);   // before the chunk plan
    assert_eq!(plan_layout(1, size(80, 40)).height(), 10);  // --speakers: one chunk row
    assert_eq!(plan_layout(8, size(80, 40)).height(), 17);
    assert!(matches!(plan_layout(20, size(80, 40)), Layout::Full { chunk_rows: 8, .. }));
    assert!(matches!(plan_layout(20, size(80, 14)), Layout::Full { chunk_rows: 4, .. }));
}

#[test]
fn layout_degrades_to_compact() {
    assert_eq!(plan_layout(20, size(80, 10)), Layout::Compact);
    assert_eq!(plan_layout(8, size(39, 40)), Layout::Compact);
    assert_eq!(plan_layout(0, size(80, 5)), Layout::Compact);
    assert_eq!(Layout::Compact.height(), 1);
}

#[test]
fn visible_window_follows_first_unfinished() {
    assert_eq!(visible_window(8, Some(3), 8), (0..8, false));
    assert_eq!(visible_window(20, Some(10), 8), (9..16, true));
    assert_eq!(visible_window(20, Some(0), 8), (0..7, true));
    assert_eq!(visible_window(20, Some(19), 8), (13..20, true));
    assert_eq!(visible_window(20, None, 8), (13..20, true));
}
```

- [ ] **Step 2 : lancer `cargo test --lib dashboard::view`.** Attendu : erreur de compilation.
- [ ] **Step 3 : ajouter ratatui à `Cargo.toml` et implémenter les quatre fonctions.** Vérifier `cargo tree -i crossterm@0.28` → erreur « package ID specification did not match ».
- [ ] **Step 4 : lancer `cargo test --lib dashboard::view`.** Attendu : PASS.
- [ ] **Step 5 : commit** `feat: lay the dashboard out for the terminal size`.

---

### Task 4 : le dessin (`render`)

**Files:**
- Modify: `src/dashboard/view.rs`
- Modify: `src/run.rs` (`mm_ss` passe en `pub(crate)`)

**Interfaces:**
- Consumes: `DashboardState` et ses méthodes (Task 2), `Layout`, `visible_window`, `CellGeometry` (Task 3), `MAX_RETRIES`, `run::mm_ss`.
- Produces :

```rust
pub const SPINNER: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
/// "0:58", "1:12", "1:02:03" (seconds rounded down).
pub fn fmt_clock(secs: f64) -> String;
/// Draws the whole viewport; `now` is the time since the dashboard started.
pub fn render(state: &DashboardState, layout: &Layout, now: Duration, frame: &mut ratatui::Frame);
```

Textes exacts (le spinner prend l'image `(now.as_millis() / 100) % 10`) :
- Cadre 1, titre ` srtty · {input_name} · {model} ` ; une ligne d'étapes `{icône} {label}` séparées par trois espaces. Icônes : `✔` vert (terminée), spinner cyan (en cours), `·` gris foncé (en attente).
- Cadre 2 (seulement si `chunks > 0`), titre ` Chunks ` ; une ligne par morceau visible : `#{i:<w} {mm_ss(start)}–{mm_ss(end)}  {statut}`, où `w` est le nombre de chiffres de `chunks.len()`. Statuts :
  - en attente : `· pending` (gris foncé) ;
  - en cours : `{spinner} transcribing` (cyan) ;
  - retry : `↻ retry {attempt}/{MAX_RETRIES}  {reason}, retrying in {ceil(reste)} s` (jaune), ou `{reason}, retrying` quand le reste est ≤ 0 ;
  - terminé : `✔ {words} words` puis `{secs:.1} s`, ou `cached` (vert).
  Si `visible_window` demande un résumé, la dernière ligne est `… {start} above, {chunks - end} below` (gris foncé).
- Cadre 3, titre ` Progress ` ; les lignes de cases selon `CellGeometry` (glyphes de la décision 6, couleurs vert, cyan, jaune, gris foncé), puis la ligne de stats : `{done}/{n} chunks · {speed:.1}× realtime · ${cost:.4} · elapsed {fmt_clock}`. Un segment inconnu est omis (vitesse `None`, coût `None`). Avant le plan de découpage, la ligne ne contient que `elapsed …`.
- `Compact` : une ligne sans cadre, `{spinner} {current_stage label} · {done}/{n} chunks · {speed}× realtime · ${cost} · {fmt_clock}`, mêmes omissions, tronquée à la largeur.

- [ ] **Step 1 : écrire les tests qui échouent.** Un helper `fn draw(state: &DashboardState, w: u16, h: u16, now: Duration) -> Vec<String>` rend dans un `Terminal<TestBackend>` (`TestBackend::new(w, h)`, disposition `plan_layout(state.chunks.len(), Size::new(w, h + 1))`) et renvoie les lignes du buffer. L'état « maquette » : `plan(8)` (60 s par morceau), `StageDone` pour Extract et Split, `StageStarted(Transcribe)` à 10 s, morceaux 0, 1, 2 et 4 terminés (100 mots, 4,0 s, 0,005 $), morceau 5 en cache, morceau 3 en retry (`attempt 1`, `wait_s 2.0`, `"HTTP 429"`) à 69 s, morceau 6 en cours, morceau 7 en attente.

```rust
#[test]
fn renders_the_mockup_layout() {
    let lines = draw(&mockup_state(), 80, 17, s(70)).join("\n");
    assert!(lines.contains("┌ srtty · talk.mkv · x-ai/grok-stt-1.0 "));
    assert!(lines.contains("✔ Extract   ✔ Split   "));
    assert!(lines.contains("Transcribe   · Assemble   · Write"));
    assert!(lines.contains("┌ Chunks "));
    assert!(lines.contains("#1 00:00–01:00  ✔ 100 words"));
    assert!(lines.contains("4.0 s"));
    assert!(lines.contains("#4 03:00–04:00  ↻ retry 1/3"));
    assert!(lines.contains("HTTP 429, retrying in 1 s"));
    assert!(lines.contains("cached"));
    assert!(lines.contains("transcribing"));
    assert!(lines.contains("#8 07:00–08:00  · pending"));
    assert!(lines.contains("████ ████ ████ ▒▒▒▒ ████ ████ ▓▓▓▓ ░░░░"));
    assert!(lines.contains("5/8 chunks · 4.0× realtime · $0.0200 · elapsed 1:10"));
}

#[test]
fn retry_countdown_never_goes_negative() {
    let lines = draw(&mockup_state(), 80, 17, s(75)).join("\n");
    assert!(lines.contains("HTTP 429, retrying"));
    assert!(!lines.contains("retrying in"));
}

#[test]
fn speakers_run_has_one_chunk_row() {
    // plan(1) only: one "#1 " row and one cell
    let lines = draw(&state_with_plan(1), 80, 10, s(1));
    assert_eq!(lines.iter().filter(|l| l.contains("#1 ")).count(), 1);
    assert!(!lines.iter().any(|l| l.contains("#2 ")));
}

#[test]
fn long_lists_scroll_with_a_summary_row() {
    // plan(20), chunks 0..10 done, 10..12 running: window 9..16
    let lines = draw(&state_twenty(), 80, 17, s(1)).join("\n");
    assert!(lines.contains("#10 "));
    assert!(!lines.contains("#9 "));
    assert!(lines.contains("… 9 above, 4 below"));
}

#[test]
fn before_the_plan_only_elapsed_is_shown() {
    let lines = draw(&DashboardState::new("a.mp3".into(), "m".into()), 80, 6, s(3)).join("\n");
    assert!(lines.contains("elapsed 0:03"));
    assert!(!lines.contains("chunks ·"));
}

#[test]
fn compact_line() {
    let lines = draw(&mockup_state(), 80, 1, s(70));
    // plan_layout(8, 80x2) is Compact
    assert!(lines[0].contains("Transcribe · 5/8 chunks · 4.0× realtime · $0.0200 · 1:10"));
}

#[test]
fn clock_format() {
    assert_eq!(fmt_clock(3.9), "0:03");
    assert_eq!(fmt_clock(72.4), "1:12");
    assert_eq!(fmt_clock(3723.0), "1:02:03");
}
```

- [ ] **Step 2 : lancer `cargo test --lib dashboard::view`.** Attendu : erreur de compilation (`render`, `fmt_clock` absents).
- [ ] **Step 3 : implémenter `render`, `fmt_clock`, `SPINNER`**, avec `Block::bordered()`, `Paragraph` et des `Line`/`Span` stylés. Découper la zone avec `ratatui::layout::Layout::vertical` selon les hauteurs de la Task 3.
- [ ] **Step 4 : lancer `cargo test --lib dashboard::view`.** Attendu : PASS.
- [ ] **Step 5 : commit** `feat: draw the dashboard (stages, chunks, progress band, stats)`.

---

### Task 5 : le runtime `Dashboard<B>` derrière `Reporter`

**Files:**
- Modify: `src/dashboard/mod.rs`, `src/report.rs` (trait)

**Interfaces:**
- Consumes: `DashboardState` (Task 2), `plan_layout`, `Layout::height`, `render` (Tasks 3 et 4).
- Produces, dans `report.rs` (implémentations par défaut vides, `PlainReporter` ne change pas) :

```rust
pub trait Reporter: Send + Sync {
    fn event(&self, e: &Event);
    /// Hands the terminal back before an interactive prompt; the next event takes it again.
    fn suspend(&self) {}
    /// Erases any live display and restores the cursor. Called once a run ends,
    /// whatever its outcome, before anything else is printed.
    fn finish(&self) {}
}
```

- Produces, dans `dashboard/mod.rs` :

```rust
/// How the dashboard reaches the terminal; swapped for a `TestBackend` in tests.
pub struct Screen<B: Backend> {
    pub size: Box<dyn Fn() -> std::io::Result<Size> + Send>,
    /// Opens an inline viewport of `height` rows at the cursor.
    pub open: Box<dyn FnMut(u16) -> std::io::Result<Terminal<B>> + Send>,
}
pub fn stderr_screen() -> Screen<CrosstermBackend<std::io::Stderr>>;
pub const TICK: Duration = Duration::from_millis(100);

pub struct Dashboard<B: Backend + Send + 'static> { /* Mutex<Inner<B>>, Instant, ticker */ }
impl<B: Backend + Send + 'static> Dashboard<B> {
    /// Opens the first viewport (header and elapsed time); fails if the terminal cannot be opened.
    pub fn open(screen: Screen<B>, input_name: String, model: String) -> std::io::Result<Arc<Self>>;
    /// Redraws every `every` on a background thread until `finish`.
    pub fn start_ticker(self: &Arc<Self>, every: Duration);
    /// Draws one frame now.
    pub fn draw(&self);
}
impl<B: Backend + Send + 'static> Reporter for Dashboard<B> { /* event, suspend, finish */ }
```

Comportement :
- `draw` : rien si suspendu ou terminé. Calcule `plan_layout(state.chunks.len(), (screen.size)()?)`. Si aucun terminal n'est ouvert, ou si sa hauteur diffère de `layout.height()`, efface l'ancien viewport (voir « effacer » ci-dessous) et en ouvre un nouveau par `(screen.open)(height)`. Imprime ensuite `state.take_warnings()` au-dessus avec `insert_before(1, …)`, une ligne `warning: {message}` par avertissement, `warning:` en jaune. Enfin, `terminal.draw(|f| render(...))`. Toutes les erreurs de terminal sont ignorées : l'affichage est au mieux, il ne fait jamais échouer le run.
- « effacer » : `terminal.clear()`, puis `set_cursor_position` au coin haut gauche du viewport (`get_frame().area()`), puis `show_cursor()`.
- `event` : replie l'événement (`now = started.elapsed()`), lève la suspension, puis `draw`.
- `suspend` : efface et ferme le terminal (`None`), suspend. Le ticker ne dessine plus jusqu'au prochain événement.
- `finish` : arrête le ticker (canal + `join`) **avant** de prendre le verrou, sinon le ticker bloqué sur le verrou ne rend jamais la main ; imprime les avertissements en attente s'il reste un terminal, efface, marque terminé. Le terminal est gardé (les tests l'inspectent). La méthode est idempotente.
- `Drop` : si un terminal est encore ouvert et pas terminé, `show_cursor()`.
- `stderr_screen` : `size` = `ratatui::crossterm::terminal::size()` ; `open(h)` = `Terminal::with_options(CrosstermBackend::new(std::io::stderr()), TerminalOptions { viewport: Viewport::Inline(h) })`.

- [ ] **Step 1 : écrire les tests qui échouent** dans `dashboard/mod.rs`. Helper `fn test_screen(w: u16, h: u16) -> Screen<TestBackend>`, qui ouvre un `TestBackend::new(w, h)` neuf à chaque `open`. Accesseurs `#[cfg(test)]` sur `Dashboard` : `fn viewport_height(&self) -> Option<u16>` et `fn screen_text(&self) -> String` (buffer + scrollback du backend courant).

```rust
#[test]
fn viewport_grows_when_chunks_are_planned() {
    let d = Dashboard::open(test_screen(80, 40), "talk.mkv".into(), "m".into()).unwrap();
    assert_eq!(d.viewport_height(), Some(6));
    d.event(&Event::ChunksPlanned((0..8).map(|i| (i as f64, i as f64 + 1.0)).collect()));
    assert_eq!(d.viewport_height(), Some(17));
    assert!(d.screen_text().contains("srtty · talk.mkv · m"));
}

#[test]
fn viewport_height_follows_terminal_size() {
    let d = Dashboard::open(test_screen(80, 10), "a".into(), "m".into()).unwrap();
    d.event(&Event::ChunksPlanned((0..20).map(|i| (i as f64, i as f64 + 1.0)).collect()));
    assert_eq!(d.viewport_height(), Some(1)); // compact
}

#[test]
fn warnings_go_above_the_viewport() {
    let d = Dashboard::open(test_screen(80, 40), "a".into(), "m".into()).unwrap();
    d.event(&Event::Warning("no speech detected".into()));
    assert_eq!(d.screen_text().matches("warning: no speech detected").count(), 1);
}

#[test]
fn warnings_queued_while_suspended_are_printed_once() {
    let d = Dashboard::open(test_screen(80, 40), "a".into(), "m".into()).unwrap();
    d.suspend();
    assert_eq!(d.viewport_height(), None);
    d.draw();
    assert_eq!(d.viewport_height(), None); // stays released until the next event
    d.event(&Event::Warning("w1".into()));
    d.draw();
    assert_eq!(d.viewport_height(), Some(6));
    assert_eq!(d.screen_text().matches("warning: w1").count(), 1);
}

#[test]
fn finish_erases_the_viewport_and_is_idempotent() {
    let d = Dashboard::open(test_screen(80, 40), "talk.mkv".into(), "m".into()).unwrap();
    d.start_ticker(TICK);
    d.event(&Event::StageStarted(Stage::Extract));
    d.finish();
    d.finish();
    assert!(!d.screen_text().contains("srtty ·"));
    d.event(&Event::StageStarted(Stage::Split)); // after finish: no redraw
    assert!(!d.screen_text().contains("srtty ·"));
}
```

- [ ] **Step 2 : lancer `cargo test --lib dashboard::tests`.** Attendu : erreur de compilation.
- [ ] **Step 3 : implémenter** le trait (méthodes par défaut), `Screen`, `stderr_screen`, `Dashboard`.
- [ ] **Step 4 : lancer `cargo test --lib dashboard`.** Attendu : PASS, sans test instable (le ticker ne tourne que dans le dernier test).
- [ ] **Step 5 : commit** `feat: drive an inline Ratatui viewport behind the Reporter trait`.

---

### Task 6 : le choix du mode et le câblage dans `transcribe`

**Files:**
- Create: `src/dashboard/recap.rs`
- Modify: `src/dashboard/mod.rs` (`DisplayMode`, `display_mode`), `src/app.rs` (`transcribe`)
- Test: `tests/cli.rs`

**Interfaces:**
- Consumes: `Dashboard::open`, `start_ticker`, `stderr_screen`, `TICK` (Task 5), `fmt_clock` (Task 4), `Reporter::suspend`/`finish`.
- Produces :

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayMode { Quiet, Plain, Dashboard }
/// See design decision 1 of plan 3.
pub fn display_mode(quiet: bool, json: bool, stdout_tty: bool, stderr_tty: bool,
                    env: &dyn Fn(&str) -> Option<String>) -> DisplayMode;
/// recap.rs
pub fn recap_lines(s: &Summary) -> Vec<String>;
```

`recap_lines` produit exactement, pour un `Summary` avec `files = [talk.srt, talk.vtt]`, `audio_duration_s = 3723.0`, `cues = 1204`, `model = "x-ai/grok-stt-1.0"`, `cost_usd = Some(0.0213)` et `elapsed_s = 72.4` :

```
✔ Done in 1:12
  files    talk.srt, talk.vtt
  audio    1:02:03
  cues     1204
  model    x-ai/grok-stt-1.0
  cost     $0.0213
```

Avec `cost_usd = None`, la dernière ligne est `  cost     n/a`. À l'affichage, `✔` est en vert (`ratatui::crossterm::style::Stylize`).

Câblage de `transcribe` dans `app.rs` :
- Découper `transcribe` en deux. La fonction externe choisit le mode, construit le reporter, installe le gestionnaire de Ctrl-C, appelle la fonction interne, puis **appelle toujours `reporter.finish()`** avant d'imprimer quoi que ce soit ou de renvoyer l'erreur. La fonction interne renvoie `Result<Outcome, SrttyError>`, avec `enum Outcome { DryRun { plan: Vec<(f64, f64)>, estimate: Option<f64> }, Done(Summary) }`. Elle couvre ce qui va de `prepare` à `run`, plus `elapsed_s` et la fusion des avertissements.
- Le reporter : `Quiet` → `PlainReporter(sink)` ; `Plain` → `PlainReporter(stderr)` ; `Dashboard` → `Dashboard::open(stderr_screen(), <nom de fichier de l'entrée>, r.model.clone())` puis `start_ticker(TICK)`, ou `PlainReporter(stderr)` si `open` échoue.
- Sortie finale : `--json` → l'objet sur stdout (inchangé) ; `Dashboard` → `recap_lines` sur stderr ; `Plain` → `summary_line` (inchangé) ; `Quiet` → rien. Le dry run s'imprime comme aujourd'hui, après `finish()`.
- Question de coût : la closure `ask` appelle `reporter.suspend()` juste avant `ask_on_terminal`, donc seulement quand la question est vraiment posée.

- [ ] **Step 1 : écrire les tests qui échouent** :

```rust
#[test]
fn display_mode_rules() {
    let none = |_: &str| None;
    let env = |k: &'static str, v: &'static str| move |name: &str| (name == k).then(|| v.to_string());
    assert_eq!(display_mode(false, false, true, true, &none), DisplayMode::Dashboard);
    assert_eq!(display_mode(true, false, true, true, &none), DisplayMode::Quiet);
    assert_eq!(display_mode(true, true, false, false, &none), DisplayMode::Quiet);
    assert_eq!(display_mode(false, true, true, true, &none), DisplayMode::Plain);  // --json
    assert_eq!(display_mode(false, false, false, true, &none), DisplayMode::Plain); // stdout piped
    assert_eq!(display_mode(false, false, true, false, &none), DisplayMode::Plain); // stderr redirected
    assert_eq!(display_mode(false, false, true, true, &env("NO_COLOR", "1")), DisplayMode::Plain);
    assert_eq!(display_mode(false, false, true, true, &env("NO_COLOR", "")), DisplayMode::Dashboard);
    assert_eq!(display_mode(false, false, true, true, &env("TERM", "dumb")), DisplayMode::Plain);
}
```

Plus `fn recap_lists_files_audio_cues_model_cost()` et `fn recap_unknown_cost()`, avec les valeurs ci-dessus. Dans `tests/cli.rs` : `e2e_plain_progress_has_no_ansi`, sur le modèle de `e2e_writes_srt_and_json_summary` mais sans `--json` et avec `NO_COLOR=1`. Il vérifie le code 0, que stderr contient `transcribe: chunk 1/1 done` et `done: `, et qu'aucune des deux sorties ne contient `\x1b`.

- [ ] **Step 2 : lancer `cargo test --lib dashboard` et `cargo test --test cli e2e_plain_progress_has_no_ansi`.** Attendu : erreur de compilation, puis le test e2e passe déjà (non-régression) ou échoue sur `recap`.
- [ ] **Step 3 : implémenter** `display_mode`, `recap_lines` et le câblage de `transcribe`.
- [ ] **Step 4 : lancer `cargo test`.** Attendu : tous les tests passent, y compris les 22 e2e existants, qui tournent hors TTY donc en `Plain`.
- [ ] **Step 5 : commit** `feat: show the dashboard on interactive runs and a recap at the end`.

---

### Task 7 : un second Ctrl-C force la sortie

**Files:**
- Modify: `src/app.rs` (remplace `cancel_on_ctrl_c`), `src/report.rs` (`Event::Cancelling`, ligne simple), `src/dashboard/state.rs` et `view.rs` (indication)

**Interfaces:**
- Consumes: `Reporter::finish`, `DashboardState`, `render`.
- Produces :

```rust
// report.rs
Event::Cancelling,   // plain line: "interrupt: stopping; press Ctrl-C again to quit now"
// app.rs
/// Runs `first` on the first item of `signals` and `second` on the next one.
pub(crate) async fn on_interrupts<S>(signals: S, first: impl FnOnce(), second: impl FnOnce())
where S: futures::Stream<Item = ()> + Unpin;
```

Dans `transcribe`, le flux vient de `tokio::signal::unix::signal(SignalKind::interrupt())`, enveloppé par `futures::stream::unfold`. `first` = `cancel.cancel()` puis `reporter.event(&Event::Cancelling)`. `second` = `reporter.finish()`, `eprintln!("interrupted")`, `std::process::exit(130)`. Dans le tableau de bord, `Cancelling` ajoute ` · stopping (Ctrl-C again to quit)` à la fin de la ligne de stats et de la ligne compacte. Effet connu de la sortie forcée : les fichiers temporaires (audio extrait, `.tmp` d'une écriture en cours) peuvent rester. Le noter dans la doc de `on_interrupts`.

- [ ] **Step 1 : écrire les tests qui échouent** :

```rust
#[tokio::test]
async fn first_interrupt_cancels_second_forces() {
    let (tx, rx) = futures::channel::mpsc::unbounded::<()>();
    let (first, second) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicUsize::new(0)));
    let (f, s) = (first.clone(), second.clone());
    let task = tokio::spawn(on_interrupts(rx, move || { f.fetch_add(1, SeqCst); }, move || { s.fetch_add(1, SeqCst); }));
    tx.unbounded_send(()).unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert_eq!((first.load(SeqCst), second.load(SeqCst)), (1, 0));
    tx.unbounded_send(()).unwrap();
    task.await.unwrap();
    assert_eq!((first.load(SeqCst), second.load(SeqCst)), (1, 1));
}
```

Plus : dans le test de `PlainReporter`, un événement `Event::Cancelling` qui donne la ligne `interrupt: stopping; press Ctrl-C again to quit now` ; dans `view.rs`, `fn cancelling_hint_in_stats_line()` qui vérifie `stopping (Ctrl-C again to quit)` après `Cancelling`.

- [ ] **Step 2 : lancer `cargo test --lib app:: report:: dashboard::`.** Attendu : erreur de compilation.
- [ ] **Step 3 : implémenter** `Event::Cancelling`, son rendu, `on_interrupts` et le câblage.
- [ ] **Step 4 : lancer `cargo test`.** Attendu : PASS.
- [ ] **Step 5 : commit** `fix: force exit on a second Ctrl-C`.

---

### Task 8 : `compat probe` ne rétrograde qu'après deux échecs de suite

Décision d'Adrien (2026-10-31). Aujourd'hui, une seule sonde ratée fait passer un modèle compatible en `unusable` ou `incompatible`. Désormais, un premier échec le met **à l'épreuve** : il reste `compatible`, et `compat.json` garde la trace de l'échec. La rétrogradation n'a lieu qu'à la sonde **concluante** suivante, si elle échoue aussi. Une sonde non concluante (gardée pour budget, prix ou raison de `Verdict::Inconclusive`) ne change rien. La règle ne protège que les modèles déjà compatibles : un nouveau modèle, ou un modèle déjà rétrogradé, prend le verdict tout de suite. Le schéma reste à 1 : le nouveau champ est optionnel et omis quand il est vide, et serde ignore les champs inconnus.

**Files:**
- Modify: `src/compat.rs` (`CompatEntry`, `FailedProbe`), `src/compat_probe.rs` (`probe_all`, `ProbeReport`, `check_demotions`, `model_line`), et les littéraux `CompatEntry { … }` du crate (`failed_probe: None`)

**Interfaces:**
- Produces, dans `compat.rs` :

```rust
/// A compatible model's first failed conclusive probe; the next failure demotes it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FailedProbe { pub status: CompatStatus, pub checked_at: String }
// new field of CompatEntry:
#[serde(default, skip_serializing_if = "Option::is_none")]
pub failed_probe: Option<FailedProbe>,
```

- Produces, dans `compat_probe.rs` : `ProbeReport::failed: Vec<String>`, les modèles dont la sonde concluante de ce run a donné `Unusable` ou `Incompatible`, à l'épreuve ou non. `check_demotions` compte ses échecs dans `report.failed`, et non plus dans les statuts de la nouvelle liste : la garde contre une rétrogradation massive joue dès le premier échec, comme aujourd'hui.

Règles de `probe_all` pour un verdict `Unusable` ou `Incompatible` :
- ancienne entrée `Compatible` sans `failed_probe` → l'ancienne entrée, reprise au prix du jour, avec `checked_at: today` et `failed_probe: Some(FailedProbe { status: <verdict>, checked_at: today })` ; `words`, `fine_segments` et `diarization` sont conservés, et la sonde de diarisation n'est pas lancée ;
- ancienne entrée `Compatible` avec `failed_probe` → rétrogradation comme aujourd'hui (`status` du verdict, `words` et `fine_segments` à `false`, `diarization: None`), avec `failed_probe: None` ;
- pas d'ancienne entrée, ou ancienne entrée non compatible → comportement actuel, avec `failed_probe: None`.

Un verdict `Usable` écrit `failed_probe: None`. `Inconclusive` garde l'ancienne entrée telle quelle, `failed_probe` compris. Dans la sortie de la commande, `model_line` affiche `compatible, on probation ({unusable|incompatible})` pour un modèle mis à l'épreuve.

- [ ] **Step 1 : écrire les tests qui échouent**, avec les helpers existants (`FakeProber`, `entry`, `listed`, `policy`, `list`) :
  - `first_failure_puts_a_compatible_model_on_probation` : ancienne entrée `a/x` `Compatible`, avec `words: true` et `diarization: Some(json!({"xai": {"diarize": true}}))` ; la sonde dit `Unusable`. La nouvelle entrée est `Compatible`, avec `words: true`, la même `diarization`, `checked_at == Some("2030-05-06")` et `failed_probe == Some(FailedProbe { status: Unusable, checked_at: "2030-05-06".into() })`. On vérifie aussi `report.failed == ["a/x"]` et que `prober.calls()` ne contient aucun `diarize`.
  - `second_failure_in_a_row_demotes` : ancienne entrée `Compatible` avec `failed_probe: Some(FailedProbe { status: Incompatible, .. })` ; la sonde dit `Unusable`. On attend `status == Unusable`, `failed_probe == None`, `words == false` et `diarization == None`.
  - `usable_probe_clears_probation` : ancienne entrée à l'épreuve, sonde `usable(true, true, None)` → `failed_probe == None`.
  - `inconclusive_probe_keeps_probation` : ancienne entrée à l'épreuve, `Verdict::Inconclusive("timeout".into())` → l'entrée vaut `repriced(&old, …)`, `failed_probe` compris.
  - `only_compatible_models_get_probation` : sans ancienne entrée, `Unusable` → `Unusable` tout de suite ; ancienne entrée `Unusable`, puis `Incompatible` → `Incompatible` tout de suite.
  - `mass_guard_counts_first_failures` : 4 anciens `Compatible`, 3 sondes `Unusable` → `check_demotions` renvoie `Err`, alors qu'aucun statut n'a changé dans la nouvelle liste.
  - dans `compat.rs`, `failed_probe_is_optional_in_the_file` : une entrée JSON sans le champ se lit avec `None`, et une entrée `None` se sérialise sans la clé `failed_probe`.
  Adapter les tests existants qui attendaient une rétrogradation immédiate d'un modèle compatible : leur donner une ancienne entrée déjà à l'épreuve, ou attendre la mise à l'épreuve. Les citer dans le rapport de la tâche.
- [ ] **Step 2 : lancer `cargo test --lib compat`.** Attendu : erreur de compilation (`FailedProbe` absent).
- [ ] **Step 3 : implémenter** le champ, `ProbeReport::failed`, les règles de `probe_all`, `check_demotions` et `model_line`.
- [ ] **Step 4 : lancer `cargo test`.** Attendu : PASS. `compat.json` et `tests/fixtures/compat_seed.json` ne changent pas (`git status`).
- [ ] **Step 5 : commit** `feat: demote a compatible model only after two failed probes in a row`.

---

### Task 9 : validation réelle dans un pseudo-terminal, puis documentation

Tâche du contrôleur, pas d'un sous-agent : elle dépense des crédits. **Coût estimé : au plus 0,04 $** (Grok STT à 0,10 $/h : « La Barbe bleue », 664 s, en morceaux de 60 s ≈ 0,018 $ ; la scène 10, 43 s, avec `--speakers` ≈ 0,001 $ ; un run interrompu ≤ 0,018 $ ; les relances depuis le cache sont gratuites). À annoncer avant de lancer.

**Files:**
- Create: `docs/validation/2026-10-31-plan3.md`
- Modify: `docs/HANDOFF.md`
- Outil hors dépôt : `/tmp/srtty-plan3/pty_harness.py`

- [ ] **Step 1 : écrire le harnais.** `python3 -I /tmp/srtty-plan3/pty_harness.py --rows R --cols C [--interrupt-at S [S2]] -- srtty …` lance la commande dans un pty (`pty.fork`, taille fixée par `TIOCSWINSZ`) et lit la sortie dans un `pyte.Screen`. Il répond aux requêtes de position du curseur en surchargeant `write_process_input`, qui renvoie la réponse sur le maître. Il envoie `\x03` aux instants demandés, et sauve une capture de l'écran par seconde, ainsi que la capture finale et les octets bruts.
- [ ] **Step 2 : scénarios**, tous avec `OPENROUTER_API_KEY="$(cat ~/.config/secrets/openrouter_api_key)"`, une config temporaire (`SRTTY_CONFIG`) et un cache temporaire (`XDG_CACHE_HOME`) :
  1. 30×100, `barbe_bleue.mp3 --chunk 60 --format srt,vtt` : captures pendant le run (en-tête, morceaux, bande, stats), puis capture finale (viewport effacé, récapitulatif) ;
  2. même commande relancée : morceaux `cached`, pas de vitesse ;
  3. 12×60 puis 8×60 : liste qui défile, puis ligne compacte ;
  4. `p_09.mp3 --speakers` : une seule ligne de morceau ;
  5. `--no-cache`, Ctrl-C à 3 s puis à 4 s : indication `stopping`, sortie 130, curseur réaffiché, aucun fichier final ;
  6. `--json | cat` et `NO_COLOR=1` : aucun `\x1b` dans les octets bruts de stdout, et le mode simple sur stderr.
- [ ] **Step 3 : écrire `docs/validation/2026-10-31-plan3.md`** : les captures utiles, le coût réel, les écarts constatés.
- [ ] **Step 4 : mettre à jour `docs/HANDOFF.md`** : état courant, historique, décisions de ce plan, points reportés (plan 4 : backend local whisper-rs). Retirer les deux décisions ouvertes : seuil de diarisation gardé à 2 voix, et rétrogradation après deux échecs (Task 8).
- [ ] **Step 5 : commit** `docs: plan 3 validation and handoff`, puis une PR en français (Résumé, Tests, Limites connues), liée et surveillée. Ne pas merger.

---

## Écarts décidés pendant l'exécution

Ces écarts ont été décidés pendant l'exécution, à partir des relectures par tâche, de la revue finale de la branche et de la validation réelle. Ils priment sur le texte des tâches ci-dessus.

- **`MIN_WIDTH` = 53**, et non 40 (décision 7). C'est la plus petite largeur où la ligne d'étapes tient entière avec des séparateurs d'un espace. La ligne utilise trois espaces quand la largeur le permet, un seul sinon.
- **Colonne de détails alignée** dans la liste des morceaux, sur la plus longue étiquette des lignes qui ont un détail (terminées ou en retry).
- **Ligne de résumé de la liste** : le côté vide est omis (`… 4 below`, pas `… 0 above, 4 below`).
- **Indication `stopping (Ctrl-C again to quit)`** en tête de la ligne de stats et de la ligne compacte, en jaune, et non en fin de ligne : en fin de ligne, elle était coupée à 80 colonnes.
- **Runtime** :
  - `suspend` et `start_ticker` ne font rien après `finish` ;
  - une réouverture ratée du viewport éteint le tableau de bord pour le reste du run, au lieu de réinterroger le terminal à chaque image ;
  - les avertissements longs passent à la ligne ;
  - `Cancelling` ne rouvre pas un tableau de bord suspendu (question de coût en cours) ;
  - l'effacement ne passe plus par `Terminal::clear()`, qui interroge la position du curseur : curseur ramené à l'origine du viewport, `ClearType::AfterCursor`, puis curseur réaffiché.
- **Signaux** : SIGTERM est traité comme Ctrl-C (premier signal : arrêt propre ; second : sortie forcée), pour que `timeout` rende le curseur. SIGHUP n'est pas écouté, pour que `nohup` continue de protéger les longs runs. Le flux de signaux est créé avant le reporter.
- **Sortie forcée avec `--json`** : `{"error":"interrupted","exit_code":130}` sur stdout.
