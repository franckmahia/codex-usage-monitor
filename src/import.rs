use std::path::Path;
use std::time::Instant;

use rusqlite::params;

use crate::discovery;
use crate::parser;
use crate::pricing::PricingEngine;
use crate::state;
use crate::store::{self, Checkpoint};

#[derive(Debug, Default, Clone)]
pub struct ImportReport {
    pub files_seen: usize,
    pub files_skipped: usize,
    pub files_changed: usize,
    pub bytes_read: u64,
    pub calls_inserted: u64,
    pub duplicates_ignored: u64,
    pub parse_errors: u64,
    pub threads_enriched: usize,
    pub calls_enriched: u64,
    pub repriced: bool,
    pub priced_calls: u64,
    pub tiers_repaired: u64,
    pub duration_ms: u64,
}

fn pricing_fingerprint(engine: &PricingEngine) -> String {
    format!(
        "codex:{}|api:{}",
        engine.codex.version, engine.api.version
    )
}

/// Import incremental : decouverte -> parse depuis checkpoint -> upsert dedup
/// -> enrichissement state db -> re-pricing si le catalogue a change.
pub fn import(
    home: &Path,
    db_path: &Path,
    engine: &PricingEngine,
    force_full: bool,
    force_reprice: bool,
) -> Result<ImportReport, String> {
    let started = Instant::now();
    let conn = store::open_db(db_path)?;
    store::init_schema(&conn)?;

    let mut report = ImportReport::default();
    let run_started = crate::util::iso_utc(crate::util::now_unix());

    let files = discovery::discover(home);
    report.files_seen = files.len();

    for file in &files {
        let rel = file
            .path
            .strip_prefix(home)
            .map(|p| p.display().to_string())
            .unwrap_or_else(|_| file.path.display().to_string());

        if !force_full {
            if let Some(cp) = store::get_checkpoint(&conn, &rel) {
                if cp.size == file.size && cp.mtime_unix == file.mtime_unix {
                    report.files_skipped += 1;
                    continue;
                }
            }
        }

        // Fichier retreci ou import forcé : on repart de zero.
        let cp: Option<Checkpoint> = if force_full {
            None
        } else {
            store::get_checkpoint(&conn, &rel)
        };
        let (start_offset, lines_before) = match &cp {
            Some(c) if c.last_complete_offset <= file.size => {
                (c.last_complete_offset, c.lines_total)
            }
            _ => (0, 0),
        };

        let parsed = parser::parse_file_from(&file.path, start_offset, lines_before);
        let res = parsed.result;

        conn.execute_batch("BEGIN").map_err(|e| e.to_string())?;
        let mut inserted_in_file = 0u64;
        let mut dup_in_file = 0u64;
        let mut tiers_repaired = 0u64;

        let primary = res.primary_events > 0;
        // Les settings du fichier alimentent d'abord la base (fallback
        // inter-fichiers pour les imports incrementaux suivants).
        for (tid, ts, tier) in &res.thread_settings {
            store::upsert_thread_settings(&conn, tid, tier, ts)?;
        }
        for event in &res.raw_calls {
            if primary && event.source_format == crate::model::SourceFormat::LegacyTokenCount {
                continue;
            }
            let mut call = parser::finalize_call(
                event,
                &res.meta,
                &res.turn_models,
                &res.turn_cwd,
                &res.root_models,
                &res.thread_settings,
                &rel,
                file.archived,
            );
            if call.service_tier == crate::model::ServiceTier::Unknown {
                // Fallback DB : settings appliques avant le checkpoint courant.
                if let Some(tid) = event.thread_id.as_deref() {
                    if let Some((tier, applied_ts)) = store::get_thread_tier(&conn, tid) {
                        let applicable = event
                            .timestamp_utc
                            .as_deref()
                            .map_or(true, |ts| ts >= applied_ts.as_str());
                        if applicable {
                            call.service_tier = crate::model::ServiceTier::parse(&tier);
                        }
                    }
                }
            }
            let (is_new, tier_repaired) = store::insert_call(&conn, &call)?;
            if tier_repaired {
                tiers_repaired += 1;
            }
            if is_new {
                inserted_in_file += 1;
                let cd = engine.cost_codex(&call);
                if cd.model_known {
                    store::insert_pricing_result(&conn, &call.event_uid, &cd, &engine.codex.version)?;
                }
            } else {
                dup_in_file += 1;
            }
        }

        store::save_checkpoint(
            &conn,
            &rel,
            file.archived,
            file.size,
            file.mtime_unix,
            parsed.end_offset,
            lines_before + res.counters.lines_total,
        )?;
        conn.execute_batch("COMMIT").map_err(|e| e.to_string())?;

        report.tiers_repaired += tiers_repaired;
        report.files_changed += 1;
        report.calls_inserted += inserted_in_file;
        report.duplicates_ignored += dup_in_file;
        report.bytes_read += parsed.end_offset - start_offset;
        report.parse_errors += res.errors.len() as u64;
        for e in res.errors.iter().take(20) {
            let _ = conn.execute(
                "INSERT INTO import_errors(run_id, source_file, message) VALUES(0, ?1, ?2)",
                params![rel, e],
            );
        }
    }

    // Enrichissement state_5.sqlite (lecture seule cote state).
    let (threads_enriched, calls_enriched) =
        state::enrich_calls(&conn, &home.join("state_5.sqlite"))?;
    report.threads_enriched = threads_enriched;
    report.calls_enriched = calls_enriched;

    // Re-pricing si le catalogue a change ou si des tiers ont ete repares
    // (le cout fast depend du tier).
    let fp = pricing_fingerprint(engine);
    if force_reprice
        || report.tiers_repaired > 0
        || store::get_meta(&conn, "pricing_fingerprint").as_deref() != Some(fp.as_str())
    {
        report.repriced = true;
        report.priced_calls = store::reprice_all(&conn, engine)?;
        store::set_meta(&conn, "pricing_fingerprint", &fp)?;
        store::store_pricing_rules(&conn, engine)?;
    }

    report.duration_ms = started.elapsed().as_millis() as u64;
    conn.execute(
        "INSERT INTO import_run(started_at, finished_at, files_seen, files_skipped,
             files_changed, calls_inserted, duplicates_ignored, parse_errors,
             threads_enriched, repriced, duration_ms)
         VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
        params![
            run_started,
            crate::util::iso_utc(crate::util::now_unix()),
            report.files_seen as i64,
            report.files_skipped as i64,
            report.files_changed as i64,
            report.calls_inserted as i64,
            report.duplicates_ignored as i64,
            report.parse_errors as i64,
            report.threads_enriched as i64,
            report.repriced as i64,
            report.duration_ms as i64
        ],
    )
    .map_err(|e| e.to_string())?;

    Ok(report)
}

/// Charge les appels de la base pour agregation (avec filtre de periode).
pub fn load_calls(
    db_path: &Path,
    from: Option<&str>,
    to: Option<&str>,
) -> Result<Vec<crate::model::InferenceCall>, String> {
    let conn = store::open_db(db_path)?;
    store::init_schema(&conn)?;
    store::load_calls(&conn, from, to)
}
