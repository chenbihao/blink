import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
import {createRecordingSession} from './recording-session.js';
globalThis.window ||= {};
const {computeInteractionRect}=await import('./ss-interaction.js');

function fixture() {
    const state=createRecordingSession();state.reset('ready');
    const ss={selCss:{x:100,y:100,w:300,h:200},canvas:{width:1600,height:1200}};
    const changes=[],interactions=[],captures=new Set();let invalidations=0;
    const frame={events:{},addEventListener(k,fn){this.events[k]=fn;},setPointerCapture:id=>captures.add(id),releasePointerCapture:id=>captures.delete(id)};
    const context=vm.createContext({window:{__blinkScreenMeta:{renderScaleX:2,renderScaleY:2}},computeInteractionRect,
        findDisplayCssAt:()=>({x:0,y:0,w:800,h:600})});
    const source=readFileSync(new URL('./recording-selection.js',import.meta.url),'utf8').replace(/^import .*;\r?$/gm,'').replace(/export /g,'');
    vm.runInContext(source,context);
    const controller=context.bindRecordingSelection({frame,state,ss,
        onInvalidate:()=>invalidations++,onInteraction:(active,epoch)=>interactions.push({active,epoch}),
        onChange:(rect,commit)=>{ss.selCss={...rect};changes.push({rect:{...rect},commit});}});
    const event=(handle,x,y,id=1)=>({button:0,pointerId:id,clientX:x,clientY:y,
        target:{closest:selector=>selector==='[data-recording-resize]'?(handle?{dataset:{recordingResize:handle}}:null):handle?null:{}},
        preventDefault(){},stopPropagation(){}});
    return {frame,state,ss,changes,interactions,captures,controller,event,get invalidations(){return invalidations;}};
}

test('preparation resizing uses screenshot geometry and commits release coordinates without moving at pointerdown',()=>{
    const f=fixture();
    f.frame.events.pointerdown(f.event('se',402,302));
    assert.equal(f.captures.has(1),true);assert.equal(f.invalidations,0);
    f.frame.events.pointermove(f.event('se',452,332));
    assert.deepEqual(f.ss.selCss,{x:100,y:100,w:350,h:230});
    f.frame.events.pointerup(f.event('se',462,342));
    assert.deepEqual(f.ss.selCss,{x:100,y:100,w:360,h:240});
    assert.equal(f.changes.at(-1).commit,true);assert.equal(f.invalidations,1);
    assert.equal(f.captures.size,0);assert.deepEqual(f.interactions.map(i=>i.active),[true,false]);
});

test('all eight handles and moving the border remain inside the source monitor; cancelled drags restore geometry',()=>{
    for(const handle of ['nw','n','ne','e','se','s','sw','w']) {
        const f=fixture();
        const x=handle.includes('w')?100:handle.includes('e')?400:250;
        const y=handle.includes('n')?100:handle.includes('s')?300:200;
        f.frame.events.pointerdown(f.event(handle,x,y));
        f.frame.events.pointerup(f.event(handle,handle.includes('w')?-100:1000,handle.includes('n')?-100:1000));
        const r=f.ss.selCss;assert.ok(r.x>=0 && r.y>=0 && r.x+r.w<=800 && r.y+r.h<=600);
    }
    const f=fixture();
    f.frame.events.pointerdown(f.event(null,200,100));
    f.frame.events.pointermove(f.event(null,300,150));
    assert.deepEqual(f.ss.selCss,{x:200,y:150,w:300,h:200});
    f.frame.events.pointercancel(f.event(null,300,150));
    assert.deepEqual(f.ss.selCss,{x:100,y:100,w:300,h:200});
});

test('starting, recording and paused states lock size, and a late preparation drag cannot alter the recording',()=>{
    for(const mode of ['starting','recording','paused']) {
        const f=fixture();f.state.setMode(mode);
        if(mode!=='starting')f.state.sessionId='one';
        f.frame.events.pointerdown(f.event('se',400,300));
        f.frame.events.pointerup(f.event('se',600,500));assert.equal(f.changes.length,0);
    }
    const f=fixture();f.frame.events.pointerdown(f.event('se',400,300));
    f.state.setMode('starting');f.frame.events.pointerup(f.event('se',600,500));
    assert.equal(f.changes.length,0);assert.equal(f.captures.size,0);
});

test('starting commits the latest drag before locking the size and releases pointer capture',()=>{
    const f=fixture();f.frame.events.pointerdown(f.event('se',400,300));
    f.frame.events.pointermove(f.event('se',450,350));
    f.controller.commit();
    assert.deepEqual(f.ss.selCss,{x:100,y:100,w:350,h:250});
    assert.equal(f.changes.at(-1).commit,true);assert.equal(f.captures.size,0);
    f.state.setMode('starting');f.frame.events.pointermove(f.event('se',700,550));
    assert.deepEqual(f.ss.selCss,{x:100,y:100,w:350,h:250});
});
