import test from 'node:test';
import assert from 'node:assert/strict';
import {bindEditorSelects} from './editor-controls.js';

test('playback timeline scrolling keeps a parameter menu open while anchor scrolling closes it',()=>{
    const events={}, created=[];
    const element=()=>({
        id:'', handlers:{}, children:[], attributes:{}, style:{}, offsetWidth:100, offsetHeight:64,
        classList:{add(){}}, append(...items){this.children.push(...items);},
        replaceChildren(){this.children=[];}, insertAdjacentHTML(){}, after(){}, closest(){return null;},
        setAttribute(k,v){this.attributes[k]=v;}, getAttribute(k){return this.attributes[k];},
        addEventListener(k,h){this.handlers[k]=h;}, getBoundingClientRect(){return {width:64,left:10,top:10,bottom:42};},
        matches(){return this.open;},showPopover(){this.open=true;},hidePopover(){this.open=false;},
        contains(target){return this===target || this.children.includes(target);},
        querySelector(){return this.children[0] || null;},focus(){},
    });
    const select=element();select.id='sel-speed';select.value='1';select.options=[{value:'1',textContent:'1×',selected:true}];
    select.selectedOptions=select.options;
    const doc={querySelectorAll:()=>[select],createElement:()=>{const n=element();created.push(n);return n;},
        body:element(),addEventListener:(event,handler)=>{events[event]=handler;}};
    const previousWindow=globalThis.window, previousObserver=globalThis.MutationObserver;
    globalThis.window={innerWidth:1000,innerHeight:700,addEventListener(){}};
    globalThis.MutationObserver=class{observe(){}};
    try {
        const sync=bindEditorSelects(doc), [trigger,,menu]=created;
        trigger.handlers.click();assert.equal(menu.open,true);
        sync();assert.equal(menu.open,true,'value synchronization does not rebuild an open menu');
        events.scroll({target:{id:'timeline-scroll'}});assert.equal(menu.open,true);
        events.scroll({target:menu});assert.equal(menu.open,true);
        events.scroll({target:{id:'preview-stage'}});assert.equal(menu.open,false);
    } finally {
        globalThis.window=previousWindow;globalThis.MutationObserver=previousObserver;
    }
});
