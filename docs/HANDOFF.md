# srtty — passation entre sessions

Journal antéchronologique : l'état courant en haut, l'historique en bas. Mettre ce fichier à jour à la fin de chaque plan, ou quand une décision change. Lire la section « État courant », puis seulement les entrées utiles.

## État courant (2026-10-31)

- Plans 1, 2 et 3 livrés et mergés (PR #1, #2, #3 ; `main` = `e3573c9`). 285 tests (262 unitaires, 23 de bout en bout). `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings` et `cargo test` sont propres.
- Travail en cours : **plan 4**, le backend local whisper-rs, sur `feat/plan-4-local-whisper` (plan : `docs/superpowers/plans/2026-10-31-srtty-4-local-whisper.md`).
- La CI hebdomadaire `compat` tourne : chaque lundi à 05:17 UTC, plus un déclenchement manuel. Premier run réussi le 2026-10-31 (0,063 $). Le secret `OPENROUTER_API_KEY` est configuré dans le dépôt.

## Documents de référence

| Document | Rôle |
| --- | --- |
| `docs/superpowers/specs/2026-10-30-srtty-design.md` | Conception validée. **Elle fait foi**, devant le PRD et les plans. |
| `docs/PRD.md` | PRD d'origine. La conception le corrige là où ils divergent. |
| `docs/superpowers/plans/2026-10-30-srtty-1-core.md` | Plan 1, exécuté. Son en-tête donne le découpage en 5 plans. |
| `docs/superpowers/plans/2026-10-30-srtty-2-models-setup.md` | Plan 2, exécuté. |
| `docs/superpowers/plans/2026-10-31-srtty-3-dashboard.md` | Plan 3, exécuté. Sa dernière section liste les écarts décidés pendant l'exécution. |
| `docs/spikes/2026-10-30-openrouter-stt.md` (+ `docs/spikes/2026-10-30-data/`) | Mesures du spike OpenRouter. |
| `docs/validation/2026-10-30-plan1.md`, `docs/validation/2026-10-30-plan2.md`, `docs/validation/2026-10-31-plan3.md` | Validations contre la vraie API. Celle du plan 3 inclut des captures du tableau de bord. |
| `assets/README.md` | Provenance des extraits audio embarqués (LibriVox, domaine public). |

Ces fichiers sont gros : les lire par plages (`grep -n '^#' <fichier>`, puis Read avec offset et limit).

## Façon de travailler d'Adrien

- Workflow superpowers. La conception est validée, donc chaque plan commence directement par `superpowers:writing-plans`. Le plan va dans `docs/superpowers/plans/<date>-srtty-<n>-<sujet>.md`.
- Il relit le plan avant l'exécution. Il commente parfois les choix ligne par ligne : intégrer ses retours, puis commiter le plan.
- Exécution en `superpowers:subagent-driven-development`, sur une branche de feature créée depuis `main`. Le plan se termine par une PR.
- **Ne jamais merger une PR soi-même.** Ouvrir la PR, la lier, la surveiller, et attendre qu'Adrien merge.
- Tout le texte destiné à l'utilisateur de la CLI est en anglais (messages, prompts, aide, README). Les docs du dépôt sont en français.
- Commits terminés par `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`. Descriptions de PR terminées par `🤖 Generated with [Claude Code](https://claude.com/claude-code)`. Les PR précédentes sont rédigées en français (Résumé, Tests, Limites connues).
- **Toute dépense d'API réelle (sondes, validation) : annoncer le coût estimé avant de la faire.** Les validations des plans 1 et 2 ont coûté 0,24 $ et 0,09 $.
- La clé OpenRouter est stockée dans `~/.config/secrets/openrouter_api_key` (mode 600, hors du dépôt). Pour l'utiliser : `OPENROUTER_API_KEY="$(cat ~/.config/secrets/openrouter_api_key)" <commande>`. Ne jamais l'afficher, ni la mettre dans un fichier du dépôt ou dans un argument de commande visible.

## Pièges connus de l'environnement

- Un hook `rtk` résume la sortie de certaines commandes (`cargo test`, `git diff`, `git show`). Pour voir la sortie brute : `rtk proxy cargo test`, `rtk proxy git diff …`.
- Sous zsh, `set -- $var` ne découpe pas les mots : utiliser des tableaux ou `${p%%:*}`.
- Médias de test (LibriVox) déjà téléchargés sur la machine : `/tmp/srtty-spike/media/barbe_bleue.mp3` et `/tmp/srtty-spike/media2/p_00.mp3` à `p_19.mp3` (scènes des Précieuses ridicules). Les URL d'origine sont dans `assets/README.md`.
- `compat.json` est réécrit chaque semaine par la CI. Les tests qui vérifient des données de modèles lisent le fixture figé `tests/fixtures/compat_seed.json` (`test_support::compat_seed()`), jamais la liste embarquée.
- Pour voir le tableau de bord sans vrai terminal : un pty émulé par `pyte` (installé dans le site utilisateur, donc **pas** `python3 -I`), qui répond aux requêtes `ESC [ 6 n`. Sans réponse, crossterm attend 2 s et srtty retombe sur les lignes simples. C'est ce qui se passe sous `script`. Le harnais du plan 3 est décrit dans `docs/validation/2026-10-31-plan3.md`.
- serde_json a la feature `float_roundtrip`, et les prix écrits dans `compat.json` sont arrondis à 6 décimales. Sans ça, la sonde réécrivait le fichier à chaque run.

## Architecture en bref

- Un seul crate lib + bin, en async (tokio, reqwest 0.13 rustls, clap 4.6). Rust 1.99, édition 2024.
- Pipeline : `src/run.rs` (extraction ffmpeg en Opus 32 kbit/s, découpe sur les silences, premier morceau seul puis le reste en parallèle, retries, cache par morceau, assemblage depuis les mots, écriture atomique).
- Affichage : trait `Reporter` (`src/report.rs`), qui reçoit des `Event` et a deux méthodes par défaut, `suspend()` (avant une question) et `finish()` (efface l'affichage vivant, appelée avant toute sortie finale). Deux implémentations :
  - `PlainReporter` : une ligne par événement sur stderr, sans ANSI ;
  - `Dashboard<B>` (`src/dashboard/`) : viewport inline Ratatui, avec `state.rs` (modèle pur), `view.rs` (disposition et dessin), `mod.rs` (runtime, ticker à 10 Hz, `DisplayMode`) et `recap.rs`.
- Choix du mode dans `app.rs` (`display_mode`). `--quiet` donne le silence. `--json`, stdout ou stderr hors TTY, `NO_COLOR` non vide ou `TERM=dumb` donnent `PlainReporter`. Sinon, le tableau de bord. Raison : crossterm écrit sa requête de position du curseur sur **stdout**. Si l'ouverture du viewport échoue, on retombe sur `PlainReporter`.
- Signaux : SIGINT et SIGTERM. Le premier annule proprement, le second force la sortie (130) après `finish()`. SIGHUP est volontairement ignoré, pour que `nohup` marche.
- Plan 2 :
  - `src/providers.rs` : liste des modèles d'un fournisseur.
  - `src/compat.rs` : `refresh` depuis GitHub raw vers le cache, `ModelStatus`.
  - `src/models.rs` : `srtty models`.
  - `src/probe.rs` : classement d'un modèle sur un extrait embarqué.
  - `src/compat_probe.rs` : `srtty compat probe`, commande cachée utilisée par la CI.
  - `src/setup/` : `srtty setup`, avec des prompts `inquire` derrière le trait `Prompter`, et l'écriture de la config avec `toml_edit`.
  - `src/fsutil.rs` : `write_atomic`.
- Codes de retour : `0` succès, `1` usage, config ou E/S, `2` réseau, API ou modèle inadapté, `3` ffmpeg, `130` interruption.
- `--json` : un seul objet JSON sur stdout, rien d'autre, y compris pour les erreurs.

## Plans restants

4. **Backend local whisper-rs** (feature `local`), timestamps au mot via whisper.cpp, téléchargement des modèles ggml dans `setup` (Hugging Face) vers `~/.local/share/srtty/models/`. Il remplace l'option « Local Whisper (not available in this build) » de `setup`.
5. **Distribution** : dist (ex-cargo-dist), `cargo binstall`, Homebrew (tap `AdrienGras/homebrew-tap`, à créer par Adrien), CI des PR sous Linux et macOS (fmt, clippy, tests, build `--features local`), licences MIT OR Apache-2.0, README en anglais.

## Décisions d'Adrien (2026-10-31)

1. **Seuil de diarisation de la sonde : gardé à 2 voix.** `elevenlabs/scribe-v2` reste « + intervenants ».
2. **Rétrogradation sur deux runs : adoptée (plan 3, Task 8).** Un modèle compatible dont la sonde concluante échoue passe « à l'épreuve » : il reste compatible, et `compat.json` garde `failed_probe { status, checked_at }`. La sonde concluante suivante le rétrograde si elle échoue aussi, ou efface la marque si elle réussit. Une sonde non concluante ne change rien. Les nouveaux modèles et les modèles déjà rétrogradés prennent le verdict tout de suite. La garde contre une rétrogradation massive compte les échecs du run (`ProbeReport::failed`). Le premier run de la CI après le merge sera le premier à pouvoir écrire ce champ.

Aucune décision ouverte pour l'instant.

## Points reportés

- Mineurs du plan 3 :
  - `wrapped_height` (`src/dashboard/mod.rs`) alloue un tampon proportionnel à la longueur de l'avertissement (quelques Mo pour un message géant) ;
  - avec `--json`, un second signal qui tombe pendant l'écriture du JSON d'erreur par `main` peut produire deux objets (fenêtre de quelques µs) ;
  - une entrée absente en mode tableau de bord ouvre brièvement le viewport avant l'erreur ;
  - « interrupted » peut s'imprimer deux fois si le second Ctrl-C tombe pendant la sortie normale ;
  - un relancement entièrement servi par le cache affiche `cost n/a` ;
  - pas de test qui prouve que l'effacement n'interroge pas le curseur (vérifié à la lecture et en pty) ;
  - en dessous d'environ 46 colonnes, la ligne compacte peut couper l'indication `stopping`.
- Plan 5 : vérifier que `compat.json` et `assets/*.ogg` sont dans le paquet crates.io (`cargo package --list`).
- Mineurs du plan 2 :
  - un lien symbolique cassé pour `config.toml` n'est détecté qu'à l'écriture ;
  - `endpoint_tags` n'échappe pas l'id du modèle dans l'URL ;
  - les colonnes de `srtty models` sont mesurées en octets (ids non ASCII) ;
  - `--quiet` ne coupe pas les lignes stderr de `models` ;
  - un fournisseur OpenAI sans `api_key_env` donne un 401, code 2 ;
  - le budget de la sonde peut être dépassé d'une requête (au plus 0,045 $) ;
  - les retries de la sonde ne sont pas comptés dans l'estimation ;
  - dans le workflow, l'artefact est téléchargé dans la racine du checkout (`runner.temp` serait plus sûr) ;
  - `require_key()` sur un fournisseur local rend une clé vide (à revoir au plan 4) ;
  - `parse_compat` lit le schéma après la désérialisation complète (le message serait moins clair pour un futur schéma 2).

## Historique

### 2026-10-31 : plan 3 exécuté

9 tâches. Chaque tâche a eu sa relecture, avec de petits correctifs demandés au fil de l'eau : alignement de la colonne de détails, gardes du runtime, indication `stopping` placée en tête de ligne. Puis une revue finale de toute la branche (2 points importants : requête de position du curseur pendant la question de coût, effacement qui dépendait de cette requête), suivie d'une vague de 8 correctifs. La relecture de cette vague a fait retirer SIGHUP, pour que `nohup` reste efficace. Décisions d'Adrien sur les deux questions ouvertes : voir plus haut.

Validation réelle dans un pseudo-terminal émulé (pyte), pour environ 0,021 $ : voir `docs/validation/2026-10-31-plan3.md`.

### 2026-10-31 : plan 2 mergé, CI compat en service

PR #2 mergée (`b269771`). Premier run du workflow : `c0e8e2d chore: refresh compat.json`, avec nemotron-3.5-transcribe qui passe « unusable ».

### 2026-10-30 : plan 2 exécuté

10 tâches, chacune relue, puis une revue finale de toute la branche suivie d'une vague de 13 corrections. Décisions prises pendant l'exécution :
- seules les erreurs HTTP 400, 415 et 422 rendent un modèle incompatible (un 402 « crédits épuisés » garde l'ancien statut) ;
- une liste OpenRouter vide fait échouer la sonde sans toucher au fichier ;
- garde contre une rétrogradation massive : la sonde refuse d'écrire si plus de la moitié des modèles compatibles sondés (au moins 4) tombent d'un coup ;
- les entrées gardées prennent le prix actuel de la liste ;
- le test payant de `setup` fait une seule tentative ;
- le workflow sépare la sonde (lecture seule, clé sur une seule étape) du commit (écriture, sans cargo), avec des actions épinglées par SHA.

Validation réelle : voir `docs/validation/2026-10-30-plan2.md`.

### 2026-10-30 : plan 1 exécuté et mergé

Cœur du pipeline. Validation réelle : voir `docs/validation/2026-10-30-plan1.md`.
