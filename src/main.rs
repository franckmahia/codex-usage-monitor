use std::path::PathBuf;

use clap::{Parser, Subcommand, ValueEnum};

use codex_meter::{aggregate, discovery, pricing::PricingEngine, scan};

#[derive(Parser)]
#[command(
    name = "codex-meter",
    about = "Compteur de tokens Codex local et auditable. Lecture seule, hors ligne.",
    version
)]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
    /// Chemin du codex home (defaut: $CODEX_HOME sinon ~/.codex)
    #[arg(long, global = true)]
    codex_home: Option<PathBuf>,
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
    /// Scan complet et affichage des totaux + diagnostics sommaires
    Scan,
    /// Totaux de periode, par modele/jour/projet, avec cout equivalent
    Summary {
        /// Ventilation additionnelle
        #[arg(long, value_enum, default_value = "day")]
        by: Breakdown,
    },
    /// Diagnostics detailles et consistency check
    Doctor,
}

#[derive(Copy, Clone, ValueEnum)]
enum Breakdown {
    Day,
    Project,
    Thread,
    Activity,
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
    let whole = v as u64;
    let cents = ((v - whole as f64) * 100.0).round() as u64;
    let cents = if cents > 99 { 99 } else { cents };
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
    if !home.exists() {
        return Err(format!("codex home introuvable : {}", home.display()));
    }
    let engine = match &cli.pricing {
        Some(p) => PricingEngine::load(p, None)?,
        None => PricingEngine::embedded(),
    };

    match &cli.cmd {
        Cmd::Scan => {
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
        Cmd::Summary { by } => {
            let out = scan::scan(&home);
            let calls = scan::filter_period(out.calls, cli.from.as_deref(), cli.to.as_deref());
            render_summary(&calls, by, &engine, cli, out.diagnostics.legacy_file_totals);
        }
        Cmd::Doctor => {
            let out = scan::scan(&home);
            render_doctor(&out, &engine, cli);
        }
    }
    Ok(())
}

fn print_totals(t: &codex_meter::model::TokenUsage, calls: usize) {
    println!("Calls        {}", fmt_int(calls as u64));
    println!("Input        {}", fmt_tokens(t.input_tokens));
    println!("Cached       {}", fmt_tokens(t.cached_input_tokens));
    println!("Non cached   {}", fmt_tokens(t.ordinary_input()));
    println!("Output       {}", fmt_tokens(t.output_tokens));
    if let Some(pct) = if t.input_tokens > 0 {
        Some(t.cached_input_tokens as f64 / t.input_tokens as f64 * 100.0)
    } else {
        None
    } {
        println!("Cache hit    {:.1} %", pct);
    }
}

fn render_summary(
    calls: &[codex_meter::model::InferenceCall],
    by: &Breakdown,
    engine: &PricingEngine,
    cli: &Cli,
    legacy_totals: codex_meter::model::TokenUsage,
) {
    let totals = aggregate::usage_of(calls);
    let mut total_cd = aggregate::Bucket::default();
    for c in calls {
        let cd = engine.cost_codex(c);
        total_cd.add(c, cd.model_known.then_some(&cd));
    }

    let (exact, inferred, unknown) = aggregate::confidence_stats(calls);
    let period_note = match (&cli.from, &cli.to) {
        (Some(f), Some(t)) => format!("{f} → {t}"),
        (Some(f), None) => format!("depuis {f}"),
        (None, Some(t)) => format!("jusqu'à {t}"),
        (None, None) => "toute l'historique".to_string(),
    };

    if cli.json {
        let by_day = aggregate::by_day(calls, engine);
        let by_model = aggregate::by_model(calls, engine);
        let extra = match by {
            Breakdown::Project => Some(aggregate::by_project(calls, engine)),
            Breakdown::Thread => Some(aggregate::by_thread(calls, engine)),
            Breakdown::Activity => Some(aggregate::by_activity(calls, engine)),
            Breakdown::Day => None,
        };
        let bucket_json = |b: &aggregate::Bucket| {
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
                "by_day": by_day.iter().map(|(k, b)| (k.clone(), bucket_json(b))).collect::<serde_json::Map<_, _>>(),
                "by_model": by_model.iter().map(|(k, b)| (k.clone(), bucket_json(b))).collect::<serde_json::Map<_, _>>(),
                "breakdown": extra.map(|m| m.iter().map(|(k, b)| (k.clone(), bucket_json(b))).collect::<serde_json::Map<_, _>>()),
                "anomalies": aggregate::detect_anomalies(calls, &by_day, &aggregate::by_activity(calls, engine), total_cd.long_context_calls),
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
    let (lc, _lv) = legacy_vs_primary(&totals, legacy_totals);
    if lc {
        println!("Cohérence legacy   ✔ (voir `doctor` pour le détail)");
    }

    println!();
    println!("Par modèle");
    let by_model = aggregate::by_model(calls, engine);
    for (model, b) in &by_model {
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
                println!(
                    "  {:<38} in {:>9}  calls {:>6}  {}",
                    t,
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
    let anomalies = aggregate::detect_anomalies(calls, &by_day, &aggregate::by_activity(calls, engine), total_cd.long_context_calls);
    if !anomalies.is_empty() {
        println!();
        println!("Anomalies");
        for a in anomalies {
            println!("  ! {a}");
        }
    }
    let unknown_tier = calls.len() as u64;
    println!();
    println!(
        "Note : service tier non déterminé pour {unknown_tier} appel(s) — valorisés au tarif standard (fast/régional non appliqués)."
    );
}

/// Compare primaire vs legacy : cohérent si aucun fichier purement legacy
/// n'est divergent ou si les totaux legacy sont absents.
fn legacy_vs_primary(_primary: &codex_meter::model::TokenUsage, legacy: codex_meter::model::TokenUsage) -> (bool, codex_meter::model::TokenUsage) {
    let zero = codex_meter::model::TokenUsage::default();
    (legacy == zero || _primary.total_tokens >= legacy.total_tokens, legacy)
}

fn render_doctor(out: &scan::ScanOutcome, engine: &PricingEngine, cli: &Cli) {
    let d = &out.diagnostics;
    let totals = &out.totals;
    if cli.json {
        println!(
            "{}",
            serde_json::json!({
                "codex_home": out.codex_home.display().to_string(),
                "files_found": d.files_found,
                "files_parsed": d.files_parsed,
                "files_failed": d.files_failed,
                "archived_files": d.archived_files,
                "lines_total": d.lines_total,
                "usage_records": d.usage_records,
                "unique_response_ids": d.unique_response_ids,
                "duplicates_ignored": d.duplicates_ignored,
                "legacy_records": d.legacy_records,
                "legacy_null_info": d.legacy_null_info,
                "calls_unknown_model": d.calls_unknown_model,
                "calls_inferred_model": d.calls_inferred_model,
                "subset_violations": d.subset_violations,
                "parse_errors": d.parse_errors,
                "totals": totals,
                "consistency": consistency(out, totals),
            })
        );
        return;
    }

    println!("Codex home            {}", out.codex_home.display());
    println!("Rollout files found   {}", d.files_found);
    println!("Rollout files parsed  {}", d.files_parsed);
    println!("Rollout files failed  {}", d.files_failed);
    println!("  archived            {}", d.archived_files);
    println!("Lines scanned         {}", fmt_int(d.lines_total));
    println!("Usage records         {}", fmt_int(d.usage_records));
    println!("Unique response_ids   {}", fmt_int(d.unique_response_ids));
    println!("Duplicates ignored    {}", fmt_int(d.duplicates_ignored));
    println!("Legacy records        {}", fmt_int(d.legacy_records));
    println!("Legacy null info      {}", fmt_int(d.legacy_null_info));
    println!("Parsing errors        {}", d.parse_errors.len());
    println!("Subset violations     {}", d.subset_violations);
    println!("Calls w/o model       {} ({} tokens)", d.calls_unknown_model, fmt_tokens(d.unknown_model_tokens));
    println!("Calls inferred model  {}", d.calls_inferred_model);
    println!();
    println!("Totals (primary, dedup)");
    println!("  Input      {}", fmt_int(totals.input_tokens));
    println!("  Cached     {}", fmt_int(totals.cached_input_tokens));
    println!("  CacheW     {}", fmt_int(totals.cache_write_input_tokens));
    println!("  Output     {}", fmt_int(totals.output_tokens));
    println!("  Reasoning  {}", fmt_int(totals.reasoning_output_tokens));
    println!("  Total      {}", fmt_int(totals.total_tokens));
    println!();
    let (status, legacy) = consistency(out, totals);
    println!("Run consistency check");
    println!(
        "  token_usage_record usage   {}",
        fmt_tokens(totals.input_tokens)
    );
    println!(
        "  legacy last_token_usage    {}",
        fmt_tokens(legacy.input_tokens)
    );
    println!("  state DB lifetime totals   N/A (Phase 2)");
    println!("  Pricing catalog            {} v{} ({}, vérifié {}, {})", engine.codex.catalog, engine.codex.version, engine.codex.profile, engine.codex.last_verified, engine.codex.source_url);
    println!("  Status: {status}");

    if !d.parse_errors.is_empty() {
        println!();
        println!("Erreurs (max 10) :");
        for e in d.parse_errors.iter().take(10) {
            println!("  ! {e}");
        }
    }
}

fn consistency(out: &scan::ScanOutcome, totals: &codex_meter::model::TokenUsage) -> (&'static str, codex_meter::model::TokenUsage) {
    let legacy = out.legacy_totals();
    let zero = codex_meter::model::TokenUsage::default();
    if legacy == zero {
        ("CONSISTENT (aucun fichier purement legacy)", legacy)
    } else if legacy.total_tokens <= totals.total_tokens {
        ("CONSISTENT", legacy)
    } else {
        ("DIVERGENCE — inspecter les fichiers legacy", legacy)
    }
}
