use std::collections::HashSet;

use crate::discovery::{self, SourceFile};
use crate::model::{
    FileParseResult, InferenceCall, ModelConfidence, SourceFormat, TokenUsage,
};
use crate::parser;

#[derive(Debug, Default)]
pub struct Diagnostics {
    pub files_found: usize,
    pub files_parsed: usize,
    pub files_failed: usize,
    pub lines_total: u64,
    pub usage_records: u64,
    pub unique_response_ids: u64,
    pub calls_without_response_id: u64,
    pub duplicates_ignored: u64,
    pub legacy_records: u64,
    pub legacy_null_info: u64,
    pub legacy_total_usage_present: u64,
    pub subset_violations: u64,
    pub unknown_model_tokens: u64,
    pub calls_unknown_model: u64,
    pub calls_inferred_model: u64,
    pub archived_files: usize,
    pub parse_errors: Vec<String>,
    pub legacy_file_totals: TokenUsage,
}

impl Diagnostics {
    pub fn add_errors(&mut self, errs: &[String]) {
        for e in errs {
            self.parse_errors.push(e.clone());
        }
    }
}

/// Resultat d'un scan complet : appels dedupliques + diagnostics + sommes
/// de secours legacy pour le consistency check du doctor.
pub struct ScanOutcome {
    pub calls: Vec<InferenceCall>,
    pub diagnostics: Diagnostics,
    pub totals: TokenUsage,
    pub files: Vec<SourceFile>,
    pub codex_home: std::path::PathBuf,
}

impl ScanOutcome {
    pub fn legacy_totals(&self) -> TokenUsage {
        self.diagnostics.legacy_file_totals
    }
}

/// Scan complet d'un codex home : discovery -> parse streaming -> resolution
/// modele -> deduplication (response_id prioritaire, sinon event_uid SHA256).
/// Le scan est deterministe : rescanner ne change aucun total.
pub fn scan(codex_home: &std::path::Path) -> ScanOutcome {
    let files = discovery::discover(codex_home);
    let mut diagnostics = Diagnostics {
        files_found: files.len(),
        ..Default::default()
    };
    diagnostics.archived_files = files.iter().filter(|f| f.archived).count();

    let mut seen_keys: HashSet<String> = HashSet::new();
    let mut calls: Vec<InferenceCall> = Vec::new();
    let mut totals = TokenUsage::default();
    let mut legacy_totals = TokenUsage::default();

    for file in &files {
        let path_str = file.path.display().to_string();
        let FileParseResult {
            meta,
            open_failed,
            raw_calls,
            turn_models,
            turn_cwd,
            root_models,
            counters,
            errors,
            legacy_totals: file_legacy,
            legacy_events,
            primary_events,
        } = parser::parse_file(&file.path);

        if open_failed {
            diagnostics.files_failed += 1;
        } else {
            diagnostics.files_parsed += 1;
        }

        diagnostics.lines_total += counters.lines_total;
        diagnostics.usage_records += counters.usage_records;
        diagnostics.duplicates_ignored += counters.duplicates_ignored;
        diagnostics.subset_violations += counters.subset_violations;
        diagnostics.calls_without_response_id += counters.records_without_response_id;
        diagnostics.legacy_null_info += counters.legacy_null_info;
        diagnostics.legacy_total_usage_present += counters.legacy_total_usage_present;
        diagnostics.add_errors(&errors);

        // Priorite par session/fichier (invariant : ne jamais melanger les
        // deux methodes) : 1. token_usage_record.usage -> 2. legacy
        // last_token_usage -> 3. diagnostic.
        if primary_events == 0 && legacy_events > 0 {
            diagnostics.legacy_records += legacy_events;
            diagnostics.legacy_file_totals.add(&file_legacy);
        }

        for event in &raw_calls {
            let is_legacy = event.source_format == SourceFormat::LegacyTokenCount;
            if primary_events > 0 && is_legacy {
                // Fichier primaire : les events legacy sont des doublons de
                // vue, jamais une source d'agregation supplementaire.
                continue;
            }

            let key = match &event.response_id {
                Some(rid) => format!("rid:{rid}"),
                None => format!("uid:{}", parser::event_uid(event, &path_str)),
            };
            if !seen_keys.insert(key) {
                diagnostics.duplicates_ignored += 1;
                continue;
            }
            if event.response_id.is_some() {
                diagnostics.unique_response_ids += 1;
            }

            let (model, confidence) = parser::resolve_model(event, &turn_models, &root_models);
            if model.is_none() {
                diagnostics.calls_unknown_model += 1;
                diagnostics.unknown_model_tokens += event.usage.total_tokens;
            } else if confidence == ModelConfidence::Inferred {
                diagnostics.calls_inferred_model += 1;
            }

            let project_path = meta
                .cwd
                .clone()
                .or_else(|| event.turn_id.as_ref().and_then(|t| turn_cwd.get(t).cloned()));

            totals.add(&event.usage);
            legacy_totals.add(&event.usage);

            calls.push(InferenceCall {
                event_uid: parser::event_uid(event, &path_str),
                response_id: event.response_id.clone(),
                timestamp_utc: event.timestamp_utc.clone(),
                session_id: event.session_id.clone().or_else(|| meta.session_id.clone()),
                thread_id: event.thread_id.clone(),
                turn_id: event.turn_id.clone(),
                root_turn_id: event.root_turn_id.clone(),
                model_slug: model,
                model_confidence: confidence,
                project_path,
                usage: event.usage,
                source_format: event.source_format,
                source_file: path_str.clone(),
                source_ordinal: event.source_ordinal,
                archived: file.archived,
            });
        }
    }

    ScanOutcome {
        calls,
        diagnostics,
        totals,
        files,
        codex_home: codex_home.to_path_buf(),
    }
}

/// Filtre les appels sur la periode [from, to] incluse (dates UTC YYYY-MM-DD).
pub fn filter_period(
    calls: Vec<InferenceCall>,
    from: Option<&str>,
    to: Option<&str>,
) -> Vec<InferenceCall> {
    calls.into_iter().filter(|c| {
        let day = c.day();
        from.map_or(true, |f| day >= f) && to.map_or(true, |t| day <= t)
    }).collect()
}
