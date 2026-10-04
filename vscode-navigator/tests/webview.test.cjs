const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const { JSDOM } = require("jsdom");

test('conversation search counts and caps rows, filters titles and keyboard clears then closes',()=>{
  const {chatHtml}=require('../out/tests/chatHtml.js');const dom=new JSDOM(chatHtml('n','chat.js','chat.css','resource:'),{runScripts:'outside-only'});
  const posts=[];dom.window.acquireVsCodeApi=()=>({postMessage:m=>posts.push(m),getState(){},setState(){}});dom.window.eval(fs.readFileSync('out/chat.js','utf8'));
  const d=dom.window.document;const receive=data=>dom.window.dispatchEvent(new dom.window.MessageEvent('message',{data:{...data,generation:1}}));
  receive({type:'conversationOpened',id:'S',title:'S'});receive({type:'conversations',items:Array.from({length:300},(_,i)=>({id:String(i),title:`Conversation ${i}`,updatedAt:'2026-01-01',messageCount:1}))});
  d.querySelector('.conv-trigger').click();const search=d.querySelector('.conv-panel .picker-search');assert.equal(d.activeElement,search);assert.equal(d.querySelectorAll('.conv-row').length,50);
  search.value='Conversation 299';search.dispatchEvent(new dom.window.Event('input'));assert.equal(d.querySelectorAll('.conv-row').length,1);assert.match(d.querySelector('.conv-panel .picker-count').textContent,/1 of 300/);
  const key=key=>search.dispatchEvent(new dom.window.KeyboardEvent('keydown',{key,bubbles:true,cancelable:true}));key('Escape');assert.equal(search.value,'');assert.equal(d.querySelector('.conv-panel').hidden,false);key('Escape');assert.equal(d.querySelector('.conv-panel').hidden,true);dom.window.close();
});

test('searchable shipped pickers rank names, match descriptions, cap 300 rows and support clear-then-close Escape',()=>{
  const {chatHtml}=require('../out/tests/chatHtml.js');
  const dom=new JSDOM(chatHtml('n','chat.js','chat.css','resource:'),{runScripts:'outside-only'});const posts=[];
  dom.window.acquireVsCodeApi=()=>({postMessage:m=>posts.push(m),getState(){},setState(){}});
  dom.window.eval(fs.readFileSync('out/chat.js','utf8'));
  const d=dom.window.document;const receive=data=>dom.window.dispatchEvent(new dom.window.MessageEvent('message',{data:{...data,generation:1}}));
  receive({type:'conversationOpened',id:'S',title:'S'});receive({type:'status',state:'ready'});
  const items=Array.from({length:300},(_,i)=>({name:`model-${i}`,description:`provider id ${i}`}));
  items[90]={name:'target',description:'ordinary'};items[91]={name:'target-more',description:'ordinary'};items[0]={name:'other',description:'target description'};
  receive({type:'commandPicker',kind:'model',items});
  const search=d.querySelector('#command-palette .picker-search');assert.equal(d.activeElement,search);assert.equal(d.querySelectorAll('#picker-options [role=option]').length,50);assert.match(d.querySelector('#command-palette .picker-count').textContent,/300 of 300.*Type to narrow/);
  const input=value=>{search.value=value;search.dispatchEvent(new dom.window.Event('input'));};
  const key=key=>search.dispatchEvent(new dom.window.KeyboardEvent('keydown',{key,bubbles:true,cancelable:true}));
  input('TARGET');assert.match(d.querySelector('#command-palette .picker-count').textContent,/3 of 300/);assert.match(d.querySelector('#picker-options [role=option]').textContent,/^targetordinary/);
  key('ArrowDown');assert.equal(search.getAttribute('aria-activedescendant'),'command-option-1');key('Enter');assert.equal(posts.at(-1).text,'/model target-more');
  receive({type:'commandPicker',kind:'skills',items});input('provider id 299');assert.match(d.querySelector('#command-palette .picker-count').textContent,/1 of 300/);key('Enter');assert.equal(d.querySelector('#prompt').value,'/skill model-299 ');
  receive({type:'commandPicker',kind:'variant',items});input('missing');assert.match(d.querySelector('#picker-options').textContent,/No matches/);key('Escape');assert.equal(search.value,'');assert.equal(d.querySelector('#command-palette').hidden,false);key('Escape');assert.equal(d.querySelector('#command-palette').hidden,true);
  receive({type:'models',items:items.map(i=>({alias:i.name,model:i.description})),locked:false});d.querySelector('#model').dispatchEvent(new dom.window.MouseEvent('mousedown',{bubbles:true,cancelable:true}));assert.equal(d.activeElement,search);
  dom.window.close();
});

test("R2 shipped page restores refused admitted skill without overwriting newer typing; R5 picker locks", () => {
  const { chatHtml } = require("../out/tests/chatHtml.js");
  const dom = new JSDOM(chatHtml("n", "chat.js", "chat.css", "resource:"), { runScripts: "outside-only" });
  dom.window.acquireVsCodeApi = () => ({ postMessage() {}, getState() {}, setState() {} });
  dom.window.eval(fs.readFileSync("out/chat.js", "utf8"));
  const receive = data => dom.window.dispatchEvent(new dom.window.MessageEvent("message", { data: { ...data, generation: 1 } }));
  receive({ type: "conversationOpened", id: "S", title: "S" });
  const prompt = dom.window.document.querySelector("#prompt");
  prompt.value = "/skill review check";
  receive({ type: "sendResult", text: prompt.value, accepted: true });
  receive({ type: "sendResult", text: "/skill review check", accepted: false, restoreDraft: true });
  assert.equal(prompt.value, "/skill review check");
  receive({ type: "sendResult", text: prompt.value, accepted: true });
  prompt.value = "new typing"; prompt.dispatchEvent(new dom.window.Event("input"));
  receive({ type: "sendResult", text: "/skill review check", accepted: false, restoreDraft: true });
  assert.equal(prompt.value, "new typing");
  receive({ type: "models", items: [{ alias: "a", model: "a" }], locked: true });
  assert.equal(dom.window.document.querySelector("#model").disabled, true);
  dom.window.dispatchEvent(new dom.window.MessageEvent("message", { data: { type: "conversationOpened", id: "B", title: "B", generation: 2 } }));
  prompt.value = "/stop";
  dom.window.dispatchEvent(new dom.window.MessageEvent("message", { data: { type: "sendResult", text: "/stop", accepted: true, generation: 1 } }));
  assert.equal(prompt.value, "/stop", "R6 stale acknowledgement cannot consume an identical draft in B");
  dom.window.close();
});

test("shipped palette supports filter, arrows, Tab, Enter, Escape and mouse; agent replay", () => {
  const { chatHtml } = require("../out/tests/chatHtml.js");
  const dom = new JSDOM(chatHtml("n", "chat.js", "chat.css", "resource:"), { runScripts: "outside-only" });
  dom.window.acquireVsCodeApi = () => ({ postMessage() {}, getState() {}, setState() {} });
  dom.window.eval(fs.readFileSync("out/chat.js", "utf8"));
  const d = dom.window.document;
  const receive = data => dom.window.dispatchEvent(new dom.window.MessageEvent("message", { data: { ...data, generation: 1 } }));
  receive({ type: "conversationOpened", title: "A", id: "A" });
  receive({ type: "status", state: "ready" });
  receive({ type: "commands", items: [{ name: "model", description: "Switch model", args_hint: "[alias]" }, { name: "compact", description: "Compact", args_hint: "[focus]" }] });
  const prompt = d.querySelector("#prompt");
  const input = text => { prompt.value = text; prompt.dispatchEvent(new dom.window.Event("input")); };
  const key = key => prompt.dispatchEvent(new dom.window.KeyboardEvent("keydown", { key, bubbles: true, cancelable: true }));
  input("/");
  assert.equal(d.querySelectorAll('[role="option"]').length, 2);
  key("ArrowDown"); key("Tab"); assert.equal(prompt.value, "/compact ");
  input("/mo"); key("Enter"); assert.equal(prompt.value, "/model ");
  input("/"); key("Escape"); assert.equal(d.querySelector("#command-palette").hidden, true);
  input("/"); key("ArrowUp"); assert.equal(d.querySelector('#command-palette .picker-search').getAttribute("aria-activedescendant"), "command-option-1");
  d.querySelector('[role="option"]').click(); assert.equal(prompt.value, "/model ");
  receive({ type: "history", items: [{ type: "agent", name: "plan", generation: 1 }] });
  assert.equal(d.querySelector(".agent-label").textContent, "plan");
  receive({ type: "agent", name: "old", generation: 0 });
  dom.window.close();
});

test("real webview initializes with frozen VS Code API, renders history and posts Allow", () => {
  const { chatHtml } = require("../out/tests/chatHtml.js");
  // jsdom does not enforce CSP; verify the nonce/CSP in the real VS Code panel manually.
  const dom = new JSDOM(chatHtml("test-nonce", "chat.js", "chat.css", "vscode-resource:"), { runScripts: "outside-only" });
  assert.match(dom.window.document.querySelector('meta[http-equiv="Content-Security-Policy"]').content, /script-src 'nonce-test-nonce'/);
  assert.equal(dom.window.document.querySelector("script").nonce, "test-nonce");
  const posts = [];
  const errors = [];
  dom.window.addEventListener("error", event => { errors.push(event.error); event.preventDefault(); });
  dom.window.acquireVsCodeApi = () => Object.freeze({ postMessage: message => posts.push(message), getState: () => undefined, setState() {} });
  dom.window.eval(fs.readFileSync("out/chat.js", "utf8"));
  assert.equal(posts[0].type, "ready");
  const receive = data => dom.window.dispatchEvent(new dom.window.MessageEvent("message", { data }));
  receive({ type: "conversationOpened", id: "A", title: "Conversation A", generation: 2 });
  receive({ type: "history", generation: 2, items: [{ type: "message", author: "driver", text: "Hello from history", id: "M", generation: 2 }] });
  assert.match(dom.window.document.querySelector("#transcript").textContent, /Hello from history/);
  receive({ type: "permission", generation: 2, card: { requestId: "RA", callId: "C", tool: "bash", agent: "build", scope: { kind: "folder", value: "/a" }, reason: "Review", call: "pwd", allowFolder: false } });
  [...dom.window.document.querySelectorAll("button")].find(button => button.textContent === "Allow once").click();
  assert.deepEqual(JSON.parse(JSON.stringify(posts.at(-1))), { type: "answerPermission", sessionId: "A", requestId: "RA", decision: "allow_once", generation: 2 });
  assert.deepEqual(errors, []);
  dom.window.close();
});
