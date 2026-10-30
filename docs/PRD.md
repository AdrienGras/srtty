# PRD — srtty, CLI de transcription vidéo → SRT

2026-10-30 · Adrien

> Copie locale de l'artefact claude.ai : https://claude.ai/artifact/T8x326dFSMz2soYbCUXLQg (rév. 12).

srtty est une CLI Rust, sous licence MIT, qui transforme un fichier vidéo ou audio local en sous-titres SRT ou VTT. Elle passe par une API de transcription compatible OpenAI (OpenRouter en premier lieu) ou, en option, par un Whisper local. Le nom est provisoire.

## Contexte et problème

OpenRouter expose un endpoint de transcription `/api/v1/audio/transcriptions`, qui accepte aussi les requêtes multipart au format OpenAI. Mais il ne renvoie jamais de SRT ou de VTT : les formats `text`, `srt` et `vtt` sont refusés avec une erreur 400. Seul `verbose_json` donne des segments horodatés, et pas chez tous les fournisseurs ([doc OpenRouter STT](https://openrouter.ai/docs/guides/overview/multimodal/stt)).

Les outils existants ne couvrent pas proprement ce cas. Buzz accepte une base URL personnalisée, mais rien ne garantit qu'il envoie le slug de modèle complet (`openai/whisper-1`) ni qu'il passe par `verbose_json`. Ce montage n'a pas été testé. Il manque un outil scriptable, pensé pour Linux, qui gère lui-même le découpage, les timestamps et l'assemblage du SRT, avec une sortie terminal soignée.

## Objectifs et non-objectifs

La v1 fait une seule chose bien : une vidéo en entrée, un fichier de sous-titres horodatés en sortie, en une commande.

Objectifs v1 :

- Transcrire un fichier unique vers SRT, VTT ou les deux, en une commande.
- Fonctionner avec tout endpoint compatible OpenAI : OpenRouter, OpenAI, Groq, ou un serveur auto-hébergé.
- Proposer un backend Whisper local en option, derrière une feature Cargo, sans alourdir le build par défaut.
- Découper l'audio automatiquement pour rester sous les limites des API, puis recoller les timestamps.
- Afficher une progression lisible et un récapitulatif final avec la durée, le nombre de segments et le coût quand l'API le renvoie.
- Rester scriptable : sortie sobre hors TTY, mode JSON, codes de retour fiables.

Hors périmètre v1 :

- Traduction des sous-titres (prévue en v2).
- Traitement par lot d'un dossier (prévu en v2).
- TUI plein écran, interface graphique, édition de sous-titres.
- Incrustation des sous-titres dans la vidéo, diarisation.

## Utilisateurs et cas d'usage

La cible : des développeurs sous Linux, à l'aise en terminal, qui ont déjà une clé d'API et veulent des sous-titres sans passer par une interface graphique.

1. Sous-titrer l'enregistrement d'un talk ou d'une démo avant de le publier.
2. Produire un VTT pour un lecteur web.
3. Transcrire une réunion enregistrée en vidéo pour la relire ou la chercher.
4. Brancher srtty dans un script ou un hook (sortie JSON, codes de retour), par exemple après un export vidéo.

## Périmètre fonctionnel v1

Les exigences « Doit » conditionnent la sortie de la v1. Les « Devrait » sont souhaitées, mais peuvent glisser en v1.1.

| ID | Exigence | Priorité |
| --- | --- | --- |
| F1 | Accepter en entrée tout fichier audio ou vidéo lisible par ffmpeg (binaire système, version minimale à fixer). | Doit |
| F2 | Extraire la piste audio en mono, dans un format compressé (MP3 ou Opus) pour rester sous le plafond multipart de 25 Mo. | Doit |
| F3 | Découper l'audio en morceaux d'une durée cible configurable, en coupant de préférence sur un silence proche de la cible (`silencedetect` de ffmpeg). | Doit |
| F4 | Transcrire chaque morceau via le backend choisi, avec une concurrence configurable et des retries avec backoff sur les erreurs 429 et 5xx. | Doit |
| F5 | Demander `verbose_json` avec des segments horodatés. Si le modèle ou le fournisseur le refuse, échouer avec un message qui le dit explicitement et propose d'autres modèles. | Doit |
| F6 | Recaler les timestamps de chaque morceau sur son offset, renuméroter, puis écrire du SRT, du VTT ou les deux. | Doit |
| F7 | Couper les segments trop longs pour l'affichage : nombre maximal de caractères par ligne, deux lignes au plus, durée maximale par sous-titre. Valeurs par défaut configurables. | Doit |
| F8 | Options `--language` (ISO-639-1, détection automatique sinon) et `--prompt` (vocabulaire attendu). | Doit |
| F9 | Lire la configuration depuis un fichier TOML, des variables d'environnement et des flags, dans cet ordre de priorité croissante. La clé d'API n'apparaît jamais dans les logs ni dans la sortie. | Doit |
| F10 | Afficher la durée d'audio traitée et le coût cumulé quand l'API renvoie `usage.cost`. | Doit |
| F11 | Backend Whisper local derrière la feature Cargo `local`, avec un modèle fourni par l'utilisateur. | Devrait |
| F12 | Cache des morceaux déjà transcrits, indexé par l'empreinte du fichier et les paramètres, pour reprendre après un échec sans repayer. | Devrait |
| F13 | Option `--words` pour demander des timestamps au mot, quand le fournisseur les renvoie. | Devrait |
| F14 | `--dry-run` : affiche le découpage prévu et, si le tarif du modèle est connu, une estimation du coût, sans appel de transcription. | Devrait |

## Interface CLI et sorties

srtty est une commande principale, accompagnée de deux sous-commandes utilitaires. Ce n'est pas une TUI : on lance la commande, on suit la progression, on obtient un fichier.

```
srtty <INPUT> [-o <OUT>] [--format srt|vtt|both]
      [--provider <nom>] [--model <slug>] [--language <iso>] [--prompt <texte>]
      [--chunk <secondes>] [--jobs <n>] [--words] [--dry-run]
      [--json] [--quiet] [--no-cache]

srtty config init | show      # crée / affiche la config résolue (clé masquée)
srtty models [--provider <nom>]  # liste les modèles de transcription du fournisseur
```

Par défaut, la sortie s'écrit à côté de l'entrée, sous le même nom de base (`talk.mkv` → `talk.srt`). `srtty models` interroge `/api/v1/models?output_modalities=transcription` chez OpenRouter. Pour les autres fournisseurs, il se rabat sur une liste déclarée dans la config.

La configuration nomme plusieurs fournisseurs et en désigne un par défaut :

```toml
# ~/.config/srtty/config.toml
default_provider = "openrouter"

[providers.openrouter]
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
model = "openai/whisper-large-v3"
request_mode = "multipart"   # ou "json-base64"

[providers.groq]
base_url = "https://api.groq.com/openai/v1"
api_key_env = "GROQ_API_KEY"
model = "whisper-large-v3"

[subtitles]
max_chars_per_line = 42
max_lines = 2
```

La config pointe vers une variable d'environnement plutôt que de stocker la clé en clair. Les valeurs de sous-titres ci-dessus sont des conventions courantes, à ajuster après essai. L'URL Groq est donnée de mémoire et reste à vérifier.

Rendu en terminal :

- Une ligne par étape (extraction, découpage, transcription, assemblage), avec spinner, puis coche ou croix.
- Pendant la transcription, une barre `n/N morceaux`, le débit et le coût cumulé.
- En fin de run, un récapitulatif : fichier écrit, durée audio, nombre de sous-titres, modèle, coût, temps écoulé.
- Les erreurs sont présentées comme un diagnostic, avec la cause, le morceau concerné et une suggestion (« ce modèle ne renvoie pas de timestamps, essayez `srtty models` »).
- Hors TTY, ou avec `--quiet` ou `NO_COLOR`, aucune animation ni code ANSI. Avec `--json`, un objet JSON unique en sortie standard.

Codes de retour : `0` succès, `1` erreur d'usage ou de config, `2` erreur réseau ou API après épuisement des retries, `3` erreur ffmpeg.

## Architecture technique

srtty est un pipeline linéaire. La seule abstraction porte sur la transcription : un trait `Transcriber` que chaque backend implémente, pour qu'ajouter un fournisseur ne touche ni au découpage ni à l'assemblage.

Schéma « pipeline srtty · 6 étapes, 2 backends » (transcrit du diagramme de l'artefact) :

```
Fichier média ──▶ Extraction audio ──▶ Découpage
(vidéo ou audio,   (mono, MP3 ou Opus)  (coupe sur un silence)
 via ffmpeg)                                 │
                                             ▼
Écriture ◀──────── Assemblage ◀──────── Transcription
(SRT, VTT ou JSON) (recalage,           (parallèle, retries, cache)
                    coupe des lignes)        │ verbose_json
                                             ▼
                          Backends · trait Transcriber
                          ├─ Whisper local (feature Cargo `local`)
                          └─ API compatible OpenAI (multipart ou json-base64)
```

L'audio est découpé avant tout appel réseau. Chaque morceau transcrit rentre avec son offset, et l'assemblage est la seule étape qui manipule des timestamps absolus.

```rust
#[async_trait]
trait Transcriber {
    async fn transcribe(&self, chunk: &AudioChunk, opts: &Options)
        -> Result<Vec<Segment>, TranscribeError>;
}
// Segment { start: f64, end: f64, text: String, words: Option<Vec<Word>> }
// relatif au morceau ; l'assemblage ajoute chunk.offset
```

ffmpeg est appelé en sous-processus plutôt que via des bindings, pour garder un build simple : c'est un prérequis système documenté. Crates envisagées, à confirmer au démarrage :

| Rôle | Crate |
| --- | --- |
| Arguments CLI | clap (derive) |
| Async et concurrence | tokio |
| HTTP, multipart | reqwest (rustls) |
| Sérialisation | serde, serde_json, toml, base64 |
| Progression et couleurs | indicatif, owo-colors |
| Diagnostics d'erreur lisibles | miette, thiserror |
| Chemins config et cache | directories |
| Backend local (feature `local`) | whisper-rs |

## Contraintes et risques connus

Le risque principal est le timestamp : sans `verbose_json`, impossible de produire un SRT. Le choix du modèle passe donc avant tout le reste. Les contraintes ci-dessous viennent de la [doc STT d'OpenRouter](https://openrouter.ai/docs/guides/overview/multimodal/stt), sauf mention contraire.

| Contrainte ou risque | Effet sur srtty | Parade prévue |
| --- | --- | --- |
| `srt`, `vtt` et `text` sont refusés (400) ; `verbose_json` est rejeté par les fournisseurs sans sortie structurée et par certains modèles (`openai/gpt-4o-transcribe`, `microsoft/mai-transcribe-1.5`) | Pas de timestamps, donc pas de SRT | Toujours demander `verbose_json`, assembler le SRT soi-même (F5, F6). Erreur explicite et pointeur vers `srtty models` |
| Les préférences de routage (`order`, `only`, `ignore`) ne s'appliquent pas à la transcription | On ne peut pas forcer un fournisseur qui renvoie des segments ; le comportement dépend du modèle choisi | Choisir un modèle par défaut validé par test (F5). Point à revérifier, OpenRouter peut changer ce comportement |
| Upload multipart plafonné à 25 Mo | Un long fichier WAV dépasse vite la limite | Audio compressé (F2), découpage (F3). Le mode `json-base64` passe au-delà de 25 Mo, mais seulement sur les modèles OpenAI et Groq |
| Timeout amont de 60 s par requête | Un morceau trop long échoue | Durée de morceau par défaut à calibrer par test, retries (F4) |
| `prompt` accepté mais ignoré en multipart | `--prompt` sans effet en mode multipart chez OpenRouter | En mode `json-base64`, le passer via `provider.options.<slug>` ; prévenir l'utilisateur sinon |
| Slug de modèle complet exigé (`openai/whisper-1`, pas `whisper-1`) | Erreur « model not found » | Le slug vient de la config du fournisseur, sans normalisation implicite |
| Coupure au milieu d'un mot entre deux morceaux | Mot perdu ou dupliqué à la jonction | Couper sur un silence (F3). Un léger chevauchement reste une option si les tests montrent des pertes |
| Whisper tend à inventer du texte répété sur des passages silencieux (comportement souvent rapporté, non mesuré ici) | Sous-titres fantômes | Filtrer les segments vides ou répétés à l'assemblage ; règle exacte à définir après test |
| Le backend local via whisper.cpp demande une toolchain C/C++ (cmake, clang) à la compilation (à ma connaissance) | Build par défaut plus lourd et plus fragile | Isoler derrière la feature `local` (F11) |

## Critères d'acceptation

La v1 est livrable quand tous ces scénarios passent sur une machine Linux propre. Les seuils chiffrés restent à fixer avant le développement.

- [ ] Une vidéo d'une heure en français, transcrite via OpenRouter, donne un SRT accepté par un parseur strict et par mpv, avec des timestamps strictement croissants.
- [ ] Le même fichier passe via OpenAI et via Groq en ne changeant que `--provider`.
- [ ] Sur cinq points de contrôle répartis dans la vidéo, le décalage entre parole et sous-titre reste dans un seuil à définir.
- [ ] Une coupure réseau simulée sur un morceau déclenche des retries. Si l'échec persiste, une relance ne retranscrit que les morceaux manquants (F12).
- [ ] Un modèle sans `verbose_json` produit une erreur claire et un code de retour `2`, sans fichier partiel écrit.
- [ ] Rediriger vers un fichier ou poser `NO_COLOR` donne une sortie sans code ANSI ; `--json` produit du JSON valide.
- [ ] La clé d'API n'apparaît dans aucune sortie, y compris en mode verbeux et dans les messages d'erreur.
- [ ] `cargo build` sans feature compile sans toolchain whisper.cpp ; `cargo build --features local` transcrit le même fichier hors ligne.

## Licence, distribution et conventions

Licence proposée : MIT. Dans l'écosystème Rust, le double « MIT OR Apache-2.0 » est très répandu, et c'est un choix à trancher (voir Questions ouvertes).

- Dépôt GitHub public : README avec une démo animée du terminal, un exemple de config et un tableau de modèles testés.
- Distribution par `cargo install srtty`, plus des binaires Linux x86_64 et aarch64 en GitHub Releases (outillage de release à choisir).
- CI : `cargo fmt --check`, `clippy -D warnings` et les tests sur chaque PR. Les tests unitaires couvrent l'assemblage SRT et VTT et le recalage des offsets, sur des réponses `verbose_json` enregistrées, sans appel réseau.
- Un test d'intégration optionnel contre une vraie API ne tourne que si une clé est présente dans l'environnement.

## Roadmap v2 et au-delà

La v2 ajoute les deux fonctions sorties du périmètre v1. Les idées suivantes ne sont pas encore priorisées.

1. **Traduction par LLM (v2).** Les segments sont envoyés par lots à un modèle de chat (même clé OpenRouter), avec une consigne de traduction vers la langue cible qui laisse les timestamps intacts. La CLI vérifie que le nombre et les ID des segments en sortie correspondent à l'entrée, et relance le lot sinon.
2. **Traitement par lot (v2).** `srtty <dossier>` traite tous les fichiers média du dossier, avec une file partagée, un récapitulatif global et un saut des fichiers déjà sous-titrés.
3. Pistes ultérieures : mode surveillance d'un dossier, téléchargement des modèles locaux, incrustation des sous-titres via ffmpeg, diarisation via `provider.options` quand le fournisseur la propose.

## Questions ouvertes

- [ ] Nom définitif : « srtty » est provisoire, sa disponibilité sur crates.io et GitHub n'est pas vérifiée.
- [ ] Licence : MIT seule, ou double MIT OR Apache-2.0 ?
- [ ] Rendu : indicatif + couleurs suffit pour une CLI linéaire. Ratatui avec un viewport inline (à ma connaissance disponible dans les versions récentes) permettrait un tableau de bord par morceau plus riche, au prix de plus de code. Lequel ?
- [ ] Modèle par défaut : lequel renvoie bien des segments via OpenRouter ? À établir par un test sur deux ou trois modèles Whisper.
- [ ] Durée de morceau et format audio par défaut (MP3 64 kbit/s ou Opus) : à calibrer contre le timeout de 60 s et la qualité.
- [ ] Mode de requête OpenRouter par défaut : multipart (plus simple, compatible OpenAI) ou JSON base64 (prompt via `provider.options`, au-delà de 25 Mo sur certains modèles) ?
- [ ] Backend local : bindings whisper.cpp, ou un binaire externe (whisper.cpp ou faster-whisper) piloté en sous-processus ?

## Sources

- [OpenRouter — Speech-to-Text](https://openrouter.ai/docs/guides/overview/multimodal/stt), consulté le 30 octobre 2026
- [Buzz — Preferences](https://chidiwilliams.github.io/buzz/docs/preferences)
