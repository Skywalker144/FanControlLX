// Blocking head script: resolve the palette before styles and page content paint.
(()=>{
  const key='fancontrollx-theme';
  const device=matchMedia('(prefers-color-scheme: dark)');
  const normalize=value=>value==='light'||value==='dark'?value:'system';
  const read=()=>{
    try{return normalize(localStorage.getItem(key));}catch{return 'system';}
  };
  let mode=read();
  const apply=()=>{
    document.documentElement.dataset.theme=mode==='system'?(device.matches?'dark':'light'):mode;
    const select=document.getElementById('theme-select');
    if(select)select.value=mode;
  };
  apply();
  device.addEventListener('change',()=>{if(mode==='system')apply();});
  window.addEventListener('storage',event=>{
    if(event.key===key||event.key===null){mode=read();apply();}
  });
  document.addEventListener('DOMContentLoaded',()=>{
    apply();
    document.getElementById('theme-select').addEventListener('change',event=>{
      mode=normalize(event.target.value);
      apply();
      try{localStorage.setItem(key,mode);}catch{}
    });
  },{once:true});
})();
