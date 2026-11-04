import test from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
import {readFileSync} from 'node:fs';
import {createRecordingSession} from './recording-session.js';
globalThis.window = globalThis.window || {};
const {meterSegments} = await import('./ss-gif-audio.js');
test('meters use silence plus six green, two amber and two red threshold levels', () => {
    const amplitude = db => 10 ** (db / 20);
    assert.equal(meterSegments(0), 0);
    assert.equal(meterSegments(NaN), 0);
    assert.equal(meterSegments(amplitude(-70)), 0);
    assert.equal(meterSegments(amplitude(-12)), 6);
    assert.equal(meterSegments(amplitude(-6)), 8);
    assert.equal(meterSegments(amplitude(-2)), 9);
    assert.equal(meterSegments(1), 10);
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
