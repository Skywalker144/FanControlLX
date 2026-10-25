// Isolated Chromium renderer measurements; no npm packages or hardware writes.
// CHROME_BIN=/path/to/chrome node tests/benchmark_frontend.mjs URL OUTPUT.json
import {spawn} from 'node:child_process';
import {mkdtemp, readFile, rm, writeFile} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';

const [url='http://127.0.0.1:18091', output='test-results/frontend.json', size]=process.argv.slice(2);
if(!process.env.CHROME_BIN)throw new Error('Set CHROME_BIN to an existing Chromium executable');
const health=await(await fetch(new URL('/api/view',url))).json();
if(!health.demo)throw new Error('Benchmark requires an isolated demo instance');
if(size && size!=='--large')throw new Error('Optional third argument: --large (replaces disposable demo configuration)');
if(size==='--large'){
  const state=await(await fetch(new URL('/api/state',url))).json(),fan=state.config.fans[0];
  if(!state.demo)throw new Error('Refusing to replace a non-demo configuration');
  const points=Array.from({length:32},(_,i)=>[20+i*80/31,Math.min(100,i*4)]);
  const config={version:2,
    curves:Array.from({length:128},(_,i)=>({id:`curve-${i}`,name:`Curve ${i}`,points})),
    fans:Array.from({length:64},(_,i)=>({...fan,id:`fan-${i}`,name:`Fan ${i}`,points,curve_id:`curve-${i}`,channel_id:null,rpm_sensor_id:null,identified:false,autostart:false}))};
  const response=await fetch(new URL('/api/config',url),{method:'POST',headers:{'Content-Type':'application/json','X-FanControl-Token':state.csrf_token},body:JSON.stringify(config)});
  if(!response.ok)throw new Error(await response.text());
}
const profile=await mkdtemp(join(tmpdir(),'fancontrollx-perf-'));
const browser=spawn(process.env.CHROME_BIN,['--headless','--no-sandbox','--no-first-run','--no-default-browser-check','--password-store=basic','--use-mock-keychain','--disable-background-networking','--disable-component-update','--disable-sync','--disable-dev-shm-usage','--disable-gpu','--remote-debugging-port=0',`--user-data-dir=${profile}`,'about:blank'],{stdio:['ignore','ignore','pipe']});
let socket;
try{
  const endpoint=await new Promise((resolve,reject)=>{
    let log='';const timer=setTimeout(()=>reject(new Error('Chromium startup timed out')),15000);
    browser.stderr.on('data',chunk=>{log+=chunk;const match=log.match(/DevTools listening on (ws:\/\/\S+)/);if(match){clearTimeout(timer);resolve(match[1]);}});
    browser.once('exit',code=>{clearTimeout(timer);reject(new Error(`Chromium exited ${code}: ${log.slice(-1000)}`));});
  });
  socket=new WebSocket(endpoint);
  await new Promise((resolve,reject)=>{socket.addEventListener('open',resolve,{once:true});socket.addEventListener('error',reject,{once:true});});
  let next=0,session;const pending=new Map(),errors=[];
  const send=(method,params={},sessionId=session)=>new Promise((resolve,reject)=>{
    const id=++next,timer=setTimeout(()=>{pending.delete(id);reject(new Error(`${method} timed out`));},15000);
    pending.set(id,{resolve,reject,timer});socket.send(JSON.stringify({id,method,params,...(sessionId?{sessionId}:{})}));
  });
  socket.addEventListener('message',event=>{
    const message=JSON.parse(event.data),p=pending.get(message.id);
    if(p){clearTimeout(p.timer);pending.delete(message.id);message.error?p.reject(new Error(JSON.stringify(message.error))):p.resolve(message.result);}
    if(message.method==='Runtime.exceptionThrown')errors.push(message.params.exceptionDetails);
  });
  const target=await send('Target.createTarget',{url:'about:blank'},null);
  session=(await send('Target.attachToTarget',{targetId:target.targetId,flatten:true},null)).sessionId;
  for(const method of ['Page.enable','Runtime.enable','Performance.enable'])await send(method);
  await send('Emulation.setDeviceMetricsOverride',{width:1440,height:1100,deviceScaleFactor:1,mobile:false});
  const evaluate=async expression=>{
    const value=await send('Runtime.evaluate',{expression,returnByValue:true,awaitPromise:true});
    if(value.exceptionDetails)throw new Error(value.exceptionDetails.exception?.description||value.exceptionDetails.text);
    return value.result.value;
  };
  const waitFor=async expression=>{
    for(let i=0;i<100;i++){if(await evaluate(expression))return;await new Promise(resolve=>setTimeout(resolve,100));}
    throw new Error(`Timed out: ${expression}`);
  };
  const metrics=async()=>Object.fromEntries((await send('Performance.getMetrics')).metrics.map(m=>[m.name,m.value]));
  const rendererMemory=async()=>{
    // JS heap excludes native DOM/select storage. PSS apportions shared pages
    // across processes instead of counting each shared page in every RSS.
    const renderers=(await send('SystemInfo.getProcessInfo',{},null)).processInfo.filter(p=>p.type==='renderer');
    const records=await Promise.all(renderers.map(async p=>{
      try{
        const rollup=await readFile(`/proc/${p.id}/smaps_rollup`,'utf8');
        const bytes=name=>Number(rollup.match(new RegExp(`^${name}:\\s+(\\d+)`,'m'))?.[1]||0)*1024;
        return {pss_bytes:bytes('Pss'),private_bytes:bytes('Private_Clean')+bytes('Private_Dirty')};
      }catch{return null;}
    }));
    if(!records.length||records.some(r=>r===null))return null;
    return {processes:records.length,pss_bytes:records.reduce((sum,r)=>sum+r.pss_bytes,0),private_bytes:records.reduce((sum,r)=>sum+r.private_bytes,0)};
  };
  const samples=[];
  for(const scenario of ['viewer','editor','curve-library']){
    await send('Page.navigate',{url:new URL(scenario==='viewer'?'/':'/edit',url).href});
    await waitFor(`document.getElementById('connection-text')?.textContent.includes('已连接')`);
    // Repeatable telemetry: fixed configuration, changing 80-sample history per
    // poll. This measures rendering, not random demo values or backend sampling.
    await evaluate(`(async()=>{
      const path=${JSON.stringify(scenario==='viewer'?'/api/view':'/api/state')};
      const original=window.fetch.bind(window),snapshot=await(await original(path)).json();let tick=0;
      window.__polls=0;
      window.fetch=(input,...args)=>{
        if(String(input).split('?')[0]!==path)return original(input,...args);
        const s=structuredClone(snapshot);tick++;window.__polls++;
        // Match production polling: stable configuration objects are retained
        // by the client while telemetry changes on every response.
        const since=new URL(String(input),location.href).searchParams.get('since');
        if(path==='/api/state'&&since===s.config_revision)delete s.config;
        if(path==='/api/view'&&since===s.view_revision){
          s.fans_delta=true;
          s.fans=s.fans.map(({id,output,pending})=>({id,output,pending}));
        }
        s.history=Array.from({length:80},(_,i)=>({time:i+tick,cpu:45+(i+tick)%10,gpu:40+(i+tick)%8}));
        if(s.fans&&s.cpu!==undefined){s.cpu=45+tick%10;s.sample_time+=tick;for(const f of s.fans){f.output.actual=40+tick%10;f.output.temperature=45+tick%10;}}
        else{s.time+=tick;for(const o of Object.values(s.outputs)){o.actual=40+tick%10;o.temperature=45+tick%10;}}
        return Promise.resolve(new Response(JSON.stringify(s),{headers:{'Content-Type':'application/json'}}));
      };
      if(${JSON.stringify(scenario)}==='curve-library')document.getElementById('new-curve').click();
      await Promise.all(document.getAnimations().map(a=>a.finished.catch(()=>{})));
    })()`);
    await send('HeapProfiler.collectGarbage');
    await evaluate(`(()=>{
      window.__mutations={childList:0,attributes:0,characterData:0,addedNodes:0};window.__polls=0;
      const root=${JSON.stringify(scenario)}==='curve-library'?document.getElementById('curve-chart'):document.getElementById('fan-cards');
      window.__observer=new MutationObserver(rows=>{for(const r of rows){window.__mutations[r.type]++;window.__mutations.addedNodes+=r.addedNodes.length;}});
      window.__observer.observe(root,{subtree:true,childList:true,attributes:true,characterData:true});
    })()`);
    const before=await metrics();
    await new Promise(resolve=>setTimeout(resolve,6500));
    const after=await metrics();
    const dom=await evaluate(`(()=>{window.__observer.disconnect();return{mutations:window.__mutations,polls:window.__polls,resources:performance.getEntriesByType('resource').filter(r=>!new URL(r.name).pathname.startsWith('/api/')).map(r=>({path:new URL(r.name).pathname,bytes:r.decodedBodySize})),nodes:document.getElementsByTagName('*').length}})()`);
    await send('HeapProfiler.collectGarbage');
    const collected=await metrics();
    samples.push({scenario,seconds:after.Timestamp-before.Timestamp,task_ms:(after.TaskDuration-before.TaskDuration)*1000,script_ms:(after.ScriptDuration-before.ScriptDuration)*1000,layout_ms:(after.LayoutDuration-before.LayoutDuration)*1000,layouts:after.LayoutCount-before.LayoutCount,heap_bytes:collected.JSHeapUsedSize,renderer_memory:await rendererMemory(),...dom});
    console.log(JSON.stringify(samples.at(-1)));
  }
  if(errors.length)throw new Error(JSON.stringify(errors));
  await writeFile(output,JSON.stringify({browser:await send('Browser.getVersion',{},null),configuration:size==='--large'?'64 offline fans, 128 curves, 32 points':'existing demo configuration',conditions:'Headless, GPU disabled, 1440x1100; 6.5 seconds per scenario; deterministic changing telemetry and 80-sample history, fixed config; JS heap and Linux renderer PSS/private memory after GC; renderer task time includes measurement overhead; not whole-browser RSS or power.',samples},null,2)+'\n');
}finally{
  socket?.close();browser.kill('SIGTERM');
  await new Promise(resolve=>{browser.once('exit',resolve);setTimeout(resolve,2000);});
  await rm(profile,{recursive:true,force:true,maxRetries:10,retryDelay:100});
}
