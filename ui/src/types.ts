export interface BucketDto {
  name: string;
  calls: number;
  input: number;
  cached: number;
  ordinary: number;
  cache_write: number;
  output: number;
  reasoning: number;
  cache_hit_percent: number | null;
  equivalent_cost: number;
  cost_without_cache: number;
  cache_savings: number;
  unknown_model_tokens: number;
  long_context_calls: number;
}

export interface SummaryDto {
  from: string | null;
  to: string | null;
  calls: number;
  input: number;
  cached: number;
  ordinary: number;
  cache_write: number;
  output: number;
  reasoning: number;
  cache_hit_percent: number | null;
  equivalent_cost: number;
  cost_without_cache: number;
  cache_savings: number;
  cache_savings_percent: number | null;
  pricing_confidence_percent: number;
  tiers: [number, number, number];
  by_day: BucketDto[];
  by_model: BucketDto[];
  by_project: BucketDto[];
  by_activity: BucketDto[];
  anomalies: string[];
}

export interface ThreadRow {
  thread_id: string;
  title: string;
  project: string;
  calls: number;
  input: number;
  output: number;
  cache_hit_percent: number | null;
  equivalent_cost: number;
  activity: string;
}

export interface DiagnosticsDto {
  codex_home: string;
  db_path: string;
  files_found: number;
  files_parsed: number;
  lines_total: number;
  usage_records: number;
  duplicates_ignored: number;
  legacy_records: number;
  parse_errors: string[];
  subset_violations: number;
  db_calls: number;
  db_input: number;
  db_output: number;
  consistent: boolean;
  state_lifetime_tokens_used: number;
  state_threads_by_activity: Record<string, number>;
  pricing_codex_version: string;
  pricing_api_version: string;
  pricing_last_verified: string;
}

export interface ImportReportDto {
  files_seen: number;
  files_skipped: number;
  files_changed: number;
  calls_inserted: number;
  duplicates_ignored: number;
  parse_errors: number;
  duration_ms: number;
}

export interface PricingInfoDto {
  catalog: string;
  profile: string;
  version: string;
  last_verified: string;
  source_url: string;
  models: string[];
}

export function fmtInt(n: number): string {
  return new Intl.NumberFormat("fr-FR").format(n);
}

export function fmtTokens(n: number): string {
  if (n >= 1e9) return `${(n / 1e9).toFixed(1)} B`;
  if (n >= 1e6) return `${(n / 1e6).toFixed(1)} M`;
  if (n >= 1e3) return `${(n / 1e3).toFixed(1)} K`;
  return fmtInt(n);
}

export function fmtMoney(v: number): string {
  return new Intl.NumberFormat("fr-FR", {
    style: "currency",
    currency: "USD",
    maximumFractionDigits: 2,
  }).format(v);
}

export function fmtPct(v: number | null, digits = 1): string {
  return v === null ? "n/a" : `${v.toFixed(digits)} %`;
}
