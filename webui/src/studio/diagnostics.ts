import type { Diagnostic, HealthRow, RunRecord } from "./types";

/** Stable across refetches; changed diagnostics can be shown again after dismissal. */
export function diagnosticKey(diagnostic: Diagnostic): string {
  return JSON.stringify([diagnostic.code, diagnostic.message, diagnostic.path, diagnostic.node_id, diagnostic.line, diagnostic.column]);
}

export function healthWarnings(row: HealthRow): string[] {
  const messages = [...new Set((row.errors ?? []).map((error) => error.message))];
  return row.unavailable ? ["Flow unavailable", ...messages] : messages;
}

export function applicationLabel(status: RunRecord["application"]["status"]): string {
  return status === "suppressed" ? "suppressed · wake cooldown" : status.replaceAll("_", " ");
}
