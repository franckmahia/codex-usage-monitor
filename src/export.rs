use std::io::Write;
use std::path::Path;

use crate::aggregate::{self, Bucket, Buckets};
use crate::model::InferenceCall;
use crate::pricing::PricingEngine;

fn write_csv<W: Write>(w: &mut W, label: &str, rows: &[(&str, &Bucket)]) -> Result<(), String> {
    let mut writer = csv::Writer::from_writer(w);
    writer
        .write_record([
            "bucket",
            "calls",
            "input_tokens",
            "cached_input_tokens",
            "cache_write_input_tokens",
            "ordinary_input_tokens",
            "output_tokens",
            "reasoning_output_tokens",
            "cache_hit_percent",
            "equivalent_cost_codex",
            "cost_without_cache",
            "cache_savings",
            "cache_savings_percent",
            "unknown_model_tokens",
            "long_context_calls",
        ])
        .map_err(|e| e.to_string())?;
    for (name, b) in rows {
        writer
            .write_record([
                &format!("{label}:{name}"),
                &b.calls.to_string(),
                &b.usage.input_tokens.to_string(),
                &b.usage.cached_input_tokens.to_string(),
                &b.usage.cache_write_input_tokens.to_string(),
                &b.usage.ordinary_input().to_string(),
                &b.usage.output_tokens.to_string(),
                &b.usage.reasoning_output_tokens.to_string(),
                &b.cache_hit_percent().map(|p| format!("{p:.2}")).unwrap_or_default(),
                &format!("{:.6}", b.equivalent_cost),
                &format!("{:.6}", b.cost_without_cache),
                &format!("{:.6}", b.cache_savings),
                &b.cache_savings_percent().map(|p| format!("{p:.2}")).unwrap_or_default(),
                &b.unknown_model_tokens.to_string(),
                &b.long_context_calls.to_string(),
            ])
            .map_err(|e| e.to_string())?;
    }
    writer.flush().map_err(|e| e.to_string())?;
    Ok(())
}

fn bucket_rows(b: &Buckets) -> Vec<(String, &Bucket)> {
    b.iter().map(|(k, v)| (k.clone(), v)).collect()
}

/// Exporte les agregats normalises en CSV dans un dossier.
/// Retourne la liste des fichiers ecrits.
pub fn export_csv(
    dir: &Path,
    calls: &[InferenceCall],
    engine: &PricingEngine,
) -> Result<Vec<std::path::PathBuf>, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let mut written = Vec::new();

    let mut dump = |name: &str, buckets: &Buckets| -> Result<(), String> {
        let path = dir.join(name);
        let f = std::fs::File::create(&path).map_err(|e| format!("create {}: {e}", path.display()))?;
        let rows = bucket_rows(buckets);
        let refs: Vec<(&str, &Bucket)> = rows.iter().map(|(k, b)| (k.as_str(), *b)).collect();
        let mut w = std::io::BufWriter::new(f);
        write_csv(&mut w, name.trim_end_matches(".csv"), &refs)?;
        written.push(path);
        Ok(())
    };

    dump("by-day.csv", &aggregate::by_day(calls, engine))?;
    dump("by-model.csv", &aggregate::by_model(calls, engine))?;
    dump("by-project.csv", &aggregate::by_project(calls, engine))?;
    dump("by-activity.csv", &aggregate::by_activity(calls, engine))?;
    dump("by-thread.csv", &aggregate::by_thread(calls, engine))?;

    let calls_path = dir.join("calls.csv");
    let f = std::fs::File::create(&calls_path).map_err(|e| e.to_string())?;
    let mut writer = csv::Writer::from_writer(std::io::BufWriter::new(f));
    writer
        .write_record([
            "event_uid", "response_id", "timestamp_utc", "day", "session_id", "thread_id",
            "thread_title", "turn_id", "root_turn_id", "model_slug", "model_confidence",
            "activity", "parent_thread_id", "project_path", "input_tokens",
            "cached_input_tokens", "cache_write_input_tokens", "output_tokens",
            "reasoning_output_tokens", "total_tokens", "source_format", "source_file",
            "source_ordinal", "archived", "equivalent_cost_codex", "long_context",
            "model_known",
        ])
        .map_err(|e| e.to_string())?;
    for c in calls {
        let cd = engine.cost_codex(c);
        writer
            .write_record([
                &c.event_uid,
                c.response_id.as_deref().unwrap_or(""),
                c.timestamp_utc.as_deref().unwrap_or(""),
                c.day(),
                c.session_id.as_deref().unwrap_or(""),
                c.thread_id.as_deref().unwrap_or(""),
                c.thread_title.as_deref().unwrap_or(""),
                c.turn_id.as_deref().unwrap_or(""),
                c.root_turn_id.as_deref().unwrap_or(""),
                c.model_slug.as_deref().unwrap_or(""),
                &format!("{:?}", c.model_confidence).to_lowercase(),
                c.activity.as_deref().unwrap_or(""),
                c.parent_thread_id.as_deref().unwrap_or(""),
                c.project_path.as_deref().unwrap_or(""),
                &c.usage.input_tokens.to_string(),
                &c.usage.cached_input_tokens.to_string(),
                &c.usage.cache_write_input_tokens.to_string(),
                &c.usage.output_tokens.to_string(),
                &c.usage.reasoning_output_tokens.to_string(),
                &c.usage.total_tokens.to_string(),
                &format!("{:?}", c.source_format).to_lowercase(),
                &c.source_file,
                &c.source_ordinal.to_string(),
                &(c.archived as u8).to_string(),
                &format!("{:.6}", cd.equivalent_cost),
                &(cd.long_context as u8).to_string(),
                &(cd.model_known as u8).to_string(),
            ])
            .map_err(|e| e.to_string())?;
    }
    writer.flush().map_err(|e| e.to_string())?;
    written.push(calls_path);
    Ok(written)
}
