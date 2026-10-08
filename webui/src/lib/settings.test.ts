import { describe, expect, it } from "vitest";
import type { ModelEntry, SettingField, SettingsBody } from "../api/types";
import { groupCatalog } from "./catalog";
import {
  agentKeys,
  appliesText,
  asChoice,
  carryVariant,
  choiceOf,
  fileLines,
  initialSave,
  saveReducer,
  searchIndex,
  searchMatches,
  serverTheme,
  statusLine,
  summaries,
  usedByLabel,
  type SaveEvent,
} from "./settings";

const field = (key: string, value: unknown, extra: Partial<SettingField> = {}): SettingField => ({ key, value, default: null, set: value !== null, error: null, applies: "new_sessions", ...extra });

const body: SettingsBody = {
  revision: "sha256:1",
  paths: { config: "/home/u/.config/navai/config.json", models: "/home/u/.config/navai/models.json", agents: [] },
  fields: [
    field("layers.reflex", { model: "inception/mercury-edit-2", variant: null }),
    field("layers.instinct", null),
    field("layers.metacognition", { model: "omniroute/combo/deepseek-v4-flash", variant: "low" }),
    field("default_agent", "build", { default: "build" }),
    field("agents.build", { model: "omniroute/cc/claude-opus-5-5", variant: "high" }),
    field("agents.explore", null),
    field("permissions.automatic_review", null, { default: true }),
    field("subagents.max_concurrent", 6, { default: 4 }),
    field("tui.theme", "nord-light", { applies: "live" }),
  ],
  gateways: [
    { name: "omniroute", base_url: "http://x/v1", api_key_env: "OMNIROUTE_API_KEY", key_source: "environment", catalog_format: "omniroute", connect_timeout_ms: 5000, request_timeout_ms: 600000, headers: [], model_count: 3, used_by: ["layers.metacognition", "agents.build"] },
    { name: "inception", base_url: "https://i/v1", api_key_env: null, key_source: "auth_store", catalog_format: null, connect_timeout_ms: 5000, request_timeout_ms: 600000, headers: [], model_count: 1, used_by: ["layers.reflex"] },
  ],
};

const model = (name: string, extra: Partial<ModelEntry> = {}): ModelEntry => {
  const [gateway, ...rest] = name.split("/");
  return { name, gateway: gateway!, provider_group: rest.length > 1 ? rest[0]! : gateway!, upstream_id: rest.join("/"), api: "chat", supports_tools: true, supports_parallel_tool_calls: true, context_window: 128000, ...extra };
};

describe("settings field mapping", () => {
  it("reads model choices from the object or the bare string config.json may hold", () => {
    expect(asChoice("a/b")).toEqual({ model: "a/b", variant: null });
    expect(asChoice({ model: "a/b", variant: "low" })).toEqual({ model: "a/b", variant: "low" });
    expect(asChoice({ model: "a/b" })).toEqual({ model: "a/b", variant: null });
    // A stored "off" is a choice of its own, not the model's default.
    expect(asChoice({ model: "a/b", variant: "off" })).toEqual({ model: "a/b", variant: "off" });
    expect(asChoice(null)).toBeNull();
    expect(asChoice(42)).toBeNull();
  });
  it("uses the default for a key config.json does not hold", () => {
    expect(choiceOf(body, "layers.instinct")).toBeNull();
    expect(choiceOf(body, "layers.metacognition")?.variant).toBe("low");
  });
  it("lists the agents the server reports a field for", () => {
    expect(agentKeys(body)).toEqual(["build", "explore"]);
  });
  it("adopts the server theme only when config.json sets a known one", () => {
    expect(serverTheme(body)).toBe("nord-light");
    expect(serverTheme({ ...body, fields: [field("tui.theme", null)] })).toBeNull();
    expect(serverTheme({ ...body, fields: [field("tui.theme", "solarized")] })).toBeNull();
  });
  it("keeps a variant only when the new model offers it", () => {
    const withLow = model("g/p/a", { variants: ["low", "high"] });
    const without = model("g/p/b");
    expect(carryVariant("low", withLow)).toBe("low");
    expect(carryVariant("medium", withLow)).toBeNull();
    expect(carryVariant("off", withLow)).toBe("off");
    expect(carryVariant("off", without)).toBeNull();
    expect(carryVariant(null, withLow)).toBeNull();
  });
});

describe("sidebar summaries", () => {
  it("summarise each section from live values", () => {
    expect(summaries(body)).toEqual({
      gateways: "omniroute · inception",
      layers: "reflex · meta · cognition",
      permissions: "automatic review on",
      subagents: "max 6 per session",
      appearance: "Nord Light",
    });
  });
  it("say when the review is off", () => {
    expect(summaries({ ...body, fields: [field("permissions.automatic_review", false)] }).permissions).toBe("ask for every call");
  });
  it("show one folder for files that share it", () => {
    expect(fileLines(body.paths)).toEqual(["~/.config/withnav/", "config.json · models.json"]);
    expect(fileLines({ config: "/home/u/.config/navai/config.json", models: "/ws/models.json", agents: [] })).toEqual(["~/.config/withnav/config.json", "/ws/models.json"]);
  });
  it("label the users of a gateway like the design", () => {
    expect(usedByLabel("layers.reflex")).toBe("reflex");
    expect(usedByLabel("agents.build")).toBe("cognition · build");
  });
});

describe("settings search", () => {
  const index = searchIndex(body, [{ id: "build", description: "Implements", mode: "primary", assignment: null, effective: null, warning: null, inherits_parent: false }]);
  it("shows everything without a query", () => {
    expect(searchMatches(index, "  ")).toBeNull();
  });
  it("matches every word, across keys, labels and values", () => {
    const hits = searchMatches(index, "watchdog root")!;
    expect([...hits]).toEqual(["sub:root"]);
    expect(searchMatches(index, "opus")!.has("agent:build")).toBe(true);
    expect(searchMatches(index, "OMNIROUTE_API_KEY")!.has("gateway:omniroute")).toBe(true);
    expect(searchMatches(index, "nothing-like-this")!.size).toBe(0);
  });
});

describe("save state", () => {
  const run = (...events: SaveEvent[]) => events.reduce(saveReducer, initialSave);
  it("shows Saving… while a change is out, then Saved", () => {
    const busy = run({ type: "start", key: "tui.theme", value: "ayu-light" });
    expect(statusLine(busy)).toEqual({ tone: "busy", text: "Saving…" });
    expect(busy.pending["tui.theme"]).toBe("ayu-light");
    const done = saveReducer(busy, { type: "saved", key: "tui.theme" });
    expect(statusLine(done)).toEqual({ tone: "ok", text: "Saved" });
    expect(done.pending).toEqual({});
  });
  it("says when the saved change takes effect, as the server reports it", () => {
    const done = run({ type: "start", key: "layers.reflex", value: null }, { type: "saved", key: "layers.reflex" });
    expect(statusLine(done, "restart").text).toBe("Saved · applies after restarting WithNav");
    expect(statusLine(done, "new_sessions").text).toBe("Saved · applies to new sessions and subagents");
    expect(appliesText("live")).toBe("Applies here now; other open windows on their next start");
    expect(statusLine(initialSave, "restart").text).toBe("Saved");
    expect(appliesText(undefined)).toBeNull();
  });
  it("keeps a 400 at its key until that key saves", () => {
    const bad = run({ type: "start", key: "subagents.max_concurrent", value: 99 }, { type: "invalid", key: "subagents.max_concurrent", message: "must be from 1 to 64" });
    expect(bad.fieldErrors["subagents.max_concurrent"]).toBe("must be from 1 to 64");
    // The message is shown at the control; the top bar only says it was not saved.
    expect(statusLine(bad)).toEqual({ tone: "bad", text: "Not saved" });
    const otherSaved = [{ type: "start", key: "tui.theme", value: "x" }, { type: "saved", key: "tui.theme" }] as SaveEvent[];
    expect(otherSaved.reduce(saveReducer, bad).fieldErrors["subagents.max_concurrent"]).toBe("must be from 1 to 64");
    const fixed = saveReducer(saveReducer(bad, { type: "start", key: "subagents.max_concurrent", value: 8 }), { type: "saved", key: "subagents.max_concurrent" });
    expect(fixed.fieldErrors).toEqual({});
  });
  it("remembers which control sent the refused value", () => {
    const bad = run({ type: "start", key: "agents.build", value: { model: "m", variant: "x" }, control: "variant" }, { type: "invalid", key: "agents.build", message: "no variant x" });
    expect(bad.errorControl["agents.build"]).toBe("variant");
  });
  it("on 409 drops every queued change and says why", () => {
    const stale = run({ type: "start", key: "c", value: 0 }, { type: "invalid", key: "c", message: "no" }, { type: "start", key: "a", value: 1 }, { type: "start", key: "b", value: 2 }, { type: "stale" });
    expect(stale.inFlight).toBe(0);
    expect(stale.fieldErrors).toEqual({});
    expect(stale.pending).toEqual({});
    expect(statusLine(stale)).toEqual({ tone: "warn", text: "config.json changed elsewhere; reloaded" });
  });
  it("reports a network failure without blaming a field", () => {
    const failed = run({ type: "start", key: "a", value: 1 }, { type: "failed", key: "a", message: "unreachable" });
    expect(failed.fieldErrors).toEqual({});
    expect(statusLine(failed).text).toBe("Not saved: unreachable");
  });
});

describe("catalogue grouping", () => {
  const models = [model("omniroute/cc/opus"), model("omniroute/zai/glm"), model("inception/mercury", { api: "fim" })];
  it("groups gateway › provider and shows direct models under (direct) when asked", () => {
    const rows = groupCatalog(models, "", { collapsed: () => false, directRows: true });
    expect(rows.map((r) => (r.type === "model" ? r.model.name : `${r.type}:${r.label}`))).toEqual([
      "gateway:inception",
      "provider:(direct)",
      "inception/mercury",
      "gateway:omniroute",
      "provider:cc",
      "omniroute/cc/opus",
      "provider:zai",
      "omniroute/zai/glm",
    ]);
  });
  it("folds groups, but never while searching", () => {
    const folded = groupCatalog(models, "", { collapsed: (k) => k === "omniroute/cc" });
    expect(folded.some((r) => r.type === "model" && r.model.name === "omniroute/cc/opus")).toBe(false);
    const searching = groupCatalog(models, "opus", { collapsed: () => true });
    expect(searching.filter((r) => r.type === "model").map((r) => r.type === "model" && r.model.name)).toEqual(["omniroute/cc/opus"]);
  });
});
