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
        serde_json::from_str(include_str!("../pricing/openai-codex-2026-09-23.json")).unwrap();
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
        assert_eq!(r.len(), 28);
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

#[test]
fn load_calls_with_single_bound_works() {
    // Regression : --from seul fournissait 2 params pour 1 placeholder
    // (« Got 2, needed 1 »), meme chose pour --to seul et l UI.
    let h = TempHome::new(
        "singlebound",
        &[
            ("2026/09/15", "current_token_usage_record.jsonl"),
            ("2026/09/03", "legacy_token_count.jsonl"),
        ],
    );
    let engine = PricingEngine::embedded();
    import::import(&h.home, &h.db, &engine, false, false).unwrap();

    let from_only = import::load_calls(&h.db, Some("2026-09-11"), None).unwrap();
    assert_eq!(from_only.len(), 2, "from seul : seulement les primaires");

    let to_only = import::load_calls(&h.db, None, Some("2026-09-10")).unwrap();
    assert_eq!(to_only.len(), 2, "to seul : seulement les legacy");

    let none = import::load_calls(&h.db, None, None).unwrap();
    assert_eq!(none.len(), 4);
}

#[test]
fn reprice_keeps_service_tier_fast_multiplier() {
    // Regression : reprice_all reconstruisait les appels en tier Unknown et
    // perdait le multiplicateur fast dans pricing_results.
    let h = TempHome::new("repricefast", &[("2026/09/15", "fast_mode.jsonl")]);
    let engine = PricingEngine::embedded();
    import::import(&h.home, &h.db, &engine, false, false).unwrap();

    let cost_for = |db: &std::path::Path, response_id: &str| -> f64 {
        let conn = store::open_db(db).unwrap();
        conn.query_row(
            "SELECT p.equivalent_cost FROM pricing_results p
             JOIN inference_calls i ON i.event_uid = p.event_uid
             WHERE i.response_id = ?1",
            [response_id],
            |r| r.get(0),
        )
        .unwrap()
    };

    // resp-fast-1 : astra 1000 in / 100 out, fast x2.5 => 0.0375.
    let before = cost_for(&h.db, "resp-fast-1");
    assert!((before - 0.0375).abs() < 1e-9, "attendu 0.0375, obtenu {before}");

    let conn = store::open_db(&h.db).unwrap();
    let n = store::reprice_all(&conn, &engine).unwrap();
    assert_eq!(n, 2);

    let after = cost_for(&h.db, "resp-fast-1");
    assert!(
        (after - 0.0375).abs() < 1e-9,
        "le re-pricing ne doit pas perdre le tier fast : {after}"
    );
    // resp-fast-2 (avant l'event settings) reste Unknown => tarif standard.
    let unknown = cost_for(&h.db, "resp-fast-2");
    assert!((unknown - 0.03).abs() < 1e-9, "tier inconnu = standard : {unknown}");
}

fn legacy_fixture_for_primary_session() -> String {
    fixture("legacy_token_count.jsonl").replace("sess-legacy", "sess-primary")
}

#[test]
fn legacy_rows_pruned_when_primary_session_arrives() {
    // Ordre legacy d'abord, puis format moderne pour la meme session :
    // les lignes legacy deja inserees doivent etre purgees (invariant 7).
    let h = TempHome::new("prune", &[]);
    let engine = PricingEngine::embedded();
    let d_old = h.home.join("sessions/2026/09/03");
    fs::create_dir_all(&d_old).unwrap();
    fs::write(
        d_old.join("rollout-legacy.jsonl"),
        legacy_fixture_for_primary_session(),
    )
    .unwrap();

    let rep1 = import::import(&h.home, &h.db, &engine, false, false).unwrap();
    assert_eq!(rep1.calls_inserted, 2, "legacy compte tant que seul");
    let (_, t1) = h.db_totals();
    assert_eq!(t1.input_tokens, 48_619 + 48_620);

    let d_new = h.home.join("sessions/2026/09/15");
    fs::create_dir_all(&d_new).unwrap();
    fs::write(
        d_new.join("rollout-primary.jsonl"),
        fixture("current_token_usage_record.jsonl"),
    )
    .unwrap();
    let rep2 = import::import(&h.home, &h.db, &engine, false, false).unwrap();
    assert_eq!(rep2.legacy_pruned, 2, "les 2 lignes legacy de la session sont purgees");

    let (n, t2) = h.db_totals();
    assert_eq!(n, 2, "seuls les appels primaires restent");
    assert_eq!(t2.input_tokens, 30_000);
}

#[test]
fn legacy_import_skipped_when_session_already_primary() {
    // Ordre inverse : la session est deja couverte en moderne, le fichier
    // legacy subsequant ne doit rien inserer.
    let h = TempHome::new("skiplegacy", &[]);
    let engine = PricingEngine::embedded();
    let d_new = h.home.join("sessions/2026/09/15");
    fs::create_dir_all(&d_new).unwrap();
    fs::write(
        d_new.join("rollout-primary.jsonl"),
        fixture("current_token_usage_record.jsonl"),
    )
    .unwrap();
    import::import(&h.home, &h.db, &engine, false, false).unwrap();

    let d_old = h.home.join("sessions/2026/09/03");
    fs::create_dir_all(&d_old).unwrap();
    fs::write(
        d_old.join("rollout-legacy.jsonl"),
        legacy_fixture_for_primary_session(),
    )
    .unwrap();
    let rep = import::import(&h.home, &h.db, &engine, false, false).unwrap();
    assert_eq!(rep.calls_inserted, 0, "legacy ignore pour une session primaire");

    let (n, t) = h.db_totals();
    assert_eq!(n, 2);
    assert_eq!(t.input_tokens, 30_000);
}

fn uid_set(db: &std::path::Path) -> std::collections::HashSet<String> {
    let conn = store::open_db(db).unwrap();
    let mut stmt = conn.prepare("SELECT event_uid FROM inference_calls").unwrap();
    let rows = stmt
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    rows.into_iter().collect()
}

fn scan_uid_set(home: &std::path::Path) -> std::collections::HashSet<String> {
    codex_meter::scan::scan(home)
        .calls
        .into_iter()
        .map(|c| c.event_uid)
        .collect()
}

#[test]
fn trailing_newline_arrival_keeps_ordinals_aligned_with_scan() {
    // Une derniere ligne acceptee SANS \n, puis l'arrivee du seul \n, ne
    // doit pas compter une ligne fantome : les event_uid (ordinaux) doivent
    // rester identiques a ceux du scan.
    let h = TempHome::new("phantom", &[]);
    let engine = PricingEngine::embedded();
    let full = fixture("current_token_usage_record.jsonl");
    let dir = h.home.join("sessions/2026/09/15");
    fs::create_dir_all(&dir).unwrap();
    let path = dir.join("rollout-phantom.jsonl");
    let trimmed = full.strip_suffix('\n').unwrap_or(&full);
    fs::write(&path, trimmed).unwrap();

    let rep1 = import::import(&h.home, &h.db, &engine, false, false).unwrap();
    assert_eq!(rep1.calls_inserted, 2);

    // Le \n final arrive seul :1 octet, aucun nouvel evenement.
    let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
    std::io::Write::write_all(&mut f, b"\n").unwrap();
    drop(f);
    let rep2 = import::import(&h.home, &h.db, &engine, false, false).unwrap();
    assert_eq!(rep2.calls_inserted, 0, "le \\n ne cree aucune ligne");
    assert_eq!(rep2.duplicates_ignored, 0);

    // Un nouveau record ensuite : ordinal correct, modele resolu via
    // l'enrichissement du checkpoint (turn_context est deja passe).
    let new_line = concat!(
        r#"{"timestamp":"2026-09-15T08:03:00.000Z","type":"token_usage_record","#,
        r#""payload":{"session_id":"sess-primary","thread_id":"thread-1","turn_id":"turn-1","#,
        r#""root_turn_id":"turn-1","response_id":"resp-3","#,
        r#""usage":{"input_tokens":5000,"cached_input_tokens":4000,"cache_write_input_tokens":0,"#,
        r#""output_tokens":500,"reasoning_output_tokens":300,"total_tokens":5500}}}"#,
    );
    let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
    std::io::Write::write_all(&mut f, new_line.as_bytes()).unwrap();
    std::io::Write::write_all(&mut f, b"\n").unwrap();
    drop(f);
    let rep3 = import::import(&h.home, &h.db, &engine, false, false).unwrap();
    assert_eq!(rep3.calls_inserted, 1);

    let conn = store::open_db(&h.db).unwrap();
    let (model, conf): (String, String) = conn
        .query_row(
            "SELECT model_slug, model_confidence FROM inference_calls WHERE response_id='resp-3'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(model, "gpt-6-astra", "turn_context passe le checkpoint : enrichment conserve");
    assert_eq!(conf, "exact");

    // Les uid de la base sont exactement ceux d'un scan frais.
    assert_eq!(uid_set(&h.db), scan_uid_set(&h.home));
}

#[test]
fn legacy_appended_to_primary_file_never_counts() {
    // Segment incremental ne contenant QUE du token_count dans un fichier
    // deja primaire : le drapeau primaire est un fait de fichier, pas de
    // segment (sinon la ligne legacy s'ajoute aux records modernes).
    let h = TempHome::new("segmix", &[("2026/09/15", "current_token_usage_record.jsonl")]);
    let engine = PricingEngine::embedded();
    import::import(&h.home, &h.db, &engine, false, false).unwrap();
    let (_, t1) = h.db_totals();
    assert_eq!(t1.input_tokens, 30_000);

    let path = h
        .home
        .join("sessions/2026/09/15/rollout-current_token_usage_record.jsonl");
    let legacy_line = concat!(
        r#"{"timestamp":"2026-09-15T08:04:00.000Z","type":"event_msg","#,
        r#""payload":{"type":"token_count","info":{"last_token_usage":{"#,
        r#""input_tokens":111111,"cached_input_tokens":0,"cache_write_input_tokens":0,"#,
        r#""output_tokens":111,"reasoning_output_tokens":11,"total_tokens":111222}}}}"#,
    );
    let mut f = fs::OpenOptions::new().append(true).open(&path).unwrap();
    std::io::Write::write_all(&mut f, legacy_line.as_bytes()).unwrap();
    std::io::Write::write_all(&mut f, b"\n").unwrap();
    drop(f);

    let rep = import::import(&h.home, &h.db, &engine, false, false).unwrap();
    assert_eq!(rep.calls_inserted, 0, "segment legacy dans un fichier primaire = ignore");

    let (n, t2) = h.db_totals();
    assert_eq!(n, 2);
    assert_eq!(t2.input_tokens, 30_000, "jamais 30_000 + 111_111");
    assert_eq!(uid_set(&h.db), scan_uid_set(&h.home));
}

#[test]
fn full_rebuild_matches_scan_uids() {
    // --full reconstruit depuis zero : les uid doivent coincider avec un
    // scan frais (aucun residue d'ancien checkpoint).
    let h = TempHome::new(
        "rebuild",
        &[
            ("2026/09/15", "current_token_usage_record.jsonl"),
            ("2026/09/03", "legacy_token_count.jsonl"),
            ("2026/09/15", "cache_write.jsonl"),
        ],
    );
    let engine = PricingEngine::embedded();
    import::import(&h.home, &h.db, &engine, false, false).unwrap();
    let (n_before, before) = h.db_totals();
    assert_eq!(n_before, 5, "2 primaires + 2 legacy + 1 cache_write");

    let rep = import::import(&h.home, &h.db, &engine, true, false).unwrap();
    assert_eq!(rep.calls_inserted, 5, "rebuild reimporte tous les appels");
    let (n, after) = h.db_totals();
    assert_eq!(n, 5, "rebuild = meme nombre d appels");
    assert_eq!(after.input_tokens, before.input_tokens);
    assert_eq!(uid_set(&h.db), scan_uid_set(&h.home));
}
