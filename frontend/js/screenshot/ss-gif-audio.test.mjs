import test from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
import {readFileSync} from 'node:fs';
import {createRecordingSession} from './recording-session.js';
globalThis.window = globalThis.window || {};
const {meterLevel} = await import('./ss-gif-audio.js');
test('continuous meters map -60 to 0 dB without quantization and clamp silence', () => {
    const amplitude = db => 10 ** (db / 20);
    assert.equal(meterLevel(0), 0);
    assert.equal(meterLevel(NaN), 0);
    assert.equal(meterLevel(amplitude(-70)), 0);
    assert.equal(meterLevel(amplitude(-12)), 0.8);
    assert.equal(meterLevel(amplitude(-6)), 0.9);
    assert.ok(meterLevel(amplitude(-11)) > meterLevel(amplitude(-12)));
    assert.equal(meterLevel(1), 1);
    assert.equal(meterLevel(2), 1);
});

test('monitor polls at 150ms without overlap and drives both continuous meter locations',async()=>{
    const makeRow=()=>({style:{values:{},setProperty(k,v){this.values[k]=v;}},classList:{toggle(){}},setAttribute(){}});
    const toolbar=[makeRow(),makeRow()], panel=[makeRow(),makeRow()], nodes=new Map();
    const container=rows=>({querySelector:selector=>rows[Number(selector.match(/\d/)[0])]});
    nodes.set('gif-audio-meters',container(toolbar));nodes.set('gif-audio-settings',container(panel));
    nodes.set('gif-audio-mode',{value:'both'});
    const state=createRecordingSession();state.reset('ready');
    let tick,interval,intervalStarts=0,reads=0,resolve,now=0,configureResolve,slowConfigure=false;
    const context=vm.createContext({state,ss:{},t:key=>key,performance:{now:()=>now},
        document:{getElementById:id=>nodes.get(id),querySelector:()=>null},
        showRecordingMessage(){},normalizeError:assert.fail,clearInterval(){},
        setInterval(fn,ms){tick=fn;interval=ms;intervalStarts++;return 1;},
        invoke:async command=>command==='gif_audio_monitor_status'?(reads++,new Promise(r=>{resolve=r;})):
            slowConfigure?new Promise(r=>{configureResolve=r;}):null});
    const source=readFileSync(new URL('./ss-gif-audio.js',import.meta.url),'utf8').replace(/^import .*;\r?$/gm,'').replace(/export /g,'');
    vm.runInContext(source,context);
    await context.monitor();assert.equal(interval,150);
    const polling=tick();await tick();assert.equal(reads,1);
    const value={active:true,tracks:[{source:0,name:'mic'},{source:1,name:'system'}],rms:[0.1,0.25],peak:[0.2,0.5]};
    resolve(value);await polling;
    for(let i=0;i<2;i++) {
        assert.equal(toolbar[i].style.values['--audio-level'],meterLevel(value.rms[i]));
        assert.deepEqual(panel[i].style.values,toolbar[i].style.values);
    }
    const peak=toolbar[0].style.values['--audio-peak'];
    now=500;context.renderAudioMeters({...value,peak:[0.01,0.01]});
    assert.equal(toolbar[0].style.values['--audio-peak'],peak,'peak marker holds for one second');
    now=1500;context.renderAudioMeters({...value,peak:[0.01,0.01]});
    assert.ok(toolbar[0].style.values['--audio-peak']<peak);
    context.renderAudioMeters({...value,muted:[true,false]});
    assert.equal(toolbar[0].style.values['--audio-level'],0);
    assert.equal(panel[0].style.values['--audio-peak'],0);
    slowConfigure=true;
    const changing=context.monitor();
    for(let i=0;i<4;i++)await Promise.resolve();
    assert.equal(intervalStarts,1,'changing a device must not replace the polling timer');
    assert.equal(toolbar[1].style.values['--audio-level'],meterLevel(value.rms[1]));
    const whileOpening=tick();
    resolve({...value,error:'mic unavailable',sourceErrors:['mic unavailable',null]});await whileOpening;
    assert.equal(toolbar[0].style.values['--audio-level'],0);
    assert.equal(toolbar[1].style.values['--audio-level'],meterLevel(value.rms[1]),'a per-source error leaves the other meter live');
    configureResolve();await changing;
});

test('system default is a null device id and uses the system marker, never the first listed endpoint',async()=>{
    const nodes=new Map();
    for(let i=0;i<2;i++) {
        nodes.set('gif-audio-device-'+i,{value:'',dispatchEvent(){}});
        nodes.set('gif-audio-devices-'+i,{children:[],replaceChildren(){this.children=[];},append(button){this.children.push(button);}});
    }
    nodes.set('gif-audio-mode',{value:'both'});
    const devices=[{source:0,id:'virtual',name:'Virtual microphone'}, {source:0,id:'usb',name:'USB microphone',isDefault:true},
        {source:1,id:'monitor',name:'Monitor audio'},{source:1,id:'realtek',name:'Realtek(R) Audio',isDefault:true}];
    const state=createRecordingSession();state.reset('ready');
    const context=vm.createContext({state,ss:{},Event,
        t:(_key,{name}={})=>name,
        document:{getElementById:id=>nodes.get(id),createElement:()=>({attributes:{},events:{},setAttribute(k,v){this.attributes[k]=v;},addEventListener(k,fn){this.events[k]=fn;}})},
        invoke:async()=>devices,normalizeError:assert.fail});
    const source=readFileSync(new URL('./ss-gif-audio.js',import.meta.url),'utf8').replace(/^import .*;\r?$/gm,'').replace(/export /g,'');
    vm.runInContext(source,context);
    await context.refreshAudioDevices();
    assert.equal(context.audioOptions().microphone,null);
    assert.equal(context.audioOptions().system,null);
    assert.equal(nodes.get('gif-audio-devices-0').children[0].textContent,'USB microphone');
    assert.equal(nodes.get('gif-audio-devices-1').children[0].textContent,'Realtek(R) Audio');
    for(let i=0;i<2;i++) {
        const options=nodes.get('gif-audio-devices-'+i).children;
        assert.equal(options[0].attributes['aria-pressed'],'true');
        assert.equal(options[1].attributes['aria-pressed'],'false');
    }
    nodes.get('gif-audio-devices-1').children[2].events.click();
    assert.equal(context.audioOptions().system,'realtek','an explicit device choice must retain its stable identity');
});
