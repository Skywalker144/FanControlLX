const $ = id => document.getElementById(id);
const icon = name => `<svg class="ui-icon" aria-hidden="true"><use href="#icon-${name}"></use></svg>`;

// Theme stays local to this browser; it never changes the hardware configuration.
function setTheme(theme){
  document.documentElement.dataset.theme=theme;
  const dark=theme==='dark',label=dark?'切换为浅色主题':'切换为深色主题';
  $('theme-toggle').setAttribute('aria-label',label);$('theme-toggle').title=label;
  $('theme-toggle').setAttribute('aria-pressed',String(dark));
}
let savedTheme;
try{savedTheme=localStorage.getItem('fancontrollx-theme');}catch{}
setTheme(['light','dark'].includes(savedTheme)?savedTheme:'light');
$('theme-toggle').addEventListener('click',()=>{
  const theme=document.documentElement.dataset.theme==='dark'?'light':'dark';
  setTheme(theme);try{localStorage.setItem('fancontrollx-theme',theme);}catch{}
});

const esc = value => String(value ?? '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const clone = value => JSON.parse(JSON.stringify(value));
const same = (a,b) => JSON.stringify(a) === JSON.stringify(b);
const fmt = (value, digits=0) => value == null ? '—' : Number(value).toFixed(digits).replace(/\.0$/, '');
const presets = {quiet:[[25,30],[45,30],[60,42],[75,65],[85,100]],balanced:[[25,30],[40,35],[55,50],[70,75],[85,100]],performance:[[25,40],[40,50],[55,65],[70,85],[80,100]],flat:[[25,50],[85,50]]};
const numericFields = {'min-duty':'min_duty','max-duty':'max_duty',hysteresis:'hysteresis',response:'response_s',smoothing:'smoothing_s',critical:'critical_temp','step-up':'step_up','step-down':'step_down'};
const sources = [['@cpu','CPU'],['@gpu','GPU'],['@cpu-gpu','CPU / GPU 取大']];
let state, connected=false, readSequence=0, appliedSequence=0, currentTab='control', toastTimer;
let curveEdit=null, deviceEdit=null, identifyingChannel=null, identifyBusy=false, scanBusy=false, dialogSaving=false;
const pendingFans=new Set(), editingRanges=new Set(), cardSignatures=new Map(), cardMessages=new Map();
let discoverySignature='', librarySignature='';
const editable = () => connected && state?.workspace_api===2;
const findFan = id => state?.config.fans.find(f=>f.id===id);
const findCurve = id => state?.config.curves?.find(c=>c.id===id);
const pointsFor = f => findCurve(f.curve_id)?.points || f.points;
function toast(message,error=false){clearTimeout(toastTimer);$('toast').textContent=message;$('toast').classList.toggle('error',error);$('toast').hidden=false;toastTimer=setTimeout(()=>{$('toast').hidden=true;},error?7000:3500);}
function errorIn(id,message){$(id).textContent=message;$(id).hidden=!message;}
async function api(path,body){const r=await fetch(path,{method:'POST',headers:{'Content-Type':'application/json','X-FanControl-Token':state?.csrf_token||''},body:JSON.stringify(body),signal:AbortSignal.timeout(10000)});const value=await r.json();if(!r.ok)throw new Error(value.error||`请求失败 ${r.status}`);return value;}
async function refresh(){
  const sequence=++readSequence;
  try{
    const r=await fetch('/api/state',{cache:'no-store',signal:AbortSignal.timeout(6000)});if(!r.ok)throw new Error(`HTTP ${r.status}`);
    const next=await r.json();if(sequence<appliedSequence)return;
    appliedSequence=sequence;state=next;connected=true;render();
  }catch(error){if(sequence<appliedSequence)return;appliedSequence=sequence;connected=false;if(state)render();else{$('connection-error').hidden=false;$('connection-text').textContent='连接失败';$('service-value').textContent='未连接';}}
}
function sourceTemperature(f){
  const values=[];
  for(const id of f.sensor_ids){
    if(id.startsWith('@')){for(const kind of id==='@cpu-gpu'?['cpu','gpu']:[id.slice(1)]){const group=state.sensors.filter(s=>s.kind===kind);if(!group.length||group.some(s=>s.value==null))return null;values.push(Math.max(...group.map(s=>s.value)));}}
    else{const s=state.sensors.find(s=>s.id===id);if(s?.value==null)return null;values.push(s.value);}
  }
  if(!values.length)return null;
  return f.aggregation==='average'?values.reduce((a,b)=>a+b,0)/values.length:f.aggregation==='min'?Math.min(...values):Math.max(...values);
}
function interpolate(points,t){if(t<=points[0][0])return points[0][1];for(let i=1;i<points.length;i++){const [a,b]=[points[i-1],points[i]];if(t<=b[0])return a[1]+(b[1]-a[1])*(t-a[0])/(b[0]-a[0]);}return points.at(-1)[1];}
function tab(name){currentTab=name;for(const key of ['control','devices','sensors','diagnostics'])$(`tab-${key}`).hidden=key!==name;document.querySelectorAll('nav [data-tab]').forEach(el=>{el.classList.toggle('active',el.dataset.tab===name);el.setAttribute('aria-current',el.dataset.tab===name?'page':'false');});$('page-title').textContent={control:'风扇控制',devices:'设备设置',sensors:'硬件传感器',diagnostics:'诊断与日志'}[name];}
function spark(id,kind){const values=state.history.slice(-80);let path='',drawing=false;values.forEach((v,i)=>{if(v[kind]==null){drawing=false;return;}path+=`${drawing?'L':'M'}${i/Math.max(1,values.length-1)*200},${42-Math.max(0,Math.min(100,v[kind]))/100*42} `;drawing=true;});$(id).innerHTML=`<path d="${path}" fill="none" stroke="currentColor" stroke-width="1.7"/>`;}
function render(){
  $('connection-error').hidden=connected;$('connection-dot').classList.toggle('offline',!connected);$('connection-text').textContent=connected?(state.demo?'已连接 · 演示设备':'本机服务已连接'):'连接已断开';$('machine-name').textContent=state.hostname;$('version').textContent=`v${state.version}`;
  for(const kind of ['cpu','gpu']){const group=state.sensors.filter(s=>s.kind===kind&&s.value!=null);$(`${kind}-temp`).textContent=connected&&group.length?fmt(Math.max(...group.map(s=>s.value)),1):'—';spark(`${kind}-spark`,kind);}
  const active=Object.values(state.outputs).filter(o=>o.active).length;
  $('service-value').textContent=!connected?'状态未知':state.error?'需要检查':`${active} 路控制中`;
  $('channel-caption').textContent=connected?`${state.config.fans.length} 个风扇配置 · ${state.channels.length} 个 PWM 通道`:'读数已过期';
  $('service-caption').textContent=state.demo?'模拟设备 · 不写入真实硬件':state.background_service?'后台独立调速 · 关闭网页不影响运行':'临时前台服务 · 退出终端将停止';
  $('mode-badge').textContent=state.demo?'演示模式':state.control_allowed?'后台已连接':'监控模式';
  $('release-all').disabled=!editable()||(!active&&!state.identification&&!Object.keys(state.pending_autostart||{}).length);
  $('new-curve').disabled=!editable();$('scan-fans').disabled=!editable()||scanBusy;$('add-fan').disabled=!editable();$('import-config').disabled=!editable();$('export-config').disabled=!connected;
  $('last-update').textContent=connected?`更新于 ${new Date(state.time*1000).toLocaleTimeString('zh-CN',{hour12:false})}`:'连接已断开';
  let banner='';
  if(state.workspace_api!==2)banner='网页已更新，后台仍是旧版本。请重启 fancontrollx 服务后刷新页面；现有配置会保留。';
  else if(state.error)banner=`控制服务需要检查：${state.error}`;
  else if(state.demo)banner='演示模式：这里的操作仅作用于模拟设备，配置与真实设备分开保存。';
  else if(!state.channels.length)banner='尚未发现 PWM 通道。可先编辑离线配置，再到设备设置中绑定硬件。';
  $('hardware-banner').classList.toggle('info',!!state.demo&&!state.error&&state.workspace_api===2);$('hardware-banner').hidden=!banner;$('hardware-banner').textContent=banner;
  renderFanCards();renderLibrary();renderDiscovery();renderSensors();renderDiagnostics();updateIdentification();
  if(curveEdit&&curveEdit.dragging===null)drawChart();
}
function sourceOptions(f){const value=f.sensor_ids.length===1&&sources.some(s=>s[0]===f.sensor_ids[0])?f.sensor_ids[0]:'@custom';return sources.map(([id,name])=>`<option value="${id}" ${id===value?'selected':''}>${name}</option>`).join('')+`<option value="@custom" ${value==='@custom'?'selected':''}>自定义组合…</option>`;}
function curveOptions(f){return `<option value="" ${!f.curve_id?'selected':''}>独立曲线</option>`+(state.config.curves||[]).map(c=>`<option value="${esc(c.id)}" ${f.curve_id===c.id?'selected':''}>${esc(c.name)}</option>`).join('');}
function renderFanCards(){
  $('fan-count').textContent=state.config.fans.length;
  const ids=new Set(state.config.fans.map(f=>f.id));
  for(const el of [...$('fan-cards').children])if(!ids.has(el.dataset.fan)){el.remove();cardSignatures.delete(el.dataset.fan);}
  for(const f of state.config.fans){
    const o=state.outputs[f.id]||{},c=state.channels.find(c=>c.id===f.channel_id);
    const requested=state.background_service?f.autostart:!!o.active;
    const testing=state.identification?.channel_id===f.channel_id;
    const disabled=!editable()||pendingFans.has(f.id);
    const signature=JSON.stringify([f,state.config.curves,connected,state.workspace_api,state.control_allowed,state.background_service,c?.writable,c?.name,pendingFans.has(f.id),requested]);
    let el=[...$('fan-cards').children].find(el=>el.dataset.fan===f.id);
    if(!el){el=document.createElement('article');el.className='fan-card';el.dataset.fan=f.id;$('fan-cards').append(el);}
    if(cardSignatures.get(f.id)!==signature&&!editingRanges.has(f.id)){
      cardSignatures.set(f.id,signature);
      el.innerHTML=`<div class="card-top"><span class="fan-symbol">${icon('fan')}</span><div class="card-name"><h3>${esc(f.name)}</h3><div class="chip-label">${esc(c?`${c.chip} · ${c.name}`:f.channel_id?'已绑定通道离线':'离线配置')}</div></div><span class="status-tag" data-live="status"></span></div>
        <div class="fan-values"><div><span class="value-label">当前输出</span><strong data-live="actual">—</strong><small>% 输出</small></div><div class="reading"><span data-live="rpm">—</span> RPM<br><span data-live="temperature">—</span> °C 输入</div></div><div class="progress"><i data-live="progress"></i></div>
        <div class="mode-buttons" role="group" aria-label="${esc(f.name)} 控制方式"><button data-mode="curve" class="${f.mode==='curve'?'selected':''}" aria-pressed="${f.mode==='curve'}" ${disabled?'disabled':''}>温度曲线</button><button data-mode="manual" class="${f.mode==='manual'?'selected':''}" aria-pressed="${f.mode==='manual'}" ${disabled?'disabled':''}>手动调速</button></div>
        <div class="card-fields"><label>温度源<select data-field="source" aria-label="${esc(f.name)} 温度源" ${disabled?'disabled':''}>${sourceOptions(f)}</select></label><label>曲线<select data-field="curve_id" aria-label="${esc(f.name)} 曲线" ${disabled||f.mode==='manual'?'disabled':''}>${curveOptions(f)}</select></label></div>
        <div class="manual-field" ${f.mode==='manual'?'':'hidden'}><div class="slider-label"><label for="manual-${esc(f.id)}">手动输出</label><output data-live="manual">${fmt(f.manual_duty)}%</output></div><input id="manual-${esc(f.id)}" data-manual type="range" min="${f.min_duty}" max="${f.max_duty}" step="1" value="${Math.max(f.min_duty,Math.min(f.max_duty,f.manual_duty))}" aria-label="${esc(f.name)} 手动输出" ${disabled?'disabled':''}><div class="hint">${fmt(f.min_duty)}–${fmt(f.max_duty)}% · 松开即保存，按升降速限制调整</div></div>
        <div class="toolbar card-actions"><button class="text-button" data-edit-curve ${disabled?'disabled':''}>${icon('curve')}编辑曲线</button><button class="text-button" data-device ${disabled?'disabled':''}>${icon('settings')}设置</button>${!f.identified&&c?`<button class="text-button" data-identify="${esc(c.id)}" ${disabled?'disabled':''}>确认设备</button>`:''}</div>
        <div class="card-footer"><label class="control-toggle"><input type="checkbox" data-toggle aria-label="${esc(f.name)} 启用控制" ${requested?'checked':''} ${disabled||!state.control_allowed||(!requested&&(!f.identified||!c?.writable))?'disabled':''}><span>启用控制</span></label><span class="hint" data-live="policy"></span></div><p class="card-feedback" data-live="feedback" role="status"></p>`;
    }
    const live=(key,value)=>{const target=el.querySelector(`[data-live="${key}"]`);if(target)target.textContent=value;};
    const status=!connected?'连接中断':testing?'测试辨认中':state.pending_autostart?.[f.id]?'等待设备就绪':o.active?(o.fault?'保护运行':'正在控制'):!f.identified?'待确认设备':c?'未启用控制':f.channel_id?'通道离线':'离线配置';
    el.classList.toggle('is-active',connected&&!!o.active);el.classList.toggle('has-fault',connected&&!!o.active&&!!o.fault);live('status',status);el.querySelector('[data-live="status"]').className=`status-tag ${connected&&o.active?'active':''} ${connected&&o.fault&&o.active?'fault':''}`;
    live('actual',connected?fmt(o.actual,1):'—');live('rpm',connected?fmt(o.rpm):'—');live('temperature',connected?fmt(o.temperature,1):'—');
    el.querySelector('[data-live="progress"]').style.width=`${connected?Math.max(0,Math.min(100,o.actual||0)):0}%`;
    live('policy',requested?(state.background_service?'已保存 · 开机恢复':'本次运行有效'):'保持原硬件控制');
    const message=cardMessages.get(f.id);const fault=connected&&o.active?o.fault:null;
    live('feedback',!connected?'读数已过期':pendingFans.has(f.id)?'正在应用…':fault||state.pending_autostart?.[f.id]||message?.text||(o.active?`目标 ${fmt(o.requested??o.target,1)}% · ${Math.abs((o.requested??o.target)-o.target)>1?'正在平滑调整':'已自动保存'}`:'设置已保存，启用控制后应用'));
    el.querySelector('[data-live="feedback"]').classList.toggle('error',!!fault||!!message?.error);
  }
  if(!ids.size)$('fan-cards').innerHTML='<div class="empty">还没有风扇配置。<br><button class="button secondary" data-tab="devices">前往设备设置</button></div>';
}
function miniCurve(points){const coords=points.map(([t,d])=>`${(t+20)/140*260},${68-d*.6}`).join(' ');return `<svg viewBox="0 0 260 75" aria-label="曲线预览"><path d="M0,68H260" stroke="var(--line)" fill="none"/><polyline points="${coords}" fill="none" stroke="var(--accent)" stroke-width="2"/></svg>`;}
function renderLibrary(){
  const signature=JSON.stringify([state.config.curves,state.config.fans.map(f=>[f.curve_id,f.name]),editable()]);if(signature===librarySignature)return;librarySignature=signature;
  $('curve-library').innerHTML=(state.config.curves||[]).map(c=>{const users=state.config.fans.filter(f=>f.curve_id===c.id);return `<article class="curve-card"><h3>${esc(c.name)}</h3>${miniCurve(c.points)}<p>${users.length?`用于 ${esc(users.map(f=>f.name).join('、'))}`:'尚未分配给风扇'} · ${c.points.length} 个节点</p><div class="toolbar"><button class="text-button" data-library-edit="${esc(c.id)}" ${!editable()?'disabled':''}>编辑</button><button class="text-button" data-library-copy="${esc(c.id)}" ${!editable()?'disabled':''}>复制</button><button class="text-button danger" data-library-delete="${esc(c.id)}" ${!editable()||users.length?'disabled':''} title="${users.length?'请先更换引用此曲线的风扇所用曲线':'删除未使用的曲线'}">删除</button></div></article>`;}).join('')||`<div class="empty curve-empty"><span class="empty-icon">${icon('curve')}</span><div><h3>从一条好曲线开始</h3><p>新建共享曲线，或将风扇的独立曲线保存到这里。</p></div><button class="button secondary" data-create-curve ${!editable()?'disabled':''}>${icon('plus')}创建第一条曲线</button></div>`;
}
function renderDiscovery(){
  const devices=state.discovery||[];const pending=devices.filter(c=>c.connection!=='rotating'&&!c.identified);
  $('discovery-summary').textContent=`${devices.filter(c=>c.connection==='rotating').length} 路有转速信号 · ${devices.filter(c=>c.identified).length} 路已确认 · ${pending.length} 路待确认`;
  const signature=JSON.stringify([devices.map(c=>[c.id,c.display_name,c.identified,c.writable,c.connection]),editable()]);
  if(discoverySignature!==signature){discoverySignature=signature;
    const card=c=>`<article class="discovery-card" data-channel="${esc(c.id)}"><div class="card-top"><span class="fan-symbol">${icon('fan')}</span><div class="card-name"><h3>${esc(c.display_name)}</h3><div class="chip-label">${esc(c.chip)} · ${esc(c.name)} · ${esc(c.id.split('/').at(-1))}</div></div><span class="badge">${c.identified?'已确认':'待确认'}</span></div><div class="discovery-readings"><strong data-rpm>—</strong><span data-duty>—</span></div><div class="toolbar"><button class="button secondary" data-identify="${esc(c.id)}" ${!editable()?'disabled':''}>${c.identified?'辨认 / 改名':'确认设备'}</button>${c.fan_id?`<button class="text-button" data-device-id="${esc(c.fan_id)}" ${!editable()?'disabled':''}>绑定与设置</button>`:''}</div></article>`;
    $('discovered-fans').innerHTML=devices.filter(c=>!pending.includes(c)).map(card).join('')||'<div class="empty">没有正在旋转或已确认的设备，请检查驱动或下方通道。</div>';
    $('unconfirmed-fans').innerHTML=pending.map(card).join('');$('unconfirmed-channels').hidden=!pending.length;$('unconfirmed-summary').textContent=`其他待确认通道 · ${pending.length}`;
  }
  for(const el of document.querySelectorAll('[data-channel]')){const c=devices.find(c=>c.id===el.dataset.channel);el.querySelector('[data-rpm]').textContent=`${connected?fmt(c?.rpm):'—'} RPM`;el.querySelector('[data-duty]').textContent=`${connected?fmt(c?.duty,1):'—'}% 硬件输出`;}
}
function renderSensors(){$('sensor-table').innerHTML=[...state.sensors.map(s=>`<tr><td>${esc(s.name)}</td><td>${esc(s.chip)}</td><td>${s.kind==='cpu'?'CPU':s.kind==='gpu'?'GPU':'温度'}</td><td>${connected?fmt(s.value,1):'—'} °C</td></tr>`),...state.fans.map(f=>`<tr><td>${esc(f.name)}</td><td>${esc(f.chip)}</td><td>转速</td><td>${connected?fmt(f.rpm):'—'} RPM</td></tr>`)].join('');}
function renderDiagnostics(){const row=(name,value)=>`<div class="diagnostic-row"><span>${esc(name)}</span><span>${esc(value)}</span></div>`;$('diagnostics-content').innerHTML=`<article class="panel diagnostic-card"><h3>本机服务</h3>${row('运行方式',state.background_service?'systemd 后台服务':'临时前台')}${row('硬件写入',state.control_allowed?'已允许':'仅监控')}${row('GPU 温度',state.gpu_error||'正常')}${row('配置格式',`v${state.config.version}`)}${row('配置文件',state.demo?'data/config.demo.json':'data/config.json')}<p class="hint">首次升级保存前自动备份旧配置。更新代码后需重启服务；关闭网页不会停止后台调速。</p></article><article class="panel diagnostic-card"><h3>PWM 通道</h3>${state.channels.map(c=>row(`${c.chip} · ${c.name}`,c.reason||`${fmt(c.duty,1)}% · ${c.watchdog?'硬件看门狗':'无硬件看门狗'}`)).join('')||'<p class="hint">未检测到可用通道</p>'}</article><article class="panel diagnostic-card"><h3>维护</h3><p class="hint">查看状态：<code>systemctl status fancontrollx</code><br>更新后重启：<code>sudo systemctl restart fancontrollx</code><br>查看日志：<code>sudo journalctl -u fancontrollx -n 50 --no-pager</code></p></article>`;$('event-list').innerHTML=state.events.map(e=>`<div class="event-row"><time>${new Date(e.time*1000).toLocaleTimeString('zh-CN',{hour12:false})}</time><span>${esc(e.message)}</span></div>`).join('')||'<p class="hint">暂无运行事件</p>';}
async function changeFan(id,changes){
  if(!editable()||pendingFans.has(id))return;const f=findFan(id);const expected=Object.fromEntries(Object.keys(changes).map(k=>[k,clone(f[k])]));pendingFans.add(id);cardMessages.delete(id);renderFanCards();
  try{await api('/api/fan',{id,changes,expected});cardMessages.set(id,{text:'已自动保存并应用'});await refresh();}
  catch(error){cardMessages.set(id,{text:error.message,error:true});toast(error.message,true);await refresh();}
  finally{pendingFans.delete(id);renderFanCards();}
}
function showDialog(id){$(id).showModal();$(id).scrollTop=0;}
function closeDialog(id,force=false){if(dialogSaving)return;if(!force&&((id==='curve-dialog'&&curveEdit?.dirty)||(id==='device-dialog'&&deviceEdit?.dirty))&&!confirm('放弃尚未应用的修改？'))return;$(id).close();if(id==='curve-dialog')curveEdit=null;if(id==='device-dialog')deviceEdit=null;}
function openCurve({fanId=null,curveId=null,copy=false}={}){
  if(!editable())return;const f=fanId?findFan(fanId):null;const curve=findCurve(curveId||f?.curve_id);
  curveEdit={fanId:f?.id||null,baseFan:f?{points:clone(f.points),curve_id:f.curve_id}:null,base:curve&&!copy?clone(curve):null,name:curve?curve.name+(copy?' 副本':''):f?`${f.name} 曲线`:'新曲线',points:clone(curve?.points||f?.points||presets.balanced),dirty:false,selected:0,dragging:null};
  $('curve-title').textContent=copy?'复制曲线':curve?'编辑共享曲线':f?'编辑风扇曲线':'新建曲线';$('curve-name').value=curveEdit.name;$('curve-share').checked=false;$('share-choice').hidden=!!curve||!f;$('curve-name').disabled=!curve&&!!f;
  const users=curve&&!copy?state.config.fans.filter(f=>f.curve_id===curve.id):[];
  $('curve-context').textContent=users.length?`应用后更新 ${users.length} 个风扇：${users.map(f=>f.name).join('、')}。各自的温度源保持不变。`:f?'此处只编辑曲线形状，温度源在风扇卡片中选择。':'保存后可在任意风扇卡片中选择这条曲线。';
  $('preset').value='custom';errorIn('curve-error','');renderPoints();drawChart();showDialog('curve-dialog');
}
const chart={left:58,right:770,top:22,bottom:291};
function domain(){return [Math.min(20,...curveEdit.points.map(p=>p[0])),Math.max(100,...curveEdit.points.map(p=>p[0]))];}
function xy(t,d){const [lo,hi]=domain();return [chart.left+(t-lo)/(hi-lo)*(chart.right-chart.left),chart.bottom-d/100*(chart.bottom-chart.top)];}
function eventPoint(event){const p=$('curve-chart').createSVGPoint();p.x=event.clientX;p.y=event.clientY;const v=p.matrixTransform($('curve-chart').getScreenCTM().inverse());const [lo,hi]=domain();return [Math.round(Math.max(-20,Math.min(120,lo+(v.x-chart.left)/(chart.right-chart.left)*(hi-lo)))),Math.round(Math.max(0,Math.min(100,(chart.bottom-v.y)/(chart.bottom-chart.top)*100)))];}
function drawChart(){
  if(!curveEdit)return;const [lo,hi]=domain();let html='';
  for(let d=0;d<=100;d+=20){const y=xy(lo,d)[1];html+=`<line x1="58" y1="${y}" x2="770" y2="${y}" stroke="var(--line)" stroke-dasharray="3 5"/><text x="43" y="${y+3}" text-anchor="end" fill="var(--muted)" font-size="11">${d}</text>`;}
  for(let t=Math.ceil(lo/10)*10;t<=hi;t+=10){const x=xy(t,0)[0];html+=`<line x1="${x}" y1="22" x2="${x}" y2="291" stroke="var(--grid)"/><text x="${x}" y="315" text-anchor="middle" fill="var(--muted)" font-size="11">${t}°</text>`;}
  const coords=[xy(lo,curveEdit.points[0][1]),...curveEdit.points.map(p=>xy(...p)),xy(hi,curveEdit.points.at(-1)[1])];const line=coords.map((p,i)=>`${i?'L':'M'}${p[0]},${p[1]}`).join(' ');html+=`<path d="${line} L770,291 L58,291 Z" fill="var(--accent)" opacity=".07"/><path d="${line}" fill="none" stroke="var(--accent)" stroke-width="2.5"/>`;
  const f=findFan(curveEdit.fanId);const temp=f?sourceTemperature(f):null;
  if(connected&&temp!=null){const x=xy(Math.max(lo,Math.min(hi,temp)),0)[0],y=xy(temp,interpolate(curveEdit.points,temp))[1];html+=`<line x1="${x}" y1="22" x2="${x}" y2="291" stroke="var(--blue)" stroke-dasharray="4 5"/><circle cx="${x}" cy="${y}" r="5" fill="var(--blue)"/>`;}
  curveEdit.points.forEach((p,i)=>{const [x,y]=xy(...p);html+=`<g data-node="${i}" tabindex="0" role="slider" aria-label="节点 ${i+1}，${p[0]} 摄氏度，${p[1]} 百分比" aria-valuemin="0" aria-valuemax="100" aria-valuenow="${p[1]}" style="cursor:grab"><circle cx="${x}" cy="${y}" r="15" fill="transparent"/><circle cx="${x}" cy="${y}" r="${i===curveEdit.selected?6:4.5}" fill="var(--panel)" stroke="var(--accent)" stroke-width="2"/></g>`;});
  // Keep focused graph nodes stable between polling updates and keyboard steps.
  const focus=$('curve-chart').contains(document.activeElement)?document.activeElement.dataset.node:null;
  $('curve-chart').innerHTML=html;if(focus!=null)$('curve-chart').querySelector(`[data-node="${focus}"]`)?.focus({preventScroll:true});
  $('chart-current').textContent=f?(connected&&temp!=null?`${fmt(temp,1)}°C · 曲线预览 ${fmt(interpolate(curveEdit.points,temp),1)}%`:'温度源不可用'):'曲线预览';
}
function renderPoints(){$('point-count').textContent=`${curveEdit.points.length} 个节点`;$('point-list').innerHTML=curveEdit.points.map((p,i)=>`<div class="point ${i===curveEdit.selected?'selected':''}"><div class="point-top"><span>节点 ${i+1}</span><button type="button" data-remove-point="${i}" aria-label="删除节点 ${i+1}" ${curveEdit.points.length<=2?'disabled':''}>×</button></div><div class="point-values"><label>温度 °C<input type="number" value="${p[0]}" min="-20" max="120" step="1" data-point="${i}" data-axis="0" aria-label="节点 ${i+1} 温度"></label><span>→</span><label>输出 %<input type="number" value="${p[1]}" min="0" max="100" step="1" data-point="${i}" data-axis="1" aria-label="节点 ${i+1} 输出"></label></div></div>`).join('');}
function setPoint(i,t,d){const p=curveEdit.points;const low=i?p[i-1][0]+1:-20,high=i<p.length-1?p[i+1][0]-1:120;p[i]=[Math.round(Math.max(low,Math.min(high,t))),Math.round(Math.max(0,Math.min(100,d)))];curveEdit.selected=i;curveEdit.dirty=true;$('preset').value='custom';drawChart();}
function addPoint(p){if(curveEdit.points.length>=32)return toast('最多 32 个节点',true);if(curveEdit.points.some(v=>v[0]===p[0]))return toast('该温度已有节点，请调整现有节点');curveEdit.points.push(p);curveEdit.points.sort((a,b)=>a[0]-b[0]);curveEdit.selected=curveEdit.points.indexOf(p);curveEdit.dirty=true;renderPoints();drawChart();}
function removePoint(i){if(curveEdit.points.length<=2)return;curveEdit.points.splice(i,1);curveEdit.selected=Math.min(i,curveEdit.points.length-1);curveEdit.dirty=true;renderPoints();drawChart();}
function options(items,current,empty){return `<option value="">${esc(empty)}</option>`+(current&&!items.some(i=>i.id===current)?`<option value="${esc(current)}">离线：${esc(current)}</option>`:'')+items.map(i=>`<option value="${esc(i.id)}">${esc(`${i.chip} · ${i.name}`)}</option>`).join('');}
function openDevice(id,mix=false){
  if(!editable())return;const f=findFan(id);if(!f)return;deviceEdit={id,base:clone(f),dirty:false,mixChanged:false};
  $('fan-name').value=f.name;$('channel-select').innerHTML=options(state.channels,f.channel_id,'暂不绑定');$('channel-select').value=f.channel_id||'';$('rpm-select').innerHTML=options(state.fans,f.rpm_sensor_id,'不绑定测速');$('rpm-select').value=f.rpm_sensor_id||'';
  for(const [id,key] of Object.entries(numericFields))$(id).value=f[key];
  const sensors=[{id:'@cpu',name:'CPU'},{id:'@gpu',name:'GPU'},{id:'@cpu-gpu',name:'CPU / GPU 取大'},...state.sensors];for(const id of f.sensor_ids)if(!sensors.some(s=>s.id===id))sensors.push({id,name:`离线：${id}`});
  $('sensor-checkboxes').innerHTML=sensors.map(s=>`<label class="checkbox"><input type="checkbox" value="${esc(s.id)}" ${f.sensor_ids.includes(s.id)?'checked':''}>${esc(s.chip?`${s.chip} · ${s.name}`:s.name)}</label>`).join('');$('aggregation').value=f.aggregation;$('mix-details').open=mix;errorIn('device-error','');showDialog('device-dialog');
}
function openIdentification(id){if(!editable())return;const c=state.discovery.find(c=>c.id===id);if(!c)return;identifyingChannel=id;$('identify-title').textContent=c.identified?'辨认与命名':'确认设备';$('identify-channel').textContent=`${c.chip} · ${c.name} · ${c.id.split('/').at(-1)}`;$('identify-name').value=c.display_name;$('identify-kind').value=c.device_kind;$('identify-slider').value=100;errorIn('identify-error','');identificationKindChanged();updateIdentification();showDialog('identify-dialog');}
function identificationKindChanged(){const c=state.discovery.find(c=>c.id===identifyingChannel);const kind=$('identify-kind').value;const pump=kind==='pump'||(c?.name.toLowerCase().includes('pump')&&kind!=='fan');$('identify-slider').min=pump?100:50;if(pump)$('identify-slider').value=100;$('identify-output').textContent=`${$('identify-slider').value}%`;$('identify-pump-help').hidden=!pump;}
function updateIdentification(){if(!identifyingChannel)return;const c=state.discovery.find(c=>c.id===identifyingChannel),test=state.identification?.channel_id===identifyingChannel?state.identification:null;$('identify-rpm').textContent=connected?fmt(c?.rpm):'—';$('identify-hardware-duty').textContent=connected?fmt(c?.duty,1):'—';const canTest=editable()&&state.control_allowed&&c?.writable;for(const id of ['identify-full','identify-apply','identify-slider'])$(id).disabled=!canTest||identifyBusy;$('identify-stop').disabled=!editable()||!test||identifyBusy;$('identify-save').disabled=!editable()||identifyBusy;$('identify-test-status').textContent=!connected?'连接中断，等待测试超时恢复':test?.stopping?'恢复状态失败，后台正在重试':test?`${test.fault||`正在以 ${fmt(test.effective_duty)}% 测试`} · ${test.remaining_s} 秒后结束`:!canTest?'当前不能写入硬件；仍可确认名称和设备类型':'测试未运行，保持此前控制模式';}
async function identify(duty){if(identifyBusy)return;identifyBusy=true;updateIdentification();try{await api('/api/identify',{channel_id:identifyingChannel,duty,device_kind:$('identify-kind').value});await refresh();errorIn('identify-error','');}catch(error){errorIn('identify-error',error.message);}finally{identifyBusy=false;updateIdentification();}}
async function stopIdentification(){await api('/api/identify-stop',{channel_id:identifyingChannel});await refresh();}
async function closeIdentification(){if(identifyBusy)return;identifyBusy=true;try{if(state?.identification?.channel_id===identifyingChannel)await stopIdentification();$('identify-dialog').close();identifyingChannel=null;}catch(error){errorIn('identify-error',error.message);}finally{identifyBusy=false;updateIdentification();}}
function download(name,value){const url=URL.createObjectURL(new Blob([JSON.stringify(value,null,2)+'\n'],{type:'application/json'}));const a=document.createElement('a');a.href=url;a.download=name;a.click();setTimeout(()=>URL.revokeObjectURL(url),500);}

// The card controls submit only the fields they changed, never a page-wide draft.
document.addEventListener('click',event=>{
  const nav=event.target.closest('[data-tab]');if(nav){tab(nav.dataset.tab);return;}
  const close=event.target.closest('[data-close]');if(close){closeDialog(close.dataset.close);return;}
  const identify=event.target.closest('[data-identify]');if(identify){openIdentification(identify.dataset.identify);return;}
  const device=event.target.closest('[data-device-id]');if(device){openDevice(device.dataset.deviceId);return;}
});
$('fan-cards').addEventListener('click',event=>{const card=event.target.closest('[data-fan]');if(!card)return;const id=card.dataset.fan;const mode=event.target.closest('[data-mode]');if(mode){if(findFan(id).mode!==mode.dataset.mode)changeFan(id,{mode:mode.dataset.mode});return;}if(event.target.closest('[data-edit-curve]'))openCurve({fanId:id});if(event.target.closest('[data-device]'))openDevice(id);});
$('fan-cards').addEventListener('input',event=>{if(event.target.matches('[data-manual]')){const card=event.target.closest('[data-fan]');editingRanges.add(card.dataset.fan);card.querySelector('[data-live="manual"]').textContent=`${event.target.value}%`;}});
$('fan-cards').addEventListener('change',async event=>{
  const el=event.target,card=el.closest('[data-fan]');if(!card)return;const id=card.dataset.fan;
  if(el.matches('[data-manual]')){editingRanges.delete(id);await changeFan(id,{manual_duty:Number(el.value)});}
  if(el.matches('[data-toggle]')){if(pendingFans.has(id))return;pendingFans.add(id);renderFanCards();try{await api(el.checked?'/api/activate':'/api/release',{id});cardMessages.set(id,{text:'控制开关已保存'});await refresh();}catch(error){cardMessages.set(id,{text:error.message,error:true});toast(error.message,true);await refresh();}finally{pendingFans.delete(id);renderFanCards();}}
  if(el.dataset.field==='source'){if(el.value==='@custom'){el.value=findFan(id).sensor_ids.length===1?findFan(id).sensor_ids[0]:'@custom';openDevice(id,true);}else await changeFan(id,{sensor_ids:[el.value],aggregation:'max'});}
  if(el.dataset.field==='curve_id')await changeFan(id,{curve_id:el.value||null});
});
$('new-curve').addEventListener('click',()=>openCurve());
$('curve-library').addEventListener('click',async event=>{if(event.target.closest('[data-create-curve]')){openCurve();return;}const edit=event.target.closest('[data-library-edit]'),copy=event.target.closest('[data-library-copy]'),remove=event.target.closest('[data-library-delete]');if(edit)openCurve({curveId:edit.dataset.libraryEdit});if(copy)openCurve({curveId:copy.dataset.libraryCopy,copy:true});if(remove){const curve=clone(findCurve(remove.dataset.libraryDelete));if(!confirm(`删除曲线“${curve.name}”？`))return;remove.disabled=true;try{await api('/api/curve/delete',{id:curve.id,expected:curve});await refresh();toast('曲线已删除');}catch(error){toast(error.message,true);remove.disabled=false;}}});
$('curve-name').addEventListener('input',()=>{curveEdit.dirty=true;});$('curve-share').addEventListener('change',()=>{curveEdit.dirty=true;$('curve-name').disabled=!$('curve-share').checked;});
$('preset').addEventListener('change',()=>{const p=presets[$('preset').value];if(p){curveEdit.points=clone(p);curveEdit.selected=0;curveEdit.dirty=true;renderPoints();drawChart();}});
$('point-list').addEventListener('change',event=>{const el=event.target;if(!el.matches('[data-point]'))return;if(el.value===''||!Number.isFinite(Number(el.value))){renderPoints();return;}const i=Number(el.dataset.point),p=[...curveEdit.points[i]];p[Number(el.dataset.axis)]=Number(el.value);setPoint(i,...p);renderPoints();});
$('point-list').addEventListener('click',event=>{const b=event.target.closest('[data-remove-point]');if(b)removePoint(Number(b.dataset.removePoint));});
$('add-point').addEventListener('click',()=>{const p=curveEdit.points;let gap=0,best;for(let i=1;i<p.length;i++)if(p[i][0]-p[i-1][0]>gap){gap=p[i][0]-p[i-1][0];best=[Math.round((p[i][0]+p[i-1][0])/2),Math.round((p[i][1]+p[i-1][1])/2)];}if(gap>=2)addPoint(best);else toast('节点间没有空间，请先移动节点');});
let moved=false, nodeClick=false;
$('curve-chart').addEventListener('pointerdown',event=>{const node=event.target.closest('[data-node]');moved=false;nodeClick=!!node;if(!node||event.button!==0)return;event.preventDefault();curveEdit.dragging=Number(node.dataset.node);curveEdit.selected=curveEdit.dragging;$('curve-chart').setPointerCapture(event.pointerId);drawChart();});
$('curve-chart').addEventListener('pointermove',event=>{if(curveEdit?.dragging==null)return;moved=true;setPoint(curveEdit.dragging,...eventPoint(event));});
const endDrag=()=>{if(curveEdit?.dragging!=null){curveEdit.dragging=null;renderPoints();}};$('curve-chart').addEventListener('pointerup',endDrag);$('curve-chart').addEventListener('pointercancel',endDrag);
$('curve-chart').addEventListener('click',event=>{if(event.detail!==1||moved||nodeClick||event.target.closest('[data-node]'))return;const p=eventPoint(event);if(event.offsetX>=0)addPoint(p);});
$('curve-chart').addEventListener('contextmenu',event=>{const node=event.target.closest('[data-node]');if(node){event.preventDefault();removePoint(Number(node.dataset.node));}});
$('curve-chart').addEventListener('keydown',event=>{const node=event.target.closest('[data-node]');if(!node)return;const i=Number(node.dataset.node),p=[...curveEdit.points[i]],step=event.shiftKey?5:1;if(['Delete','Backspace'].includes(event.key)){event.preventDefault();removePoint(i);return;}if(!['ArrowUp','ArrowDown','ArrowLeft','ArrowRight'].includes(event.key))return;event.preventDefault();p[event.key==='ArrowLeft'||event.key==='ArrowRight'?0:1]+=(event.key==='ArrowLeft'||event.key==='ArrowDown'?-step:step);setPoint(i,...p);renderPoints();});
$('curve-form').addEventListener('submit',async event=>{event.preventDefault();if(dialogSaving||!editable())return;dialogSaving=true;$('save-curve').disabled=true;errorIn('curve-error','');try{const e=curveEdit;if(e.fanId&&!e.base&&!$('curve-share').checked)await api('/api/fan',{id:e.fanId,changes:{points:e.points,curve_id:null},expected:e.baseFan});else await api('/api/curve',{curve:{id:e.base?.id,name:$('curve-name').value.trim(),points:e.points},expected:e.base,assign_to:!e.base?e.fanId:null,expected_fan:e.baseFan});await refresh();dialogSaving=false;closeDialog('curve-dialog',true);toast('曲线已保存并应用');}catch(error){errorIn('curve-error',error.message);}finally{dialogSaving=false;$('save-curve').disabled=false;}});
for(const id of ['curve-dialog','device-dialog','add-dialog'])$(id).addEventListener('cancel',event=>{event.preventDefault();closeDialog(id);});
$('device-form').addEventListener('input',()=>{if(deviceEdit)deviceEdit.dirty=true;});$('device-form').addEventListener('change',()=>{if(deviceEdit)deviceEdit.dirty=true;});
for(const id of ['sensor-checkboxes','aggregation'])$(id).addEventListener('change',()=>{deviceEdit.mixChanged=true;});
$('device-form').addEventListener('submit',async event=>{
  event.preventDefault();if(dialogSaving||!editable())return;const e=deviceEdit,values={name:$('fan-name').value.trim(),channel_id:$('channel-select').value||null,rpm_sensor_id:$('rpm-select').value||null};
  for(const [id,key] of Object.entries(numericFields))values[key]=Number($(id).value);
  if(values.min_duty>values.max_duty)return errorIn('device-error','最低输出不能高于最高输出');
  if(e.mixChanged){values.sensor_ids=[...$('sensor-checkboxes').querySelectorAll('input:checked')].map(el=>el.value);values.aggregation=$('aggregation').value;if(!values.sensor_ids.length)return errorIn('device-error','请至少选择一个温度源');}
  const changes=Object.fromEntries(Object.entries(values).filter(([k,v])=>!same(v,e.base[k])));if(!Object.keys(changes).length){closeDialog('device-dialog',true);return;}
  const expected=Object.fromEntries(Object.keys(changes).map(k=>[k,e.base[k]]));dialogSaving=true;$('save-device').disabled=true;errorIn('device-error','');
  try{await api('/api/fan',{id:e.id,changes,expected});await refresh();dialogSaving=false;closeDialog('device-dialog',true);toast('风扇设置已保存');}catch(error){errorIn('device-error',error.message);}finally{dialogSaving=false;$('save-device').disabled=false;}
});
$('delete-fan').addEventListener('click',async()=>{if(dialogSaving||!editable()||!confirm('删除此风扇配置并停止控制？硬件通道仍可重新扫描发现。'))return;dialogSaving=true;try{const config=clone(state.config);config.fans=config.fans.filter(f=>f.id!==deviceEdit.id);await api('/api/config',{...config,expected_revision:state.config_revision});await refresh();dialogSaving=false;closeDialog('device-dialog',true);toast('风扇配置已删除');}catch(error){errorIn('device-error',error.message);}finally{dialogSaving=false;}});
$('scan-fans').addEventListener('click',async()=>{if(scanBusy||!editable())return;scanBusy=true;render();try{const r=await api('/api/scan',{});await refresh();toast(`扫描完成，新增 ${r.added} 路设备`);}catch(error){toast(error.message,true);}finally{scanBusy=false;render();}});
$('add-fan').addEventListener('click',()=>{$('new-fan-name').value='';errorIn('add-error','');showDialog('add-dialog');});
$('add-form').addEventListener('submit',async event=>{event.preventDefault();if(dialogSaving||!editable())return;dialogSaving=true;try{const config=clone(state.config);config.fans.push({id:crypto.randomUUID(),name:$('new-fan-name').value.trim()});await api('/api/config',{...config,expected_revision:state.config_revision});await refresh();dialogSaving=false;closeDialog('add-dialog',true);toast('离线配置已添加');}catch(error){errorIn('add-error',error.message);}finally{dialogSaving=false;}});
$('identify-kind').addEventListener('change',identificationKindChanged);$('identify-slider').addEventListener('input',()=>{$('identify-output').textContent=`${$('identify-slider').value}%`;});$('identify-slider').addEventListener('change',()=>{if(state.identification?.channel_id===identifyingChannel)identify(Number($('identify-slider').value));});$('identify-full').addEventListener('click',()=>{$('identify-slider').value=100;$('identify-output').textContent='100%';identify(100);});$('identify-apply').addEventListener('click',()=>identify(Number($('identify-slider').value)));$('identify-stop').addEventListener('click',async()=>{if(identifyBusy)return;identifyBusy=true;updateIdentification();try{await stopIdentification();}catch(error){errorIn('identify-error',error.message);}finally{identifyBusy=false;updateIdentification();}});$('identify-close').addEventListener('click',closeIdentification);$('identify-dialog').addEventListener('cancel',event=>{event.preventDefault();closeIdentification();});
$('identify-save').addEventListener('click',async()=>{if(identifyBusy||!editable())return;identifyBusy=true;updateIdentification();try{await api('/api/identity',{channel_id:identifyingChannel,name:$('identify-name').value,device_kind:$('identify-kind').value});await refresh();$('identify-dialog').close();identifyingChannel=null;toast('设备已确认；在风扇卡片中选择温度源和曲线后启用控制');}catch(error){errorIn('identify-error',error.message);}finally{identifyBusy=false;updateIdentification();}});
function abandonIdentification(){if(!identifyingChannel||state?.identification?.channel_id!==identifyingChannel)return;fetch('/api/identify-stop',{method:'POST',headers:{'Content-Type':'application/json','X-FanControl-Token':state.csrf_token},body:JSON.stringify({channel_id:identifyingChannel}),keepalive:true}).catch(()=>{});}
window.addEventListener('pagehide',abandonIdentification);document.addEventListener('visibilitychange',()=>{if(document.visibilityState==='hidden')abandonIdentification();});
$('release-all').addEventListener('click',async()=>{if(!editable())return;$('release-all').disabled=true;try{await api('/api/release-all',{});await refresh();toast('已停止全部控制并恢复原硬件状态');}catch(error){toast(error.message,true);await refresh();}});
$('retry').addEventListener('click',refresh);$('export-config').addEventListener('click',()=>download('fancontrollx-config.json',state.config));$('download-diagnostics').addEventListener('click',()=>{const {csrf_token,...safe}=state;download('fancontrollx-diagnostics.json',safe);});$('import-config').addEventListener('click',()=>$('import-file').click());
$('import-file').addEventListener('change',async()=>{const file=$('import-file').files[0];if(!file)return;try{if(file.size>256*1024)throw new Error('配置文件超过 256 KB');const config=JSON.parse(await file.text());if(!confirm('导入会替换此机器的全部配置，并按其中的控制开关应用。继续？'))return;await api('/api/config',{...config,expected_revision:state.config_revision});await refresh();toast('配置已导入；旧版格式已兼容读取');}catch(error){toast(error.message,true);}finally{$('import-file').value='';}});
window.addEventListener('beforeunload',event=>{if(curveEdit?.dirty||deviceEdit?.dirty){event.preventDefault();event.returnValue='';}});
document.addEventListener('keydown',event=>{if((event.ctrlKey||event.metaKey)&&event.key==='s'){event.preventDefault();if($('curve-dialog').open)$('curve-form').requestSubmit();else if($('device-dialog').open)$('device-form').requestSubmit();}});
async function poll(){await refresh();setTimeout(poll,1500);}poll();
