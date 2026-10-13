const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const { JSDOM } = require("jsdom");

test('child watchdog continuation needs no root pause token and pending actions stay disabled', () => {
  const { dom, d, receive } = keyboardPage();
  const child = { id: 'child', agent: 'build', goal: 'Check code', state: 'watchdog' };
  const render = changes => receive({ type: 'subagents', parentId: 'S', items: [{ ...child, ...changes }] });
  render({});
  let buttons = d.querySelectorAll('.child-actions button');
  assert.equal(buttons[0].disabled, false);
  assert.equal(buttons[0].title, '');
  assert.equal(buttons[1].disabled, false);
  render({ pauseToken: { incarnation: 'run', hold_id: 'hold' } });
  assert.equal(d.querySelector('.child-actions button').disabled, false);
  render({ state: 'budget' });
  assert.equal(d.querySelector('.child-actions button').disabled, false);
  render({ pending: 'continue', pauseToken: { incarnation: 'run', hold_id: 'hold' } });
  assert.equal(d.querySelector('.child-actions button').disabled, true);
  dom.window.close();
});

test('conversation colour hooks preserve the agent of each turn and tool labels', () => {
  const { dom, d, receive } = keyboardPage();
  receive({ type: 'agent', name: 'build' });
  receive({ type: 'turnStart', id: 'build-turn' });
  receive({ type: 'agent', name: 'plan' });
  receive({ type: 'turnStart', id: 'plan-turn' });
  assert.deepEqual([...d.querySelectorAll('.message.cognition')].map(n => n.dataset.agent), ['build', 'plan']);
  assert.equal(d.querySelector('.agent-label').dataset.agent, 'plan');
  receive({ type: 'tool', id: 'build-turn', callId: 'shell', phase: 'start', name: 'bash', summary: 'bash npm test' });
  assert.equal(d.querySelector('.tool-verb').textContent, 'bash');
  assert.equal(d.querySelector('.tool-text').textContent, 'bash npm test');
  assert.equal(d.querySelector('.tool-row').getAttribute('aria-label'), 'Running: bash npm test');
  dom.window.close();
});

function keyboardPage(){
  const {chatHtml}=require('../out/tests/chatHtml.js');const dom=new JSDOM(chatHtml('n','chat.js','chat.css','resource:'),{runScripts:'outside-only'});const posts=[];
  dom.window.acquireVsCodeApi=()=>({postMessage:m=>posts.push(m),getState(){},setState(){}});dom.window.eval(fs.readFileSync('out/chat.js','utf8'));
  const d=dom.window.document;const receive=data=>dom.window.dispatchEvent(new dom.window.MessageEvent('message',{data:{...data,generation:1}}));
  receive({type:'conversationOpened',id:'S',title:'S'});receive({type:'status',state:'ready'});
  const key=(key,extra={})=>{const target=d.activeElement;const event=new dom.window.KeyboardEvent('keydown',{key,bubbles:true,cancelable:true,...extra});target.dispatchEvent(event);if(!event.defaultPrevented&&key.length===1){target.value+=key;target.dispatchEvent(new dom.window.Event('input',{bubbles:true}));}};
  return {dom,d,posts,receive,key,type:text=>{for(const c of text)key(c);}};
}

test('subagent controls sit beside the state and goals keep their full tooltip without delivery labels', () => {
  const { dom, d, receive, posts } = keyboardPage();
  const goal = 'Review a long delegated task. '.repeat(30);
  const child = { id: 'child', agent: 'lux', goal, state: 'running' };
  for (const delivery of ['next_turn', 'steer', 'interrupt', 'future_wire_value']) {
    receive({ type: 'subagents', parentId: 'S', items: [{ ...child, delivery }] });
    const row = d.querySelector('.child-row');
    assert.ok(!row.textContent.includes(delivery));
    assert.equal(row.querySelector('.child-goal').textContent, goal);
    assert.equal(row.querySelector('.child-goal').title, goal);
    const heading = row.querySelector('.child-row-heading');
    assert.match(heading.textContent, /lux · RunningStop/);
    assert.ok(heading.querySelector('.child-open.control-secondary'));
    const stop = heading.querySelector('.child-actions button');
    assert.equal(stop.className, 'control-secondary control-danger');
    assert.equal(stop.type, 'button');
  }
  d.querySelector('.child-actions button').click();
  assert.deepEqual(JSON.parse(JSON.stringify(posts.at(-1))), { type: 'controlChild', parentId: 'S', childId: 'child', action: 'stop', generation: 1 });
  receive({ type: 'subagents', parentId: 'S', items: [{ ...child, state: 'watchdog', error: 'Cannot stop: connection lost', reason: 'Please review the result' }] });
  assert.equal(d.querySelector('.child-row .command-error').textContent, 'Cannot stop: connection lost');
  assert.equal(d.querySelector('.child-reason').textContent, 'Please review the result');
  assert.equal(d.querySelector('.child-actions button').className, 'control-secondary');
  dom.window.close();
});

test('subagent view and watchdog controls share panel button classes and preserve errors', () => {
  const { dom, d, receive } = keyboardPage();
  receive({ type: 'childView', parentId: 'S', child: { id: 'child', agent: 'lux', goal: 'Review', state: 'watchdog', error: 'Connection lost' } });
  assert.deepEqual([...d.querySelectorAll('.child-header .child-actions button')].map(b => b.className), ['control-secondary', 'control-secondary control-danger']);
  assert.equal(d.querySelector('.child-header .command-error').textContent, 'Connection lost');
  receive({ type: 'childView' });
  receive({ type: 'pause', id: 'pause', kind: 'watchdog', text: 'Please review', token: { incarnation: 'run', hold_id: 'hold' } });
  assert.deepEqual([...d.querySelectorAll('.pause-card .control-actions button')].map(b => b.className), ['control-primary', 'control-secondary control-danger']);
  dom.window.close();
});

test('shared control styles reset native buttons with theme borders, focus and two-line goals', () => {
  const css = fs.readFileSync('out/chat.css', 'utf8').replace(/\s+/g, ' ');
  assert.match(css, /\.control-primary, \.control-secondary\s*\{[^}]*padding: 5px 9px;[^}]*border: 1px solid var\(--vscode-contrastBorder, transparent\);[^}]*border-radius: 3px;[^}]*font-size:/);
  assert.match(css, /\.control-primary:focus-visible, \.control-secondary:focus-visible\s*\{[^}]*outline: 2px solid var\(--vscode-contrastActiveBorder, var\(--vscode-focusBorder\)\)/);
  assert.match(css, /\.control-secondary\s*\{[^}]*--vscode-button-secondaryForeground[^}]*--vscode-button-secondaryBackground/);
  assert.match(css, /\.control-danger:hover:not\(:disabled\), \.control-danger:focus-visible\s*\{[^}]*--vscode-inputValidation-errorBorder[^}]*--vscode-errorForeground/);
  assert.match(css, /\.child-goal\s*\{[^}]*-webkit-line-clamp: 2;[^}]*overflow: hidden/);
});

test('question card requires explicit selection, retries errors and replays a read-only answer', () => {
  const {dom,d,posts,receive}=keyboardPage();
  const question={type:'question',id:'T',questionCallId:'ask',text:'Where next?',choices:{options:[{label:'Develop',description:'Integration branch'},{label:'Leave open'}],multiple:true,allow_custom:true,recommended:0}};
  receive(question);
  const buttons=d.querySelectorAll('.question-option');
  assert.equal(buttons[0].getAttribute('aria-pressed'),'false');
  const send=d.querySelector('.question-card .control-primary');
  assert.equal(send.disabled,true);
  buttons[1].click();send.click();
  assert.deepEqual(JSON.parse(JSON.stringify(posts.at(-1).answer)),{question_call_id:'ask',selected:[1]});
  assert.equal(buttons[0].disabled,true);
  receive({type:'questionState',questionCallId:'ask',error:'Stale answer'});
  assert.equal(buttons[0].disabled,false);
  assert.match(d.querySelector('.control-outcome').textContent,/Stale answer/);
  receive({type:'history',items:[{...question,generation:1},{type:'message',generation:1,id:'m',author:'captain',text:'Leave open',answer:{question_call_id:'ask',selected:[1],custom:'Until reviewed'}}]});
  assert.equal(d.querySelectorAll('.question-option')[1].getAttribute('aria-pressed'),'true');
  assert.equal(d.querySelector('.question-option').disabled,true);
  assert.match(d.querySelector('.question-card').textContent,/Until reviewed/);
  dom.window.close();
});
test('R1.3 focused composer key-by-key preserves literal slash, command arguments and palette completion',()=>{
  const {dom,d,posts,receive,key,type}=keyboardPage();receive({type:'commands',items:[{name:'model',description:'Choose model',args_hint:'[name]'}]});const prompt=d.querySelector('#prompt');prompt.focus();
  type('//text');assert.equal(d.activeElement,prompt);key('Enter');assert.equal(posts.at(-1).text,'//text');receive({type:'sendResult',text:'//text',accepted:true});
  type('/compact focus');assert.equal(d.activeElement,prompt);key('Enter');assert.equal(posts.at(-1).text,'/compact focus');receive({type:'sendResult',text:'/compact focus',accepted:true});
  type('/mo');key('Tab');assert.equal(prompt.value,'/model ');assert.equal(d.activeElement,prompt);dom.window.close();
});
test('R1.2 selector picker selection and refusal preserve existing typed draft',()=>{
  const {dom,d,posts,receive,key,type}=keyboardPage();const prompt=d.querySelector('#prompt');prompt.focus();type('my unfinished request');
  receive({type:'models',items:[{name:'target',gateway:'gw',provider_group:'provider',upstream_id:'provider/model',api:'chat',enabled:true}],locked:false});d.querySelector('#model').focus();key('Enter');type('target');key('Enter');assert.deepEqual(JSON.parse(JSON.stringify(posts.at(-1))),{type:'selectModel',name:'target',generation:1});assert.equal(prompt.value,'my unfinished request');receive({type:'commandError',text:'Switch refused'});assert.equal(prompt.value,'my unfinished request');dom.window.close();
});
test('R1.6 conversation editing and IME stay in search; arrows scroll selected last row',()=>{
  const {dom,d,posts,receive,key}=keyboardPage();receive({type:'conversations',items:Array.from({length:50},(_,i)=>({id:String(i),title:`item ${i}`,updatedAt:'2026-01-01',messageCount:1}))});d.querySelector('.conv-trigger').click();const search=d.activeElement;
  key('Home');assert.equal(d.activeElement,search);key('End');assert.equal(d.activeElement,search);const before=posts.length;key('Enter',{isComposing:true});assert.equal(posts.length,before);
  let scrolled;dom.window.HTMLElement.prototype.scrollIntoView=function(){scrolled=this.id;};key('ArrowUp');assert.equal(search.getAttribute('aria-activedescendant'),'conversation-option-49');assert.equal(scrolled,'conversation-option-49');dom.window.close();
});

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
  receive({type:'models',items:items.map(i=>({name:i.name,gateway:"gw",provider_group:"gw",upstream_id:i.description,api:"chat",enabled:true})),locked:false});d.querySelector('#model').dispatchEvent(new dom.window.MouseEvent('mousedown',{bubbles:true,cancelable:true}));assert.equal(d.activeElement,search);assert.equal(d.querySelectorAll('#picker-options [role=option]').length,50);
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
  receive({ type: "models", items: [{ name: "a", gateway: "gw", provider_group: "gw", upstream_id: "a", api: "chat", enabled: true }], locked: true });
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
  receive({ type: "commands", items: [{ name: "model", description: "Switch model", args_hint: "[name]" }, { name: "compact", description: "Compact", args_hint: "[focus]" }] });
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
  // Legacy history authors retain the same Captain presentation.
  receive({ type: "history", generation: 2, items: [{ type: "message", author: "driver", text: "Hello from history", id: "M", generation: 2 }] });
  assert.match(dom.window.document.querySelector("#transcript").textContent, /Hello from history/);
  assert.ok(dom.window.document.querySelector(".message.captain"));
  receive({ type: "permission", generation: 2, card: { requestId: "RA", callId: "C", tool: "bash", agent: "build", scope: { kind: "folder", value: "/a" }, reason: "Review", call: "pwd" } });
  [...dom.window.document.querySelectorAll("button")].find(button => button.textContent === "Once").click();
  assert.deepEqual(JSON.parse(JSON.stringify(posts.at(-1))), { type: "answerPermission", sessionId: "A", requestId: "RA", decision: "allow_once", generation: 2 });
  assert.ok(![...dom.window.document.querySelectorAll("button")].some(button => button.textContent === "Allow WithNav config for this session"));
  assert.ok(![...dom.window.document.querySelectorAll("button")].some(button => button.textContent.startsWith("Always")));
  receive({ type: "permission", generation: 2, card: { requestId: "RC", callId: "CC", tool: "write", agent: "build", scope: { kind: "folder", value: "/config" }, reason: "Review", call: "file", alwaysLabel: "Always (project): write *" } });
  [...dom.window.document.querySelectorAll("button")].find(button => button.textContent === "Always (project): write *").click();
  assert.equal(posts.at(-1).decision, "allow_always");
  receive({ type: "permission", generation: 2, card: { requestId: "RR", callId: "CR", tool: "bash", agent: "build", scope: { kind: "folder", value: "/a" }, reason: "Review", call: "pwd" } });
  const rejectCard = [...dom.window.document.querySelectorAll(".permission-card")].at(-1);
  rejectCard.querySelector("textarea").value = "Use a safer command";
  [...rejectCard.querySelectorAll("button")].find(button => button.textContent === "Reject").click();
  assert.equal(posts.at(-1).decision, "reject");
  assert.equal(posts.at(-1).message, "Use a safer command");
  assert.deepEqual(errors, []);
  dom.window.close();
});

function modelPicker(items){
  const page=keyboardPage();const {d,receive,key}=page;
  receive({type:'models',items:items.map(m=>({api:'chat',enabled:true,...m})),locked:false});d.querySelector('#model').focus();key('Enter');
  const css=d.createElement('style');css.textContent=fs.readFileSync('out/chat.css','utf8');d.head.append(css);
  return {...page,rows:()=>[...d.querySelectorAll('#picker-options > *')].map(e=>({text:e.matches('.picker-group')?e.textContent:e.title,level:e.dataset.level,kind:e.matches('.picker-group')?'group':'option',label:e.getAttribute('aria-label')}))};
}
const hierarchy=[
  {name:'omniroute/cc/claude-opus',gateway:'omniroute',provider_group:'cc',upstream_id:'cc/claude-opus'},
  {name:'omniroute/kimi/k2',gateway:'omniroute',provider_group:'kimi',upstream_id:'kimi/k2'},
  {name:'omniroute/kimi/fim',gateway:'omniroute',provider_group:'kimi',upstream_id:'kimi/fim',api:'fim'},
  {name:'direct/solo',gateway:'direct',provider_group:'direct',upstream_id:'solo'},
];
test('model picker levels: gateway 0, provider 1, model 2, direct models 1, no redundant provider row',()=>{
  const {dom,d,rows}=modelPicker(hierarchy);
  assert.deepEqual(rows(),[
    {text:'direct',level:'0',kind:'group',label:'direct, gateway'},
    {text:'direct/solo',level:'1',kind:'option',label:'direct/solo'},
    {text:'omniroute',level:'0',kind:'group',label:'omniroute, gateway'},
    {text:'cc',level:'1',kind:'group',label:'cc, provider in omniroute'},
    {text:'omniroute/cc/claude-opus',level:'2',kind:'option',label:'omniroute/cc/claude-opus'},
    {text:'kimi',level:'1',kind:'group',label:'kimi, provider in omniroute'},
    {text:'omniroute/kimi/k2',level:'2',kind:'option',label:'omniroute/kimi/k2'},
  ]);
  const sheet=[...d.styleSheets].flatMap(s=>[...s.cssRules]).map(r=>r.cssText).join('\n');
  assert.match(sheet,/--picker-indent: 12px/);assert.match(sheet,/\[data-level="1"\][^}]*margin-left: var\(--picker-indent\)/);assert.match(sheet,/\[data-level="2"\][^}]*margin-left: calc\(2 \* var\(--picker-indent\)\)/);
  for(const group of d.querySelectorAll('.picker-group'))assert.equal(group.tagName,'BUTTON');
  assert.equal(d.querySelector('#picker-options').getAttribute('role'),'listbox');assert.equal(d.querySelectorAll('[role=tree],[role=treeitem]').length,0);
  dom.window.close();
});
test('model picker search keeps levels, collapse hides children and group keys never post a model',()=>{
  const {dom,d,posts,rows,type}=modelPicker(hierarchy);const group=t=>[...d.querySelectorAll('.picker-group')].find(g=>g.textContent===t);
  const before=posts.length;
  group('kimi').focus();group('kimi').dispatchEvent(new dom.window.KeyboardEvent('keydown',{key:'Enter',bubbles:true,cancelable:true}));group('kimi').click();
  assert.equal(group('kimi').getAttribute('aria-expanded'),'false');assert.equal(group('kimi').dataset.level,'1');assert(!rows().some(r=>r.text==='omniroute/kimi/k2'));
  group('omniroute').dispatchEvent(new dom.window.KeyboardEvent('keydown',{key:' ',bubbles:true,cancelable:true}));group('omniroute').click();
  assert.deepEqual(rows().map(r=>r.text),['direct','direct/solo','omniroute']);assert.equal(posts.length,before);
  const search=d.querySelector('#command-palette .picker-search');search.focus();type('k2');
  assert.deepEqual(rows().map(r=>[r.text,r.level]),[['omniroute','0'],['kimi','1'],['omniroute/kimi/k2','2']]);
  d.activeElement.dispatchEvent(new dom.window.KeyboardEvent('keydown',{key:'Enter',bubbles:true,cancelable:true}));
  assert.deepEqual(JSON.parse(JSON.stringify(posts.at(-1))),{type:'selectModel',name:'omniroute/kimi/k2',generation:1});
  dom.window.close();
});
test('model picker alt+a folds every group, then unfolds them all',()=>{
  const {dom,d,rows}=modelPicker(hierarchy);
  const search=d.querySelector('#command-palette .picker-search');search.focus();
  const altA=()=>search.dispatchEvent(new dom.window.KeyboardEvent('keydown',{key:'a',code:'KeyA',altKey:true,bubbles:true,cancelable:true}));
  const all=rows();
  altA();
  assert.deepEqual(rows().map(r=>r.text),['direct','omniroute']);
  assert.match(d.querySelector('#command-palette .picker-count').textContent,/Alt\+A/);
  altA();
  assert.deepEqual(rows(),all);
  dom.window.close();
});
test('model picker long canonical names stay whole in the title and wrap inside the indented row',()=>{
  const long='omniroute/cc/'+'very-long-model-name-'.repeat(8);
  const {dom,d}=modelPicker([{name:long,gateway:'omniroute',provider_group:'cc',upstream_id:long.slice(10)}]);
  const option=d.querySelector('#picker-options [role=option]');assert.equal(option.title,long);assert.equal(option.getAttribute('aria-label'),long);assert.equal(option.querySelector('strong').textContent,long.slice('omniroute/cc/'.length));assert.equal(option.dataset.level,'2');
  assert.equal(dom.window.getComputedStyle(option.querySelector('strong')).overflowWrap,'anywhere');
  dom.window.close();
});

const ctrlF=(dom,target)=>{const e=new dom.window.KeyboardEvent('keydown',{key:'f',code:'KeyF',ctrlKey:true,bubbles:true,cancelable:true});target.dispatchEvent(e);return e;};
const optionTitles=d=>[...d.querySelectorAll('#picker-options [role=option]')].map(o=>o.title);
test('favorites section: first, unfoldable, starred, hidden when empty and filtered by search',()=>{
  const {dom,d,receive,rows,type}=modelPicker(hierarchy);
  assert(!d.querySelector('.picker-section'),'no favorites, no section');
  assert.equal(rows()[0].text,'direct');
  d.querySelector('#command-palette').hidden=true;
  receive({type:'favorites',items:['omniroute/kimi/k2','direct/solo','gone/model']});
  d.querySelector('#model').focus();dom.window.document.querySelector('#model').dispatchEvent(new dom.window.MouseEvent('mousedown',{bubbles:true,cancelable:true}));
  const heading=d.querySelector('#picker-options > .picker-section');
  assert.equal(heading.tagName,'H3');assert.equal(heading.textContent,'Favorites');assert.equal(heading.getAttribute('aria-label'),'Favorites, always open');
  assert.equal(heading,d.querySelector('#picker-options').firstElementChild,'first');
  assert.equal(heading.querySelector('button'),null,'no fold control');
  assert.equal(heading.hasAttribute('aria-expanded'),false);
  const titles=optionTitles(d);
  assert.deepEqual(titles.slice(0,2),['direct/solo','omniroute/kimi/k2'],'sorted by name, unknown ones hidden');
  assert.equal(titles.filter(t=>t==='direct/solo').length,2,'also kept in its group');
  assert(!titles.includes('gone/model'));
  const ids=[...d.querySelectorAll('#picker-options [role=option]')].map(o=>o.id);assert.equal(new Set(ids).size,ids.length);
  assert.equal(ids[0],'command-option-0');
  const stars=[...d.querySelectorAll('.favorite-toggle')];
  assert.equal(stars.length,titles.length,'a star on every model row');
  assert.deepEqual(stars.map(s=>s.getAttribute('aria-pressed')),titles.map(t=>String(['direct/solo','omniroute/kimi/k2'].includes(t))));
  assert.equal(stars[0].getAttribute('aria-label'),'Remove from favorites');
  assert.equal(stars.find(s=>s.getAttribute('aria-pressed')==='false').getAttribute('aria-label'),'Add to favorites');
  assert.match(d.querySelector('#command-palette .picker-count').textContent,/Ctrl\+F favorite/);
  // Alt+A folds every group; the section stays.
  const search=d.querySelector('#command-palette .picker-search');search.focus();
  search.dispatchEvent(new dom.window.KeyboardEvent('keydown',{key:'a',code:'KeyA',altKey:true,bubbles:true,cancelable:true}));
  assert.deepEqual(optionTitles(d),['direct/solo','omniroute/kimi/k2']);
  assert.deepEqual([...d.querySelectorAll('#picker-options .picker-group')].map(g=>g.getAttribute('aria-expanded')),['false','false']);
  // Search shows only the matching favorites; none matching hides the heading.
  type('k2');
  assert.deepEqual(optionTitles(d).slice(0,2),['omniroute/kimi/k2','omniroute/kimi/k2']);
  assert.equal(d.querySelectorAll('.picker-section').length,1);
  search.value='cc';search.dispatchEvent(new dom.window.Event('input'));
  assert(!d.querySelector('.picker-section'));assert.deepEqual(optionTitles(d),['omniroute/cc/claude-opus']);
  dom.window.close();
});
test('favorites: ctrl+f posts toggleFavorite for the highlighted model, keeps the highlight and heals when the last one goes',()=>{
  const {dom,d,posts,receive,key}=modelPicker(hierarchy);
  const search=d.querySelector('#command-palette .picker-search');search.focus();
  const active=()=>d.getElementById(search.getAttribute('aria-activedescendant')).title;
  key('ArrowDown');assert.equal(active(),'omniroute/cc/claude-opus');
  const before=posts.length;const e=ctrlF(dom,search);assert.equal(e.defaultPrevented,true);
  assert.deepEqual(JSON.parse(JSON.stringify(posts.at(-1))),{type:'toggleFavorite',name:'omniroute/cc/claude-opus',generation:1});assert.equal(posts.length,before+1);
  assert.equal(d.querySelector('.picker-section'),null,'nothing changes until the host answers');
  receive({type:'favorites',items:['omniroute/cc/claude-opus']});
  assert(d.querySelector('.picker-section'));assert.equal(active(),'omniroute/cc/claude-opus','highlight stays on the same model');
  assert.equal(search.getAttribute('aria-activedescendant'),'command-option-2','its row in its group');
  // Toggled from the section row: removing the last favorite hides the section and keeps a valid row.
  key('ArrowUp');key('ArrowUp');
  assert.equal(search.getAttribute('aria-activedescendant'),'command-option-0');assert.equal(active(),'omniroute/cc/claude-opus','the section row');
  ctrlF(dom,search);assert.equal(posts.at(-1).name,'omniroute/cc/claude-opus');
  receive({type:'favorites',items:[]});
  assert(!d.querySelector('.picker-section'));
  assert.equal(active(),'omniroute/cc/claude-opus');
  assert(d.getElementById(search.getAttribute('aria-activedescendant')));
  // ctrl+f never posts selectModel and Enter on a favorite row picks the model.
  receive({type:'favorites',items:['direct/solo']});
  key('ArrowUp');
  assert.equal(search.getAttribute('aria-activedescendant'),'command-option-0');
  key('Enter');assert.deepEqual(JSON.parse(JSON.stringify(posts.at(-1))),{type:'selectModel',name:'direct/solo',generation:1});
  dom.window.close();
});
test('favorites: the star toggles with the mouse without choosing the model and ctrl+f is ignored outside the model picker',()=>{
  const {dom,d,posts,receive}=modelPicker(hierarchy);
  const star=d.querySelector('#picker-options .favorite-toggle');
  const before=posts.length;star.click();
  assert.equal(posts.length,before+1);assert.equal(posts.at(-1).type,'toggleFavorite');assert.equal(posts.at(-1).name,'direct/solo');
  assert(!posts.some(p=>p.type==='selectModel'));assert.equal(d.querySelector('#command-palette').hidden,false,'picker stays open');
  assert.equal(star.tagName,'BUTTON');assert.equal(star.getAttribute('aria-label'),'Add to favorites');
  receive({type:'favorites',items:['direct/solo']});
  assert.equal(d.querySelector('#picker-options .favorite-toggle').getAttribute('aria-label'),'Remove from favorites');
  // Another picker kind has no stars and ignores ctrl+f.
  receive({type:'commandPicker',kind:'variant',items:[{name:'low',description:''}]});
  assert.equal(d.querySelectorAll('.favorite-toggle').length,0);
  const search=d.querySelector('#command-palette .picker-search');const n=posts.length;
  const e=ctrlF(dom,search);assert.equal(e.defaultPrevented,false);assert.equal(posts.length,n);
  dom.window.close();
});
test('favorites: the first-50 cut applies to the groups only and option ids stay aligned with rows',()=>{
  const many=Array.from({length:80},(_,i)=>({name:`gw/m${String(i).padStart(2,'0')}`,gateway:'gw',provider_group:'gw',upstream_id:`m${i}`}));
  const {dom,d,posts,receive,key}=modelPicker(many);
  receive({type:'favorites',items:['gw/m70','gw/m05']});
  d.querySelector('#command-palette').hidden=true;
  d.querySelector('#model').dispatchEvent(new dom.window.MouseEvent('mousedown',{bubbles:true,cancelable:true}));
  const titles=optionTitles(d);
  assert.deepEqual(titles.slice(0,2),['gw/m05','gw/m70']);assert.equal(titles.length,2+50);
  const search=d.querySelector('#command-palette .picker-search');search.focus();
  key('ArrowDown');key('ArrowDown');key('Enter');
  assert.equal(posts.at(-1).name,titles[2],'enter picks the row that is highlighted');
  dom.window.close();
});

test('model rows inside a group show the short name; favorites show the full name; title and accessible name stay canonical',()=>{
  const {dom,d,receive}=modelPicker(hierarchy);
  const visible=()=>[...d.querySelectorAll('#picker-options [role=option]')].map(o=>[o.querySelector('strong').textContent,o.title,o.getAttribute('aria-label')]);
  assert.deepEqual(visible(),[
    ['solo','direct/solo','direct/solo'],
    ['claude-opus','omniroute/cc/claude-opus','omniroute/cc/claude-opus'],
    ['k2','omniroute/kimi/k2','omniroute/kimi/k2'],
  ]);
  receive({type:'favorites',items:['omniroute/kimi/k2']});
  assert.deepEqual(visible(),[
    ['omniroute/kimi/k2','omniroute/kimi/k2',null],
    ['solo','direct/solo','direct/solo'],
    ['claude-opus','omniroute/cc/claude-opus','omniroute/cc/claude-opus'],
    ['k2','omniroute/kimi/k2','omniroute/kimi/k2'],
  ]);
  // Search still matches the canonical name.
  const search=d.querySelector('#command-palette .picker-search');search.value='omniroute/kimi';search.dispatchEvent(new dom.window.Event('input'));
  assert.deepEqual(visible().map(v=>v[1]),['omniroute/kimi/k2','omniroute/kimi/k2']);
  dom.window.close();
});
test('a model whose name does not start with its gateway/provider keeps its full name in a group',()=>{
  const {dom,d}=modelPicker([{name:'odd/name',gateway:'omniroute',provider_group:'cc',upstream_id:'x'},{name:'omniroute/cc/',gateway:'omniroute',provider_group:'cc',upstream_id:'y'}]);
  assert.deepEqual([...d.querySelectorAll('#picker-options [role=option] strong')].map(e=>e.textContent),['odd/name','omniroute/cc/']);
  dom.window.close();
});

test("subagent states, pauses and wake diffs carry their own colour hooks, with words beside them", () => {
  const { dom, d, receive } = keyboardPage();
  receive({ type: "conversationOpened", id: "S", title: "S" });
  receive({ type: "subagents", parentId: "S", items: [
    { id: "a", agent: "vigie", goal: "Look", state: "running" },
    { id: "b", agent: "lux", goal: "Weigh", state: "watchdog" },
    { id: "c", agent: "mousse", goal: "Rename", state: "failed" },
  ] });
  const states = [...d.querySelectorAll(".child-open .child-state")].map(s => [s.dataset.state, s.textContent]);
  assert.deepEqual(states, [["running", "Running"], ["watchdog", "In doubt · watchdog paused"], ["failed", "Failed"]]);
  assert.equal(d.querySelector(".child-open").textContent, "vigie · Running");
  receive({ type: "pause", id: "p", kind: "watchdog", text: "Stuck", token: { incarnation: "i", hold_id: "h" } });
  const pause = d.querySelector(".pause-card");
  assert.equal(pause.dataset.kind, "watchdog");
  receive({ type: "pauseState", id: "p", state: "resolved" });
  assert.equal(pause.dataset.state, "resolved");
  receive({ type: "message", id: "w", author: "nav", text: "Navigator noticed: drift in src/a.rs:3\n```diff\n@@ -1,2 +1,3 @@\n-old\n+new\n+more\n```" });
  const count = d.querySelector(".note-meta .diff-count");
  assert.equal(count.querySelector(".diff-added").textContent, "+2");
  assert.equal(count.querySelector(".diff-removed").textContent, "−1");
  assert.equal(count.title, "2 lines added, 1 removed");
  const css = fs.readFileSync("src/webview/chat.css", "utf8");
  for (const rule of [/\.child-state\[data-state=done\][^}]*--nav-passed-text/, /\.pause-card \.control-kicker[^}]*--nav-warning-text/, /\.diff-added, \.diff-line\.add[^}]*--nav-added-text/])
    assert.match(css, rule);
  assert.doesNotMatch(css.slice(css.indexOf("--nav-warning-text"), css.indexOf(".bypass-toggle")), /#[0-9a-f]{3,6}\b/i, "theme tokens only");
  dom.window.close();
});
test('GFM renders structural constructs safely and coalesces streaming frames', () => {
  const { dom, d, receive } = keyboardPage();
  const frames = []; dom.window.requestAnimationFrame = callback => frames.push(callback);
  receive({ type: 'delta', id: 'markdown', text: '| A | B | C |\n| :--- | :---: | ---: |\n| `code` | two | three |\n\n> quote\n\n---\n\n3. outer\n   - nested\n\n- [ ] pending\n- [x] done\n\n~~strike~~\n\n##### five\n\n###### six\n\nhard  \nbreak\n\n<img onerror="alert(1)">\n<script>alert(1)</script>\n\n[ok](https://example.com) [mail](mailto:a@example.com) [bad](javascript:alert)\n' });
  receive({ type: 'delta', id: 'markdown', text: '\nend' });
  assert.equal(frames.length, 1); frames.shift()();
  assert.equal(d.querySelectorAll('.message-body th').length, 3);
  assert.equal(d.querySelector('.message-body td code').textContent, 'code');
  for (const selector of ['blockquote', 'hr', 'ol[start="3"] ul', 'del', 'h5', 'h6', 'br']) assert.ok(d.querySelector('.message-body ' + selector), selector);
  assert.equal(d.querySelectorAll('.message-body input:disabled').length, 2);
  assert.equal(d.querySelector('.message-body img'), null);
  assert.equal(d.querySelector('.message-body script'), null);
  assert.ok(d.querySelector('.message-body').textContent.includes('<script>'));
  assert.equal(d.querySelectorAll('.message-body a').length, 2);
  dom.window.close();
});
test('answer file references post openFile but code fences and URLs do not', () => {
  const { dom, d, receive, posts } = keyboardPage();
  const frames = []; dom.window.requestAnimationFrame = callback => frames.push(callback);
  receive({ type: 'delta', id: 'files', text: '`vscode-withnav/src/editorContext.ts:29` and vscode-withnav/src/editorContext.ts:29\n\nsrc/a.rs src/a.rs:12:3 src/a.rs#L12 ./src/a.rs /workspace/src/a.rs\n\nhttps://example.com/src/a.rs e.g. v0.7.1 1.5\n\n```\nsrc/fenced.rs:2\n```' });
  frames.shift()();
  const buttons = [...d.querySelectorAll('.message-body .file-link')];
  assert.equal(buttons.length, 7);
  assert.ok(buttons[0].closest('code'));
  assert.equal(buttons[0].title, 'Open vscode-withnav/src/editorContext.ts at line 29');
  buttons[0].click();
  assert.deepEqual(JSON.parse(JSON.stringify(posts.at(-1))), { type: 'openFile', path: 'vscode-withnav/src/editorContext.ts', line: 29, generation: 1 });
  buttons[3].click();
  assert.equal(posts.at(-1).line, 12); assert.equal(posts.at(-1).path, 'src/a.rs');
  assert.equal(d.querySelector('.message-body pre .file-link'), null);
  dom.window.close();
});
