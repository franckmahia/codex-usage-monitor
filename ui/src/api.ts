import { invoke } from "@tauri-apps/api/core";
import type {
  DiagnosticsDto,
  ImportReportDto,
  PricingInfoDto,
  SummaryDto,
  ThreadRow,
} from "./types";

export function getSummary(from: string | null, to: string | null): Promise<SummaryDto> {
  return invoke("get_summary", { from, to });
}

export function getThreads(
  from: string | null,
  to: string | null,
  limit: number,
): Promise<ThreadRow[]> {
  return invoke("get_threads", { from, to, limit });
}

export function getDiagnostics(): Promise<DiagnosticsDto> {
  return invoke("get_diagnostics");
}

export function refreshImport(): Promise<ImportReportDto> {
  return invoke("refresh_import");
}

export function exportCsvFiles(
  dir: string,
  from: string | null,
  to: string | null,
): Promise<string[]> {
  return invoke("export_csv_files", { dir, from, to });
}

export function getPricing(): Promise<PricingInfoDto[]> {
  return invoke("get_pricing");
}
