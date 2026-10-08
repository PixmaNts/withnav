import type { ToolCall } from "./reducer";

/** What a tool row says: the verb, what it touched, and a short result count. */
export interface ToolLine {
  verb: string;
  target: string;
  meta: string;
}

const s = (value: unknown): string | undefined => (typeof value === "string" && value ? value : undefined);
const n = (value: unknown): number | undefined => (typeof value === "number" && Number.isFinite(value) ? value : undefined);
const plural = (count: number, one: string, many = `${one}s`) => `${count} ${count === 1 ? one : many}`;

export function toolLine(call: ToolCall): ToolLine {
  const a = call.args && typeof call.args === "object" ? call.args : {};
  const m = call.metadata && typeof call.metadata === "object" ? call.metadata : {};
  let target = "";
  let meta = "";
  switch (call.name) {
    case "read":
    case "write":
    case "edit":
      target = s(a.path) ?? "";
      if (call.name === "read" && call.output && call.status === "ok") meta = plural(call.output.split("\n").length, "line");
      break;
    case "grep":
      target = s(a.pattern) ? `"${a.pattern}"` : "";
      if (call.output !== undefined && call.status === "ok") {
        const lines = call.output.split("\n").filter((line) => line && !line.startsWith("["));
        meta = plural(lines.length, "match", "matches");
      }
      break;
    case "glob":
      target = s(a.pattern) ?? "";
      if (n(m.total_matches) !== undefined) meta = plural(m.total_matches, "file");
      break;
    case "bash":
      target = (s(a.command) ?? "").replace(/\s+/g, " ").trim();
      if (n(m.exit_code) !== undefined && m.exit_code !== 0) meta = `exit ${m.exit_code}`;
      break;
    case "fetch":
      target = s(a.url) ?? "";
      break;
    case "task":
      target = s(a.agent) ? `@${a.agent}` : "";
      break;
    case "skill":
      target = s(a.name) ?? "";
      break;
    default: {
      const first = Object.values(a).find((value) => typeof value === "string");
      target = typeof first === "string" ? first : "";
    }
  }
  if (call.status === "running") meta = "running";
  else if (call.status === "denied") meta = "denied";
  else if (call.status === "interrupted") meta = "interrupted";
  else if (call.status === "error" && !meta) meta = "failed";
  const gate = m.permission_gate;
  if (gate && typeof gate === "object" && gate.policy_version === 1) {
    const source = gate.source;
    const approval = source === "instinct" || source === "jev" ? "auto · instinct"
      : source === "metacognition" ? "auto · metacognition"
      : source === "captain" || source === "driver" ? "approved · Captain" : "";
    if (approval) meta = meta ? `${meta} · ${approval}` : approval;
  }
  return { verb: call.name, target: target.length > 160 ? `${target.slice(0, 157)}…` : target, meta };
}
