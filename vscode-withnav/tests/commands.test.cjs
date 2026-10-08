const { test } = require("node:test");
const assert = require("node:assert/strict");
const { routeComposer, commandRows } = require("../out/tests/commands.js");
test("first-column routing and literal slash escape", () => {
  assert.deepEqual(routeComposer("/x hello"), { kind: "command", name: "x", arguments: "hello" });
  assert.deepEqual(routeComposer("//x"), { kind: "message", text: "/x" });
  assert.deepEqual(routeComposer(" //x"), { kind: "message", text: " //x" });
  assert.deepEqual(routeComposer("//skill review"), { kind: "message", text: "/skill review" });
});
test("palette filters by name and closes for arguments or escapes", () => {
  const rows = [{ name: "model" }, { name: "compact" }];
  assert.equal(commandRows(rows, "/").length, 2);
  assert.equal(commandRows(rows, "/mo")[0].name, "model");
  assert.equal(commandRows(rows, "/model ").length, 0);
  assert.equal(commandRows(rows, "//").length, 0);
});
