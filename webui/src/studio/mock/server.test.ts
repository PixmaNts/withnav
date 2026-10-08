import { describe, expect, it } from "vitest";
import { HttpError } from "../../api/http";
import { createMockApi } from "./server";

const WS = "/repo";

describe("mock flows API", () => {
  it("exposes host diagnostics, health warnings, reload propagation and a suppressed live wake", async () => {
    const api = createMockApi(WS);
    expect((await api.list()).errors?.[0]?.message).toContain("Instinct unavailable");
    const health = await api.health();
    expect(health.flows.find((row) => row.name === "guard-mutations")).toMatchObject({ unavailable: true, errors: [{ code: "unavailable" }] });
    expect((await api.reload()).propagation).toContain("next event");
    const { runs } = await api.runs("turn-digest");
    expect((await api.run("turn-digest", runs[0]!.run_id)).application).toEqual({
      status: "suppressed",
      actions: [{ node_id: "wake_cognition", action: "wake", status: "suppressed", message: "wake cooldown" }],
    });
    expect((await api.run("turn-digest", runs[1]!.run_id)).application).toEqual({
      status: "applied",
      actions: [
        { node_id: "wake_cognition", action: "wake", status: "suppressed", message: "wake cooldown" },
        { node_id: "audit", action: "log", status: "applied", message: null },
      ],
    });
  });
  it("lists the seed flows sorted by trigger, and opens guard-mutations", async () => {
    const api = createMockApi(WS);
    const { flows } = await api.list();
    expect(flows.map((f) => [f.trigger, f.name, f.location])).toEqual([
      ["save", "public-api-drift", "global"],
      ["tool.pre_call", "guard-mutations", "workspace"],
      ["turn.end", "turn-digest", "builtin"],
    ]);
    const doc = await api.get("guard-mutations", "workspace");
    expect(doc.errors).toEqual([]);
    expect(doc.definition?.node).toHaveLength(9);
    expect(doc.path).toBe("/repo/.withnav/flows/guard-mutations.toml");
    expect(doc.saved_since_reload).toBe(false);
  });

  it("saves with the revision, marks the flow saved since reload, refuses a stale save", async () => {
    const api = createMockApi(WS);
    const doc = await api.get("guard-mutations", "workspace");
    const def = { ...doc.definition!, flow: { ...doc.definition!.flow, budget_ms: 900 } };
    const saved = await api.save("guard-mutations", { location: "workspace", revision: doc.revision, content: { format: "graph", definition: def, templates: {} } });
    expect(saved.saved_since_reload).toBe(true);
    expect(saved.toml).toContain("budget_ms = 900");
    await expect(api.save("guard-mutations", { location: "workspace", revision: doc.revision, content: { format: "graph", definition: def, templates: {} } })).rejects.toMatchObject({ status: 409, reason: "stale" });
  });

  it("keeps an invalid TOML save, then refuses the reload with its errors", async () => {
    const api = createMockApi(WS);
    await api.setTrust(true);
    const doc = await api.get("guard-mutations", "workspace");
    const bad = await api.save("guard-mutations", { location: "workspace", revision: doc.revision, content: { format: "toml", toml: "[flow\n", templates: {} } });
    expect(bad.definition).toBeNull();
    expect(bad.errors[0]?.code).toBe("parse");
    const error = await api.reload().catch((e: unknown) => e);
    expect(error).toBeInstanceOf(HttpError);
    expect((error as HttpError).status).toBe(422);
    expect((error as HttpError).body.errors.length).toBeGreaterThan(0);
    expect((await api.list()).errors?.some((diagnostic) => diagnostic.code === "parse")).toBe(true);
  });

  it("refuses sample and replay of workspace flows until trusted", async () => {
    const api = createMockApi(WS);
    const doc = await api.get("guard-mutations", "workspace");
    const source = { saved: { name: doc.name, location: doc.location, revision: doc.revision } };
    await expect(api.sample({ source, payload: {}, session_id: null })).rejects.toMatchObject({ status: 403, reason: "untrusted" });
    await api.setTrust(true);
    const run = await api.sample({ source, payload: { tool: "write", args: { path: "x" }, call_id: "s" }, session_id: null });
    expect(run.mode).toBe("sample");
    expect(run.nodes.find((n) => n.id === "effect")?.status).toBe("completed");
    const skipped = await api.sample({ source, payload: { tool: "read", args: {}, call_id: "s" }, session_id: null });
    expect(skipped.status).toBe("skipped");
  });

  it("replays the recorded run without new answers", async () => {
    const api = createMockApi(WS);
    await api.setTrust(true);
    const doc = await api.get("guard-mutations", "workspace");
    const { runs } = await api.runs("guard-mutations");
    const live = runs.find((r) => r.mode === "live")!;
    const result = await api.replay({ source: { saved: { name: doc.name, location: doc.location, revision: doc.revision } }, run_id: live.run_id });
    expect(result.comparable).toBe(true);
    expect(result.outcome?.actions.map((a) => a.action)).toEqual(["deny", "log"]);
    expect(result.nodes.find((n) => n.id === "trusted_host")?.status).toBe("not_reached");

    // Changing a question makes it non-comparable.
    const def = doc.definition!;
    const changed = { ...def, node: def.node.map((n) => (n.id === "asked_for" && n.type === "noul" ? { ...n, instructions: "Was this asked for?" } : n)) };
    const again = await api.replay({ source: { content: { format: "graph", definition: changed, templates: {} } }, run_id: live.run_id });
    expect(again.comparable).toBe(false);
    expect(again.outcome).toBeNull();
    expect(again.nodes.find((n) => n.id === "asked_for")?.status).toBe("question_changed");
  });
});
