import {miniCurve, updateMiniCurve} from './curve.js';
import {icon, text, attribute, installTheme} from './ui.js';

const $=id=>document.getElementById(id);
const esc=v=>String(v??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const fmt=v=>v==null?'—':Number(v).toLocaleString('zh-CN',{maximumFractionDigits:1});
let state,connected=false,timer,generation=0,sequence=0,applied=0;
installTheme($('theme-toggle'));
function render(){
  $('connection-error').hidden=connected;
  text($('connection-text'),connected?'已连接 · 只读观看':'连接中断');
  text($('cpu-temp'),fmt(connected?state?.cpu:null));
  text($('gpu-temp'),fmt(connected?state?.gpu:null));
  text($('service-value'),connected?`${state.fans.filter(f=>f.output?.active).length} 路控制中`:'状态未知');
  if(!state)return;
  text($('machine-name'),state.hostname);
  text($('version'),`v${state.version}`);
  text($('fan-count'),state.fans.length);
  $('empty').hidden=state.fans.length!==0;
  text($('service-caption'),state.demo?'演示设备 · 不写入真实硬件':state.background_service?'后台独立运行 · 关闭网页不影响控制':'临时前台服务 · 退出服务将停止控制');
  $('service-error').hidden=!state.error;
  text($('service-error'),state.error||'');
  const cards=new Map([...$('fan-cards').children].map(card=>[card.dataset.fan,card]));
  const ids=new Set(state.fans.map(f=>f.id));
  for(const [id,card] of cards)if(!ids.has(id)){card.remove();cards.delete(id);}
  for(const [index,f] of state.fans.entries()){
    let card=cards.get(f.id);
    if(!card){card=document.createElement('article');card.className='fan-card view-card';card.dataset.fan=f.id;$('fan-cards').append(card);}
    const signature=JSON.stringify([f.name,f.mode,f.curve_name,f.points]);
    if(card._signature!==signature){
      card._signature=signature;
      card.innerHTML=`<div class="card-top"><span class="fan-symbol">${icon('fan')}</span><div class="card-name"><h3>${esc(f.name)}</h3><div class="chip-label">${f.mode==='manual'?'手动模式':'曲线模式'} · ${esc(f.curve_name||'独立曲线')}</div></div><span data-live="status" class="status-tag"></span></div>
        <div class="fan-values"><div><span class="value-label">当前输出 <span class="target-reading">目标 <span data-live="target"></span></span></span><div class="output-reading"><strong data-live="actual"></strong><small>% 输出</small></div></div><div class="reading"><span data-live="rpm"></span> RPM<br><span data-live="temperature"></span> °C 输入</div></div><div class="progress"><i data-live="progress"></i></div>
        <div class="fan-curve-preview">${miniCurve(f.points,true)}<div class="preview-caption">${f.points.length} 个节点${f.mode==='manual'?' · 手动模式，曲线未使用':''}</div></div><p class="view-fault" data-live="fault" hidden></p>`;
      card._live=Object.fromEntries([...card.querySelectorAll('[data-live]')].map(el=>[el.dataset.live,el]));
      card._curve=card.querySelector('.fan-curve-preview>svg');
    }
    const o=f.output||{},live=card._live,value=k=>fmt(connected?o[k]:null);
    const active=connected&&!!o.active,fault=active&&!!o.fault;
    card.classList.toggle('is-active',active);card.classList.toggle('has-fault',fault);
    text(live.status,!connected?'读数已过期':f.pending?'等待接管':o.status||'等待采样');
    attribute(live.status,'class',`status-tag ${active?'active':''} ${fault?'fault':''}`);
    text(live.target,connected&&o.automatic?'自动':`${value('target')}%`);
    for(const key of ['actual','rpm','temperature'])text(live[key],value(key));
    const width=`${connected?Math.max(0,Math.min(100,o.actual||0)):0}%`;
    if(live.progress.style.width!==width)live.progress.style.width=width;
    const message=connected?(o.fault||f.pending||''):'';
    text(live.fault,message);if(live.fault.hidden!==!message)live.fault.hidden=!message;
    updateMiniCurve(card._curve,f.points,connected?o.temperature:null,f.mode);
    if($('fan-cards').children[index]!==card)$('fan-cards').insertBefore(card,$('fan-cards').children[index]||null);
  }
  text($('last-update'),connected?`采样于 ${new Date(state.sample_time*1000).toLocaleTimeString('zh-CN',{hour12:false})}`:'读数已过期');
}
async function refresh(){
  const request=++sequence;
  try{
    const r=await fetch('/api/view',{cache:'no-store',signal:AbortSignal.timeout(6000)});
    if(!r.ok)throw new Error(`HTTP ${r.status}`);
    const next=await r.json();
    if(request<applied)return;
    applied=request;state=next;connected=true;
  }catch{if(request<applied)return;applied=request;connected=false;}
  if(!document.hidden)render();
}
async function poll(g){
  if(document.hidden||g!==generation)return;
  await refresh();
  if(!document.hidden&&g===generation)timer=setTimeout(()=>poll(g),1500);
}
function resume(){clearTimeout(timer);const g=++generation;if(!document.hidden)poll(g);}
document.addEventListener('visibilitychange',resume);
window.addEventListener('pageshow',resume);
window.addEventListener('pagehide',()=>{clearTimeout(timer);++generation;});
$('retry').onclick=resume;
resume();
