# Codex Usage Monitor

Compteur de tokens Codex **local, auditable et hors ligne** : mesure précise de la consommation observée dans les logs de Codex (Desktop / CLI / IDE, agents et sous-agents, Auto Review), valorisation « équivalent » selon la grille Codex/Work, diagnostics de cohérence, export CSV/JSON et dashboard macOS.

> **Ce que mesure l'application :**
>
> - **Usage observé** — mesures provenant directement des logs locaux.
> - **Coût équivalent Codex** — valorisation de cet usage selon la grille versionnée.
> - **Coût API estimé** — profil tarifaire alternatif (écritures de cache à 1,25×).
>
> Une valeur estimée est toujours étiquetée « équivalent » ou « estimé ». **Ce n'est jamais une facture OpenAI.**

---

## Principes fondamentaux

Les 12 invariants de comptage sont gravés dans [`AGENTS.md`](AGENTS.md). Les essentiels :

1. `token_usage_record.payload.usage` est l'unité primaire de comptabilité.
2. Les compteurs cumulatifs (`turn_token_usage`, `thread_token_usage`, `threads.tokens_used`) ne sont **jamais** sommés — c'est l'erreur classique qui produit des totaux absurdes (~49,8 B tokens).
3. `cached_input ⊆ input`, `cache_write_input ⊆ input`, `reasoning_output ⊆ output`.
4. Déduplication par `response_id` en priorité, sinon `event_uid` = SHA256(session | thread | turn | timestamp | input | cached | output | ordinal) — **sans le chemin** : un fichier déplacé vers `archived_sessions/` ne double-compte pas.
5. Aucun prix pour un modèle inconnu (tokens comptés, coût `N/A`).
6. Les fichiers `~/.codex/` sont uniquement **lus**, jamais modifiés. `state_5.sqlite` est ouvert en READ ONLY strict.
7. Aucun réseau. Aucune télémétrie. Aucun upload de contenu (prompts, réponses, fichiers ne quittent jamais la machine).

---

## Installation

Prérequis : [Rust](https://rustup.rs) (stable), Node.js ≥ 20.

```bash
git clone https://github.com/franckmahia/codex-usage-monitor.git
cd codex-usage-monitor

# CLI
cargo build --release

# UI Tauri (première fois)
npm --prefix ui install
./ui/node_modules/.bin/tauri build    # .app + .dmg macOS
```

Emplacements :

| Élément | Chemin |
|---|---|
| Binaire CLI | `target/release/codex-meter` |
| App macOS | `src-tauri/target/release/bundle/macos/Codex Usage Monitor.app` |
| DMG | `src-tauri/target/release/bundle/dmg/` |
| Base SQLite | `~/Library/Application Support/codex-meter/meter.sqlite` |
| Logs Codex (lecture seule) | `~/.codex/` (ou `$CODEX_HOME`) |

---

## CLI

```bash
# Totaux de période + ventilation par modèle/jour + coût équivalent + anomalies
codex-meter summary --from 2026-09-12 --to 2026-09-18

# Ventilations
codex-meter summary --from 2026-09-12 --to 2026-09-18 --by day|project|thread|activity|model
codex-meter summary --from 2026-09-12 --to 2026-09-18 --json        # export JSON

# Import incrémental dans SQLite (checkpoints, dédup, enrichissement state_5)
codex-meter import                # incrémental : ne relit que les fichiers modifiés
codex-meter import --full         # re-scan complet (la dédup garantit l'idempotence)
codex-meter import --reprice      # recalcule tous les coûts

# Diagnostics et consistency check (base vs scan frais)
codex-meter doctor

# Surveillance temps réel (notify) : import à chaque changement
codex-meter watch

# Export CSV normalisé
codex-meter export --csv <dossier>            # by-day/model/project/activity/thread + calls.csv
codex-meter export --csv <dossier> --json out.json

# Scan en mémoire sans base (vérification / gate)
codex-meter scan
```

Options globales : `--codex-home <chemin>` (défaut `$CODEX_HOME` sinon `~/.codex`), `--db <chemin>`, `--pricing <catalogue.json>` (remplace le catalogue Codex embarqué), `--json`.

Exemple de sortie (`summary`) :

```
Periode            2026-09-12 → 2026-09-18
Calls        6,296
Input        773.2 M
Cached       743.5 M
Non cached   29.7 M
Output       2.5 M
Cache hit    96.2 %

Equivalent Codex   $730.28
Sans cache         $4,984.61
Economie cache     $4,254.33 (85.3 %)
Confiance tarif    87.4 %

Par modèle
  gpt-6-astra        in   371.0 M  cached  358.6 M  ...  $550.68
  gpt-5.6-sol        in   244.2 M  cached  238.7 M  ...  $133.92
  codex-auto-review  in    78.1 M  cached   68.2 M  ...   $43.54
```

---

## Dashboard Tauri

```bash
./ui/node_modules/.bin/tauri dev      # développement (vite + app)
./ui/node_modules/.bin/tauri build    # bundle macOS (.app + .dmg)
```

- **Dashboard** : cartes INPUT / CACHED / CACHE HIT / OUTPUT / ÉQUIVALENT / ÉCONOMIE CACHE, filtre de période, graphique d'usage quotidien, tables par modèle / activité / projet, anomalies.
- **Threads** : top threads avec titres enrichis (state DB) et badges `main / subagent / auto_review`.
- **Diagnostics** : cohérence base ↔ scan frais, erreurs de parsing, état de l'enrichissement, grilles tarifaires versionnées.
- **Export CSV** via sélecteur de dossier natif.
- Aucun réseau, lecture seule des logs Codex.

---

## Comptabilisation

### Priorité des sources (jamais mélanger pour une même session)

1. `token_usage_record.payload.usage` (format moderne)
2. Legacy : `event_msg` avec `payload.type == "token_count"` → `payload.info.last_token_usage` — **jamais** `total_token_usage`
3. Sinon : diagnostic

### Attribution modèle

Table `turn_id → model` reconstruite depuis `turn_context`. Confiances : `exact` (turn trouvé), `inferred` (root unique, ou modèle du thread pour les records legacy via state DB), `unknown`. Un modèle inconnu reste compté en tokens, jamais tarifé.

### Service tier

Capté depuis `event_msg → thread_settings_applied → thread_settings.service_tier`. Trois états : `standard / fast / unknown` — pas de supposition silencieuse du standard. Un event postérieur ne requalifie jamais un appel antérieur. Le multiplicateur fast (×2,5 pour Astra/GPT-5.6) n'est appliqué que si le tier `fast` est explicitement connu. Régional : réservé (jamais supposé). La **confiance tarifaire** (%) = appels avec modèle connu **et** tier déterminé.

### Long context

Décision **appel par appel** (`input > 272 000`) : input ×2, cached ×2, output ×1,5 — sauf GPT-6 Astra (exception Codex). Jamais sur des totaux agrégés.

### Cache

- Profil **Codex** : les écritures de cache ne sont pas facturées séparément ; hypothèse documentée : valorisées au tarif cached (jamais au multiplicateur API).
- Profil **API** : `cache_write` facturé 1,25 × l'entrée ordinaire.
- `ordinary_input = input − cached − cache_write`.

---

## Pricing

Catalogues **versionnés** dans `pricing/` :

| Catalogue | Contenu |
|---|---|
| `openai-codex-2026-09-23.json` | Grille Codex/Work : Astra 10/1/50, Sol 4/0,40/20, Terra 2/0,20/12, Luna 0,20/0,02/1,20, GPT-5.5 5/0,50/30, GPT-5.4 2,50/0,25/15, GPT-5.4-Mini, GPT-5.3-Codex, GPT-5.2, Rosalind, Daybreak |
| `openai-api-2026-09-23.json` | Profil API (grille officielle) : `cache_write_rate` = 1,25 × input quand supporté, sinon N/A |

Chaque règle porte `effective_from` / `effective_until`, `source_url`, `last_verified`. Alias : `codex-auto-review → gpt-5.4`. Un changement de version de catalogue déclenche un re-pricing automatique ; les coûts restent séparés des tokens bruts (`pricing_results`).

---

## Architecture

```text
React / TypeScript (ui/)
        │
        ▼  commands Tauri (src-tauri/) — aucune logique de comptage dupliquée
        │
┌────────────────────────────────────────┐
│              Rust Core                 │
│  discovery   → sessions/ + archived/   │
│  parser      → JSONL streaming,        │
│                checkpoints d'offset,   │
│                tolérant aux lignes     │
│                partielles/corrompues   │
│  scan        → scan mémoire (gate)     │
│  import      → incrémental + dédup     │
│  state       → state_5.sqlite READ ONLY│
│                (titres, sous-agents,   │
│                parent, modèle legacy)  │
│  pricing     → catalogues versionnés   │
│  aggregate   → buckets + anomalies     │
│  export      → CSV normalisés          │
│  watch       → notify temps réel       │
└────────────────┬───────────────────────┘
                 ▼
     SQLite (meter.sqlite) — hors de ~/.codex
```

Tables principales : `inference_calls` (event_uid PK, response_id UNIQUE), `threads`, `thread_settings`, `source_files` (checkpoints `last_complete_offset` + `lines_total`), `pricing_rules`, `pricing_results`, `import_run`, `import_errors`.

Une dernière ligne JSON incomplète (écriture partielle) n'avance **jamais** le checkpoint : elle est relue à la passe suivante. Un record invalide échoue localement et le parsing continue.

### Enrichissement state_5 (READ ONLY)

- Titres de threads, projet, liens parent/enfants (`thread_spawn_edges`)
- Classification d'activité : `main / subagent / auto_review / voice / other` — sans double comptage (chaque appel appartient à exactement une activité)
- Modèle des records legacy inféré depuis le modèle du thread (confiance `inferred`)
- `threads.tokens_used` affiché comme **indication lifetime uniquement**, jamais source de totaux

---

## Tests

```bash
cargo test -q        # 33 tests : parser, dédup, checkpoints, pricing, tiers, store, export
```

Fixtures couvrant : format moderne, legacy, dédup response_id, déplacement actif↔archivé, reprise après append, ligne partielle, record malformé, multi-modèles, cache write, long context (272 000 = court, 272 001 = long), sous-agents, Auto Review, modèle inconnu, service tier (fast, rétroactivité, fallback DB), idempotence des ré-imports, CSV parseable.

### Gate de non-régression

Sur les données réelles du 11–18 septembre 2026, `codex-meter summary` doit retrouver :

- Input ≈ 0,89 Md · Output ≈ 3,0 M · Cache hit ≈ 96 % · Équivalent ≈ $860
- et **ne pas** afficher ≈ 49,8 B tokens (somme des compteurs cumulatifs).

---

## Limites & périmètre

- **Mesuré** : Codex Desktop / CLI / IDE, agents et sous-agents, Auto Review, voix.
- **Non mesuré** : conversations ChatGPT (pas de source fiable locale).
- Les appels legacy de l'ancien format peuvent rester sans modèle (coût `N/A`, tokens comptés) et sans tier.
- Les tarifs sont des estimations issues de grilles publiques vérifiées à la date indiquée — à mettre à jour dans `pricing/` quand OpenAI publie de nouvelles grilles.

## Roadmap

- [x] Phase 1 — Core CLI (scan / summary / doctor) + gate
- [x] Phase 2 — SQLite + import incrémental + watcher + enrichissement state_5 + export CSV
- [x] Phase 3 — Service tier réel, multiplicateur fast, confiance tarifaire
- [x] Phase 4 — UI Tauri (dashboard, threads, diagnostics, export)
- [ ] Phase 5 — Temps réel dans l'UI (le watcher CLI existe déjà)
- [ ] Phase 6 — Connecteur API OpenAI (usage/costs administratifs, clé en Keychain, jamais en SQLite)
- [ ] Phase 7 — Packaging Linux / Windows
