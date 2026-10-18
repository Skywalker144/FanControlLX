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
const browser=spawn(browserPath,['--headless','--no-sandbox','--disable-dev-shm-usage','--disable-gpu','--remote-debugging-port=0',`--user-data-dir=${profile}`,'about:blank'],{stdio:['ignore','ignore','pipe']});
let socket;
try{
  const endpoint=await new Promise((resolve,reject)=>{
    let output='';const timer=setTimeout(()=>reject(new Error('Chromium startup timed out')),15000);
    browser.stderr.on('data',chunk=>{output+=chunk;const match=output.match(/DevTools listening on (ws:\/\/[^\s]+)/);if(match){clearTimeout(timer);resolve(match[1]);}});
    browser.on('exit',code=>{clearTimeout(timer);reject(new Error(`Chromium exited ${code}: ${output.slice(-1500)}`));});
  });
  socket=new WebSocket(endpoint);
  await new Promise((resolve,reject)=>{socket.addEventListener('open',resolve,{once:true});socket.addEventListener('error',reject,{once:true});});
  let next=0,session;const pending=new Map(),errors=[];
  const send=(method,params={},sessionId=session)=>new Promise((resolve,reject)=>{
    const id=++next;const timer=setTimeout(()=>{pending.delete(id);reject(new Error(`${method} timed out`));},12000);
    pending.set(id,{resolve,reject,timer});socket.send(JSON.stringify({id,method,params,...(sessionId?{sessionId}:{})}));
  });
  socket.addEventListener('message',event=>{
    const value=JSON.parse(event.data);
    if(value.id){const p=pending.get(value.id);if(p){clearTimeout(p.timer);pending.delete(value.id);value.error?p.reject(new Error(JSON.stringify(value.error))):p.resolve(value.result);}}
    if(value.method==='Runtime.exceptionThrown')errors.push(value.params.exceptionDetails.text+': '+(value.params.exceptionDetails.exception?.description||''));
    if(value.method==='Page.javascriptDialogOpening')send('Page.handleJavaScriptDialog',{accept:true}).catch(()=>{});
  });
  const target=await send('Target.createTarget',{url:'about:blank'},null);
  session=(await send('Target.attachToTarget',{targetId:target.targetId,flatten:true},null)).sessionId;
  await send('Page.enable');await send('Runtime.enable');
  await send('Emulation.setDeviceMetricsOverride',{width:1440,height:1100,deviceScaleFactor:1,mobile:false});
  await send('Page.navigate',{url});
  const evaluate=async expression=>{
    const result=await send('Runtime.evaluate',{expression,returnByValue:true,awaitPromise:true});
    if(result.exceptionDetails)throw new Error(result.exceptionDetails.exception?.description||result.exceptionDetails.text);
    return result.result.value;
  };
  const waitFor=async expression=>{const deadline=Date.now()+10000;while(Date.now()<deadline){if(await evaluate(expression))return;await new Promise(resolve=>setTimeout(resolve,120));}throw new Error(`Wait timed out: ${expression}`);};
  await waitFor('document.getElementById("connection-text")?.textContent.includes("已连接")');
  assert.equal(await evaluate('document.querySelectorAll("#point-list .point").length'),5);
  assert.equal(await evaluate('document.documentElement.scrollWidth <= window.innerWidth'),true,'Desktop horizontal overflow');
  if(!snapshotOnly){
    assert.equal(await evaluate('(async()=> (await (await fetch("/api/state")).json()).demo)()'),true,'Mutating browser tests must run in demo mode');
    const initialFans=await evaluate('document.querySelectorAll(".fan-card").length');
    assert.equal(initialFans,3,'All three simulated connected fans must be discovered');
    assert.equal(await evaluate('document.querySelectorAll("#discovered-fans .discovery-card").length'),3);
    // Identify the first real channel in the simulation before enabling its curve.
    await evaluate(`document.querySelector('[data-identify]').click()`);
    await waitFor(`document.getElementById('identify-dialog').open`);
    await evaluate(`document.getElementById('identify-kind').value='fan';document.getElementById('identify-kind').dispatchEvent(new Event('change'));document.getElementById('identify-full').click()`);
    await waitFor(`document.getElementById('identify-test-status').textContent.includes('正在以 100%')`);
    await evaluate(`document.getElementById('identify-slider').value=70;document.getElementById('identify-slider').dispatchEvent(new Event('input'));document.getElementById('identify-slider').dispatchEvent(new Event('change'))`);
    await waitFor(`document.getElementById('identify-test-status').textContent.includes('正在以 70%')`);
    assert.equal(await evaluate(`(async()=>{const s=await(await fetch('/api/state')).json();return s.identification.duty})()`),70);
    await evaluate(`document.getElementById('identify-stop').click()`);
    await waitFor(`(async()=> (await(await fetch('/api/state')).json()).identification===null)()`);
    await evaluate(`document.getElementById('identify-name').value='前置进风';document.getElementById('identify-save').click()`);
    await waitFor(`!document.getElementById('identify-dialog').open`);
    assert.equal(await evaluate(`(async()=> (await(await fetch('/api/state')).json()).config.fans[0].name)()`),'前置进风');
    assert.equal(await evaluate(`(async()=> (await(await fetch('/api/state')).json()).config.fans[0].identified)()`),true);
    await evaluate(`document.getElementById('scan-fans').click()`);
    await waitFor(`!document.getElementById('scan-fans').disabled`);
    assert.equal(await evaluate('document.querySelectorAll(".fan-card").length'),initialFans,'Rescan must not duplicate fans');
    assert.equal(await evaluate(`document.getElementById('fan-name').value`),'前置进风');
    await evaluate('document.getElementById("preset").value="quiet";document.getElementById("preset").dispatchEvent(new Event("change"))');
    assert.equal(await evaluate('document.querySelector("[data-point=\\"1\\"][data-axis=\\"1\\"]").value'),'30');
    // Drag the third curve point with actual pointer input.
    await evaluate(`document.getElementById('curve-chart').scrollIntoView({block:'center'})`);
    const box=await evaluate('(()=>{const b=document.querySelector("#curve-chart [data-node=\\"2\\"] circle").getBoundingClientRect();return{x:b.x+b.width/2,y:b.y+b.height/2}})()');
    await send('Input.dispatchMouseEvent',{type:'mousePressed',x:box.x,y:box.y,button:'left',clickCount:1});
    await send('Input.dispatchMouseEvent',{type:'mouseMoved',x:box.x+12,y:box.y-12,buttons:1});
    await send('Input.dispatchMouseEvent',{type:'mouseReleased',x:box.x+12,y:box.y-12,button:'left',clickCount:1});
    assert.notEqual(await evaluate('document.querySelector("[data-point=\\"2\\"][data-axis=\\"1\\"]").value'),'42');
    await evaluate('document.getElementById("add-point").click()');
    assert.equal(await evaluate('document.querySelectorAll("#point-list .point").length'),6);
    await evaluate('document.querySelector("[data-remove-point=\\"1\\"]").click()');
    assert.equal(await evaluate('document.querySelectorAll("#point-list .point").length'),5);
    await evaluate('document.getElementById("fan-name").value="测试 CPU 风扇";document.getElementById("fan-name").dispatchEvent(new Event("input"));document.getElementById("save-config").click()');
    await waitFor('document.getElementById("save-status").textContent.includes("已保存")');
    assert.equal(await evaluate('(async()=> (await (await fetch("/api/state")).json()).config.fans[0].name)()'),'测试 CPU 风扇');
    await evaluate('document.querySelector("[data-action=\\"activate\\"]").click()');
    await waitFor('document.querySelector("[data-action=\\"release\\"]")!==null');
    await evaluate('document.getElementById("release-all").click()');
    await waitFor('document.querySelector("[data-action=\\"release\\"]")===null');
    await evaluate('document.getElementById("add-fan").click();document.getElementById("new-fan-name").value="后置风扇";document.querySelector("#add-dialog button[value=\\"add\\"]").click()');
    await waitFor(`document.querySelectorAll('.fan-card').length===${initialFans+1}`);
    await evaluate('document.getElementById("save-config").click()');
    await waitFor('document.getElementById("save-status").textContent.includes("已保存")');
    // Reload proves the data is persistent rather than only held in the editor.
    await send('Page.reload');
    await waitFor(`document.querySelectorAll('.fan-card').length===${initialFans+1}`);
    assert.equal(await evaluate('document.querySelectorAll("[data-action=\\"release\\"]").length'),0);
    assert.equal(await evaluate(`(async()=> (await(await fetch('/api/state')).json()).config.fans.at(-1).identified)()`),false,'An offline configuration must not inherit another device identity');
    await evaluate('document.querySelector("[data-tab=\\"sensors\\"]").click()');
    assert.equal(await evaluate('document.querySelectorAll("#sensor-table tr").length'),6);
    await evaluate('document.querySelector("[data-tab=\\"diagnostics\\"]").click()');
    assert.equal(await evaluate('document.getElementById("diagnostics-content").textContent.includes("PWM 控制通道")'),true);
    await evaluate('document.querySelector("[data-tab=\\"control\\"]").click()');
    const managed=await evaluate(`(async()=> (await(await fetch('/api/state')).json()).background_service)()`);
    if(managed){
      await evaluate(`document.querySelector('[data-fan]').click();document.getElementById('autostart-control').checked=true;document.getElementById('autostart-control').dispatchEvent(new Event('change'));document.getElementById('save-config').click()`);
      await waitFor(`(async()=>{const s=await(await fetch('/api/state')).json();return s.config.fans[0].autostart&&s.outputs[s.config.fans[0].id].active})()`);
      assert.equal(await evaluate(`document.getElementById('save-config').textContent.includes('应用')`),true);
      await evaluate(`document.querySelector('[data-action="release"]').click()`);
      await waitFor(`(async()=> !(await(await fetch('/api/state')).json()).config.fans[0].autostart)()`);
      await waitFor(`!document.getElementById('save-config').disabled`);
      await evaluate(`document.getElementById('autostart-control').checked=true;document.getElementById('autostart-control').dispatchEvent(new Event('change'));document.getElementById('save-config').click()`);
      await waitFor(`(async()=>{const s=await(await fetch('/api/state')).json();return s.config.fans[0].autostart&&s.outputs[s.config.fans[0].id].active})()`);
    }
  }
  await mkdir('test-results',{recursive:true});
  await evaluate('window.scrollTo(0,0)');
  const prefix=snapshotOnly?'real':'demo';
  const desktop=await send('Page.captureScreenshot',{format:'png',captureBeyondViewport:true});
  await writeFile(`test-results/${prefix}-desktop.png`,Buffer.from(desktop.data,'base64'));
  await send('Emulation.setDeviceMetricsOverride',{width:390,height:844,deviceScaleFactor:1,mobile:true});
  await new Promise(resolve=>setTimeout(resolve,300));
  assert.equal(await evaluate('document.documentElement.scrollWidth <= window.innerWidth'),true,'Mobile horizontal overflow');
  const mobile=await send('Page.captureScreenshot',{format:'png',captureBeyondViewport:true});
  await writeFile(`test-results/${prefix}-mobile.png`,Buffer.from(mobile.data,'base64'));
  assert.deepEqual(errors,[],'Browser JavaScript exceptions');
  console.log(`PASS: ${snapshotOnly?'real telemetry and responsive layout':'automatic discovery, identification at 100%, slider at 70%, restoration, naming, rescan, curve dragging, save, activation, reload persistence and responsive layout'}`);
}finally{
  socket?.close();
  browser.kill('SIGTERM');
  await new Promise(resolve=>{browser.once('exit',resolve);setTimeout(resolve,2000);});
  await rm(profile,{recursive:true,force:true,maxRetries:10,retryDelay:100});
}
