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
