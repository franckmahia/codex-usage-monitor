use std::collections::HashMap;
use std::path::Path;

use rusqlite::params;

use crate::store::open_state_readonly;

#[derive(Debug, Clone, Default)]
pub struct StateThread {
    pub id: String,
    pub title: Option<String>,
    pub model: Option<String>,
    pub cwd: Option<String>,
    pub project_name: Option<String>,
    pub thread_source: Option<String>,
    pub activity: String,
    pub parent_thread_id: Option<String>,
    pub created_at_ms: Option<i64>,
    pub updated_at_ms: Option<i64>,
    pub state_tokens_used: Option<i64>,
    pub rollout_path: Option<String>,
}

/// Classification d'activite sans double comptage, depuis thread_source,
/// le JSON source (guardian / thread_spawn) et le modele.
pub fn classify(thread_source: Option<&str>, source: &str, model: Option<&str>) -> &'static str {
    let ts = thread_source.unwrap_or("");
    if ts == "guardian_review" || source.contains("guardian") {
        return "auto_review";
    }
    match ts {
        "subagent" | "agent_created_thread" => "subagent",
        "user" => "main",
        "voice_chat" | "realtime_voice" => "voice",
        _ => {
            if model == Some("codex-auto-review") {
                "auto_review"
            } else {
                "other"
            }
        }
    }
}

fn extract_parent_from_source(source: &str) -> Option<String> {
    let v: serde_json::Value = serde_json::from_str(source).ok()?;
    v.get("subagent")?
        .get("thread_spawn")?
        .get("parent_thread_id")?
        .as_str()
        .map(|s| s.to_string())
}

/// Lit les threads de state_5.sqlite (LECTURE SEULE, jamais d'ecriture).
/// Toute erreur => None (l'enrichissement est facultatif, jamais bloquant).
pub fn read_threads(state_db: &Path) -> Option<Vec<StateThread>> {
    let conn = open_state_readonly(state_db).ok()?;

    let mut parents: HashMap<String, String> = HashMap::new();
    if let Ok(mut stmt) = conn.prepare("SELECT child_thread_id, parent_thread_id FROM thread_spawn_edges") {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }) {
            for r in rows.flatten() {
                parents.insert(r.0, r.1);
            }
        }
    }

    let mut projects: HashMap<String, String> = HashMap::new();
    if let Ok(mut stmt) = conn.prepare("SELECT id, name FROM projects") {
        if let Ok(rows) = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }) {
            for r in rows.flatten() {
                projects.insert(r.0, r.1);
            }
        }
    }

    let mut stmt = conn
        .prepare(
            "SELECT id, title, name, model, cwd, project_id, thread_source, source,
                    created_at_ms, updated_at_ms, tokens_used, rollout_path
             FROM threads",
        )
        .ok()?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<i64>>(8)?,
                row.get::<_, Option<i64>>(9)?,
                row.get::<_, Option<i64>>(10)?,
                row.get::<_, Option<String>>(11)?,
            ))
        })
        .ok()?;

    let mut out = Vec::new();
    for r in rows.flatten() {
        let (
            id,
            title,
            name,
            model,
            cwd,
            project_id,
            thread_source,
            source,
            created_at_ms,
            updated_at_ms,
            tokens_used,
            rollout_path,
        ) = r;
        let title = Some(title)
            .filter(|t| !t.trim().is_empty())
            .or(name)
            .filter(|t| !t.trim().is_empty());
        let project_name = project_id
            .as_deref()
            .and_then(|pid| projects.get(pid).cloned())
            .or_else(|| {
                cwd.as_deref().map(|c| {
                    c.rsplit('/')
                        .find(|s| !s.is_empty())
                        .unwrap_or(c)
                        .to_string()
                })
            });
        let activity = classify(thread_source.as_deref(), source.as_deref().unwrap_or(""), model.as_deref()).to_string();
        let parent_thread_id = parents
            .get(&id)
            .cloned()
            .or_else(|| source.as_deref().and_then(extract_parent_from_source));
        out.push(StateThread {
            id,
            title,
            model,
            cwd,
            project_name,
            thread_source,
            activity,
            parent_thread_id,
            created_at_ms,
            updated_at_ms,
            state_tokens_used: tokens_used,
            rollout_path,
        });
    }
    Some(out)
}

/// Comptages d'activite depuis la table threads de state db (diagnostic).
pub fn thread_activity_counts(state_db: &Path) -> Option<HashMap<String, u64>> {
    let threads = read_threads(state_db)?;
    let mut counts: HashMap<String, u64> = HashMap::new();
    for t in threads {
        *counts.entry(t.activity).or_default() += 1;
    }
    Some(counts)
}

/// Insere/rafraichit les threads dans NOTRE base, puis propage l'enrichissement
/// vers inference_calls (activity, parent, titre) et infere le modele des
/// appels legacy depuis le modele du thread. Retourne le nombre de lignes
/// de calls modifiees.
pub fn enrich_calls(
    conn: &rusqlite::Connection,
    state_db: &Path,
) -> Result<(usize, u64), String> {
    let threads = match read_threads(state_db) {
        Some(t) => t,
        None => return Ok((0, 0)),
    };
    for t in &threads {
        conn.execute(
            "INSERT OR REPLACE INTO threads(
                thread_id, title, model, cwd, project_name, thread_source,
                activity, parent_thread_id, created_at_ms, updated_at_ms,
                state_tokens_used, rollout_path
             ) VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12)",
            params![
                t.id,
                t.title,
                t.model,
                t.cwd,
                t.project_name,
                t.thread_source,
                t.activity,
                t.parent_thread_id,
                t.created_at_ms,
                t.updated_at_ms,
                t.state_tokens_used,
                t.rollout_path
            ],
        )
        .map_err(|e| format!("upsert thread: {e}"))?;
    }

    let mut touched: u64 = 0;
    for sql in [
        "UPDATE inference_calls
         SET activity = t.activity,
             parent_thread_id = COALESCE(t.parent_thread_id, inference_calls.parent_thread_id),
             thread_title = t.title
         FROM threads t
         WHERE inference_calls.thread_id = t.thread_id",
        // Attribution "inferred" du modele des appels legacy depuis le modele
        // du thread (state db). Jamais pour les appels primaires.
        "UPDATE inference_calls
         SET model_slug = t.model, model_confidence = 'inferred'
         FROM threads t
         WHERE COALESCE(inference_calls.thread_id, inference_calls.session_id) = t.thread_id
           AND inference_calls.model_slug IS NULL
           AND inference_calls.source_format = 'legacy_token_count'
           AND t.model IS NOT NULL",
        "UPDATE inference_calls
         SET project_path = t.cwd
         FROM threads t
         WHERE COALESCE(inference_calls.thread_id, inference_calls.session_id) = t.thread_id
           AND inference_calls.project_path IS NULL
           AND t.cwd IS NOT NULL",
    ] {
        conn.execute(sql, []).map_err(|e| format!("enrich: {e}"))?;
        touched += conn.changes() as u64;
    }
    Ok((threads.len(), touched))
}
