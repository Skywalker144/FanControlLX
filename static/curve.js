import {attribute, text} from './ui.js';

// Shared read-only curve rendering for the viewer and editor.
const fmt = (value, digits=0) => value == null ? "—" : Number(value).toFixed(digits).replace(/\.0$/, "");
function interpolate(points,t){if(t<=points[0][0])return points[0][1];for(let i=1;i<points.length;i++){const [a,b]=[points[i-1],points[i]];if(t<=b[0])return a[1]+(b[1]-a[1])*(t-a[0])/(b[0]-a[0]);}return points.at(-1)[1];}
function updateMiniCurve(svg,points,temperature,mode){
  const marker=svg.querySelector('[data-curve-current]'),available=Number.isFinite(temperature);
  attribute(marker,'visibility',available?'visible':'hidden');
  if(!available){attribute(svg,'aria-label','曲线预览：当前温度不可用');return;}
  const low=Math.min(20,points[0][0]),high=Math.max(100,points.at(-1)[0]);
  const duty=interpolate(points,temperature),x=12+(Math.max(low,Math.min(high,temperature))-low)/(high-low)*236;
  attribute(marker,'transform',`translate(${x} 0)`);
  attribute(marker.querySelector('circle'),'cy',64-duty*.52);
  const reading=`当前温度 ${fmt(temperature,1)}°C · ${mode==='manual'?'曲线参考输出':'曲线目标输出'} ${fmt(duty,1)}%${mode==='manual'?'（手动模式，曲线未使用）':''}`;
  text(marker.querySelector('title'),reading);
  attribute(svg,'aria-label',`曲线预览：${reading}`);
}
function miniCurve(points,live=false){
  const low=Math.min(20,points[0][0]),high=Math.max(100,points.at(-1)[0]);
  const xy=([t,d])=>[12+(t-low)/(high-low)*236,64-d*.52];
  const coords=[[12,xy(points[0])[1]],...points.map(xy),[248,xy(points.at(-1))[1]]].map(p=>p.join(',')).join(' ');
  return `<svg viewBox="0 0 260 80" role="img" aria-label="曲线预览：${points.map(([t,d])=>`${t}°C ${d}%`).join('，')}"><path d="M12,12H248M12,38H248M12,64H248" stroke="var(--line)" stroke-dasharray="2 4" fill="none"/><polygon points="12,64 ${coords} 248,64" fill="var(--accent)" opacity=".08"/><polyline points="${coords}" fill="none" stroke="var(--accent)" stroke-width="2" stroke-linejoin="round"/>${points.map(p=>{const [x,y]=xy(p);return `<circle cx="${x}" cy="${y}" r="3" fill="var(--panel)" stroke="var(--accent)" stroke-width="1.5"/>`;}).join('')}${live?'<g data-curve-current visibility="hidden"><title></title><line x1="0" y1="12" x2="0" y2="64" stroke="var(--blue)" stroke-width="1.5" stroke-dasharray="3 3"/><circle cx="0" cy="64" r="4" fill="var(--blue)" stroke="var(--panel)" stroke-width="1.5"/></g>':''}<g fill="var(--muted)" font-size="8"><text x="12" y="77">${low}°</text><text x="248" y="77" text-anchor="end">${high}°C</text><text x="248" y="8" text-anchor="end">100%</text></g></svg>`;
}

export {interpolate, miniCurve, updateMiniCurve};
