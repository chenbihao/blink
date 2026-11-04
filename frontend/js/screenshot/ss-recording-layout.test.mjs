import test from 'node:test';
import assert from 'node:assert/strict';
globalThis.window = {devicePixelRatio:1,innerWidth:685,innerHeight:1119};
globalThis.document = {getElementById:()=>null};
const {ss} = await import('./ss-state.js');
const {positionToolbar,invalidateDisplaysCache} = await import('./ss-display.js');

function toolbar(width,left=37,top=428) {
    const classes = new Set();
    return {offsetWidth:width,offsetHeight:40,style:{left:`${left}px`,top:`${top}px`},dataset:{keepAnchor:'true'},
        classList:{remove:c=>classes.delete(c),contains:c=>classes.has(c),toggle(c,on){if(on)classes.add(c);else classes.delete(c);}}};
}

test('mode changes preserve the left grip; late device discovery still fits high-DPI narrow screens', () => {
    window.__blinkScreenMeta={vx:0,vy:0,renderScaleX:1,renderScaleY:1,
        physicalDisplays:[{x:0,y:0,w:685,h:1119,dpi:144}]};
    invalidateDisplaysCache();
    ss.toolbar=toolbar(300);
    const selection={x:100,y:120,w:500,h:300};
    positionToolbar(selection,true);
    assert.equal(ss.toolbar.style.left,'37px');
    assert.equal(Number(ss.toolbar.dataset.uiScale),1.5);
    // A device response can add two level meters after the first placement.
    ss.toolbar.offsetWidth=500;ss.toolbar.dataset.keepAnchor='true';
    positionToolbar(selection,true);
    const left=parseFloat(ss.toolbar.style.left),width=ss.toolbar.offsetWidth*Number(ss.toolbar.dataset.uiScale);
    assert.ok(left>=8 && left+width<=685-8+0.001);
    assert.equal(ss.toolbar.style.top,'428px');
});

test('dragged recording toolbars use the grip monitor DPI rather than the original capture monitor', () => {
    window.__blinkScreenMeta={vx:0,vy:0,renderScaleX:1,renderScaleY:1,
        physicalDisplays:[{x:0,y:0,w:700,h:1000,dpi:96},{x:700,y:0,w:700,h:1000,dpi:192}]};
    invalidateDisplaysCache();
    ss.toolbar=toolbar(200,800,300);ss.toolbar.dataset.userMoved='true';
    positionToolbar({x:100,y:120,w:400,h:300},true);
    assert.equal(ss.toolbar.style.left,'800px');
    assert.equal(Number(ss.toolbar.dataset.uiScale),2);
});

test('recording tool changes retain the grip even after the one-shot anchor flag is consumed', () => {
    window.__blinkScreenMeta={vx:0,vy:0,renderScaleX:1,renderScaleY:1,
        physicalDisplays:[{x:0,y:0,w:1920,h:1080,dpi:96}]};
    invalidateDisplaysCache();
    ss.toolbar=toolbar(360,60,370);ss.toolbar.dataset.mode='ready';
    delete ss.toolbar.dataset.keepAnchor;
    const selection={x:120,y:80,w:640,h:260};
    positionToolbar(selection,true);
    ss.toolbar.offsetWidth=700;ss.toolbar.dataset.drawing='true';
    positionToolbar(selection,true);
    positionToolbar(selection,true);
    assert.equal(ss.toolbar.style.left,'60px');
    assert.equal(ss.toolbar.style.top,'370px');
});
