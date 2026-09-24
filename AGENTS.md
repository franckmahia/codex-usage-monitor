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
  La règle est **globale à la session** (table `primary_sessions`), pas seulement
  par fichier : dès qu'une session porte un `token_usage_record`, ses events
  legacy sont ignorés — même ajoutés plus tard dans un autre segment du fichier.
- Attribution modèle : table `turn_id → model` construite depuis `turn_context`,
  persistée par fichier dans `source_files.enrichment_json` pour survivre aux
  imports incrémentaux (un record dont le `turn_context` précède le checkpoint
  garde son modèle). Confiances : `exact`, `inferred`, `unknown`. Ne jamais
  inventer un modèle.
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

Sur les données réelles du 11 au 18 septembre 2026, `codex-meter summary` doit retrouver
(référence mesurée le 2026-09-24 ; les rollouts de ces journées ont pu grossir depuis
l'ancienne référence jq 876 415 995 — revérifier la référence, pas seulement l'ordre de grandeur) :

- Input = 898 872 208 (≈ 0,90 Md)
- Output = 3 005 379 (≈ 3,0 M)
- Cache hit ≈ 96 %
- Équivalent ≈ $860
- et surtout NE PAS afficher ≈ 49,8 B tokens (somme des compteurs cumulatifs = erreur connue).
- `cargo run -q -- doctor` après import : `Status: CONSISTENT`.

## Commandes

```bash
cargo test -q
cargo run -q -- summary --from 2026-09-11 --to 2026-09-18
cargo run -q -- doctor
```

Phase 2 (SQLite) :

```bash
cargo run -q -- import                    # import incremental (checkpoints, dedup, enrichissement state_5)
cargo run -q -- import --full             # RECONSTRUCTION : purge + reimport complet (aligne les event_uid
                                          # sur le scan, supprime les lignes de fichiers disparus)
cargo run -q -- import --reprice          # recalcule tous les couts
cargo run -q -- watch                     # surveillance temps reel (notify), import a chaque changement
cargo run -q -- export --csv <dossier>    # export CSV normalise (by-day/model/project/activity/thread + calls)
cargo run -q -- scan                      # scan en memoire SANS base (verification / gate)
```

- Base par defaut : `<data_dir>/codex-meter/meter.sqlite` (jamais dans `~/.codex`).
- `state_5.sqlite` ouvert en READ ONLY strict (fallback `immutable=1`) ; `threads.tokens_used` affiché comme indication lifetime uniquement, jamais source de totaux.
- Checkpoints par fichier : `(size, mtime)` pour skip, `last_complete_offset` + `lines_total` pour la reprise ; une dernière ligne incomplète (JSON non terminé) n'avance jamais le checkpoint. `pending_newline` évite de compter deux fois le `\n` final d'une ligne acceptée sans newline ; `had_primary` porte le format moderne au niveau fichier ; `enrichment_json` persiste `session_meta`/`turn_context` entre segments.
- `event_uid` (fallback sans response_id) = SHA256(session|thread|turn|timestamp|input|cached|output|ordinal) — sans le chemin : un fichier déplacé vers `archived_sessions/` ne double-compte pas.
- La base métier a un `busy_timeout` de 5 s (UI + watch + CLI concurrents) et les transactions sont `BEGIN IMMEDIATE`.
- Re-pricing automatique si la version du catalogue change ; les coûts restent séparés des tokens bruts (`pricing_results`), avec `service_tier` conservé (le multiplicateur fast survit au re-pricing).
- Attribution modèle des appels legacy : `model` du thread (confiance `inferred`), jamais inventé.

## UI Tauri (Phase 4)

```bash
./ui/node_modules/.bin/tauri dev     # dev (vite + app)
./ui/node_modules/.bin/tauri build   # bundle macOS (.app + .dmg)
```

- Backend : `src-tauri/` (commands get_summary/get_threads/get_diagnostics/refresh_import/export_csv_files/get_pricing), réutilise la lib `codex-meter` — aucune logique de comptage dupliquée.
- Frontend : `ui/` (React 19 + TypeScript strict + Vite). `npm --prefix ui install` avant le premier build.
- L'app ne fait que lire la base et relancer l'import incrémental ; aucun réseau.

## Phases

1. ✅ Core CLI (scan / summary / doctor) — gate de non-régression sur données réelles.
2. ✅ SQLite + import incrémental + watcher + enrichissement state_5 (sous-agents, titres) + export CSV.
3. ✅ Pricing engine complet (Codex / API, historique, long context, fast, régional).
4. UI Tauri (dashboard, diagnostics, export CSV/JSON).
5. Temps réel.
6. Connecteur API OpenAI (usage/costs administratifs, clé en Keychain, jamais en SQLite).
7. Packaging macOS / Linux / Windows.
