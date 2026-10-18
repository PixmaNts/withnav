// A server command's outcome as the short text the transcript shows: the
// client's own wording over the `command.result` data, like the terminal
// client (`crates/withnav-client-state/src/command.rs`). Selection switches return null: their
// `model.changed` event already says what changed.

import { tokens } from "./contextMeter";

// eslint-disable-next-line @typescript-eslint/no-explicit-any
export function outcomeText(name: string, ok: boolean, data: any, error?: string): string | null {
  if (!ok) return error || "the command failed";
  if (!data || typeof data !== "object") return null;
  if (data.applies === "now") return null;
  if (data.applies === "after_turn" || data.applies === "next_turn") return "accepted, applies when this turn ends";
  const text = (key: string) => (typeof data[key] === "string" ? data[key] : "");
  switch (name) {
    case "help":
      return Array.isArray(data.commands)
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        ? data.commands.map((c: any) => `/${c.name}${c.args_hint ? ` ${c.args_hint}` : ""} · ${c.description}`).join("\n")
        : null;
    case "model":
      return `${text("model") || "unknown"}${data.variant ? ` · ${data.variant}` : ""}${typeof data.window === "number" ? ` · window ${tokens(data.window)}` : ""}`;
    case "context":
      if (typeof data.estimate_total !== "number" || typeof data.window !== "number") return null;
      return `about ${tokens(data.estimate_total)} of ${tokens(data.window)} · ${data.percent ?? "?"}%${typeof data.input_budget === "number" ? ` · input budget ${tokens(data.input_budget)}` : ""}`;
    case "variant": {
      const offered: string[] = Array.isArray(data.offered) ? data.offered.filter((v: unknown) => typeof v === "string") : [];
      return `variant ${data.variant ?? "off"} · ${offered.length ? `on offer: ${offered.join(", ")}` : "this model has no variants"}`;
    }
    case "agent":
      if (data.changed === true) return null;
      if (Array.isArray(data.agents))
        // eslint-disable-next-line @typescript-eslint/no-explicit-any
        return [`agent ${text("agent")}`, ...data.agents.map((a: any) => `${a.id} · ${a.description || a.mode}`)].join("\n");
      return `agent ${text("agent")}`;
    case "stop":
      return "stopped";
    case "new":
      return null;
    case "compact":
      return null;
    default:
      return null;
  }
}
