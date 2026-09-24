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
    pub legacy_pruned: u64,
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

    // Rattrapage avant les fichiers : sessions modernes deja en base mais
    // jamais marquees (checkpoints sautes), + purge de leur legacy.
    report.legacy_pruned += store::backfill_primary_sessions(&conn)?;

    // --full = reconstruction garantie : on repart d'une base vide pour que
    // les event_uid (ordinaux) soient exactement ceux du scan, meme si un
    // ancien drift de checkpoint avait pu s'insinuer.
    if force_full {
        conn.execute_batch(
            "DELETE FROM inference_calls;
             DELETE FROM pricing_results;",
        )
        .map_err(|e| e.to_string())?;
    }

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
        let (start_offset, lines_before, resume_pending) = match &cp {
            Some(c) if c.last_complete_offset <= file.size => {
                (c.last_complete_offset, c.lines_total, c.pending_newline)
            }
            _ => (0, 0, false),
        };

        let parsed = parser::parse_file_from(&file.path, start_offset, lines_before, resume_pending);
        let mut res = parsed.result;

        // Continuite des segments : le checkpoint porte l'enrichissement des
        // lignes deja passees (session_meta, turn_context). Le segment
        // courant le complete ; sans cela, un record dont le turn_context
        // precde le checkpoint perdrait son modele et un legacy son session.
        let prior: crate::model::FileEnrichment = cp
            .as_ref()
            .and_then(|c| c.enrichment_json.as_deref())
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or_default();
        let mut enr = prior;
        if res.meta.session_id.is_some() {
            enr.meta = std::mem::take(&mut res.meta);
        }
        for (k, v) in std::mem::take(&mut res.turn_models) {
            enr.turn_models.insert(k, v);
        }
        for (k, v) in std::mem::take(&mut res.turn_cwd) {
            enr.turn_cwd.insert(k, v);
        }
        for (k, v) in std::mem::take(&mut res.root_models) {
            match enr.root_models.get(&k) {
                Some(prev) if prev != &v => {
                    enr.root_models.remove(&k);
                }
                _ => {
                    enr.root_models.insert(k, v);
                }
            }
        }
        let enrichment_json =
            serde_json::to_string(&enr).map_err(|e| format!("enrichment {rel}: {e}"))?;

        // BEGIN IMMEDIATE : verrou d'ecriture des le depart, compatible avec
        // le busy_timeout (un BEGIN differe peut mourir en BUSY a la montee
        // de verrou sans jamais reessayer).
        conn.execute_batch("BEGIN IMMEDIATE")
            .map_err(|e| e.to_string())?;
        let mut inserted_in_file = 0u64;
        let mut dup_in_file = 0u64;
        let mut tiers_repaired = 0u64;

        // Le format moderne est un fait de FICHIER, pas du segment : un
        // token_count appendé dans un fichier deja primaire n'est jamais un
        // supplément de comptabilite (invariant 7).
        let had_primary_before = cp.as_ref().map(|c| c.had_primary).unwrap_or(false);
        let primary = had_primary_before || res.primary_events > 0;
        // Invariant 7 a l'echelle de la base : des qu'une session est couverte
        // par le format moderne, ses events legacy sont ignores (et les lignes
        // legacy deja inserees pour cette session sont purgees).
        if primary {
            let mut sessions: Vec<&str> = Vec::new();
            if let Some(s) = enr.meta.session_id.as_deref() {
                sessions.push(s);
            }
            for ev in &res.raw_calls {
                if ev.source_format == crate::model::SourceFormat::TokenUsageRecord {
                    if let Some(s) = ev.session_id.as_deref() {
                        if !sessions.contains(&s) {
                            sessions.push(s);
                        }
                    }
                }
            }
            for s in sessions {
                store::mark_primary_session(&conn, s)?;
                report.legacy_pruned += store::clear_legacy_for_session(&conn, s)?;
            }
        }
        // Les settings du fichier alimentent d'abord la base (fallback
        // inter-fichiers pour les imports incrementaux suivants).
        for (tid, ts, tier) in &res.thread_settings {
            store::upsert_thread_settings(&conn, tid, tier, ts)?;
        }
        for event in &res.raw_calls {
            if event.source_format == crate::model::SourceFormat::LegacyTokenCount {
                if primary {
                    continue;
                }
                // Session deja primaire via un AUTRE fichier : ne jamais
                // recompter l'ancienne methode pour cette session.
                let covered = event
                    .session_id
                    .as_deref()
                    .is_some_and(|s| store::is_primary_session(&conn, s));
                if covered {
                    continue;
                }
            }
            let mut call = parser::finalize_call(
                event,
                &enr.meta,
                &enr.turn_models,
                &enr.turn_cwd,
                &enr.root_models,
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
            primary,
            parsed.pending_newline,
            &enrichment_json,
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
