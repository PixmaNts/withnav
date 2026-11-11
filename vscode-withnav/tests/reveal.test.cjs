const { test } = require("node:test");
const assert = require("node:assert/strict");
const Module = require("node:module");
const originalLoad = Module._load;
const groups = { all: [], activeTabGroup: undefined };
const fs = require("node:fs");
const os = require("node:os");
const path = require("node:path");

class Position {
  constructor(line, character) { this.line = line; this.character = character; }
  translate(lines, characters) { return new Position(this.line + lines, this.character + characters); }
}
class Range {
  constructor(a, b, c, d) {
    if (a instanceof Position) { this.start = a; this.end = b; } else { this.start = new Position(a, b); this.end = new Position(c, d); }
  }
}
class Selection extends Range {}
const uriFile = fsPath => ({ scheme: "file", fsPath, path: fsPath });
const opened = [];
const shown = [];
const ws = { folders: [], beforeOpen: () => {} };
const vscode = {
  ViewColumn: { Active: -1, Beside: -2, One: 1 },
  FileType: { File: 1, Directory: 2 },
  TextEditorRevealType: { InCenter: 2 },
  Position, Range, Selection,
  Uri: { file: uriFile, joinPath: (base, rel) => uriFile(path.join(base.fsPath, rel)) },
  commands: { executeCommand: async () => [] },
  workspace: {
    get workspaceFolders() { return ws.folders; },
    getWorkspaceFolder: uri => ws.folders.find(folder => !path.relative(folder.uri.fsPath, uri.fsPath).startsWith("..")),
    fs: { stat: async uri => ({ type: fs.statSync(uri.fsPath).isFile() ? 1 : 2 }) },
    openTextDocument: async uri => {
      ws.beforeOpen();
      const text = fs.readFileSync(uri.fsPath, "utf8");
      const lines = text.split("\n");
      opened.push({ uri, text });
      const positionAt = offset => {
        const before = text.slice(0, offset).split("\n");
        return new Position(before.length - 1, before[before.length - 1].length);
      };
      return { uri, lineCount: lines.length, getText: () => text, positionAt };
    },
  },
  window: {
    tabGroups: groups,
    showTextDocument: async document => {
      const editor = { document, viewColumn: 1, selection: undefined, revealRange() {} };
      shown.push(editor);
      return editor;
    },
  },
};
Module._load = function (id, ...args) {
  if (id === "vscode") return vscode;
  return originalLoad.call(this, id, ...args);
};
const { targetColumn, reveal } = require("../out/tests/reveal.js");

function layout(columns, active) {
  groups.all = columns.map(viewColumn => ({ viewColumn }));
  groups.activeTabGroup = { viewColumn: active };
}

test("active keeps the current behavior", () => {
  layout([1, 2], 1);
  assert.equal(targetColumn("active"), vscode.ViewColumn.Active);
});

test("neighbor reuses the next group, else the previous one", () => {
  layout([1, 2, 3], 2);
  assert.equal(targetColumn("neighbor"), 3);
  layout([1, 2], 2);
  assert.equal(targetColumn("neighbor"), 1);
});

test("neighbor without another group is an explicit error", () => {
  layout([1], 1);
  assert.match(String(targetColumn("neighbor")), /no editor group/);
});

test("beside reuses a neighbor and creates one only when there is none", () => {
  layout([1, 2], 1);
  assert.equal(targetColumn("beside"), 2);
  layout([1], 1);
  assert.equal(targetColumn("beside"), vscode.ViewColumn.Beside);
});

test("an unknown placement is an explicit error", () => {
  layout([1], 1);
  assert.match(String(targetColumn("left")), /unknown placement/);
});

/** A workspace folder holding `a.c` (10 lines) and a file outside it. */
function workspace() {
  const base = fs.realpathSync(fs.mkdtempSync(path.join(os.tmpdir(), "withnav-reveal-")));
  const root = path.join(base, "ws");
  fs.mkdirSync(root);
  fs.writeFileSync(path.join(root, "a.c"), Array.from({ length: 9 }, (_, i) => `line ${i + 1}`).join("\n") + "\nline 10");
  fs.writeFileSync(path.join(base, "outside.c"), "outside\n");
  ws.folders = [{ uri: uriFile(root) }];
  ws.beforeOpen = () => {};
  opened.length = 0;
  shown.length = 0;
  return { base, root };
}

test("reveal opens the checked path, not a link retargeted after the check", async () => {
  const { base, root } = workspace();
  const link = path.join(root, "link.c");
  fs.symlinkSync(path.join(root, "a.c"), link);
  ws.beforeOpen = () => { fs.unlinkSync(link); fs.symlinkSync(path.join(base, "outside.c"), link); };
  const result = await reveal({ path: "link.c", placement: "active" });
  assert.equal(result.error, undefined);
  // The opened path is the resolved one: compare resolved paths, since a temporary folder can be
  // named by its short form (Windows `RUNNER~1`) before resolution.
  assert.equal(opened[0].uri.fsPath, fs.realpathSync.native(path.join(root, "a.c")));
  assert.doesNotMatch(opened[0].text, /outside/);
});

test("reveal refuses a line past the end without opening a tab", async () => {
  workspace();
  const result = await reveal({ path: "a.c", line: 99999, placement: "active" });
  assert.match(result.error, /line 99999 is past the end of a\.c \(10 lines\)/);
  assert.equal(shown.length, 0);
  const last = await reveal({ path: "a.c", line: 10, placement: "active" });
  assert.equal(last.error, undefined);
  assert.equal(shown[0].selection.start.line, 9);
});

test("reveal keeps an out-of-range line as a hint when the symbol is found", async () => {
  workspace();
  const result = await reveal({ path: "a.c", symbol: "line", line: 99999, placement: "active" });
  assert.equal(result.error, undefined);
  assert.equal(result.symbol_found, true);
  assert.equal(shown[0].selection.start.line, 9);
});

test("reveal puts the cursor on line 1 when the symbol is missing and no line is given", async () => {
  workspace();
  const result = await reveal({ path: "a.c", symbol: "missing", placement: "active" });
  assert.equal(result.symbol_found, false);
  assert.equal(shown[0].selection.start.line, 0);
  assert.equal(shown[0].selection.start.character, 0);
});
