import { describe, expect, it } from "vitest";
import { applicationLabel, diagnosticKey, healthWarnings } from "./diagnostics";
import type { Diagnostic, HealthRow } from "./types";

const diagnostic: Diagnostic = { code: "io", message: "Instinct unavailable: no model configured", path: "", node_id: null, line: null, column: null };
const row: HealthRow = { name: "guard", runs: 0, failures: 0, fallbacks_applied: 0, cost_usd: null, cost_unknown_runs: 0 };

describe("host diagnostic presentation", () => {
  it("keeps dismissal keys stable across copies but distinguishes changed messages and paths", () => {
    expect(diagnosticKey({ ...diagnostic })).toBe(diagnosticKey(diagnostic));
    expect(diagnosticKey({ ...diagnostic, message: "Reload failed" })).not.toBe(diagnosticKey(diagnostic));
    expect(diagnosticKey({ ...diagnostic, path: "guard.toml" })).not.toBe(diagnosticKey(diagnostic));
  });
  it("handles older health rows, unavailable rows and distinct diagnostic messages", () => {
    expect(healthWarnings(row)).toEqual([]);
    expect(healthWarnings({ ...row, unavailable: true })).toEqual(["Flow unavailable"]);
    expect(healthWarnings({ ...row, errors: [diagnostic, { ...diagnostic }] })).toEqual([diagnostic.message]);
    expect(healthWarnings({ ...row, unavailable: true, errors: [diagnostic] })).toEqual(["Flow unavailable", diagnostic.message]);
  });
  it("labels every application status with neutral wording", () => {
    expect(applicationLabel("suppressed")).toBe("suppressed · wake cooldown");
    expect(applicationLabel("not_applied")).toBe("not applied");
    for (const status of ["applied", "failed", "uncertain"] as const) expect(applicationLabel(status)).toBe(status);
  });
});
