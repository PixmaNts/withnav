/// <reference lib="dom" />
import type { HostToWebview, WebviewToHost, PermissionEvidence, PermissionDecision } from "../chatProtocol";
import { evidenceLabel, permissionOutcome } from "../eventView";
import { contextMeter } from "../contextMeter";
import { ConversationIdentity } from "./conversationIdentity";
import { mountConversations } from "./conversations";
import { diffKind, parseWake } from "./wake";
import { commandRows } from "../commands";
import type { CommandInfo } from "../chatProtocol";

type ViewState = { includeContext?: boolean };
declare function acquireVsCodeApi(): { postMessage(message: WebviewToHost): void; getState(): ViewState | undefined; setState(state: ViewState): void };
const vscode = acquireVsCodeApi();
const rawPost = vscode.postMessage.bind(vscode);
const identity = new ConversationIdentity(() => {
  for (const button of document.querySelectorAll<HTMLButtonElement>(".control-card button, .control-card textarea")) button.disabled = true;
});
const post = (message: WebviewToHost): void => rawPost({ ...message, generation: identity.generation });
const transcript = document.querySelector<HTMLElement>("#transcript")!;
const empty = document.querySelector<HTMLElement>("#empty")!;
const status = document.querySelector<HTMLElement>("#status")!;
const statusLabel = document.querySelector<HTMLElement>("#status-label")!;
const form = document.querySelector<HTMLFormElement>("#composer")!;
const prompt = document.querySelector<HTMLTextAreaElement>("#prompt")!;
const send = document.querySelector<HTMLButtonElement>("#send")!;
const stop = document.querySelector<HTMLButtonElement>("#stop")!;
const includeContext = document.querySelector<HTMLInputElement>("#include-context")!;
const contextPreview = document.querySelector<HTMLElement>("#context-preview")!;
const modelSelect = document.querySelector<HTMLSelectElement>("#model")!;
const palette = document.createElement("div");
palette.id = "command-palette";
palette.className = "command-palette";
palette.setAttribute("role", "listbox");
palette.setAttribute("aria-label", "Commands");
palette.hidden = true;
form.prepend(palette);
const commandError = document.createElement("div");
commandError.className = "command-error";
commandError.setAttribute("role", "alert");
commandError.hidden = true;
prompt.after(commandError);
const agentLabel = document.createElement("span");
agentLabel.className = "agent-label";
agentLabel.textContent = "build";
const pendingModel = document.createElement("span");
pendingModel.className = "pending-model";
modelSelect.after(agentLabel, pendingModel);
let commands: CommandInfo[] = [];
let paletteRows: { name: string; description: string; args_hint?: string }[] = [];
let paletteIndex = 0;
let pickerKind: "model" | "variant" | "skills" | undefined;
let paletteDismissed = false;
function closePalette(): void {
  palette.hidden = true;
  prompt.removeAttribute("aria-activedescendant");
  prompt.setAttribute("aria-expanded", "false");
}
function choosePalette(index: number): void {
  const row = paletteRows[index];
  if (!row) return;
  if (pickerKind === "skills") prompt.value = `/skill ${row.name} `;
  else if (pickerKind) {
    const text = `/${pickerKind} ${row.name}`;
    prompt.value = text;
    post({ type: "send", text, includeContext: false });
  } else prompt.value = `/${row.name} `;
  pickerKind = undefined;
  closePalette();
  prompt.focus();
  updateControls();
}
function renderPalette(): void {
  palette.replaceChildren();
  if (!paletteRows.length) { closePalette(); return; }
  paletteIndex = Math.min(paletteIndex, paletteRows.length - 1);
  palette.hidden = false;
  prompt.setAttribute("aria-controls", palette.id);
  prompt.setAttribute("aria-expanded", "true");
  prompt.setAttribute("aria-activedescendant", `command-option-${paletteIndex}`);
  for (const [index, row] of paletteRows.entries()) {
    const option = document.createElement("div");
    option.id = `command-option-${index}`;
    option.className = "command-option";
    option.setAttribute("role", "option");
    option.setAttribute("aria-selected", String(index === paletteIndex));
    const name = document.createElement("strong");
    name.textContent = `${pickerKind ? "" : "/"}${row.name}${row.args_hint ? ` ${row.args_hint}` : ""}`;
    const description = document.createElement("span");
    description.textContent = row.description;
    option.append(name, description);
    option.addEventListener("mousedown", event => event.preventDefault());
    option.addEventListener("click", () => choosePalette(index));
    palette.append(option);
  }
  palette.children[paletteIndex]?.scrollIntoView?.({ block: "nearest" });
}
function filterPalette(): void {
  pickerKind = undefined;
  paletteRows = paletteDismissed ? [] : commandRows(commands, prompt.value);
  paletteIndex = 0;
  renderPalette();
}
const usage = document.querySelector<HTMLElement>("#usage")!;
const usageFill = document.querySelector<HTMLElement>("#usage-fill")!;
const usageLabel = document.querySelector<HTMLElement>("#usage-label")!;
modelSelect.addEventListener("change", () => {
  if (modelSelect.value) post({ type: "selectModel", alias: modelSelect.value });
});
includeContext.checked = vscode.getState()?.includeContext ?? true;
const conversations = mountConversations(message => post(message), () => {
  if (!prompt.disabled) prompt.focus();
  else document.querySelector<HTMLElement>(".conv-trigger")?.focus();
});
type Turn = { root: HTMLElement; content: HTMLElement; reasoning: HTMLElement; activity: HTMLElement; text: string; thoughts: string; calls: Map<string, HTMLElement> };
const turns = new Map<string, Turn>();
let state = "connecting";
let pendingQuestion = false;
let stickToBottom = true;
/** True while a history replay renders: only live wakes get the arrival cue. */
let replaying = false;
let sessionId = "";
let sending = false;
let lastConsumedDraft: string | undefined;
const permissionCards = new Map<string, { root: HTMLElement; outcome: HTMLElement; controls: HTMLElement; call: HTMLElement }>();
const pauseCards = new Map<string, { outcome: HTMLElement; controls: HTMLElement }>();

function updateControls(): void {
  prompt.disabled = state === "connecting";
  send.disabled = prompt.disabled || sending || !prompt.value.trim();
  stop.hidden = state !== "busy";
}

function setStatus(next: string, message?: string): void {
  state = next;
  status.dataset.state = next;
  statusLabel.textContent = message ? `${next}: ${message}` : next;
  updateControls();
}

function follow(): void {
  if (stickToBottom) transcript.scrollTop = transcript.scrollHeight;
}
transcript.addEventListener("scroll", () => {
  stickToBottom = transcript.scrollHeight - transcript.scrollTop - transcript.clientHeight < 72;
});

function add(element: HTMLElement): void {
  empty.hidden = true;
  transcript.append(element);
  follow();
}

function el<K extends keyof HTMLElementTagNameMap>(tag: K, className?: string, text?: string): HTMLElementTagNameMap[K] {
  const node = document.createElement(tag);
  if (className) node.className = className;
  if (text !== undefined) node.textContent = text;
  return node;
}

function message(author: string, text: string, context?: string, skill?: { name: string; origin: string }, source?: string): void {
  if (author === "navigator") { add(navigatorNote(text)); return; }
  const root = el("article", `message ${author}`);
  const byline = el("div", "byline", author === "driver" ? pendingQuestion ? "You · Answer" : "You" : author === "cognition" ? "Cognition" : "Subagent output · untrusted data");
  if (author === "subagent" && source) byline.title = `Source: ${source}`;
  if (skill) {
    const note = el("span", "skill-note", `skill: ${skill.name}`);
    note.title = `Selected skill · ${skill.origin}`;
    byline.append(note);
  }
  root.append(byline);
  const body = el("div", "message-body");
  renderMarkdown(body, text);
  root.append(body);
  if (context) {
    const chip = el("div", "context-chip sent", context);
    chip.title = "Editor context sent with this message";
    root.append(chip);
  }
  add(root);
  if (pendingQuestion && author === "driver") pendingQuestion = false;
}

// A wake: the navigator glancing over the driver's shoulder. One quiet
// summary, the change and the intent behind a disclosure.
let noteCount = 0;
function navigatorNote(text: string): HTMLElement {
  const wake = parseWake(text);
  const root = el("article", "message note");
  if (!replaying) root.classList.add("arrived");
  const head = el("div", "note-head");
  head.append(navigatorMark());
  const line = el("div", "note-line");
  line.append(el("span", "note-label", "Navigator noticed"));
  if (wake.reason) line.append(el("span", "note-reason", wake.reason));
  head.append(line);
  root.append(head);

  const meta = el("div", "note-meta");
  if (wake.path) {
    const link = el("button", "file-link note-file", displayPath(wake.path) + (wake.line ? `:${wake.line}` : ""));
    link.type = "button";
    link.title = `Open ${wake.path}${wake.line ? ` at line ${wake.line}` : ""}`;
    const target = { path: wake.path, line: wake.line };
    link.addEventListener("click", () => post({ type: "openFile", ...target }));
    meta.append(link);
  }
  if (wake.score) {
    const score = el("span", "note-score", wake.score);
    score.title = "Score, from 0 to 1";
    meta.append(score);
  }

  const details = el("div", "note-details");
  details.id = `note-${++noteCount}`;
  details.hidden = true;
  if (wake.lead) details.append(el("p", "note-lead", wake.lead));
  if (wake.diff) details.append(diffBlock(wake.diff));
  if (wake.intent) {
    const intent = el("section", "note-intent");
    intent.append(el("div", "note-section", "Intent on record"));
    const body = el("div", "message-body");
    renderMarkdown(body, wake.intent);
    intent.append(body);
    details.append(intent);
  }
  if (wake.rest) {
    const body = el("div", "message-body note-rest");
    renderMarkdown(body, wake.rest);
    details.append(body);
  }
  if (details.childElementCount) {
    const toggle = el("button", "note-toggle");
    toggle.type = "button";
    toggle.setAttribute("aria-expanded", "false");
    toggle.setAttribute("aria-controls", details.id);
    const label = el("span", "", wake.diff ? "Show change" : "Show details");
    toggle.append(chevron(), label);
    toggle.addEventListener("click", () => {
      const open = details.hidden;
      details.hidden = !open;
      toggle.setAttribute("aria-expanded", String(open));
      label.textContent = open ? "Hide" : wake.diff ? "Show change" : "Show details";
      if (open) follow();
    });
    meta.append(toggle);
  }
  if (meta.childElementCount) root.append(meta);
  root.append(details);
  return root;
}

/** A relative path stays whole; an absolute one keeps its last two parts. */
function displayPath(path: string): string {
  if (!/^(\/|[A-Za-z]:[\\/])/.test(path)) return path;
  const parts = path.split(/[\\/]/).filter(Boolean);
  return parts.length > 2 ? `…/${parts.slice(-2).join("/")}` : path;
}

const SVG = "http://www.w3.org/2000/svg";
function svgIcon(className: string, d: string): SVGSVGElement {
  const svg = document.createElementNS(SVG, "svg");
  svg.setAttribute("viewBox", "0 0 16 16");
  svg.setAttribute("aria-hidden", "true");
  svg.setAttribute("class", className);
  const path = document.createElementNS(SVG, "path");
  path.setAttribute("d", d);
  svg.append(path);
  return svg;
}
const navigatorMark = (): SVGSVGElement => svgIcon("note-mark", "M8 2.5 12.5 13 8 10.4 3.5 13Z");
const chevron = (): SVGSVGElement => svgIcon("note-chevron", "M6 4l4 4-4 4");

function codeBlock(label: string, value: string, fill: (code: HTMLElement) => void): HTMLElement {
  const wrap = el("div", "code-block");
  const bar = el("div", "code-bar");
  bar.append(el("span", "", label));
  const copy = el("button", "copy", "Copy");
  copy.type = "button";
  copy.addEventListener("click", async () => {
    try { await navigator.clipboard.writeText(value); copy.textContent = "Copied"; }
    catch { copy.textContent = "Copy failed"; }
  });
  bar.append(copy);
  const pre = el("pre");
  const code = el("code");
  fill(code);
  pre.append(code);
  wrap.append(bar, pre);
  return wrap;
}

function diffBlock(diff: string): HTMLElement {
  const block = codeBlock("Diff", diff, code => {
    for (const text of diff.split("\n")) code.append(el("span", `diff-line ${diffKind(text)}`, text || " "));
  });
  block.classList.add("diff");
  return block;
}

function turn(id: string): Turn {
  const existing = turns.get(id);
  if (existing) return existing;
  const root = el("article", "message cognition");
  root.append(el("div", "byline", "Cognition"));
  const content = el("div", "message-body");
  const details = el("details", "reasoning");
  details.append(el("summary", "", "Reasoning"));
  const reasoning = el("div", "reasoning-body");
  details.append(reasoning);
  details.hidden = true;
  const activity = el("div", "activity");
  root.append(details, content, activity);
  const item = { root, content, reasoning, activity, text: "", thoughts: "", calls: new Map<string, HTMLElement>() };
  turns.set(id, item);
  add(root);
  return item;
}

function toolPath(summary: string): { path: string; line?: number } | undefined {
  // Only file tools get a link. A shell command is shown as plain text: the
  // first path-looking word of `cat -n src/a.rs | …` is not what it operates
  // on in general, and linking it split the row into misleading pieces.
  const match = /^(?:read|edit|write)\s+([\w./\\-]+)(?::(\d+))?$/i.exec(summary);
  return match ? { path: match[1], line: match[2] ? Number(match[2]) : undefined } : undefined;
}

function activity(item: Turn, event: Extract<HostToWebview, { type: "tool" }>): void {
  let row = item.calls.get(event.callId);
  if (!row) {
    row = el("div", "tool-row");
    item.calls.set(event.callId, row);
    item.activity.append(row);
  }
  row.replaceChildren();
  row.className = `tool-row ${event.phase === "start" ? "working" : event.ok === false ? "failed" : "complete"}`;
  row.append(el("span", "tool-mark", event.phase === "start" ? "◌" : event.ok === false ? "×" : "✓"));
  const summary = event.summary || event.name;
  const target = toolPath(summary);
  if (target) {
    const at = summary.indexOf(target.path);
    // A span with preserved whitespace: a bare text node in the flex row
    // lost its trailing space and glued "read" to the path.
    row.append(el("span", "tool-verb", summary.slice(0, at)));
    const link = el("button", "file-link", target.path + (target.line ? `:${target.line}` : ""));
    link.type = "button";
    link.addEventListener("click", () => post({ type: "openFile", ...target }));
    row.append(link);
  } else {
    row.append(el("span", "tool-text", summary));
  }
  row.title = summary;
  row.setAttribute("aria-label", `${event.phase === "start" ? "Running" : event.ok === false ? "Failed" : "Finished"}: ${summary}`);
  if (event.evidence) row.append(evidenceDetails(event.evidence));
  follow();
}

function evidenceDetails(evidence: PermissionEvidence, context: "tool" | "permission" = "tool"): HTMLElement {
  const details = el("details", "approval-evidence");
  const label = evidenceLabel(evidence, context);
  details.append(el("summary", "", label));
  details.setAttribute("aria-label", `Permission review: ${label}`);
  const body = el("div", "evidence-body");
  if (evidence.reason) body.append(el("p", "", evidence.reason));
  body.append(el("div", "", `Policy version ${evidence.policy_version}`));
  for (const key of ["verdicts", "overrides", "models", "latency_ms", "truncated"] as const) {
    if (evidence[key] === undefined) continue;
    const value = typeof evidence[key] === "object" ? JSON.stringify(evidence[key], null, 2) : String(evidence[key]);
    body.append(el("div", "evidence-field", `${key}: ${value}`));
  }
  details.append(body);
  return details;
}

function actionButton(label: string, action: () => void, primary = false): HTMLButtonElement {
  const button = el("button", primary ? "control-primary" : "control-secondary", label);
  button.type = "button";
  button.addEventListener("click", action);
  return button;
}

function renderPermission(event: Extract<HostToWebview, { type: "permission" }>): void {
  const post = identity.bind(rawPost);
  const card = event.card;
  const existing = permissionCards.get(card.requestId);
  if (existing) {
    existing.call.replaceChildren(codeBlock("Call to review", card.call, code => { code.textContent = card.call; }));
    follow();
    return;
  }
  const root = el("article", "control-card permission-card");
  root.setAttribute("aria-label", `Permission: ${card.agent} wants to run ${card.tool}`);
  root.append(el("div", "control-kicker", "Permission needed"), el("h3", "control-title", `${card.agent} wants to run ${card.tool}`));
  if (card.childSessionId) root.append(el("div", "control-meta", `Subagent · ${card.childSessionId}`));
  root.append(el("p", "control-meta", `${card.scope.kind}: ${card.scope.value}${card.tool === "bash" ? " · working directory, not confinement" : ""}`));
  root.append(el("p", "permission-reason", card.reason));
  const call = el("div", "permission-call");
  call.append(codeBlock("Call to review", card.call, code => { code.textContent = card.call; }));
  root.append(call);
  if (card.evidence) root.append(evidenceDetails(card.evidence, "permission"));
  const controls = el("div", "permission-controls");
  const denyMessage = el("textarea", "deny-message");
  denyMessage.rows = 2;
  denyMessage.placeholder = "Deny message (optional)";
  denyMessage.setAttribute("aria-label", `Optional deny message for ${card.tool}`);
  const buttons = el("div", "control-actions");
  const answer = (decision: PermissionDecision): void => {
    if (!sessionId) return;
    for (const button of buttons.querySelectorAll("button")) button.disabled = true;
    denyMessage.disabled = true;
    outcome.textContent = "Sending answer…";
    post({ type: "answerPermission", sessionId, requestId: card.requestId, decision,
      ...(decision === "deny" && denyMessage.value.trim() ? { message: denyMessage.value.trim() } : {}) });
  };
  buttons.append(actionButton("Allow once", () => answer("allow_once"), true));
  if (card.allowFolder) buttons.append(actionButton(card.scope.kind === "origin" ? "Allow site" : "Allow folder", () => answer("allow_folder")));
  buttons.append(actionButton("Deny", () => answer("deny")));
  controls.append(denyMessage, buttons);
  const outcome = el("div", "control-outcome", "Waiting for your answer");
  outcome.setAttribute("role", "status");
  root.append(controls, outcome);
  permissionCards.set(card.requestId, { root, controls, outcome, call });
  add(root);
}

function renderPause(event: Extract<HostToWebview, { type: "pause" }>): void {
  const post = identity.bind(rawPost);
  if (pauseCards.has(event.id)) return;
  const root = el("article", "control-card pause-card");
  root.append(el("div", "control-kicker", event.kind === "budget" ? "Step limit reached" : "Watchdog pause"),
    el("h3", "control-title", event.text));
  const controls = el("div", "control-actions");
  const outcome = el("div", "control-outcome");
  outcome.setAttribute("role", "status");
  const control = (action: "continue" | "stop"): void => {
    if (!sessionId) return;
    for (const button of controls.querySelectorAll("button")) button.disabled = true;
    outcome.textContent = action === "continue" ? "Continuing…" : "Stopping…";
    post({ type: "controlPause", sessionId, id: event.id, action });
  };
  const resume = actionButton("Continue", () => control("continue"), true);
  if (event.kind === "watchdog" && !event.token) {
    resume.disabled = true;
    outcome.textContent = "Pause token unavailable. Send a new message or stop instead.";
  }
  controls.append(resume);
  if (event.kind === "watchdog") controls.append(actionButton("Stop", () => control("stop")));
  root.append(controls, outcome);
  pauseCards.set(event.id, { controls, outcome });
  add(root);
  setStatus("ready");
}

function receive(event: HostToWebview): void {
  if (!identity.receive(event)) return;
  if (event.type === "conversationOpened") {
    sessionId = event.id ?? "";
    sending = false;
    lastConsumedDraft = undefined;
    closePalette();
    commandError.hidden = true;
    agentLabel.textContent = "build";
    pendingModel.textContent = "";
  }
  if (conversations(event)) return;
  switch (event.type) {
    case "commands": commands = event.items; filterPalette(); break;
    case "commandError": commandError.textContent = event.text; commandError.hidden = false; sending = false; updateControls(); break;
    case "agent": agentLabel.textContent = event.name; break;
    case "modelPending": pendingModel.textContent = event.alias ? `→ ${event.alias} · pending after turn` : ""; break;
    case "commandPicker":
      sending = false;
      if (event.disabledReason) { commandError.textContent = event.disabledReason; commandError.hidden = false; closePalette(); }
      else { pickerKind = event.kind; paletteRows = event.items; paletteIndex = 0; renderPalette(); prompt.focus(); }
      updateControls();
      break;
    case "history":
      if (!event.items.length) usage.hidden = true;
      transcript.replaceChildren(empty);
      empty.hidden = false;
      turns.clear();
      permissionCards.clear();
      pauseCards.clear();
      pendingQuestion = false;
      stickToBottom = true;
      replaying = true;
      try { for (const item of event.items) if (item.type !== "history") receive(item); }
      finally { replaying = false; }
      if (state === "connecting") setStatus("ready");
      follow();
      break;
    case "status": setStatus(event.state, event.message); break;
    case "message": message(event.author, event.text, event.context, event.selectedSkill, event.source); break;
    case "sendResult":
      sending = false;
      if (event.accepted && prompt.value === event.text) { lastConsumedDraft = event.text; prompt.value = ""; }
      if (!event.accepted && event.restoreDraft && !prompt.value && lastConsumedDraft === event.text) {
        prompt.value = event.text;
        lastConsumedDraft = undefined;
      }
      updateControls();
      break;
    case "permission": renderPermission(event); break;
    case "permissionCall": {
      const card = permissionCards.get(event.requestId);
      if (card) card.call.replaceChildren(codeBlock("Call to review", event.call, code => { code.textContent = event.call; }));
      follow();
      break;
    }
    case "permissionState": {
      const card = permissionCards.get(event.requestId);
      if (!card) break;
      card.outcome.textContent = permissionOutcome(event.resolution, event.message);
      card.root.dataset.resolution = event.resolution;
      const review = card.root.querySelector(".approval-evidence summary");
    if (review?.textContent === "Driver review required" && ["allow_once", "allow_folder"].includes(event.resolution)) review.textContent = "approved · driver";
      card.controls.hidden = !["pending", "error"].includes(event.resolution);
      for (const control of card.controls.querySelectorAll<HTMLButtonElement | HTMLTextAreaElement>("button, textarea")) control.disabled = event.resolution === "pending";
      follow();
      break;
    }
    case "pause": renderPause(event); break;
    case "pauseState": {
      const card = pauseCards.get(event.id);
      if (!card) break;
      card.outcome.textContent = event.message ?? (event.state === "pending" ? "Waiting for the server…" : "No longer paused");
      card.controls.hidden = event.state === "resolved";
      for (const button of card.controls.querySelectorAll("button")) button.disabled = event.state === "pending";
      follow();
      break;
    }
    case "notice": {
      const notice = el("article", "message control-notice", event.text);
      if (event.operationId) { notice.dataset.operationId = event.operationId; notice.title = `Operation: ${event.operationId}`; }
      if (event.ok === false) notice.classList.add("command-error");
      add(notice);
      break;
    }
    case "models": renderModels(event); break;
    case "usage": renderUsage(event.snapshot); break;
    case "editorContext":
      contextPreview.textContent = event.label;
      contextPreview.title = `Attached to your next message: ${event.label}`;
      break;
    case "turnStart": turn(event.id); setStatus("busy"); break;
    case "delta": {
      const item = turn(event.id);
      item.text += event.text;
      renderMarkdown(item.content, item.text);
      follow();
      break;
    }
    case "reasoning": {
      const item = turn(event.id);
      item.thoughts += event.text;
      item.root.querySelector("details")!.hidden = false;
      item.reasoning.textContent = item.thoughts;
      follow();
      break;
    }
    case "tool": activity(turn(event.id), event); break;
    case "question": {
      const item = turn(event.id);
      const note = el("div", "question");
      note.append(el("span", "question-label", "Question"));
      const body = el("div", "message-body");
      renderMarkdown(body, event.text);
      note.append(body);
      item.root.append(note);
      pendingQuestion = true;
      setStatus("ready");
      follow();
      break;
    }
    case "turnEnd": {
      const item = turn(event.id);
      if (event.outcome !== "done") item.root.append(el("div", "turn-result", event.error || (event.outcome === "stopped" ? "Stopped" : "The turn failed.")));
      setStatus(event.outcome === "error" ? "error" : "ready", event.error);
      follow();
      break;
    }
  }
}

/** Rebuilds the model picker; a locked picker shows the open conversation's model. */
function renderModels(event: Extract<HostToWebview, { type: "models" }>): void {
  const options = event.items.map(model => {
    const option = el("option", undefined, model.alias);
    option.value = model.alias;
    option.title = model.contextWindow ? `${model.model} · ${tokens(model.contextWindow)} context` : model.model;
    return option;
  });
  if (event.current && !event.items.some(model => model.alias === event.current)) {
    const option = el("option", undefined, event.current);
    option.value = event.current;
    options.push(option);
  }
  if (!options.length) options.push(el("option", undefined, "Default model"));
  modelSelect.replaceChildren(...options);
  if (event.current) modelSelect.value = event.current;
  modelSelect.disabled = event.locked || event.items.length === 0;
  modelSelect.title = "Switch model · running turns finish before the switch";
}

/** The next request's estimate, against the usable input budget. */
function tokens(count: number): string {
  return count >= 1000 ? `${Math.round(count / 1000)}k` : String(count);
}

function renderUsage(snapshot: import("../contextMeter").ContextSnapshot): void {
  const meter = contextMeter(snapshot);
  usage.hidden = false;
  usageFill.style.width = `${meter.fill}%`;
  usage.dataset.level = meter.level;
  usageLabel.textContent = meter.label;
  usage.setAttribute("aria-valuenow", String(meter.fill));
  usage.setAttribute("aria-valuetext", meter.detail);
  usage.tabIndex = 0;
  usage.setAttribute("aria-label", "Next request context estimate");
  usage.title = meter.detail;
  usage.querySelector<HTMLElement>(".usage-bar")!.hidden = false;
}

// All source text is inserted as text nodes. Only explicitly created elements become markup.
function renderMarkdown(root: HTMLElement, source: string): void {
  root.replaceChildren();
  const lines = source.replace(/\r\n?/g, "\n").split("\n");
  for (let i = 0; i < lines.length;) {
    const line = lines[i];
    if (!line.trim()) { i++; continue; }
    const fence = /^\s{0,3}(`{3,}|~{3,})([^\s`]*)\s*$/.exec(line);
    if (fence) {
      const marker = fence[1];
      const code: string[] = [];
      i++;
      while (i < lines.length && !new RegExp(`^\\s{0,3}${marker[0]}{${marker.length},}\\s*$`).test(lines[i])) code.push(lines[i++]);
      if (i < lines.length) i++;
      const value = code.join("\n");
      root.append(fence[2] === "diff" ? diffBlock(value) : codeBlock(fence[2] || "Code", value, node => { node.textContent = value; }));
      continue;
    }
    const heading = /^\s{0,3}(#{1,4})\s+(.+)$/.exec(line);
    if (heading) {
      const title = el(`h${heading[1].length + 2}` as "h3" | "h4" | "h5" | "h6");
      inline(title, heading[2]);
      root.append(title);
      i++;
      continue;
    }
    const bullet = /^\s{0,3}([-*+] |\d+\. )(.+)$/.exec(line);
    if (bullet) {
      const numbered = /^\d/.test(bullet[1]);
      const list = el(numbered ? "ol" : "ul");
      while (i < lines.length) {
        const next = /^\s{0,3}([-*+] |\d+\. )(.+)$/.exec(lines[i]);
        if (!next || /^\d/.test(next[1]) !== numbered) break;
        const li = el("li");
        inline(li, next[2]);
        list.append(li);
        i++;
      }
      root.append(list);
      continue;
    }
    const paragraph: string[] = [];
    while (i < lines.length && lines[i].trim() && (i === 0 || paragraph.length === 0 || !/^\s{0,3}(?:#{1,4}\s|[-*+] |\d+\. |`{3,}|~{3,})/.test(lines[i]))) paragraph.push(lines[i++]);
    const p = el("p");
    inline(p, paragraph.join("\n"));
    root.append(p);
  }
}

function inline(parent: HTMLElement, source: string): void {
  // Match one inline token at a time; anything else is plain text.
  const token = /(`+)([^`\n]+?)\1|\[([^\]]+)\]\(([^\s)]+)\)|(\*\*|__)(.+?)\5|(\*|_)([^\n]+?)\7/g;
  let at = 0;
  for (const match of source.matchAll(token)) {
    const index = match.index!;
    parent.append(document.createTextNode(source.slice(at, index)));
    if (match[1]) parent.append(el("code", "", match[2]));
    else if (match[3]) {
      let url: URL | undefined;
      try { url = new URL(match[4]); } catch { /* Relative and malformed links stay plain text. */ }
      if (url && ["https:", "http:", "mailto:"].includes(url.protocol)) {
        const a = el("a", "", match[3]);
        a.href = url.href;
        a.rel = "noopener noreferrer";
        parent.append(a);
      } else parent.append(document.createTextNode(match[0]));
    } else {
      const emphasis = el(match[5] ? "strong" : "em");
      inline(emphasis, match[6] || match[8]);
      parent.append(emphasis);
    }
    at = index + match[0].length;
  }
  parent.append(document.createTextNode(source.slice(at)));
}

window.addEventListener("message", event => {
  if (event.data && typeof event.data.type === "string") receive(event.data as HostToWebview);
});
form.addEventListener("submit", event => {
  event.preventDefault();
  const text = prompt.value;
  if (!text.trim() || prompt.disabled || sending) return;
  closePalette();
  commandError.hidden = true;
  sending = true;
  post({ type: "send", text, includeContext: includeContext.checked });
  updateControls();
  stickToBottom = true;
  follow();
});
prompt.addEventListener("input", () => { lastConsumedDraft = undefined; paletteDismissed = false; commandError.hidden = true; filterPalette(); updateControls(); });
function updateContextToggle(): void {
  contextPreview.classList.toggle("off", !includeContext.checked);
}
includeContext.addEventListener("change", () => {
  vscode.setState({ ...vscode.getState(), includeContext: includeContext.checked });
  updateContextToggle();
});
updateContextToggle();
prompt.addEventListener("keydown", event => {
  if (!palette.hidden && !event.isComposing) {
    if (event.key === "Escape") { event.preventDefault(); paletteDismissed = true; pickerKind = undefined; closePalette(); return; }
    if (["ArrowUp", "ArrowDown"].includes(event.key)) {
      event.preventDefault(); paletteIndex = (paletteIndex + (event.key === "ArrowDown" ? 1 : -1) + paletteRows.length) % paletteRows.length; renderPalette(); return;
    }
    if ((event.key === "Enter" && !event.shiftKey) || event.key === "Tab") { event.preventDefault(); choosePalette(paletteIndex); return; }
  }
  if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
    event.preventDefault();
    form.requestSubmit();
  }
});
stop.addEventListener("click", () => post({ type: "stop" }));
post({ type: "ready" });
