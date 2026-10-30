# Extraits audio embarqués

Deux extraits courts, en français et dans le domaine public (enregistrements LibriVox), compilés dans le binaire par `src/probe.rs`. Ils servent à tester un modèle de transcription : `srtty setup` (test optionnel et payant) et la commande cachée `srtty compat probe` (CI hebdomadaire).

Format : Ogg Opus, mono, 16 kHz, 32 kbit/s, mode `voip`.

Les commandes ci-dessous supposent que les MP3 téléchargés ont été renommés `barbe_bleue.mp3` et `p_10.mp3` (noms d'origine : `contes_du_temps_passe_04_perrault_64kb.mp3` et `precieusesridicules_10_moliere_64kb.mp3`), et qu'elles sont lancées à la racine du dépôt avec les MP3 dans le dossier courant.

## `sample-30s.ogg` (30 s, environ 114 Ko)

Une seule voix. Extrait de *La Barbe bleue* (Charles Perrault, recueil *Contes du temps passé*, LibriVox), 30 s à partir de 1 min 00.

- Source : <https://www.archive.org/download/contes_temps_passe_librivox/contes_du_temps_passe_04_perrault_64kb.mp3>

```bash
ffmpeg -v error -ss 60 -t 30 -i barbe_bleue.mp3 -vn -ac 1 -ar 16000 -c:a libopus -b:a 32k -application voip assets/sample-30s.ogg
```

## `speakers-45s.ogg` (45 s, environ 169 Ko)

Plusieurs voix qui alternent. Extrait de la scène 10 des *Précieuses ridicules* (Molière, LibriVox), 45 s à partir de 0 min 50.

- Source : <https://archive.org/download/precieusesridicules_1012_librivox/precieusesridicules_10_moliere_64kb.mp3>

```bash
ffmpeg -v error -ss 50 -t 45 -i p_10.mp3 -vn -ac 1 -ar 16000 -c:a libopus -b:a 32k -application voip assets/speakers-45s.ogg
```

Choix de la fenêtre : `tests/fixtures/grok_diar.json` est la scène 10 diarisée par Grok (durée 897,5 s, 5 locuteurs sur l'ensemble). Ses timestamps sont ceux de la scène, sans décalage. Parmi toutes les fenêtres de 45 s (pas de 1 s) dont les mots sont entièrement inclus, la fenêtre démarrant à 50 s est celle qui contient le plus de locuteurs distincts (5 sur 5), avec le plus d'alternances de locuteur dans les mots (10 changements, 128 mots). La fenêtre a été choisie d'après la diarisation de Grok, elle n'a pas été vérifiée à l'oreille.
