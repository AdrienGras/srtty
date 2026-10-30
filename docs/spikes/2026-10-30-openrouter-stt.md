# Spike — endpoint STT d'OpenRouter (2026-10-30)

Spike jetable pour trancher les questions ouvertes du PRD sur le modèle, le format audio, la durée des morceaux et le mode de requête. Le script est resté hors du dépôt, dans `/tmp/srtty-spike/`. Coût total : 0,47 $.

Média de test : « La Barbe bleue » (Perrault), lecture LibriVox en français, domaine public, 11 min 04 s, emballée en MKV avec une piste vidéo noire. Une seule voix et un enregistrement propre, ce qui est plus favorable qu'un talk ou une réunion.

## Conclusions

1. **Construire les sous-titres à partir des mots, pas des segments.** Sur 18 modèles qui acceptent `verbose_json`, 10 renvoient **un seul segment** pour 30 s d'audio. Le même modèle (`openai/whisper-large-v3`) donne 1 segment ou 7 selon le fournisseur qui sert la requête, et le routage ne se contrôle pas. En revanche, 16 de ces 18 modèles renvoient des timestamps au mot. Les segments renvoyés atteignent aussi 20 s, trop long pour un sous-titre.
2. **Les deux familles AssemblyAI sont inutilisables** pour du SRT : un segment par requête et aucun mot.
3. **9 modèles refusent `verbose_json`**, dont toute la gamme `gpt-*-transcribe`. Le message d'erreur est explicite (« does not support response_format "verbose_json" »), sauf pour les deux Voxtral 3.0, qui renvoient un 400 générique.
4. **La latence n'est pas le facteur limitant.** Un morceau de 600 s revient en 5 à 23 s selon le modèle, loin du timeout de 60 s.
5. **Opus 32 kbit/s, mono, 16 kHz convient comme format par défaut** : 1,1 Mo pour 5 min et environ 13 Mo par heure. Il garde 98,6 à 99,7 % de similarité de texte avec le FLAC. Les écarts restants viennent surtout du changement de fournisseur entre deux requêtes, pas du format.
6. **Les modes multipart et json-base64 donnent les mêmes résultats** sur les 27 modèles testés. Avec des morceaux Opus de quelques Mo, le plafond multipart de 25 Mo n'est jamais atteint. Multipart suffit par défaut.
7. **La découpe sur silence évite les pertes aux jonctions.** Avec une coupe fixe toutes les 180 s, des mots disparaissent : « du *sang* sur cette clé », « d'une *main* ». Avec une coupe au milieu du silence le plus proche (`silencedetect`, -30 dB, 0,4 s, fenêtre de ±30 s), les trois jonctions tombent entre deux phrases, sans perte, sans chevauchement ni segment qui déborde du morceau.

## Modèles (sonde de 30 s, MP3 64 kbit/s, `language=fr`, segments et mots demandés)

Segments : nombre renvoyé en multipart / en json-base64. Coût par heure extrapolé depuis `usage.cost`.

| Modèle | `verbose_json` | Segments /30 s | Mots | Coût ≈ $/h | Remarque |
| --- | --- | --- | --- | --- | --- |
| `deepgram/nova-3` | oui | 7/7 | 96 | 0,26 | latence stable ~5 s |
| `openai/whisper-large-v3-turbo` | oui | 7/7 | 103 | 0,01 | mots découpés en tokens (« c », « 'est ») ; fournisseur et prix variables |
| `nvidia/nemotron-3.5-transcribe` | oui | 5/5 | 96 | 0,12 | |
| `openai/whisper-1` | oui | 4/4 | 100 | 0,36 | mots découpés en tokens |
| `qwen/qwen3-asr-0.6b` | oui | 3/3 | 96 | 0,01 | |
| `qwen/qwen3-asr-1.7b` | oui | 3/3 | 96 | 0,03 | |
| `nvidia/nemotron-3.5-asr-streaming-multilingual-0.6b` | oui | 2/2 | 96 | 0,01 | |
| `nvidia/parakeet-tdt-0.6b-v3` | oui | 2/2 | 96 | 0,09 | |
| `openai/whisper-large-v3` | oui | 1/7 | 96 | 0,03–0,09 | granularité selon le fournisseur (Together : 1, DeepInfra : 7) |
| `elevenlabs/scribe-v2` | oui | 1/1 | 96 | 0,22 | mots avec confiance |
| `elevenlabs/scribe-v2-medical` | oui | 1/1 | 96 | 0,22 | |
| `fish-audio/transcribe-1` | oui | 1/1 | 96 | 0,36 | |
| `fish-audio/transcribe-1-pro` | oui | 1/1 | 96 | 0,36 | |
| `google/gemini-3.5-transcribe` | oui | 1/1 | 97 | 0,37 | pas de `language` ni `duration` renvoyés |
| `mistralai/voxtral-mini-transcribe` | oui | 1/1 | 96 | 0,18 | |
| `x-ai/grok-stt-1.0` | oui | 1/1 | 96 | 0,10 | |
| `assemblyai/universal-3-5-pro` | oui | 1/1 | 0 | 0,30 | inutilisable : ni segments fins ni mots |
| `assemblyai/universal-3-6-pro` | oui | 1/1 | 0 | 0,30 | inutilisable : ni segments fins ni mots |
| `google/chirp-3` | non | | | | refuse `verbose_json` |
| `meta/muse-voice-transcribe-1.0` | non | | | | refuse `verbose_json` |
| `mistralai/voxtral-mini-3b-2507` | non | | | | refuse `verbose_json` |
| `mistralai/voxtral-small-24b-2507-stt` | non | | | | refuse `verbose_json` |
| `openai/gpt-4o-mini-transcribe` | non | | | | refuse `verbose_json` |
| `openai/gpt-4o-transcribe` | non | | | | refuse `verbose_json` |
| `openai/gpt-transcribe` | non | | | | refuse `verbose_json` |
| `mistralai/voxtral-mini-transcribe-3-0` | non | | | | 400 générique du fournisseur, à revérifier |
| `mistralai/voxtral-small-transcribe-3-0` | non | | | | 400 générique du fournisseur, à revérifier |

Non testés : `microsoft/mai-transcribe-1.5` et `microsoft/mai-transcribe-2`, annoncés à 0,36 et 0,10 $ par seconde d'audio. Le premier refuse `verbose_json` d'après la doc OpenRouter.

## Latence selon la durée du morceau (Opus 32 kbit/s, multipart, segments et mots)

| Modèle | 60 s | 180 s | 300 s | 600 s |
| --- | --- | --- | --- | --- |
| `openai/whisper-large-v3-turbo` | 11,9 s | 7,8 s | 14,5 s | 23,0 s |
| `deepgram/nova-3` | 5,1 s | 4,3 s | 4,3 s | 5,2 s |
| `elevenlabs/scribe-v2` | 5,5 s | 7,7 s | 13,1 s | 15,9 s |

## Format audio (extrait de 300 s, similarité des mots avec la version FLAC)

| Format | Taille | whisper-large-v3-turbo | scribe-v2 |
| --- | --- | --- | --- |
| FLAC 16 kHz | 9,1 Mo | référence | référence |
| MP3 64 kbit/s | 2,3 Mo | 0,991 | 0,996 |
| MP3 32 kbit/s | 1,1 Mo | 0,984 | 0,996 |
| Opus 32 kbit/s | 1,1 Mo | 0,986 | 0,997 |
| Opus 24 kbit/s | 0,8 Mo | 0,960 (autre fournisseur) | 0,998 |
| Opus 16 kbit/s | 0,6 Mo | 0,959 (autre fournisseur) | 0,996 |

## Diarisation (identification des voix)

Média : « Les Précieuses ridicules » (Molière), lecture LibriVox à plusieurs voix, un lecteur par rôle. Scène 10 (15 min, 4 à 5 voix), puis la pièce entière (58 min, 13 lecteurs crédités). Option envoyée : `provider.options.<tag> = {"diarize": true}`. Coût de cette partie : 0,71 $, soit 1,19 $ pour l'ensemble des spikes.

### Conclusions

1. **Deux modèles marchent : `x-ai/grok-stt-1.0` et `deepgram/nova-3`.** Chacun est servi par un seul fournisseur, donc le résultat est stable. Chaque mot porte un index `speaker`, et les segments en portent un chez Deepgram.
2. **Le champ `provider` passe aussi en multipart**, sous forme de chaîne JSON. La doc ne le mentionne pas.
3. **Un morceau = une numérotation.** Chaque morceau renumérote ses voix : Cathos est la voix 3 dans le fichier entier, mais la voix 0 dans le deuxième morceau. Les regroupements restent propres à l'intérieur d'un morceau. On ne peut pas les recoller avec ce que renvoie l'API, qui ne fournit pas d'empreinte de voix.
4. **La diarisation impose donc une seule requête pour tout le fichier.** Sur 58 min (12,8 Mo en Opus 32 kbit/s), Grok répond en 35 s et Deepgram en 44,5 s. Le timeout de 60 s place la limite pratique vers 1 h 15–1 h 20. Deepgram refuse le gros fichier en json-base64 (« does not support large audio inputs ») mais l'accepte en multipart.

### Résultats

| Modèle | Scène 10 (15 min) | Pièce entière (58 min) | Coût / h | Remarque |
| --- | --- | --- | --- | --- |
| `x-ai/grok-stt-1.0` | 5 voix, retrouve Almanzor (1 mot) | 8 voix, 34,8 s | 0,10 $ | meilleur rapport qualité/prix |
| `deepgram/nova-3` | 4 voix, rate Almanzor | 10 voix, 44,5 s (multipart) | 0,25 $ | segments déjà découpés par voix |
| `elevenlabs/scribe-v2` | 3 voix, mélange les personnages | non testé | 0,22 $ | inutilisable pour la diarisation |
| `mistralai/voxtral-mini-transcribe` | option ignorée sans erreur | non testé | 0,18 $ | pas de diarisation |

Non vérifié : la justesse de l'attribution sur la pièce entière. Avec 13 lecteurs, les deux modèles en trouvent moins, ce qui laisse penser que des voix proches sont fusionnées.

## Non couvert

- L'effet réel de `--prompt` : il est ignoré en multipart chez OpenRouter, et on peut le passer via `provider.options.<tag>` en json-base64, mais le champ diffère selon le fournisseur.
- Un audio réel bruité, avec plusieurs voix ou des silences longs (hallucinations de Whisper).
- Les fournisseurs en direct (OpenAI, Groq).
