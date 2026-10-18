const $ = (id) => document.getElementById(id);
const esc = (value) => String(value ?? '').replace(/[&<>"']/g, c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const clone = (v) => JSON.parse(JSON.stringify(v));
const fmt = (v, digits = 0) => v == null ? '—' : Number(v).toFixed(digits).replace(/\.0$/, '');
const presets = {
  quiet: [[25,30],[45,30],[60,42],[75,65],[85,100]],
  balanced: [[25,30],[40,35],[55,50],[70,75],[85,100]],
  performance: [[25,40],[40,50],[55,65],[70,85],[80,100]],
  flat: [[25,50],[85,50]],
};
let state, draft, selected, dirty = false, connected = false, selectedPoint = 0;
let dragging = null, busy = false, inventoryKey = '', toastTimeout;
let identifyingChannel = null, identifyBusy = false, scanBusy = false;
const fan = () => draft?.fans.find(f => f.id === selected);

function toast(message, error = false) {
  clearTimeout(toastTimeout);
  $('toast').textContent = message;
  $('toast').classList.toggle('error', error);
  $('toast').hidden = false;
  toastTimeout = setTimeout(() => { $('toast').hidden = true; }, error ? 7000 : 3500);
}

async function api(path, body) {
  const result = await fetch(path, {method: 'POST', headers: {'Content-Type': 'application/json', 'X-FanControl-Token': state?.csrf_token || ''}, body: JSON.stringify(body), signal: AbortSignal.timeout(10000)});
  const value = await result.json();
  if (!result.ok) throw new Error(value.error || `请求失败 ${result.status}`);
  return value;
}

async function refresh() {
  try {
    const response = await fetch('/api/state', {signal: AbortSignal.timeout(6000), cache:'no-store'});
    if (!response.ok) throw new Error(`HTTP ${response.status}`);
    state = await response.json();
    connected = true;
    if (!draft || (!dirty && JSON.stringify(draft)!==JSON.stringify(state.config))) {
      draft = clone(state.config);
      if(!draft.fans.some(f=>f.id===selected))selected = draft.fans[0]?.id;
      renderEditor();
    }
    updateLive();
    const key = JSON.stringify([state.channels.map(c => [c.id,c.writable]), state.sensors.map(s => s.id), state.fans.map(f => f.id)]);
    if (key !== inventoryKey) {
      inventoryKey = key;
      renderEditor();
    }
  } catch (error) {
    connected = false;
    $('connection-error').hidden = false;
    $('connection-dot').classList.add('offline');
    $('connection-text').textContent = '连接已断开';
    $('service-value').textContent = '已断开';
    $('service-caption').textContent = '读数已过期 · 请检查服务';
    $('service-value').style.color = 'var(--danger)';
    $('cpu-temp').textContent = '—';
    $('gpu-temp').textContent = '—';
    $('cpu-caption').textContent = '读数已过期';
    $('gpu-caption').textContent = '读数已过期';
    $('channel-count').textContent = '—';
    if (draft) { renderFanCards(); renderDiscovery(); updateIdentification(); }
  }
}

function markDirty() {
  dirty = true;
  $('save-status').textContent = '● 有未保存的修改';
  $('save-status').style.color = 'var(--accent)';
  $('save-config').textContent = state.background_service?'保存并应用 ↗':'保存配置 ↗';
  renderSidebar();
  renderFanCards();
  drawChart();
}

function sourceTemperature(f) {
  const values = [];
  for (const id of f.sensor_ids) {
    if (id.startsWith('@')) {
      const kinds = id === '@cpu-gpu' ? ['cpu','gpu'] : [id.slice(1)];
      for (const kind of kinds) {
        const group = state.sensors.filter(s => s.kind === kind);
        if (!group.length || group.some(s => s.value == null)) return null;
        values.push(Math.max(...group.map(s => s.value)));
      }
    } else {
      const s = state.sensors.find(s => s.id === id);
      if (s?.value == null) return null;
      values.push(s.value);
    }
  }
  if (!values.length) return null;
  return f.aggregation === 'average' ? values.reduce((a,b) => a+b,0)/values.length : f.aggregation === 'min' ? Math.min(...values) : Math.max(...values);
}

function interpolate(points, t) {
  if (t <= points[0][0]) return points[0][1];
  for (let i=1;i<points.length;i++) {
    const [a,b] = [points[i-1],points[i]];
    if (t <= b[0]) return a[1]+(b[1]-a[1])*(t-a[0])/(b[0]-a[0]);
  }
  return points.at(-1)[1];
}

function updateLive() {
  $('connection-error').hidden = true;
  $('connection-dot').classList.remove('offline');
  $('connection-text').textContent = state.demo ? '已连接 · 演示设备' : '本机服务已连接';
  $('machine-name').textContent = state.hostname;
  $('version').textContent = state.version;
  const cpu = state.sensors.filter(s=>s.kind==='cpu' && s.value!=null);
  const gpu = state.sensors.filter(s=>s.kind==='gpu' && s.value!=null);
  $('cpu-temp').textContent = cpu.length ? fmt(Math.max(...cpu.map(s=>s.value)),1) : '—';
  $('gpu-temp').textContent = gpu.length ? fmt(Math.max(...gpu.map(s=>s.value)),1) : '—';
  $('cpu-caption').textContent = cpu.length ? `${cpu[0].chip} · ${cpu.length} 个温度通道取最高值` : 'CPU 温度不可用';
  $('gpu-caption').textContent = gpu.length ? gpu.map(g=>g.name).join(' / ') : 'GPU 温度不可用 · 查看诊断';
  $('channel-count').textContent = state.channels.length;
  $('channel-caption').textContent = state.channels.length ? `${state.channels.filter(c=>c.writable).length} 个通道具有控制权限` : '尚未发现主板 PWM 接口';
  const active = Object.values(state.outputs).filter(o=>o.active).length + (state.identification ? 1 : 0);
  $('service-value').textContent = state.error ? '需要检查' : active ? `${active} 路控制中` : Object.keys(state.pending_autostart||{}).length?'等待设备就绪':'运行正常';
  $('service-value').style.color = state.error ? 'var(--danger)' : 'var(--accent)';
  $('service-caption').textContent = state.demo ? '模拟硬件 · 不写入真实设备' : state.background_service?'后台独立调速 · 浏览器可关闭':'临时前台运行 · 退出终端将停止';
  $('mode-badge').textContent = state.demo ? '演示模式 · 模拟数据' : state.background_service?'后台常驻控制':state.control_allowed ? '硬件控制模式' : '监控与曲线编辑';
  $('release-all').disabled = !active && !Object.keys(state.pending_autostart||{}).length;
  $('last-update').textContent = `最后更新 ${new Date(state.time*1000).toLocaleTimeString('zh-CN',{hour12:false})} · 1s`;
  const banner = $('hardware-banner');
  banner.hidden = !state.demo && !!state.channels.length && !state.error && Object.hasOwn(state,'background_service');
  if (!Object.hasOwn(state,'background_service')) {
    $('banner-title').textContent = '后端服务需要重启更新';
    $('banner-description').textContent = '后台控制功能已经更新。请停止旧的前台进程，再运行 sudo ./install-service.sh --enable-confirmed 安装后台服务，然后刷新本页。';
  } else if (state.demo) {
    $('banner-title').textContent = '当前为演示模式';
    $('banner-description').textContent = '传感器、风扇与控制效果均为模拟数据，演示配置与真实配置独立保存。';
  } else if (state.error) {
    $('banner-title').textContent = '控制服务需要检查';
    $('banner-description').textContent = state.error;
  } else {
    $('banner-title').textContent = '温度监控已就绪，风扇调速等待主板驱动';
    $('banner-description').textContent = '当前未发现 PWM 通道。你可以先绘制并保存每个风扇的曲线，驱动就绪后再绑定；这些配置暂未应用到硬件。';
  }
  $('diagnostic-dot').hidden = !!state.channels.length && !state.gpu_error;
  spark('cpu-spark', 'cpu'); spark('gpu-spark', 'gpu');
  renderSidebar(); renderFanCards(); renderSensors(); renderDiagnostics(); renderDiscovery(); updateIdentification();
  if (dragging === null) drawChart();
}

function spark(id, kind) {
  const entries = state.history.slice(-80);
  let path = '', drawing = false;
  entries.forEach((h,i)=>{
    if(h[kind]==null){drawing=false;return;}
    const x = i / Math.max(1,entries.length-1) * 200;
    const y = 42-Math.max(0,Math.min(100,h[kind]))/100*42;
    path += `${drawing?'L':'M'}${x.toFixed(1)},${y.toFixed(1)} `; drawing=true;
  });
  $(id).innerHTML = `<path d="${path}" fill="none" stroke="currentColor" stroke-width="1.5"/>`;
}

function renderSidebar() {
  $('fan-count').textContent = draft.fans.length;
  $('fan-list').innerHTML = draft.fans.map(f=>`<button class="fan-item ${f.id===selected?'selected':''}" data-fan="${esc(f.id)}"><span>${esc(f.name)}</span><small>${state.outputs[f.id]?.active?'控制中':f.channel_id?'已绑定':'待绑定'}</small></button>`).join('');
}

function selectFan(id) {
  selected = id; selectedPoint = 0; renderEditor(); renderSidebar(); renderFanCards();
}

function renderDiscovery() {
  const devices = state.discovery || [];
  const rotating = devices.filter(c=>c.connection==='rotating');
  const known = devices.filter(c=>c.connection==='rotating'||c.identified);
  const pending = devices.filter(c=>c.connection!=='rotating'&&!c.identified);
  $('discovery-summary').textContent = connected ? `检测到 ${rotating.length} 路有转速信号 · ${devices.filter(c=>c.identified).length} 路已命名确认 · ${pending.length} 路待确认` : '连接中断 · 设备读数已过期';
  const card = c => {
    const configured = draft.fans.find(f=>f.channel_id===c.id);
    const testing = state.identification?.channel_id===c.id;
    return `<article class="discovery-card ${c.identified?'identified':''}"><div class="discovery-card-top"><span class="fan-symbol">✣</span><div><h3>${esc(c.display_name)}</h3><div class="chip-label">${esc(c.chip)} · ${esc(c.id.split('/').at(-1))} · ${esc(c.name)}</div></div><span class="identity-badge">${testing?'测试中':c.identified?'已确认':c.connection==='rotating'?'待辨认':'连接待确认'}</span></div><div class="discovery-readings"><strong>${connected?fmt(c.rpm):'—'} <small>RPM</small></strong><span>${connected?fmt(c.duty,1):'—'}% · 硬件输出</span></div><div class="discovery-actions"><button class="button ${c.identified?'secondary':'primary'} small" data-identify="${esc(c.id)}" ${!connected?'disabled':''}>${c.identified?'重新识别 / 改名':'识别并命名'}</button>${configured?`<button class="text-button" data-edit-curve="${esc(configured.id)}">编辑曲线 ↗</button>`:''}</div></article>`;
  };
  $('discovered-fans').innerHTML=known.length?known.map(card).join(''):'<div class="panel fan-empty">尚未检测到有转速信号的控制通道。检查驱动，或展开下方待确认通道。</div>';
  $('unconfirmed-channels').hidden=!pending.length;
  $('unconfirmed-summary').textContent=`其他待确认通道 · ${pending.length}`;
  $('unconfirmed-fans').innerHTML=pending.map(card).join('');
  $('scan-fans').disabled=!connected||scanBusy;
}

function resetDraft() {
  draft=clone(state.config);
  if(!draft.fans.some(f=>f.id===selected))selected=draft.fans[0]?.id;
  dirty=false;
  $('save-status').textContent='配置与硬件列表已同步';
  renderEditor();renderSidebar();renderFanCards();renderDiscovery();
}

function openIdentification(channelId) {
  if(dirty)return toast('请先保存或导出当前曲线修改，再进行设备识别',true);
  const device=state.discovery.find(c=>c.id===channelId);
  if(!device)return;
  identifyingChannel=channelId;
  $('identify-title').textContent=device.identified?'重新识别与命名':'这颗风扇在哪里？';
  $('identify-channel').textContent=`${device.chip} · ${device.id.split('/').at(-1)} · ${device.name}`;
  $('identify-name').value=device.display_name;
  $('identify-kind').value=device.device_kind;
  $('identify-slider').value=100;
  $('identify-output').textContent='100%';
  $('identify-dialog').showModal();
  identificationKindChanged();updateIdentification();
}

function identificationKindChanged() {
  const device=state.discovery.find(c=>c.id===identifyingChannel);
  const kind=$('identify-kind').value;
  const pump=kind==='pump'||(device?.name.toLowerCase().includes('pump')&&kind!=='fan');
  $('identify-slider').min=pump?100:50;
  $('identify-min').textContent=pump?'100%':'50%';
  $('identify-pump-help').hidden=!pump;
  if(pump)$('identify-slider').value=100;
  $('identify-output').textContent=`${$('identify-slider').value}%`;
}

function updateIdentification() {
  if(!$('identify-dialog').open)return;
  const device=state.discovery?.find(c=>c.id===identifyingChannel);
  const test=state.identification?.channel_id===identifyingChannel?state.identification:null;
  $('identify-rpm').textContent=connected?fmt(device?.rpm):'—';
  $('identify-hardware-duty').textContent=connected?fmt(device?.duty,1):'—';
  const canTest=connected&&state.control_allowed&&device?.writable;
  $('identify-apply').disabled=!canTest||identifyBusy;
  $('identify-full').disabled=!canTest||identifyBusy;
  $('identify-stop').disabled=!connected||!test||identifyBusy;
  $('identify-save').disabled=!connected||identifyBusy;
  $('identify-test-status').textContent=!connected?'服务连接中断，正在等待测试超时恢复':test?.stopping?'恢复原状态时遇到错误，服务正在重试':test?`${test.fault||`正在以 ${fmt(test.effective_duty)}% 测试`} · ${test.remaining_s} 秒后自动恢复`:!state.control_allowed?'当前为监控模式，实际测试需 sudo ./webui.sh --control':!device?.writable?device?.reason||'通道不可用':'当前未测试，硬件保持原控制状态';
}

async function applyIdentification(duty) {
  if(identifyBusy)return;
  identifyBusy=true;updateIdentification();
  try{
    await api('/api/identify',{channel_id:identifyingChannel,duty,device_kind:$('identify-kind').value});
    await refresh();
  }catch(error){toast(error.message,true);}
  finally{identifyBusy=false;updateIdentification();}
}

async function stopIdentification() {
  if(!identifyingChannel)return;
  await api('/api/identify-stop',{channel_id:identifyingChannel});
  await refresh();
}

async function closeIdentification() {
  if(identifyBusy)return;
  try{await stopIdentification();}catch(error){toast('恢复请求未完成；服务会在 15 秒无操作后恢复。'+error.message,true);}
  $('identify-dialog').close();identifyingChannel=null;
}

const numericFields = {'manual-duty':'manual_duty','min-duty':'min_duty','max-duty':'max_duty',hysteresis:'hysteresis',response:'response_s',smoothing:'smoothing_s',critical:'critical_temp','step-up':'step_up','step-down':'step_down'};

function options(items, current, empty, value = 'id', label = 'name') {
  let html = `<option value="">${esc(empty)}</option>`;
  if(current && !items.some(i=>i[value]===current)) html += `<option value="${esc(current)}">离线：${esc(current)}</option>`;
  html += items.map(i=>`<option value="${esc(i[value])}">${esc(i.chip ? `${i.chip} · ${i[label]}` : i[label])}</option>`).join('');
  return html;
}

function renderEditor() {
  const f = fan();
  $('curve-chart').closest('.editor-grid').hidden = !f;
  $('editor-empty').hidden=!!f;
  if(!f) return;
  $('fan-name').value = f.name;
  $('channel-select').innerHTML = options(state.channels,f.channel_id,'暂不绑定 · 仅编辑曲线');
  $('channel-select').value = f.channel_id || '';
  $('rpm-select').innerHTML = options(state.fans,f.rpm_sensor_id,'不绑定测速信号');
  $('rpm-select').value = f.rpm_sensor_id || '';
  $('autostart-control').checked=!!f.autostart;
  $('autostart-control').disabled=!f.identified||!f.channel_id;
  $('autostart-help').textContent=state.background_service?'保存后由后台持续应用此路设置，服务重启时自动恢复。':'安装后台服务后生效；当前仍是临时前台运行。';
  $('save-help').textContent=state.background_service?'保存即应用后台策略，关闭网页不影响调速。':'保存会释放已接管风扇，启用后再应用。';
  $('save-config').textContent=state.background_service?'保存并应用 ↗':'保存配置 ↗';
  const channel = state.channels.find(c=>c.id===f.channel_id);
  $('binding-badge').textContent = channel ? channel.name : f.channel_id ? '通道离线' : '待绑定';
  $('channel-help').textContent = channel ? channel.reason || '通道可用。保存配置后，可在风扇卡片中启用。' : '驱动未就绪时可先保存曲线，之后再绑定通道。';
  const sourceOptions = [['@cpu','CPU 最高温度'],['@gpu','GPU 最高温度'],['@cpu-gpu','CPU / GPU 取最高温度'],['@custom','自定义多温度组合']];
  $('source-select').innerHTML = sourceOptions.map(([id,name])=>`<option value="${id}">${name}</option>`).join('') + state.sensors.map(s=>`<option value="${esc(s.id)}">${esc(`${s.chip} · ${s.name}`)}</option>`).join('');
  if(f.sensor_ids.length===1 && !$('source-select').querySelector(`option[value="${CSS.escape(f.sensor_ids[0])}"]`)){
    const opt = document.createElement('option'); opt.value=f.sensor_ids[0];opt.textContent=`离线：${f.sensor_ids[0]}`;$('source-select').append(opt);
  }
  $('source-select').value = f.sensor_ids.length===1 ? f.sensor_ids[0] : '@custom';
  $('mix-details').open = f.sensor_ids.length>1;
  renderSensorCheckboxes();
  $('aggregation').value = f.aggregation;
  $('mode-select').value = f.mode;
  $('manual-field').hidden = f.mode !== 'manual';
  for(const [id,key] of Object.entries(numericFields)) $(id).value=f[key];
  $('preset').value = 'custom';
  renderPoints(); drawChart();
}

function renderSensorCheckboxes() {
  const f = fan();
  const all = [{id:'@cpu',name:'CPU 最高温度'},{id:'@gpu',name:'GPU 最高温度'},...state.sensors];
  for(const id of f.sensor_ids) if(!all.some(s=>s.id===id)) all.push({id,name:id==='@cpu-gpu'?'CPU / GPU 最高温度':`离线：${id}`});
  $('sensor-checkboxes').innerHTML = all.map(s=>`<label class="sensor-choice"><input type="checkbox" value="${esc(s.id)}" ${f.sensor_ids.includes(s.id)?'checked':''}><span>${esc(s.chip ? `${s.chip} · ${s.name}` : s.name)}</span></label>`).join('');
}

function renderPoints() {
  const f = fan(); if(!f) return;
  $('point-count').textContent = `${f.points.length} 个节点`;
  $('point-list').innerHTML = f.points.map((p,i)=>`<div class="point ${i===selectedPoint?'selected':''}"><div class="point-index"><span>POINT ${String(i+1).padStart(2,'0')}</span><button data-remove-point="${i}" aria-label="删除节点 ${i+1}" ${f.points.length<=2?'disabled':''}>×</button></div><div class="point-values"><input type="number" value="${p[0]}" min="-20" max="120" step="1" data-point="${i}" data-axis="0" aria-label="节点 ${i+1} 温度"><span>°C</span><span class="arrow">→</span><input type="number" value="${p[1]}" min="0" max="100" step="1" data-point="${i}" data-axis="1" aria-label="节点 ${i+1} 输出"><span>%</span></div></div>`).join('');
}

// SVG coordinates stay stable under resize and support pointer, touch and keyboard.
const chart = {left:58,right:770,top:22,bottom:291};
function domain() {const points=fan()?.points||[];return [Math.min(20,...points.map(p=>p[0])),Math.max(100,...points.map(p=>p[0]))];}
function xy(t,d) {const [lo,hi]=domain();return [chart.left+(t-lo)/(hi-lo)*(chart.right-chart.left), chart.bottom-d/100*(chart.bottom-chart.top)];}
function eventPoint(event) {
  const p = $('curve-chart').createSVGPoint();p.x=event.clientX;p.y=event.clientY;
  const point = p.matrixTransform($('curve-chart').getScreenCTM().inverse());
  const [lo,hi]=domain();
  return [Math.round(Math.max(-20,Math.min(120,lo+(point.x-chart.left)/(chart.right-chart.left)*(hi-lo)))),Math.round(Math.max(0,Math.min(100,(chart.bottom-point.y)/(chart.bottom-chart.top)*100)))];
}

function drawChart() {
  const f = fan(); if(!f || !state) return;
  const [lo,hi] = domain();
  let html='<defs><linearGradient id="curve-fill" x1="0" x2="0" y1="0" y2="1"><stop offset="0" stop-color="#bcf47a" stop-opacity=".16"/><stop offset="1" stop-color="#bcf47a" stop-opacity=".01"/></linearGradient></defs>';
  for(let d=0;d<=100;d+=20){const y=xy(lo,d)[1];html+=`<line x1="${chart.left}" y1="${y}" x2="${chart.right}" y2="${y}" stroke="#2b332b" stroke-dasharray="3 5"/><text x="43" y="${y+3}" text-anchor="end" fill="#76856e" font-size="10">${d}</text>`;}
  for(let t=Math.ceil(lo/10)*10;t<=hi;t+=10){const x=xy(t,0)[0];html+=`<line x1="${x}" y1="${chart.top}" x2="${x}" y2="${chart.bottom}" stroke="#242d24"/><text x="${x}" y="315" text-anchor="middle" fill="#76856e" font-size="10">${t}°</text>`;}
  const coords = [xy(lo,f.points[0][1]),...f.points.map(p=>xy(...p)),xy(hi,f.points.at(-1)[1])];
  const line = coords.map((p,i)=>`${i?'L':'M'}${p[0]},${p[1]}`).join(' ');
  html+=`<path d="${line} L${chart.right},${chart.bottom} L${chart.left},${chart.bottom} Z" fill="url(#curve-fill)"/><path d="${line}" fill="none" stroke="#bcf47a" stroke-width="2.5" stroke-linejoin="round"/>`;
  const temp = sourceTemperature(f);
  if(temp!=null && connected){const x=xy(Math.max(lo,Math.min(hi,temp)),0)[0];const duty=f.mode==='manual'?f.manual_duty:interpolate(f.points,temp);const y=xy(temp,duty)[1];html+=`<line x1="${x}" y1="${chart.top}" x2="${x}" y2="${chart.bottom}" stroke="#88bded" stroke-opacity=".7" stroke-dasharray="4 5"/><circle cx="${x}" cy="${y}" r="5" fill="#88bded" stroke="#191d1b" stroke-width="2"/>`;}
  f.points.forEach((p,i)=>{const [x,y]=xy(...p);html+=`<g data-node="${i}" tabindex="0" role="slider" aria-label="节点 ${i+1}：${p[0]} 摄氏度，${p[1]} 百分比" aria-valuemin="0" aria-valuemax="100" aria-valuenow="${p[1]}" style="cursor:grab"><circle cx="${x}" cy="${y}" r="16" fill="transparent"/><circle cx="${x}" cy="${y}" r="${i===selectedPoint?6:4.5}" fill="#191d1b" stroke="#bcf47a" stroke-width="2"/>${i===selectedPoint?`<rect x="${Math.min(chart.right-92,Math.max(chart.left,x-42))}" y="${Math.max(1,y-34)}" width="88" height="21" rx="4" fill="#313f2a"/><text x="${Math.min(chart.right-48,Math.max(chart.left+44,x+2))}" y="${Math.max(15,y-20)}" text-anchor="middle" fill="#d1efb5" font-size="10">${p[0]}°C · ${p[1]}%</text>`:''}</g>`;});
  $('curve-chart').innerHTML=html;
  $('chart-current').textContent = !connected?'读数已过期':`${temp==null?'温度源不可用':fmt(temp,1)+'°C'}${dirty?' · 未保存预览':f.mode==='manual'?' · 固定输出模式':' · 实时输入'}`;
}

function setPoint(i, t, d) {
  const f=fan();
  const low=i?f.points[i-1][0]+1:-20;
  const high=i<f.points.length-1?f.points[i+1][0]-1:120;
  f.points[i]=[Math.max(low,Math.min(high,t)),Math.max(0,Math.min(100,d))];
  selectedPoint=i; markDirty();
}

function removePoint(i) {if(fan().points.length<=2) return toast('曲线至少需要两个节点',true);fan().points.splice(i,1);selectedPoint=Math.min(i,fan().points.length-1);markDirty();renderPoints();}
function addPoint(point) {
  const f=fan();if(f.points.length>=32)return toast('最多 32 个节点',true);
  if(f.points.some(p=>p[0]===point[0]))return toast('这个温度已有节点，请拖动现有节点');
  f.points.push(point);f.points.sort((a,b)=>a[0]-b[0]);selectedPoint=f.points.indexOf(point);markDirty();renderPoints();
}

function renderFanCards() {
  $('fan-cards').innerHTML=draft.fans.length?draft.fans.map(f=>{
    const o=state.outputs[f.id]||{}, c=state.channels.find(c=>c.id===f.channel_id);
    const canActivate=connected && !dirty && state.control_allowed && c?.writable && f.identified;
    const requested=state.background_service && state.config.fans.some(saved=>saved.id===f.id && saved.autostart);
    const release=o.active||requested;
    const pending=state.pending_autostart?.[f.id];
    const status=connected?(pending?'等待后台接管':o.status||'未保存'):'连接中断 · 状态未知';
    const label=release?(state.background_service?'停止后台控制':'释放控制'):!f.identified?'先识别并命名':state.background_service?'启用后台控制':state.demo?'启用模拟控制':'启用控制';
    return `<article class="fan-card ${f.id===selected?'selected':''}" data-select="${esc(f.id)}">
      <div class="fan-card-top"><span class="fan-symbol">✣</span><div><h3>${esc(f.name)}</h3><div class="chip-label">${c?esc(`${c.chip} · ${c.name}`):f.channel_id?'已绑定通道离线':'尚未绑定 PWM 通道'}</div></div><span class="status-tag ${o.active&&connected?'active':''}">${esc(status)}</span></div>
      <div class="fan-values"><div><div class="value">${connected?fmt(o.target):'—'}<small>%</small></div><div class="caption">${dirty?'已保存配置的目标':'目标输出'} · ${o.active&&connected?'已接管':'未应用'}</div></div><div><div class="value">${connected?fmt(o.rpm):'—'}<small>RPM</small></div><div class="caption">${f.rpm_sensor_id?'测速读数':'尚未绑定测速'}</div></div><div><div class="value">${connected?fmt(o.temperature):'—'}<small>°C</small></div><div class="caption">输入温度</div></div></div>
      <div class="fan-progress"><i style="width:${connected?Math.max(0,Math.min(100,o.target||0)):0}%"></i></div>
      ${o.fault?`<p class="fault">${esc(o.fault)}</p>`:''}${pending?`<p class="fault">${esc(pending)}</p>`:''}
      <div class="fan-card-bottom"><button class="text-button" data-delete="${esc(f.id)}">删除配置</button><button class="button ${release?'secondary':'primary'}" data-action="${release?'release':'activate'}" data-id="${esc(f.id)}" ${(!connected||(!release&&!canActivate))?'disabled':''}>${label}</button></div>
    </article>`;
  }).join(''):'<div class="panel fan-empty">还没有风扇配置。请先扫描和识别硬件，或添加离线配置。</div>';
}

function renderSensors() {
  $('sensor-table').innerHTML = [...state.sensors.map(s=>`<tr><td>${esc(s.name)}</td><td>${esc(s.chip)}</td><td>${s.kind==='cpu'?'CPU':s.kind==='gpu'?'GPU':'温度'}</td><td>${s.value==null?'不可用':fmt(s.value,1)+' °C'}</td></tr>`),...state.fans.map(f=>`<tr><td>${esc(f.name)}</td><td>${esc(f.chip)}</td><td>风扇测速</td><td>${f.rpm==null?'不可用':fmt(f.rpm)+' RPM'}</td></tr>`)].join('');
}

function renderDiagnostics() {
  const row=(k,v)=>`<div class="row"><span>${esc(k)}</span><span>${esc(v)}</span></div>`;
  let html=`<article class="panel diagnostic-card"><h3>本机服务</h3>${row('主机',state.hostname)}${row('版本',state.version)}${row('运行模式',state.demo?'演示模式：完全隔离的模拟设备':state.background_service?'后台服务：持续调速并恢复保存的控制策略':state.control_allowed?'临时前台控制：需手动启用':'监控与编辑：未开启硬件写入')}${row('GPU 读取',state.gpu_error||'正常')}${row('控制状态',state.error||'正常')}${state.background_service?'<p>后台服务独立于浏览器和 SSH 终端。保存设置后立即应用，启动时自动恢复勾选的通道；停止某路后台控制会保存其停用状态。停止整个服务：<code>sudo systemctl stop fancontrollx</code>。</p>':'<p>当前是临时前台运行，关闭网页不停止服务，但退出启动终端可能停止。安装后台服务：先 Ctrl+C 停止前台，再执行 <code>sudo ./install-service.sh --enable-confirmed</code>。</p>'}</article>`;
  html+=`<article class="panel diagnostic-card"><h3>PWM 控制通道 · ${state.channels.length}</h3>${state.channels.length?state.channels.map(c=>`<div class="row"><span>${esc(c.chip+' · '+c.name)}</span><span>${esc(c.reason||'可控制')}<br><code>${esc(c.path)}</code><br>当前 ${fmt(c.duty,1)}% · ${c.watchdog?'支持硬件超时恢复':'无硬件超时恢复接口'}</span></div>`).join(''):'<p>系统尚未暴露主板 PWM 接口。请确认主板驱动已经加载；安装后台服务后会在当前内核开机时加载 nct6687；临时手动加载仅本次启动有效。若加载时报签名错误，再检查模块签名和 MOK 证书登记状态。你仍可先编辑和保存曲线。</p>'}<p>只有已验证控制模式的驱动允许写入。测速通道需按实际接线选择；分线器通常只返回一个风扇的测速。</p></article>`;
  html+=`<article class="panel diagnostic-card"><h3>已检测 hwmon 设备</h3>${state.chips.map(c=>row(c.name,c.path)).join('')}</article>`;
  html+=`<article class="panel diagnostic-card"><h3>远程访问</h3><p>服务仅监听回环地址。请在你的电脑执行 SSH 转发，然后打开本地浏览器：</p><p><code>ssh -L 8090:127.0.0.1:8090 sky@${esc(state.hostname)}</code></p><p>浏览器地址：<code>http://127.0.0.1:8090</code></p><p>配置入口：<code>./webui.sh</code><br>安装后台与开机自启：<code>sudo ./install-service.sh --enable-confirmed</code><br>查看后台：<code>systemctl status fancontrollx</code><br>独立演示：<code>./webui.sh --demo --port 8091</code></p></article>`;
  $('diagnostics-content').innerHTML=html;
  $('event-list').innerHTML=state.events.length?state.events.map(e=>`<div class="event-row"><time>${new Date(e.time*1000).toLocaleTimeString('zh-CN',{hour12:false})}</time><span>${esc(e.message)}</span></div>`).join(''):'<div class="event-row">暂无控制事件。配置与运行日志会显示在这里。</div>';
}

function tab(name) {
  const titles={control:['让散热，恰到好处','观察温度，绘制曲线，让每一颗风扇按你的方式运转。','FAN CONTROL'],sensors:['感知每一度变化','来自本机硬件的真实读数，持续更新。','SENSORS'],diagnostics:['了解你的散热系统','驱动、权限和控制状态，都在这里。','DIAGNOSTICS']};
  for(const key of Object.keys(titles)) $(`tab-${key}`).hidden=key!==name;
  document.querySelectorAll('[data-tab]').forEach(el=>el.classList.toggle('active',el.dataset.tab===name));
  $('page-title').textContent=titles[name][0];
  $('page-subtitle').textContent=titles[name][1];
  $('tab-english').textContent=titles[name][2];
}

async function save() {
  if(busy || !draft)return;
  if(!connected)return toast('服务未连接，无法保存配置',true);
  if(!state.background_service&&Object.values(state.outputs).some(o=>o.active)&&!confirm('保存配置将释放当前已接管的风扇，之后需要重新启用。继续？'))return;
  busy=true;$('save-config').disabled=true;
  try{await api('/api/config',draft);dirty=false;await refresh();draft=clone(state.config);renderEditor();renderSidebar();renderFanCards();$('save-status').textContent='✓ 配置已保存到本机';$('save-status').style.color='var(--muted)';toast(state.background_service?'配置已保存，后台已应用持续控制策略':'配置已保存；尚未接管的风扇不会改变转速');}
  catch(error){toast(error.message,true);}
  finally{busy=false;$('save-config').disabled=false;}
}

function download(name,value) {
  const url=URL.createObjectURL(new Blob([JSON.stringify(value,null,2)+'\n'],{type:'application/json'}));
  const a=document.createElement('a');a.href=url;a.download=name;a.click();setTimeout(()=>URL.revokeObjectURL(url),500);
}

document.querySelectorAll('[data-tab]').forEach(el=>el.addEventListener('click',()=>tab(el.dataset.tab)));
$('scan-fans').addEventListener('click',async()=>{
  if(dirty)return toast('请先保存当前修改，再重新扫描',true);
  scanBusy=true;renderDiscovery();
  try{const result=await api('/api/scan',{});await refresh();resetDraft();toast(`扫描完成，新增 ${result.added} 个有测速信号的设备`);}
  catch(error){toast(error.message,true);}
  finally{scanBusy=false;renderDiscovery();}
});
for(const id of ['discovered-fans','unconfirmed-fans'])$(id).addEventListener('click',event=>{
  const identify=event.target.closest('[data-identify]'),edit=event.target.closest('[data-edit-curve]');
  if(identify)openIdentification(identify.dataset.identify);
  if(edit){selectFan(edit.dataset.editCurve);$('curve-chart').closest('.editor-grid').scrollIntoView({behavior:'smooth',block:'start'});}
});
$('identify-slider').addEventListener('input',()=>{$('identify-output').textContent=`${$('identify-slider').value}%`;});
$('identify-slider').addEventListener('change',()=>{
  if(state.identification?.channel_id===identifyingChannel)applyIdentification(Number($('identify-slider').value));
});
$('identify-kind').addEventListener('change',identificationKindChanged);
$('identify-apply').addEventListener('click',()=>applyIdentification(Number($('identify-slider').value)));
$('identify-full').addEventListener('click',()=>{$('identify-slider').value=100;$('identify-output').textContent='100%';applyIdentification(100);});
$('identify-stop').addEventListener('click',async()=>{try{await stopIdentification();toast('测试已结束，已恢复原状态');}catch(error){toast(error.message,true);}});
$('identify-close').addEventListener('click',closeIdentification);
$('identify-dialog').addEventListener('cancel',event=>{event.preventDefault();closeIdentification();});
$('identify-save').addEventListener('click',async()=>{
  if(identifyBusy)return;
  identifyBusy=true;updateIdentification();
  try{
    const result=await api('/api/identity',{channel_id:identifyingChannel,name:$('identify-name').value,device_kind:$('identify-kind').value});
    await refresh();selected=result.id;resetDraft();
    $('identify-dialog').close();identifyingChannel=null;
    toast('名称已保存，测试已结束；可以继续设置温度曲线');
  }catch(error){toast(error.message,true);}
  finally{identifyBusy=false;updateIdentification();}
});
function abandonIdentification(){
  if(!identifyingChannel||state?.identification?.channel_id!==identifyingChannel)return;
  fetch('/api/identify-stop',{method:'POST',headers:{'Content-Type':'application/json','X-FanControl-Token':state.csrf_token},body:JSON.stringify({channel_id:identifyingChannel}),keepalive:true}).catch(()=>{});
}
window.addEventListener('pagehide',abandonIdentification);
document.addEventListener('visibilitychange',()=>{if(document.visibilityState==='hidden')abandonIdentification();});
$('show-diagnostics').addEventListener('click',()=>tab('diagnostics'));
$('retry').addEventListener('click',refresh);
$('fan-list').addEventListener('click',event=>{const target=event.target.closest('[data-fan]');if(target)selectFan(target.dataset.fan);});
$('fan-name').addEventListener('input',()=>{if(fan())fan().name=$('fan-name').value;markDirty();});
$('channel-select').addEventListener('change',()=>{
  const channelId=$('channel-select').value||null;
  if(channelId!==fan().channel_id){fan().identified=false;fan().device_kind='unknown';fan().rpm_sensor_id=null;fan().autostart=false;}
  fan().channel_id=channelId;markDirty();renderEditor();
});
$('rpm-select').addEventListener('change',()=>{fan().rpm_sensor_id=$('rpm-select').value||null;markDirty();});
$('autostart-control').addEventListener('change',()=>{fan().autostart=$('autostart-control').checked;markDirty();});
$('source-select').addEventListener('change',()=>{const v=$('source-select').value;if(v==='@custom'){$('mix-details').open=true;return;}fan().sensor_ids=[v];fan().aggregation='max';markDirty();renderEditor();});
$('sensor-checkboxes').addEventListener('change',()=>{fan().sensor_ids=[...$('sensor-checkboxes').querySelectorAll('input:checked')].map(el=>el.value);$('source-select').value='@custom';markDirty();});
$('aggregation').addEventListener('change',()=>{fan().aggregation=$('aggregation').value;markDirty();});
$('mode-select').addEventListener('change',()=>{fan().mode=$('mode-select').value;$('manual-field').hidden=fan().mode!=='manual';markDirty();});
for(const [id,key] of Object.entries(numericFields)) $(id).addEventListener('change',()=>{if($(id).value===''){toast('请输入有效数值',true);$(id).value=fan()[key];return;}fan()[key]=Number($(id).value);markDirty();});
$('preset').addEventListener('change',()=>{if(presets[$('preset').value]){fan().points=clone(presets[$('preset').value]);selectedPoint=0;markDirty();renderPoints();}});
$('reset-curve').addEventListener('click',()=>{fan().points=clone(presets.balanced);selectedPoint=0;markDirty();renderPoints();$('preset').value='balanced';});
$('point-list').addEventListener('change',event=>{const e=event.target;if(!e.matches('[data-point]'))return;const i=Number(e.dataset.point),axis=Number(e.dataset.axis),p=[...fan().points[i]];if(e.value===''||!Number.isFinite(Number(e.value))){renderPoints();return;}p[axis]=Math.round(Number(e.value));setPoint(i,...p);renderPoints();});
$('point-list').addEventListener('click',event=>{const e=event.target.closest('[data-remove-point]');if(e)removePoint(Number(e.dataset.removePoint));});
$('add-point').addEventListener('click',()=>{const p=fan().points;let best=null,gap=0;for(let i=1;i<p.length;i++)if(p[i][0]-p[i-1][0]>gap){gap=p[i][0]-p[i-1][0];best=[Math.round((p[i][0]+p[i-1][0])/2),Math.round((p[i][1]+p[i-1][1])/2)];}if(gap<2)return toast('节点之间没有空间，请先移动节点');addPoint(best);});
$('curve-chart').addEventListener('pointerdown',event=>{const target=event.target.closest('[data-node]');if(!target||event.button!==0)return;event.preventDefault();dragging=Number(target.dataset.node);selectedPoint=dragging;$('curve-chart').setPointerCapture(event.pointerId);drawChart();});
$('curve-chart').addEventListener('pointermove',event=>{if(dragging===null)return;setPoint(dragging,...eventPoint(event));});
const endDrag=()=>{if(dragging!==null){dragging=null;renderPoints();}};
$('curve-chart').addEventListener('pointerup',endDrag);$('curve-chart').addEventListener('pointercancel',endDrag);
$('curve-chart').addEventListener('dblclick',event=>{if(!event.target.closest('[data-node]'))addPoint(eventPoint(event));});
$('curve-chart').addEventListener('contextmenu',event=>{const node=event.target.closest('[data-node]');if(node){event.preventDefault();removePoint(Number(node.dataset.node));}});
$('curve-chart').addEventListener('keydown',event=>{const node=event.target.closest('[data-node]');if(!node)return;const i=Number(node.dataset.node),p=[...fan().points[i]],step=event.shiftKey?5:1;if(event.key==='Delete'||event.key==='Backspace'){event.preventDefault();removePoint(i);return;}if(!['ArrowUp','ArrowDown','ArrowLeft','ArrowRight'].includes(event.key))return;event.preventDefault();p[event.key==='ArrowLeft'||event.key==='ArrowRight'?0:1]+=event.key==='ArrowLeft'||event.key==='ArrowDown'?-step:step;setPoint(i,...p);renderPoints();$('curve-chart').querySelector(`[data-node="${i}"]`)?.focus();});
$('save-config').addEventListener('click',save);
document.addEventListener('keydown',event=>{if((event.ctrlKey||event.metaKey)&&event.key==='s'){event.preventDefault();save();}});
window.addEventListener('beforeunload',event=>{if(dirty){event.preventDefault();event.returnValue='';}});
for(const id of ['add-fan','add-fan-bottom'])$(id).addEventListener('click',()=>{$('new-fan-name').value='';$('add-dialog').showModal();$('new-fan-name').focus();});
$('add-dialog').addEventListener('close',()=>{if($('add-dialog').returnValue!=='add')return;const name=$('new-fan-name').value.trim();if(!name)return;const template=draft.fans[0]||{sensor_ids:['@cpu'],aggregation:'max',mode:'curve',points:presets.balanced,manual_duty:50,min_duty:30,max_duty:100,hysteresis:2,response_s:3,smoothing_s:5,step_up:25,step_down:5,critical_temp:85};const f={...clone(template),id:crypto.randomUUID(),name,channel_id:null,rpm_sensor_id:null,identified:false,device_kind:'unknown',autostart:false};draft.fans.push(f);selected=f.id;markDirty();renderEditor();});
$('fan-cards').addEventListener('click',async event=>{
  const action=event.target.closest('[data-action]'),remove=event.target.closest('[data-delete]'),card=event.target.closest('[data-select]');
  if(action){
    if(busy)return;busy=true;action.disabled=true;$('save-config').disabled=true;
    try{
      const enable=action.dataset.action==='activate';
      await api('/api/'+action.dataset.action,{id:action.dataset.id});
      if(state.background_service){const f=draft.fans.find(f=>f.id===action.dataset.id);if(f)f.autostart=enable;}
      await refresh();
      toast(state.background_service?(enable?'已保存后台控制策略，设备就绪后持续应用':'已停止该路后台控制，重启后也不会自动接管'):(enable?'已接管，开始应用保存的配置':'已释放，恢复接管前状态'));
    }catch(error){toast(error.message,true);}finally{busy=false;$('save-config').disabled=false;renderFanCards();}return;
  }
  if(remove){const id=remove.dataset.delete;if(!confirm('删除此风扇配置？保存后生效，保存时会释放已接管的风扇。'))return;draft.fans=draft.fans.filter(f=>f.id!==id);if(selected===id)selected=draft.fans[0]?.id;markDirty();renderEditor();return;}
  if(card)selectFan(card.dataset.select);
});
$('release-all').addEventListener('click',async()=>{try{await api('/api/release-all',{});if(state.background_service)for(const f of draft.fans)f.autostart=false;await refresh();toast(state.background_service?'已停止全部后台控制并保存停用状态':'已释放全部风扇控制');}catch(error){toast(error.message,true);}});
$('export-config').addEventListener('click',()=>download('fancontrollx-config.json',draft));
$('download-diagnostics').addEventListener('click',()=>{const {csrf_token,...safe}=state;download('fancontrollx-diagnostics.json',safe);});
$('import-config').addEventListener('click',()=>$('import-file').click());
$('import-file').addEventListener('change',async()=>{const file=$('import-file').files[0];if(!file)return;try{if(file.size>256*1024)throw new Error('配置文件超过 256 KB');const config=JSON.parse(await file.text());if(!confirm(state.background_service?'导入将替换当前全部配置并应用其中的后台控制策略。继续？':'导入将替换当前全部配置，并释放已接管风扇。继续？'))return;await api('/api/config',config);dirty=false;await refresh();draft=clone(state.config);selected=draft.fans[0]?.id;renderEditor();renderSidebar();renderFanCards();$('save-status').textContent='✓ 已导入并保存';toast('配置已导入并保存');}catch(error){toast(error.message,true);}finally{$('import-file').value='';}});

async function poll(){await refresh();setTimeout(poll,1500);}
poll();
