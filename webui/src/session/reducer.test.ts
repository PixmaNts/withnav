import { readFileSync } from "node:fs";
import { describe, expect, it } from "vitest";
import { SseDecoder } from "../api/sse";
import { toolLine } from "./toolText";
import type { Envelope } from "../api/types";
import { emptyView, openPermissions, reduce, reduceAll, type Turn } from "./reducer";

/** A recorded 0.5.0 server stream: build lists files, then writes one. */
function fixture(name: string): Envelope[] {
  const raw = readFileSync(new URL(`../../../tests/fixtures/events/${name}`, import.meta.url), "utf8");
  return new SseDecoder().push(raw).map((frame) => JSON.parse(frame.data) as Envelope);
}

let seq = 0;
function env(kind: string, payload: object, at = "2026-10-06T10:00:00.000Z"): Envelope {
  return { schema_version: 1, event_id: `e${seq}`, seq: seq++, session_id: "S", actor_id: "agent:main", kind, occurred_at: at, causation_id: null, correlation_id: null, payload };
}

const turns = (items: ReturnType<typeof emptyView>["items"]) => items.filter((i): i is Turn => i.kind === "turn");

describe("transcript projection", () => {
  it("keeps the image references of a Captain message and of a tool result, never bytes", () => {
    const image = { attachment_id: "A1", media_type: "image/png", name: "shot.png", size: 10, width: 2, height: 2 };
    const view = reduceAll(emptyView(), [
      env("message.received", { source: "user:http", content: "", images: [image, { name: "no id" }] }),
      env("tool.post_call", { tool_call_id: "r1", name: "read", status: "ok", output: "shot.png is an image", metadata: { images: [image] } }),
    ]);
    const captain = view.items[0];
    expect(captain?.kind === "captain" && captain.images).toEqual([image]);
    expect(view.tools.r1?.metadata.images).toEqual([image]);
  });
  it("renders stored legacy instinct approvals with the layer name", () => {
    const view = reduceAll(emptyView(), fixture("write-file.sse"));
    expect(toolLine(Object.values(view.tools).find((call) => call.name === "write")!).meta).toContain("auto · instinct");
  });
  it("projects a recorded live stream: Captain message, tools in order, final text", () => {
    const view = reduceAll(emptyView(), fixture("write-file.sse"));
    expect(view.items.map((i) => i.kind)).toEqual(["captain", "turn"]);
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
    // Recorded replay: edit, then a bash call gated for the Captain, denied.
    const events = fixture("permission-denied.sse");
    const waiting = reduceAll(emptyView(), events.filter((e) => e.seq <= 31));
    expect(waiting.activity).toBe("running");
    expect(openPermissions(waiting).map((p) => p.tool)).toEqual(["bash"]);
    const done = reduceAll(emptyView(), events);
    expect(done.items.map((i) => i.kind)).toEqual(["captain", "turn"]);
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

  it("replaces streamed text with final response text across a reconnection gap", () => {
    const events = fixture("reconnect-gap.sse");
    const view = reduceAll(emptyView(), events);
    const [turn] = turns(view.items);
    expect(turn!.parts).toEqual([
      { type: "text", stepId: "01M48RGAPSTEP0000000000000", text: "Paris is the capital of France and its largest city." },
    ]);
    expect(turn!.status).toBe("done");
    expect(view.activity).toBe("idle");
  });

  it("settles running turns and tool calls on session.end", () => {
    const events = fixture("session-end-interrupted.sse");
    const view = reduceAll(emptyView(), events);
    const [turn] = turns(view.items);
    expect(turn!.status).toBe("interrupted");
    expect(view.tools["toolu_01listfilescall000000001"]?.status).toBe("ok");
    expect(view.tools["toolu_01longtaskcall000000001"]?.status).toBe("interrupted");
    expect(view.activity).toBe("stopped");
  });

  it("cancels a pending permission on session.end and keeps the completed call", () => {
    const events = fixture("session-end-permission.sse");
    const view = reduceAll(emptyView(), events);
    const permission = view.permissions["01M48ENPREQ00000000000000"];
    expect(permission?.resolution).toBe("cancelled");
    expect(openPermissions(view)).toEqual([]);
    expect(view.tools["toolu_01listcall00000000000001"]?.status).toBe("ok");
    expect(view.activity).toBe("stopped");
    const [turn] = turns(view.items);
    expect(turn!.status).toBe("yielded");
  });

  it("settles nothing twice on a repeated session.end", () => {
    const events = fixture("session-end-permission.sse");
    const view = reduceAll(emptyView(), events);
    const second = events.slice();
    const end = second[second.length - 1]!;
    second[second.length - 1] = { ...end, seq: end.seq + 1 };
    const again = reduceAll(view, second.slice(-1));
    // The cursor and the ended timestamp legitimately move; the settlement
    // itself does not repeat.
    expect(again.permissions["01M48ENPREQ00000000000000"]?.resolution).toBe("cancelled");
    expect(again.tools["toolu_01listcall00000000000001"]?.status).toBe("ok");
    expect(again.activity).toBe("stopped");
    const [turn] = turns(again.items);
    expect(turn!.status).toBe("yielded");
  });

  it("projects turn failures and preserves error details", () => {
    const events = fixture("turn-error.sse");
    const view = reduceAll(emptyView(), events);
    const [turn] = turns(view.items);
    expect(turn!.status).toBe("error");
    expect(turn!.error).toBe("Prompt exceeds maximum context length");
    expect(view.activity).toBe("idle");
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

  it("keeps why an Always answer was not saved", () => {
    seq = 0;
    let view = reduceAll(emptyView(), [
      env("turn.start", { turn_id: "t1", actor_id: "agent:main", task_id: "k" }),
      env("permission.requested", { request_id: "R", tool_call_id: "c1", tool: "bash", arguments: "{}", scope: { kind: "folder", value: "/w" }, agent: "build", reason: "r" }),
    ]);
    view = reduce(view, env("permission.resolved", { request_id: "R", tool_call_id: "c1", resolution: "allow_always", always_saved: false, always_error: "database is locked" }));
    expect(view.permissions.R).toMatchObject({ resolution: "allow_always", alwaysSaved: false, message: "database is locked" });
    view = reduce(view, env("permission.resolved", { request_id: "R", tool_call_id: "c1", resolution: "allow_always" }));
    expect(view.permissions.R!.alwaysSaved).toBeUndefined();
  });

  it("labels messages by author: Captain, Nav wake, subagent result", () => {
    seq = 0;
    const view = reduceAll(emptyView(), [
      env("message.received", { command_id: "a", source: "user:http", target: "agent:main", mode: "next-turn", content: "hi", selected_skill: { name: "git", origin: "captain" } }),
      env("message.received", { command_id: "b", source: "navigator", target: "agent:main", mode: "next-turn", content: "Navigator noticed: drift from the intent (0.86) in src/config.rs:42\n```diff\n+x\n```" }),
      env("message.received", { command_id: "c", source: "subagent:vigie", target: "agent:main", mode: "next-turn", content: "found it" }),
    ]);
    expect(view.items.map((i) => i.kind)).toEqual(["captain", "wake", "subagent-result"]);
    expect(view.items[0]).toMatchObject({ skill: "git" });
    expect(view.items[1]).toMatchObject({ note: { reason: "drift from the intent", score: "0.86", path: "src/config.rs", line: 42, diff: "+x" } });
  });

  it("tracks subagents from spawn to done", () => {
    seq = 0;
    let view = reduceAll(emptyView(), [
      env("session.agent_changed", { agent: "plan", previous: "build" }),
      env("subagent.spawned", { parent_session_id: "S", child_session_id: "C", agent: "vigie", goal: "map callers", tool_call_id: "t1" }, "2026-10-06T10:00:00.000Z"),
    ]);
    expect(view.children.C).toMatchObject({ agent: "vigie", status: "running", spawnedBy: "plan" });
    view = reduce(view, env("subagent.done", { parent_session_id: "S", child_session_id: "C", agent: "vigie", outcome: "done", notification_id: "N" }, "2026-10-06T10:00:42.000Z"));
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

  it("tells the Captain which MCP tools a refresh added or removed", () => {
    seq = 0;
    const view = reduceAll(emptyView(), [
      env("command.result", { operation_id: "o", name: "mcp_tools", ok: true, data: { added: ["mcp__a__x"], removed: ["mcp__a__y"], removed_more: 2 } }),
    ]);
    expect(view.items).toEqual([
      { kind: "notice", key: "n:0", text: "/mcp_tools · MCP tools added: mcp__a__x\nMCP tools removed: mcp__a__y, and 2 more", tone: "info" },
    ]);
  });
});
