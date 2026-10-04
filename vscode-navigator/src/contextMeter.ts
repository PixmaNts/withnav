export interface ContextSnapshot {
  alias: string;
  window: number;
  window_source: string;
  estimate_total: number;
  breakdown: { system: number; rules: number; skills_catalog: number; tools: number; messages: number };
  output_reserve: number;
  margin: number;
  input_budget: number;
  percent: number;
  revision: number;
  incarnation?: string;
  last_usage?: { prompt_tokens: number; completion_tokens: number; step: string } | null;
}

const count = (n: unknown): n is number => Number.isSafeInteger(n) && (n as number) >= 0;

/** Validate server data before it reaches the webview. */
export function parseContext(value: any): ContextSnapshot | undefined {
  if (!value || typeof value.alias !== "string" || typeof value.window_source !== "string" ||
      ![value.window, value.estimate_total, value.output_reserve, value.margin, value.input_budget, value.percent, value.revision].every(count) ||
      !value.breakdown || !["system", "rules", "skills_catalog", "tools", "messages"].every(key => count(value.breakdown[key])) ||
      (value.incarnation !== undefined && typeof value.incarnation !== "string") ||
      (value.last_usage != null && (![value.last_usage.prompt_tokens, value.last_usage.completion_tokens].every(count) || typeof value.last_usage.step !== "string"))) return undefined;
  return value;
}

export function tokens(n: number): string {
  if (n >= 999500) {
    const tenths = Math.round(n / 100000);
    return `${tenths / 10}M`;
  }
  return n >= 1000 ? `${Math.round(n / 1000)}k` : String(n);
}

export function contextMeter(snapshot: ContextSnapshot) {
  // The server's percent (estimate over the full window) is what the terminal client shows too, so
  // one conversation reads the same in both clients; the input budget stays in the detail.
  const percent = snapshot.percent;
  const rows = [
    "Next request · estimated tokens",
    `System: ${tokens(snapshot.breakdown.system)}`,
    `Rules: ${tokens(snapshot.breakdown.rules)}`,
    `Skills catalog: ${tokens(snapshot.breakdown.skills_catalog)}`,
    `Tools: ${tokens(snapshot.breakdown.tools)}`,
    `Messages: ${tokens(snapshot.breakdown.messages)}`,
    `Input budget: ${tokens(snapshot.input_budget)}`,
    `Window: ${tokens(snapshot.window)} (${snapshot.window_source})`,
    `Output reserve: ${tokens(snapshot.output_reserve)}`,
    `Safety margin: ${tokens(snapshot.margin)}`,
  ];
  if (snapshot.last_usage) rows.push(`Last request · provider count: ${tokens(snapshot.last_usage.prompt_tokens)} prompt + ${tokens(snapshot.last_usage.completion_tokens)} completion`);
  else rows.push("Last request · provider count: —");
  return { percent, fill: Math.min(100, percent), level: percent >= 90 ? "high" : percent >= 70 ? "mid" : "low",
    label: `~${tokens(snapshot.estimate_total)} / ${tokens(snapshot.window)} · ${percent}%`, detail: rows.join("\n") };
}
