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

/// Sessions couvertes par le format moderne (token_usage_record), collectees
/// sur TOUS les fichiers avant de compter le moindre event legacy : la regle
/// « jamais melanger les deux methodes pour une meme session » est globale,
/// pas seulement par fichier.
fn primary_sessions(parsed: &[(usize, FileParseResult)]) -> HashSet<String> {
    let mut sessions = HashSet::new();
    for (_, res) in parsed {
        if res.primary_events == 0 {
            continue;
        }
        if let Some(s) = &res.meta.session_id {
            sessions.insert(s.clone());
        }
        for ev in &res.raw_calls {
            if ev.source_format == SourceFormat::TokenUsageRecord {
                if let Some(s) = &ev.session_id {
                    sessions.insert(s.clone());
                }
            }
        }
    }
    sessions
}

/// Scan complet d'un codex home : discovery -> parse streaming (2 passes :
/// parse global puis resolution/dedup) -> resolution modele -> deduplication
/// (response_id prioritaire, sinon event_uid SHA256).
/// Le scan est deterministe : rescanner ne change aucun total.
pub fn scan(codex_home: &std::path::Path) -> ScanOutcome {
    let files = discovery::discover(codex_home);
    let mut diagnostics = Diagnostics {
        files_found: files.len(),
        ..Default::default()
    };
    diagnostics.archived_files = files.iter().filter(|f| f.archived).count();

    // Passe 1 : parse de tous les fichiers + compteurs/diagnostics.
    let mut parsed: Vec<(usize, FileParseResult)> = Vec::with_capacity(files.len());
    for (idx, file) in files.iter().enumerate() {
        let res = parser::parse_file(&file.path);

        if res.open_failed {
            diagnostics.files_failed += 1;
        } else {
            diagnostics.files_parsed += 1;
        }

        diagnostics.lines_total += res.counters.lines_total;
        diagnostics.usage_records += res.counters.usage_records;
        diagnostics.subset_violations += res.counters.subset_violations;
        diagnostics.calls_without_response_id += res.counters.records_without_response_id;
        diagnostics.legacy_null_info += res.counters.legacy_null_info;
        diagnostics.legacy_total_usage_present += res.counters.legacy_total_usage_present;
        diagnostics.add_errors(&res.errors);

        parsed.push((idx, res));
    }
    let sessions_with_primary = primary_sessions(&parsed);

    // Priorite par session (invariant : ne jamais melanger les deux
    // methodes) : 1. token_usage_record.usage -> 2. legacy last_token_usage
    // -> 3. diagnostic. Comptes ici, apres la vue globale des sessions
    // primaires : un fichier legacy dont la session est deja couverte en
    // moderne n'est plus une source de comptabilite.
    for (_, res) in &parsed {
        if res.primary_events == 0 && res.legacy_events > 0 {
            let covered = res
                .meta
                .session_id
                .as_deref()
                .is_some_and(|s| sessions_with_primary.contains(s));
            if !covered {
                diagnostics.legacy_records += res.legacy_events;
                diagnostics.legacy_file_totals.add(&res.legacy_totals);
            }
        }
    }

    // Passe 2 : dedup + finalisation des appels.
    let mut seen_keys: HashSet<String> = HashSet::new();
    let mut calls: Vec<InferenceCall> = Vec::new();
    let mut totals = TokenUsage::default();

    for (file_idx, res) in &parsed {
        let file = &files[*file_idx];
        let path_str = file.path.display().to_string();

        for event in &res.raw_calls {
            let is_legacy = event.source_format == SourceFormat::LegacyTokenCount;
            if is_legacy {
                // Fichier primaire OU session deja couverte par le format
                // moderne : l'event legacy est une vue d'ancienne methode,
                // jamais une source d'agregation supplementaire.
                if res.primary_events > 0 {
                    continue;
                }
                let covered = event
                    .session_id
                    .as_deref()
                    .is_some_and(|s| sessions_with_primary.contains(s));
                if covered {
                    continue;
                }
            }

            let key = match &event.response_id {
                Some(rid) => format!("rid:{rid}"),
                None => format!("uid:{}", parser::event_uid(event)),
            };
            if !seen_keys.insert(key) {
                diagnostics.duplicates_ignored += 1;
                continue;
            }
            if event.response_id.is_some() {
                diagnostics.unique_response_ids += 1;
            }

            let call = parser::finalize_call(
                event,
                &res.meta,
                &res.turn_models,
                &res.turn_cwd,
                &res.root_models,
                &res.thread_settings,
                &path_str,
                file.archived,
            );
            if call.model_slug.is_none() {
                diagnostics.calls_unknown_model += 1;
                diagnostics.unknown_model_tokens += event.usage.total_tokens;
            } else if call.model_confidence == ModelConfidence::Inferred {
                diagnostics.calls_inferred_model += 1;
            }

            totals.add(&event.usage);
            calls.push(call);
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
