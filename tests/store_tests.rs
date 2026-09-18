use std::fs;
use std::path::{Path, PathBuf};

use codex_meter::import::{self};
use codex_meter::pricing::PricingEngine;
use codex_meter::store;

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

fn fixture(name: &str) -> String {
    fs::read_to_string(fixtures_dir().join(name)).expect("fixture exists")
}

struct TempHome {
    home: PathBuf,
    db: PathBuf,
}

impl TempHome {
    fn new(tag: &str, layout: &[(&str, &str)]) -> Self {
        let home = std::env::temp_dir().join(format!(
            "codex-meter-db-{tag}-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&home);
        for (subdir, fixture_name) in layout {
            let dir = home.join("sessions").join(subdir);
            fs::create_dir_all(&dir).unwrap();
            fs::write(
                dir.join(format!("rollout-{fixture_name}")),
                fixture(fixture_name),
            )
            .unwrap();
        }
        TempHome {
            db: home.join("meter.sqlite"),
            home,
        }
    }

    fn db_totals(&self) -> (u64, codex_meter::model::TokenUsage) {
        let conn = store::open_db(&self.db).unwrap();
        store::db_totals(&conn).unwrap()
    }

    fn checkpoint(&self, rel: &str) -> Option<store::Checkpoint> {
        let conn = store::open_db(&self.db).unwrap();
        store::get_checkpoint(&conn, rel)
    }
}

impl Drop for TempHome {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.home);
    }
}

#[test]
fn double_import_is_idempotent() {
    let h = TempHome::new(
        "idem",
        &[
            ("2026/09/15", "current_token_usage_record.jsonl"),
            ("2026/09/16", "duplicate_active_archived.jsonl"),
            ("2026/09/15", "cache_write.jsonl"),
        ],
    );
    let engine = PricingEngine::embedded();
    let rep1 = import::import(&h.home, &h.db, &engine, false, false).unwrap();
    assert_eq!(rep1.calls_inserted, 4); // 2 primary + 1 dup-file + 1 cache_write
    let rep2 = import::import(&h.home, &h.db, &engine, false, false).unwrap();
    assert_eq!(rep2.files_skipped, 3, "rien n'a change : tout skip");
    assert_eq!(rep2.calls_inserted, 0);
    let (n, t) = h.db_totals();
    assert_eq!(n, 4);
    assert_eq!(t.input_tokens, 30_000 + 5_000 + 10_000);
    assert_eq!(rep1.duplicates_ignored, 1, "resp-dup-1 deux fois dans son fichier");
}

#[test]
fn append_resume_imports_only_new_bytes() {
    let h = TempHome::new("resume", &[]);
    let engine = PricingEngine::embedded();

    // Copie partielle : la premiere ligne complete seulement.
    let full = fixture("current_token_usage_record.jsonl");
    let cutoff = full.find("\"turn_context\"").unwrap();
    let dir = h.home.join("sessions/2026/09/15");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rollout-resume.jsonl");
    fs::write(&path, &full[..cutoff]).unwrap();

    // Import d'un fichier dont la derniere ligne est eventuellement tronquee :
    // on coupe avant le \n de la ligne session_meta pour simuler une ecriture
    // partielle, puis on complete.
    let first_line_end = full.find('\n').unwrap() + 1;
    fs::write(&path, &full[..first_line_end - 1]).unwrap(); // sans \n final
    let rep1 = import::import(&h.home, &h.db, &engine, false, false).unwrap();
    assert_eq!(rep1.calls_inserted, 0);
    let cp1 = h.checkpoint("sessions/2026/09/15/rollout-resume.jsonl").unwrap();
    let held_offset = cp1.last_complete_offset;

    // On complete le fichier.
    fs::write(&path, &full).unwrap();
    let rep2 = import::import(&h.home, &h.db, &engine, false, false).unwrap();
    assert_eq!(rep2.calls_inserted, 2);
    let cp2 = h.checkpoint("sessions/2026/09/15/rollout-resume.jsonl").unwrap();
    assert!(cp2.last_complete_offset > held_offset);
    let (_, t) = h.db_totals();
    assert_eq!(t.input_tokens, 30_000);
    // Repasse finale : rien de nouveau.
    let rep3 = import::import(&h.home, &h.db, &engine, false, false).unwrap();
    assert_eq!(rep3.calls_inserted, 0);
}

#[test]
fn moved_to_archived_never_double_counts() {
    let h = TempHome::new("move", &[("2026/09/15", "current_token_usage_record.jsonl")]);
    let engine = PricingEngine::embedded();
    let rep1 = import::import(&h.home, &h.db, &engine, false, false).unwrap();
    assert_eq!(rep1.calls_inserted, 2);

    // Deplacement sessions/ -> archived_sessions/ (codex le fait regulierement).
    let src = h.home.join("sessions/2026/09/15/rollout-current_token_usage_record.jsonl");
    let dst_dir = h.home.join("archived_sessions/2026/09/15");
    fs::create_dir_all(&dst_dir).unwrap();
    fs::rename(&src, dst_dir.join("rollout-current_token_usage_record.jsonl")).unwrap();

    let rep2 = import::import(&h.home, &h.db, &engine, false, false).unwrap();
    assert_eq!(rep2.calls_inserted, 0, "meme event_uid apres deplacement");
    let (n, t) = h.db_totals();
    assert_eq!(n, 2);
    assert_eq!(t.input_tokens, 30_000);
}

#[test]
fn reprice_updates_costs_when_catalog_changes() {
    let h = TempHome::new("reprice", &[("2026/09/15", "cache_write.jsonl")]);
    let engine = PricingEngine::embedded();
    import::import(&h.home, &h.db, &engine, false, false).unwrap();

    let conn = store::open_db(&h.db).unwrap();
    let cost_before: f64 = conn
        .query_row(
            "SELECT COALESCE(SUM(equivalent_cost),0) FROM pricing_results",
            [],
            |r| r.get(0),
        )
        .unwrap();

    // Catalogue alternatif : input x2 pour sol.
    let alt_path = h.home.join("alt-codex.json");
    let mut alt: serde_json::Value =
        serde_json::from_str(include_str!("../pricing/openai-codex-2026-09-18.json")).unwrap();
    for rule in alt["rules"].as_array_mut().unwrap().iter_mut() {
        if rule["model"] == "gpt-5.6-sol" {
            rule["input_rate"] = serde_json::json!(8.0);
        }
    }
    alt["version"] = serde_json::json!("2026-09-18-alt");
    fs::write(&alt_path, serde_json::to_string(&alt).unwrap()).unwrap();
    let alt_engine = PricingEngine::load(alt_path.to_str().unwrap(), None).unwrap();

    import::import(&h.home, &h.db, &alt_engine, false, false).unwrap();
    let cost_after: f64 = conn
        .query_row(
            "SELECT COALESCE(SUM(equivalent_cost),0) FROM pricing_results",
            [],
            |r| r.get(0),
        )
        .unwrap();
    // ordinary 4000 passe de $4/M a $8/M => +0.016 exactement.
    assert!((cost_after - cost_before - 0.016).abs() < 1e-9);
}

#[test]
fn state_enrichment_activity_title_parent_model() {
    let h = TempHome::new(
        "enrich",
        &[
            ("2026/09/18", "auto_review.jsonl"),
            ("2026/09/15", "subagents.jsonl"),
            ("2026/09/03", "legacy_token_count.jsonl"),
        ],
    );
    let engine = PricingEngine::embedded();
    import::import(&h.home, &h.db, &engine, false, false).unwrap();

    // state_5.sqlite minimal mais avec les colonnes attendues par le lecteur.
    let state_path = h.home.join("state_5.sqlite");
    let conn = rusqlite::Connection::open(&state_path).unwrap();
    conn.execute_batch(
        r#"
        CREATE TABLE threads (
            id TEXT PRIMARY KEY, title TEXT NOT NULL, name TEXT,
            model TEXT, cwd TEXT, project_id TEXT, thread_source TEXT, source TEXT,
            created_at_ms INTEGER, updated_at_ms INTEGER, tokens_used INTEGER,
            rollout_path TEXT
        );
        CREATE TABLE thread_spawn_edges (
            parent_thread_id TEXT NOT NULL,
            child_thread_id TEXT NOT NULL PRIMARY KEY,
            status TEXT NOT NULL
        );
        CREATE TABLE projects (id TEXT PRIMARY KEY, name TEXT NOT NULL);
        INSERT INTO threads VALUES('01a0b4dd-3bc1-7c02-85ac-14bd10b6c1a2', 'Revue auto', NULL,
            'codex-auto-review', '/x/Nora', NULL, 'guardian_review', '{}', 0, 0, 1000, NULL);
        INSERT INTO threads VALUES('sess-sub', 'Thread principal', NULL,
            'gpt-6-astra', '/x/Iota', NULL, 'user', '{}', 0, 0, 5000, NULL);
        INSERT INTO threads VALUES('thread-subagent-1', 'Sous-agent A', NULL,
            'gpt-5.6-sol', '/x/Iota', NULL, 'subagent', '{}', 0, 0, 900, NULL);
        INSERT INTO threads VALUES('thread-subagent-2', 'Sous-agent B', NULL,
            'gpt-5.6-sol', '/x/Iota', NULL, 'subagent', '{}', 0, 0, 900, NULL);
        INSERT INTO threads VALUES('sess-legacy', 'Ancienne session', NULL,
            'gpt-5.4', '/x/Beta', NULL, 'user', '{}', 0, 0, 999, NULL);
        INSERT INTO thread_spawn_edges VALUES('sess-sub', 'thread-subagent-1', 'open');
        INSERT INTO thread_spawn_edges VALUES('sess-sub', 'thread-subagent-2', 'open');
        "#,
    )
    .unwrap();
    drop(conn);

    import::import(&h.home, &h.db, &engine, false, false).unwrap();

    let conn = store::open_db(&h.db).unwrap();
    let (activity, parent, title): (String, Option<String>, String) = conn
        .query_row(
            "SELECT activity, parent_thread_id, thread_title FROM inference_calls
             WHERE thread_id = 'thread-subagent-1'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap();
    assert_eq!(activity, "subagent");
    assert_eq!(parent.as_deref(), Some("sess-sub"));
    assert_eq!(title, "Sous-agent A");

    let auto: String = conn
        .query_row(
            "SELECT activity FROM inference_calls WHERE thread_id LIKE '01a0b4dd%'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(auto, "auto_review");

    // Legacy : modele infere depuis le thread, confiance "inferred".
    let (model, conf): (String, String) = conn
        .query_row(
            "SELECT model_slug, model_confidence FROM inference_calls
             WHERE source_format = 'legacy_token_count' LIMIT 1",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(model, "gpt-5.4");
    assert_eq!(conf, "inferred");
}

#[test]
fn csv_export_is_parseable() {
    let h = TempHome::new(
        "csv",
        &[
            ("2026/09/15", "current_token_usage_record.jsonl"),
            ("2026/09/15", "cache_write.jsonl"),
        ],
    );
    let engine = PricingEngine::embedded();
    import::import(&h.home, &h.db, &engine, false, false).unwrap();
    let calls = import::load_calls(&h.db, None, None).unwrap();
    assert_eq!(calls.len(), 3);
    // Enrichissement absent : activity = None => heuristique par modele.
    assert!(calls.iter().all(|c| c.activity.is_none()));

    let out_dir = h.home.join("export");
    let files = codex_meter::export::export_csv(&out_dir, &calls, &engine).unwrap();
    assert_eq!(files.len(), 6);

    let calls_csv = fs::read_to_string(out_dir.join("calls.csv")).unwrap();
    let mut rdr = csv::Reader::from_reader(calls_csv.as_bytes());
    let mut rows = 0;
    for r in rdr.records() {
        let r = r.unwrap();
        assert_eq!(r.len(), 27);
        rows += 1;
    }
    assert_eq!(rows, 3);

    let by_day = fs::read_to_string(out_dir.join("by-day.csv")).unwrap();
    assert!(by_day.starts_with("bucket,calls,input_tokens"));
    assert!(by_day.contains("day:2026-09-15"));
}

#[test]
fn period_filter_reads_from_db() {
    let h = TempHome::new(
        "period",
        &[
            ("2026/09/15", "current_token_usage_record.jsonl"),
            ("2026/09/03", "legacy_token_count.jsonl"),
        ],
    );
    let engine = PricingEngine::embedded();
    import::import(&h.home, &h.db, &engine, false, false).unwrap();
    let all = import::load_calls(&h.db, None, None).unwrap();
    assert_eq!(all.len(), 4);
    let kept = import::load_calls(&h.db, Some("2026-09-11"), Some("2026-09-18")).unwrap();
    assert_eq!(kept.len(), 2, "records legacy du 09/03 hors fenetre");
    assert!(kept.iter().all(|c| c.day() >= "2026-09-11" && c.day() <= "2026-09-18"));
}
