import {createPreparationLease} from './recording-preparation.js';
import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
import {createRecordingSession} from './recording-session.js';

test('preparation drag preserves the real screenshot presentation cleanup contract in every direction',async()=>{
    const makeNode=()=>{const classes=new Set();return {style:{left:'500px',top:'400px'},classList:{add:k=>classes.add(k),contains:k=>classes.has(k)},removeAttribute(){}};};
    const toolbar=makeNode(),canvas=makeNode(),hint=makeNode();
    const ss={toolbar,annotCanvas:canvas,sizeHint:hint,selCss:{x:300,y:200,w:640,h:300},selectionRevision:0,translationRevision:0};
    const state=createRecordingSession();state.reset('ready');
    let committed=0;
    const context=vm.createContext({createPreparationLease,ss,state,
        window:{innerWidth:1920,innerHeight:1080,__blinkScreenMeta:{}},document:{getElementById:()=>null},
        annot:{clearOverlay(){}},resetQrPanel(){},updateOutputButtonsDisabled(){},exitReadingMode(){},updateOverlayButtonsActive(){},
        closeRecordingPanel(){},cssPointToScreen:(x,y)=>({x,y}),cssRectToBitmap:r=>r,drawFinalSelection(){committed++;},updateLiveSelection(){}});
    const index=readFileSync(new URL('./index.js',import.meta.url),'utf8');
    const a=index.indexOf('function invalidateSelectionContent('),b=index.indexOf('// ═',a);
    vm.runInContext(index.slice(a,b),context);
    ss._invalidateSelectionContent=context.invalidateSelectionContent;
    const rec=readFileSync(new URL('./ss-gif-rec.js',import.meta.url),'utf8').replace(/^import .*;\r?$/gm,'').replace(/export /g,'');
    vm.runInContext(rec,context);
    const drag=context.beginRecordingGripDrag();
    assert.equal(toolbar.classList.contains('hidden'),false);
    assert.equal(toolbar.style.left,'500px');
    assert.equal(toolbar.style.top,'400px');
    for(const [dx,dy] of [[-100,-100],[120,-50],[-50,80],[80,90]]) {
        const delta=drag.move(dx,dy);
        assert.equal(delta.dx,dx);assert.equal(delta.dy,dy);
        assert.equal(ss.selCss.x,300+dx);assert.equal(ss.selCss.y,200+dy);
        assert.equal(toolbar.classList.contains('hidden'),false);
    }
    await drag.end();
    assert.equal(committed,1);
    assert.equal(ss.selectionRevision,1);
    assert.equal(toolbar.classList.contains('hidden'),false);
    assert.equal(canvas.classList.contains('hidden'),false);
    assert.equal(toolbar.style.left,'500px');
    assert.equal(toolbar.style.top,'400px');
});
