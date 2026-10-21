// Shared browser primitives; no framework, timers, or background work.
export const icon=name=>`<svg class="ui-icon" viewBox="0 0 24 24" aria-hidden="true"><use href="/icons.svg#icon-${name}"></use></svg>`;

export function text(element,value){
  const next=String(value??'');
  if(element.textContent!==next)element.textContent=next;
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

export function installTheme(button){
  const apply=value=>{
    document.documentElement.dataset.theme=value;
    const dark=value==='dark',label=dark?'切换为浅色主题':'切换为深色主题';
    attribute(button,'aria-label',label);
    attribute(button,'title',label);
    attribute(button,'aria-pressed',dark);
  };
  let saved;
  try{saved=localStorage.getItem('fancontrollx-theme');}catch{}
  apply(saved==='dark'?'dark':'light');
  button.addEventListener('click',()=>{
    const value=document.documentElement.dataset.theme==='dark'?'light':'dark';
    apply(value);
    try{localStorage.setItem('fancontrollx-theme',value);}catch{}
  });
}
