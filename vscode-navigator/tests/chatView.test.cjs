const { test } = require("node:test");
const assert = require("node:assert/strict");
const Module = require("node:module");
const originalLoad = Module._load;
const noopDisposable = () => ({ dispose() {} });
let provider;
const vscode = {
  ViewColumn: { Active: 1 },
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

test('S1 real watchdog doubt continues with null; S3 confirmation revision rejects changed child; S4 late acceptance retains done', async t => {
  const {b,event,dom,tick}=fullChat(t);b.state.session='P';b.scheduleChildRead=()=>{};
  event('subagent.spawned',{parent_session_id:'P',child_session_id:'C',agent:'plan',goal:'work'});
  event('subagent.watchdog',{parent_session_id:'P',child_session_id:'C',verdict:'doubt'});
  let confirm;vscode.window.showWarningMessage=()=>new Promise(r=>confirm=r);
  let finish;const calls=[];t.mock.method(globalThis,'fetch',(url,init)=>{calls.push({url,...init});return new Promise(r=>finish=r);});
  const continueButton=()=>[...dom.window.document.querySelectorAll('.child-row button')].find(x=>x.textContent==='Continue');
  continueButton().click();event('subagent.done',{parent_session_id:'P',child_session_id:'C',outcome:'done'});confirm('Confirm');await tick();assert.equal(calls.length,0);
  b.state.children.get('C').state='watchdog';b.publishSubagents();
  vscode.window.showWarningMessage=async()=> 'Confirm';continueButton().click();await tick();
  assert.deepEqual(JSON.parse(calls[0].body),{pause_token:null});
  event('subagent.done',{parent_session_id:'P',child_session_id:'C',outcome:'done'});finish(new Response(JSON.stringify({accepted:true})));await tick();
  assert.equal(b.state.children.get('C').state,'done');assert.equal(continueButton(),undefined);
});
test('S2 panel inspection preserves pending parent send, skill refusal and model result; S9 parent snapshot stays', async t => {
  const {b,event,dom,tick,send,panels,prompt}=fullChat(t);b.state.session='P';b.scheduleChildRead=()=>{};
  event('subagent.spawned',{parent_session_id:'P',child_session_id:'C',agent:'plan',goal:'work'});
  const generation=b.currentGeneration();let finish;
  t.mock.method(globalThis,'fetch',(url)=>url.endsWith('/messages')?new Promise(r=>finish=r):Promise.resolve(new Response(JSON.stringify({parent_session_id:'P',promptable:false,child_status:'done',agent:'plan'}))));
  send('hello');dom.window.document.querySelector('.child-open').click();await tick();await tick();
  assert.equal(panels.length,1);assert.equal(b.currentGeneration(),generation);assert.equal(b.currentConversationId(),'P');
  finish(new Response(JSON.stringify({accepted:true})));await tick();assert.equal(prompt.value,'');
  assert.equal(panels[0].page.window.document.querySelector('#composer').hidden,true);
  const snapshot={incarnation:'I',revision:1,estimate_total:42,window:1000,breakdown:{system:1,rules:1,skills:1,tools:1,messages:38}};b.state.context=snapshot;b.emit({type:'usage',snapshot});
  const label=dom.window.document.querySelector('#usage-label').textContent;
  panels[0].send({type:'send',text:'forged',generation});await tick();assert.equal(prompt.value,'');
  assert.equal(dom.window.document.querySelector('#usage-label').textContent,label);
  send('/skill review check');await tick();finish(new Response(JSON.stringify({error:'Skill refused'}),{status:400}));await tick();assert.match(dom.window.document.querySelector('.command-error').textContent,/Skill refused/);
  b.state.commands.set('M',{draft:'/model next',name:'model',arguments:'next'});event('command.result',{operation_id:'M',name:'model',ok:false,error:'Model refused'});
  assert.match(dom.window.document.querySelector('.command-error').textContent,/Model refused/);
  panels[0].panel.dispose();
});
test('S5 panel replay cannot regress seq100 budget row; S6 terminal metadata seeds done; S8 panel disposal cancels stream', async t=>{
  const {b,event,dom,tick,panels}=fullChat(t);b.state.session='P';b.scheduleChildRead=()=>{};
  event('subagent.spawned',{parent_session_id:'P',child_session_id:'C',agent:'plan',goal:'work'});
  b.state.children.get('C').state='budget';b.state.childStateSeq.set('C',100);
  let controller;let signal;const encoder=new TextEncoder();
  t.mock.method(globalThis,'fetch',async(url,init)=>{
    if(url.endsWith('/events')){signal=init.signal;return new Response(new ReadableStream({start(c){controller=c;signal.addEventListener('abort',()=>c.close());}}));}
    return new Response(JSON.stringify({parent_session_id:'P',promptable:false,agent:'plan',child_status:'out_of_steps'}));
  });
  dom.window.document.querySelector('.child-open').click();await tick();await tick();
  controller.enqueue(encoder.encode('id: 10\ndata: '+JSON.stringify({seq:10,kind:'turn.start',payload:{turn_id:'T'}})+'\n\n'));await tick();
  assert.equal(b.state.children.get('C').state,'budget');
  panels[0].panel.dispose();assert.equal(signal.aborted,true);
  t.mock.method(globalThis,'fetch',async()=>new Response(JSON.stringify({sessions:[{id:'C',parent_session_id:'P',promptable:false,child_status:'done'}]})));
  delete b.listConversations;await b.listConversations();assert.equal(b.state.children.get('C').state,'done');
});
test('S7 two child reader EOFs both retry and resync clears cursor; S8 chat disposal cancels readers', async t=>{
  const {b,event,tick}=fullChat(t);b.state.session='P';
  const counts={};const encoder=new TextEncoder();
  t.mock.method(globalThis,'fetch',async(url,init)=>{
    const id=url.includes('/C/')?'C':'D';counts[id]=(counts[id]??0)+1;
    if(counts[id]===1)return new Response('event: resync\ndata: {}\n\n');
    return new Response(new ReadableStream({start(c){init.signal.addEventListener('abort',()=>{try{c.close();}catch{}});}}));
  });
  for(const id of ['C','D']){event('subagent.spawned',{parent_session_id:'P',child_session_id:id,agent:'plan',goal:'work'});b.state.childStateCursors.set(id,'100');}
  await new Promise(r=>setTimeout(r,900));await tick();
  assert(counts.C>=2&&counts.D>=2);assert.equal(b.state.childStateCursors.has('C'),false);assert.equal(b.state.childStateCursors.has('D'),false);
  const signals=[...b.state.childReads.values()];b.cancelPermissionReads();assert(signals.every(x=>x.signal.aborted));assert.equal(b.state.childRetryTimers.size,0);
});
test('S3 panel disposal cancels confirmation; S8 terminal panel replay EOF never reconnects', async t=>{
  const {b,event,dom,tick,panels}=fullChat(t);b.state.session='P';b.scheduleChildRead=()=>{};
  event('subagent.spawned',{parent_session_id:'P',child_session_id:'C',agent:'plan',goal:'work'});
  let streams=0;let posts=0;
  t.mock.method(globalThis,'fetch',async(url,init)=>{
    if(init.method==='POST'){posts++;return new Response(JSON.stringify({accepted:true}));}
    if(url.endsWith('/events')){streams++;return new Response('');}
    return new Response(JSON.stringify({parent_session_id:'P',promptable:false,agent:'plan',child_status:'done'}));
  });
  dom.window.document.querySelector('.child-open').click();await tick();await tick();
  await new Promise(r=>setTimeout(r,650));assert.equal(streams,1);
  b.state.children.get('C').state='budget';b.publishSubagents();
  let confirm;vscode.window.showWarningMessage=()=>new Promise(r=>confirm=r);
  panels[0].send({type:'controlChild',parentId:'P',childId:'C',action:'continue',generation:b.currentGeneration()});
  panels[0].panel.dispose();confirm('Confirm');await tick();assert.equal(posts,0);
});
test('S2.1 concurrent delayed opens reserve one panel then close and reopen', async t=>{
  const {b,event,dom,tick,panels}=fullChat(t);b.state.session='P';b.scheduleChildRead=()=>{};
  event('subagent.spawned',{parent_session_id:'P',child_session_id:'C',agent:'plan',goal:'work'});
  let finish;let count=0;
  t.mock.method(globalThis,'fetch',()=>{count++;return new Promise(r=>finish=r);});
  const open=()=>dom.window.document.querySelector('.child-open').click();open();open();assert.equal(count,1);
  finish(new Response(JSON.stringify({parent_session_id:'P',promptable:false,child_status:'done'})));await tick();await tick();assert.equal(panels.length,1);
  panels[0].panel.dispose();const before=count;open();assert.equal(count,before+1);
  finish(new Response(JSON.stringify({parent_session_id:'P',promptable:false,child_status:'done'})));await tick();await tick();assert.equal(panels.length,2);panels[1].panel.dispose();
});
test('S2.2 running metadata is real running but preserves newer budget and terminal projections', async t=>{
  const {b,event,dom,tick,panels}=fullChat(t);b.state.session='P';b.scheduleChildRead=()=>{};
  event('subagent.spawned',{parent_session_id:'P',child_session_id:'C',agent:'plan',goal:'work'});
  t.mock.method(globalThis,'fetch',async()=>new Response(JSON.stringify({parent_session_id:'P',promptable:false,child_status:'running'})));
  event('agent.yield',{child_session_id:'C',kind:'budget'});
  dom.window.document.querySelector('.child-open').click();await tick();await tick();assert.equal(b.state.children.get('C').state,'budget');
  event('subagent.done',{parent_session_id:'P',child_session_id:'C',outcome:'done'});
  b.seedChild(b.state,'C',{child_status:'active'});assert.equal(b.state.children.get('C').state,'done');
  assert.equal(b.childStatus('running'),'running');assert.equal(b.childStatus('active'),'running');panels[0].panel.dispose();
});
test('S2.3 terminal replay coalesces, ignores yields, retries delayed metadata and cancels hanging lookup', async t=>{
  const {b,event,tick}=fullChat(t);b.state.session='P';b.scheduleChildRead=()=>{};
  event('subagent.spawned',{parent_session_id:'P',child_session_id:'C',agent:'plan',goal:'work'});
  let count=0;let finish;let signal;
  t.mock.method(globalThis,'fetch',(_url,init)=>{count++;signal=init.signal;return new Promise(r=>finish=r);});
  for(let i=0;i<50;i++)event('turn.end',{child_session_id:'C',outcome:'yielded'});assert.equal(count,0);
  for(let i=0;i<50;i++)event('turn.end',{child_session_id:'C',outcome:'done'});assert.equal(count,1);
  finish(new Response(JSON.stringify({child_status:'running'})));await new Promise(r=>setTimeout(r,150));assert.equal(count,2);
  finish(new Response(JSON.stringify({child_status:'done'})));await tick();
  if(count===3){finish(new Response(JSON.stringify({child_status:'done'})));await tick();}
  assert.equal(b.state.children.get('C').state,'done');assert(count<=3);
  b.state.children.get('C').state='running';event('session.end',{child_session_id:'C'});assert.equal(signal.aborted,false);b.dispose();assert.equal(signal.aborted,true);
});
test('S2.4 two outstanding confirmed descriptors across accepted completion send one POST', async t=>{
  const {b,event,dom,tick}=fullChat(t);b.state.session='P';b.scheduleChildRead=()=>{};
  event('subagent.spawned',{parent_session_id:'P',child_session_id:'C',agent:'plan',goal:'work'});event('agent.yield',{child_session_id:'C',kind:'budget'});
  const confirmations=[];vscode.window.showWarningMessage=()=>new Promise(r=>confirmations.push(r));
  let count=0;t.mock.method(globalThis,'fetch',async()=>{count++;return new Response(JSON.stringify({accepted:true}));});
  const button=[...dom.window.document.querySelectorAll('.child-row button')].find(x=>x.textContent==='Continue');button.click();button.click();
  confirmations[0]('Confirm');await tick();confirmations[1]('Confirm');await tick();assert.equal(count,1);assert.equal(b.state.children.get('C').state,'budget');
});
test('S2.3 hanging terminal metadata GET times out and releases reconciliation', async t=>{
  const {b,event,tick}=fullChat(t);b.state.session='P';b.scheduleChildRead=()=>{};
  event('subagent.spawned',{parent_session_id:'P',child_session_id:'C',agent:'plan',goal:'work'});
  let signal;t.mock.method(globalThis,'fetch',(_url,init)=>{signal=init.signal;return new Promise((_resolve,reject)=>signal.addEventListener('abort',()=>reject(new Error('aborted'))));});
  event('session.end',{child_session_id:'C'});
  await new Promise(r=>setTimeout(r,2100));await tick();assert.equal(signal.aborted,true);assert.equal(b.childReconciliations.size,0);
});
test('compaction real host/page starts immediately, survives deferred acceptance, completes and rejects stale ownership',async t=>{
  const {b,event,dom,tick,send}=fullChat(t);b.state.session='P';b.commands.push({name:'compact',description:'Compact',args_hint:''});
  let finish;t.mock.method(globalThis,'fetch',()=>new Promise(r=>finish=r));
  send('/compact focus');assert.match(dom.window.document.querySelector('.compaction-progress').textContent,/Compacting/);assert.equal(dom.window.document.querySelector('#prompt').disabled,false);assert.equal(dom.window.document.querySelector('#stop').hidden,false);assert.equal(dom.window.document.querySelector('#usage').getAttribute('aria-busy'),'true');
  event('command.result',{name:'compact',operation_id:'C',ok:true,data:{accepted:true,applies:'after_turn'}});
  finish(new Response(JSON.stringify({operation_id:'C',ok:true})));await tick();assert.equal(b.state.compaction.pending,true);
  event('context.compacted',{estimate_before:200,estimate_after:100});assert.equal(dom.window.document.querySelector('.compaction-progress').dataset.pending,'true');
  event('command.result',{name:'compact',operation_id:'C',ok:true,data:{estimate_before:200,estimate_after:100}});assert.equal(dom.window.document.querySelector('.compaction-progress').dataset.pending,'false');assert.equal(dom.window.document.querySelector('#prompt').disabled,false);
  send('/compact');await tick();await b.newConversation();finish(new Response(JSON.stringify({operation_id:'OLD',ok:true})));await tick();assert.equal(dom.window.document.querySelector('.compaction-progress'),null);assert.equal(b.state.compaction,undefined);
});
test('compaction correlated failure replaces progress and frees composer',async t=>{
  const {b,event,dom,tick,send}=fullChat(t);b.state.session='P';b.commands.push({name:'compact',description:'Compact',args_hint:''});
  t.mock.method(globalThis,'fetch',async()=>new Response(JSON.stringify({operation_id:'C',ok:true})));
  send('/compact');await tick();event('command.result',{name:'compact',operation_id:'C',ok:false,error:'Summary unavailable'});
  assert.match(dom.window.document.querySelector('.compaction-progress').textContent,/Summary unavailable/);assert.equal(dom.window.document.querySelector('#prompt').disabled,false);
});
test('compaction pending progress survives recreated host view',async t=>{
  const {b,send,tick}=fullChat(t);b.state.session='P';b.commands.push({name:'compact',description:'Compact',args_hint:''});
  t.mock.method(globalThis,'fetch',async()=>new Response(JSON.stringify({operation_id:'C',ok:true})));
  send('/compact');await tick();
  const messages=[];let receive;
  provider.resolveWebviewView({webview:{cspSource:'test',asWebviewUri:v=>v,postMessage:m=>messages.push(m),onDidReceiveMessage:fn=>{receive=fn;return noopDisposable();}},onDidDispose(){}});
  receive({type:'ready'});assert.equal(messages.find(m=>m.type==='compaction').pending,true);assert.equal(messages.find(m=>m.type==='compaction').operationId,'C');
});
test('R1.1 compaction queues ordinary send and allows help while preventing second compact',async t=>{
  const {b,event,send,tick,dom}=fullChat(t);b.state.session='P';b.commands.push({name:'compact',description:'Compact',args_hint:''});
  b.refreshCommands=async()=>{};
  const calls=[];t.mock.method(globalThis,'fetch',async(url,init)=>{calls.push(url);return new Response(JSON.stringify(url.endsWith('/messages')?{accepted:true}:{operation_id:'C'}));});
  send('/compact');await tick();send('queued input');await tick();assert(calls.some(url=>url.endsWith('/messages')));assert.equal(dom.window.document.querySelector('#prompt').disabled,false);
  b.commands.push({name:'compact',description:'Compact',args_hint:''});await b.runComposerCommand('/help');await assert.rejects(b.runComposerCommand('/compact'),/already pending/);event('command.result',{name:'compact',operation_id:'C',ok:true});
});
test('R1.4 manual compact binds HTTP id before cached results and ignores automatic notices and older completions',async t=>{
  const {b,event,send,tick}=fullChat(t);b.state.session='P';b.commands.push({name:'compact',description:'Compact',args_hint:''});let finish;
  b.fetchContext=async()=>{};b.refreshCommands=async()=>{};
  t.mock.method(globalThis,'fetch',()=>new Promise(r=>finish=r));send('/compact');
  event('command.result',{name:'compact',operation_id:'OTHER',ok:false,error:'unrelated'});assert.equal(b.state.compaction.pending,true);
  event('command.result',{name:'compact',operation_id:'C',ok:true,data:{accepted:true,applies:'after_turn'}});finish(new Response(JSON.stringify({operation_id:'C'})));await tick();assert.equal(b.state.compaction.pending,true);
  event('context.compacted',{estimate_before:50,estimate_after:25});assert.equal(b.state.compaction.pending,true);event('command.result',{name:'compact',operation_id:'C',ok:false,error:'Own failure'});assert.match(b.state.compaction.text,/Own failure/);
  b.commands.push({name:'compact',description:'Compact',args_hint:''});send('/compact again');event('command.result',{name:'compact',operation_id:'C',ok:true});assert.equal(b.state.compaction.pending,true);finish(new Response(JSON.stringify({operation_id:'NEW'})));await tick();assert.equal(b.state.compaction.operationId,'NEW');
});
for(const deferred of [true,false])test(`R1.5 resync republishes ${deferred?'deferred':'executing'} compaction progress`,async t=>{
  const {b,event,send,tick,dom}=fullChat(t);b.state.session='P';b.commands.push({name:'compact',description:'Compact',args_hint:''});t.mock.method(globalThis,'fetch',async()=>new Response(JSON.stringify({operation_id:'C'})));send('/compact');await tick();
  if(deferred)event('command.result',{name:'compact',operation_id:'C',ok:true,data:{accepted:true,applies:'after_turn'}});
  b.handleFrame({id:'',event:'resync'});assert.match(dom.window.document.querySelector('.compaction-progress').textContent,/Compacting/);assert.equal(dom.window.document.querySelector('.compaction-progress').dataset.pending,'true');
});
function focusedKeys(dom) {
  const key = key => {
    const target = dom.window.document.activeElement;
    const event = new dom.window.KeyboardEvent('keydown', { key, bubbles: true, cancelable: true });
    target.dispatchEvent(event);
    if (!event.defaultPrevented && key.length === 1) { target.value += key; target.dispatchEvent(new dom.window.Event('input', { bubbles: true })); }
    if (!event.defaultPrevented && key === 'Backspace') { target.value = target.value.slice(0,-1); target.dispatchEvent(new dom.window.Event('input',{bubbles:true})); }
  };
  return { key, type: text => { for (const character of text) key(character); } };
}
test('R2.1 registered host posts queued message while idle compact HTTP remains pending and preserves its submission lock', async t=>{
  const {b,dom,tick,prompt}=fullChat(t);b.state.session='P';b.refreshCommands=async()=>{};b.commands.push({name:'compact',description:'Compact',args_hint:''});
  const {key,type}=focusedKeys(dom);let compactResponse;let messageResponse;const calls=[];
  t.mock.method(globalThis,'fetch',(url,init)=>{calls.push({url,...init});return new Promise(resolve=>{if(url.endsWith('/commands'))compactResponse=resolve;else messageResponse=resolve;});});
  prompt.focus();type('/compact focus');key('Enter');await tick();assert.equal(calls.length,1);assert.equal(prompt.value,'/compact focus');
  while(prompt.value)key('Backspace');type('queued message');key('Enter');await tick();assert.match(calls[1].url,/\/messages$/);
  compactResponse(new Response(JSON.stringify({operation_id:'C',ok:true})));await tick();assert.equal(dom.window.document.querySelector('#send').disabled,true);
  b.handleFrame({id:'1',event:'',envelope:{seq:1,kind:'command.result',payload:{name:'compact',operation_id:'C',ok:false,error:'Summary refused'}}});await tick();assert.equal(dom.window.document.querySelector('#send').disabled,true);
  messageResponse(new Response(JSON.stringify({accepted:true})));await tick();assert.equal(prompt.value,'');
});
test('R2.2 cancelled selector cannot route slash completion as model selection, including after conversation switch',async t=>{
  const {b,dom,tick,prompt}=fullChat(t);b.state.session='P';b.refreshCommands=async()=>{};
  b.commands.push({name:'compact',description:'Compact',args_hint:''});b.emit({type:'commands',items:b.commands});b.emit({type:'models',items:[{name:'a',model:'provider/a'}],locked:false});
  await tick();b.emit({type:'commands',items:[{name:'compact',description:'Compact',args_hint:''}]});
  const {key,type}=focusedKeys(dom);let posts=0;
  t.mock.method(globalThis,'fetch',()=>{posts++;return new Promise(()=>{});});
  const selector=dom.window.document.querySelector('#model');selector.focus();key('Enter');key('Escape');prompt.focus();type('/com');key('Tab');assert.equal(prompt.value,'/compact ');assert.equal(posts,0);
  selector.focus();key('Enter');await b.newConversation();await tick();prompt.focus();while(prompt.value)key('Backspace');type('/com');key('Tab');assert.equal(prompt.value,'/compact ');assert.equal(posts,0);
});
test('R2.3 unknown bare slash reaches registered host validation and retains focused draft',async t=>{
  const {b,dom,tick,prompt}=fullChat(t);b.state.session='P';b.refreshCommands=async()=>{};b.emit({type:'commands',items:b.commands});
  let calls=0;t.mock.method(globalThis,'fetch',()=>{calls++;return new Promise(()=>{});});const {key,type}=focusedKeys(dom);
  prompt.focus();type('/nonexistent');key('ArrowDown');key('Enter');await tick();assert.match(dom.window.document.querySelector('.command-error').textContent,/Unknown command/);assert.equal(prompt.value,'/nonexistent');assert.equal(calls,0);
});
test('target configured default agent, editor creation body, agent switching and layer tooltip use canonical identities',async t=>{
  const {b,dom,tick,event,send}=fullChat(t);b.state.session=undefined;
  const assignments=[{id:'plan',description:'Plan changes',mode:'primary',effective:{model:'omniroute/combo/think',variant:'high',source:'config'}},{id:'build',description:'Build',mode:'all',effective:{model:'direct/model',variant:null,source:'config'}},{id:'child',description:'Child',mode:'subagent',effective:null}];
  const calls=[];t.mock.method(globalThis,'fetch',async(url,init)=>{
    calls.push({url,...init});
    if(url.includes('/agents?'))return new Response(JSON.stringify({default_agent:'plan',agents:assignments}));
    if(url.includes('/layers?'))return new Response(JSON.stringify({layers:[{layer:'reflex',status:'disabled',model:'direct/editor',reason:'wrong API'},{layer:'instinct',status:'usable',model:'omniroute/typesafe/jev',variant:null},{layer:'metacognition',status:'usable',model:'omniroute/combo/fast',variant:'low'}]}));
    return new Response(JSON.stringify(url.endsWith('/messages')?{accepted:true}:{session_id:'S'}));
  });
  await b.refreshAssignments();assert.equal(b.state.agent,'plan');assert.equal(dom.window.document.querySelector('[aria-label="Conversation agent"]').value,'plan');assert.equal(dom.window.document.querySelectorAll('[aria-label="Conversation agent"] option').length,2);
  assert.match(dom.window.document.querySelector('.topbar').title,/FIM.*disabled.*wrong API/);assert.match(dom.window.document.querySelector('.topbar').title,/not network availability/);
  send('hello');await tick();const creation=calls.find(c=>c.url.endsWith('/v1/sessions'));assert.deepEqual(JSON.parse(creation.body),{workspace:process.cwd(),editor:true,agent:'plan'});
  event('model.changed',{operation_id:'A',reason:'agent',new:{agent:'build',model:'direct/override',gateway:'direct',upstream_id:'override',variant:null,source:'override',variant_source:'override'}});
  assert.equal(b.state.selectedModel,'direct/override');assert.match(dom.window.document.querySelector('.agent-label').textContent,/build.*override/);
  event('model.changed',{operation_id:'B',reason:'agent',new:{agent:'plan',model:'omniroute/combo/think',gateway:'omniroute',upstream_id:'combo/think',variant:'high',source:'config',variant_source:'config'}});assert.equal(b.state.selectedModel,'omniroute/combo/think');
  await b.newConversation();assert.equal(b.state.modelOverride,undefined);assert.equal(b.state.agent,'plan');
});
test('target stale agents and layers cannot update replacement conversation',async t=>{
  const {b,dom}=fullChat(t);let finishAgents,finishLayers;b.refreshCommands=async()=>{};
  t.mock.method(globalThis,'fetch',url=>new Promise(resolve=>{if(url.includes('/agents?'))finishAgents=resolve;else finishLayers=resolve;}));
  const pending=b.refreshAssignments().catch(()=>{});await b.newConversation();
  finishAgents(new Response(JSON.stringify({default_agent:'obsolete',agents:[{id:'obsolete',description:'old',mode:'primary',effective:{model:'gw/old',source:'config'}}]})));finishLayers(new Response(JSON.stringify({layers:[{layer:'reflex',status:'disabled',reason:'old reason'}]})));await pending;assert.notEqual(b.state.agent,'obsolete');assert.doesNotMatch(dom.window.document.querySelector('.topbar').title,/old reason/);
});
test('target grouped model picker canonical names collapse and search without choosing headers',async t=>{
  const {b,dom,tick}=fullChat(t);b.state.session='P';b.refreshAssignments=async()=>{};
  const models=[{name:'omniroute/combo/think',gateway:'omniroute',provider_group:'combo',upstream_id:'combo/think',api:'chat',enabled:true},{name:'direct/model',gateway:'direct',provider_group:'direct',upstream_id:'model',api:'chat',enabled:true}];
  b.models=models;b.emit({type:'models',items:models,locked:false});const {key,type}=focusedKeys(dom);dom.window.document.querySelector('#model').focus();key('Enter');
  const groups=[...dom.window.document.querySelectorAll('.picker-group')];assert.deepEqual(groups.map(g=>g.textContent),['direct','omniroute','combo']);
  groups.find(g=>g.textContent==='omniroute').click();assert.equal(dom.window.document.querySelectorAll('#picker-options [role=option]').length,1);
  const search=dom.window.document.querySelector('#command-palette .picker-search');search.focus();type('combo/think');assert.equal(dom.window.document.querySelectorAll('#picker-options [role=option]').length,1);assert.equal(dom.window.document.querySelector('#picker-options [role=option]').title,'omniroute/combo/think');
  let request;t.mock.method(globalThis,'fetch',async(url,init)=>{request=JSON.parse(init.body);return new Response(JSON.stringify({operation_id:'M'}));});key('Enter');await tick();assert.deepEqual(request,{name:'model',arguments:'omniroute/combo/think'});
});
test('target reopened summary restores effective override and blocks stale metadata selection',async t=>{
  const {b,dom,event}=fullChat(t);b.state.session='S';b.refreshAssignments=async()=>{};
  t.mock.method(globalThis,'fetch',async()=>new Response(JSON.stringify({current_seq:3,agent:'plan',model:'gw/saved',variant:'high',source:'override',variant_source:'override'})));
  await b.loadSessionModel('S');assert.equal(b.state.selectedModel,'gw/saved');assert.match(dom.window.document.querySelector('.topbar').title,/gw\/saved.*override/);
  for(let i=0;i<5;i++)event('model.changed',{new:{agent:'build',model:'gw/new',gateway:'gw',upstream_id:'new',variant:null,source:'config',variant_source:'config'}});
  await b.loadSessionModel('S');assert.equal(b.state.selectedModel,'gw/new');
});
function fullChat(t) {
  const { JSDOM } = require('jsdom');
  const { chatHtml } = require('../out/tests/chatHtml.js');
  const fs = require('node:fs');
  const b = new CognitionBackend(); b.endpoint = { port: 1234, token: 't' };
  b.connect = async () => {}; b.fetchContext = async () => {}; b.listConversations = async () => [];
  b.commands = ['model','agent','variant'].map(name => ({ name, description: name, args_hint: '' }));
  let receive;
  const panels=[];
  const dom = new JSDOM(chatHtml('n','chat.js','chat.css','resource:'), { runScripts: 'outside-only' });
  t.after(() => dom.window.close());
  registerChatView({ extensionUri: 'extension' }, b);
  provider.resolveWebviewView({ webview: { cspSource: 'test', asWebviewUri: value => value,
    postMessage(data) { dom.window.dispatchEvent(new dom.window.MessageEvent('message', { data })); },
    onDidReceiveMessage(listener) { receive = listener; return noopDisposable(); } }, onDidDispose() {} });
  vscode.ViewColumn = {Active:1};
  vscode.window.createWebviewPanel = (_kind,title) => {
    const page = new JSDOM(chatHtml('n','chat.js','chat.css','resource:'), {runScripts:'outside-only'});
    const handlers=[];let handler;
    const panel={title, reveal(){}, webview:{cspSource:'test',options:{},asWebviewUri:v=>v,
      postMessage(data){page.window.dispatchEvent(new page.window.MessageEvent('message',{data}));},
      onDidReceiveMessage(fn){handler=fn;return noopDisposable();}},
      onDidDispose(fn){handlers.push(fn);return noopDisposable();},
      dispose(){for(const fn of handlers.splice(0))fn();page.window.close();}};
    panels.push({panel,page,send:m=>handler(m)});
    // Start the shipped page once the panel host handler has been registered.
    setImmediate(()=>{page.window.acquireVsCodeApi=()=>({postMessage:m=>handler(m),getState(){},setState(){}});page.window.eval(fs.readFileSync('out/chat.js','utf8'));});
    return panel;
  };
  dom.window.acquireVsCodeApi = () => ({ postMessage: message => receive(message), getState() {}, setState() {} });
  dom.window.eval(fs.readFileSync('out/chat.js','utf8'));
  const prompt = dom.window.document.querySelector('#prompt');
  const send = text => { prompt.value = text; receive({ type: 'send', text, includeContext: false, generation: b.currentGeneration() }); };
  let seq = 0;
  const event = (kind,payload) => b.handleFrame({ id: String(++seq), event: '', envelope: { seq,kind,payload } });
  const tick = () => new Promise(resolve => setImmediate(resolve));
  return { b, dom, prompt, send, event, tick, panels };
}
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
  assert.deepEqual(JSON.parse(calls.at(-1).body),{pause_token:null});
  [...dom.window.document.querySelectorAll('.child-row button')].find(x=>x.textContent==='Stop').click(); await tick();
  assert.match(calls.at(-1).url,/\/C\/stop$/); assert.equal(calls.at(-1).body,undefined);
  const old=b.currentGeneration(); await b.newConversation();
  await assert.rejects(b.controlChild('P','C','stop',old),/conversation changed/);
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
  b.models=[{name:'gw/a',gateway:'gw',provider_group:'gw',upstream_id:'a',api:'chat',enabled:true}];
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
  b.state.pendingModel={name:'old',operationId:'OLD'}; b.emit({type:'modelPending',name:'old'});
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
  assert.equal(b.state.pendingModel.name,'next'); assert.match(dom.window.document.querySelector('.pending-model').textContent,/next/);
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
    publishCompaction() {},
    refreshAssignments: async()=>{},
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
