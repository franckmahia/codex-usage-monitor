use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use codex_meter::{aggregate, discovery, export, import, pricing::PricingEngine, scan, store, watch};

#[derive(Parser)]
#[command(
    name = "codex-meter",
    about = "Compteur de tokens Codex local et auditable. Lecture seule des logs, hors ligne.",
    version
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    /// Chemin du codex home (defaut: $CODEX_HOME sinon ~/.codex)
    #[arg(long, global = true)]
    codex_home: Option<PathBuf>,
    /// Chemin de la base SQLite (defaut: dossier data utilisateur)
    #[arg(long, global = true)]
    db: Option<PathBuf>,
    /// Date debut YYYY-MM-DD (UTC, inclusive, sur le timestamp des records)
    #[arg(long, global = true)]
    from: Option<String>,
    /// Date fin YYYY-MM-DD (UTC, inclusive)
    #[arg(long, global = true)]
    to: Option<String>,
    /// Catalogue tarifaire Codex alternatif (JSON versionne)
    #[arg(long, global = true)]
    pricing: Option<String>,
    /// Sortie JSON
    #[arg(long, global = true)]
    json: bool,
}

#[derive(Subcommand)]
enum Cmd {
    /// Scan complet en memoire, sans base (verification / gate)
    Scan,
    /// Import incremental dans SQLite (checkpoints, dedup, enrichissement)
    Import {
        /// Re-scan complet depuis zero (les doublons restent dedupliques)
        #[arg(long)]
        full: bool,
        /// Recalcule tous les couts (re-pricing)
        #[arg(long)]
        reprice: bool,
    },
    /// Totaux de periode, par modele/jour/projet/activite, avec cout equivalent
    Summary {
        /// Ventilation additionnelle
        #[arg(long, value_enum, default_value = "day")]
        by: Breakdown,
        /// Ne pas importer avant le rapport
        #[arg(long)]
        no_import: bool,
    },
    /// Diagnostics detailles, consistency check base vs scan frais
    Doctor,
    /// Surveillance temps reel : import incremental a chaque changement
    Watch {
        /// Periode de quietude avant import (ms)
        #[arg(long, default_value = "800")]
        quiet_ms: u64,
    },
    /// Export CSV des agregats normalises (+ optionnellement les appels)
    Export {
        /// Dossier cible des CSV
        #[arg(long)]
        csv: PathBuf,
        /// Chemin d'un JSON de synthese additionnel
        #[arg(long)]
        json_out: Option<PathBuf>,
        /// Ne pas importer avant l'export
        #[arg(long)]
        no_import: bool,
    },
}

#[derive(Copy, Clone, ValueEnum)]
enum Breakdown {
    Day,
    Project,
    Thread,
    Activity,
    Model,
}

fn fmt_int(n: u64) -> String {
    let s = n.to_string();
    let mut out = String::new();
    let bytes = s.as_bytes();
    for (i, b) in bytes.iter().enumerate() {
        if i > 0 && (bytes.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(*b as char);
    }
    out
}

fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000_000 {
        format!("{:.1} B", n as f64 / 1e9)
    } else if n >= 1_000_000 {
        format!("{:.1} M", n as f64 / 1e6)
    } else if n >= 1_000 {
        format!("{:.1} K", n as f64 / 1e3)
    } else {
        n.to_string()
    }
}

fn fmt_money(v: f64) -> String {
    let sign = if v < 0.0 { "-" } else { "" };
    let v = v.abs();
    // Arrondi au centime le plus proche avec report vers les dollars
    // (1.995 -> $2.00, pas $1.99 par ecratement de 100).
    let cents_total = (v * 100.0).round() as u64;
    let whole = cents_total / 100;
    let cents = cents_total % 100;
    format!("{sign}${}.{:02}", fmt_int(whole), cents)
}

fn main() {
    let cli = Cli::parse();
    match run(&cli) {
        Ok(()) => {}
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}

fn run(cli: &Cli) -> Result<(), String> {
    let home = discovery::codex_home(cli.codex_home.as_deref());
    let db_path = cli.db.clone().unwrap_or_else(store::default_db_path);
    let engine = match &cli.pricing {
        Some(p) => PricingEngine::load(p, None)?,
        None => PricingEngine::embedded(),
    };

    match &cli.cmd {
        Cmd::Scan => {
            if !home.exists() {
                return Err(format!("codex home introuvable : {}", home.display()));
            }
            let out = scan::scan(&home);
            let filtered = scan::filter_period(out.calls, cli.from.as_deref(), cli.to.as_deref());
            let totals = aggregate::usage_of(&filtered);
            let d = &out.diagnostics;
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "codex_home": out.codex_home.display().to_string(),
                        "files_found": d.files_found,
                        "files_parsed": d.files_parsed,
                        "files_failed": d.files_failed,
                        "calls": filtered.len(),
                        "totals": {
                            "input": totals.input_tokens,
                            "cached": totals.cached_input_tokens,
                            "cache_write": totals.cache_write_input_tokens,
                            "output": totals.output_tokens,
                            "reasoning": totals.reasoning_output_tokens,
                            "total": totals.total_tokens,
                        },
                        "parse_errors": d.parse_errors,
                    })
                );
            } else {
                print_totals(&totals, filtered.len());
                println!(
                    "Files  {} found / {} parsed / {} failed ({} archived)",
                    d.files_found, d.files_parsed, d.files_failed, d.archived_files
                );
                println!("Errors {}", d.parse_errors.len());
            }
        }
        Cmd::Import { full, reprice } => {
            let rep = import::import(&home, &db_path, &engine, *full, *reprice)?;
            render_import_report(&rep, cli);
        }
        Cmd::Summary { by, no_import } => {
            if !*no_import {
                let rep = import::import(&home, &db_path, &engine, false, false)?;
                if rep.calls_inserted > 0 && !cli.json {
                    eprintln!(
                        "[import] +{} appels ({} fichiers, {} ms)",
                        rep.calls_inserted, rep.files_changed, rep.duration_ms
                    );
                }
            }
            let calls = import::load_calls(&db_path, cli.from.as_deref(), cli.to.as_deref())?;
            render_summary(&calls, by, &engine, cli);
        }
        Cmd::Doctor => {
            render_doctor(&home, &db_path, &engine, cli);
        }
        Cmd::Watch { quiet_ms } => {
            import::import(&home, &db_path, &engine, false, false)?;
            watch::run_watch(&home, &db_path, &engine, *quiet_ms)?;
        }
        Cmd::Export { csv, json_out, no_import } => {
            if !*no_import {
                import::import(&home, &db_path, &engine, false, false)?;
            }
            let calls = import::load_calls(&db_path, cli.from.as_deref(), cli.to.as_deref())?;
            let files = export::export_csv(csv, &calls, &engine)?;
            if let Some(jp) = json_out {
                let totals = aggregate::usage_of(&calls);
                let json = serde_json::json!({
                    "period": {"from": cli.from, "to": cli.to},
                    "calls": calls.len(),
                    "totals": totals,
                });
                std::fs::write(jp, serde_json::to_string_pretty(&json).map_err(|e| e.to_string())?)
                    .map_err(|e| e.to_string())?;
            }
            if cli.json {
                println!(
                    "{}",
                    serde_json::json!({
                        "csv_files": files.iter().map(|p| p.display().to_string()).collect::<Vec<_>>(),
                        "calls": calls.len(),
                    })
                );
            } else {
                for f in &files {
                    println!("{}", f.display());
                }
            }
        }
    }
    Ok(())
}

fn render_import_report(rep: &import::ImportReport, cli: &Cli) {
    if cli.json {
        println!(
            "{}",
            serde_json::json!({
                "files_seen": rep.files_seen,
                "files_skipped": rep.files_skipped,
                "files_changed": rep.files_changed,
                "calls_inserted": rep.calls_inserted,
                "duplicates_ignored": rep.duplicates_ignored,
                "parse_errors": rep.parse_errors,
                "threads_enriched": rep.threads_enriched,
                "calls_enriched": rep.calls_enriched,
                "tiers_repaired": rep.tiers_repaired,
                "legacy_pruned": rep.legacy_pruned,
                "repriced": rep.repriced,
                "priced_calls": rep.priced_calls,
                "duration_ms": rep.duration_ms,
            })
        );
        return;
    }
    println!("Import termine en {} ms", rep.duration_ms);
    println!("  Fichiers vus       {}", rep.files_seen);
    println!("  Fichiers ignores   {} (checkpoint a jour)", rep.files_skipped);
    println!("  Fichiers modifies  {}", rep.files_changed);
    println!("  Octets lus         {}", fmt_int(rep.bytes_read));
    println!("  Appels insérés     {}", fmt_int(rep.calls_inserted));
    println!("  Doublons ignores   {}", fmt_int(rep.duplicates_ignored));
    if rep.legacy_pruned > 0 {
        println!(
            "  Legacy purges      {} (session deja couverte en moderne)",
            fmt_int(rep.legacy_pruned)
        );
    }
    println!("  Erreurs parsing    {}", rep.parse_errors);
    println!(
        "  Threads enrichis   {} ({} calls touches)",
        rep.threads_enriched,
        fmt_int(rep.calls_enriched)
    );
    if rep.tiers_repaired > 0 {
        println!("  Tiers repares      {}", fmt_int(rep.tiers_repaired));
    }
    if rep.repriced {
        println!("  Re-pricing         {} appels valorises", rep.priced_calls);
    }
}

fn print_totals(t: &codex_meter::model::TokenUsage, calls: usize) {
    println!("Calls        {}", fmt_int(calls as u64));
    println!("Input        {}", fmt_tokens(t.input_tokens));
    println!("Cached       {}", fmt_tokens(t.cached_input_tokens));
    println!("Non cached   {}", fmt_tokens(t.ordinary_input()));
    println!("Output       {}", fmt_tokens(t.output_tokens));
    if t.input_tokens > 0 {
        println!(
            "Cache hit    {:.1} %",
            t.cached_input_tokens as f64 / t.input_tokens as f64 * 100.0
        );
    }
}

fn bucket_json(b: &aggregate::Bucket) -> serde_json::Value {
    serde_json::json!({
        "calls": b.calls,
        "input": b.usage.input_tokens,
        "cached": b.usage.cached_input_tokens,
        "cache_write": b.usage.cache_write_input_tokens,
        "output": b.usage.output_tokens,
        "reasoning": b.usage.reasoning_output_tokens,
        "cache_hit_percent": b.cache_hit_percent(),
        "equivalent_cost_codex": b.equivalent_cost,
        "cost_without_cache": b.cost_without_cache,
        "cache_savings": b.cache_savings,
        "unknown_model_tokens": b.unknown_model_tokens,
        "long_context_calls": b.long_context_calls,
    })
}

fn render_summary(
    calls: &[codex_meter::model::InferenceCall],
    by: &Breakdown,
    engine: &PricingEngine,
    cli: &Cli,
) {
    let totals = aggregate::usage_of(calls);
    let mut total_cd = aggregate::Bucket::default();
    for c in calls {
        let cd = engine.cost_codex(c);
        total_cd.add(c, cd.model_known.then_some(&cd));
    }

    let (exact, inferred, unknown) = aggregate::confidence_stats(calls);
    let tier_stats = codex_meter::pricing::tier_stats(calls);
    let confidence = codex_meter::pricing::pricing_confidence(calls);
    let period_note = match (&cli.from, &cli.to) {
        (Some(f), Some(t)) => format!("{f} → {t}"),
        (Some(f), None) => format!("depuis {f}"),
        (None, Some(t)) => format!("jusqu'à {t}"),
        (None, None) => "tout l'historique".to_string(),
    };

    if cli.json {
        let extra = match by {
            Breakdown::Project => Some(aggregate::by_project(calls, engine)),
            Breakdown::Thread => Some(aggregate::by_thread(calls, engine)),
            Breakdown::Activity => Some(aggregate::by_activity(calls, engine)),
            Breakdown::Model | Breakdown::Day => None,
        };
        let to_map = |m: aggregate::Buckets| {
            m.iter()
                .map(|(k, b)| (k.clone(), bucket_json(b)))
                .collect::<serde_json::Map<_, _>>()
        };
        println!(
            "{}",
            serde_json::json!({
                "period": period_note,
                "calls": calls.len(),
                "totals": {
                    "input": totals.input_tokens,
                    "cached": totals.cached_input_tokens,
                    "ordinary": totals.ordinary_input(),
                    "cache_write": totals.cache_write_input_tokens,
                    "output": totals.output_tokens,
                    "reasoning": totals.reasoning_output_tokens,
                    "cache_hit_percent": total_cd.cache_hit_percent(),
                },
                "cost_equivalent_codex": {
                    "equivalent_cost": total_cd.equivalent_cost,
                    "cost_without_cache": total_cd.cost_without_cache,
                    "cache_savings": total_cd.cache_savings,
                    "cache_savings_percent": total_cd.cache_savings_percent(),
                    "note": "estimation basee sur la grille Codex/Work ; jamais une facture OpenAI",
                },
                "model_confidence": { "exact": exact, "inferred": inferred, "unknown": unknown },
                "service_tier": { "fast": tier_stats.0, "standard": tier_stats.1, "unknown": tier_stats.2 },
                "pricing_confidence_percent": confidence,
                "by_day": to_map(aggregate::by_day(calls, engine)),
                "by_model": to_map(aggregate::by_model(calls, engine)),
                "by_activity": to_map(aggregate::by_activity(calls, engine)),
                "breakdown": extra.map(to_map),
                "anomalies": aggregate::detect_anomalies(calls, &aggregate::by_day(calls, engine), &aggregate::by_activity(calls, engine), total_cd.long_context_calls),
            })
        );
        return;
    }

    println!("Periode            {period_note}");
    print_totals(&totals, calls.len());
    println!();
    println!("Equivalent Codex   {}", fmt_money(total_cd.equivalent_cost));
    println!("Sans cache         {}", fmt_money(total_cd.cost_without_cache));
    println!(
        "Economie cache     {} ({})",
        fmt_money(total_cd.cache_savings),
        total_cd
            .cache_savings_percent()
            .map(|p| format!("{p:.1} %"))
            .unwrap_or_else(|| "n/a".into())
    );
    if total_cd.unknown_model_calls > 0 {
        println!(
            "Coût N/A           {} appels / {} tokens sans tarif connu",
            fmt_int(total_cd.unknown_model_calls),
            fmt_tokens(total_cd.unknown_model_tokens)
        );
    }
    if inferred + unknown > 0 {
        println!(
            "Confiance modèle   exact {exact} · inferred {inferred} · unknown {unknown}"
        );
    }
    println!("Confiance tarif    {:.1} %", confidence);
    if tier_stats.0 > 0 {
        println!(
            "Fast mode          {} appel(s) — multiplicateur fast appliqué",
            fmt_int(tier_stats.0)
        );
    }

    println!();
    println!("Par modèle");
    for (model, b) in aggregate::by_model(calls, engine) {
        println!(
            "  {:<22} in {:>9}  cached {:>9}  out {:>9}  hit {:>5}  calls {:>6}  {}",
            model,
            fmt_tokens(b.usage.input_tokens),
            fmt_tokens(b.usage.cached_input_tokens),
            fmt_tokens(b.usage.output_tokens),
            b.cache_hit_percent()
                .map(|p| format!("{p:.1}%"))
                .unwrap_or_else(|| "n/a".into()),
            fmt_int(b.calls),
            if b.unknown_model_calls > 0 {
                "coût N/A".to_string()
            } else {
                fmt_money(b.equivalent_cost)
            },
        );
    }

    match by {
        Breakdown::Day => {
            println!();
            println!("Par jour");
            for (day, b) in aggregate::by_day(calls, engine) {
                println!(
                    "  {:<12} in {:>9}  out {:>9}  hit {:>5}  calls {:>6}  {}",
                    day,
                    fmt_tokens(b.usage.input_tokens),
                    fmt_tokens(b.usage.output_tokens),
                    b.cache_hit_percent()
                        .map(|p| format!("{p:.1}%"))
                        .unwrap_or_else(|| "n/a".into()),
                    fmt_int(b.calls),
                    fmt_money(b.equivalent_cost),
                );
            }
        }
        Breakdown::Model => {}
        Breakdown::Project => {
            println!();
            println!("Par projet");
            for (p, b) in aggregate::by_project(calls, engine) {
                println!(
                    "  {:<24} in {:>9}  calls {:>6}  {}",
                    p,
                    fmt_tokens(b.usage.input_tokens),
                    fmt_int(b.calls),
                    fmt_money(b.equivalent_cost),
                );
            }
        }
        Breakdown::Thread => {
            println!();
            println!("Par thread (top 20 par input)");
            let mut v: Vec<_> = aggregate::by_thread(calls, engine).into_iter().collect();
            v.sort_by(|a, b| b.1.usage.input_tokens.cmp(&a.1.usage.input_tokens));
            for (t, b) in v.iter().take(20) {
                let title = calls
                    .iter()
                    .find(|c| c.thread_id.as_deref() == Some(t.as_str()))
                    .and_then(|c| c.thread_title.clone())
                    .unwrap_or_default();
                let label = if title.is_empty() {
                    t.clone()
                } else {
                    format!("{t} — {}", &title.chars().take(48).collect::<String>())
                };
                println!(
                    "  {:<64} in {:>9}  calls {:>6}  {}",
                    label,
                    fmt_tokens(b.usage.input_tokens),
                    fmt_int(b.calls),
                    fmt_money(b.equivalent_cost),
                );
            }
        }
        Breakdown::Activity => {
            println!();
            println!("Par activité");
            for (a, b) in aggregate::by_activity(calls, engine) {
                println!(
                    "  {:<14} in {:>9}  out {:>9}  calls {:>6}  {}",
                    a,
                    fmt_tokens(b.usage.input_tokens),
                    fmt_tokens(b.usage.output_tokens),
                    fmt_int(b.calls),
                    fmt_money(b.equivalent_cost),
                );
            }
        }
    }

    let by_day = aggregate::by_day(calls, engine);
    let anomalies = aggregate::detect_anomalies(
        calls,
        &by_day,
        &aggregate::by_activity(calls, engine),
        total_cd.long_context_calls,
    );
    if !anomalies.is_empty() {
        println!();
        println!("Anomalies");
        for a in anomalies {
            println!("  ! {a}");
        }
    }
    println!();
    if tier_stats.2 > 0 {
        println!(
            "Note : service tier non déterminé pour {} appel(s) — valorisés au tarif standard (fast/régional non appliqués).",
            fmt_int(tier_stats.2)
        );
    } else if !calls.is_empty() {
        println!("Note : service tier déterminé pour tous les appels (régional jamais supposé).");
    }
}

fn render_doctor(home: &std::path::Path, db_path: &std::path::Path, engine: &PricingEngine, cli: &Cli) {
    // Import incremental d'abord : une divergence APRES import est une vraie
    // incoherence ; avant, ce n'est qu'un fichier plus recent que la base.
    match import::import(home, db_path, engine, false, false) {
        Ok(rep) => {
            if rep.calls_inserted > 0 && !cli.json {
                eprintln!(
                    "[import] +{} appels avant le diagnostic",
                    rep.calls_inserted
                );
            }
        }
        Err(e) => eprintln!("warning: import prealable echoue : {e}"),
    }
    let fresh = scan::scan(home);
    let d = &fresh.diagnostics;

    let conn = store::open_db(db_path).and_then(|c| {
        store::init_schema(&c)?;
        Ok(c)
    });
    let (db_calls, db_totals) = match &conn {
        Ok(c) => store::db_totals(c).map(|(n, t)| (n, t)).unwrap_or_default(),
        Err(_) => (0, codex_meter::model::TokenUsage::default()),
    };

    let state_threads = codex_meter::state::thread_activity_counts(&home.join("state_5.sqlite"));
    let state_tokens_used: i64 = conn.as_ref().ok().and_then(|c| {
        c.query_row(
            "SELECT COALESCE(SUM(state_tokens_used),0) FROM threads",
            [],
            |row| row.get(0),
        )
        .ok()
    }).unwrap_or(0);

    if cli.json {
        println!(
            "{}",
            serde_json::json!({
                "codex_home": home.display().to_string(),
                "db": db_path.display().to_string(),
                "fresh_scan": {
                    "files_found": d.files_found,
                    "files_parsed": d.files_parsed,
                    "lines_total": d.lines_total,
                    "usage_records": d.usage_records,
                    "duplicates_ignored": d.duplicates_ignored,
                    "legacy_records": d.legacy_records,
                    "parse_errors": d.parse_errors,
                    "subset_violations": d.subset_violations,
                },
                "db": {
                    "calls": db_calls,
                    "totals": db_totals,
                },
                "consistency": {
                    "status": if db_totals == fresh.totals { "CONSISTENT" } else { "DIVERGENCE" },
                    "state_db_lifetime_tokens_used": state_tokens_used,
                    "note": "threads.tokens_used = indication lifetime uniquement, jamais source de totaux",
                },
                "state_threads_by_activity": state_threads,
                "pricing": {
                    "codex_version": engine.codex.version,
                    "api_version": engine.api.version,
                    "last_verified": engine.codex.last_verified,
                },
            })
        );
        return;
    }

    println!("Codex home            {}", home.display());
    println!("Base                  {}", db_path.display());
    println!();
    println!("Scan frais (memoire)");
    println!("  Rollout files found   {}", d.files_found);
    println!("  Rollout files parsed  {}", d.files_parsed);
    println!("  Lines scanned         {}", fmt_int(d.lines_total));
    println!("  Usage records         {}", fmt_int(d.usage_records));
    println!("  Duplicates ignored    {}", fmt_int(d.duplicates_ignored));
    println!("  Legacy records        {}", fmt_int(d.legacy_records));
    println!("  Legacy null info      {}", fmt_int(d.legacy_null_info));
    println!("  Parsing errors        {}", d.parse_errors.len());
    println!("  Subset violations     {}", d.subset_violations);
    println!();
    println!("Base SQLite");
    println!("  Appels stockes        {}", fmt_int(db_calls));
    println!("  Input                 {}", fmt_int(db_totals.input_tokens));
    println!("  Cached                {}", fmt_int(db_totals.cached_input_tokens));
    println!("  Output                {}", fmt_int(db_totals.output_tokens));
    println!();
    println!("Run consistency check");
    println!(
        "  scan frais (memoire)  {} tokens input / {} appels",
        fmt_tokens(fresh.totals.input_tokens),
        fmt_int(fresh.calls.len() as u64)
    );
    println!(
        "  base SQLite           {} tokens input / {} appels",
        fmt_tokens(db_totals.input_tokens),
        fmt_int(db_calls)
    );
    let status = if db_totals == fresh.totals {
        "CONSISTENT"
    } else {
        "DIVERGENCE — relancer `import --full`"
    };
    println!(
        "  state DB lifetime     {} tokens_used (indication UNIQUEMENT, jamais source de totaux)",
        fmt_int(state_tokens_used.max(0) as u64)
    );
    println!("  Status: {status}");
    println!();
    println!("Threads state db (enrichissement)");
    match state_threads {
        Some(counts) => {
            for (k, v) in counts {
                println!("  {k:<14} {v}");
            }
        }
        None => println!("  state_5.sqlite indisponible — enrichissement desactive"),
    }
    println!();
    println!("Pricing catalog");
    println!(
        "  {} v{} (vérifié {}, {})",
        engine.codex.catalog, engine.codex.version, engine.codex.last_verified, engine.codex.source_url
    );
    println!(
        "  {} v{} (vérifié {})",
        engine.api.catalog, engine.api.version, engine.api.last_verified
    );

    if !d.parse_errors.is_empty() {
        println!();
        println!("Erreurs de parsing (max 10) :");
        for e in d.parse_errors.iter().take(10) {
            println!("  ! {e}");
        }
    }
}
