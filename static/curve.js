import {attribute, text} from './ui.js';

// Shared read-only curve rendering for the viewer and editor.
const fmt = (value, digits=0) => value == null ? "—" : Number(value).toFixed(digits).replace(/\.0$/, "");
function interpolate(points,t){if(t<=points[0][0])return points[0][1];for(let i=1;i<points.length;i++){const [a,b]=[points[i-1],points[i]];if(t<=b[0])return a[1]+(b[1]-a[1])*(t-a[0])/(b[0]-a[0]);}return points.at(-1)[1];}
const readings=new WeakMap();
function updateMiniCurve(svg,points,temperature,mode){
  const previous=readings.get(svg);
  if(previous?.points===points&&Object.is(previous.temperature,temperature)&&previous.mode===mode)return;
  readings.set(svg,{points,temperature,mode});
  const marker=svg.querySelector('[data-curve-current]'),available=Number.isFinite(temperature);
  attribute(marker,'visibility',available?'visible':'hidden');
  if(!available){attribute(svg,'aria-label','曲线预览：当前温度不可用');return;}
  const plottedTemperature=Math.max(20,Math.min(100,temperature));
  const duty=interpolate(points,plottedTemperature),x=12+(plottedTemperature-20)/80*236,y=64-duty*.52;
  attribute(marker,'transform',`translate(${x} 0)`);
  attribute(marker.querySelector('line'),'y2',y);
  attribute(marker.querySelector('circle'),'cy',y);
  const reading=`当前温度 ${fmt(temperature,1)}°C · ${mode==='manual'?'曲线参考输出':'曲线目标输出'} ${fmt(duty,1)}%${mode==='manual'?'（手动模式，曲线未使用）':''}`;
  text(marker.querySelector('title'),reading);
  attribute(svg,'aria-label',`曲线预览：${reading}`);
}

// Card controls remain in the document for keyboard navigation. Only the SVG
// preview is released outside the viewport; CSS reserves its exact aspect ratio.
function releaseCardCurve(card){
  card._curve?.remove();card._curve=null;
}
function updateCardCurve(card,points,temperature,mode){
  if(!card._curve){
    const preview=card.querySelector('.fan-curve-preview');
    preview.innerHTML=miniCurve(points,true);card._curve=preview.firstElementChild;
  }
  updateMiniCurve(card._curve,points,temperature,mode);
}
function miniCurve(points,live=false){
  const xy=([t,d])=>[12+(t-20)/80*236,64-d*.52];
  const visible=points.filter(([t])=>t>=20&&t<=100);
  const coords=[xy([20,interpolate(points,20)]),...visible.map(xy),xy([100,interpolate(points,100)])].map(p=>p.join(',')).join(' ');
  return `<svg viewBox="0 0 260 76" role="img" aria-label="曲线预览：${points.map(([t,d])=>`${t}°C ${d}%`).join('，')}"><path d="M12,12H248M12,38H248M12,64H248" stroke="var(--line)" stroke-dasharray="2 4" fill="none"/><polygon points="12,64 ${coords} 248,64" fill="var(--accent)" opacity=".08"/><polyline points="${coords}" fill="none" stroke="var(--accent)" stroke-width="1.5" stroke-linejoin="round"/>${visible.map(p=>{const [x,y]=xy(p);return `<circle cx="${x}" cy="${y}" r="3" fill="var(--panel)" stroke="var(--accent)" stroke-width="1.5"/>`;}).join('')}${live?'<g data-curve-current visibility="hidden" stroke="var(--blue)" stroke-width="1"><title></title><line x1="0" x2="0" y1="64" y2="64"/><circle cx="0" r="3.5" fill="var(--blue)" stroke="var(--panel)"/></g>':''}<g fill="var(--muted)" font-size="8"><text x="12" y="73">20°</text><text x="248" y="73" text-anchor="end">100°C</text><text x="248" y="8" text-anchor="end">100%</text></g></svg>`;
}

export {interpolate, miniCurve, updateMiniCurve, updateCardCurve, releaseCardCurve};
