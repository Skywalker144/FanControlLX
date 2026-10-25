import {interpolate} from './curve.js';
import {attribute, html} from './ui.js';

// Editing-only plot. Polling moves one marker; axes and editable nodes stay put.
export function createCurvePlot(svg){
  const bounds={left:40,right:784,top:20,bottom:300};
  const domain=[20,100];
  const xy=(temperature,duty)=>[
    bounds.left+(temperature-domain[0])/(domain[1]-domain[0])*(bounds.right-bounds.left),
    bounds.bottom-duty/100*(bounds.bottom-bounds.top),
  ];
  let signature='';
  function draw(points,selected,temperature){
    const width=svg.clientWidth||800,height=svg.clientHeight||330;
    const next=JSON.stringify([width,height,points,selected]);
    if(next!==signature){
      signature=next;
      attribute(svg,'viewBox',`0 0 ${width} ${height}`);
      Object.assign(bounds,{right:width-16,bottom:height-30});
      const [low,high]=domain;
      let body='';
      for(let duty=0;duty<=100;duty+=20){
        const y=xy(low,duty)[1];
        body+=`<line x1="${bounds.left}" y1="${y}" x2="${bounds.right}" y2="${y}" stroke="var(--line)" stroke-dasharray="3 5"/><text x="${bounds.left-10}" y="${y+3}" text-anchor="end" fill="var(--muted)" font-size="10">${duty}</text>`;
      }
      const step=width<400?20:10;
      for(let t=Math.ceil(low/step)*step;t<=high;t+=step){
        const x=xy(t,0)[0];
        body+=`<line x1="${x}" y1="${bounds.top}" x2="${x}" y2="${bounds.bottom}" stroke="var(--grid)"/><text x="${x}" y="${bounds.bottom+20}" text-anchor="middle" fill="var(--muted)" font-size="10">${t}°</text>`;
      }
      const coords=[xy(low,interpolate(points,low)),...points.filter(p=>p[0]>=low&&p[0]<=high).map(p=>xy(...p)),xy(high,interpolate(points,high))];
      const line=coords.map((p,i)=>`${i?'L':'M'}${p[0]},${p[1]}`).join(' ');
      body+=`<path d="${line} L${bounds.right},${bounds.bottom} L${bounds.left},${bounds.bottom} Z" fill="var(--accent)" opacity=".07"/><path d="${line}" fill="none" stroke="var(--accent)" stroke-width="2.5"/>`;
      body+='<g data-plot-current visibility="hidden"><line x1="0" x2="0" stroke="var(--blue)" stroke-width="1"/><circle cx="0" r="5" fill="var(--blue)"/></g>';
      points.forEach((p,i)=>{
        if(p[0]<low||p[0]>high)return;
        const [x,y]=xy(...p);
        body+=`<g data-node="${i}" tabindex="0" role="slider" aria-label="节点 ${i+1}，${p[0]} 摄氏度，${p[1]} 百分比" aria-valuemin="0" aria-valuemax="100" aria-valuenow="${p[1]}" style="cursor:grab"><circle cx="${x}" cy="${y}" r="15" fill="transparent"/><circle cx="${x}" cy="${y}" r="${i===selected?6:4.5}" fill="var(--panel)" stroke="var(--accent)" stroke-width="2"/></g>`;
      });
      const focused=svg.contains(document.activeElement)?document.activeElement.dataset.node:null;
      html(svg,body);
      if(focused!=null)(svg.querySelector(`[data-node="${focused}"]`)||svg.querySelector(`[data-node="${selected}"]`))?.focus({preventScroll:true});
    }
    const marker=svg.querySelector('[data-plot-current]');
    attribute(marker,'visibility',Number.isFinite(temperature)?'visible':'hidden');
    if(!Number.isFinite(temperature))return;
    const plottedTemperature=Math.max(domain[0],Math.min(domain[1],temperature));
    const [x,y]=xy(plottedTemperature,interpolate(points,plottedTemperature));
    attribute(marker,'transform',`translate(${x} 0)`);
    const line=marker.querySelector('line');
    attribute(line,'y1',bounds.bottom);attribute(line,'y2',y);
    attribute(marker.querySelector('circle'),'cy',y);
  }
  function eventPoint(event){
    const point=svg.createSVGPoint();point.x=event.clientX;point.y=event.clientY;
    const v=point.matrixTransform(svg.getScreenCTM().inverse());
    return [
      Math.round(Math.max(20,Math.min(100,domain[0]+(v.x-bounds.left)/(bounds.right-bounds.left)*(domain[1]-domain[0])))),
      Math.round(Math.max(0,Math.min(100,(bounds.bottom-v.y)/(bounds.bottom-bounds.top)*100))),
    ];
  }
  return {draw,eventPoint};
}
