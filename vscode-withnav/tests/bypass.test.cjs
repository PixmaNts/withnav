const { test } = require('node:test');
const assert = require('node:assert/strict');
const Module = require('node:module');
const load = Module._load;
let confirmations = 0;
Module._load = function(id, ...args) {
  if (id === 'vscode') return { workspace: { workspaceFolders: [] }, window: { showWarningMessage: async () => { confirmations++; return 'Enable bypass'; } } };
  return load.call(this, id, ...args);
};
const { CognitionBackend } = require('../out/tests/cognition.js');
Module._load = load;
function setup() {
  const b = new CognitionBackend(); b.state.session = 'S';
  b.reconcileSession = async () => ({ bypass: b.state.bypass, bypass_seq: b.state.bypassSeq, incarnation: b.state.incarnation });
  b.acceptBypassSnapshot({ bypass: false, bypass_seq: 10, incarnation: 'runtime' });
  return b;
}
test('confirmation once per session incarnation is shared by typed and header toggles', async () => {
  confirmations = 0; const b = setup();
  b.request = async (_path, init) => ({ enabled: JSON.parse(init.body).enabled, bypass_seq: b.state.bypassSeq + 1 });
  await b.runComposerCommand('/bypass on'); await b.toggleBypassPermissions(); await b.toggleBypassPermissions();
  assert.equal(confirmations, 1);
  b.state.incarnation = 'next'; await b.toggleBypassPermissions(); await b.toggleBypassPermissions();
  assert.equal(confirmations, 2);
});
test('snapshot watermark rejects replay ON, newer events apply, end clears and notices remain', () => {
  const b = setup();
  const old = b.mapEvent({ kind: 'session.bypass', seq: 9, payload: { enabled: true } });
  assert.equal(b.state.bypass, false); assert.match(old.text, /Bypass ON/);
  b.mapEvent({ kind: 'session.bypass', seq: 11, payload: { enabled: true } });
  assert.equal(b.state.bypass, true);
  b.acceptBypassSnapshot({ bypass: false, bypass_seq: 10, incarnation: 'runtime' });
  assert.equal(b.state.bypass, true);
  b.mapEvent({ kind: 'session.end', seq: 12, payload: {} });
  b.mapEvent({ kind: 'session.bypass', seq: 11, payload: { enabled: true } });
  assert.equal(b.state.bypass, false);
});
test('pending setter never optimistically enables and rejects repeat submission', async () => {
  const b = setup(); let finish;
  b.request = () => new Promise(resolve => { finish = resolve; });
  const changing = b.toggleBypassPermissions();
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(b.state.bypass, false); assert.equal(b.state.bypassPending, true);
  await assert.rejects(b.toggleBypassPermissions(), /already pending/);
  finish({ enabled: true, bypass_seq: 11 }); await changing;
  assert.equal(b.state.bypass, true); assert.equal(b.state.bypassPending, false);
});
test('child and archive views show state but cannot set it', async () => {
  const b = setup(); b.state.readOnly = true; b.state.bypass = true;
  let view; b.subscribe(m => { view = m; }); b.publishBypass();
  assert.equal(view.enabled, true); assert.equal(view.disabled, true);
  await assert.rejects(b.toggleBypassPermissions(), /uwithnavlable/);
});
test('events arriving before the snapshot stay dark until reconciliation', () => {
  const b = new CognitionBackend();
  b.mapEvent({ kind: 'session.bypass', seq: 9, payload: { enabled: true } });
  assert.equal(b.state.bypass, false);
  b.acceptBypassSnapshot({ bypass: false, bypass_seq: 10, incarnation: 'runtime' });
  assert.equal(b.state.bypass, false);
  const live = new CognitionBackend();
  live.mapEvent({ kind: 'session.bypass', seq: 11, payload: { enabled: true } });
  live.acceptBypassSnapshot({ bypass: false, bypass_seq: 10, incarnation: 'runtime' });
  assert.equal(live.state.bypass, true);
});
