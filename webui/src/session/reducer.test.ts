import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { SseDecoder } from "../api/sse";
import type { Envelope } from "../api/types";
import { emptyView, openPermissions, reduce, reduceAll, type Turn } from "./reducer";

/** A real stream captured from `navai serve` 0.5.0: build lists files, then writes one. */
function fixture(name: string): Envelope[] {
  const raw = readFileSync(new URL(`./__fixtures__/${name}`, import.meta.url), "utf8");
  return new SseDecoder().push(raw).map((frame) => JSON.parse(frame.data) as Envelope);
}

let seq = 0;
function env(kind: string, payload: object, at = "2026-10-06T10:00:00.000Z"): Envelope {
  return { schema_version: 1, event_id: `e${seq}`, seq: seq++, session_id: "S", actor_id: "agent:main", kind, occurred_at: at, causation_id: null, correlation_id: null, payload };
}

const turns = (items: ReturnType<typeof emptyView>["items"]) => items.filter((i): i is Turn => i.kind === "turn");

describe("transcript projection", () => {
  it("projects a recorded live stream: driver message, tools in order, final text", () => {
    const view = reduceAll(emptyView(), fixture("write-file.sse"));
    expect(view.items.map((i) => i.kind)).toEqual(["driver", "turn"]);
    const [turn] = turns(view.items);
    expect(turn!.status).toBe("done");
    expect(turn!.parts.map((p) => p.type)).toEqual(["tools", "text"]);
    const calls = (turn!.parts[0] as { calls: string[] }).calls.map((id) => view.tools[id]!);
    expect(calls.map((c) => [c.name, c.status])).toEqual([["glob", "ok"], ["write", "ok"]]);
    expect((turn!.parts[1] as { text: string }).text).toContain("hello.txt");
    expect(view.activity).toBe("idle");
    expect(view.context?.percent).toBeTypeOf("number");
  });

  it("reaches the same transcript from a replay without deltas", () => {
    const live = reduceAll(emptyView(), fixture("write-file.sse"));
    const durable = fixture("write-file.sse").filter((e) => e.kind !== "llm.delta" && e.kind !== "context.updated");
    const replayed = reduceAll(emptyView(), durable);
    expect(replayed.items).toEqual(live.items);
    expect(replayed.tools).toEqual(live.tools);
  });

  it("keeps a turn that yielded for a permission in one block, and running until it is answered", () => {
    // Recorded replay: edit, then a bash call gated for the driver, denied.
    const events = fixture("permission-denied.sse");
    const waiting = reduceAll(emptyView(), events.filter((e) => e.seq <= 31));
    expect(waiting.activity).toBe("running");
    expect(openPermissions(waiting).map((p) => p.tool)).toEqual(["bash"]);
    const done = reduceAll(emptyView(), events);
    expect(done.items.map((i) => i.kind)).toEqual(["driver", "turn"]);
    const [turn] = turns(done.items);
    expect(turn!.parts.map((p) => p.type)).toEqual(["tools", "permission", "text"]);
    expect(Object.values(done.permissions)[0]!.resolution).toBe("deny");
    expect(done.activity).toBe("idle");
  });

  it("ignores events at or before the cursor (reconnect overlap)", () => {
    const events = fixture("write-file.sse");
    const once = reduceAll(emptyView(), events);
    const twice = reduceAll(once, events.slice(5));
    expect(twice).toBe(once);
  });

  it("streams text deltas into one block, then the response replaces it", () => {
    seq = 0;
    let view = reduceAll(emptyView(), [
      env("turn.start", { turn_id: "T", task_id: "K", actor_id: "agent:main" }),
      env("llm.delta", { turn_id: "T", step_id: "A", delta: { delta_type: "thinking", text: "let me " } }),
      env("llm.delta", { turn_id: "T", step_id: "A", delta: { delta_type: "text", text: "Hel" } }),
      env("llm.delta", { turn_id: "T", step_id: "A", delta: { delta_type: "text", text: "lo" } }),
    ]);
    let [turn] = turns(view.items);
    expect(turn!.parts).toEqual([{ type: "reasoning", stepId: "A" }, { type: "text", stepId: "A", text: "Hello" }]);
    view = reduce(view, env("llm.response", { turn_id: "T", step_id: "A", message: { text: "Hello.", thinking: "let me think", tool_calls: [] } }));
    [turn] = turns(view.items);
    expect(turn!.parts[1]).toEqual({ type: "text", stepId: "A", text: "Hello." });
    expect(turn!.reasoning.A).toBe("let me think");
  });

  it("places a permission inside its turn and resolves it", () => {
    seq = 0;
    let view = reduceAll(emptyView(), [
      env("turn.start", { turn_id: "T", task_id: "K", actor_id: "agent:main" }),
      env("tool.pre_call", { tool_call_id: "c1", name: "bash", args: { command: "ls" }, actor_id: "agent:main" }),
      env("permission.requested", { request_id: "R", tool_call_id: "c1", tool: "bash", arguments: '{"command":"ls"}', scope: { kind: "folder", value: "/w" }, agent: "build", reason: "`bash` acts in /w" }),
    ]);
    expect(openPermissions(view).map((p) => p.requestId)).toEqual(["R"]);
    expect(turns(view.items)[0]!.parts.map((p) => p.type)).toEqual(["tools", "permission"]);
    view = reduce(view, env("permission.resolved", { request_id: "R", tool_call_id: "c1", resolution: "deny", message: "no" }));
    expect(view.permissions.R).toMatchObject({ resolution: "deny", message: "no" });
    expect(openPermissions(view)).toEqual([]);
  });

  it("labels messages by author: driver, navigator wake, subagent result", () => {
    seq = 0;
    const view = reduceAll(emptyView(), [
      env("message.received", { command_id: "a", source: "user:http", target: "agent:main", mode: "next-turn", content: "hi", selected_skill: { name: "git", origin: "driver" } }),
      env("message.received", { command_id: "b", source: "navigator", target: "agent:main", mode: "next-turn", content: "Navigator noticed: drift from the intent (0.86) in src/config.rs:42\n```diff\n+x\n```" }),
      env("message.received", { command_id: "c", source: "subagent:explore", target: "agent:main", mode: "next-turn", content: "found it" }),
    ]);
    expect(view.items.map((i) => i.kind)).toEqual(["driver", "wake", "subagent-result"]);
    expect(view.items[0]).toMatchObject({ skill: "git" });
    expect(view.items[1]).toMatchObject({ note: { reason: "drift from the intent", score: "0.86", path: "src/config.rs", line: 42, diff: "+x" } });
  });

  it("tracks subagents from spawn to done", () => {
    seq = 0;
    let view = reduceAll(emptyView(), [
      env("session.agent_changed", { agent: "plan", previous: "build" }),
      env("subagent.spawned", { parent_session_id: "S", child_session_id: "C", agent: "explore", goal: "map callers", tool_call_id: "t1" }, "2026-10-06T10:00:00.000Z"),
    ]);
    expect(view.children.C).toMatchObject({ agent: "explore", status: "running", spawnedBy: "plan" });
    view = reduce(view, env("subagent.done", { parent_session_id: "S", child_session_id: "C", agent: "explore", outcome: "done", notification_id: "N" }, "2026-10-06T10:00:42.000Z"));
    expect(view.children.C).toMatchObject({ status: "done", endedAt: "2026-10-06T10:00:42.000Z" });
  });

  it("session.end interrupts what was running and cancels waiting permissions", () => {
    seq = 0;
    const view = reduceAll(emptyView(), [
      env("turn.start", { turn_id: "T", task_id: "K", actor_id: "agent:main" }),
      env("tool.pre_call", { tool_call_id: "c1", name: "read", args: { path: "a" }, actor_id: "agent:main" }),
      env("permission.requested", { request_id: "R", tool_call_id: "c1", tool: "read", arguments: "{}", scope: { kind: "folder", value: "/" }, agent: "build", reason: "" }),
      env("session.end", { reason: "stop", final_seq: 3 }),
    ]);
    expect(view.activity).toBe("stopped");
    expect(turns(view.items)[0]!.status).toBe("interrupted");
    expect(view.tools.c1!.status).toBe("interrupted");
    expect(view.permissions.R!.resolution).toBe("cancelled");
  });

  it("follows model, variant and agent changes and notes the switches", () => {
    seq = 0;
    const view = reduceAll(emptyView(), [
      env("model.changed", { revision: 1, reason: "initial", new: { model: "gw/a", agent: "build" }, variant: null, window: 1, input_budget: 1, from_message: 0 }),
      env("model.changed", { revision: 2, reason: "variant", new: { model: "gw/a", agent: "build" }, variant: "high", window: 1, input_budget: 1, from_message: 0 }),
    ]);
    expect([view.model, view.variant, view.agent]).toEqual(["gw/a", "high", "build"]);
    expect(view.items).toEqual([{ kind: "notice", key: "n:1", text: "Variant: high", tone: "info" }]);
  });

  it("shows a failed command and stays quiet on a selection switch", () => {
    seq = 0;
    const view = reduceAll(emptyView(), [
      env("command.result", { operation_id: "o", name: "agent", ok: false, error: "the agent is working" }),
      env("command.result", { operation_id: "p", name: "agent", ok: true, data: { applies: "now" } }),
    ]);
    expect(view.items).toEqual([{ kind: "notice", key: "n:0", text: "/agent · the agent is working", tone: "error" }]);
  });
});
