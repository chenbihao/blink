import test from 'node:test';
import assert from 'node:assert/strict';
import {bindRecordingHoverMenu} from './recording-hover-menu.js';

function fixture() {
    let now=0,id=0,enabled=true,opened=false,focused=0;
    const timers=new Map(),requests=[];
    const item={focus(){focused++;}};
    const node=()=>({events:{},contains(target){return target===this;},addEventListener(k,fn){this.events[k]=fn;}});
    const trigger=node(),panel=node();panel.contains=target=>target===panel || target===item;panel.querySelector=()=>item;
    const menu=bindRecordingHoverMenu({trigger,panel,enabled:()=>enabled,isOpen:()=>opened,
        open:keyboard=>{opened=true;requests.push(keyboard);},close:()=>{opened=false;},
        setTimer:(fn,ms)=>{timers.set(++id,{fn,at:now+ms});return id;},clearTimer:id=>timers.delete(id)});
    const advance=ms=>{
        const end=now+ms;
        for(;;) {
            const next=[...timers.entries()].sort((a,b)=>a[1].at-b[1].at)[0];
            if(!next || next[1].at>end)break;
            now=next[1].at;timers.delete(next[0]);next[1].fn();
        }
        now=end;
    };
    return {trigger,panel,item,menu,requests,advance,disable:()=>{enabled=false;menu.sync();},get opened(){return opened;},get focused(){return focused;}};
}

test('hover intent waits, crossing the gap keeps the menu, and leaving both closes after a grace period',()=>{
    const f=fixture();f.trigger.events.pointerenter();f.advance(179);assert.equal(f.opened,false);
    f.advance(1);assert.equal(f.opened,true);assert.deepEqual(f.requests,[false]);
    f.trigger.events.pointerleave({relatedTarget:null});f.advance(100);
    f.panel.events.pointerenter();f.advance(500);assert.equal(f.opened,true);
    f.panel.events.pointerleave({relatedTarget:null});f.advance(219);assert.equal(f.opened,true);
    f.advance(1);assert.equal(f.opened,false);
});

test('quick flyover and recording teardown cancel pending hover actions',()=>{
    const f=fixture();f.trigger.events.pointerenter();f.advance(100);
    f.trigger.events.pointerleave({relatedTarget:null});f.advance(500);assert.deepEqual(f.requests,[]);
    f.trigger.events.pointerenter();f.disable();f.advance(500);assert.equal(f.opened,false);assert.deepEqual(f.requests,[]);
});

test('keyboard opens immediately and restoring focus after Escape does not reopen the menu',async()=>{
    const f=fixture();const event={key:'ArrowDown',preventDefault(){this.prevented=true;}};
    f.trigger.events.keydown(event);assert.equal(f.opened,true);assert.equal(event.prevented,true);assert.equal(f.focused,1);
    f.menu.dismiss(true);f.panel.events.click();
    const before=f.requests.length;f.trigger.events.focusin();assert.equal(f.requests.length,before);
    await Promise.resolve();f.trigger.events.focusin();assert.equal(f.requests.at(-1),true);
});
