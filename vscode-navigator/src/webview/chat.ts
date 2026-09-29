/// <reference lib="dom" />
import type { HostToWebview, WebviewToHost } from "../chatProtocol";

type ViewState = { includeContext?: boolean };
declare function acquireVsCodeApi(): { postMessage(message: WebviewToHost): void; getState(): ViewState | undefined; setState(state: ViewState): void };
const vscode = acquireVsCodeApi();
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
includeContext.checked = vscode.getState()?.includeContext ?? true;
type Turn = { root: HTMLElement; content: HTMLElement; reasoning: HTMLElement; activity: HTMLElement; text: string; thoughts: string; calls: Map<string, HTMLElement> };
const turns = new Map<string, Turn>();
let state = "connecting";
let pendingQuestion = false;
let stickToBottom = true;

function updateControls(): void {
  prompt.disabled = state === "connecting";
  send.disabled = prompt.disabled || !prompt.value.trim();
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

function message(author: string, text: string, context?: string): void {
  const root = el("article", `message ${author}`);
  root.append(el("div", "byline", author === "driver" ? pendingQuestion ? "You · Answer" : "You" : author === "cognition" ? "Cognition" : "Navigator"));
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
  // Paths are only offered for file-related activity; no arbitrary shell arguments.
  const match = /(?:^|\s)(?:(?:read|edit|write|open|search)\s+)?([\w./\\-]+\.[\w-]+)(?::(\d+))?(?=\s|$)/i.exec(summary);
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
    row.append(document.createTextNode(summary.slice(0, at)));
    const link = el("button", "file-link", target.path + (target.line ? `:${target.line}` : ""));
    link.type = "button";
    link.addEventListener("click", () => vscode.postMessage({ type: "openFile", ...target }));
    row.append(link, document.createTextNode(summary.slice(at + link.textContent!.length)));
  } else {
    row.append(document.createTextNode(summary));
  }
  row.setAttribute("aria-label", `${event.phase === "start" ? "Running" : event.ok === false ? "Failed" : "Finished"}: ${summary}`);
  follow();
}

function receive(event: HostToWebview): void {
  switch (event.type) {
    case "history":
      transcript.replaceChildren(empty);
      empty.hidden = false;
      turns.clear();
      pendingQuestion = false;
      stickToBottom = true;
      for (const item of event.items) if (item.type !== "history") receive(item);
      if (state === "connecting") setStatus("ready");
      follow();
      break;
    case "status": setStatus(event.state, event.message); break;
    case "message": message(event.author, event.text, event.context); break;
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
      const wrap = el("div", "code-block");
      const bar = el("div", "code-bar");
      bar.append(el("span", "", fence[2] || "Code"));
      const copy = el("button", "copy", "Copy");
      copy.type = "button";
      const value = code.join("\n");
      copy.addEventListener("click", async () => {
        try { await navigator.clipboard.writeText(value); copy.textContent = "Copied"; }
        catch { copy.textContent = "Copy failed"; }
      });
      bar.append(copy);
      const pre = el("pre");
      pre.append(el("code", "", value));
      wrap.append(bar, pre);
      root.append(wrap);
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
  const text = prompt.value.trim();
  if (!text || prompt.disabled) return;
  vscode.postMessage({ type: "send", text, includeContext: includeContext.checked });
  prompt.value = "";
  updateControls();
  stickToBottom = true;
  follow();
});
prompt.addEventListener("input", updateControls);
function updateContextToggle(): void {
  contextPreview.classList.toggle("off", !includeContext.checked);
}
includeContext.addEventListener("change", () => {
  vscode.setState({ ...vscode.getState(), includeContext: includeContext.checked });
  updateContextToggle();
});
updateContextToggle();
prompt.addEventListener("keydown", event => {
  if (event.key === "Enter" && !event.shiftKey && !event.isComposing) {
    event.preventDefault();
    form.requestSubmit();
  }
});
stop.addEventListener("click", () => vscode.postMessage({ type: "stop" }));
vscode.postMessage({ type: "ready" });
