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
test('subagent events replay list, child read-only transcript and Back use real host and shipped page', async t => {
  const { b,dom,event,tick,send } = fullChat(t); b.state.session='P'; b.scheduleChildRead=()=>{};
  event('subagent.spawned',{parent_session_id:'P',child_session_id:'C',agent:'review',goal:'<script>check</script>'});
  const row=dom.window.document.querySelector('.child-row');
  assert.match(row.textContent,/review/); assert.equal(row.querySelector('script'),null);
  const calls=[];
  t.mock.method(globalThis,'fetch',async (url,init) => { calls.push({url,...init}); return new Response(JSON.stringify({parent_session_id:'P',promptable:false,agent:'review'})); });
  row.querySelector('.child-open').click(); await tick();
  assert.equal(b.isReadOnly(),true); assert.equal(dom.window.document.querySelector('#prompt').disabled,true);
  assert(calls.every(c=>c.method==='GET')); assert.match(dom.window.document.querySelector('.child-header').textContent,/Read-only/);
  send('forged message'); await tick(); assert.equal(calls.length,1);
  await assert.rejects(b.send('direct'),/read-only/); await assert.rejects(b.runComposerCommand('/stop'),/read-only/);
  event('message.received',{source:'user:subagent',content:'child output',command_id:'M'});
  assert.match(dom.window.document.querySelector('#transcript').textContent,/untrusted/);
  event('permission.requested',{request_id:'R',tool:'bash',agent:'review',arguments:'{}',reason:'Review me'});
  assert.equal([...dom.window.document.querySelectorAll('button')].some(x=>x.textContent==='Allow once'),false);
  dom.window.document.querySelector('.child-header button').click(); await tick();
  assert.equal(b.isReadOnly(),false); assert.equal(dom.window.document.querySelector('#prompt').disabled,false);
  assert.match(dom.window.document.querySelector('.child-row').textContent,/Waiting for permission/);
});
test('subagent Stop and Continue confirm, exact bodies, refusals and stale parent controls', async t => {
  const { b,dom,event,tick } = fullChat(t); b.state.session='P'; b.scheduleChildRead=()=>{};
  event('subagent.spawned',{parent_session_id:'P',child_session_id:'C',agent:'build',goal:'work'});
  event('agent.yield',{child_session_id:'C',kind:'budget'});
  let confirmed=0; vscode.window.showWarningMessage=async()=>{confirmed++;return 'Confirm';};
  const calls=[]; t.mock.method(globalThis,'fetch',async(url,init)=>{calls.push({url,...init});return new Response(JSON.stringify({accepted:false,error:'Not parked'}));});
  [...dom.window.document.querySelectorAll('.child-row button')].find(x=>x.textContent==='Continue').click(); await tick();
  assert.equal(confirmed,1); assert.deepEqual(JSON.parse(calls[0].body),{pause_token:null});
  assert.match(calls[0].url,/\/P\/children\/C\/continue$/); assert.match(dom.window.document.querySelector('.child-row').textContent,/Not parked/);
  const token={incarnation:'I',hold_id:'H'};
  event('agent.yield',{child_session_id:'C',kind:'watchdog',details:{pause_token:token}});
  t.mock.method(globalThis,'fetch',async(url,init)=>{calls.push({url,...init});return new Response(JSON.stringify({accepted:true}));});
  [...dom.window.document.querySelectorAll('.child-row button')].find(x=>x.textContent==='Continue').click(); await tick();
  assert.deepEqual(JSON.parse(calls.at(-1).body),{pause_token:token});
  [...dom.window.document.querySelectorAll('.child-row button')].find(x=>x.textContent==='Stop').click(); await tick();
  assert.match(calls.at(-1).url,/\/C\/stop$/); assert.equal(calls.at(-1).body,undefined);
  const old=b.currentGeneration(); await b.newConversation();
  await assert.rejects(b.controlChild('P','C','stop',old),/conversation changed/);
});
test('child event stream aborts on Back, parent switch and dispose; never POST resume', async t => {
  const { b,event,tick } = fullChat(t); b.state.session='P'; b.scheduleChildRead=()=>{};
  event('subagent.spawned',{parent_session_id:'P',child_session_id:'C',agent:'review',goal:'work'});
  delete b.connect;
  const signals=[];const calls=[];
  t.mock.method(globalThis,'fetch',async(url,init)=>{
    calls.push({url,...init});
    if(url.endsWith('/events')) { signals.push(init.signal); return new Response(new ReadableStream({start(controller){init.signal.addEventListener('abort',()=>controller.close());}})); }
    return new Response(JSON.stringify({parent_session_id:'P',promptable:false,agent:'review'}));
  });
  await b.openChild('P','C');await tick();const first=signals.at(-1);
  await b.backToParent('P');assert.equal(first.aborted,true);
  await b.openChild('P','C');await tick();const second=signals.at(-1);
  await b.newConversation();assert.equal(second.aborted,true);
  b.state.session='P';b.state.children.set('C',{id:'C',agent:'review',goal:'',state:'running'});
  await b.openChild('P','C');await tick();const third=signals.at(-1);
  b.dispose();assert.equal(third.aborted,true);
  assert(calls.every(c=>!c.method||c.method==='GET'));
});
test('subagent session list seeds rows, replay preserves terminal states and watchdog resolution', async t => {
  const { b,event,dom,tick } = fullChat(t);b.state.session='P';b.scheduleChildRead=()=>{};
  t.mock.method(globalThis,'fetch',async()=>new Response(JSON.stringify({sessions:[{id:'P',promptable:true},{id:'C',parent_session_id:'P',promptable:false,agent:'plan',child_status:'out_of_steps'}]})));
  delete b.listConversations;await b.listConversations();
  assert.equal(b.state.children.get('C').state,'budget');assert.match(dom.window.document.querySelector('.child-row').textContent,/Out of steps/);
  const token={incarnation:'I',hold_id:'H'};
  event('agent.yield',{child_session_id:'C',kind:'watchdog',details:{pause_token:token}});
  assert.equal(b.state.children.get('C').state,'watchdog');
  event('command.result',{child_session_id:'C',name:'watchdog',ok:true,data:{resolution:'continued',pause_token:token,message:'Continued'}});
  assert.equal(b.state.children.get('C').state,'running');
  event('subagent.done',{parent_session_id:'P',child_session_id:'C',outcome:'done'});
  event('subagent.spawned',{parent_session_id:'P',child_session_id:'C',agent:'plan',goal:'replayed'});
  assert.equal(b.state.children.get('C').state,'done');
  const old=b.currentGeneration();b.fetchModels=async()=>{};b.bootstrapConversations=async()=>{};b.refreshCommands=async()=>{};
  b.setServerInfo({port:5678,token:'new'});await tick();
  await assert.rejects(b.controlChild('P','C','stop',old),/conversation changed/);
});
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
  const backend = { isReadOnly: () => false, captureOperation: () => ({ owner: {}, generation: 2 }), ownsOperation: () => true, currentGeneration: () => 2, currentConversationId: () => "S", subscribe: () => () => {}, hasOpenPermissions: () => true,
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
    isReadOnly: () => false,
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
    publishSubagents() {},
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
