import test from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
import {readFileSync} from 'node:fs';
import {zh} from '../i18n/zh.js';
import {createRecordingSession} from './recording-session.js';
import {bindRecordingHoverMenu} from './recording-hover-menu.js';

function fixture() {
    const nodes=new Map(), observers=[], frames=new Map(), timers=new Map(), focusRequests=[];let frameId=0,timerId=0;
    function node(id='', classes='') {
        const attrs=new Map(), names=new Set(classes.split(' ').filter(Boolean));
        const n={id,children:[],events:{},style:{},dataset:{},value:'',disabled:false,naturalHeight:100,
            classList:{add:k=>names.add(k),remove:k=>names.delete(k),contains:k=>names.has(k),toggle(k,on){if(on)names.add(k);else names.delete(k);}},
            set className(v){names.clear();for(const c of v.split(' '))names.add(c);},
            setAttribute(k,v){attrs.set(k,String(v));},getAttribute:k=>attrs.get(k),
            addEventListener(k,fn){this.events[k]=fn;},dispatchEvent(event){this.events[event.type]?.(event);return true;},
            append(...items){for(const item of items){item.parent=this;this.children.push(item);if(item.id)nodes.set(item.id,item);}},
            replaceChildren(){this.children=[];},contains(target){return target===this || this.children.some(c=>c.contains(target));},
            closest(selector){return selector==='.recording-split'?(names.has('recording-split')?this:this.parent?.closest(selector)):null;},
            querySelector(selector){return this.children.find(c=>selector==='.recording-select-label'?c.classList.contains('recording-select-label'):c.getAttribute('aria-selected')==='true') || null;},
            get offsetWidth(){return Math.min(200,parseFloat(this.style.maxWidth)||Infinity);},
            get offsetHeight(){return Math.min(this.naturalHeight,parseFloat(this.style.maxHeight)||Infinity);},
            getBoundingClientRect(){return this.rect || {left:100,top:450,bottom:482,width:86,height:32};},focus(){},
        };
        if(id)nodes.set(id,n);return n;
    }
    const fps=node('fps'), arrow=node('fps-menu'), group=node('fps-group','recording-split');
    fps.dataset.recordingSelect='gif-prep-fps';fps.append(node('fps-label','recording-select-label'));group.append(fps,arrow);
    node('gif-prep-fps').value='15';node('gif-audio-mode').value='silent';
    const sound=node('gif-audio-open'), soundMenu=node('gif-audio-menu');
    node('sound-group','recording-split').append(sound,soundMenu);
    const panel=node('gif-audio-settings','hidden');
    const stop=node('gif-rec-stop'), finishPanel=node('gif-recording-finish-menu','hidden');
    finishPanel.append(node('gif-rec-discard'));
    for(const id of ['gif-audio-device-0','gif-audio-device-1','gif-audio-devices-0','gif-audio-devices-1',
        'gif-audio-field-0','gif-audio-field-1','gif-audio-label','gif-audio-device-note'])node(id);
    const modes=['silent','microphone','system','both'].map(mode=>{const n=node();n.dataset.audioMode=mode;return n;});
    const state=createRecordingSession();state.reset('ready');
    const context=vm.createContext({state,ss:{toolbar:node('toolbar'),gifRecHooks:{onPanelFocus:v=>focusRequests.push(v)}},Event,console,
        bindRecordingHoverMenu:options=>bindRecordingHoverMenu({...options,
            setTimer:fn=>{timers.set(++timerId,fn);return timerId;},clearTimer:id=>timers.delete(id)}),
        document:{body:node(),documentElement:node(),getElementById:id=>nodes.get(id),createElement:()=>node(),addEventListener(){},
            querySelector:selector=>selector.includes('data-recording-menu')?arrow:null,
            querySelectorAll:selector=>selector==='[data-recording-select]'?[fps]:selector==='[data-audio-mode]'?modes:[]},
        window:{addEventListener(){}},positionToolbar(){},applyFloatingUiScale:()=>1,
        findDisplayCssAt:()=>({x:0,y:0,w:800,h:600}),
        requestAnimationFrame(fn){frames.set(++frameId,fn);return frameId;},
        ResizeObserver:class{constructor(fn){this.fn=fn;this.observed=new Set();observers.push(this);}observe(n){this.observed.add(n);}unobserve(n){this.observed.delete(n);}},
        t:(key,params={})=>Object.entries(params).reduce((s,[k,v])=>s.replaceAll('{'+k+'}',v),zh[key]||key),onLangChange(){},
        invoke:async command=>command==='gif_audio_devices'?[]:null,normalizeError:e=>e,
        setInterval:()=>1,clearInterval(){},
    });
    const read=name=>readFileSync(new URL(name,import.meta.url),'utf8').replace(/^import .*;\r?$/gm,'').replace(/export /g,'');
    vm.runInContext(`(()=>{${read('./ss-recording-ui.js')}\nglobalThis.ui={bindRecordingControls,renderRecordingUi,openRecordingPanel,closeRecordingPanel,recordingUiChanged,reflowRecordingPanels,syncRecordingSelect,showRecordingMessage,nextRecordingValue,bindRecordingWheel};})()`,context);
    Object.assign(context,context.ui);
    vm.runInContext(`(()=>{${read('./ss-gif-audio.js')}\nglobalThis.bindAudio=bindAudioControls;})()`,context);
    context.ui.bindRecordingControls();context.bindAudio();
    const layout=()=>{const batch=[...frames.values()];frames.clear();for(const fn of batch)fn();};
    const fireTimers=()=>{const batch=[...timers.values()];timers.clear();for(const fn of batch)fn();};
    return {state,nodes,fps,arrow,sound,soundMenu,panel,modes,ui:context.ui,layout,observers,stop,finishPanel,focusRequests,fireTimers};
}

test('main buttons cycle values; separate arrows only open menus; live recording protects source and fps',()=>{
    const f=fixture(), fpsPanel=f.nodes.get('gif-prep-fps-options');
    for(const expected of ['24','30','60','10','15']) {
        f.fps.events.click();assert.equal(f.nodes.get('gif-prep-fps').value,expected);
        assert.equal(fpsPanel.classList.contains('hidden'),true);
    }
    f.arrow.events.click();f.layout();
    assert.equal(fpsPanel.classList.contains('hidden'),false);assert.equal(f.nodes.get('gif-prep-fps').value,'15');
    for(const expected of ['microphone','system','both','silent']) {
        f.sound.events.click();assert.equal(f.nodes.get('gif-audio-mode').value,expected);
        assert.equal(f.panel.classList.contains('hidden'),true);
    }
    f.soundMenu.events.click();f.layout();
    assert.equal(f.panel.classList.contains('hidden'),false);assert.equal(f.nodes.get('gif-audio-mode').value,'silent');
    f.state.sessionId='one';f.state.setMode('recording');f.ui.renderRecordingUi();
    assert.equal(f.sound.disabled,true);assert.equal(f.soundMenu.disabled,false);
    f.sound.events.click();f.fps.events.click();
    assert.equal(f.nodes.get('gif-audio-mode').value,'silent');assert.equal(f.nodes.get('gif-prep-fps').disabled,true);
});

test('an opened audio panel reanchors when sources grow or shrink and device discovery changes height',()=>{
    const f=fixture();f.soundMenu.events.click();f.layout();
    assert.equal(f.panel.style.top,'488px');
    f.panel.naturalHeight=400;f.modes[3].events.click();f.layout();
    assert.equal(f.panel.style.top,'44px','larger source content flips above while retaining its anchor');
    const observer=f.observers.find(o=>o.observed.has(f.panel));
    f.panel.naturalHeight=700;observer.fn();f.layout();
    assert.equal(f.panel.style.maxHeight,'436px');assert.equal(f.panel.style.top,'8px');
    assert.equal(parseFloat(f.panel.style.top)+f.panel.offsetHeight,444,'constrained content scrolls without covering the trigger');
    f.panel.naturalHeight=60;observer.fn();f.layout();
    assert.equal(f.panel.style.top,'488px','shrinking content returns next to the trigger instead of floating at the old top');
    f.ui.closeRecordingPanel();assert.equal(observer.observed.has(f.panel),false);
    observer.fn();f.layout();assert.equal(f.panel.classList.contains('hidden'),true);
});

test('wheel follows screenshot toolbar direction and clamps at ends without opening a menu',()=>{
    const f=fixture();
    const wheel=(control,deltaY)=>{
        const event={deltaY,preventDefault(){this.prevented=true;},stopPropagation(){this.stopped=true;}};
        control.parent.events.wheel(event);return event;
    };
    assert.equal(wheel(f.fps,100).prevented,true);assert.equal(f.nodes.get('gif-prep-fps').value,'24');
    wheel(f.fps,-100);assert.equal(f.nodes.get('gif-prep-fps').value,'15');
    wheel(f.fps,-100);wheel(f.fps,-100);assert.equal(f.nodes.get('gif-prep-fps').value,'10');
    wheel(f.sound,-100);assert.equal(f.nodes.get('gif-audio-mode').value,'silent');
    wheel(f.sound,100);assert.equal(f.nodes.get('gif-audio-mode').value,'microphone');
    assert.equal(f.panel.classList.contains('hidden'),true);
    f.state.setMode('starting');f.ui.renderRecordingUi();
    assert.equal(wheel(f.sound,100).prevented,undefined);assert.equal(f.nodes.get('gif-audio-mode').value,'microphone');
});

test('hovering finish never requests native focus and a closed finish menu cannot close another panel',()=>{
    const f=fixture();f.state.sessionId='one';f.state.setMode('recording');f.ui.renderRecordingUi();
    f.stop.events.pointerenter();f.fireTimers();f.layout();
    assert.equal(f.finishPanel.classList.contains('hidden'),false);
    assert.deepEqual(f.focusRequests,[],'悬停不能抢走被录应用焦点');
    f.stop.events.focusin();assert.deepEqual(f.focusRequests,[true],'键盘进入时允许菜单获得焦点');
    f.stop.focus=()=>f.stop.events.focusin();
    f.ui.closeRecordingPanel({focus:true});
    assert.equal(f.finishPanel.classList.contains('hidden'),true,'Esc 还焦点时不能立即重开');
    assert.deepEqual(f.focusRequests,[true,false]);
    f.soundMenu.events.click();f.layout();
    f.finishPanel.events.click();f.fireTimers();
    assert.equal(f.panel.classList.contains('hidden'),false,'旧撤销菜单的迟到事件不能关闭新面板');
});
