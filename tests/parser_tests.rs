use std::fs;
use std::path::{Path, PathBuf};

use codex_meter::model::{ModelConfidence, SourceFormat};
use codex_meter::pricing::PricingEngine;
use codex_meter::scan::{self, scan};

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn fixture(name: &str) -> String {
    fs::read_to_string(fixtures_dir().join(name)).expect("fixture exists")
}

/// Construit un codex home temporaire avec les fixtures copies dans
/// sessions/YYYY/MM/DD. Retourne le chemin du home.
fn temp_home(tag: &str, layout: &[(&str, &str)]) -> PathBuf {
    let home = std::env::temp_dir().join(format!(
        "codex-meter-test-{tag}-{}",
        std::process::id()
    ));
    let _ = fs::remove_dir_all(&home);
    for (subdir, fixture_name) in layout {
        let dir = home.join("sessions").join(subdir);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(format!("rollout-{fixture_name}")), fixture(fixture_name)).unwrap();
    }
    home
}

fn totals(home: &Path) -> scan::ScanOutcome {
    scan(home)
}

#[test]
fn primary_format_basic() {
    let home = temp_home(
        "primary",
        &[("2026/09/15", "current_token_usage_record.jsonl")],
    );
    let out = totals(&home);
    let d = &out.diagnostics;
    assert_eq!(d.usage_records, 2);
    assert_eq!(d.legacy_records, 0);
    assert_eq!(out.calls.len(), 2);
    // Les cumuls dans le fixture sont enormes volontairement : s'ils
    // fuyaient dans les totaux, ces asserts explosent.
    assert_eq!(out.totals.input_tokens, 30_000);
    assert_eq!(out.totals.cached_input_tokens, 23_000);
    assert_eq!(out.totals.output_tokens, 3_000);
    assert_eq!(out.totals.reasoning_output_tokens, 1_800);
    assert_eq!(out.totals.total_tokens, 33_000);
    let c = &out.calls[0];
    assert_eq!(c.model_slug.as_deref(), Some("gpt-6-astra"));
    assert_eq!(c.model_confidence, ModelConfidence::Exact);
    assert_eq!(c.project_path.as_deref(), Some("/Users/fmahia/Dev/Alpha"));
    assert!(c.response_id.is_some());
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn legacy_fallback_uses_last_not_total() {
    let home = temp_home("legacy", &[("2026/09/03", "legacy_token_count.jsonl")]);
    let out = totals(&home);
    let d = &out.diagnostics;
    assert_eq!(d.legacy_records, 2, "info null doit etre diagnostique, pas compte");
    assert_eq!(d.legacy_null_info, 1);
    // last_token_usage x2, PAS total_token_usage (serait 999.9G+).
    assert_eq!(out.totals.input_tokens, 48_619 + 48_620);
    assert_eq!(out.totals.output_tokens, 285 + 286);
    assert!(out.totals.input_tokens < 100_000);
    for c in &out.calls {
        assert_eq!(c.source_format, SourceFormat::LegacyTokenCount);
    }
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn never_mix_primary_and_legacy_in_same_file() {
    // current_token_usage_record.jsonl contient primary + un event legacy :
    // seul le primary doit compter.
    let home = temp_home(
        "mix",
        &[("2026/09/15", "current_token_usage_record.jsonl")],
    );
    let out = totals(&home);
    assert_eq!(out.calls.len(), 2);
    assert_eq!(out.totals.input_tokens, 30_000);
    assert!(out.calls.iter().all(|c| c.source_format == SourceFormat::TokenUsageRecord));
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn duplicate_response_id_counts_once() {
    let home = temp_home("dup", &[("2026/09/16", "duplicate_active_archived.jsonl")]);
    let out = totals(&home);
    assert_eq!(out.calls.len(), 1, "response_id duplique = 1 appel");
    assert_eq!(out.diagnostics.duplicates_ignored, 1);
    assert_eq!(out.totals.input_tokens, 5_000);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn archived_sessions_never_double_count() {
    let home = temp_home(
        "archivedup",
        &[
            ("2026/09/16", "duplicate_active_archived.jsonl"),
            ("2026/09/16", "duplicate_active_archived.jsonl"),
        ],
    );
    // Meme fichier en sessions/ et archived_sessions/ : les deux sont
    // scannes, un seul appel doit rester.
    let archived = home.join("archived_sessions/2026/09/16");
    fs::create_dir_all(&archived).unwrap();
    fs::write(
        archived.join("rollout-duplicate_active_archived.jsonl"),
        fixture("duplicate_active_archived.jsonl"),
    )
    .unwrap();

    let out = totals(&home);
    assert_eq!(out.diagnostics.archived_files, 1);
    assert_eq!(out.calls.len(), 1, "archive = aucun double comptage");
    assert_eq!(out.totals.input_tokens, 5_000);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn rescan_is_idempotent() {
    let home = temp_home(
        "idempotent",
        &[
            ("2026/09/15", "current_token_usage_record.jsonl"),
            ("2026/09/16", "duplicate_active_archived.jsonl"),
            ("2026/09/15", "cache_write.jsonl"),
        ],
    );
    let first = totals(&home);
    let second = totals(&home);
    assert_eq!(first.totals.input_tokens, second.totals.input_tokens);
    assert_eq!(first.totals.output_tokens, second.totals.output_tokens);
    assert_eq!(first.calls.len(), second.calls.len());
    // Reimporter un fichier qui existe deja a l'identique : aucun changement.
    let third = totals(&home);
    assert_eq!(first.totals.input_tokens, third.totals.input_tokens);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn subsets_and_violations() {
    let home = temp_home(
        "subsets",
        &[
            ("2026/09/15", "current_token_usage_record.jsonl"),
            ("2026/09/15", "cache_write.jsonl"),
            ("2026/09/15", "subagents.jsonl"),
        ],
    );
    let out = totals(&home);
    assert_eq!(out.diagnostics.subset_violations, 0);
    for c in &out.calls {
        assert!(c.usage.cached_input_tokens <= c.usage.input_tokens);
        assert!(c.usage.cache_write_input_tokens <= c.usage.input_tokens);
        assert!(c.usage.reasoning_output_tokens <= c.usage.output_tokens);
    }
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn partial_last_line_is_tolerated() {
    let home = temp_home("partial", &[("2026/09/17", "partial_last_line.jsonl")]);
    let out = totals(&home);
    // Le record valide avant la ligne tronquee doit compter.
    assert_eq!(out.calls.len(), 1);
    assert_eq!(out.totals.input_tokens, 1_000);
    assert!(out.diagnostics.parse_errors.len() >= 1, "diagnostic requis");
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn malformed_record_fails_locally_and_continues() {
    let home = temp_home("malformed", &[("2026/09/17", "malformed_record.jsonl")]);
    let out = totals(&home);
    // resp-mal-1 invalide (input "not-a-number") : echec local, resp-mal-2 compte.
    assert_eq!(out.calls.len(), 1);
    assert_eq!(out.totals.input_tokens, 3_000);
    assert!(out.diagnostics.parse_errors.len() >= 1);
    // Le type de record inconnu n'explose pas le fichier (resilience).
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn multiple_models_attributed_exactly() {
    let home = temp_home("multi", &[("2026/09/15", "multiple_models.jsonl")]);
    let out = totals(&home);
    assert_eq!(out.calls.len(), 3);
    let mut models: Vec<_> = out
        .calls
        .iter()
        .map(|c| (c.model_slug.clone().unwrap(), c.model_confidence))
        .collect();
    models.sort();
    assert_eq!(
        models,
        vec![
            ("gpt-5.6-luna".into(), ModelConfidence::Exact),
            ("gpt-5.6-sol".into(), ModelConfidence::Exact),
            ("gpt-6-astra".into(), ModelConfidence::Exact),
        ]
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn cache_write_codex_vs_api_profiles() {
    let home = temp_home("cachewrite", &[("2026/09/15", "cache_write.jsonl")]);
    let out = totals(&home);
    let call = &out.calls[0];
    // ordinary = input - cached - cache_write = 10000 - 4000 - 2000
    assert_eq!(call.usage.ordinary_input(), 4_000);

    let engine = PricingEngine::embedded();
    // Profil Codex : cache_write valorise au tarif cached (hypothese
    // documentee), JAMAIS au multiplicateur API 1.25x sur l'input.
    let codex = engine.cost_codex(call);
    let expected_codex = (4_000.0 * 4.0 + 4_000.0 * 0.4 + 2_000.0 * 0.4 + 1_000.0 * 20.0) / 1e6;
    assert!((codex.equivalent_cost - expected_codex).abs() < 1e-9);

    // Profil API : cache_write facture 1.25x l'entree ordinaire.
    let api = engine.cost_api(call);
    let expected_api = (4_000.0 * 4.0 + 4_000.0 * 0.4 + 2_000.0 * 5.0 + 1_000.0 * 20.0) / 1e6;
    assert!((api.equivalent_cost - expected_api).abs() < 1e-9);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn long_context_is_per_call() {
    let home = temp_home("longctx", &[("2026/09/15", "long_context.jsonl")]);
    let out = totals(&home);
    let engine = PricingEngine::embedded();
    // 272000 = short context ; 272001 = long context (appel par appel).
    assert!(!engine.cost_codex(&out.calls[0]).long_context);
    assert!(engine.cost_codex(&out.calls[1]).long_context);
    // GPT-6 Astra : exception, pas de multiplicateur long context dans Codex.
    assert!(!engine.cost_codex(&out.calls[2]).long_context);
    // Verif du prix long : sol, input 272001 (ordinary 12001), cached 260000.
    let long = engine.cost_codex(&out.calls[1]);
    let expected = (12_001.0 * 4.0 * 2.0 + 260_000.0 * 0.4 * 2.0 + 1_000.0 * 20.0 * 1.5) / 1e6;
    assert!((long.equivalent_cost - expected).abs() < 1e-9);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn subagents_attributed_without_inventing() {
    let home = temp_home("subagents", &[("2026/09/15", "subagents.jsonl")]);
    let out = totals(&home);
    assert_eq!(out.calls.len(), 3);
    let main = out
        .calls
        .iter()
        .find(|c| c.thread_id.as_deref() == Some("thread-main"))
        .unwrap();
    assert_eq!(main.model_slug.as_deref(), Some("gpt-6-astra"));
    let sub = out
        .calls
        .iter()
        .find(|c| c.thread_id.as_deref() == Some("thread-subagent-1"))
        .unwrap();
    assert_eq!(sub.model_slug.as_deref(), Some("gpt-5.6-sol"));
    // turn sans turn_context : unknown, jamais invente.
    let orphan = out
        .calls
        .iter()
        .find(|c| c.thread_id.as_deref() == Some("thread-subagent-2"))
        .unwrap();
    assert_eq!(orphan.model_slug, None);
    assert_eq!(orphan.model_confidence, ModelConfidence::Unknown);
    assert_eq!(out.diagnostics.calls_unknown_model, 1);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn auto_review_uses_gpt54_rate_card() {
    let home = temp_home("autoreview", &[("2026/09/18", "auto_review.jsonl")]);
    let out = totals(&home);
    assert_eq!(out.calls.len(), 1);
    let engine = PricingEngine::embedded();
    let cd = engine.cost_codex(&out.calls[0]);
    assert!(cd.model_known, "codex-auto-review -> alias gpt-5.4");
    let expected = (10_675.0 * 2.5 + 4_864.0 * 0.25 + 160.0 * 15.0) / 1e6;
    assert!((cd.equivalent_cost - expected).abs() < 1e-9);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn unknown_model_tokens_counted_cost_na() {
    let home = temp_home("unknownmodel", &[("2026/09/15", "unknown_model.jsonl")]);
    let out = totals(&home);
    // turn_context arrive APRES le record : resolution fin de fichier.
    assert_eq!(out.calls.len(), 1);
    assert_eq!(out.calls[0].model_slug.as_deref(), Some("brand-new-model-9x"));
    let engine = PricingEngine::embedded();
    let cd = engine.cost_codex(&out.calls[0]);
    assert!(!cd.model_known, "modele sans regle tarifaire = jamais prix");
    assert_eq!(cd.equivalent_cost, 0.0);
    // Les tokens restent comptes (invariant 9).
    assert_eq!(out.totals.input_tokens, 7_000);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn period_filter_on_record_timestamp() {
    let home = temp_home(
        "period",
        &[
            ("2026/09/15", "current_token_usage_record.jsonl"),
            ("2026/09/03", "legacy_token_count.jsonl"),
        ],
    );
    let out = scan(&home);
    let kept = scan::filter_period(out.calls, Some("2026-09-11"), Some("2026-09-18"));
    assert_eq!(kept.len(), 2, "les records legacy du 09/03 sortent de la fenetre");
    let mut inp = 0;
    for c in &kept {
        inp += c.usage.input_tokens;
    }
    assert_eq!(inp, 30_000);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn cumulative_counters_never_leak_into_totals() {
    // Garde-fou du bug Tokus : si un jour quelqu'un somme les cumuls,
    // ce test echoue. Les totaux de la fixture primaire sont connus.
    let home = temp_home(
        "noaccumulate",
        &[("2026/09/15", "current_token_usage_record.jsonl")],
    );
    let out = totals(&home);
    // turn_token_usage/thread_token_usage du fixture : >= 444M chacun.
    assert!(out.totals.input_tokens < 1_000_000);
    assert_eq!(out.totals.input_tokens, 30_000);
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn same_session_never_mixes_primary_and_legacy_across_files() {
    // Deux fichiers distincts, MEME session : l'event legacy de l'ancienne
    // methode ne doit jamais s'ajouter aux token_usage_record (invariant 7,
    // applicable globalement et pas seulement a l interieur d un fichier).
    let home = temp_home(
        "globalmix",
        &[("2026/09/15", "current_token_usage_record.jsonl")],
    );
    let legacy = fixture("legacy_token_count.jsonl").replace("sess-legacy", "sess-primary");
    let d = home.join("sessions/2026/09/03");
    fs::create_dir_all(&d).unwrap();
    fs::write(d.join("rollout-legacy-same-session.jsonl"), legacy).unwrap();

    let out = totals(&home);
    assert_eq!(
        out.calls.len(),
        2,
        "seuls les 2 appels primaires de sess-primary doivent compter"
    );
    assert!(out
        .calls
        .iter()
        .all(|c| c.source_format == SourceFormat::TokenUsageRecord));
    assert_eq!(out.totals.input_tokens, 30_000, "jamais 30_000 + legacy");
    assert_eq!(
        out.diagnostics.legacy_records, 0,
        "fichier legacy couvert en moderne : plus une source"
    );
    let _ = fs::remove_dir_all(&home);
}
