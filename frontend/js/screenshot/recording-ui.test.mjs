import test from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
import {readFileSync} from 'node:fs';
import {zh} from '../i18n/zh.js';
import {createRecordingSession} from './recording-session.js';

function node() {
    const classes=new Set(),attrs=new Map();
    return {disabled:true,hidden:false,value:'15',textContent:'',title:'',dataset:{},style:{},
        classList:{contains:key=>classes.has(key),toggle(key,on){if(on)classes.add(key);else classes.delete(key);}},
        setAttribute(key,value){attrs.set(key,value);},getAttribute:key=>attrs.get(key),
        querySelector:()=>null,querySelectorAll:()=>[]};
}
function fixture() {
    const state=createRecordingSession(),nodes=new Map();
    for(const id of ['gif-record-toggle','gif-record-toggle-label','gif-rec-stop','gif-rec-stop-label','gif-record-exit','gif-record-exit-label','gif-rec-draw','gif-audio-open','gif-prep-fps','gif-recording-status','gif-rec-time','gif-rec-storage'])nodes.set(id,node());
    const toolbar=node();toolbar.dataset={mode:'screenshot',drawing:'false'};
    const context=vm.createContext({state,ss:{toolbar},document:{getElementById:id=>nodes.get(id),body:node(),documentElement:node()},
        t:(key,params={})=>Object.entries(params).reduce((s,[k,v])=>s.replaceAll('{'+k+'}',v),zh[key]||key)});
    const source=readFileSync(new URL('./ss-recording-ui.js',import.meta.url),'utf8').replace(/^import .*;\r?$/gm,'').replace(/export /g,'');
    vm.runInContext(`${source}\nglobalThis.render=renderRecordingUi;`,context);
    return {state,nodes,toolbar,render:context.render};
}

test('a second preparation fully re-enables recording, sound and drawing after an interrupted session',()=>{
    const f=fixture();
    f.state.sessionId='old';f.state.setMode('stopping');f.state.restoredControlsOnly=true;f.render();
    assert.equal(f.nodes.get('gif-rec-draw').disabled,true);
    f.state.reset();f.render();f.state.reset('ready');f.render();
    for(const id of ['gif-record-toggle','gif-record-exit','gif-audio-open','gif-rec-draw','gif-prep-fps'])assert.equal(f.nodes.get(id).disabled,false,id);
    assert.equal(f.nodes.get('gif-record-toggle-label').textContent,'开始');
    assert.equal(f.nodes.get('gif-rec-stop').hidden,true);
    assert.equal(f.toolbar.dataset.mode,'ready');
    assert.equal(f.toolbar.dataset.drawing,'false');
});

test('one main action follows start/pause/resume while exit changes to explicit discard',()=>{
    const f=fixture();f.state.reset('ready');f.render();
    assert.equal(f.nodes.get('gif-record-exit').title,'退出录制');
    assert.equal(f.nodes.get('gif-record-exit-label').textContent,'退出');
    f.state.sessionId='one';f.state.setMode('recording');f.render();
    assert.equal(f.nodes.get('gif-record-toggle-label').textContent,'暂停');
    assert.equal(f.nodes.get('gif-rec-stop').hidden,false);
    assert.equal(f.nodes.get('gif-record-exit').title,'撤销当前录制');
    assert.equal(f.nodes.get('gif-record-exit-label').textContent,'撤销');
    assert.equal(f.nodes.get('gif-rec-stop-label').textContent,'结束');
    f.state.setMode('paused');f.render();assert.equal(f.nodes.get('gif-record-toggle-label').textContent,'继续');
    f.state.setMode('recording');f.render();assert.equal(f.nodes.get('gif-record-toggle-label').textContent,'暂停');
});

test('the lower status line reports available space during recording, including unknown disks',()=>{
    const f=fixture();f.state.sessionId='one';f.state.setMode('recording');
    f.state.storage={disk:{volume:'D:',availableBytes:2*1024**3},materialBytes:500*1024**2};f.render();
    assert.equal(f.nodes.get('gif-rec-storage').textContent,'可用 2.0 GB');
    f.state.storage={materialBytes:500*1024**2};f.render();
    assert.equal(f.nodes.get('gif-rec-storage').textContent,'空间未知');
});
