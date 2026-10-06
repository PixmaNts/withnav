const test = require('node:test');
const assert = require('node:assert/strict');
const fs = require('node:fs');
const Module = require('node:module');
const { JSDOM } = require('jsdom');
const { SettingsHost } = require('../out/tests/settingsHost');
const { settingsHtml } = require('../out/tests/settingsHtml');
const { buildSections } = require('../out/tests/webview/settingsModel');
const field = (key,value) => ({key,value,default:null,set:true,error:null,applies:'new_sessions'});
const settings = (revision='one') => ({revision,paths:{config:'/work/config.json',models:'/work/models.json',agents:[]},fields:[field('layers.reflex',{model:'gw/fim'}),field('agents.build',{model:'gw/chat'}),field('permissions.automatic_review',true),field('permissions.scratch_dirs',[]),field('subagents.max_concurrent',4)],gateways:[]});
const json = (body,status=200) => new Response(JSON.stringify(body),{status});
function setup(request,options={workspace:'/work'}) {
  const messages=[],calls=[];
  return {messages,calls,host:new SettingsHost({request:async(p,i)=>{calls.push({p,i});return request(p,i);}},m=>messages.push(structuredClone(m)),options)};
}
function view(snapshot) {
  const sent=[],dom=new JSDOM(settingsHtml('abc','settings.js','settings.css','https://host'),{runScripts:'outside-only'});
  dom.window.acquireVsCodeApi=()=>({postMessage:m=>sent.push(m),getState:()=>({section:'permissions'}),setState(){}});
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
  const bad=[['permissions.automatic_review',1],['subagents.max_concurrent',1.5],['subagents.watchdog_interval_secs',Infinity],['agent.watchdog_interval_secs','5'],['permissions.scratch_dirs',['/tmp',42]],['layers.reflex','gw/fim'],['layers.instinct',[]],['agents.build',{model:'gw/chat',variant:1}],['agents.plan',{variant:null}],['default_agent',false],['tui.theme',{}]];
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
test('busy navigation and search re-renders keep all controls disabled',()=>{
  const v=view({settings:settings(),models:[],agents:[],layers:[]});v.deliver({type:'saving',key:'permissions.automatic_review'});
  assert.ok([...v.document.querySelectorAll('#rows input, #rows select, #rows button')].every(c=>c.disabled));
  v.document.querySelector('[data-section="subagents"]').click();assert.ok([...v.document.querySelectorAll('#rows input')].every(c=>c.disabled));
  const search=v.document.querySelector('#search');search.value='model';search.dispatchEvent(new v.dom.window.Event('input'));assert.ok([...v.document.querySelectorAll('#rows select')].every(c=>c.disabled));
  v.deliver({type:'saved',key:'permissions.automatic_review'});assert.ok([...v.document.querySelectorAll('#rows select')].some(c=>!c.disabled));v.dom.window.close();
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
  const v=view();v.deliver(messages[0]);assert.match(v.document.querySelector('#rows').textContent,/Open a folder to edit Navai settings/);v.document.querySelector('#rows button').click();assert.equal(v.sent.at(-1).type,'openFolder');v.dom.window.close();
});
test('panel disposal cancels pending endpoint waits and Open Folder runs its command',async()=>{
  const original=Module._load;let open,receive,dispose,signal;const commands=[];
  const vscode={workspace:{workspaceFolders:undefined},Uri:{joinPath:()=>'/asset'},ViewColumn:{Active:1},Disposable:class{constructor(fn){this.dispose=fn;}},commands:{registerCommand:(_,fn)=>{open=fn;return{dispose(){}};},executeCommand:name=>commands.push(name)},window:{createWebviewPanel:()=>({webview:{asWebviewUri:x=>x,cspSource:'https://host',postMessage(){},onDidReceiveMessage:fn=>{receive=fn;return{dispose(){}};}},onDidDispose:fn=>dispose=fn}),setStatusBarMessage(){}}};
  Module._load=function(id,...args){return id==='vscode'?vscode:original.call(this,id,...args);};let registerSettingsView;try{({registerSettingsView}=require('../out/tests/settingsView'));}finally{Module._load=original;}
  const registration=registerSettingsView({extensionUri:'/extension',extension:{id:'navai'}},{settingsRequest:(_p,_i,s)=>{signal=s;return new Promise((_resolve,reject)=>s.addEventListener('abort',()=>reject(Error('cancelled'))));}});
  open();receive({type:'openFolder'});assert.equal(commands[0],'workbench.action.files.openFolder');dispose();vscode.workspace.workspaceFolders=[{uri:{fsPath:'/work'}}];open();receive({type:'ready'});await new Promise(r=>setImmediate(r));assert.ok(signal);dispose();assert.equal(signal.aborted,true);await new Promise(r=>setImmediate(r));registration.dispose();
});
test('endpoint waits cancel cleanly and time out with a Retry state',async t=>{
  const original=Module._load;Module._load=function(id,...args){return id==='vscode'?{}:original.call(this,id,...args);};let CognitionBackend;try{({CognitionBackend}=require('../out/tests/cognition'));}finally{Module._load=original;}
  const backend=Object.create(CognitionBackend.prototype);t.mock.timers.enable({apis:['setTimeout','setInterval']});const controller=new AbortController();const cancelled=backend.waitForEndpoint(controller.signal);controller.abort();await assert.rejects(cancelled,/cancelled/);
  const timed=backend.waitForEndpoint();const rejected=assert.rejects(timed,/Navai's language server is not running/);t.mock.timers.tick(30000);await rejected;
  const v=view();v.deliver({type:'loadError',message:"Navai's language server is not running"});assert.match(v.document.querySelector('#rows').textContent,/Retry/);v.document.querySelector('#rows button').click();assert.equal(v.sent.at(-1).type,'reload');v.dom.window.close();
});
