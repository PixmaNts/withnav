const { test } = require("node:test");
const assert = require("node:assert/strict");
const Module = require("node:module");
const originalLoad = Module._load;
const groups = { all: [], activeTabGroup: undefined };
const vscode = {
  ViewColumn: { Active: -1, Beside: -2, One: 1 },
  window: { tabGroups: groups },
};
Module._load = function (id, ...args) {
  if (id === "vscode") return vscode;
  return originalLoad.call(this, id, ...args);
};
const { targetColumn } = require("../out/tests/reveal.js");

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
