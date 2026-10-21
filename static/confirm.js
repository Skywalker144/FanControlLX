import {text} from './ui.js';

// Native dialog provides focus containment, Escape, and focus restoration.
export function createConfirmation(dialog){
  let pending=null;
  const finish=accepted=>{const resolve=pending;pending=null;resolve?.(accepted);};
  dialog.querySelector('form').addEventListener('submit',event=>finish(event.submitter?.value==='accept'));
  dialog.addEventListener('cancel',()=>finish(false));
  dialog.addEventListener('close',()=>{
    // close is queued by the browser; an earlier close must not settle a new
    // confirmation that has already been opened in the same frame.
    if(!dialog.open)finish(dialog.returnValue==='accept');
  });
  return (message,label='确认')=>{
    // A second click cannot submit the same destructive action twice.
    if(pending)return Promise.resolve(false);
    text(dialog.querySelector('[data-confirm-message]'),message);
    text(dialog.querySelector('[data-confirm-accept]'),label);
    dialog.returnValue='cancel';
    return new Promise(resolve=>{pending=resolve;dialog.showModal();});
  };
}
