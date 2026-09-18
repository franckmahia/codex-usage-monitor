use serde::{Deserialize, Serialize};

/// Usage d'un appel modele. `token_usage_record.payload.usage` est l'unite primaire
/// de comptabilite (invariant 1). Les sous-ensembles sont documentes :
/// cached <= input, cache_write <= input, reasoning <= output.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUsage {
    #[serde(default)]
    pub input_tokens: u64,
    #[serde(default)]
    pub cached_input_tokens: u64,
    #[serde(default)]
    pub cache_write_input_tokens: u64,
    #[serde(default)]
    pub output_tokens: u64,
    #[serde(default)]
    pub reasoning_output_tokens: u64,
    #[serde(default)]
    pub total_tokens: u64,
}

impl TokenUsage {
    /// Entree ordinaire = input - cached - cache_write (invariants 5 et 6).
    pub fn ordinary_input(&self) -> u64 {
        self.input_tokens
            .saturating_sub(self.cached_input_tokens)
            .saturating_sub(self.cache_write_input_tokens)
    }

    /// Verifie cached <= input et cache_write <= input (diagnostic, jamais un panic).
    pub fn subset_violation(&self) -> bool {
        self.cached_input_tokens > self.input_tokens
            || self.cache_write_input_tokens > self.input_tokens
            || self.reasoning_output_tokens > self.output_tokens
    }

    pub fn add(&mut self, other: &TokenUsage) {
        self.input_tokens += other.input_tokens;
        self.cached_input_tokens += other.cached_input_tokens;
        self.cache_write_input_tokens += other.cache_write_input_tokens;
        self.output_tokens += other.output_tokens;
        self.reasoning_output_tokens += other.reasoning_output_tokens;
        self.total_tokens += other.total_tokens;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ServiceTier {
    Unknown,
    Standard,
    Fast,
}

impl ServiceTier {
    pub fn as_str(&self) -> &'static str {
        match self {
            ServiceTier::Unknown => "unknown",
            ServiceTier::Standard => "standard",
            ServiceTier::Fast => "fast",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "fast" => ServiceTier::Fast,
            "standard" | "default" | "priority" | "flex" => ServiceTier::Standard,
            _ => ServiceTier::Unknown,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelConfidence {
    Unknown,
    Inferred,
    Exact,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceFormat {
    TokenUsageRecord,
    LegacyTokenCount,
}

impl SourceFormat {
    pub fn as_str(&self) -> &'static str {
        match self {
            SourceFormat::TokenUsageRecord => "token_usage_record",
            SourceFormat::LegacyTokenCount => "legacy_token_count",
        }
    }

    pub fn parse(s: &str) -> Self {
        match s {
            "legacy_token_count" => SourceFormat::LegacyTokenCount,
            _ => SourceFormat::TokenUsageRecord,
        }
    }
}

impl ModelConfidence {
    pub fn as_str(&self) -> &'static str {
        match self {
            ModelConfidence::Exact => "exact",
            ModelConfidence::Inferred => "inferred",
            ModelConfidence::Unknown => "unknown",
        }
    }
}

/// Un appel modele deduplique et normalise.
#[derive(Debug, Clone)]
pub struct InferenceCall {
    pub event_uid: String,
    pub response_id: Option<String>,
    pub timestamp_utc: Option<String>,
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub root_turn_id: Option<String>,
    pub model_slug: Option<String>,
    pub model_confidence: ModelConfidence,
    pub service_tier: ServiceTier,
    pub project_path: Option<String>,
    pub activity: Option<String>,
    pub parent_thread_id: Option<String>,
    pub thread_title: Option<String>,
    pub usage: TokenUsage,
    pub source_format: SourceFormat,
    pub source_file: String,
    pub source_ordinal: u64,
    pub archived: bool,
}

impl InferenceCall {
    /// Date UTC (YYYY-MM-DD) du record, ou "unknown".
    pub fn day(&self) -> &str {
        match &self.timestamp_utc {
            Some(ts) if ts.len() >= 10 && ts.as_bytes()[4] == b'-' && ts.as_bytes()[7] == b'-' => {
                &ts[..10]
            }
            _ => "unknown",
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct SessionMeta {
    pub session_id: Option<String>,
    pub cwd: Option<String>,
    pub originator: Option<String>,
    pub cli_version: Option<String>,
}

/// Evenement d'usage brut, avant resolution du modele.
#[derive(Debug, Clone)]
pub struct RawUsageEvent {
    pub timestamp_utc: Option<String>,
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub root_turn_id: Option<String>,
    pub response_id: Option<String>,
    pub usage: TokenUsage,
    pub source_format: SourceFormat,
    pub source_ordinal: u64,
}

#[derive(Debug, Clone, Default)]
pub struct FileCounters {
    pub lines_total: u64,
    pub lines_json_errors: u64,
    pub usage_records: u64,
    pub legacy_token_count_events: u64,
    pub legacy_null_info: u64,
    pub legacy_total_usage_present: u64,
    pub duplicates_ignored: u64,
    pub subset_violations: u64,
    pub records_without_response_id: u64,
}

#[derive(Debug, Default)]
pub struct FileParseResult {
    pub meta: SessionMeta,
    pub open_failed: bool,
    pub raw_calls: Vec<RawUsageEvent>,
    pub turn_models: std::collections::HashMap<String, String>,
    pub turn_cwd: std::collections::HashMap<String, String>,
    /// root_turn_id -> modele si unique (pour attribution "inferred")
    pub root_models: std::collections::HashMap<String, String>,
    /// thread_id -> [(ts_settings, service_tier)] dans l'ordre du fichier
    pub thread_settings: Vec<(String, String, String)>,
    pub counters: FileCounters,
    pub errors: Vec<String>,
    /// Somme de secours legacy (last_token_usage), uniquement pour le consistency check.
    pub legacy_totals: TokenUsage,
    pub legacy_events: u64,
    pub primary_events: u64,
}
