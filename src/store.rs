use std::path::{Path, PathBuf};

use rusqlite::{params, Connection, OpenFlags};

use crate::model::{InferenceCall, SourceFormat, TokenUsage};
use crate::pricing::{CostBreakdown, PricingEngine};

pub const SCHEMA_VERSION: &str = "2";

/// Emplacement par defaut de la base (jamais dans ~/.codex).
pub fn default_db_path() -> PathBuf {
    let base = dirs::data_dir().unwrap_or_else(|| PathBuf::from("."));
    base.join("codex-meter").join("meter.sqlite")
}

pub fn open_db(path: &Path) -> Result<Connection, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let conn = Connection::open(path).map_err(|e| format!("open {}: {e}", path.display()))?;
    // UI + watch + CLI peuvent ouvrir la base en parallele : attendre le
    // verrou au lieu d'echouer « database is locked » au hasard.
    conn.busy_timeout(std::time::Duration::from_secs(5))
        .map_err(|e| e.to_string())?;
    conn.pragma_update(None, "journal_mode", "WAL")
        .map_err(|e| e.to_string())?;
    conn.pragma_update(None, "synchronous", "NORMAL")
        .map_err(|e| e.to_string())?;
    // FK actives : purge des legacy (voir clear_legacy_for_session) doit
    // supprimer aussi leurs pricing_results via ON DELETE CASCADE.
    conn.pragma_update(None, "foreign_keys", "ON")
        .map_err(|e| e.to_string())?;
    Ok(conn)
}

pub fn init_schema(conn: &Connection) -> Result<(), String> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS source_files (
            path TEXT PRIMARY KEY,
            archived INTEGER NOT NULL,
            size INTEGER NOT NULL,
            mtime_unix INTEGER,
            last_complete_offset INTEGER NOT NULL DEFAULT 0,
            lines_total INTEGER NOT NULL DEFAULT 0,
            had_primary INTEGER NOT NULL DEFAULT 0,
            pending_newline INTEGER NOT NULL DEFAULT 0,
            enrichment_json TEXT,
            last_imported_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS inference_calls (
            event_uid TEXT PRIMARY KEY,
            response_id TEXT UNIQUE,
            timestamp_utc TEXT,
            day TEXT NOT NULL DEFAULT 'unknown',
            session_id TEXT,
            thread_id TEXT,
            turn_id TEXT,
            root_turn_id TEXT,
            model_slug TEXT,
            model_confidence TEXT NOT NULL,
            service_tier TEXT NOT NULL DEFAULT 'unknown',
            project_path TEXT,
            activity TEXT,
            parent_thread_id TEXT,
            thread_title TEXT,
            input_tokens INTEGER NOT NULL,
            cached_input_tokens INTEGER NOT NULL,
            cache_write_input_tokens INTEGER NOT NULL,
            output_tokens INTEGER NOT NULL,
            reasoning_output_tokens INTEGER NOT NULL,
            total_tokens INTEGER NOT NULL,
            source_format TEXT NOT NULL,
            source_file TEXT,
            source_ordinal INTEGER NOT NULL,
            archived INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL
        );
        CREATE INDEX IF NOT EXISTS idx_calls_day ON inference_calls(day);
        CREATE INDEX IF NOT EXISTS idx_calls_model ON inference_calls(model_slug);
        CREATE INDEX IF NOT EXISTS idx_calls_thread ON inference_calls(thread_id);
        CREATE INDEX IF NOT EXISTS idx_calls_activity ON inference_calls(activity);

        CREATE TABLE IF NOT EXISTS thread_settings (
            thread_id TEXT PRIMARY KEY,
            service_tier TEXT NOT NULL,
            applied_ts TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS primary_sessions (
            session_id TEXT PRIMARY KEY,
            marked_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS threads (
            thread_id TEXT PRIMARY KEY,
            title TEXT,
            model TEXT,
            cwd TEXT,
            project_name TEXT,
            thread_source TEXT,
            activity TEXT,
            parent_thread_id TEXT,
            created_at_ms INTEGER,
            updated_at_ms INTEGER,
            state_tokens_used INTEGER,
            rollout_path TEXT
        );

        CREATE TABLE IF NOT EXISTS pricing_rules (
            catalog TEXT NOT NULL,
            version TEXT NOT NULL,
            profile TEXT NOT NULL,
            source_url TEXT NOT NULL,
            last_verified TEXT NOT NULL,
            rules_json TEXT NOT NULL,
            stored_at TEXT NOT NULL,
            PRIMARY KEY (catalog, version)
        );

        CREATE TABLE IF NOT EXISTS pricing_results (
            event_uid TEXT PRIMARY KEY REFERENCES inference_calls(event_uid) ON DELETE CASCADE,
            catalog_version TEXT NOT NULL,
            profile TEXT NOT NULL,
            equivalent_cost REAL NOT NULL,
            cost_without_cache REAL NOT NULL,
            cache_savings REAL NOT NULL,
            long_context INTEGER NOT NULL,
            model_known INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS import_run (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            started_at TEXT NOT NULL,
            finished_at TEXT NOT NULL,
            files_seen INTEGER NOT NULL,
            files_skipped INTEGER NOT NULL,
            files_changed INTEGER NOT NULL,
            calls_inserted INTEGER NOT NULL,
            duplicates_ignored INTEGER NOT NULL,
            parse_errors INTEGER NOT NULL,
            threads_enriched INTEGER NOT NULL,
            repriced INTEGER NOT NULL,
            duration_ms INTEGER NOT NULL
        );

        CREATE TABLE IF NOT EXISTS import_errors (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            run_id INTEGER NOT NULL,
            source_file TEXT NOT NULL,
            message TEXT NOT NULL
        );
        "#,
    )
    .map_err(|e| format!("schema: {e}"))?;
    // Migration légère : colonne service_tier sur les bases Phase 2.
    let has_tier: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('inference_calls')
             WHERE name = 'service_tier'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .map(|n| n > 0)
        .unwrap_or(true);
    if !has_tier {
        conn.execute("ALTER TABLE inference_calls ADD COLUMN service_tier TEXT NOT NULL DEFAULT 'unknown'", [])
            .map_err(|e| format!("migration service_tier: {e}"))?;
    }
    // Migration des colonnes de continuite des checkpoints (bases Phase 2/3).
    for (table, col, ddl) in [
        (
            "source_files",
            "had_primary",
            "ALTER TABLE source_files ADD COLUMN had_primary INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "source_files",
            "pending_newline",
            "ALTER TABLE source_files ADD COLUMN pending_newline INTEGER NOT NULL DEFAULT 0",
        ),
        (
            "source_files",
            "enrichment_json",
            "ALTER TABLE source_files ADD COLUMN enrichment_json TEXT",
        ),
    ] {
        let has_col: bool = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info(?1) WHERE name = ?2",
                params![table, col],
                |row| row.get::<_, i64>(0),
            )
            .map(|n| n > 0)
            .unwrap_or(true);
        if !has_col {
            conn.execute(ddl, []).map_err(|e| format!("migration {table}.{col}: {e}"))?;
        }
    }

    conn.execute(
        "INSERT INTO meta(key, value) VALUES('schema_version', ?1)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![SCHEMA_VERSION],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

#[derive(Debug, Clone, Default)]
pub struct Checkpoint {
    pub size: u64,
    pub mtime_unix: Option<i64>,
    pub last_complete_offset: u64,
    pub lines_total: u64,
    pub had_primary: bool,
    pub pending_newline: bool,
    pub enrichment_json: Option<String>,
}

pub fn get_checkpoint(conn: &Connection, rel_path: &str) -> Option<Checkpoint> {
    conn.query_row(
        "SELECT size, mtime_unix, last_complete_offset, lines_total,
                had_primary, pending_newline, enrichment_json
         FROM source_files WHERE path = ?1",
        params![rel_path],
        |row| {
            Ok(Checkpoint {
                size: row.get::<_, i64>(0)? as u64,
                mtime_unix: row.get(1)?,
                last_complete_offset: row.get::<_, i64>(2)? as u64,
                lines_total: row.get::<_, i64>(3)? as u64,
                had_primary: row.get::<_, i64>(4)? != 0,
                pending_newline: row.get::<_, i64>(5)? != 0,
                enrichment_json: row.get(6)?,
            })
        },
    )
    .ok()
}

pub fn save_checkpoint(
    conn: &Connection,
    rel_path: &str,
    archived: bool,
    size: u64,
    mtime_unix: Option<i64>,
    last_complete_offset: u64,
    lines_total: u64,
    had_primary: bool,
    pending_newline: bool,
    enrichment_json: &str,
) -> Result<(), String> {
    conn.execute(
        "INSERT INTO source_files(path, archived, size, mtime_unix, last_complete_offset,
                                  lines_total, had_primary, pending_newline, enrichment_json,
                                  last_imported_at)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)
         ON CONFLICT(path) DO UPDATE SET
            archived = excluded.archived,
            size = excluded.size,
            mtime_unix = excluded.mtime_unix,
            last_complete_offset = excluded.last_complete_offset,
            lines_total = excluded.lines_total,
            had_primary = excluded.had_primary,
            pending_newline = excluded.pending_newline,
            enrichment_json = excluded.enrichment_json,
            last_imported_at = excluded.last_imported_at",
        params![
            rel_path,
            archived as i64,
            size as i64,
            mtime_unix,
            last_complete_offset as i64,
            lines_total as i64,
            had_primary as i64,
            pending_newline as i64,
            enrichment_json,
            crate::util::iso_utc(crate::util::now_unix())
        ],
    )
    .map_err(|e| format!("checkpoint {rel_path}: {e}"))?;
    Ok(())
}

/// Insere un appel (dedup par event_uid PK et response_id UNIQUE).
/// Retourne (nouveau, tier_repare).
pub fn insert_call(conn: &Connection, call: &InferenceCall) -> Result<(bool, bool), String> {
    let created_at = crate::util::iso_utc(crate::util::now_unix());
    conn.execute(
        "INSERT OR IGNORE INTO inference_calls(
            event_uid, response_id, timestamp_utc, day,
            session_id, thread_id, turn_id, root_turn_id,
            model_slug, model_confidence, service_tier, project_path,
            activity, parent_thread_id, thread_title,
            input_tokens, cached_input_tokens, cache_write_input_tokens,
            output_tokens, reasoning_output_tokens, total_tokens,
            source_format, source_file, source_ordinal, archived, created_at
        ) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
                 ?15, ?16, ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26)",
        params![
            call.event_uid,
            call.response_id,
            call.timestamp_utc,
            call.day(),
            call.session_id,
            call.thread_id,
            call.turn_id,
            call.root_turn_id,
            call.model_slug,
            call.model_confidence.as_str(),
            call.service_tier.as_str(),
            call.project_path,
            call.activity,
            call.parent_thread_id,
            call.thread_title,
            call.usage.input_tokens as i64,
            call.usage.cached_input_tokens as i64,
            call.usage.cache_write_input_tokens as i64,
            call.usage.output_tokens as i64,
            call.usage.reasoning_output_tokens as i64,
            call.usage.total_tokens as i64,
            call.source_format.as_str(),
            call.source_file,
            call.source_ordinal as i64,
            call.archived as i64,
            created_at
        ],
    )
    .map_err(|e| format!("insert call: {e}"))?;
    let is_new = conn.changes() > 0;
    let mut tier_repaired = false;
    if !is_new && call.service_tier != crate::model::ServiceTier::Unknown {
        // Call deja present : completer le tier si la base avait Unknown.
        conn.execute(
            "UPDATE inference_calls SET service_tier = ?2
             WHERE event_uid = ?1
               AND service_tier IN ('unknown', '')",
            params![call.event_uid, call.service_tier.as_str()],
        )
        .map_err(|e| format!("update tier: {e}"))?;
        tier_repaired = conn.changes() > 0;
    }
    Ok((is_new, tier_repaired))
}

/// Upsert du dernier service tier connu d'un thread. Remplace seulement
/// si l'event est plus recent ou egal (les fichiers sont chronologiques).
pub fn upsert_thread_settings(
    conn: &Connection,
    thread_id: &str,
    tier: &str,
    applied_ts: &str,
) -> Result<(), String> {
    let prev_ts: Option<String> = conn
        .query_row(
            "SELECT applied_ts FROM thread_settings WHERE thread_id = ?1",
            params![thread_id],
            |row| row.get(0),
        )
        .ok();
    let replace = match &prev_ts {
        Some(prev) => applied_ts >= prev.as_str(),
        None => true,
    };
    if replace {
        conn.execute(
            "INSERT OR REPLACE INTO thread_settings(thread_id, service_tier, applied_ts)
             VALUES(?1, ?2, ?3)",
            params![thread_id, tier, applied_ts],
        )
        .map_err(|e| format!("upsert settings: {e}"))?;
    }
    Ok(())
}

/// Marque une session comme couverte par le format moderne : a partir de
/// la, les events legacy de cette session ne sont JAMAIS comptes (invariant
/// 7 : jamais melanger les deux methodes pour une meme session).
pub fn mark_primary_session(conn: &Connection, session_id: &str) -> Result<(), String> {
    conn.execute(
        "INSERT OR IGNORE INTO primary_sessions(session_id, marked_at) VALUES(?1, ?2)",
        params![session_id, crate::util::iso_utc(crate::util::now_unix())],
    )
    .map_err(|e| format!("mark primary session: {e}"))?;
    Ok(())
}

pub fn is_primary_session(conn: &Connection, session_id: &str) -> bool {
    conn.query_row(
        "SELECT 1 FROM primary_sessions WHERE session_id = ?1",
        params![session_id],
        |_| Ok(()),
    )
    .is_ok()
}

/// Supprime les appels legacy deja inseres pour une session qui vient d'etre
/// couverte par le format moderne. Retourne le nombre de lignes purgees
/// (leurs pricing_results partent en CASCADE).
pub fn clear_legacy_for_session(conn: &Connection, session_id: &str) -> Result<u64, String> {
    let n = conn
        .execute(
            "DELETE FROM inference_calls
             WHERE source_format = 'legacy_token_count' AND session_id = ?1",
            params![session_id],
        )
        .map_err(|e| format!("clear legacy {session_id}: {e}"))?;
    Ok(n as u64)
}

/// Rattrapage idempotent : indexe comme primaire les sessions deja en base
/// au format moderne (fichiers incrementaux sautes depuis l'ajout de
/// primary_sessions), purge leurs lignes residuelles — y compris les events
/// legacy `session_id NULL` ajoutes par un segment incremental sans
/// session_meta dans les fichiers qui portent du primary — pour que la
/// base rejoigne la regle globale du scan sans exiger `import --full`.
pub fn backfill_primary_sessions(conn: &Connection) -> Result<u64, String> {
    conn.execute(
        "INSERT OR IGNORE INTO primary_sessions(session_id, marked_at)
         SELECT DISTINCT session_id, ?1 FROM inference_calls
         WHERE source_format = 'token_usage_record'
           AND session_id IS NOT NULL AND session_id != ''",
        params![crate::util::iso_utc(crate::util::now_unix())],
    )
    .map_err(|e| format!("backfill primary sessions: {e}"))?;
    conn.execute(
        "UPDATE source_files SET had_primary = 1
         WHERE had_primary = 0
           AND path IN (
             SELECT DISTINCT source_file FROM inference_calls
             WHERE source_format = 'token_usage_record'
               AND source_file IS NOT NULL AND source_file != ''
           )",
        [],
    )
    .map_err(|e| format!("backfill had_primary: {e}"))?;
    conn.execute(
        "DELETE FROM inference_calls
         WHERE source_format = 'legacy_token_count'
           AND (session_id IN (SELECT session_id FROM primary_sessions)
                OR source_file IN (
                  SELECT path FROM source_files WHERE had_primary = 1
                ))",
        [],
    )
    .map_err(|e| format!("backfill prune legacy: {e}"))
    .map(|n| n as u64)
}

/// Dernier tier connu en base pour un thread (fallback inter-fichiers).
pub fn get_thread_tier(conn: &Connection, thread_id: &str) -> Option<(String, String)> {
    conn.query_row(
        "SELECT service_tier, applied_ts FROM thread_settings WHERE thread_id = ?1",
        params![thread_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .ok()
}

pub fn insert_pricing_result(
    conn: &Connection,
    event_uid: &str,
    cd: &CostBreakdown,
    catalog_version: &str,
) -> Result<(), String> {
    conn.execute(
        "INSERT OR REPLACE INTO pricing_results(
            event_uid, catalog_version, profile,
            equivalent_cost, cost_without_cache, cache_savings,
            long_context, model_known
        ) VALUES(?1, ?2, 'codex', ?3, ?4, ?5, ?6, ?7)",
        params![
            event_uid,
            catalog_version,
            cd.equivalent_cost,
            cd.cost_without_cache,
            cd.cache_savings,
            cd.long_context as i64,
            cd.model_known as i64
        ],
    )
    .map_err(|e| format!("insert pricing result: {e}"))?;
    Ok(())
}

pub fn get_meta(conn: &Connection, key: &str) -> Option<String> {
    conn.query_row(
        "SELECT value FROM meta WHERE key = ?1",
        params![key],
        |row| row.get(0),
    )
    .ok()
}

pub fn set_meta(conn: &Connection, key: &str, value: &str) -> Result<(), String> {
    conn.execute(
        "INSERT INTO meta(key, value) VALUES(?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
        params![key, value],
    )
    .map_err(|e| e.to_string())?;
    Ok(())
}

pub fn store_pricing_rules(conn: &Connection, engine: &PricingEngine) -> Result<(), String> {
    let now = crate::util::iso_utc(crate::util::now_unix());
    for cat in [&engine.codex, &engine.api] {
        let rules_json = serde_json::to_string(&cat.rules).map_err(|e| e.to_string())?;
        conn.execute(
            "INSERT OR REPLACE INTO pricing_rules(catalog, version, profile, source_url, last_verified, rules_json, stored_at)
             VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![cat.catalog, cat.version, cat.profile, cat.source_url, cat.last_verified, rules_json, now],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub fn reprice_all(conn: &Connection, engine: &PricingEngine) -> Result<u64, String> {
    conn.execute("DELETE FROM pricing_results", [])
        .map_err(|e| e.to_string())?;
    let mut stmt = conn
        .prepare(
            "SELECT event_uid, model_slug, day, service_tier, input_tokens,
                    cached_input_tokens, cache_write_input_tokens, output_tokens,
                    reasoning_output_tokens, total_tokens, model_confidence
             FROM inference_calls",
        )
        .map_err(|e| e.to_string())?;
    let rows: Vec<(String, Option<String>, String, String, i64, i64, i64, i64, i64, i64, String)> = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
                row.get(10)?,
            ))
        })
        .map_err(|e| e.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| e.to_string())?;

    let mut priced = 0u64;
    for (event_uid, model_slug, day, tier, input, cached, cache_write, output, reasoning, total, conf) in
        rows
    {
        let call = InferenceCall {
            event_uid: event_uid.clone(),
            response_id: None,
            timestamp_utc: None,
            session_id: None,
            thread_id: None,
            turn_id: None,
            root_turn_id: None,
            model_slug,
            model_confidence: parse_confidence(&conf),
            service_tier: crate::model::ServiceTier::parse(&tier),
            project_path: None,
            activity: None,
            parent_thread_id: None,
            thread_title: None,
            usage: TokenUsage {
                input_tokens: input as u64,
                cached_input_tokens: cached as u64,
                cache_write_input_tokens: cache_write as u64,
                output_tokens: output as u64,
                reasoning_output_tokens: reasoning as u64,
                total_tokens: total as u64,
            },
            source_format: SourceFormat::TokenUsageRecord,
            source_file: String::new(),
            source_ordinal: 0,
            archived: false,
        };
        // day() retombe sur "unknown" sans timestamp : on reutilise le jour
        // stocke pour la resolution de regle tarifaire.
        let mut c = call.clone();
        c.timestamp_utc = Some(format!("{day}T00:00:00Z"));
        let cd = engine.cost_codex(&c);
        if cd.model_known {
            insert_pricing_result(conn, &event_uid, &cd, &engine.codex.version)?;
            priced += 1;
        }
    }
    Ok(priced)
}

fn parse_confidence(s: &str) -> crate::model::ModelConfidence {
    match s {
        "exact" => crate::model::ModelConfidence::Exact,
        "inferred" => crate::model::ModelConfidence::Inferred,
        _ => crate::model::ModelConfidence::Unknown,
    }
}

/// Charge tous les appels (enrichis) pour agregation. Volume maitrise :
/// quelques dizaines de milliers de lignes max.
pub fn load_calls(
    conn: &Connection,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<Vec<InferenceCall>, String> {
    let mut sql = String::from(
        "SELECT event_uid, response_id, timestamp_utc, day,
                session_id, thread_id, turn_id, root_turn_id,
                model_slug, model_confidence, service_tier, project_path,
                activity, parent_thread_id, thread_title,
                input_tokens, cached_input_tokens, cache_write_input_tokens,
                output_tokens, reasoning_output_tokens, total_tokens,
                source_format, source_file, source_ordinal, archived
         FROM inference_calls WHERE 1=1",
    );
    // Params positionnels construits dans l'ordre d'apparition : passer
    // ?1/?2 discrets exigeait les deux bornes meme quand une seule est
    // fournie (bug « Got 2, needed 1 » sur --from seul).
    let mut args: Vec<&str> = Vec::new();
    if let Some(f) = from {
        sql.push_str(" AND day >= ?");
        args.push(f);
    }
    if let Some(t) = to {
        sql.push_str(" AND day <= ?");
        args.push(t);
    }
    sql.push_str(" ORDER BY timestamp_utc, source_file, source_ordinal");
    let mut stmt = conn.prepare(&sql).map_err(|e| e.to_string())?;
    let map_row = |row: &rusqlite::Row| -> rusqlite::Result<InferenceCall> {
        Ok(InferenceCall {
            event_uid: row.get(0)?,
            response_id: row.get(1)?,
            timestamp_utc: row.get(2)?,
            session_id: row.get(4)?,
            thread_id: row.get(5)?,
            turn_id: row.get(6)?,
            root_turn_id: row.get(7)?,
            model_slug: row.get(8)?,
            model_confidence: parse_confidence(&row.get::<_, String>(9)?),
            service_tier: crate::model::ServiceTier::parse(&row.get::<_, String>(10)?),
            project_path: row.get(11)?,
            activity: row.get(12)?,
            parent_thread_id: row.get(13)?,
            thread_title: row.get(14)?,
            usage: TokenUsage {
                input_tokens: row.get::<_, i64>(15)? as u64,
                cached_input_tokens: row.get::<_, i64>(16)? as u64,
                cache_write_input_tokens: row.get::<_, i64>(17)? as u64,
                output_tokens: row.get::<_, i64>(18)? as u64,
                reasoning_output_tokens: row.get::<_, i64>(19)? as u64,
                total_tokens: row.get::<_, i64>(20)? as u64,
            },
            source_format: SourceFormat::parse(&row.get::<_, String>(21)?),
            source_file: row.get(22)?,
            source_ordinal: row.get::<_, i64>(23)? as u64,
            archived: row.get::<_, i64>(24)? != 0,
        })
    };
    let rows = stmt
        .query_map(rusqlite::params_from_iter(args.iter()), map_row)
        .map_err(|e| e.to_string())?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|e| e.to_string())?;
    Ok(rows)
}

pub fn db_totals(conn: &Connection) -> Result<(u64, TokenUsage), String> {
    conn.query_row(
        "SELECT COUNT(*),
                COALESCE(SUM(input_tokens),0), COALESCE(SUM(cached_input_tokens),0),
                COALESCE(SUM(cache_write_input_tokens),0), COALESCE(SUM(output_tokens),0),
                COALESCE(SUM(reasoning_output_tokens),0), COALESCE(SUM(total_tokens),0)
         FROM inference_calls",
        [],
        |row| {
            Ok((
                row.get::<_, i64>(0)? as u64,
                TokenUsage {
                    input_tokens: row.get::<_, i64>(1)? as u64,
                    cached_input_tokens: row.get::<_, i64>(2)? as u64,
                    cache_write_input_tokens: row.get::<_, i64>(3)? as u64,
                    output_tokens: row.get::<_, i64>(4)? as u64,
                    reasoning_output_tokens: row.get::<_, i64>(5)? as u64,
                    total_tokens: row.get::<_, i64>(6)? as u64,
                },
            ))
        },
    )
    .map_err(|e| e.to_string())
}

/// Ouvre state_5.sqlite en lecture stricte (jamais d'ecriture).
pub fn open_state_readonly(path: &Path) -> Result<Connection, String> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NO_MUTEX;
    match Connection::open_with_flags(path, flags) {
        Ok(c) => {
            let _ = c.busy_timeout(std::time::Duration::from_secs(2));
            Ok(c)
        }
        Err(_) => {
            // WAL actif sans shm accessible : lecture immutable en dernier recours.
            let uri = format!("file:{}?immutable=1", path.display());
            Connection::open_with_flags(uri.as_str(), flags | OpenFlags::SQLITE_OPEN_URI)
                .map_err(|e| format!("open state db readonly failed: {e}"))
        }
    }
}
