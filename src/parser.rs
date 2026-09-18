use std::collections::HashMap;
use std::io::{BufRead, BufReader, Seek, SeekFrom};
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

/// Parse un fichier rollout JSONL en streaming ligne a ligne, a partir
/// d'un offset (import incremental). Retourne aussi le checkpoint :
/// l'offset apres la derniere ligne COMPLETE traitee, et le nombre de
/// lignes traitees (cumul de lines_before). Une derniere ligne sans \n
/// et JSON invalide (ecriture partielle) est ignoree et le checkpoint
/// n'avance pas : elle sera relue a la prochaine passe.
pub fn parse_file_from(path: &Path, start_offset: u64, lines_before: u64) -> ParsedFile {
    let mut out = ParsedFile {
        result: FileParseResult::default(),
        end_offset: start_offset,
        lines_processed: 0,
    };
    let mut result = FileParseResult::default();
    let mut meta_seen = false;

    let mut file = match std::fs::File::open(path) {
        Ok(f) => f,
        Err(e) => {
            result.open_failed = true;
            result
                .errors
                .push(format!("open failed: {} ({e})", path.display()));
            out.result = result;
            return out;
        }
    };
    if start_offset > 0 {
        if let Err(e) = file.seek(SeekFrom::Start(start_offset)) {
            result.open_failed = true;
            result.errors.push(format!("seek failed: {e}"));
            out.result = result;
            return out;
        }
    }
    let mut reader = BufReader::with_capacity(256 * 1024, file);

    let mut index: u64 = 0;
    let mut offset = start_offset;
    loop {
        let mut buf = Vec::with_capacity(4096);
        let n = match reader.read_until(b'\n', &mut buf) {
            Ok(n) => n,
            Err(e) => {
                result.errors.push(format!("read failed: {e}"));
                break;
            }
        };
        if n == 0 {
            break;
        }
        let complete = buf.last() == Some(&b'\n');
        let line_str = String::from_utf8_lossy(&buf);
        let line = line_str.trim_end_matches(['\n', '\r']);

        // Ligne finale incomplete (JSON non termine) : on ne la traite pas
        // et on n'avance pas le checkpoint.
        if !complete {
            let is_valid_json = serde_json::from_str::<serde_json::Value>(line).is_ok();
            if !is_valid_json {
                break;
            }
        }

        result.counters.lines_total += 1;
        let ordinal = lines_before + index;
        index += 1;
        offset += n as u64;

        // Fast path : ignorer sans parser JSON les lignes qui ne contiennent
        // aucun des marqueurs qui nous interessent (messages, tool outputs...).
        if !(line.contains("\"token_usage_record\"")
            || line.contains("\"turn_context\"")
            || line.contains("\"token_count\"")
            || line.contains("\"session_meta\""))
        {
            out.end_offset = offset;
            out.lines_processed += 1;
            continue;
        }

        let parsed: RolloutLine = match serde_json::from_str(line) {
            Ok(p) => p,
            Err(e) => {
                result.counters.lines_json_errors += 1;
                result
                    .errors
                    .push(format!("line {ordinal}: JSON error: {e}"));
                out.end_offset = offset;
                out.lines_processed += 1;
                continue;
            }
        };

        match parsed.kind.as_str() {
            "session_meta" => {
                if !meta_seen {
                    meta_seen = true;
                    if let Ok(m) = serde_json::from_value::<SessionMetaPayload>(parsed.payload) {
                        result.meta = SessionMeta {
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
                        result.turn_models.insert(t.turn_id.clone(), model.clone());
                        if let Some(root) = &t.root_turn_id {
                            // un seul modele par root pour attribution "inferred"
                            match result.root_models.get(root) {
                                Some(prev) if prev != model => {
                                    result.root_models.remove(root);
                                }
                                _ => {
                                    result.root_models.insert(root.clone(), model.clone());
                                }
                            }
                        }
                    }
                    if let Some(cwd) = t.cwd {
                        result.turn_cwd.insert(t.turn_id, cwd);
                    }
                }
            }
            "token_usage_record" => {
                match serde_json::from_value::<TokenUsageRecordPayload>(parsed.payload) {
                    Ok(p) => {
                        if p.usage.subset_violation() {
                            result.counters.subset_violations += 1;
                        }
                        if p.response_id.is_none() {
                            result.counters.records_without_response_id += 1;
                        }
                        result.counters.usage_records += 1;
                        result.primary_events += 1;
                        result.raw_calls.push(RawUsageEvent {
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
                        result.counters.lines_json_errors += 1;
                        result
                            .errors
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
                    out.end_offset = offset;
                    out.lines_processed += 1;
                    continue;
                }
                result.counters.legacy_token_count_events += 1;
                let info = parsed.payload.get("info");
                if info.is_none() || info == Some(&Value::Null) {
                    result.counters.legacy_null_info += 1;
                    out.end_offset = offset;
                    out.lines_processed += 1;
                    continue;
                }
                match serde_json::from_value::<LegacyInfo>(info.unwrap().clone()) {
                    Ok(info) => {
                        if info.total_token_usage.is_some() {
                            result.counters.legacy_total_usage_present += 1;
                        }
                        if let Some(last) = info.last_token_usage {
                            if last.subset_violation() {
                                result.counters.subset_violations += 1;
                            }
                            result.legacy_events += 1;
                            result.legacy_totals.add(&last);
                            result.raw_calls.push(RawUsageEvent {
                                timestamp_utc: parsed.timestamp,
                                session_id: result.meta.session_id.clone(),
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
                        result.counters.lines_json_errors += 1;
                        result
                            .errors
                            .push(format!("line {ordinal}: malformed token_count info: {e}"));
                    }
                }
            }
            _ => {
                // Type inconnu : ignore silencieusement (resilience), visible
                // via les compteurs de lignes du diagnostics.
            }
        }
        out.end_offset = offset;
        out.lines_processed += 1;
    }
    out.result = result;
    out
}

/// Sortie d'un parsing de fichier : resultats + checkpoint incremental.
#[derive(Debug)]
pub struct ParsedFile {
    pub result: FileParseResult,
    /// Offset apres la derniere ligne complete traitee.
    pub end_offset: u64,
    pub lines_processed: u64,
}

/// Parse complet depuis le debut (scan en memoire).
pub fn parse_file(path: &Path) -> FileParseResult {
    parse_file_from(path, 0, 0).result
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
/// input|cached|output|ordinal), spec cahier des charges §5. Sans le chemin :
/// un fichier deplace vers archived_sessions garde la meme identite d'event.
pub fn event_uid(event: &RawUsageEvent) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
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

/// Construit un appel finalise : modele resolu (exact/inferred/unknown),
/// projet, uid. Partage entre le scan en memoire et l'import SQLite.
pub fn finalize_call(
    event: &RawUsageEvent,
    meta: &SessionMeta,
    turn_models: &HashMap<String, String>,
    turn_cwd: &HashMap<String, String>,
    root_models: &HashMap<String, String>,
    source_file: &str,
    archived: bool,
) -> crate::model::InferenceCall {
    let (model, confidence) = resolve_model(event, turn_models, root_models);
    let project_path = meta
        .cwd
        .clone()
        .or_else(|| event.turn_id.as_ref().and_then(|t| turn_cwd.get(t).cloned()));
    crate::model::InferenceCall {
        event_uid: event_uid(event),
        response_id: event.response_id.clone(),
        timestamp_utc: event.timestamp_utc.clone(),
        session_id: event.session_id.clone().or_else(|| meta.session_id.clone()),
        thread_id: event.thread_id.clone(),
        turn_id: event.turn_id.clone(),
        root_turn_id: event.root_turn_id.clone(),
        model_slug: model,
        model_confidence: confidence,
        project_path,
        activity: None,
        parent_thread_id: None,
        thread_title: None,
        usage: event.usage,
        source_format: event.source_format,
        source_file: source_file.to_string(),
        source_ordinal: event.source_ordinal,
        archived,
    }
}
