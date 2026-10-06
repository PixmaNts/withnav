const { test } = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const { JSDOM } = require("jsdom");

function keyboardPage(){
  const {chatHtml}=require('../out/tests/chatHtml.js');const dom=new JSDOM(chatHtml('n','chat.js','chat.css','resource:'),{runScripts:'outside-only'});const posts=[];
  dom.window.acquireVsCodeApi=()=>({postMessage:m=>posts.push(m),getState(){},setState(){}});dom.window.eval(fs.readFileSync('out/chat.js','utf8'));
  const d=dom.window.document;const receive=data=>dom.window.dispatchEvent(new dom.window.MessageEvent('message',{data:{...data,generation:1}}));
  receive({type:'conversationOpened',id:'S',title:'S'});receive({type:'status',state:'ready'});
  const key=(key,extra={})=>{const target=d.activeElement;const event=new dom.window.KeyboardEvent('keydown',{key,bubbles:true,cancelable:true,...extra});target.dispatchEvent(event);if(!event.defaultPrevented&&key.length===1){target.value+=key;target.dispatchEvent(new dom.window.Event('input',{bubbles:true}));}};
  return {dom,d,posts,receive,key,type:text=>{for(const c of text)key(c);}};
}
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
  receive({ type: "history", generation: 2, items: [{ type: "message", author: "driver", text: "Hello from history", id: "M", generation: 2 }] });
  assert.match(dom.window.document.querySelector("#transcript").textContent, /Hello from history/);
  receive({ type: "permission", generation: 2, card: { requestId: "RA", callId: "C", tool: "bash", agent: "build", scope: { kind: "folder", value: "/a" }, reason: "Review", call: "pwd", allowFolder: false } });
  [...dom.window.document.querySelectorAll("button")].find(button => button.textContent === "Allow once").click();
  assert.deepEqual(JSON.parse(JSON.stringify(posts.at(-1))), { type: "answerPermission", sessionId: "A", requestId: "RA", decision: "allow_once", generation: 2 });
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
    {text:'direct/solo',level:'1',kind:'option',label:null},
    {text:'omniroute',level:'0',kind:'group',label:'omniroute, gateway'},
    {text:'cc',level:'1',kind:'group',label:'cc, provider in omniroute'},
    {text:'omniroute/cc/claude-opus',level:'2',kind:'option',label:null},
    {text:'kimi',level:'1',kind:'group',label:'kimi, provider in omniroute'},
    {text:'omniroute/kimi/k2',level:'2',kind:'option',label:null},
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
  const option=d.querySelector('#picker-options [role=option]');assert.equal(option.title,long);assert.equal(option.querySelector('strong').textContent,long);assert.equal(option.dataset.level,'2');
  assert.equal(dom.window.getComputedStyle(option.querySelector('strong')).overflowWrap,'anywhere');
  dom.window.close();
});
