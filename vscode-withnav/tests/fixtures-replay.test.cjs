const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const Module = require("node:module");

const originalLoad = Module._load;
Module._load = function (id, ...args) {
  if (id === "vscode") return { workspace: { workspaceFolders: [] } };
  return originalLoad.call(this, id, ...args);
};
const { CognitionBackend } = require("../out/tests/cognition.js");
Module._load = originalLoad;

function loadFixture(name) {
  const filePath = path.join(__dirname, "../../tests/fixtures/events", name);
  return fs.readFileSync(filePath, "utf8");
}

function parseSseEnvelopes(raw) {
  const blocks = raw.split(/\r?\n\r?\n/);
  const envelopes = [];
  for (const block of blocks) {
    if (!block.trim()) continue;
    const data = [];
    for (const line of block.split(/\r?\n/)) {
      if (line.startsWith("data:")) {
        data.push(line.slice(5).trimStart());
      }
    }
    if (data.length > 0) {
      envelopes.push(JSON.parse(data.join("\n")));
    }
  }
  return envelopes;
}

function replayFixture(raw) {
  const backend = new CognitionBackend();
  backend.endpoint = { port: 1234, token: "secret" };
  const emitted = [];
  backend.subscribe((msg) => emitted.push(msg));

  const envelopes = parseSseEnvelopes(raw);
  if (envelopes.length > 0 && envelopes[0].session_id) {
    backend.state.session = envelopes[0].session_id;
  }

  let buffer = raw;
  let split;
  while ((split = buffer.indexOf("\n\n")) !== -1) {
    const frameRaw = buffer.slice(0, split);
    buffer = buffer.slice(split + 2);
    backend.handleFrame(backend.parseFrame(frameRaw));
  }
  if (buffer.trim()) {
    backend.handleFrame(backend.parseFrame(buffer));
  }

  return { backend, emitted, envelopes };
}

test("replays write-file fixture through CognitionBackend", () => {
  const raw = loadFixture("write-file.sse");
  const { backend, emitted } = replayFixture(raw);

  const captainMessages = emitted.filter((m) => m.type === "message" && m.author === "captain");
  assert.equal(captainMessages.length, 1);
  assert.ok(captainMessages[0].text.includes("hello.txt"));

  const tools = emitted.filter((m) => m.type === "tool" && m.phase === "end");
  assert.equal(tools.length, 2);
  assert.equal(tools[0].name, "glob");
  assert.equal(tools[0].ok, true);
  assert.equal(tools[0].summary, "glob");

  assert.equal(tools[1].name, "write");
  assert.equal(tools[1].ok, true);
  assert.equal(tools[1].summary, "write hello.txt");
  assert.ok(tools[1].evidence);

  const textReplacement = emitted.find((m) => m.type === "text");
  assert.ok(textReplacement, "must emit a replacement text message for final response");
  assert.ok(textReplacement.text.includes("hello.txt"));

  const turnEnds = emitted.filter((m) => m.type === "turnEnd");
  assert.equal(turnEnds.length, 1);
  assert.equal(turnEnds[0].outcome, "done");

  backend.dispose();
});

test("replays permission-denied fixture through CognitionBackend", () => {
  const raw = loadFixture("permission-denied.sse");
  const { backend, emitted } = replayFixture(raw);

  const permissions = emitted.filter((m) => m.type === "permission");
  assert.equal(permissions.length, 1);
  assert.equal(permissions[0].card.tool, "bash");

  const permissionResolutions = emitted.filter((m) => m.type === "permissionState");
  assert.equal(permissionResolutions.length, 1);
  assert.equal(permissionResolutions[0].resolution, "deny");

  const editTool = emitted.find((m) => m.type === "tool" && m.name === "edit" && m.phase === "end");
  assert.ok(editTool);
  assert.equal(editTool.ok, true);

  const turnEnds = emitted.filter((m) => m.type === "turnEnd");
  assert.equal(turnEnds.length, 2);
  assert.equal(turnEnds[0].outcome, "done");
  assert.equal(turnEnds[1].outcome, "done");

  backend.dispose();
});

test("replays reconnect-gap fixture through CognitionBackend", () => {
  const raw = loadFixture("reconnect-gap.sse");
  const { backend, emitted } = replayFixture(raw);

  const textReplacement = emitted.find((m) => m.type === "text");
  assert.ok(textReplacement, "reconnection gap must emit a replacement text message");
  assert.equal(textReplacement.text, "Paris is the capital of France and its largest city.");

  const turnEnds = emitted.filter((m) => m.type === "turnEnd");
  assert.equal(turnEnds.length, 1);
  assert.equal(turnEnds[0].outcome, "done");

  backend.dispose();
});

test("replays session-end-interrupted fixture through CognitionBackend", () => {
  const raw = loadFixture("session-end-interrupted.sse");
  const { backend, emitted } = replayFixture(raw);

  const toolStarts = emitted.filter((m) => m.type === "tool" && m.phase === "start");
  assert.equal(toolStarts.length, 1);
  assert.equal(toolStarts[0].name, "bash");

  // In the editor extension, session.end updates bypass/compaction state but
  // leaves active tool calls as-is until a reconnect or conversation switch.
  const toolEnds = emitted.filter((m) => m.type === "tool" && m.phase === "end");
  assert.equal(toolEnds.length, 0);

  backend.dispose();
});

test("replays turn-error fixture through CognitionBackend", () => {
  const raw = loadFixture("turn-error.sse");
  const { backend, emitted } = replayFixture(raw);

  const turnEnds = emitted.filter((m) => m.type === "turnEnd");
  assert.equal(turnEnds.length, 1);
  assert.equal(turnEnds[0].outcome, "error");
  assert.equal(turnEnds[0].error, "Prompt exceeds maximum context length");

  const statuses = emitted.filter((m) => m.type === "status");
  assert.ok(statuses.some((s) => s.state === "error"));

  backend.dispose();
});
