use std::path::PathBuf;

use serde::Serialize;

use codex_meter::{aggregate, discovery, export, import, pricing, store};

#[derive(Serialize)]
pub struct BucketDto {
    name: String,
    calls: u64,
    input: u64,
    cached: u64,
    ordinary: u64,
    cache_write: u64,
    output: u64,
    reasoning: u64,
    cache_hit_percent: Option<f64>,
    equivalent_cost: f64,
    cost_without_cache: f64,
    cache_savings: f64,
    unknown_model_tokens: u64,
    long_context_calls: u64,
}

fn bucket_dto(name: &str, b: &aggregate::Bucket) -> BucketDto {
    BucketDto {
        name: name.to_string(),
        calls: b.calls,
        input: b.usage.input_tokens,
        cached: b.usage.cached_input_tokens,
        ordinary: b.usage.ordinary_input(),
        cache_write: b.usage.cache_write_input_tokens,
        output: b.usage.output_tokens,
        reasoning: b.usage.reasoning_output_tokens,
        cache_hit_percent: b.cache_hit_percent(),
        equivalent_cost: b.equivalent_cost,
        cost_without_cache: b.cost_without_cache,
        cache_savings: b.cache_savings,
        unknown_model_tokens: b.unknown_model_tokens,
        long_context_calls: b.long_context_calls,
    }
}

fn to_dto(m: aggregate::Buckets) -> Vec<BucketDto> {
    m.iter().map(|(k, v)| bucket_dto(k, v)).collect()
}

#[derive(Serialize)]
pub struct SummaryDto {
    from: Option<String>,
    to: Option<String>,
    calls: u64,
    input: u64,
    cached: u64,
    ordinary: u64,
    cache_write: u64,
    output: u64,
    reasoning: u64,
    cache_hit_percent: Option<f64>,
    equivalent_cost: f64,
    cost_without_cache: f64,
    cache_savings: f64,
    cache_savings_percent: Option<f64>,
    pricing_confidence_percent: f64,
    tiers: (u64, u64, u64),
    by_day: Vec<BucketDto>,
    by_model: Vec<BucketDto>,
    by_project: Vec<BucketDto>,
    by_activity: Vec<BucketDto>,
    anomalies: Vec<String>,
}

#[tauri::command]
async fn get_summary(
    state: tauri::State<'_, AppState>,
    from: Option<String>,
    to: Option<String>,
) -> Result<SummaryDto, String> {
    let home = state.codex_home.clone();
    let db = state.db.clone();
    // Import + lecture SQLite bloquants : hors du worker async (sinon un
    // import long fige une cellule du runtime).
    tauri::async_runtime::spawn_blocking(move || {
        let engine = pricing::PricingEngine::embedded();
        import::import(&home, &db, &engine, false, false)?;
        let calls = import::load_calls(&db, from.as_deref(), to.as_deref())?;

        let totals = aggregate::usage_of(&calls);
        let mut total = aggregate::Bucket::default();
        for c in &calls {
            let cd = engine.cost_codex(c);
            total.add(c, cd.model_known.then_some(&cd));
        }
        let by_day = aggregate::by_day(&calls, &engine);
        let by_activity = aggregate::by_activity(&calls, &engine);
        let anomalies =
            aggregate::detect_anomalies(&calls, &by_day, &by_activity, total.long_context_calls);

        Ok(SummaryDto {
            from,
            to,
            calls: calls.len() as u64,
            input: totals.input_tokens,
            cached: totals.cached_input_tokens,
            ordinary: totals.ordinary_input(),
            cache_write: totals.cache_write_input_tokens,
            output: totals.output_tokens,
            reasoning: totals.reasoning_output_tokens,
            cache_hit_percent: total.cache_hit_percent(),
            equivalent_cost: total.equivalent_cost,
            cost_without_cache: total.cost_without_cache,
            cache_savings: total.cache_savings,
            cache_savings_percent: total.cache_savings_percent(),
            pricing_confidence_percent: pricing::pricing_confidence(&calls),
            tiers: pricing::tier_stats(&calls),
            by_day: to_dto(by_day),
            by_model: to_dto(aggregate::by_model(&calls, &engine)),
            by_project: to_dto(aggregate::by_project(&calls, &engine)),
            by_activity: to_dto(by_activity),
            anomalies,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Serialize)]
pub struct ThreadRow {
    thread_id: String,
    title: String,
    project: String,
    calls: u64,
    input: u64,
    output: u64,
    cache_hit_percent: Option<f64>,
    equivalent_cost: f64,
    activity: String,
}

#[tauri::command]
async fn get_threads(
    state: tauri::State<'_, AppState>,
    from: Option<String>,
    to: Option<String>,
    limit: Option<u32>,
) -> Result<Vec<ThreadRow>, String> {
    let home = state.codex_home.clone();
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let engine = pricing::PricingEngine::embedded();
        // Import incremental aussi ici : un premier affichage de l'onglet
        // Threads sans etre passe par le dashboard ne doit pas etre vide.
        import::import(&home, &db, &engine, false, false)?;
        let calls = import::load_calls(&db, from.as_deref(), to.as_deref())?;
        let buckets = aggregate::by_thread(&calls, &engine);
        let mut rows: Vec<ThreadRow> = buckets
            .iter()
            .map(|(id, b)| ThreadRow {
                thread_id: id.clone(),
                title: calls
                    .iter()
                    .find(|c| c.thread_id.as_deref() == Some(id.as_str()))
                    .and_then(|c| c.thread_title.clone())
                    .unwrap_or_default(),
                project: calls
                    .iter()
                    .find(|c| c.thread_id.as_deref() == Some(id.as_str()))
                    .and_then(|c| c.project_path.clone())
                    .map(|p| aggregate::project_name(&p))
                    .unwrap_or_default(),
                activity: calls
                    .iter()
                    .find(|c| c.thread_id.as_deref() == Some(id.as_str()))
                    .and_then(|c| c.activity.clone())
                    .unwrap_or_else(|| "unknown".into()),
                calls: b.calls,
                input: b.usage.input_tokens,
                output: b.usage.output_tokens,
                cache_hit_percent: b.cache_hit_percent(),
                equivalent_cost: b.equivalent_cost,
            })
            .collect();
        rows.sort_by(|a, b| b.input.cmp(&a.input));
        rows.truncate(limit.unwrap_or(25) as usize);
        Ok(rows)
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Serialize)]
pub struct DiagnosticsDto {
    codex_home: String,
    db_path: String,
    files_found: usize,
    files_parsed: usize,
    lines_total: u64,
    usage_records: u64,
    duplicates_ignored: u64,
    legacy_records: u64,
    parse_errors: Vec<String>,
    subset_violations: u64,
    db_calls: u64,
    db_input: u64,
    db_output: u64,
    consistent: bool,
    state_lifetime_tokens_used: u64,
    state_threads_by_activity: std::collections::BTreeMap<String, u64>,
    pricing_codex_version: String,
    pricing_api_version: String,
    pricing_last_verified: String,
}

#[tauri::command]
async fn get_diagnostics(state: tauri::State<'_, AppState>) -> Result<DiagnosticsDto, String> {
    let home = state.codex_home.clone();
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let fresh = codex_meter::scan::scan(&home);
        let d = &fresh.diagnostics;
        let conn = store::open_db(&db)?;
        store::init_schema(&conn)?;
        let (db_calls, db_totals) = store::db_totals(&conn)?;
        let state_threads =
            codex_meter::state::thread_activity_counts(&home.join("state_5.sqlite"))
                .unwrap_or_default();
        let state_tokens: i64 = conn
            .query_row(
                "SELECT COALESCE(SUM(state_tokens_used),0) FROM threads",
                [],
                |r| r.get(0),
            )
            .unwrap_or(0);
        let engine = pricing::PricingEngine::embedded();

        Ok(DiagnosticsDto {
            codex_home: home.display().to_string(),
            db_path: db.display().to_string(),
            files_found: d.files_found,
            files_parsed: d.files_parsed,
            lines_total: d.lines_total,
            usage_records: d.usage_records,
            duplicates_ignored: d.duplicates_ignored,
            legacy_records: d.legacy_records,
            parse_errors: d.parse_errors.iter().take(10).cloned().collect(),
            subset_violations: d.subset_violations,
            db_calls,
            db_input: db_totals.input_tokens,
            db_output: db_totals.output_tokens,
            consistent: db_totals == fresh.totals,
            state_lifetime_tokens_used: state_tokens.max(0) as u64,
            state_threads_by_activity: state_threads.into_iter().collect(),
            pricing_codex_version: engine.codex.version.clone(),
            pricing_api_version: engine.api.version.clone(),
            pricing_last_verified: engine.codex.last_verified.clone(),
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Serialize)]
pub struct ImportReportDto {
    files_seen: usize,
    files_skipped: usize,
    files_changed: usize,
    calls_inserted: u64,
    duplicates_ignored: u64,
    parse_errors: u64,
    duration_ms: u64,
}

#[tauri::command]
async fn refresh_import(state: tauri::State<'_, AppState>) -> Result<ImportReportDto, String> {
    let home = state.codex_home.clone();
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let engine = pricing::PricingEngine::embedded();
        let rep = import::import(&home, &db, &engine, false, false)?;
        Ok(ImportReportDto {
            files_seen: rep.files_seen,
            files_skipped: rep.files_skipped,
            files_changed: rep.files_changed,
            calls_inserted: rep.calls_inserted,
            duplicates_ignored: rep.duplicates_ignored,
            parse_errors: rep.parse_errors,
            duration_ms: rep.duration_ms,
        })
    })
    .await
    .map_err(|e| e.to_string())?
}

#[tauri::command]
async fn export_csv_files(
    state: tauri::State<'_, AppState>,
    dir: String,
    from: Option<String>,
    to: Option<String>,
) -> Result<Vec<String>, String> {
    let db = state.db.clone();
    tauri::async_runtime::spawn_blocking(move || {
        let engine = pricing::PricingEngine::embedded();
        let calls = import::load_calls(&db, from.as_deref(), to.as_deref())?;
        let files = export::export_csv(std::path::Path::new(&dir), &calls, &engine)?;
        Ok(files.iter().map(|p| p.display().to_string()).collect())
    })
    .await
    .map_err(|e| e.to_string())?
}

#[derive(Serialize)]
pub struct PricingInfoDto {
    catalog: String,
    profile: String,
    version: String,
    last_verified: String,
    source_url: String,
    models: Vec<String>,
}

#[tauri::command]
fn get_pricing() -> Result<Vec<PricingInfoDto>, String> {
    let engine = pricing::PricingEngine::embedded();
    Ok([&engine.codex, &engine.api]
        .iter()
        .map(|c| PricingInfoDto {
            catalog: c.catalog.clone(),
            profile: c.profile.clone(),
            version: c.version.clone(),
            last_verified: c.last_verified.clone(),
            source_url: c.source_url.clone(),
            models: c.rules.iter().map(|r| r.model.clone()).collect(),
        })
        .collect())
}

/// macOS + WKWebView : tao et WebKit se disputent le curseur au bord de
/// la fenetre (flicker fleche/resize). On fait accorder les deux : le web
/// signale les transitions de zone de bord, tao applique le curseur au
/// niveau AppKit, et le CSS du web affiche la meme forme.
#[tauri::command]
fn set_edge_cursor(window: tauri::WebviewWindow, cursor: Option<String>) -> Result<(), String> {
    use tauri::CursorIcon;
    let icon = match cursor.as_deref() {
        Some("ew-resize") => CursorIcon::EwResize,
        Some("ns-resize") => CursorIcon::NsResize,
        Some("nwse-resize") => CursorIcon::NwseResize,
        Some("nesw-resize") => CursorIcon::NeswResize,
        _ => CursorIcon::Default,
    };
    window.set_cursor_icon(icon).map_err(|e| e.to_string())
}

struct AppState {
    codex_home: PathBuf,
    db: PathBuf,
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let codex_home = discovery::codex_home(None);
    let db = store::default_db_path();
    tauri::Builder::default()
        .plugin(tauri_plugin_dialog::init())
        .plugin(tauri_plugin_opener::init())
        .manage(AppState { codex_home, db })
        .invoke_handler(tauri::generate_handler![
            get_summary,
            get_threads,
            set_edge_cursor,
            get_diagnostics,
            refresh_import,
            export_csv_files,
            get_pricing
        ])
        .run(tauri::generate_context!())
        .expect("error while running codex meter ui");
}
