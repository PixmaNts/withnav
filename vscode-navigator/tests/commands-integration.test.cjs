const { test } = require('node:test');
const assert = require('node:assert/strict');
const Module = require('node:module');
const load = Module._load;
Module._load = function(id, ...args) { return id === 'vscode' ? { workspace: { workspaceFolders: [] } } : load.call(this, id, ...args); };
const { CognitionBackend } = require('../out/tests/cognition.js');
Module._load = load;
function backend() {
  const b = new CognitionBackend();
  b.endpoint = { port: 1234, token: 't' }; b.state.session = 'S';
  b.commands = ['model','variant','context','agent'].map(name => ({ name, description: name, args_hint: '' }));
  b.connect = async () => {}; b.listConversations = async () => [];
  const messages = []; b.subscribe(m => messages.push(m));
  let seq = 0;
  const event = (kind, payload) => b.handleFrame({ id: String(++seq), event: '', envelope: { seq, kind, payload } });
  return { b, messages, event };
}
test('R11 model and compaction retain notices and refresh snapshot context before returning', async () => {
  const { b, messages, event } = backend();
  let refreshed = 0;
  b.fetchContext = async () => { refreshed++; };
  event('model.changed', { operation_id: 'M', new: { agent: "build", source: "override", model: 'next' } });
  assert.equal(refreshed, 1);
  assert.equal(b.state.selectedModel, 'next');
  assert.match(messages.at(-1).text, /Model: next/);
  event('context.compacted', { estimate_before: 200, estimate_after: 100 });
  assert.equal(refreshed, 2);
  assert.match(messages.at(-1).text, /Context compacted/);
  event('llm.response', { message: {}, usage: { prompt_tokens: 999, completion_tokens: 20 } });
  assert.equal(messages.some(m => m.type === 'usage'), false);
});
for (const early of [false, true]) test(`R2 skill admission followed by refusal ${early ? 'before' : 'after'} HTTP preserves correlation`, async t => {
  const { b, messages, event } = backend();
  let finish; t.mock.method(globalThis, 'fetch', () => new Promise(resolve => finish = resolve));
  const pending = b.runComposerCommand('/skill review check');
  await Promise.resolve(); await Promise.resolve();
  const admission = () => event('message.received', { command_id: 'SK', source: 'user:http', content: 'check' });
  const refusal = () => event('command.result', { operation_id: 'SK', name: 'message', ok: false, error: 'disabled' });
  if (early) { admission(); refusal(); }
  finish(new Response(JSON.stringify({ command_id: 'SK', accepted: true })));
  await pending;
  if (!early) { admission(); refusal(); }
  const results = messages.filter(m => m.type === 'sendResult');
  assert.equal(results.at(-1).accepted, false);
  assert.equal(results.at(-1).restoreDraft, true);
  if (early) assert.equal(results.some(m => m.accepted), false);
});
test('R3 watchdog command result removes host pause authority', async () => {
  const { b, event } = backend();
  const token = { incarnation: 'I', hold_id: 'H' };
  event('agent.yield', { kind: 'watchdog', question: 'pause', details: { pause_token: token } });
  event('command.result', { name: 'watchdog', ok: true, data: { resolution: 'continued', pause_token: token, message: 'continued' } });
  assert.equal(b.state.controls.pauses.size, 0);
  await assert.rejects(b.controlPause('S', 'pause:1', 'continue'), /does not belong/);
});
test('R4 SSE before HTTP and reversed model HTTP responses keep latest requested switch', async t => {
  const { b, event } = backend(); const finishes = [];
  t.mock.method(globalThis, 'fetch', () => new Promise(resolve => finishes.push(resolve)));
  const first = b.runComposerCommand('/model a'); const second = b.runComposerCommand('/model b');
  event('command.result', { operation_id: 'B', name: 'model', ok: true, data: { accepted: true, applies: 'after_turn' } });
  finishes[1](new Response(JSON.stringify({ operation_id: 'B' }))); await second;
  assert.equal(b.state.pendingModel.name, 'b');
  finishes[0](new Response(JSON.stringify({ operation_id: 'A' }))); await first;
  assert.equal(b.state.pendingModel.name, 'b');
  event('model.changed', { operation_id: 'B', new: { agent: "build", source: "override", model: 'b' } });
  assert.equal(b.state.pendingModel, undefined);
});
test('R5 starting conversation blocks model agent and variant edits', async t => {
  const { b, event, messages } = backend(); b.state.session = undefined;
  let finish; t.mock.method(globalThis, 'fetch', () => new Promise(resolve => finish = resolve));
  const pending = b.send('hello');
  await Promise.resolve(); await Promise.resolve();
  b.selectModel('b');
  for (const name of ['model b','agent plan','variant off']) await assert.rejects(b.runComposerCommand('/' + name), /starting or opening/);
  assert.equal(messages.findLast(m => m.type === 'models').locked, true);
  event('session.agent_changed', { agent: 'plan' });
  b.abortStream(); finish(new Response(JSON.stringify({ session_id: 'S' })));
  await assert.rejects(pending);
});
test('R6 stale picker failure and stop completion do not affect replacement conversation', async t => {
  const { b, messages, event } = backend(); let finish;
  t.mock.method(globalThis, 'fetch', () => new Promise(resolve => finish = resolve));
  b.selectModel('other'); b.abortStream(); event('session.agent_changed', { agent: 'plan' });
  finish(new Response(JSON.stringify({ error: 'old' }), { status: 500 }));
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(messages.some(m => m.type === 'commandError'), false);
  const pending = b.runComposerCommand('/stop'); b.abortStream();
  finish(new Response(JSON.stringify({ accepted: true })));
  await assert.rejects(pending, /conversation changed/);
});
test('R7 local commands before endpoint and catalog survives generation change but not restart', async t => {
  const { b, messages, event } = backend(); b.endpoint = undefined;
  await b.refreshCommands(); assert(messages.at(-1).items.some(c => c.name === 'skills'));
  b.fetchModels = async () => {}; b.bootstrapConversations = async () => {};
  let finish; t.mock.method(globalThis, 'fetch', () => new Promise(resolve => finish = resolve));
  b.setServerInfo({ port: 1, token: 'a' }); b.abortStream();
  event('session.agent_changed', { agent: 'plan' });
  finish(new Response(JSON.stringify({ commands: [{ name: 'fresh', description: 'fresh', args_hint: '' }] })));
  await new Promise(resolve => setImmediate(resolve)); assert.equal(b.commands[0].name, 'fresh');
  b.setServerInfo({ port: 2, token: 'b' }); assert.equal(b.commands.length, 0);
  finish(new Response(JSON.stringify({ commands: [] }))); await new Promise(resolve => setImmediate(resolve));
});
test('R8 live variant picker uses correlated pinned offer not catalog or replay', async t => {
  const { b, messages, event } = backend(); b.models = [{ name: 'a', variants: ['wrong'] }];
  t.mock.method(globalThis, 'fetch', async (_url, init) => { assert.deepEqual(JSON.parse(init.body), { name: 'variant', arguments: '' }); return new Response(JSON.stringify({ operation_id: 'V' })); });
  event('command.result', { operation_id: 'REPLAY', name: 'variant', ok: true, data: { offered: ['replay'] } });
  assert.equal(messages.some(m => m.type === 'commandPicker'), false);
  await b.runComposerCommand('/variant');
  event('command.result', { operation_id: 'V', name: 'variant', ok: true, data: { offered: ['pinned'], variant: 'pinned' } });
  assert.deepEqual(messages.findLast(m => m.type === 'commandPicker').items.map(i => i.name), ['pinned','off']);
});
test('R9 reopen seeds plan from summary and newer agent event wins', async t => {
  const { b, messages, event } = backend();
  t.mock.method(globalThis, 'fetch', async () => new Response(JSON.stringify({ agent: 'plan', model: 'a', current_seq: 3 })));
  await b.loadSessionModel('S'); assert.equal(messages.findLast(m => m.type === 'agent').name, 'plan');
  for (let i = 0; i < 4; i++) event('session.agent_changed', { agent: 'review' });
  await b.loadSessionModel('S'); assert.equal(b.state.agent, 'review');
});
for (const resume of [false,true]) test(`R10 skill ${resume ? 'resumes' : 'creates'} then posts typed message`, async t => {
  const { b, event } = backend(); b.state.session = undefined; if (resume) b.state.resumeId = 'S';
  const calls = []; t.mock.method(globalThis, 'fetch', async (url, init) => { calls.push({ url, body: JSON.parse(init.body) }); return new Response(JSON.stringify(calls.length === 1 ? { session_id: 'S' } : { command_id: 'SK' })); });
  await b.runComposerCommand('/skill review check');
  assert.equal(calls[0].body.resume, resume ? 'S' : undefined);
  assert.deepEqual(calls[1].body.content, [{ type: 'text', text: 'check' }, { type: 'skill', name: 'review' }]);
  event('command.result', { operation_id: 'SK', name: 'message', ok: false, error: 'disabled' });
  assert.equal(b.state.commands.has('SK'), false);
});
