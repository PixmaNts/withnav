// Parses a navigator wake (a `message.received` whose source is `navigator`)
// into the parts the transcript shows. Same rules as the VS Code chat
// (`vscode-navigator/src/webview/wake.ts`), which is the reference.
//
// Current shape:
//   Navigator noticed: <reason> (<score>) in <path>:<line>
//   ```diff ... ```
//   Intent on record:
//   <intent>
// Legacy shape, still in stored histories:
//   Navigator wake: <sentence>
//   File: <uri>
//   What changed:
//   <unfenced diff>
//   Intent on record:
//   <intent>
//   Recent conversation:
//   <digest, dropped: the driver already sees it>

export interface WakeNote {
  reason: string;
  score?: string;
  path?: string;
  line?: number;
  lead?: string;
  diff?: string;
  intent?: string;
  rest: string;
}

const LEGACY_REASON = "an edit at odds with the intent or the conversation";
const SECTIONS = ["rest", "diff", "intent", "drop"] as const;
type Section = (typeof SECTIONS)[number];

export function parseWake(source: string): WakeNote {
  const lines = source.replace(/\r\n?/g, "\n").split("\n");
  const note: WakeNote = { reason: "", rest: "" };
  let shape: "current" | "legacy" | "other" = "other";
  let i = 0;
  const header = /^\s*Navigator noticed:\s*(.*?)(?:\s+in\s+(\S+?)(?::(\d+))?)?\s*$/.exec(lines[0] ?? "");
  if (header) {
    shape = "current";
    note.reason = header[1] ?? "";
    if (header[2] && (header[3] || /[./\\]/.test(header[2]))) {
      note.path = toPath(header[2]);
      if (header[3]) note.line = Number(header[3]);
    } else if (header[2]) note.reason = `${header[1]} in ${header[2]}`;
    i = 1;
  } else if (/^\s*Navigator wake:/.test(lines[0] ?? "")) {
    shape = "legacy";
    note.reason = LEGACY_REASON;
    const lead: string[] = [];
    while (i < lines.length && lines[i]!.trim()) lead.push(lines[i++]!);
    const sentence = lead.join(" ").replace(/^\s*Navigator wake:\s*/, "").trim();
    note.lead = sentence.charAt(0).toUpperCase() + sentence.slice(1);
  } else {
    while (i < lines.length && !lines[i]!.trim()) i++;
    note.reason = (lines[i++] ?? "").trim();
  }

  const buckets: Record<Section, string[]> = { rest: [], diff: [], intent: [], drop: [] };
  let section = "rest" as Section;
  const enter = (next: Section): boolean => {
    if (SECTIONS.indexOf(next) <= SECTIONS.indexOf(section)) return false;
    section = next;
    return true;
  };
  for (; i < lines.length; i++) {
    const line = lines[i]!;
    if (section === "drop") break;
    const fence = /^\s{0,3}(`{3,}|~{3,})([^\s`]*)\s*$/.exec(line);
    if (fence && note.diff === undefined && (section === "rest" || section === "diff")) {
      const code: string[] = [];
      const marker = fence[1]!;
      const close = new RegExp(`^\\s{0,3}${marker[0]}{${marker.length},}\\s*$`);
      for (i++; i < lines.length && !close.test(lines[i]!); i++) code.push(lines[i]!);
      note.diff = code.join("\n");
      continue;
    }
    if (shape === "legacy" && section === "rest" && note.path === undefined) {
      const file = /^File:\s*(\S.*)$/.exec(line);
      if (file) {
        note.path = toPath(file[1]!.trim());
        continue;
      }
    }
    if (/^What changed:\s*$/.test(line) && enter("diff")) continue;
    if (/^Intent on record:\s*$/.test(line) && enter("intent")) continue;
    if (/^Recent conversation:\s*$/.test(line) && enter("drop")) continue;
    buckets[section].push(line);
  }
  const unfenced = trimBlank(buckets.diff);
  if (note.diff === undefined && unfenced) note.diff = unfenced;
  else if (unfenced) buckets.rest.push(unfenced);
  if (note.diff !== undefined && (!note.diff.trim() || /^\(no diff captured\)$/.test(note.diff.trim()))) note.diff = undefined;
  note.intent = trimBlank(buckets.intent) || undefined;
  note.rest = trimBlank(buckets.rest);
  const score = /^(.*?)\s*\((\d*\.?\d+)\)$/.exec(note.reason);
  if (score && score[1]) {
    note.reason = score[1];
    note.score = score[2];
  }
  return note;
}

function trimBlank(lines: string[]): string {
  return lines.join("\n").replace(/^\s*\n/, "").trimEnd();
}

function toPath(value: string): string {
  if (!/^file:\/\//i.test(value)) return value;
  try {
    const path = decodeURIComponent(new URL(value).pathname);
    return /^\/[A-Za-z]:\//.test(path) ? path.slice(1) : path;
  } catch {
    return value;
  }
}
