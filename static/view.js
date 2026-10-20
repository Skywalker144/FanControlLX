import {miniCurve, updateMiniCurve} from './curve.js';

const $=id=>document.getElementById(id);
const esc=v=>String(v??'').replace(/[&<>"']/g,c=>({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;',"'":'&#39;'}[c]));
const fmt=v=>v==null?'—':Number(v).toLocaleString('zh-CN',{maximumFractionDigits:1});
let state,connected=false,timer,generation=0,sequence=0,applied=0;
function theme(value){
  document.documentElement.dataset.theme=value;
  $('theme-toggle').textContent=value==='dark'?'浅色主题':'深色主题';
}
try{theme(localStorage.getItem('fancontrollx-theme')==='dark'?'dark':'light');}catch{theme('light');}
$('theme-toggle').onclick=()=>{
  const value=document.documentElement.dataset.theme==='dark'?'light':'dark';
  theme(value);try{localStorage.setItem('fancontrollx-theme',value);}catch{}
};
function render(){
  $('connection-error').hidden=connected;
  $('connection-text').textContent=connected?'已连接 · 只读观看':'连接中断';
  $('cpu-temp').textContent=fmt(connected?state?.cpu:null);
  $('gpu-temp').textContent=fmt(connected?state?.gpu:null);
  $('service-value').textContent=connected?`${state.fans.filter(f=>f.output?.active).length} 路控制中`:'状态未知';
  if(!state)return;
  $('machine-name').textContent=state.hostname;
  $('version').textContent=`v${state.version}`;
  $('fan-count').textContent=state.fans.length;
  $('empty').hidden=state.fans.length!==0;
  $('service-caption').textContent=state.demo?'演示设备 · 不写入真实硬件':state.background_service?'后台独立运行 · 关闭网页不影响控制':'临时前台服务 · 退出服务将停止控制';
  $('service-error').hidden=!state.error;
  $('service-error').textContent=state.error||'';
  const ids=new Set(state.fans.map(f=>f.id));
  for(const card of $('fan-cards').children)if(!ids.has(card.dataset.fan))card.remove();
  for(const [index,f] of state.fans.entries()){
    let card=[...$('fan-cards').children].find(c=>c.dataset.fan===f.id);
    if(!card){card=document.createElement('article');card.className='fan-card view-card';card.dataset.fan=f.id;$('fan-cards').append(card);}
    const o=f.output||{},value=k=>fmt(connected?o[k]:null);
    const status=!connected?'读数已过期':f.pending?'等待接管':o.status||'等待采样';
    const active=connected&&!!o.active,fault=active&&!!o.fault;
    card.classList.toggle('is-active',active);card.classList.toggle('has-fault',fault);
    const body=`<div class="card-top"><span class="fan-symbol" aria-hidden="true"><svg class="ui-icon" viewBox="0 0 24 24"><circle cx="12" cy="12" r="2"/><path d="M10 10C4 10 4 3 8 3c4 0 5 4 4 7m2 0c0-6 7-6 7-2 0 4-4 5-7 4m0 2c6 0 6 7 2 7-4 0-5-4-7-4m-2 0c0 6-7 6-7 2 0-4 4-5 7-4"/></svg></span><div class="card-name"><h3>${esc(f.name)}</h3><div class="chip-label">${f.mode==='manual'?'手动模式':'曲线模式'} · ${esc(f.curve_name||'独立曲线')}</div></div><span class="status-tag ${active?'active':''} ${fault?'fault':''}">${esc(status)}</span></div>
      <div class="fan-values"><div><span class="value-label">当前输出 <span class="target-reading">目标 ${connected&&o.automatic?'自动':`${value('target')}%`}</span></span><div class="output-reading"><strong>${value('actual')}</strong><small>% 输出</small></div></div><div class="reading"><span>${value('rpm')}</span> RPM<br><span>${value('temperature')}</span> °C 输入</div></div><div class="progress"><i style="width:${connected?Math.max(0,Math.min(100,o.actual||0)):0}%"></i></div>
      <div class="fan-curve-preview">${miniCurve(f.points,true)}<div class="preview-caption">${f.points.length} 个节点${f.mode==='manual'?' · 手动模式，曲线未使用':''}</div></div>${connected&&(o.fault||f.pending)?`<p class="view-fault">${esc(o.fault||f.pending)}</p>`:''}`;
    if(card._body!==body){card.innerHTML=body;card._body=body;}
    updateMiniCurve(card.querySelector('.fan-curve-preview>svg'),f.points,connected?o.temperature:null,f.mode);
    if($('fan-cards').children[index]!==card)$('fan-cards').insertBefore(card,$('fan-cards').children[index]||null);
  }
  $('last-update').textContent=connected?`采样于 ${new Date(state.sample_time*1000).toLocaleTimeString('zh-CN',{hour12:false})}`:'读数已过期';
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
