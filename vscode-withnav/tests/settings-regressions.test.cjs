const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const Module = require('node:module');
const { JSDOM } = require('jsdom');
const { SettingsHost } = require('../out/tests/settingsHost');
const { settingsHtml } = require('../out/tests/settingsHtml');
const { buildSections } = require('../out/tests/webview/settingsModel');
const field = (key,value) => ({key,value,default:null,set:true,error:null,applies:'new_sessions'});
const settings = (revision='one') => ({revision,paths:{config:'/work/config.json',models:'/work/models.json',agents:[]},fields:[field('layers.reflex',{model:'gw/fim'}),field('agents.build',{model:'gw/chat'}),field('permissions.scratch_dirs',[]),field('subagents.max_concurrent',4)],gateways:[]});
const json = (body,status=200) => new Response(JSON.stringify(body),{status});
function setup(request,options={workspace:'/work'}) {
  const messages=[],calls=[];
  return {messages,calls,host:new SettingsHost({request:async(p,i)=>{calls.push({p,i});return request(p,i);}},m=>messages.push(structuredClone(m)),options)};
}
function view(snapshot,onPost,section='permissions') {
  const sent=[],dom=new JSDOM(settingsHtml('abc','settings.js','settings.css','https://host'),{runScripts:'outside-only'});
  dom.window.acquireVsCodeApi=()=>({postMessage:m=>{sent.push(m);onPost?.(m);},getState:()=>({section}),setState(){}});
  dom.window.eval(fs.readFileSync(require.resolve('../out/settings.js'),'utf8'));
  const deliver=data=>dom.window.dispatchEvent(new dom.window.MessageEvent('message',{data}));
  if(snapshot)deliver({type:'snapshot',data:snapshot});
  return {dom,document:dom.window.document,deliver,sent};
}
test('host sanitizes gateway URLs in loaded and saved snapshots',async()=>{
  const body=settings();body.gateways=[{name:'secret',base_url:'https://user:password@example.org/api?token=secret#secret'},{name:'invalid',base_url:'user:password@ bad'}];
  const {host,messages}=setup(p=>json(p.startsWith('/v1/settings')?body:{}));
  await host.handle({type:'ready'});await host.handle({type:'save',key:'tui.theme',value:'ayu-light'});
  for(const m of messages.filter(m=>m.type==='snapshot'))assert.deepEqual(m.data.settings.gateways.map(g=>g.base_url),['https://example.org/api','(invalid URL)']);
  assert.doesNotMatch(JSON.stringify(messages),/password|token=secret/);
});
test('host rejects each invalid per-key wire shape before HTTP and permits null',async()=>{
  const {host,messages,calls}=setup(()=>json(settings()));
  const bad=[['permissions.scratch_dirs','/tmp'],['subagents.max_concurrent',1.5],['subagents.watchdog_interval_secs',Infinity],['agent.watchdog_interval_secs','5'],['permissions.scratch_dirs',['/tmp',42]],['layers.reflex','gw/fim'],['layers.instinct',[]],['agents.build',{model:'gw/chat',variant:1}],['agents.plan',{variant:null}],['default_agent',false],['tui.theme',{}]];
  for(const [key,value] of bad)await host.handle({type:'save',key,value});
  assert.equal(calls.length,0);assert.equal(messages.filter(m=>m.type==='invalid').length,bad.length);
  await host.handle({type:'ready'});for(const [key] of bad)await host.handle({type:'save',key,value:null});
  assert.equal(calls.filter(c=>c.i.method==='PUT').length,bad.length);
});
test('settings render before side loads finish and survive models and agents failures',async()=>{
  let release;const pending=new Promise(r=>release=r);
  const {host,messages}=setup(async p=>{if(p.startsWith('/v1/settings'))return json(settings());if(p.startsWith('/v1/layers'))return json({});await pending;throw Error('offline');});
  const load=host.handle({type:'ready'});await new Promise(r=>setImmediate(r));assert.equal(messages[0].type,'snapshot');release();await load;
  const snapshot=host.current();assert.match(snapshot.notes.models,/offline/);assert.match(snapshot.notes.agents,/offline/);
  const sections=buildSections(snapshot);assert.ok(sections.find(s=>s.id==='agent:build'));
  assert.deepEqual(sections.find(s=>s.id==='layer:reflex').rows[0].control.options.find(o=>o.value==='gw/fim'),{value:'gw/fim',label:'gw/fim'});
  const v=view(snapshot);v.document.querySelector('[data-section="layer:reflex"]').click();assert.match(v.document.querySelector('#rows').textContent,/Cannot read models/);assert.equal(v.document.querySelector('select').value,'gw/fim');v.dom.window.close();
});
test('delayed projections preserve Add Item draft, focus and caret, then commit saves',async()=>{
  let release,host;const pending=new Promise(r=>release=r),saves=[],puts=[];
  const v=view(undefined,m=>{if(m.type==='save')saves.push(host.handle(m));});
  host=new SettingsHost({request:async(p,i)=>{
    if(p.startsWith('/v1/settings')){
      const body=settings();
      if(i.method==='PUT'){const write=JSON.parse(i.body);puts.push(write);body.fields.find(f=>f.key===write.key).value=write.value;}
      return json(body);
    }
    await pending;return json({});
  }},m=>v.deliver(structuredClone(m)),{workspace:'/work'});
  const load=host.handle({type:'ready'});await new Promise(r=>setImmediate(r));
  v.document.querySelector('.list button').click();
  const input=v.document.querySelector('.list-edit input');input.value='/srv/scratch';input.setSelectionRange(4,8,'backward');
  release();await load;
  assert.equal(v.document.querySelector('.list-edit input'),input);assert.equal(input.value,'/srv/scratch');
  assert.equal(v.document.activeElement,input);assert.equal(input.selectionStart,4);assert.equal(input.selectionEnd,8);assert.equal(input.selectionDirection,'backward');
  input.dispatchEvent(new v.dom.window.KeyboardEvent('keydown',{key:'Enter'}));await Promise.all(saves);
  assert.deepEqual(puts,[{revision:'one',key:'permissions.scratch_dirs',value:['/srv/scratch']}]);
  assert.equal(v.document.querySelector('.list-edit'),null);assert.match(v.document.querySelector('.list').textContent,/\/srv\/scratch/);v.dom.window.close();
});
test('delayed projections keep the Add Item draft through blur and change, then OK saves',async()=>{
  let release,host;const pending=new Promise(r=>release=r),saves=[],puts=[];
  const v=view(undefined,m=>{if(m.type==='save')saves.push(host.handle(m));});
  host=new SettingsHost({request:async(p,i)=>{
    if(p.startsWith('/v1/settings')){
      const body=settings();
      if(i.method==='PUT'){const write=JSON.parse(i.body);puts.push(write);body.fields.find(f=>f.key===write.key).value=write.value;}
      return json(body);
    }
    await pending;return json({});
  }},m=>v.deliver(structuredClone(m)),{workspace:'/work'});
  const load=host.handle({type:'ready'});await new Promise(r=>setImmediate(r));
  v.document.querySelector('.list button').click();
  const input=v.document.querySelector('.list-edit input');input.value='/srv/scratch';
  release();await load;
  // Leaving the input for the OK button: the browser fires change on blur before the click.
  input.dispatchEvent(new v.dom.window.Event('change',{bubbles:true}));
  input.dispatchEvent(new v.dom.window.FocusEvent('focusout',{bubbles:true}));
  await new Promise(r=>setImmediate(r));
  const ok=[...v.document.querySelectorAll('.list-edit button')].find(b=>b.textContent==='OK');
  assert.ok(ok&&ok.isConnected,'the OK button is still in the page after blur');
  ok.click();await Promise.all(saves);
  assert.deepEqual(puts,[{revision:'one',key:'permissions.scratch_dirs',value:['/srv/scratch']}]);
  v.dom.window.close();
});
test('delayed projections preserve number draft and focus, then change saves',async()=>{
  let release,host;const pending=new Promise(r=>release=r),saves=[],puts=[];
  const v=view(undefined,m=>{if(m.type==='save')saves.push(host.handle(m));},'subagents');
  host=new SettingsHost({request:async(p,i)=>{
    if(p.startsWith('/v1/settings')){
      const body=settings();
      if(i.method==='PUT'){const write=JSON.parse(i.body);puts.push(write);body.fields.find(f=>f.key===write.key).value=write.value;}
      return json(body);
    }
    await pending;return json({});
  }},m=>v.deliver(structuredClone(m)),{workspace:'/work'});
  const load=host.handle({type:'ready'});await new Promise(r=>setImmediate(r));
  const input=v.document.querySelector('input[type="number"]');assert.equal(input.value,'4');input.focus();input.value='12';
  // Native number inputs expose no selection API; retaining the same focused
  // DOM node preserves the browser's internal caret rather than restoring it.
  const caret=[input.selectionStart,input.selectionEnd];release();await load;
  assert.equal(v.document.querySelector('input[type="number"]'),input);assert.equal(input.value,'12');assert.equal(v.document.activeElement,input);
  assert.deepEqual([input.selectionStart,input.selectionEnd],caret);
  input.dispatchEvent(new v.dom.window.Event('change',{bubbles:true}));await Promise.all(saves);
  assert.deepEqual(puts,[{revision:'one',key:'subagents.max_concurrent',value:12}]);v.dom.window.close();
});
test('409 authoritative reload replaces an in-progress Add Item draft',async()=>{
  const v=view();let reads=0,replaced=false;
  const host=new SettingsHost({request:async(p,i)=>{
    if(!p.startsWith('/v1/settings'))return json({});
    if(i.method==='PUT')return json({error:'stale'},409);
    const body=settings(`read-${++reads}`);if(reads===2)body.fields.find(f=>f.key==='permissions.scratch_dirs').value=['/external'];return json(body);
  }},m=>{
    // Isolate the reload from the separate saving notification, which also
    // renders: the authoritative snapshot itself must discard the draft.
    if(m.type==='saving')return;
    if(m.type==='snapshot'&&m.data.settings.revision==='read-2'&&!m.projectionOnly){assert.equal(v.document.querySelector('.list-edit input').value,'/srv/scratch');replaced=true;}
    v.deliver(structuredClone(m));
  },{workspace:'/work'});
  await host.handle({type:'ready'});v.document.querySelector('.list button').click();v.document.querySelector('.list-edit input').value='/srv/scratch';
  await host.handle({type:'save',key:'subagents.max_concurrent',value:5});
  assert.equal(replaced,true);assert.equal(v.document.querySelector('.list-edit'),null);assert.match(v.document.querySelector('.list').textContent,/\/external/);assert.match(v.document.querySelector('.banner').textContent,/changed elsewhere/);v.dom.window.close();
});
test('busy navigation and search re-renders keep all controls disabled',()=>{
  const v=view({settings:settings(),models:[],agents:[],layers:[]});v.deliver({type:'saving',key:'subagents.max_concurrent'});
  assert.ok([...v.document.querySelectorAll('#rows input, #rows select, #rows button')].every(c=>c.disabled));
  v.document.querySelector('[data-section="subagents"]').click();assert.ok([...v.document.querySelectorAll('#rows input')].every(c=>c.disabled));
  const search=v.document.querySelector('#search');search.value='model';search.dispatchEvent(new v.dom.window.Event('input'));assert.ok([...v.document.querySelectorAll('#rows select')].every(c=>c.disabled));
  v.deliver({type:'saved',key:'subagents.max_concurrent'});assert.ok([...v.document.querySelectorAll('#rows select')].some(c=>!c.disabled));v.dom.window.close();
});
test('a second agent edit is impossible until the first save replies, so model A never returns',()=>{
  const models=['gw/A','gw/B'].map(name=>({name,api:'chat',gateway:'gw',enabled:true,variants:['low','high'],default_variant:'low'}));
  const data=settings();data.fields.find(f=>f.key==='agents.build').value={model:'gw/A',variant:'low'};
  const v=view({settings:data,models,agents:[],layers:[]},undefined,'agent:build');
  const select=title=>[...v.document.querySelectorAll('.row')].find(r=>r.querySelector('.row-name').textContent===title).querySelector('select');
  const model=select('Model'),variant=select('Variant');
  model.value='gw/B';model.dispatchEvent(new v.dom.window.Event('change',{bubbles:true}));
  // No host reply yet: every control is already locked and the stale variant select is gone or disabled.
  assert.ok([...v.document.querySelectorAll('#rows input, #rows select, #rows button')].every(c=>c.disabled));
  variant.value='high';variant.dispatchEvent(new v.dom.window.Event('change',{bubbles:true}));
  const saves=v.sent.filter(m=>m.type==='save');
  assert.deepEqual(JSON.parse(JSON.stringify(saves.map(m=>m.value))),[{model:'gw/B',variant:'low'}]);
  assert.ok(!saves.some(m=>m.value.model==='gw/A'));
  v.deliver({type:'saving',key:'agents.build'});v.deliver({type:'saved',key:'agents.build'});
  assert.ok([...v.document.querySelectorAll('#rows select')].some(c=>!c.disabled));v.dom.window.close();
});
test('a key with two outstanding saves stays disabled until both replies arrive',()=>{
  const v=view({settings:settings(),models:[],agents:[],layers:[]});
  const controls=()=>[...v.document.querySelectorAll('#rows input, #rows select, #rows button')];
  v.deliver({type:'saving',key:'subagents.max_concurrent'});v.deliver({type:'saving',key:'subagents.max_concurrent'});
  v.deliver({type:'saved',key:'subagents.max_concurrent'});assert.ok(controls().every(c=>c.disabled));
  v.deliver({type:'saved',key:'subagents.max_concurrent'});assert.ok(controls().some(c=>!c.disabled));
  // Locally posted saves are counted once even though the host echoes each with `saving`.
  v.document.querySelector('[data-section="subagents"]').click();
  const box=v.document.querySelector('#rows input[type="number"]');box.value='7';box.dispatchEvent(new v.dom.window.Event('change',{bubbles:true}));
  assert.ok(controls().every(c=>c.disabled));v.deliver({type:'saving',key:'subagents.max_concurrent'});
  v.deliver({type:'saved',key:'subagents.max_concurrent'});assert.ok(controls().some(c=>!c.disabled));v.dom.window.close();
});
test('agents refresh after assignment and default-agent saves with non-fatal failures',async()=>{
  let generation=0;const {host}=setup((p,i)=>{if(p.startsWith('/v1/settings')){if(i.method==='PUT')generation++;return json(settings());}if(p.startsWith('/v1/agents'))return generation===2?json({error:'offline'},500):json({agents:[{id:'build',effective:{model:`gw/${generation}`,source:'config'}}]});return json({});});
  await host.handle({type:'ready'});await host.handle({type:'save',key:'agents.build',value:{model:'gw/new'}});assert.equal(host.current().agents[0].effective.model,'gw/1');
  await host.handle({type:'save',key:'default_agent',value:'plan'});assert.match(host.current().notes.agents,/offline/);assert.equal(host.current().settings.revision,'one');
});
test('real 409 reloads and drops queued saves; real 400 renders a row error',async()=>{
  let puts=0,reads=0;const {host,messages}=setup((p,i)=>{if(!p.startsWith('/v1/settings'))return json({});if(i.method==='GET')return json(settings(`read-${++reads}`));puts++;return json(puts===1?{error:'stale'}:{key:'permissions.scratch_dirs',error:'Absolute paths only'},puts===1?409:400);});
  await host.handle({type:'ready'});await Promise.all([host.handle({type:'save',key:'tui.theme',value:'ayu-light'}),host.handle({type:'save',key:'default_agent',value:'plan'})]);assert.equal(puts,1);assert.equal(host.current().settings.revision,'read-2');assert.ok(messages.some(m=>m.type==='stale'));
  await host.handle({type:'save',key:'permissions.scratch_dirs',value:['relative']});const v=view(host.current());v.deliver(messages.find(m=>m.type==='invalid'));assert.match(v.document.querySelector('.row-error').textContent,/Absolute paths only/);v.dom.window.close();
});
test('no workspace makes no requests and provides Open Folder',async()=>{
  const {host,messages,calls}=setup(()=>{throw Error('must not request');},{});await host.handle({type:'ready'});await host.handle({type:'save',key:'default_agent',value:'build'});assert.equal(calls.length,0);
  const v=view();v.deliver(messages[0]);assert.match(v.document.querySelector('#rows').textContent,/Open a folder to edit WithNav settings/);v.document.querySelector('#rows button').click();assert.equal(v.sent.at(-1).type,'openFolder');v.dom.window.close();
});
test('panel disposal cancels pending endpoint waits and Open Folder runs its command',async()=>{
  const original=Module._load;let open,receive,dispose,signal;const commands=[];
  const vscode={workspace:{workspaceFolders:undefined},Uri:{joinPath:()=>'/asset'},ViewColumn:{Active:1},Disposable:class{constructor(fn){this.dispose=fn;}},commands:{registerCommand:(_,fn)=>{open=fn;return{dispose(){}};},executeCommand:name=>commands.push(name)},window:{createWebviewPanel:()=>({webview:{asWebviewUri:x=>x,cspSource:'https://host',postMessage(){},onDidReceiveMessage:fn=>{receive=fn;return{dispose(){}};}},onDidDispose:fn=>dispose=fn}),setStatusBarMessage(){}}};
  Module._load=function(id,...args){return id==='vscode'?vscode:original.call(this,id,...args);};let registerSettingsView;try{({registerSettingsView}=require('../out/tests/settingsView'));}finally{Module._load=original;}
  const registration=registerSettingsView({extensionUri:'/extension',extension:{id:'withnav'}},{settingsRequest:(_p,_i,s)=>{signal=s;return new Promise((_resolve,reject)=>s.addEventListener('abort',()=>reject(Error('cancelled'))));}});
  open();receive({type:'openFolder'});assert.equal(commands[0],'workbench.action.files.openFolder');dispose();vscode.workspace.workspaceFolders=[{uri:{fsPath:'/work'}}];open();receive({type:'ready'});await new Promise(r=>setImmediate(r));assert.ok(signal);dispose();assert.equal(signal.aborted,true);await new Promise(r=>setImmediate(r));registration.dispose();
});
test('endpoint waits cancel cleanly and time out with a Retry state',async t=>{
  const original=Module._load;Module._load=function(id,...args){return id==='vscode'?{}:original.call(this,id,...args);};let CognitionBackend;try{({CognitionBackend}=require('../out/tests/cognition'));}finally{Module._load=original;}
  const backend=Object.create(CognitionBackend.prototype);t.mock.timers.enable({apis:['setTimeout','setInterval']});const controller=new AbortController();const cancelled=backend.waitForEndpoint(controller.signal);controller.abort();await assert.rejects(cancelled,/cancelled/);
  const timed=backend.waitForEndpoint();const rejected=assert.rejects(timed,/WithNav's language server is not running/);t.mock.timers.tick(30000);await rejected;
  const v=view();v.deliver({type:'loadError',message:"WithNav's language server is not running"});assert.match(v.document.querySelector('#rows').textContent,/Retry/);v.document.querySelector('#rows button').click();assert.equal(v.sent.at(-1).type,'reload');v.dom.window.close();
});
