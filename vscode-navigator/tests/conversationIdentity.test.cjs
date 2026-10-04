const { test } = require("node:test");
const assert = require("node:assert/strict");
const { ConversationIdentity } = require("../out/tests/webview/conversationIdentity.js");

test("old pause callbacks cannot control B between identity and history delivery", () => {
  let disabled = 0;
  const posts = [];
  const identity = new ConversationIdentity(() => disabled++);
  identity.receive({ type: "conversationOpened", id: "A", generation: 1 });
  const oldCard = identity.bind(message => posts.push(message));
  const before = disabled;
  identity.receive({ type: "conversationOpened", id: "B", generation: 2 });
  assert.ok(disabled > before);
  for (const action of ["continue", "stop"]) oldCard({ type: "controlPause", sessionId: "B", id: "pause:10", action });
  assert.equal(posts.length, 0);
  assert.equal(identity.receive({ type: "history", generation: 1, items: [] }), false);
  identity.bind(message => posts.push(message))({ type: "controlPause", sessionId: "wrong", id: "pause:10", action: "continue" });
  assert.deepEqual(posts[0], { type: "controlPause", sessionId: "B", generation: 2, id: "pause:10", action: "continue" });
});
