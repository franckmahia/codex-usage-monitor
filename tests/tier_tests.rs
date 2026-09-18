use std::fs;
use std::path::{Path, PathBuf};

use codex_meter::import::{self};
use codex_meter::model::{InferenceCall, ServiceTier};
use codex_meter::pricing::PricingEngine;
use codex_meter::store;
use codex_meter::{scan, util};

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn fixture(name: &str) -> String {
    fs::read_to_string(fixtures_dir().join(name)).expect("fixture exists")
}

#[test]
fn service_tier_fast_applies_multiplier_and_no_retroactivity() {
    let home = std::env::temp_dir().join(format!("codex-meter-tier-{}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    let dir = home.join("sessions/2026/09/15");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("rollout-fast_mode.jsonl"),
        fixture("fast_mode.jsonl"),
    )
    .unwrap();

    let out = scan::scan(&home);
    assert_eq!(out.calls.len(), 2);

    // resp-fast-1 (10:00:05, APRES settings fast) -> Fast, x2.5.
    let fast = out
        .calls
        .iter()
        .find(|c| c.response_id.as_deref() == Some("resp-fast-1"))
        .unwrap();
    assert_eq!(fast.service_tier, ServiceTier::Fast);
    let engine = PricingEngine::embedded();
    let cd = engine.cost_codex(fast);
    assert!(cd.fast_applied);
    // astra standard : (1000*10 + 100*50)/1e6 = $0.015 ; fast x2.5.
    assert!((cd.equivalent_cost - 0.0375).abs() < 1e-9);

    // resp-fast-2 (09:59:00, AVANT l'event settings) -> Unknown, pas de fast.
    let before = out
        .calls
        .iter()
        .find(|c| c.response_id.as_deref() == Some("resp-fast-2"))
        .unwrap();
    assert_eq!(before.service_tier, ServiceTier::Unknown);
    let cd_before = engine.cost_codex(before);
    assert!(!cd_before.fast_applied);
    assert!((cd_before.equivalent_cost - 0.03).abs() < 1e-9);

    // Confiance tarifaire : 1 call determiné / 2.
    let conf = codex_meter::pricing::pricing_confidence(&out.calls);
    assert!((conf - 50.0).abs() < 1e-9);
    let stats = codex_meter::pricing::tier_stats(&out.calls);
    assert_eq!(stats, (1, 0, 1));

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn tier_db_fallback_across_files() {
    let home = std::env::temp_dir().join(format!("codex-meter-tierdb-{}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    let dir = home.join("sessions/2026/09/15");
    fs::create_dir_all(&dir).unwrap();
    // Fichier 1 : settings fast seulement (import 1).
    fs::write(
        dir.join("rollout-settings.jsonl"),
        fixture("thread_settings_only.jsonl"),
    )
    .unwrap();

    let engine = PricingEngine::embedded();
    let db = home.join("meter.sqlite");
    import::import(&home, &db, &engine, false, false).unwrap();

    // Fichier 2 : un appel du meme thread, sans event settings (import 2).
    let call_only = r#"{"timestamp":"2026-09-15T11:30:00.000Z","type":"session_meta","payload":{"id":"sess-tieronly","timestamp":"2026-09-15T11:30:00.000Z","cwd":"/Users/fmahia/Dev/TierProj","originator":"Codex CLI","cli_version":"0.156.0"}}
{"timestamp":"2026-09-15T11:30:02.000Z","type":"token_usage_record","payload":{"session_id":"sess-tieronly","thread_id":"thread-tier","turn_id":"turn-t","root_turn_id":"turn-t","response_id":"resp-tier-1","usage":{"input_tokens":1000,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":100,"reasoning_output_tokens":50,"total_tokens":1100}}}
"#;
    fs::write(dir.join("rollout-call.jsonl"), call_only).unwrap();
    import::import(&home, &db, &engine, false, false).unwrap();

    let conn = store::open_db(&db).unwrap();
    let tier: String = conn
        .query_row(
            "SELECT service_tier FROM inference_calls WHERE response_id = 'resp-tier-1'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tier, "fast", "fallback DB : settings anterieurs au call");

    // Un appel ANTERIEUR au settings en base ne doit pas etre requalifie.
    let early = r#"{"timestamp":"2026-09-15T10:30:00.000Z","type":"session_meta","payload":{"id":"sess-tieronly","timestamp":"2026-09-15T10:30:00.000Z","cwd":"/Users/fmahia/Dev/TierProj","originator":"Codex CLI","cli_version":"0.156.0"}}
{"timestamp":"2026-09-15T10:30:02.000Z","type":"token_usage_record","payload":{"session_id":"sess-tieronly","thread_id":"thread-tier","turn_id":"turn-t2","root_turn_id":"turn-t2","response_id":"resp-tier-early","usage":{"input_tokens":500,"cached_input_tokens":0,"cache_write_input_tokens":0,"output_tokens":50,"reasoning_output_tokens":25,"total_tokens":550}}}
"#;
    fs::write(dir.join("rollout-call-early.jsonl"), early).unwrap();
    import::import(&home, &db, &engine, false, false).unwrap();
    let tier_early: String = conn
        .query_row(
            "SELECT service_tier FROM inference_calls WHERE response_id = 'resp-tier-early'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(tier_early, "unknown", "pas de retroactivite via le fallback DB");

    let _ = fs::remove_dir_all(&home);
}

#[test]
fn tier_unknown_by_default_without_settings() {
    let home = std::env::temp_dir().join(format!("codex-meter-tieru-{}", std::process::id()));
    let _ = fs::remove_dir_all(&home);
    let dir = home.join("sessions/2026/09/15");
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join("rollout-plain.jsonl"),
        fixture("current_token_usage_record.jsonl"),
    )
    .unwrap();
    let out = scan::scan(&home);
    assert!(out
        .calls
        .iter()
        .all(|c: &InferenceCall| c.service_tier == ServiceTier::Unknown));
    assert_eq!(
        codex_meter::pricing::pricing_confidence(&out.calls),
        0.0,
        "aucune confiance sans tier determine (invariant : pas de supposition standard)"
    );
    let _ = fs::remove_dir_all(&home);
}

#[test]
fn util_iso_roundtrip_dates() {
    // Garde-fou du helper de temps utilise par les imports/checkpoints.
    assert_eq!(util::iso_utc(0), "1970-01-01T00:00:00Z");
    assert_eq!(util::iso_utc(1_758_196_800), "2025-09-18T12:00:00Z");
}
