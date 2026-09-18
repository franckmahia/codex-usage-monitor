# AGENTS.md — Codex Usage Monitor

## TOKEN ACCOUNTING INVARIANTS

1. `token_usage_record.payload.usage` is the primary unit of accounting.
2. Never sum `turn_token_usage`.
3. Never sum `thread_token_usage`.
4. Never use `threads.tokens_used` for period totals.
5. `cached_input_tokens` is a subset of `input_tokens`.
6. `cache_write_input_tokens` is a subset of `input_tokens`.
7. `reasoning_output_tokens` is a subset of `output_tokens`.
8. Deduplicate by `response_id` whenever available.
9. Never assign a price to an unknown model silently.
10. Raw usage and estimated monetary value must remain separate.
11. Never call an estimated value an OpenAI bill.
12. Never upload Codex rollout contents.

## Sûreté

- Les fichiers Codex (`~/.codex/`) sont uniquement **lus**, jamais modifiés.
- Aucun réseau par défaut. Télémétrie interdite. Aucun upload de logs, prompts, réponses ou contenus de fichiers.
- Le parser lit les rollouts ligne à ligne, en streaming ; il ne charge jamais un rollout entier en mémoire.
- Un record inconnu, un champ absent ou un nouveau champ ne doit jamais faire échouer le fichier entier : échec local sur la ligne, continuation du fichier, diagnostic.

## Comptabilisation

- Consommation d'une période = `SUM(token_usage_record.payload.usage)` après déduplication par `response_id`.
- Fallback ancien format : `event_msg` avec `payload.type == "token_count"` → `payload.info.last_token_usage`.
  Ne jamais utiliser `total_token_usage` comme source d'agrégation.
- Jamais mélanger les deux méthodes pour une même session : priorité
  `1. token_usage_record.usage` → `2. legacy last_token_usage` → `3. diagnostic`.
- Attribution modèle : table `turn_id → model` construite depuis `turn_context`.
  Confiances : `exact`, `inferred`, `unknown`. Ne jamais inventer un modèle.
- Catégories :
  - `ordinary_input = input − cached_input − cache_write_input`
  - `reasoning_output ⊆ output`
- Long context : décision appel par appel (`input > 272 000`), jamais sur des totaux agrégés.

## Pricing

- Tarifs dans `pricing/*.json` versionnés avec `source_url`, `last_verified`, période de validité.
- Profil **Codex** : pas de facturation des écritures de cache (hypothèse documentée : `cache_write` valorisé au tarif cached).
- Profil **API** : `cache_write` facturé 1,25 × l'entrée ordinaire.
- Modèle inconnu → tokens comptés, coût `N/A`.
- Une valeur estimée est toujours étiquetée « équivalent » ou « estimé », jamais « facture ».

## Validation obligatoire (gate)

Sur les données réelles du 11 au 18 septembre 2026, `codex-meter summary` doit retrouver :

- Input ≈ 0,86 Md (référence jq : 876 415 995)
- Output ≈ 2,9 M (référence jq : 2 944 355)
- Cache hit ≈ 96 %
- et surtout NE PAS afficher ≈ 49,8 B tokens (somme des compteurs cumulatifs = erreur connue).

## Commandes

```bash
cargo test -q
cargo run -q -- summary --from 2026-09-11 --to 2026-09-18
cargo run -q -- doctor
```

## Phases

1. Core CLI (scan / summary / doctor) — gate de non-régression sur données réelles.
2. SQLite + import incrémental + watcher.
3. Pricing engine complet (Codex / API, historique, long context, fast, régional).
4. UI Tauri (dashboard, diagnostics, export CSV/JSON).
5. Temps réel.
6. Connecteur API OpenAI (usage/costs administratifs, clé en Keychain, jamais en SQLite).
7. Packaging macOS / Linux / Windows.
