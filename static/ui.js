// Shared browser primitives; no framework, timers, or background work.
export const icon=name=>`<svg class="ui-icon" viewBox="0 0 24 24" aria-hidden="true"><use href="/icons.svg#icon-${name}"></use></svg>`;
const timeFormat=new Intl.DateTimeFormat('zh-CN',{hour:'2-digit',minute:'2-digit',second:'2-digit',hour12:false});
export const formatTime=seconds=>timeFormat.format(new Date(seconds*1000));

export function text(element,value){
  const next=String(value??'');
  const node=element.firstChild;
  if(node?.nodeType===Node.TEXT_NODE&&node===element.lastChild){
    if(node.nodeValue!==next)node.nodeValue=next;
  }else if(element.textContent!==next)element.textContent=next;
}

export function attribute(element,name,value){
  const next=String(value);
  if(element.getAttribute(name)!==next)element.setAttribute(name,next);
}

const markup=new WeakMap();
export function html(element,value){
  if(markup.get(element)===value)return;
  element.innerHTML=value;
  markup.set(element,value);
}

export function spark(element,history,kind){
  const values=history.slice(-80);let path='',drawing=false;
  values.forEach((row,i)=>{
    if(!Number.isFinite(row[kind])){drawing=false;return;}
    path+=`${drawing?'L':'M'}${i/Math.max(1,values.length-1)*200},${40-(Math.max(20,Math.min(100,row[kind]))-20)/80*38} `;
    drawing=true;
  });
  if(!element.firstElementChild)element.innerHTML='<path fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round"/>';
  attribute(element.firstElementChild,'d',path);
}

export function updateOutputBar(bar,output={}){
  const actual=Number.isFinite(output.actual)?output.actual:null;
  const target=output.requested??output.target;
  const available=Number.isFinite(target)&&!output.automatic;
  const width=`${Math.max(0,Math.min(100,actual??0))}%`,fill=bar.querySelector('i');
  if(fill.style.width!==width)fill.style.width=width;
  const marker=bar.querySelector('[data-output-target]');
  if(marker.hidden!==!available)marker.hidden=!available;
  if(available){
    const left=`${Math.max(0,Math.min(100,target))}%`;
    if(marker.style.left!==left)marker.style.left=left;
    attribute(marker,'title',`目标 ${target}%`);
  }
  attribute(bar,'aria-label',`当前输出 ${actual==null?'不可用':`${actual}%`} · 目标 ${output.automatic?'显卡自动调速':available?`${target}%`:'不可用'}`);
}

// Large workspaces refresh live readouts near the viewport.
// Entering the viewport renders the newest state immediately.
export function createCardVisibility(render,leave=()=>{}){
  const visibility=new WeakMap();
  const observer=typeof IntersectionObserver==='function'?new IntersectionObserver(entries=>{
    let entered=false;
    for(const {target,isIntersecting} of entries){
      if(!visibility.has(target))continue;
      entered ||= isIntersecting&&visibility.get(target)!==true;
      visibility.set(target,isIntersecting);
      if(!isIntersecting)leave(target);
    }
    if(entered&&!document.hidden)render();
  },{rootMargin:'240px'}):null;
  return {
    visible(card,limit){
      if(!observer)return true;
      if(!limit){this.remove(card);return true;}
      if(!visibility.has(card)){visibility.set(card,null);observer.observe(card);}
      return visibility.get(card)!==false;
    },
    remove(card){observer?.unobserve(card);visibility.delete(card);}
  };
}

export function initFanColumns(select,cards){
  const key='fancontrollx-fan-columns',choices=['auto','1','2','3','4'];
  const apply=value=>{
    value=choices.includes(value)?value:'auto';select.value=value;
    if(value==='auto')cards.removeAttribute('data-columns');else cards.dataset.columns=value;
  };
  try{apply(localStorage.getItem(key));}catch{apply('auto');}
  select.addEventListener('change',()=>{
    apply(select.value);
    try{localStorage.setItem(key,select.value);}catch{}
  });
  window.addEventListener('storage',event=>{if(event.key===key||event.key===null)apply(event.newValue);});
}
