# srtty — document de conception v1

Date : 2026-10-30 · Auteur : Adrien Gras (avec Claude)

Sources : [PRD](../../PRD.md) (copie de l'artefact claude.ai, rév. 12) et [rapport de spike](../../spikes/2026-10-30-openrouter-stt.md). Les réponses brutes du spike sont dans [docs/spikes/2026-10-30-data/](../../spikes/2026-10-30-data/).

Ce document prime sur le PRD quand ils divergent. Les écarts sont signalés par **(change le PRD)**.

## 1. Intention

srtty est une CLI Rust qui transforme un fichier vidéo ou audio local en sous-titres (SRT, VTT) ou en transcription lisible (txt). Elle passe par une API de transcription compatible OpenAI (OpenRouter d'abord, puis OpenAI, Groq ou une URL personnalisée) ou par un Whisper local.

Pourquoi elle existe : OpenRouter ne renvoie jamais de SRT ni de VTT. Seul `verbose_json` donne des timestamps, et pas avec tous les modèles. Aucun outil scriptable ne gère proprement le découpage, les timestamps et l'assemblage pour cet endpoint. Le spike a confirmé le problème, et l'a précisé : même avec `verbose_json`, la plupart des modèles renvoient des segments trop grossiers pour du sous-titrage.

Pour qui : des développeurs sous Linux et macOS, à l'aise en terminal, qui ont une clé d'API. Usages :
1. sous-titrer un talk ou une démo ;
2. produire un VTT pour un lecteur web ;
3. transcrire une réunion ou un call pour la relire, ou pour qu'un LLM en fasse un compte rendu ;
4. brancher srtty dans un script ou un hook.

## 2. Objectifs, non-objectifs, anti-objectifs

**Objectifs v1.** Tout ce qui suit est dans la première version publiée, sans découpage en v1.1.
- Un fichier en entrée, des sous-titres en sortie, en une commande.
- Fonctionner avec tout endpoint compatible OpenAI.
- `srtty setup` : configuration interactive, aucun modèle verrouillé.
- Une liste de compatibilité des modèles, tenue à jour chaque semaine par la CI et téléchargée à l'exécution.
- Un tableau de bord Ratatui en viewport inline pendant les runs interactifs.
- L'identification des intervenants (`--speakers`), en option.
- Le format `txt` pour la lecture et les LLM.
- Un backend Whisper local (feature Cargo `local`).
- Linux et macOS, installables par `cargo install`, `cargo binstall` et des binaires précompilés.

**Non-objectifs v1.** Traduction, traitement d'un dossier par lot, édition de sous-titres, incrustation dans la vidéo, Windows, interface graphique, nom des intervenants (on garde « Speaker N »), diarisation au-delà d'environ 1 h 15.

**Anti-objectifs.** srtty échoue s'il fait l'une de ces choses, même en marchant par ailleurs :
- dépenser les crédits de l'utilisateur sans le lui dire : pas de requête de test automatique, coût annoncé avant toute sonde, et arrêt dès le premier morceau si le modèle est inadapté ;
- afficher ou journaliser la clé d'API, où que ce soit ;
- écrire un fichier de sortie partiel ou faux en cas d'échec ;
- figer un modèle par défaut dans le binaire ;
- envoyer de la télémétrie. Les seuls appels réseau hors transcription sont la liste des modèles du fournisseur et `compat.json`, dans `setup` et `models` uniquement.

## 3. Contraintes (mesurées au spike)

| Contrainte | Conséquence |
| --- | --- |
| OpenRouter refuse `srt`, `vtt` et `text` ; 9 modèles sur 27 refusent `verbose_json` | srtty demande toujours `verbose_json` et assemble lui-même |
| 10 modèles sur 18 renvoient un seul segment pour 30 s ; le même modèle peut renvoyer 1 ou 7 segments selon le fournisseur ; 16 sur 18 renvoient des mots | **les sous-titres sont construits à partir des mots** |
| Le routage (`order`, `only`, `ignore`) ne s'applique pas à la transcription | la granularité dépend du fournisseur qui sert la requête : tolérer les deux cas |
| Upload multipart limité à 25 Mo ; timeout amont de 60 s | morceaux en Opus 32 kbit/s : 1,1 Mo pour 5 min, réponse en 4 à 15 s |
| Une coupe arbitraire perd des mots aux jonctions | couper au milieu d'un silence |
| Diarisation : chaque morceau renumérote ses voix | `--speakers` impose une seule requête : environ 1 h 15 au maximum (35 à 45 s de réponse pour 58 min) |
| `prompt` est accepté mais ignoré en multipart par OpenRouter | avertir l'utilisateur |
| Le champ `provider` (JSON) passe aussi en multipart | les options de fournisseur (diarisation) partent en multipart, sans base64 |
| ffmpeg est un prérequis système | appelé comme programme externe, jamais lié |

## 4. Approches envisagées

| | Approche | Pour | Contre |
| --- | --- | --- | --- |
| **A (retenue)** | Un seul crate lib + bin, en async (tokio, reqwest) | multipart, retries avec backoff, limite de parallélisme et Ctrl-C simples ; un seul crate à publier ; cœur testable sans réseau | compilation plus lourde |
| B | Un seul crate, synchrone (threads, channels, ureq) | moins de dépendances, modèle plus simple | multipart, backoff et annulation à écrire soi-même |
| C | Workspace `srtty-core` + `srtty` | frontières nettes, cœur réutilisable | plusieurs crates à publier et versionner ensemble ; personne n'a demandé de bibliothèque |

Adrien a choisi A. La robustesse réseau est le risque principal, et le découpage lib + bin suffit pour tester le cœur.

## 5. Ce qui a été décidé

### 5.1 Commandes

```
srtty <INPUT> [-o <OUT>] [--format srt,vtt,txt] [--provider <nom>] [--model <slug>]
      [--language <iso>] [--prompt <texte>] [--speakers] [--chunk <secondes>] [--jobs <n>]
      [--dry-run] [--json] [--quiet] [--no-cache]
srtty setup                      # configuration interactive
srtty models [--provider <nom>]  # modèles du fournisseur + statut de compatibilité
srtty config show                # config résolue, clé masquée
```

- `--format` accepte une liste séparée par des virgules, `srt` par défaut. **(change le PRD : `both` disparaît, `txt` arrive.)**
- `--words` disparaît : srtty demande toujours les mots. **(change le PRD : F13.)**
- `config init` est remplacé par `setup`. **(change le PRD.)**
- La sortie est écrite à côté de l'entrée, avec le même nom de base. `-o` fixe le chemin, sans extension si plusieurs formats sont demandés.
- Codes de retour : `0` succès, `1` usage ou config, `2` erreur réseau, API ou modèle inadapté, `3` ffmpeg, `130` interruption.

### 5.2 Pipeline

1. **Extraction** : ffmpeg sort de l'Opus 32 kbit/s, mono, 16 kHz (`-application voip`), dans un fichier temporaire.
2. **Découpage** : `silencedetect` (point de départ du spike : -30 dB, 0,4 s). Chaque coupe tombe au milieu du silence le plus proche de la cible, dans une fenêtre de ±30 s ; sans silence dans la fenêtre, srtty coupe à la cible. Cible par défaut : 300 s. Avec `--speakers`, pas de découpage.
3. **Transcription** : le **premier morceau est envoyé seul**. Si la réponse ne contient ni mots ni segments fins, srtty s'arrête (code 2) avec un message qui renvoie vers `srtty models`. Sinon, les autres morceaux partent en parallèle (`--jobs`, 4 par défaut), avec des retries et un backoff exponentiel sur les erreurs 429, 5xx et réseau. Chaque réponse est mise en cache.
4. **Assemblage** : les timestamps sont recalés sur l'offset de chaque morceau. On normalise les mots, puis on construit les sous-titres (5.4) et on filtre les répétitions et les sous-titres vides.
5. **Écriture** : chaque format demandé est d'abord écrit dans un fichier temporaire, puis renommé à sa place. En cas d'échec, aucun fichier final n'est créé.

« Segments fins » : la durée moyenne des segments est sous un seuil (proposition : 10 s). Un segment unique qui couvre tout le morceau n'est pas fin.

### 5.3 Le trait `Transcriber`

```rust
#[async_trait]
pub trait Transcriber: Send + Sync {
    async fn transcribe(&self, chunk: &AudioChunk, opts: &TranscribeOptions)
        -> Result<Transcript, TranscribeError>;
}
// Transcript { words: Vec<Word>, segments: Vec<Segment>, language: Option<String>, cost: Option<f64> }
// Word { start: f64, end: f64, text: String, speaker: Option<u32> }   — relatif au morceau
// Segment { start: f64, end: f64, text: String, speaker: Option<u32> }
```

Deux implémentations :
- `OpenAiCompatible` : multipart, champ `provider` en JSON quand des options de fournisseur sont nécessaires. `request_mode = "json-base64"` reste disponible dans la config, mais multipart est le défaut.
- `LocalWhisper` : whisper-rs, derrière la feature `local`.

`TranscribeError` distingue au minimum : modèle inadapté, erreur d'authentification, erreur temporaire (à retenter), erreur définitive.

### 5.4 Construire les sous-titres à partir des mots

- **Normalisation** : certains modèles Whisper renvoient des fragments (« c », « 'est »). Un fragment qui ne commence ni par une espace ni par une ponctuation ouvrante se recolle au mot précédent.
- **Découpe** : un nouveau sous-titre commence sur une ponctuation forte, une pause longue, un changement d'intervenant, ou quand on dépasse le nombre maximal de caractères par ligne (42), de lignes (2) ou la durée maximale (proposition : 7 s). Les lignes se coupent de préférence après une ponctuation faible.
- **Repli sur les segments** quand le morceau n'a pas de mots mais des segments fins : un segment trop long est découpé en répartissant le temps au prorata des caractères.
- **Format txt** : un paragraphe par tour de parole, `[HH:MM:SS] Speaker N: texte`. Sans `--speakers`, un paragraphe par pause longue, sans nom.
- **Intervenants** : `[Speaker N] texte` en SRT, `<v Speaker N>texte` en VTT. N commence à 1. Le libellé « Speaker » peut être remplacé dans la config (`subtitles.speaker_label`), par exemple par « Intervenant ». *(Choix de Claude.)*

### 5.5 Intervenants (`--speakers`)

- Désactivé par défaut. Une seule requête pour tout le fichier.
- **Avant tout envoi**, srtty refuse avec le code 1 si la durée dépasse la limite (proposition : 75 min) ou si `compat.json` ne marque pas le modèle comme compatible diarisation.
- L'option de fournisseur vient de `compat.json`, par exemple `{"xai": {"diarize": true}}` pour Grok et `{"deepgram": {"diarize": true}}` pour Deepgram.
- Le backend local ne gère pas `--speakers` : srtty renvoie une erreur claire.

### 5.6 Configuration

Fichier TOML, puis variables d'environnement, puis flags, dans cet ordre de priorité croissante. La clé est lue dans la variable d'environnement nommée par `api_key_env`, jamais stockée en clair.

```toml
default_provider = "openrouter"

[providers.openrouter]
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
model = "x-ai/grok-stt-1.0"          # écrit par setup, jamais codé en dur
request_mode = "multipart"

[providers.local]
kind = "local"
model_path = "~/.local/share/srtty/models/ggml-large-v3-turbo.bin"

[subtitles]
max_chars_per_line = 42
max_lines = 2
max_duration_s = 7.0

[chunking]
target_s = 300
jobs = 4
```

### 5.7 `srtty setup`

Prompts interactifs avec une crate de prompts (proposition : `inquire`), pas de Ratatui.
1. Choisir le fournisseur : OpenRouter, OpenAI, Groq, URL personnalisée ou Whisper local.
2. Pour une API : le nom de la variable d'environnement de la clé, avec vérification qu'elle existe sans afficher sa valeur. Ensuite, la liste des modèles de transcription, chacun avec son prix et son statut (« compatible », « compatible + intervenants », « incompatible », « inconnu »). Les incompatibles sont affichés mais pas sélectionnables par défaut.
3. Option : tester le modèle choisi sur un extrait embarqué d'environ 30 s, avec le coût estimé annoncé et une confirmation.
4. Écrire la config, sans écraser une config existante sans confirmation.

`srtty models` affiche la même liste, sans interaction. Si `--json` est passé, il la sort en JSON.

### 5.8 Liste de compatibilité (`compat.json`)

- Un fichier versionné dans le dépôt, téléchargé par `setup` et `models` depuis la branche `main` sur GitHub (raw), avec un timeout court (proposition : 3 s). Ces deux commandes **enregistrent la copie téléchargée dans le répertoire de cache**. Un run, `--speakers` et `--dry-run` lisent cette copie, ou à défaut la copie embarquée au build (`include_str!`). Un run ne fait donc aucun appel réseau en dehors de la transcription. Pour rafraîchir la liste, on relance `srtty models`. *(Choix de Claude.)*
- Pour chaque modèle et chaque fournisseur, une entrée qui donne : statut, présence de mots, segments fins, option de diarisation (ou rien), prix par heure, date de dernière vérification.
- **CI hebdomadaire** (GitHub Actions, déclenchement planifié plus manuel). Elle récupère la liste OpenRouter, puis sonde chaque modèle avec l'extrait de 30 s, en multipart, en demandant `verbose_json` et les mots. Elle classe le modèle, et teste la diarisation sur un court extrait à plusieurs voix pour les modèles qui ont des mots. Elle passe par une sous-commande cachée, `srtty compat probe`, pour utiliser le même code que les utilisateurs. Garde-fous : secret `OPENROUTER_API_KEY`, modèles au-delà d'un prix par seconde ignorés (proposition : 0,001 $/s), arrêt au-delà d'un budget par run (proposition : 0,50 $). Elle ne commite que si le contenu a changé.
- Les entrées OpenAI et Groq en direct sont tenues à la main (pas de clé en CI).

### 5.9 Affichage

- **TTY** : tableau de bord Ratatui en viewport inline. Adrien a validé la maquette B suivante :

```
$ srtty talk.mkv --format srt,vtt
┌ srtty · talk.mkv · x-ai/grok-stt-1.0 ──────────────────────────────┐
│ ✔ Extraction   ✔ Découpage   ⠹ Transcription   · Assemblage         │
└─────────────────────────────────────────────────────────────────────┘
┌ Morceaux ───────────────────────────────────────────────────────────┐
│ #1 00:00–07:42  ✔ 142 segments   4,1 s                              │
│ #2 07:42–15:30  ✔ 131 segments   3,8 s                              │
│ #3 15:30–23:05  ✔ 118 segments   5,2 s                              │
│ #4 23:05–30:51  ↻ retry 1/3      429, nouvelle tentative dans 2 s   │
│ #5 30:51–38:40  ✔ 127 segments   4,4 s                              │
│ #6 38:40–46:12  ⠹ en cours                                          │
│ #7 46:12–54:00  · en attente                                        │
│ #8 54:00–62:03  · en attente                                        │
└─────────────────────────────────────────────────────────────────────┘
┌ Progression ────────────────────────────────────────────────────────┐
│ ████ ████ ████ ▒▒▒▒ ████ ░░░░                                       │
│ 5/8 morceaux · 2,3× temps réel · 0,02 $ · écoulé 0:58               │
└─────────────────────────────────────────────────────────────────────┘
```

Une case par morceau dans la bande de progression, colorée selon le statut : terminé, retry, en cours, en attente. Avec `--speakers`, il n'y a qu'une ligne de morceau. À la fin, la zone s'efface et laisse un récapitulatif : fichiers écrits, durée audio, nombre de sous-titres, modèle, coût, durée. Au-delà de N morceaux visibles, la liste défile ou se résume.
- **Hors TTY, `--quiet` ou `NO_COLOR`** : une ligne de log sobre par événement sur stderr, sans code ANSI.
- **`--json`** : un seul objet JSON sur stdout (fichiers, durée, modèle, coût, nombre de sous-titres, avertissements), rien d'autre sur stdout.
- **Erreurs** : sous forme de diagnostic (miette) : la cause, le morceau concerné et une suggestion.

### 5.10 Cache

Une réponse par morceau dans le répertoire de cache. La clé combine l'empreinte du fichier d'entrée, le fournisseur, le modèle, la langue, les bornes du morceau et les options de fournisseur. Une relance après échec ne retranscrit que les morceaux manquants. `--no-cache` le désactive.

### 5.11 `--dry-run`

Il affiche le plan de découpage et une estimation du coût, sans aucun appel réseau. Le prix vient de `compat.json` (prix par heure, avec sa date). Si le modèle n'y figure pas, le coût est affiché comme inconnu. *(Choix de Claude : pas d'appel à l'API des modèles pendant un run.)*

**Confirmation au-delà d'un seuil** *(choix de Claude, pour tenir l'anti-objectif sur les dépenses)*. Avant d'envoyer quoi que ce soit, srtty estime le coût de la même manière. S'il dépasse `confirm_above_usd` (config, 1 $ par défaut), srtty demande une confirmation en TTY. Hors TTY, il s'arrête avec le code 1, sauf si `--yes` est passé. Au tarif mesuré, une heure coûte 0,10 à 0,36 $, donc l'usage courant ne voit jamais cette étape.

### 5.12 Distribution et plateformes

- Linux (x86_64, aarch64) et macOS (aarch64).
- `cargo install srtty` compile sans la feature `local` (pas besoin de toolchain C++). `cargo install srtty --features local` l'ajoute.
- Les releases sont produites par **dist** (ex-cargo-dist) : archives GitHub Releases compatibles `cargo binstall`, script d'installation, formule Homebrew.
- **Les binaires précompilés incluent la feature `local`** (décision d'Adrien). Sur macOS, ils sont compilés avec l'accélération Metal de whisper.cpp ; sur Linux, ils tournent en CPU. Linux aarch64 est compilé sur les runners ARM natifs de GitHub, pas en compilation croisée. Dans un binaire compilé sans `local` (par exemple via `cargo install` sans feature), `setup` affiche l'option « Whisper local » comme indisponible et donne la commande pour l'activer.
- `setup` propose de télécharger un modèle ggml de whisper.cpp (Hugging Face) dans le répertoire de données, ou d'indiquer le chemin d'un modèle existant. *(Choix de Claude.)*
- CI sur chaque PR : `cargo fmt --check`, `clippy -D warnings`, tests sous Linux et macOS, et un build avec `--features local`.
- Tests unitaires sans réseau sur les réponses enregistrées du spike : assemblage, normalisation des mots, recalage des offsets, formats, repli sur les segments, intervenants. Un test d'intégration contre la vraie API ne tourne que si une clé est présente.

### 5.13 Licence

Double licence MIT OR Apache-2.0, recommandée et présentée à Adrien. **(change le PRD : MIT seule.)**

### 5.14 Langue

**Tout est en anglais** (décision d'Adrien) : messages, prompts de `setup`, diagnostics, aide de la CLI, README, et libellé des intervenants dans les fichiers (« Speaker N »). Les exemples en français de ce document, comme la maquette, illustrent la mise en page, pas les textes définitifs. Pas d'i18n en v1.

### 5.15 Chemins

Sur Linux comme sur macOS, srtty suit les conventions XDG : config dans `$XDG_CONFIG_HOME/srtty/` (par défaut `~/.config/srtty/`), cache dans `~/.cache/srtty/`, données (modèles locaux) dans `~/.local/share/srtty/`. *(Choix de Claude : les développeurs sur Mac attendent `~/.config` pour une CLI, pas `~/Library/Application Support`.)*

## 6. Critères d'acceptation

*(Repris du PRD et ajustés par Claude.)* La v1 est livrable quand ces scénarios passent sous Linux et sous macOS.

- [ ] La pièce de Molière complète (58 min) passée par OpenRouter donne un SRT accepté par un parseur strict et par mpv, avec des timestamps strictement croissants.
- [ ] Sur 5 points de contrôle répartis dans le fichier, l'écart entre la parole et le début du sous-titre reste sous 0,5 s.
- [ ] Aucun mot n'est perdu ni dupliqué aux jonctions entre morceaux. On le vérifie sur « La Barbe bleue », qui perdait des mots avec une coupe fixe.
- [ ] Un modèle sans mots ni segments fins s'arrête dès le premier morceau, avec le code 2 et sans fichier écrit.
- [ ] `--speakers` sur la scène 10 avec Grok donne au moins 4 intervenants distincts. Au-delà de 75 min, ou avec un modèle non compatible, srtty refuse avant tout envoi.
- [ ] Une coupure réseau simulée déclenche des retries. Si l'échec persiste, une relance ne retranscrit que les morceaux manquants.
- [ ] Une redirection vers un fichier, `NO_COLOR` ou `--quiet` donne une sortie sans code ANSI ; `--json` produit un JSON valide et rien d'autre sur stdout.
- [ ] La clé n'apparaît dans aucune sortie, aucun log et aucun message d'erreur.
- [ ] `cargo build` compile sans toolchain C++ ; avec `--features local`, le même fichier se transcrit hors ligne.
- [ ] `setup`, de bout en bout, écrit une config utilisable avec OpenRouter.
- [ ] OpenAI et Groq en direct : testés à la main une fois avant la release si Adrien dispose des clés, sinon documentés comme « non testés ».

## 7. Prérequis côté dépôt (actions d'Adrien)

- Le nom `srtty` est libre sur crates.io au 2026-10-30, mais pas réservé : la première publication le réserve.
- Créer un dépôt pour le tap Homebrew (proposition : `AdrienGras/homebrew-tap`, qui n'existe pas encore).
- `main` n'est pas protégée : le workflow hebdomadaire peut y pousser `compat.json` directement.
- Les secrets GitHub : `OPENROUTER_API_KEY` pour la CI de compatibilité, `CARGO_REGISTRY_TOKEN` pour crates.io, et un token d'écriture sur le tap Homebrew.

## 8. Laissé au constructeur

- Les seuils exacts : pause longue, durée maximale et minimale d'un sous-titre, « segments fins », règle anti-hallucination. À calibrer sur les fixtures du spike et sur un audio réel bruité.
- Un seuil de silence relatif au volume mesuré plutôt que fixe à -30 dB (le média de test avait un volume moyen de -34 dB).
- Le nombre de retries, les délais de backoff et le timeout HTTP par requête.
- La disposition exacte du tableau de bord au-delà de la maquette, et le comportement quand le terminal est trop petit.
- La version minimale de ffmpeg, et la vérification de sa présence au démarrage (code 3 avec un message d'installation par OS).
- Les timestamps au mot pour le backend local (timestamps de tokens de whisper.cpp).
- Le schéma exact de `compat.json`, en respectant le contenu décrit en 5.8.
- L'extrait de 30 s embarqué, et l'extrait à plusieurs voix de la CI, tirés des enregistrements LibriVox du spike (domaine public).
