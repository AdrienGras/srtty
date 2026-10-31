# srtty

CLI Rust qui transforme une vidéo ou un audio en sous-titres (SRT, VTT, TXT) via des API de transcription compatibles OpenAI (OpenRouter d'abord).

**Avant toute chose, lire `docs/HANDOFF.md`** (section « État courant », puis ce qui concerne la tâche). On y trouve l'état du projet, les plans restants, les décisions ouvertes, les points reportés, les pièges de l'environnement et la façon de travailler d'Adrien.

Règles qui s'appliquent à chaque session :

- La conception `docs/superpowers/specs/2026-10-30-srtty-design.md` fait foi.
- Workflow superpowers : `writing-plans`, relecture par Adrien, puis `subagent-driven-development` sur une branche de feature et PR. **Ne jamais merger une PR soi-même.**
- Texte de la CLI en anglais ; docs du dépôt en français. Répondre à Adrien en français.
- Commits terminés par `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>` ; PR terminées par `🤖 Generated with [Claude Code](https://claude.com/claude-code)`.
- Annoncer le coût estimé avant toute dépense d'API réelle. Clé OpenRouter : `~/.config/secrets/openrouter_api_key`, jamais affichée.
- Avant un commit : `cargo fmt --check`, `cargo clippy --all-targets -- -D warnings`, `cargo test` (sortie brute : `rtk proxy cargo test`).
- Mettre `docs/HANDOFF.md` à jour à la fin de chaque plan.
