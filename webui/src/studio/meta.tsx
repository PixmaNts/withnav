// How each node type and action looks: tag, icon, colour role, plain-language description.
// Colours are theme tokens (docs/design/README.md), never raw values.

import { BellRinging, ChartBar, ChatText, CheckCircle, Compass, Function as FunctionIcon, Gauge, GitFork, Hand, Lightning, Note, Prohibit } from "@phosphor-icons/react";
import type { ReactNode } from "react";
import type { Answer, Definition, FlowNode, RunNode } from "./types";

export interface Look {
  tag: string;
  icon: (size: number) => ReactNode;
  color: string;
}

export const TRIGGER_LOOK: Look = { tag: "TRIGGER", icon: (s) => <Lightning size={s} />, color: "var(--at)" };

export const TYPE_LOOK: Record<"noul" | "choice" | "score" | "expression", Look & { sub: string; describe: string }> = {
  noul: { tag: "NOUL", icon: (s) => <Gauge size={s} />, color: "var(--bl)", sub: "yes-prob.", describe: "Probability of yes. Ports: yes ≥ threshold, no." },
  choice: { tag: "CHOICE", icon: (s) => <GitFork size={s} />, color: "var(--at)", sub: "options", describe: "Named options. One port per option." },
  score: { tag: "SCORE", icon: (s) => <ChartBar size={s} />, color: "var(--or)", sub: "levels", describe: "2 to 10 ordered levels. Ports: at least, below." },
  expression: { tag: "EXPRESSION", icon: (s) => <FunctionIcon size={s} />, color: "var(--pu)", sub: "", describe: "A condition over its inputs. No instinct call." },
};

export const ACTION_LOOK: Record<string, Look & { label: string; describe: string }> = {
  no_objection: { tag: "ACTION", icon: (s) => <CheckCircle size={s} />, color: "var(--gr)", label: "No objection", describe: "The call goes on to the normal permission review." },
  deny: { tag: "ACTION", icon: (s) => <Prohibit size={s} />, color: "var(--rd)", label: "Deny the tool call", describe: "The call does not run; the note is what the model sees." },
  ask_captain: { tag: "ACTION", icon: (s) => <Hand size={s} />, color: "var(--or)", label: "Ask the Captain", describe: "A permission request with the note, without the model review." },
  remark: { tag: "ACTION", icon: (s) => <Compass size={s} />, color: "var(--at)", label: "Show a remark", describe: "The metacognition writes a remark in the editor." },
  wake: { tag: "ACTION", icon: (s) => <BellRinging size={s} />, color: "var(--rd)", label: "Wake the cognition", describe: "The cognition is woken with the note." },
  inject_note: { tag: "ACTION", icon: (s) => <ChatText size={s} />, color: "var(--bl)", label: "Inject a note", describe: "A note reaches the cognition at its next safe point, marked as from a flow." },
  log: { tag: "ACTION", icon: (s) => <Note size={s} />, color: "var(--mu)", label: "Log only", describe: "Recorded in the run trace only." },
};

const UNKNOWN_ACTION = { tag: "ACTION", icon: (s: number) => <Lightning size={s} />, color: "var(--mu)", label: "Unknown action", describe: "Not in the catalogue." };

export function actionLook(name: string) {
  return ACTION_LOOK[name] ?? { ...UNKNOWN_ACTION, label: name };
}

export function lookOf(node: FlowNode | null): Look {
  if (!node) return TRIGGER_LOOK;
  return node.type === "action" ? actionLook(node.action) : TYPE_LOOK[node.type];
}

/** A noul threshold as a number, resolving `constant.NAME`; null when it does not resolve. */
export function thresholdValue(node: FlowNode, def: Definition): number | null {
  if (node.type !== "noul") return null;
  if (typeof node.threshold === "number") return node.threshold;
  const v = def.constants[node.threshold.replace(/^constant\./, "")];
  return typeof v === "number" ? v : null;
}

/** The label of a port as the card shows it. */
export function portLabel(node: FlowNode | null, port: string, def: Definition): string {
  if (!node) return port;
  if (node.type === "noul" && port === "yes") {
    const t = thresholdValue(node, def);
    const name = typeof node.threshold === "string" ? node.threshold.replace(/^constant\./, "") : null;
    return `yes ≥ ${name ? `${name}${t !== null ? ` (${t})` : ""}` : t ?? "?"}`;
  }
  if (node.type === "score" && port === "at_least") return `≥ ${node.at_least || "?"}`;
  return port;
}

export function fmt(n: number, digits = 2): string {
  return Number.isInteger(n) ? String(n) : n.toFixed(digits);
}

export function answerText(answer: Answer, port: string | null, node: FlowNode): string {
  if (answer.type === "noul") return `${fmt(answer.noul)} → ${port ?? "?"}`;
  if (answer.type === "choice") return `${answer.choice} · ${fmt(answer.probabilities[answer.choice] ?? answer.confidence)}`;
  const level = node.type === "score" ? node.levels[Math.round(answer.score)] : undefined;
  return `${fmt(answer.score, 1)}${level ? ` · ${level}` : ""} → ${port === "at_least" ? "at least" : port ?? "?"}`;
}

/** The run strip text of a node in a run. */
export function runText(node: FlowNode, r: RunNode | null, failed: boolean): string {
  if (!r || r.status === "not_reached") return "not reached";
  if (r.status === "failed") return r.error ?? "failed";
  if (r.answer) return answerText(r.answer, r.port, node);
  if (node.type === "expression") {
    const vars = Object.entries(r.variables).map(([k, v]) => `${k} = ${JSON.stringify(v)}`);
    return [String(r.result), ...vars].join(" · ");
  }
  if (node.type === "action") return failed ? "discarded: the run failed" : "proposed";
  return "done";
}

export function money(usd: number | null | undefined): string {
  if (usd === null || usd === undefined) return "cost unknown";
  return usd < 0.01 ? `$${usd.toFixed(5)}` : `$${usd.toFixed(2)}`;
}

export function tokens(n: number): string {
  return n >= 10_000 ? `${(n / 1000).toFixed(1)}k` : String(n);
}
