// Dependency-free Chromium smoke test via the Chrome DevTools Protocol.
// Start a disposable demo server first; see README.md. Node 22+ is required.
import assert from 'node:assert/strict';
import {spawn} from 'node:child_process';
import {mkdir,mkdtemp,writeFile,rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join} from 'node:path';

const url=process.argv[2]||'http://127.0.0.1:18091';
const browserPath=process.env.CHROME_BIN;
if(!browserPath)throw new Error('Set CHROME_BIN to an existing Chromium executable');
const snapshotOnly=process.argv.includes('--snapshot-only');
const profile=await mkdtemp(join(tmpdir(),'fancontrollx-browser-'));
const browser=spawn(browserPath,['--headless','--no-sandbox','--no-first-run','--no-default-browser-check','--password-store=basic','--use-mock-keychain','--disable-background-networking','--disable-component-update','--disable-sync','--disable-dev-shm-usage','--disable-gpu','--remote-debugging-port=0',`--user-data-dir=${profile}`,'about:blank'],{stdio:['ignore','ignore','pipe']});
let socket;
try{
  const endpoint=await new Promise((resolve,reject)=>{
    let output='';const timer=setTimeout(()=>reject(new Error('Chromium startup timed out')),15000);
    browser.stderr.on('data',chunk=>{output+=chunk;const match=output.match(/DevTools listening on (ws:\/\/[^\s]+)/);if(match){clearTimeout(timer);resolve(match[1]);}});
    browser.on('exit',code=>{clearTimeout(timer);reject(new Error(`Chromium exited ${code}: ${output.slice(-1500)}`));});
  });
  socket=new WebSocket(endpoint);
  await new Promise((resolve,reject)=>{socket.addEventListener('open',resolve,{once:true});socket.addEventListener('error',reject,{once:true});});
  let next=0,session;const pending=new Map(),errors=[],requests=[],pausedModules=[];
  const send=(method,params={},sessionId=session)=>new Promise((resolve,reject)=>{
    const id=++next;const timer=setTimeout(()=>{pending.delete(id);reject(new Error(`${method} timed out`));},12000);
    pending.set(id,{resolve,reject,timer});socket.send(JSON.stringify({id,method,params,...(sessionId?{sessionId}:{})}));
  });
  socket.addEventListener('message',event=>{
    const value=JSON.parse(event.data);
    if(value.id){const p=pending.get(value.id);if(p){clearTimeout(p.timer);pending.delete(value.id);value.error?p.reject(new Error(JSON.stringify(value.error))):p.resolve(value.result);}}
    if(value.method==='Runtime.exceptionThrown')errors.push(value.params.exceptionDetails.text+': '+(value.params.exceptionDetails.exception?.description||''));
    if(value.method==='Network.requestWillBeSent')requests.push(value.params.request);
    if(value.method==='Fetch.requestPaused')pausedModules.push(value.params.requestId);
    if(value.method==='Page.javascriptDialogOpening')send('Page.handleJavaScriptDialog',{accept:true}).catch(()=>{});
  });
  const target=await send('Target.createTarget',{url:'about:blank'},null);
  session=(await send('Target.attachToTarget',{targetId:target.targetId,flatten:true},null)).sessionId;
  await send('Page.enable');await send('Runtime.enable');await send('Network.enable');await send('Page.bringToFront');
  await send('Emulation.setDeviceMetricsOverride',{width:1440,height:1100,deviceScaleFactor:1,mobile:false});
  await send('Page.navigate',{url:new URL('/',url).href});
  const evaluate=async expression=>{
    const result=await send('Runtime.evaluate',{expression,returnByValue:true,awaitPromise:true});
    if(result.exceptionDetails)throw new Error(result.exceptionDetails.exception?.description||result.exceptionDetails.text);
    return result.result.value;
  };
  const waitFor=async expression=>{const deadline=Date.now()+10000;while(Date.now()<deadline){if(await evaluate(expression))return;await new Promise(resolve=>setTimeout(resolve,120));}throw new Error(`Wait timed out: ${expression}`);};
  const confirmAction=async(accept=true)=>{
    await waitFor('document.getElementById("confirm-dialog").open');
    await evaluate(`document.querySelector('#confirm-dialog [value="${accept?'accept':'cancel'}"]').click()`);
    await waitFor('!document.getElementById("confirm-dialog").open');
  };
  const editorFits=async label=>{
    // A faster backend can finish a save before the dialog's opening animation.
    // Measure layout after that animation, not at an arbitrary in-between frame.
    await evaluate(`(async()=>{await Promise.all(document.getElementById('curve-dialog').getAnimations({subtree:true}).map(a=>a.finished.catch(()=>{})));await new Promise(requestAnimationFrame)})()`);
    const layout=await evaluate(`(()=>{const d=document.getElementById('curve-dialog'),b=d.getBoundingClientRect();return{fits:d.scrollHeight<=d.clientHeight+1&&d.scrollWidth<=d.clientWidth+1&&b.top>=0&&b.bottom<=innerHeight,height:d.clientHeight,scrollHeight:d.scrollHeight,parts:Array.from(d.querySelector('form').children).map(el=>[el.className,el.getBoundingClientRect().height])}})()`);
    assert.equal(layout.fits,true,`Curve editor must fit without scrolling: ${label}: ${JSON.stringify(layout)}`);
  };
  const reloadPage=async()=>{
    const loaded=new Promise((resolve,reject)=>{
      const onLoad=event=>{
        const message=JSON.parse(event.data);
        if(message.method==='Page.loadEventFired'&&message.sessionId===session){
          clearTimeout(timer);socket.removeEventListener('message',onLoad);resolve();
        }
      };
      const timer=setTimeout(()=>{socket.removeEventListener('message',onLoad);reject(new Error('Page reload timed out'));},12000);
      socket.addEventListener('message',onLoad);
    });
    await Promise.all([send('Page.reload'),loaded]);
  };
  await waitFor('document.getElementById("connection-text")?.textContent.includes("已连接")');
  assert.equal(await evaluate('!!document.querySelector("#edit-link")'),true,'Default page is the viewer');
  assert.equal(await evaluate('document.querySelectorAll("input,select:not(#theme-select):not(#fan-columns),dialog").length'),0,'Viewer contains no editor controls');
  assert.equal(await evaluate(`document.getElementById('theme-select').value`),'system','Theme defaults to following the device');
  assert.equal(await evaluate(`document.querySelectorAll('.fan-curve-preview>svg').length===document.querySelectorAll('[data-fan]').length && document.querySelectorAll('[data-fan]').length>0`),true,'Every viewer fan has a curve');
  assert.equal(await evaluate(`document.querySelectorAll('[data-fan] button,[data-fan] a,[data-fan] [tabindex],[data-fan] [draggable]').length`),0,'Viewer fan cards have no editing or drag controls');
  assert.equal(await evaluate(`(async()=>{const view=await(await fetch('/api/view')).json();return view.fans.every(f=>{const svg=Array.from(document.querySelectorAll('[data-fan]')).find(c=>c.dataset.fan===f.id).querySelector('.fan-curve-preview>svg');return svg.querySelectorAll(':scope>circle').length===f.points.filter(p=>p[0]>=20&&p[0]<=100).length&&svg.querySelector('[data-curve-current]').getAttribute('visibility')===(Number.isFinite(f.output?.temperature)?'visible':'hidden')})})()`),true,'Viewer curves show configured nodes and available temperature markers');
  assert.equal(await evaluate(`['cpu','gpu'].every(kind=>document.querySelector('#'+kind+'-spark path')?.getAttribute('d').includes('M'))`),true,'Viewer displays cached CPU and GPU temperature history');
  const chooseColumns=columns=>evaluate(`(()=>{const select=document.getElementById('fan-columns');select.value=${JSON.stringify(columns)};select.dispatchEvent(new Event('change'))})()`);
  const renderedColumns=()=>evaluate(`getComputedStyle(document.getElementById('fan-cards')).gridTemplateColumns.split(' ').length`);
  for(const columns of ['1','2','3','4']){
    await chooseColumns(columns);
    assert.equal(await renderedColumns(),Number(columns),'Viewer applies selected column count');
    assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'),true,`Viewer overflow with ${columns} columns`);
  }
  await reloadPage();
  await waitFor(`document.getElementById('connection-text')?.textContent.includes('已连接')`);
  assert.equal(await evaluate(`document.getElementById('fan-columns').value`),'4','Column preference survives reload');
  await mkdir('test-results',{recursive:true});
  for(const width of [1920,1440,320]){
    await send('Emulation.setDeviceMetricsOverride',{width,height:1100,deviceScaleFactor:1,mobile:false});
    assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'),true,`Viewer overflow at ${width}px`);
    if(width===320)assert.equal(await renderedColumns(),1,'Mobile uses one readable column');
    assert.equal(await evaluate(`Array.from(document.querySelectorAll('.view-card')).filter(card=>card.querySelector('[data-live="fault"]').hidden).every(card=>{const tick=card.querySelector('.fan-curve-preview>svg>g:last-child text'),gap=card.getBoundingClientRect().bottom-tick.getBoundingClientRect().bottom;return gap<=14&&parseFloat(getComputedStyle(card).paddingLeft)===12})`),true,`Viewer temperature ticks stay close to the card edge at ${width}px`);
    const shot=await send('Page.captureScreenshot',{format:'png',captureBeyondViewport:true});
    await writeFile(`test-results/viewer-${width}.png`,Buffer.from(shot.data,'base64'));
  }
  await evaluate(`(async()=>{window.originalViewerFetch=window.fetch;window.mockViewerState=await(await fetch('/api/view')).json();window.mockViewerState.view_revision='test-fixture';window.fetch=(...args)=>String(args[0]).split('?')[0]==='/api/view'?Promise.resolve(new Response(JSON.stringify(window.mockViewerState))):window.originalViewerFetch(...args);const f=window.mockViewerState.fans[0];f.mode='manual';f.points=[[20,10],[60,80]];f.output={...f.output,temperature:40};document.getElementById('retry').click()})()`);
  await waitFor(`document.querySelector('.card-name .chip-label').textContent.includes('手动模式')`);
  assert.equal(await evaluate(`document.querySelector('.fan-curve-preview>svg').getAttribute('aria-label').includes('曲线参考输出 45%')`),true,'Manual mode labels the curve as a reference');
  await evaluate(`window.__viewerCurve=document.querySelector('.fan-curve-preview>svg');Object.assign(window.mockViewerState.fans[0].output,{actual:53,target:65,requested:80,automatic:false});document.getElementById('retry').click()`);
  await waitFor(`document.querySelector('[data-live="actual"]').textContent==='53'`);
  assert.equal(await evaluate(`window.__viewerCurve===document.querySelector('.fan-curve-preview>svg')`),true,'Telemetry preserves viewer curve nodes');
  await evaluate(`(()=>{
    window.__spark=document.querySelector('#cpu-spark path');
    const snapshot=structuredClone(window.mockViewerState);
    snapshot.fans_delta=true;snapshot.history_delta=true;snapshot.history_cursor++;
    snapshot.history=[{time:snapshot.sample_time+1,cpu:61,gpu:42}];
    snapshot.fans=snapshot.fans.map(f=>({id:f.id,output:{...f.output,actual:54},pending:f.pending}));
    window.fetch=(...args)=>String(args[0]).split('?')[0]==='/api/view'?Promise.resolve(new Response(JSON.stringify(snapshot))):window.originalViewerFetch(...args);
    document.getElementById('retry').click();
  })()`);
  await waitFor(`document.querySelector('[data-live="actual"]').textContent==='54'`);
  assert.equal(await evaluate(`window.__viewerCurve===document.querySelector('.fan-curve-preview>svg') && window.__spark===document.querySelector('#cpu-spark path') && document.querySelector('.card-name .chip-label').textContent.includes('手动模式')`),true,'Incremental telemetry preserves configuration, curve and history nodes');
  await evaluate(`window.fetch=(...args)=>String(args[0]).split('?')[0]==='/api/view'?Promise.resolve(new Response(JSON.stringify(window.mockViewerState))):window.originalViewerFetch(...args);document.getElementById('retry').click()`);
  await waitFor(`document.querySelector('[data-live="actual"]').textContent==='53'`);
  assert.deepEqual(await evaluate(`(()=>{const card=document.querySelector('[data-fan]'),svg=card.querySelector('.fan-curve-preview>svg'),marker=svg.querySelector('[data-curve-current]'),line=marker.querySelector('line');return{target:card.querySelector('[data-output-target]').style.left,pointY:Number(marker.querySelector('circle').getAttribute('cy')),line:[...['x1','x2','y1','y2'].map(key=>Number(line.getAttribute(key)))],markerTransform:marker.getAttribute('transform'),lines:marker.querySelectorAll('line').length,noTargetPoint:!svg.querySelector('[data-curve-target]'),labelRemoved:!card.querySelector('.value-label'),countRemoved:!card.querySelector('.fan-curve-preview').textContent.includes('个节点'),border:getComputedStyle(card.querySelector('.fan-curve-preview')).borderWidth}})()`),{target:'80%',pointY:64-45*.52,line:[0,0,64,64-45*.52],markerTransform:`translate(${12+20/80*236} 0)`,lines:1,noTargetPoint:true,labelRemoved:true,countRemoved:true,border:'0px'},'A single temperature line ends at the interpolated curve point independently of actual and requested output');
  for(const target of [0,100]){
    await evaluate(`window.mockViewerState.fans[0].output.requested=${target};document.getElementById('retry').click()`);
    await waitFor(`document.querySelector('[data-output-target]').style.left==='${target}%'`);
  }
  await evaluate(`window.mockViewerState.fans[0].output.automatic=true;document.getElementById('retry').click()`);
  await waitFor(`document.querySelector('[data-output-target]').hidden && document.querySelector('[data-curve-current]').getAttribute('visibility')==='visible'`);
  await evaluate(`window.mockViewerState.fans[0].output.temperature=null;document.getElementById('retry').click()`);
  await waitFor(`document.querySelector('[data-curve-current]').getAttribute('visibility')==='hidden'`);
  await evaluate(`window.fetch=()=>Promise.reject(new Error('simulated disconnect'));document.getElementById('retry').click()`);
  await waitFor(`!document.getElementById('connection-error').hidden`);
  assert.equal(await evaluate(`Array.from(document.querySelectorAll('[data-curve-current]')).every(m=>m.getAttribute('visibility')==='hidden')`),true,'Disconnected viewer hides all current markers');
  assert.equal(await evaluate(`Array.from(document.querySelectorAll('[data-output-target]')).every(m=>m.hidden)&&!document.querySelector('[data-curve-target]')`),true,'Disconnected viewer hides bar targets and curves contain no target points');
  assert.equal(await evaluate(`document.querySelectorAll('.fan-curve-preview>svg').length===window.mockViewerState.fans.length`),true,'Disconnected viewer keeps configured curves');
  await evaluate(`window.fetch=window.originalViewerFetch;document.getElementById('retry').click()`);
  await waitFor(`document.getElementById('connection-error').hidden`);
  await evaluate(`(()=>{window.viewerFetch=window.fetch;window.viewerPolls=0;window.fetch=(...args)=>{if(String(args[0]).startsWith('/api/view'))window.viewerPolls++;return window.viewerFetch(...args);};Object.defineProperty(document,'hidden',{configurable:true,value:true});document.dispatchEvent(new Event('visibilitychange'));})()`);
  await new Promise(resolve=>setTimeout(resolve,1800));
  assert.equal(await evaluate('window.viewerPolls'),0,'Hidden viewer stops polling');
  await evaluate(`delete document.hidden;document.dispatchEvent(new Event('visibilitychange'))`);
  await waitFor('window.viewerPolls>0');
  assert.equal(requests.some(r=>r.method==='POST'),false,'Viewer sends no mutations or discovery heartbeats');
  assert.equal(requests.some(r=>['/app.js','/curve-editor.js','/confirm.js'].includes(new URL(r.url).pathname)),false,'Viewer does not load editor code');
  await send('Emulation.setDeviceMetricsOverride',{width:1440,height:1100,deviceScaleFactor:1,mobile:false});
  await send('Page.navigate',{url:new URL('/edit',url).href});
  await waitFor('document.getElementById("connection-text")?.textContent.includes("已连接") && document.querySelector("#curve-dialog")');
  assert.equal(await evaluate(`document.getElementById('fan-columns').value`),'4','Editor shares the viewer column preference');
  for(const columns of ['1','2','3','4']){
    await chooseColumns(columns);
    assert.equal(await renderedColumns(),Number(columns),'Editor applies selected column count');
    assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'),true,`Editor overflow with ${columns} columns`);
  }
  await chooseColumns('auto');
  assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'),true,'Desktop overflow');
  if(!snapshotOnly){
    assert.equal(await evaluate('(async()=>{const s=await(await fetch("/api/state")).json();return s.demo&&s.workspace_api===2})()'),true,'Mutations require the new demo backend');
    await evaluate(`(async()=>{const s=await(await fetch('/api/state')).json();for(const [path,body] of [['/api/config',{version:2,fans:[],curves:[]}],['/api/scan',{}]]){const r=await fetch(path,{method:'POST',headers:{'Content-Type':'application/json','X-FanControl-Token':s.csrf_token},body:JSON.stringify(body)});if(!r.ok)throw new Error(await r.text());}})()`);
    await reloadPage();
    await waitFor('document.querySelectorAll("[data-fan]").length===3');
    assert.equal(await evaluate(`Array.from(document.querySelectorAll('[data-fan]')).every(card=>{const feedback=card.querySelector('.card-feedback'),footer=card.querySelector('.card-footer');return feedback.textContent===''&&getComputedStyle(feedback).display==='none'&&footer.getBoundingClientRect().height<=36&&!card.querySelector('.preview-caption')})`),true,'Idle editor cards have a compact footer and no reserved feedback space');
    await evaluate(`document.querySelector('[data-create-curve]').click()`);
    await waitFor('document.getElementById("curve-dialog").open');
    assert.deepEqual(await evaluate(`Array.from(document.querySelectorAll('[data-point][data-axis="0"]')).map(el=>[Number(el.value),Number(document.querySelector('[data-point="'+el.dataset.point+'"][data-axis="1"]').value)])`),[[30,20],[50,50],[80,100]]);
    for(const [width,height] of [[1440,900],[1280,720],[1024,600],[768,900],[390,844],[320,640]]){
      await send('Emulation.setDeviceMetricsOverride',{width,height,deviceScaleFactor:1,mobile:width<580});
      await editorFits(`${width} × ${height}`);
      assert.equal(await evaluate(`(()=>{const widths=Array.from(document.querySelectorAll('.fan-card:first-child .card-controls .mode-control,.fan-card:first-child .card-controls .mode-choice')).map(el=>el.getBoundingClientRect().width);return widths.length===3&&Math.max(...widths)-Math.min(...widths)<1})()`),true,`Equal control widths at ${width}px`);
      assert.equal(await evaluate(`document.querySelector('.mode-buttons').children.length`),2,'Curve and manual share one segmented control');
    }
    await send('Emulation.setDeviceMetricsOverride',{width:1440,height:1100,deviceScaleFactor:1,mobile:false});
    await evaluate(`document.querySelector('#curve-dialog [data-close]').click()`);
    const saved=()=>evaluate('(async()=> (await(await fetch("/api/state")).json()).config)()');
    const config=await saved();
    assert.equal(config.fans.length,3);
    assert.equal(config.fans[0].min_duty,0);
    assert.deepEqual(config.fans[0].points,[[30,20],[50,50],[80,100]]);
    const first=config.fans[0].id,second=config.fans[1].id;
    const card=id=>`document.querySelector('[data-fan="${id}"]')`;
    const post=async(path,body)=>evaluate(`(async()=>{const s=await(await fetch('/api/state')).json();const r=await fetch(${JSON.stringify(path)},{method:'POST',headers:{'Content-Type':'application/json','X-FanControl-Token':s.csrf_token},body:JSON.stringify(${JSON.stringify(body)})});if(!r.ok)throw new Error(await r.text());return r.json()})()`);
    const drag=await evaluate(`(()=>{const a=${card(first)}.querySelector('[data-drag-handle]').getBoundingClientRect(),b=${card(second)}.getBoundingClientRect();return{from:{x:a.x+a.width/2,y:a.y+a.height/2},to:{x:b.x+b.width/2,y:b.y+b.height/2}}})()`);
    await send('Input.dispatchMouseEvent',{type:'mousePressed',...drag.from,button:'left',clickCount:1});
    await send('Input.dispatchMouseEvent',{type:'mouseMoved',...drag.to,buttons:1});
    await send('Input.dispatchMouseEvent',{type:'mouseReleased',...drag.to,button:'left',clickCount:1});
    await waitFor(`(async()=> (await(await fetch('/api/state')).json()).config.fans[0].id===${JSON.stringify(second)})()`);
    await reloadPage();
    await waitFor(`document.querySelector('[data-fan]')?.dataset.fan===${JSON.stringify(second)}`);
    await evaluate(`${card(first)}.querySelector('[data-drag-handle]').focus()`);
    await send('Input.dispatchKeyEvent',{type:'keyDown',key:'ArrowLeft',code:'ArrowLeft',modifiers:1});
    await send('Input.dispatchKeyEvent',{type:'keyUp',key:'ArrowLeft',code:'ArrowLeft'});
    await waitFor(`(async()=> (await(await fetch('/api/state')).json()).config.fans[0].id===${JSON.stringify(first)})()`);
    await waitFor(`document.querySelector('[data-fan]')?.dataset.fan===${JSON.stringify(first)}`);
    await send('Emulation.setDeviceMetricsOverride',{width:390,height:844,deviceScaleFactor:1,mobile:true});
    await send('Emulation.setTouchEmulationEnabled',{enabled:true,maxTouchPoints:1});
    await evaluate(`${card(first)}.scrollIntoView({block:'start'});window.scrollBy(0,-60)`);
    const touch=await evaluate(`(()=>{const a=${card(first)}.querySelector('[data-drag-handle]').getBoundingClientRect(),b=${card(second)}.getBoundingClientRect();return{from:{x:a.x+a.width/2,y:a.y+a.height/2},to:{x:b.x+b.width/2,y:b.y+30}}})()`);
    await send('Input.dispatchTouchEvent',{type:'touchStart',touchPoints:[touch.from]});
    await send('Input.dispatchTouchEvent',{type:'touchMove',touchPoints:[touch.to]});
    await send('Input.dispatchTouchEvent',{type:'touchEnd',touchPoints:[]});
    await waitFor(`(async()=> (await(await fetch('/api/state')).json()).config.fans[0].id===${JSON.stringify(second)})()`);
    await post('/api/fan-order',{ids:config.fans.map(f=>f.id)});
    await waitFor(`document.querySelector('[data-fan]')?.dataset.fan===${JSON.stringify(first)}`);
    await send('Emulation.setTouchEmulationEnabled',{enabled:false});
    await send('Emulation.setDeviceMetricsOverride',{width:1440,height:1100,deviceScaleFactor:1,mobile:false});
    await evaluate('window.scrollTo(0,0)');
    await evaluate(`document.querySelector('[data-tab="devices"]').click();document.querySelector('#discovered-fans [data-identify]').click()`);
    await waitFor('document.getElementById("identify-dialog").open');
    await evaluate(`document.getElementById('identify-kind').value='fan';document.getElementById('identify-kind').dispatchEvent(new Event('change'));document.getElementById('identify-full').click()`);
    await waitFor(`document.getElementById('identify-test-status').textContent.includes('正在以 100%')`);
    await evaluate(`document.getElementById('identify-slider').value=70;document.getElementById('identify-slider').dispatchEvent(new Event('input'));document.getElementById('identify-slider').dispatchEvent(new Event('change'))`);
    await waitFor(`document.getElementById('identify-test-status').textContent.includes('正在以 70%')`);
    await evaluate(`document.getElementById('identify-stop').click()`);
    await waitFor(`(async()=> (await(await fetch('/api/state')).json()).identification===null)()`);
    await waitFor(`!document.getElementById('identify-save').disabled`);
    await evaluate(`document.getElementById('identify-name').value='前置风扇';document.getElementById('identify-save').click()`);
    await waitFor(`!document.getElementById('identify-dialog').open`);
    await evaluate(`document.querySelector('[data-tab="control"]').click()`);
    await evaluate(`${card(first)}.querySelector('[data-field="source"]').value='@gpu';${card(first)}.querySelector('[data-field="source"]').dispatchEvent(new Event('change',{bubbles:true}))`);
    await waitFor(`(async()=> (await(await fetch('/api/state')).json()).config.fans[0].sensor_ids[0]==='@gpu')()`);
    await waitFor(`${card(first)}.querySelector('[data-edit-curve]').disabled===false`);
    const initialPoints=JSON.stringify((await saved()).fans[0].points);
    await evaluate(`${card(first)}.querySelector('[data-edit-curve]').click()`);
    await waitFor('document.getElementById("curve-dialog").open');
    assert.equal(await evaluate(`(()=>{const marker=document.querySelector('#curve-chart [data-plot-current]'),line=marker.querySelector('line');return marker.querySelectorAll('line').length===1&&line.getAttribute('x1')===line.getAttribute('x2')&&line.getAttribute('y2')===marker.querySelector('circle').getAttribute('cy')&&Number(line.getAttribute('y1'))>=Number(line.getAttribute('y2'))})()`),true,'The curve editor temperature line also stops at its curve intersection');
    assert.equal(await evaluate(`${card(first)}.querySelector('[data-edit-curve] svg[role="img"]')!==null`),true,'Card thumbnail opens curve editor');
    assert.equal(await evaluate(`${card(first)}.querySelector('.card-top [data-toggle]')!==null`),true,'Control toggle is in card header');
    assert.equal(await evaluate(`${card(first)}.querySelector('.progress [data-output-target]')!==null && !${card(first)}.querySelector('.value-label')`),true,'Target is represented on the output bar');
    await evaluate(`document.getElementById('preset').value='quiet';document.getElementById('preset').dispatchEvent(new Event('change'))`);
    assert.equal(await evaluate(`document.querySelectorAll('#point-list .point').length`),5,'All nodes are listed together');
    assert.equal(await evaluate(`document.querySelector('.point-pagination')===null`),true,'Node pagination is removed');
    assert.deepEqual(await evaluate(`Array.from(document.querySelectorAll('[data-point][data-axis="0"]')).map(el=>[el.min,el.max])`),Array(5).fill(['20','100']),'Temperature inputs use the fixed range');
    await evaluate(`(()=>{for(const [index,value] of [[0,0],[4,120]]){const input=document.querySelector('[data-point="'+index+'"][data-axis="0"]');input.value=value;input.dispatchEvent(new Event('change',{bubbles:true}))}})()`);
    assert.deepEqual(await evaluate(`Array.from(document.querySelectorAll('[data-point][data-axis="0"]')).map(el=>Number(el.value))`),[20,45,60,75,100],'Typing clamps temperatures to 20–100°C');
    assert.equal(await evaluate(`Array.from(document.querySelectorAll('#curve-chart text')).filter(el=>el.textContent.endsWith('°')).map(el=>el.textContent).join(',')`),'20°,30°,40°,50°,60°,70°,80°,90°,100°','Editing always retains the fixed temperature axis');
    await evaluate(`document.querySelector('#curve-chart [data-node="0"]').focus()`);
    await send('Input.dispatchKeyEvent',{type:'keyDown',key:'ArrowLeft',code:'ArrowLeft'});
    assert.equal(await evaluate(`document.querySelector('[data-point="0"][data-axis="0"]').value`),'20','Keyboard movement respects the left boundary');
    await evaluate(`window.__plotNode=document.querySelector('#curve-chart [data-node="2"]');window.__plotNode.focus({preventScroll:true})`);
    await new Promise(resolve=>setTimeout(resolve,1800));
    assert.equal(await evaluate(`window.__plotNode===document.querySelector('#curve-chart [data-node="2"]')&&document.activeElement===window.__plotNode`),true,'Polling preserves curve nodes and keyboard focus');

    await editorFits('five nodes');
    await send('Emulation.setDeviceMetricsOverride',{width:320,height:640,deviceScaleFactor:1,mobile:true});
    await editorFits('five independent nodes with sharing option at 320 × 640');
    await send('Emulation.setDeviceMetricsOverride',{width:1440,height:1100,deviceScaleFactor:1,mobile:false});
    await evaluate(`document.getElementById('point-list').scrollTop=0`);
    await evaluate(`document.getElementById('curve-chart').scrollIntoView({block:'center'})`);
    await evaluate(`new Promise(resolve=>requestAnimationFrame(()=>requestAnimationFrame(resolve)))`);
    const box=await evaluate(`(()=>{const b=document.querySelector('#curve-chart [data-node="2"]').getBoundingClientRect();return{x:b.x+b.width/2,y:b.y+b.height/2}})()`);
    await send('Input.dispatchMouseEvent',{type:'mouseMoved',x:box.x,y:box.y});
    await send('Input.dispatchMouseEvent',{type:'mousePressed',x:box.x,y:box.y,button:'left',clickCount:1});
    await send('Input.dispatchMouseEvent',{type:'mouseMoved',x:box.x+18,y:box.y-12,buttons:1});
    await send('Input.dispatchMouseEvent',{type:'mouseReleased',x:box.x+18,y:box.y-12,button:'left',clickCount:1});
    assert.notEqual(await evaluate(`document.querySelector('[data-point="2"][data-axis="1"]').value`),'42');
    await evaluate(`document.querySelector('#curve-dialog [data-close]').click()`);
    await confirmAction(false);
    assert.equal(await evaluate('document.getElementById("curve-dialog").open'),true,'Cancel preserves the editing dialog and draft');
    await evaluate(`document.querySelector('#curve-dialog [data-close]').click()`);
    await confirmAction();
    await waitFor('!document.getElementById("curve-dialog").open');
    assert.equal(JSON.stringify((await saved()).fans[0].points),initialPoints,'Cancel must leave running curve untouched');
    await evaluate(`${card(first)}.querySelector('[data-edit-curve]').click();document.getElementById('curve-share').checked=true;document.getElementById('curve-share').dispatchEvent(new Event('change'));document.getElementById('curve-name').value='日常曲线';document.getElementById('preset').value='quiet';document.getElementById('preset').dispatchEvent(new Event('change'));document.getElementById('curve-form').requestSubmit()`);
    await waitFor('!document.getElementById("curve-dialog").open');
    const curve=(await saved()).curves[0];
    assert.equal(curve.name,'日常曲线');
    assert.equal((await saved()).fans[0].curve_id,curve.id);
    await evaluate(`${card(second)}.querySelector('[data-field="curve_id"]').value=${JSON.stringify(curve.id)};${card(second)}.querySelector('[data-field="curve_id"]').dispatchEvent(new Event('change',{bubbles:true}))`);
    await waitFor(`(async()=> (await(await fetch('/api/state')).json()).config.fans[1].curve_id===${JSON.stringify(curve.id)})()`);
    await waitFor(`${card(second)}.querySelector('[data-field="source"]').disabled===false`);
    await evaluate(`${card(second)}.querySelector('[data-field="source"]').value='@cpu-gpu';${card(second)}.querySelector('[data-field="source"]').dispatchEvent(new Event('change',{bubbles:true}))`);
    await waitFor(`(async()=> (await(await fetch('/api/state')).json()).config.fans[1].sensor_ids[0]==='@cpu-gpu')()`);
    await evaluate(`document.querySelector('[data-library-edit]').click();document.getElementById('preset').value='performance';document.getElementById('preset').dispatchEvent(new Event('change'));document.getElementById('curve-form').requestSubmit()`);
    await waitFor('!document.getElementById("curve-dialog").open');
    assert.deepEqual((await saved()).fans.slice(0,2).map(f=>f.sensor_ids),[['@gpu'],['@cpu-gpu']]);
    assert.equal(await evaluate('document.querySelector("[data-library-delete]").disabled'),true);
    await evaluate(`${card(first)}.querySelector('[data-mode="manual"]').click()`);
    await waitFor(`${card(first)}.querySelector('[data-manual]')?.disabled===false`);
    await evaluate(`${card(first)}.querySelector('[data-manual]').value=67;${card(first)}.querySelector('[data-manual]').dispatchEvent(new Event('input',{bubbles:true}));${card(first)}.querySelector('[data-manual]').dispatchEvent(new Event('change',{bubbles:true}))`);
    await waitFor(`(async()=> (await(await fetch('/api/state')).json()).config.fans[0].manual_duty===67)()`);
    await waitFor(`${card(first)}.querySelector('[data-field="curve_id"]').disabled===false`);
    assert.equal(await evaluate(`${card(first)}.querySelector('[data-field="curve_id"]').value`),'@manual');
    await evaluate(`${card(first)}.querySelector('[data-field="curve_id"]').value=${JSON.stringify(curve.id)};${card(first)}.querySelector('[data-field="curve_id"]').dispatchEvent(new Event('change',{bubbles:true}))`);
    await waitFor(`(async()=>{const f=(await(await fetch('/api/state')).json()).config.fans[0];return f.mode==='curve'&&f.curve_id===${JSON.stringify(curve.id)}})()`);
    await waitFor(`${card(first)}.querySelector('[data-mode="manual"]').disabled===false`);
    await evaluate(`${card(first)}.querySelector('[data-mode="manual"]').click()`);
    await waitFor(`${card(first)}.querySelector('[data-manual]')?.disabled===false`);
    assert.equal((await saved()).fans[0].manual_duty,67,'Mode changes preserve manual output');
    await waitFor(`${card(first)}.querySelector('[data-toggle]').disabled===false`);
    await evaluate(`${card(first)}.querySelector('[data-toggle]').click()`);
    await waitFor(`(async()=> (await(await fetch('/api/state')).json()).outputs[${JSON.stringify(first)}].active)()`);
    await waitFor(`${card(first)}.querySelector('[data-device]').disabled===false`);
    await evaluate(`${card(first)}.querySelector('[data-device]').click();document.getElementById('min-duty').value=35;document.getElementById('min-duty').dispatchEvent(new Event('input',{bubbles:true}));document.getElementById('device-form').requestSubmit()`);
    await waitFor('!document.getElementById("device-dialog").open');
    assert.equal((await saved()).fans[0].min_duty,35);
    await evaluate(`${card(first)}.querySelector('[data-device]').click();document.getElementById('min-duty').value=0;document.getElementById('min-duty').dispatchEvent(new Event('input',{bubbles:true}));document.getElementById('device-form').requestSubmit()`);
    await waitFor('!document.getElementById("device-dialog").open');
    assert.equal((await saved()).fans[0].min_duty,0,'Zero minimum saves through settings');
    await evaluate(`document.querySelector('[data-tab="devices"]').click();document.querySelector('#discovered-fans [data-identify]').click();document.getElementById('identify-full').click()`);
    await waitFor(`document.getElementById('identify-test-status').textContent.includes('正在以 100%')`);
    await evaluate(`document.getElementById('identify-close').click()`);
    await waitFor('!document.getElementById("identify-dialog").open');
    await waitFor(`(async()=> (await(await fetch('/api/state')).json()).outputs[${JSON.stringify(first)}].active)()`);
    await evaluate(`document.getElementById('scan-fans').click()`);
    await waitFor('!document.getElementById("scan-fans").disabled');assert.equal((await saved()).fans.length,3);
    await evaluate(`document.getElementById('add-fan').click();document.getElementById('new-fan-name').value='离线风扇';document.getElementById('add-form').requestSubmit()`);
    await waitFor('!document.getElementById("add-dialog").open');
    const offline=(await saved()).fans.find(f=>f.name==='离线风扇');assert.ok(offline);assert.equal(offline.channel_id,null);
    await evaluate(`document.querySelector('[data-tab="control"]').click();${card(offline.id)}.querySelector('[data-device]').click();document.getElementById('delete-fan').click()`);
    await confirmAction();
    await waitFor('!document.getElementById("device-dialog").open');assert.equal((await saved()).fans.length,3);
    await evaluate(`document.getElementById('new-curve').click();document.getElementById('curve-name').value='未分配曲线';document.getElementById('curve-form').requestSubmit()`);
    await waitFor('!document.getElementById("curve-dialog").open');
    assert.equal((await saved()).curves.length,2);
    const free=(await saved()).curves.find(c=>c.name==='未分配曲线');
    await evaluate(`document.querySelector('[data-library-copy="${free.id}"]').click();document.getElementById('curve-form').requestSubmit()`);
    await waitFor('!document.getElementById("curve-dialog").open');assert.equal((await saved()).curves.length,3);
    await evaluate(`document.querySelector('[data-library-delete="${free.id}"]').click()`);
    await confirmAction();
    await waitFor(`(async()=> (await(await fetch('/api/state')).json()).config.curves.length===2)()`);
    // Another client changes the shared curve while this editor is open.
    await evaluate(`document.querySelector('[data-library-edit="${curve.id}"]').click()`);
    const latest=(await saved()).curves.find(c=>c.id===curve.id);
    await post('/api/curve',{curve:{...latest,name:'来自另一页面'},expected:latest});
    await evaluate(`document.getElementById('curve-form').requestSubmit()`);
    await waitFor(`!document.getElementById('curve-error').hidden`);
    assert.equal(await evaluate(`document.getElementById('curve-dialog').open`),true);
    await evaluate(`document.querySelector('#curve-dialog [data-close]').click()`);
    await waitFor('!document.getElementById("curve-dialog").open');
    await reloadPage();
    await waitFor(`${card(first)}?.querySelector('[data-manual]')?.value==='67'`);
    assert.deepEqual((await saved()).fans.slice(0,2).map(f=>f.sensor_ids),[['@gpu'],['@cpu-gpu']]);
    assert.equal((await saved()).curves.find(c=>c.id===curve.id).name,'来自另一页面');
    const many=await post('/api/curve',{curve:{name:'滚动测试',points:Array.from({length:32},(_,i)=>[i*3,Math.min(100,i*4)])}});
    await waitFor(`document.querySelector('[data-library-edit="${many.id}"]')!==null`);
    await evaluate(`document.querySelector('[data-library-edit="${many.id}"]').click()`);
    assert.equal(await evaluate(`document.querySelectorAll('#point-list .point').length`),32,'All 32 nodes are rendered without pagination');
    for(const [width,height] of [[1440,900],[1024,600],[390,844],[320,640]]){
      await send('Emulation.setDeviceMetricsOverride',{width,height,deviceScaleFactor:1,mobile:width<580});
      await editorFits(`32 scrollable nodes at ${width} × ${height}`);
      assert.equal(await evaluate(`(()=>{const list=document.getElementById('point-list');return list.scrollHeight>list.clientHeight})()`),true,'Only the node list scrolls');
      const before=await evaluate(`['curve-chart','save-curve'].map(id=>document.getElementById(id).getBoundingClientRect().top)`);
      await evaluate(`document.getElementById('point-list').scrollTop=document.getElementById('point-list').scrollHeight`);
      assert.equal(await evaluate(`(()=>{const list=document.getElementById('point-list').getBoundingClientRect(),last=document.querySelector('[data-point="31"]').closest('.point').getBoundingClientRect();return last.top>=list.top-1&&last.bottom<=list.bottom+1})()`),true,'Scrolling reaches the final node');
      assert.deepEqual(await evaluate(`['curve-chart','save-curve'].map(id=>document.getElementById(id).getBoundingClientRect().top)`),before,'Graph and save button stay fixed while scrolling nodes');
    }
    await evaluate(`document.getElementById('point-list').scrollTop=0;const node=document.querySelector('#curve-chart [data-node="31"]');node.focus({preventScroll:true});node.dispatchEvent(new KeyboardEvent('keydown',{key:'ArrowDown',bubbles:true}))`);
    assert.equal(await evaluate(`document.getElementById('point-list').scrollTop>0`),true,'Selecting a graph node reveals its list entry');
    await evaluate(`const input=document.querySelector('[data-point="31"][data-axis="1"]');input.value=98;input.dispatchEvent(new Event('change',{bubbles:true}))`);
    assert.equal(await evaluate(`document.getElementById('point-list').scrollTop>0&&document.querySelector('[data-point="31"][data-axis="1"]').value==='98'`),true,'Editing preserves the scrolled node position');
    await send('Emulation.setDeviceMetricsOverride',{width:1440,height:1100,deviceScaleFactor:1,mobile:false});
    await evaluate(`document.querySelector('#curve-dialog [data-close]').click()`);
    await confirmAction();
    await post('/api/curve/delete',{id:many.id});
    // Navigation is addressable and follows browser history, including reloads.
    await evaluate(`document.querySelector('nav [data-tab="devices"]').click();document.querySelector('nav [data-tab="sensors"]').click();history.back()`);
    await waitFor(`document.querySelector('nav [aria-current="page"]').dataset.tab==='devices'`);
    await evaluate('history.forward()');
    await waitFor(`document.querySelector('nav [aria-current="page"]').dataset.tab==='sensors'`);
    await reloadPage();
    await waitFor(`document.getElementById('connection-text').textContent.includes('已连接')`);
    assert.equal(await evaluate(`!document.getElementById('tab-sensors').hidden&&document.title.startsWith('硬件传感器')`),true,'Reload retains the selected page and document title');
    await evaluate(`document.querySelector('nav [data-tab="control"]').click();const source=${card(first)}.querySelector('[data-field="source"]');source.focus();source.dispatchEvent(new Event('change',{bubbles:true}))`);
    await waitFor(`${card(first)}.querySelector('[data-field="source"]').disabled===false`);
    assert.equal(await evaluate(`document.activeElement===${card(first)}.querySelector('[data-field="source"]')`),true,'Automatic save restores keyboard focus');

    await evaluate(`document.getElementById('new-curve').click();document.getElementById('curve-name').value='键盘保存回归';window.__pointInput=document.querySelector('[data-point="0"][data-axis="0"]');window.__pointInput.focus();window.__pointInput.select()`);
    await send('Input.insertText',{text:'31'});
    await send('Input.dispatchKeyEvent',{type:'keyDown',key:'Tab',code:'Tab'});
    await send('Input.dispatchKeyEvent',{type:'keyUp',key:'Tab',code:'Tab'});
    assert.equal(await evaluate(`window.__pointInput===document.querySelector('[data-point="0"][data-axis="0"]')&&document.activeElement.matches('[data-point="0"][data-axis="1"]')`),true,'Editing a node preserves its input and natural Tab order');
    await evaluate(`window.__pointInput.focus();window.__pointInput.select()`);
    await send('Input.insertText',{text:'32'});
    await evaluate(`window.__uxFetch=window.fetch;window.__saveCalls=0;window.fetch=(url,...args)=>String(url)==='/api/curve'?(window.__saveCalls++,new Promise(resolve=>window.__finishSave=resolve)):window.__uxFetch(url,...args)`);
    await send('Input.dispatchKeyEvent',{type:'keyDown',key:'s',code:'KeyS',modifiers:2});
    await send('Input.dispatchKeyEvent',{type:'keyUp',key:'s',code:'KeyS'});
    await waitFor(`typeof window.__finishSave==='function'`);
    assert.equal(await evaluate(`document.getElementById('curve-dialog').getAttribute('aria-busy')==='true'&&document.getElementById('save-curve').textContent.includes('正在保存')&&Array.from(document.querySelectorAll('#curve-dialog button,#curve-dialog input,#curve-dialog select')).every(el=>el.disabled)`),true,'Pending save locks all form edits and shows progress');
    await evaluate(`document.getElementById('curve-form').requestSubmit();document.querySelector('#curve-chart [data-node="0"]').dispatchEvent(new KeyboardEvent('keydown',{key:'ArrowRight',bubbles:true}));window.__finishSave(new Response(JSON.stringify({error:'模拟保存失败，草稿保留'}),{status:409}))`);
    await waitFor(`!document.getElementById('save-curve').disabled`);
    assert.equal(await evaluate('window.__saveCalls'),1,'Repeated save does not submit a second request');
    assert.equal(await evaluate(`document.querySelector('[data-point="0"][data-axis="0"]').value`),'32','Ctrl+S commits the focused number before saving and locks the graph');
    assert.equal(await evaluate(`document.getElementById('curve-dialog').open&&!document.getElementById('curve-error').hidden&&!document.getElementById('curve-name').disabled`),true,'Failure keeps the draft open and editable');
    await evaluate(`window.fetch=(url,...args)=>String(url).startsWith('/api/state')?Promise.reject(new Error('模拟断线')):window.__uxFetch(url,...args);document.getElementById('retry').click()`);
    await waitFor(`document.getElementById('save-curve').disabled`);
    assert.equal(await evaluate(`!document.querySelector('#curve-dialog [data-connection-status]').hidden&&document.getElementById('curve-name').value==='键盘保存回归'`),true,'Disconnection explains why saving is disabled without discarding the draft');
    await evaluate(`window.fetch=window.__uxFetch;document.getElementById('retry').click()`);
    await waitFor(`!document.getElementById('save-curve').disabled`);
    await evaluate(`document.querySelector('#curve-dialog [data-close]').click()`);
    await waitFor(`document.getElementById('confirm-dialog').open`);
    await send('Input.dispatchKeyEvent',{type:'keyDown',key:'s',code:'KeyS',modifiers:2});
    await send('Input.dispatchKeyEvent',{type:'keyUp',key:'s',code:'KeyS'});
    assert.equal(await evaluate(`document.getElementById('confirm-dialog').open&&document.getElementById('curve-dialog').getAttribute('aria-busy')==='false'`),true,'Save shortcut cannot bypass a confirmation dialog');
    await confirmAction(false);
    await evaluate(`document.getElementById('curve-form').requestSubmit()`);
    await waitFor(`!document.getElementById('curve-dialog').open`);
    const keyboardCurve=(await saved()).curves.find(c=>c.name==='键盘保存回归');
    assert.equal(keyboardCurve.points[0][0],32,'Recovered draft saves the committed keyboard edit');
    await post('/api/curve/delete',{id:keyboardCurve.id});
    for(const name of ['sensors','diagnostics','devices','control'])await evaluate(`document.querySelector('[data-tab="${name}"]').click()`);
    await evaluate(`(()=>{
      window.__originalFetch=window.fetch;window.__stateReads=0;
      window.fetch=(...args)=>{if(String(args[0]).startsWith('/api/state'))window.__stateReads++;return window.__originalFetch(...args)};
      Object.defineProperty(document,'hidden',{configurable:true,value:true});
      Object.defineProperty(document,'visibilityState',{configurable:true,value:'hidden'});
      document.dispatchEvent(new Event('visibilitychange'));
    })()`);
    await new Promise(resolve=>setTimeout(resolve,1800));
    assert.equal(await evaluate('window.__stateReads'),0,'Hidden page stops state polling');
    await evaluate(`delete document.hidden;delete document.visibilityState;document.dispatchEvent(new Event('visibilitychange'))`);
    await waitFor('window.__stateReads>0');
    await evaluate('window.fetch=window.__originalFetch;delete window.__originalFetch;delete window.__stateReads');
  }
  await mkdir('test-results',{recursive:true});
  // Delay page modules to verify the first painted palette is independent of them.
  const selectTheme=mode=>evaluate(`(()=>{const select=document.getElementById('theme-select');select.value=${JSON.stringify(mode)};select.dispatchEvent(new Event('change'))})()`);
  const deviceTheme=value=>send('Emulation.setEmulatedMedia',{features:[{name:'prefers-color-scheme',value}]});
  const frameProbe=await send('Page.addScriptToEvaluateOnNewDocument',{source:`window.__themeFrames=[];requestAnimationFrame(function sample(){if(document.body)window.__themeFrames.push([document.documentElement.dataset.theme,getComputedStyle(document.documentElement).backgroundColor]);if(window.__themeFrames.length<60)requestAnimationFrame(sample)})`});
  for(const path of ['/','/edit']){
    await deviceTheme('dark');
    for(const [saved,expected] of [[null,'dark'],['dark','dark'],['light','light']]){
      await evaluate(saved===null?`localStorage.removeItem('fancontrollx-theme')`:`localStorage.setItem('fancontrollx-theme',${JSON.stringify(saved)})`);
      await send('Fetch.enable',{patterns:[{urlPattern:'*/app.js'},{urlPattern:'*/view.js'}]});
      const previousOrigin=await evaluate('performance.timeOrigin');
      const navigation=send('Page.navigate',{url:new URL(path,url).href});
      await waitFor(`performance.timeOrigin!==${previousOrigin} && window.__themeFrames?.length>0 && performance.getEntriesByType('paint').some(p=>p.name==='first-contentful-paint')`);
      assert.ok(pausedModules.length>0,'Page modules are still blocked at first paint');
      const frames=await evaluate('window.__themeFrames');
      assert.ok(frames.every(([theme,bg])=>theme===expected&&bg===(expected==='dark'?'rgb(20, 20, 22)':'rgb(247, 247, 248)')),`${path}: correct palette from first frame with preference ${saved}: ${JSON.stringify(frames)}`);
      for(const requestId of pausedModules.splice(0))await send('Fetch.continueRequest',{requestId});
      await send('Fetch.disable');
      await navigation;
      await waitFor('document.readyState==="complete" && document.getElementById("connection-text")?.textContent.includes("已连接")');
      assert.equal(await evaluate(`document.getElementById('theme-select').value`),saved??'system');
    }
    await selectTheme('system');
    await deviceTheme('light');
    await waitFor(`document.documentElement.dataset.theme==='light'`);
    await deviceTheme('dark');
    await waitFor(`document.documentElement.dataset.theme==='dark'`);
    await selectTheme('light');
    await deviceTheme('light');
    await deviceTheme('dark');
    assert.equal(await evaluate('document.documentElement.dataset.theme'),'light','Explicit light overrides a dark device');
    await selectTheme('dark');
    await deviceTheme('light');
    assert.equal(await evaluate('document.documentElement.dataset.theme'),'dark','Explicit dark overrides a light device');
  }
  await send('Page.removeScriptToEvaluateOnNewDocument',{identifier:frameProbe.identifier});
  await evaluate(`localStorage.setItem('fancontrollx-theme','invalid')`);
  await reloadPage();
  assert.equal(await evaluate(`document.getElementById('theme-select').value`),'system','Invalid preferences fall back to device mode');
  await selectTheme('system');
  await reloadPage();
  assert.equal(await evaluate(`document.getElementById('theme-select').value`),'system','Device mode survives reload');
  const blockedStorage=await send('Page.addScriptToEvaluateOnNewDocument',{source:`Object.defineProperty(window,'localStorage',{get(){throw new DOMException('Storage disabled','SecurityError')}})`});
  await deviceTheme('dark');
  await reloadPage();
  assert.equal(await evaluate('document.documentElement.dataset.theme'),'dark','Blocked storage defaults to device mode');
  await selectTheme('light');
  assert.equal(await evaluate('document.documentElement.dataset.theme'),'light','Theme can still change without storage');
  await send('Page.removeScriptToEvaluateOnNewDocument',{identifier:blockedStorage.identifier});
  await reloadPage();
  await selectTheme('dark');
  assert.equal(await evaluate('document.documentElement.dataset.theme'),'dark');
  await reloadPage();
  await waitFor('document.getElementById("connection-text")?.textContent.includes("已连接")');
  assert.equal(await evaluate('document.documentElement.dataset.theme'),'dark');
  assert.equal(await evaluate(`document.getElementById('theme-select').value`),'dark');
  await evaluate('window.scrollTo(0,0)');
  const prefix=snapshotOnly?'real':'demo';
  const fullScreenshot=async()=>{
    await evaluate('Promise.all(document.getAnimations().map(animation=>animation.finished.catch(()=>{})))');
    const {cssContentSize}=await send('Page.getLayoutMetrics');
    return send('Page.captureScreenshot',{format:'png',captureBeyondViewport:true,clip:{x:0,y:0,width:cssContentSize.width,height:cssContentSize.height,scale:1}});
  };
  const dark=await fullScreenshot();
  await writeFile(`test-results/${prefix}-desktop-dark.png`,Buffer.from(dark.data,'base64'));
  await selectTheme('light');
  assert.equal(await evaluate('document.documentElement.dataset.theme'),'light');
  for(const width of [320,768,1024]){
    await send('Emulation.setDeviceMetricsOverride',{width,height:900,deviceScaleFactor:1,mobile:width<580});
    for(const name of ['devices','sensors','diagnostics','control']){
      await evaluate(`document.querySelector('nav [data-tab="${name}"]').click()`);
      assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'),true,`${name} overflow at ${width}px`);
      assert.equal(await evaluate(`document.querySelector('nav [aria-current="page"]').dataset.tab`),name);
    }
  }
  await send('Emulation.setDeviceMetricsOverride',{width:1440,height:1100,deviceScaleFactor:1,mobile:false});
  const desktop=await fullScreenshot();
  await writeFile(`test-results/${prefix}-desktop.png`,Buffer.from(desktop.data,'base64'));
  await send('Emulation.setDeviceMetricsOverride',{width:390,height:844,deviceScaleFactor:1,mobile:true});
  await new Promise(resolve=>setTimeout(resolve,300));
  assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'),true,'Mobile overflow');
  const mobile=await send('Page.captureScreenshot',{format:'png',captureBeyondViewport:true});
  await writeFile(`test-results/${prefix}-mobile.png`,Buffer.from(mobile.data,'base64'));
  if(!snapshotOnly){
    await evaluate(`document.querySelector('[data-edit-curve]').click()`);
    await waitFor('document.getElementById("curve-dialog").open');
    await send('Emulation.setEmulatedMedia',{features:[{name:'prefers-reduced-motion',value:'reduce'}]});
    assert.equal(await evaluate(`getComputedStyle(document.getElementById('curve-dialog')).animationName`),'none');
    assert.equal(await evaluate('document.getElementById("curve-dialog").scrollWidth <= document.getElementById("curve-dialog").clientWidth'),true,'Mobile editor overflow');
    await editorFits('mobile with shared curve');
    const editor=await send('Page.captureScreenshot',{format:'png'});
    await writeFile('test-results/demo-mobile-editor.png',Buffer.from(editor.data,'base64'));
    await evaluate(`document.querySelector('#curve-dialog [data-close]').click()`);
    // UI-only NVIDIA fixture. API polling reads this snapshot; no synthetic
    // GPU operation is submitted to the backend and no real hardware is used.
    await evaluate(`(async()=>{
      const snapshot=await(await fetch('/api/state')).json(),sensor='nvidia/GPU-aaaaaaaa-bbbb-cccc-dddd-eeeeeeeeeeee',channel=sensor+'/fan0';
      const f={...structuredClone(snapshot.config.fans[0]),id:'browser-nvidia',name:'显卡风扇 1',channel_id:channel,rpm_sensor_id:channel+'/rpm',sensor_ids:[sensor],mode:'manual',manual_duty:20,min_duty:20,max_duty:100,identified:false,device_kind:'unknown',autostart:false,curve_id:null};
      const c={id:channel,name:'NVIDIA RTX <test> · 风扇 1',chip:'nvidia',backend:'nvidia',duty:0,mode:0,writable:true,present:true,min_duty:30,max_duty:100,software_watchdog:true,curve_zero_auto:true,temperature_sensor_id:sensor,rpm_sensor_id:channel+'/rpm'};
      snapshot.sensors.push({id:sensor,name:'NVIDIA RTX <test>',chip:'nvidia',kind:'gpu',value:40});
      snapshot.config.fans.push(f);snapshot.channels.push(c);snapshot.fans.push({id:channel+'/rpm',name:c.name,chip:'nvidia',rpm:0});
      snapshot.discovery.push({...c,connection:'present',rpm:0,fan_id:f.id,identified:false,device_kind:'unknown',display_name:f.name});
      snapshot.outputs[f.id]={actual:0,target:30,requested:20,rpm:0,temperature:40,active:false,fault:null};
      const original=window.fetch.bind(window);window.fetch=(url,...args)=>String(url).split('?')[0]==='/api/state'?Promise.resolve(new Response(JSON.stringify(snapshot),{headers:{'Content-Type':'application/json'}})):original(url,...args);
      window.nvidiaFixture=snapshot;
      document.querySelector('nav [data-tab="devices"]').click();
    })()`);
    await waitFor(`document.querySelector('#discovered-fans [data-channel^="nvidia/"]')!==null`);
    assert.equal(await evaluate(`document.querySelector('#discovered-fans [data-channel^="nvidia/"]')!==null`),true,'Zero RPM GPU must be visible among online devices');
    await evaluate(`document.querySelector('nav [data-tab="control"]').click()`);
    await waitFor(`document.querySelector('[data-fan="browser-nvidia"]')!==null`);
    const gpuCard=`document.querySelector('[data-fan="browser-nvidia"]')`;
    assert.equal(await evaluate(`${gpuCard}.querySelector('[data-manual]').min`),'30','GPU slider respects driver minimum');
    assert.equal(await evaluate(`${gpuCard}.querySelector('[data-live="manual"]').textContent`),'30%','Manual output label agrees with clamped slider');
    assert.equal(await evaluate(`${gpuCard}.querySelector('[data-field="source"]').selectedOptions[0].textContent`),'NVIDIA RTX <test>','GPU source names render as text');
    assert.equal(await evaluate(`${gpuCard}.querySelector('[data-live="policy"]').textContent`),'显卡自动调速');
    await evaluate(`${gpuCard}.querySelector('[data-device]').click()`);
    assert.equal(await evaluate(`document.getElementById('min-duty').min`),'30');
    assert.equal(await evaluate(`document.getElementById('device-hardware-help').textContent.includes('30–100%')`),true);
    await evaluate(`document.querySelector('#device-dialog [data-close]').click();${gpuCard}.querySelector('[data-identify]').click()`);
    assert.equal(await evaluate(`document.getElementById('identify-kind').value`),'fan');
    assert.equal(await evaluate(`document.getElementById('identify-kind').disabled`),true);
    await evaluate(`document.getElementById('identify-close').click()`);
    await evaluate(`(()=>{const s=window.nvidiaFixture,f=s.config.fans.find(f=>f.id==='browser-nvidia');f.mode='curve';f.identified=true;f.autostart=true;f.device_kind='fan';f.points=[[25,0],[45,0],[55,50],[85,100]];Object.assign(s.outputs[f.id],{actual:37,rpm:740,target:0,requested:0,automatic:true,active:true});})()`);
    await waitFor(`${gpuCard}.querySelector('[data-live="status"]').textContent==='显卡自动调速'`);
    assert.equal(await evaluate(`${gpuCard}.querySelector('[data-live="policy"]').textContent.includes('曲线 0%')`),true);
    assert.equal(await evaluate(`${gpuCard}.querySelector('[data-live="actual"]').textContent`),'37','Native mode still shows real reported output');
    assert.equal(await evaluate(`${gpuCard}.querySelector('[data-toggle]').checked`),true,'Automatic delegation retains enabled policy');
    assert.equal(await evaluate(`${gpuCard}.querySelector('[data-live="feedback"]').textContent.includes('由显卡决定')`),true);
    await evaluate(`${gpuCard}.querySelector('[data-edit-curve]').click()`);
    assert.equal(await evaluate(`document.getElementById('curve-behavior-help').textContent.includes('0%')`),true);
    assert.equal(await evaluate(`document.getElementById('chart-current').textContent.includes('显卡自动模式')`),true);
    await evaluate(`document.querySelector('#curve-dialog [data-close]').click()`);
    await evaluate(`Object.assign(window.nvidiaFixture.outputs['browser-nvidia'],{automatic:false,target:100,requested:100,actual:100,fault:'显卡自身温度异常，使用最大输出'})`);
    await waitFor(`${gpuCard}.querySelector('[data-live="status"]').textContent==='保护运行'`);
    assert.equal(await evaluate(`${gpuCard}.querySelector('[data-live="feedback"]').textContent.includes('温度异常')`),true);
    await evaluate(`Object.assign(window.nvidiaFixture.outputs['browser-nvidia'],{automatic:true,target:0,requested:0,actual:37,fault:null})`);
    await waitFor(`${gpuCard}.querySelector('[data-live="status"]').textContent==='显卡自动调速'`);
    for(const width of [320,1440]){
      await send('Emulation.setDeviceMetricsOverride',{width,height:1000,deviceScaleFactor:1,mobile:width<580});
      assert.equal(await evaluate('document.documentElement.scrollWidth <= innerWidth'),true,`NVIDIA layout overflow at ${width}px`);
    }
    const gpuScreenshot=await fullScreenshot();
    await writeFile('test-results/demo-nvidia-desktop.png',Buffer.from(gpuScreenshot.data,'base64'));
    await reloadPage();
  }
  assert.deepEqual(errors,[],'Browser JavaScript exceptions');
  console.log(`PASS: ${snapshotOnly?'read-only telemetry and responsive layout':'identification and restoration, independent sources, shared curves, drag/cancel/apply, manual slider, control switch, zero minimum, offline CRUD, curve copy/delete, edit conflict, mouse/touch/keyboard card reorder and persistence, independently scrolling nodes, fixed graph/actions and desktop/mobile layouts'}; first-painted theme with delayed modules, device theme changes, explicit overrides, preference persistence, blocked storage and 320/768/1024px navigation layouts${snapshotOnly?'':'; NVIDIA zero RPM discovery, source selection, driver bounds, zero-curve automatic status and protection'}`);
}finally{
  socket?.close();
  browser.kill('SIGTERM');
  await new Promise(resolve=>{browser.once('exit',resolve);setTimeout(resolve,2000);});
  await rm(profile,{recursive:true,force:true,maxRetries:10,retryDelay:100});
}
