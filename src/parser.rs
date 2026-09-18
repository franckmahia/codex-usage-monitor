use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::Path;

use serde_json::Value;

use crate::model::{
    FileParseResult, RawUsageEvent, SessionMeta, SourceFormat, TokenUsage,
};

/// Longueur maximale sauvegardee pour un message d'erreur de ligne.
const MAX_ERR: usize = 240;

#[derive(serde::Deserialize)]
struct RolloutLine {
    #[serde(default)]
    timestamp: Option<String>,
    #[serde(rename = "type")]
    kind: String,
    payload: Value,
}

#[derive(serde::Deserialize)]
struct TokenUsageRecordPayload {
    #[serde(default)]
    session_id: Option<String>,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    turn_id: Option<String>,
    #[serde(default)]
    root_turn_id: Option<String>,
    #[serde(default)]
    response_id: Option<String>,
    usage: TokenUsage,
    // turn_token_usage et thread_token_usage sont volontairement ignores :
    // ce sont des cumuls, jamais une source d'agregation (invariants 2 et 3).
}

#[derive(serde::Deserialize)]
struct SessionMetaPayload {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    originator: Option<String>,
    #[serde(default)]
    cli_version: Option<String>,
}

#[derive(serde::Deserialize)]
struct TurnContextPayload {
    turn_id: String,
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    cwd: Option<String>,
    #[serde(default)]
    root_turn_id: Option<String>,
}

#[derive(serde::Deserialize)]
struct LegacyInfo {
    #[serde(default)]
    last_token_usage: Option<TokenUsage>,
    #[serde(default)]
    total_token_usage: Option<TokenUsage>,
}

/// Parse un fichier rollout JSONL en streaming ligne a ligne.
/// Ne charge jamais le fichier entier en memoire. Un record invalide
/// produit une erreur locale et le parsing continue.
pub fn parse_file(path: &Path) -> FileParseResult {
    let mut out = FileParseResult::default();
    let mut meta_seen = false;

    let file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            out.open_failed = true;
            out.errors
                .push(format!("open failed: {} ({e})", path.display()));
            return out;
        }
    };
    let reader = BufReader::with_capacity(256 * 1024, file);

    for (ordinal, line) in reader.lines().enumerate() {
        out.counters.lines_total += 1;
        let line = match line {
            Ok(l) => l,
            Err(e) => {
                out.errors.push(format!("read failed: {e}"));
                break;
            }
        };
        let ordinal = ordinal as u64;

        // Fast path : ignorer sans parser JSON les lignes qui ne contiennent
        // aucun des marqueurs qui nous interessent (messages, tool outputs...).
        if !(line.contains("\"token_usage_record\"")
            || line.contains("\"turn_context\"")
            || line.contains("\"token_count\"")
            || line.contains("\"session_meta\""))
        {
            continue;
        }

        let parsed: RolloutLine = match serde_json::from_str(&line) {
            Ok(p) => p,
            Err(e) => {
                out.counters.lines_json_errors += 1;
                out.errors
                    .push(format!("line {ordinal}: JSON error: {e}"));
                continue;
            }
        };

        match parsed.kind.as_str() {
            "session_meta" => {
                if !meta_seen {
                    meta_seen = true;
                    if let Ok(m) = serde_json::from_value::<SessionMetaPayload>(parsed.payload) {
                        out.meta = SessionMeta {
                            session_id: m.id,
                            cwd: m.cwd,
                            originator: m.originator,
                            cli_version: m.cli_version,
                        };
                    }
                }
            }
            "turn_context" => {
                if let Ok(t) = serde_json::from_value::<TurnContextPayload>(parsed.payload) {
                    if let Some(model) = &t.model {
                        out.turn_models.insert(t.turn_id.clone(), model.clone());
                        if let Some(root) = &t.root_turn_id {
                            // un seul modele par root pour attribution "inferred"
                            match out.root_models.get(root) {
                                Some(prev) if prev != model => {
                                    out.root_models.remove(root);
                                }
                                _ => {
                                    out.root_models.insert(root.clone(), model.clone());
                                }
                            }
                        }
                    }
                    if let Some(cwd) = t.cwd {
                        out.turn_cwd.insert(t.turn_id, cwd);
                    }
                }
            }
            "token_usage_record" => {
                match serde_json::from_value::<TokenUsageRecordPayload>(parsed.payload) {
                    Ok(p) => {
                        if p.usage.subset_violation() {
                            out.counters.subset_violations += 1;
                        }
                        if p.response_id.is_none() {
                            out.counters.records_without_response_id += 1;
                        }
                        out.counters.usage_records += 1;
                        out.primary_events += 1;
                        out.raw_calls.push(RawUsageEvent {
                            timestamp_utc: parsed.timestamp,
                            session_id: p.session_id,
                            thread_id: p.thread_id,
                            turn_id: p.turn_id,
                            root_turn_id: p.root_turn_id,
                            response_id: p.response_id,
                            usage: p.usage,
                            source_format: SourceFormat::TokenUsageRecord,
                            source_ordinal: ordinal,
                        });
                    }
                    Err(e) => {
                        out.counters.lines_json_errors += 1;
                        out.errors
                            .push(format!("line {ordinal}: malformed token_usage_record: {e}"));
                    }
                }
            }
            "event_msg" => {
                // Fallback ancien format : event_msg payload.type == "token_count".
                let is_token_count = parsed
                    .payload
                    .get("type")
                    .and_then(|v| v.as_str())
                    .map(|s| s == "token_count")
                    .unwrap_or(false);
                if !is_token_count {
                    continue;
                }
                out.counters.legacy_token_count_events += 1;
                let info = parsed.payload.get("info");
                if info.is_none() || info == Some(&Value::Null) {
                    out.counters.legacy_null_info += 1;
                    continue;
                }
                match serde_json::from_value::<LegacyInfo>(info.unwrap().clone()) {
                    Ok(info) => {
                        if info.total_token_usage.is_some() {
                            out.counters.legacy_total_usage_present += 1;
                        }
                        if let Some(last) = info.last_token_usage {
                            if last.subset_violation() {
                                out.counters.subset_violations += 1;
                            }
                            out.legacy_events += 1;
                            out.legacy_totals.add(&last);
                            out.raw_calls.push(RawUsageEvent {
                                timestamp_utc: parsed.timestamp,
                                session_id: out.meta.session_id.clone(),
                                thread_id: None,
                                turn_id: None,
                                root_turn_id: None,
                                response_id: None,
                                usage: last,
                                source_format: SourceFormat::LegacyTokenCount,
                                source_ordinal: ordinal,
                            });
                        }
                    }
                    Err(e) => {
                        out.counters.lines_json_errors += 1;
                        out.errors
                            .push(format!("line {ordinal}: malformed token_count info: {e}"));
                    }
                }
            }
            _ => {
                // Type inconnu : ignore silencieusement (resilience), visible
                // via les compteurs de lignes du diagnostics.
            }
        }
    }
    out
}

/// Resolve le modele d'un evenement : exact (turn_id), puis inferred
/// (root_turn_id unique), sinon unknown. Ne jamais inventer un modele.
pub fn resolve_model(
    event: &RawUsageEvent,
    turn_models: &HashMap<String, String>,
    root_models: &HashMap<String, String>,
) -> (Option<String>, crate::model::ModelConfidence) {
    use crate::model::ModelConfidence;
    if let Some(turn) = &event.turn_id {
        if let Some(model) = turn_models.get(turn) {
            return (Some(model.clone()), ModelConfidence::Exact);
        }
    }
    if let Some(root) = &event.root_turn_id {
        if let Some(model) = root_models.get(root) {
            return (Some(model.clone()), ModelConfidence::Inferred);
        }
    }
    (None, ModelConfidence::Unknown)
}

/// Fallback de cle de deduplication : SHA256(session|thread|turn|timestamp|
/// input|cached|output|ordinal). Utilise quand response_id est absent.
pub fn event_uid(event: &RawUsageEvent, source_file: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(source_file.as_bytes());
    hasher.update(b"|");
    hasher.update(event.session_id.as_deref().unwrap_or("").as_bytes());
    hasher.update(b"|");
    hasher.update(event.thread_id.as_deref().unwrap_or("").as_bytes());
    hasher.update(b"|");
    hasher.update(event.turn_id.as_deref().unwrap_or("").as_bytes());
    hasher.update(b"|");
    hasher.update(event.timestamp_utc.as_deref().unwrap_or("").as_bytes());
    hasher.update(b"|");
    hasher.update(event.usage.input_tokens.to_le_bytes());
    hasher.update(event.usage.cached_input_tokens.to_le_bytes());
    hasher.update(event.usage.output_tokens.to_le_bytes());
    hasher.update(event.source_ordinal.to_le_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn truncate_err(s: &str) -> String {
    if s.len() > MAX_ERR {
        format!("{}…", &s[..MAX_ERR])
    } else {
        s.to_string()
    }
}
