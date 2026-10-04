const { test } = require("node:test");
const assert = require("node:assert/strict");
const Module = require("node:module");
const originalLoad = Module._load;
const noopDisposable = () => ({ dispose() {} });
let provider;
const vscode = {
  window: {
    showErrorMessage() {},
    visibleTextEditors: [],
    registerWebviewViewProvider(_id, value) { provider = value; return noopDisposable(); },
    onDidChangeActiveTextEditor: noopDisposable,
    onDidChangeTextEditorSelection: noopDisposable,
    onDidChangeVisibleTextEditors: noopDisposable,
    tabGroups: { onDidChangeTabs: noopDisposable, all: [] },
  },
  workspace: { workspaceFolders: [] },
  Uri: { joinPath(_uri, ...parts) { return parts.join("/"); } },
};
Module._load = function (id, ...args) {
  if (id === "vscode") return vscode;
  return originalLoad.call(this, id, ...args);
};
const { registerChatView } = require("../out/tests/chatView.js");
const { CognitionBackend } = require("../out/tests/cognition.js");
Module._load = originalLoad;

function fullChat(t) {
  const { JSDOM } = require('jsdom');
  const { chatHtml } = require('../out/tests/chatHtml.js');
  const fs = require('node:fs');
  const b = new CognitionBackend(); b.endpoint = { port: 1234, token: 't' };
  b.connect = async () => {}; b.fetchContext = async () => {}; b.listConversations = async () => [];
  b.commands = ['model','agent','variant'].map(name => ({ name, description: name, args_hint: '' }));
  let receive;
  const dom = new JSDOM(chatHtml('n','chat.js','chat.css','resource:'), { runScripts: 'outside-only' });
  t.after(() => dom.window.close());
  registerChatView({ extensionUri: 'extension' }, b);
  provider.resolveWebviewView({ webview: { cspSource: 'test', asWebviewUri: value => value,
    postMessage(data) { dom.window.dispatchEvent(new dom.window.MessageEvent('message', { data })); },
    onDidReceiveMessage(listener) { receive = listener; return noopDisposable(); } }, onDidDispose() {} });
  dom.window.acquireVsCodeApi = () => ({ postMessage: message => receive(message), getState() {}, setState() {} });
  dom.window.eval(fs.readFileSync('out/chat.js','utf8'));
  const prompt = dom.window.document.querySelector('#prompt');
  const send = text => { prompt.value = text; receive({ type: 'send', text, includeContext: false, generation: b.currentGeneration() }); };
  let seq = 0;
  const event = (kind,payload) => b.handleFrame({ id: String(++seq), event: '', envelope: { seq,kind,payload } });
  const tick = () => new Promise(resolve => setImmediate(resolve));
  return { b, dom, prompt, send, event, tick };
}
for (const early of [true,false]) test(`R3.1 end-to-end failed open unlocks abandoned creation with response ${early ? 'before' : 'after'} rollback`, async t => {
  const { b,send,tick,prompt } = fullChat(t);
  let finishCreate; let finishOpen;
  t.mock.method(globalThis,'fetch',url => new Promise(resolve => {
    if (url.endsWith('/v1/sessions')) finishCreate=resolve;
    else finishOpen=resolve;
  }));
  send('abandoned'); await tick();
  const opening = b.openConversation('missing').catch(() => {});
  await tick();
  if (early) { finishCreate(new Response(JSON.stringify({session_id:'abandoned'}))); await tick(); }
  // Reconciliation retries the missing metadata three times before rollback.
  for (let attempt=0; attempt<3; attempt++) {
    finishOpen(new Response(JSON.stringify({error:'missing'}),{status:404}));
    if (attempt<2) await new Promise(resolve => setTimeout(resolve, 110*(attempt+1)));
  }
  await opening;
  if (!early) { finishCreate(new Response(JSON.stringify({session_id:'abandoned'}))); await tick(); }
  assert.equal(b.state.starting,false);
  t.mock.method(globalThis,'fetch',async url => new Response(JSON.stringify(url.endsWith('/messages') ? {accepted:true} : {session_id:'fresh'})));
  send('fresh'); await tick();
  assert.equal(b.currentConversationId(),'fresh'); assert.equal(prompt.value,'');
});
test('R3.2 end-to-end populated picker locks at nonzero opening generation and unlocks on completion', async t => {
  const { b,dom,tick } = fullChat(t);
  b.models=[{alias:'a',model:'a'}];
  await b.newConversation();
  const picker=dom.window.document.querySelector('#model');
  assert(b.currentGeneration()>=1); assert.equal(picker.disabled,false);
  let finish;
  t.mock.method(globalThis,'fetch',url => url.endsWith('/v1/sessions') ? Promise.resolve(new Response(JSON.stringify({session_id:'B'}))) : new Promise(resolve => finish=resolve));
  const opening=b.openConversation('B');
  await tick(); assert.equal(picker.disabled,true);
  finish(new Response(JSON.stringify({current_seq:1,model:'a',agent:'build',actor_state:'idle'})));
  await opening; assert.equal(picker.disabled,false);
  // Settle the independent model-summary read started after opening.
  finish(new Response(JSON.stringify({current_seq:1,model:'a',agent:'build',actor_state:'idle'})));
  await tick();
});
for (const resume of [false,true]) test(`R2.1 end-to-end ordinary ${resume ? 'resume' : 'create'} clears composer at destination generation`, async t => {
  const { b,prompt,send,tick } = fullChat(t); if (resume) b.state.resumeId = 'S';
  t.mock.method(globalThis,'fetch',async url => new Response(JSON.stringify(url.endsWith('/messages') ? { accepted:true } : { session_id:'S' })));
  send('hello'); await tick(); assert.equal(prompt.value,'');
});
test('R2.1 end-to-end first skill HTTP refusal stays inline at destination generation', async t => {
  const { dom,prompt,send,tick } = fullChat(t);
  t.mock.method(globalThis,'fetch',async url => new Response(JSON.stringify(url.endsWith('/messages') ? { error:'Skill disabled' } : { session_id:'S' }), { status:url.endsWith('/messages') ? 400 : 200 }));
  send('/skill review check'); await tick();
  assert.equal(prompt.value,'/skill review check'); assert.match(dom.window.document.querySelector('.command-error').textContent,/Skill disabled/);
});
test('R2.1 end-to-end failed creation reports inline error at destination generation', async t => {
  const { dom,prompt,send,tick } = fullChat(t);
  t.mock.method(globalThis,'fetch',async () => new Response(JSON.stringify({error:'Create refused'}),{status:400}));
  send('hello'); await tick();
  assert.equal(prompt.value,'hello'); assert.match(dom.window.document.querySelector('.command-error').textContent,/Create refused/);
});
for (const opening of [false,true]) test(`R2.2 end-to-end New abandons pending ${opening ? 'opening' : 'creation'} and unlocks picker`, async t => {
  const { b,dom,prompt,send,tick } = fullChat(t); let finish;
  t.mock.method(globalThis,'fetch',() => new Promise(resolve => finish=resolve));
  if (opening) { void b.openConversation('B').catch(() => {}); }
  else send('abandoned');
  await tick(); assert.equal(dom.window.document.querySelector('#model').disabled,true);
  send('/new'); await tick();
  assert.equal(b.state.starting,false);
  finish(new Response(JSON.stringify({ session_id:'old', current_seq:1 }))); await tick();
  t.mock.method(globalThis,'fetch',async url => new Response(JSON.stringify(url.endsWith('/messages') ? { accepted:true } : { session_id:'fresh' })));
  send('fresh'); await tick(); assert.equal(prompt.value,''); assert.equal(b.currentConversationId(),'fresh');
});
test('R2.2 end-to-end restart abandons pending creation', async t => {
  const { b,send,tick,prompt } = fullChat(t); let finish;
  t.mock.method(globalThis,'fetch',() => new Promise(resolve => finish=resolve));
  send('abandoned'); await tick();
  b.fetchModels = async () => {}; b.bootstrapConversations = async () => {}; b.refreshCommands = async () => {};
  b.setServerInfo({port:5678,token:'new'});
  assert.equal(b.state.starting,false);
  finish(new Response(JSON.stringify({session_id:'old'}))); await tick();
  t.mock.method(globalThis,'fetch',async url => new Response(JSON.stringify(url.endsWith('/messages') ? {accepted:true} : {session_id:'fresh'})));
  send('fresh'); await tick(); assert.equal(prompt.value,'');
});
test('R2.3 end-to-end newer refused model relinquishes existing pending switch before HTTP', async t => {
  const { b,send,event,tick,dom } = fullChat(t); b.state.session='A';
  b.state.pendingModel={alias:'old',operationId:'OLD'}; b.emit({type:'modelPending',alias:'old'});
  let finish; t.mock.method(globalThis,'fetch',() => new Promise(resolve => finish=resolve));
  send('/model next'); assert.equal(dom.window.document.querySelector('.pending-model').textContent,'');
  event('command.result',{name:'model',operation_id:'NEW',ok:false,error:'refused'});
  finish(new Response(JSON.stringify({operation_id:'NEW'}))); await tick();
  assert.equal(b.state.pendingModel,undefined); assert.equal(dom.window.document.querySelector('.pending-model').textContent,'');
});
test('R2.4 end-to-end failed open restores outstanding model command ownership', async t => {
  const { b,send,event,tick,dom } = fullChat(t); b.state.session='A';
  let finish; t.mock.method(globalThis,'fetch',url => url.endsWith('/commands') ? new Promise(resolve => finish=resolve) : Promise.resolve(new Response(JSON.stringify({error:'missing'}),{status:404})));
  send('/model next'); const opening=b.openConversation('B').catch(() => {}); await opening;
  event('command.result',{name:'model',operation_id:'M',ok:true,data:{accepted:true,applies:'after_turn'}});
  finish(new Response(JSON.stringify({operation_id:'M'}))); await tick();
  assert.equal(b.state.pendingModel.alias,'next'); assert.match(dom.window.document.querySelector('.pending-model').textContent,/next/);
});

test("R1 skill confirms permission cancellation before submission without editor context", async () => {
  let receive; let submitted = 0; let confirms = 0;
  vscode.window.showWarningMessage = async () => { confirms++; return "Send anyway"; };
  Object.defineProperty(vscode.window, "activeTextEditor", { configurable: true, get() { throw new Error("No editor context for skills"); } });
  const backend = { captureOperation: () => ({ owner: {}, generation: 2 }), ownsOperation: () => true, currentGeneration: () => 2, currentConversationId: () => "S", subscribe: () => () => {}, hasOpenPermissions: () => true,
    runComposerCommand: async () => { assert.equal(confirms, 1); submitted++; return false; } };
  registerChatView({ extensionUri: "extension" }, backend);
  provider.resolveWebviewView({ webview: { cspSource: "test", asWebviewUri: value => value, postMessage() {}, onDidReceiveMessage(listener) { receive = listener; return noopDisposable(); } }, onDidDispose() {} });
  receive({ type: "send", text: "/skill review check", generation: 2, includeContext: true });
  await new Promise(resolve => setImmediate(resolve));
  assert.equal(submitted, 1);
  delete vscode.window.activeTextEditor;
});

test("host routes slash commands before editor context or permission confirmation", async () => {
  let receive;
  let contextReads = 0;
  let command;
  Object.defineProperty(vscode.window, "activeTextEditor", { configurable: true, get() { contextReads++; throw new Error("Editor must not be read"); } });
  const backend = {
    captureOperation: () => ({ owner: {}, generation: 2 }), ownsOperation: () => true,
    currentGeneration: () => 2,
    subscribe: () => () => {},
    currentConversationId: () => "S",
    runComposerCommand: async (text, generation) => { command = { text, generation }; return false; },
    hasOpenPermissions: () => { throw new Error("Commands do not cancel permissions"); },
  };
  registerChatView({ extensionUri: "extension" }, backend);
  provider.resolveWebviewView({ webview: { cspSource: "test", asWebviewUri: value => value, postMessage() {}, onDidReceiveMessage(listener) { receive = listener; return noopDisposable(); } }, onDidDispose() {} });
  receive({ type: "send", text: "/context", includeContext: true, generation: 2 });
  await Promise.resolve(); await Promise.resolve();
  assert.deepEqual(command, { text: "/context", generation: 2 });
  assert.equal(contextReads, 0);
  delete vscode.window.activeTextEditor;
});

test("late-created and recreated views receive current session identity before replay", () => {
  let cancelled = 0;
  const backend = {
    currentContext: () => ({ incarnation: "I", revision: 3, estimate_total: 42 }),
    currentGeneration: () => 1,
    refreshCommands: async () => {},
    resumePermissionReads: () => {},
    currentConversationId: () => "already-open",
    history: () => [{ type: "pause", id: "P", kind: "budget", text: "Out of steps" }],
    subscribe: () => () => {},
    cancelPermissionReads: () => { cancelled++; },
  };
  registerChatView({ extensionUri: "extension" }, backend);
  for (let i = 0; i < 2; i++) {
    const messages = [];
    let receive;
    let dispose;
    provider.resolveWebviewView({
      webview: {
        cspSource: "test", asWebviewUri: value => value,
        postMessage(message) { messages.push(message); },
        onDidReceiveMessage(listener) { receive = listener; return noopDisposable(); },
      },
      onDidDispose(listener) { dispose = listener; },
    });
    receive({ type: "ready" });
    assert.deepEqual(messages[0], { type: "conversationOpened", id: "already-open", title: "Conversation", generation: 1 });
    assert.equal(messages[1].type, "history");
    assert.deepEqual(messages[2], { type: "usage", snapshot: backend.currentContext(), generation: 1 });
    dispose();
  }
  assert.equal(cancelled, 2);
});
