import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
import {ACTION_IDS, normalizeLayout, fitLayout} from '../shared/screenshot-layout.js';
import {ss as screenshotState} from './ss-state.js';
import {applyFloatingUiScale, findDisplayCssAt, invalidateDisplaysCache, positionResultPanel, getActionAnchorRect} from './ss-display.js';
import {registerScreenshotAction, runScreenshotAction} from './ss-actions.js';
import {uiScaleAtCss} from './ss-selection-geometry.js';
import {placeActionMenu} from './ss-panel-resize.js';

class Element {
    constructor(id='') {
        this.id=id;this.children=[];this.hidden=false;this.disabled=false;this.dataset={};this.style={};
        this.listeners = {};
        const names=new Set();this.classList={add:name=>names.add(name),remove:name=>names.delete(name),contains:name=>names.has(name)};
    }
    detach(node) { if(node.parent)node.parent.children=node.parent.children.filter(item=>item!==node);node.parent=this; }
    append(node) { this.detach(node);this.children.push(node); }
    insertBefore(node,anchor) { if(node===anchor)return;this.detach(node);const index=this.children.indexOf(anchor);this.children.splice(index<0?this.children.length:index,0,node); }
    get firstChild() { return this.children[0]; }
    setAttribute(key,value) { this[key]=value; }
    getAttribute(key) { return this[key]??null; }
    removeAttribute(key) { delete this[key]; }
    addEventListener(name, run) { (this.listeners[name] ??= []).push(run); }
    querySelector(selector) { return selector.startsWith('button') ? this.children.find(node => !node.disabled && !node.hidden) : this.children.find(node=>node.className==='ss-action-label'); }
    querySelectorAll() { return this.children; }
    focus() { document.activeElement = this; }
    getBoundingClientRect() { return {left: 600, right: 632, top: 300, bottom: 332}; }
}
let harnessId = 0;
async function harness() {
    const nodes=Object.fromEntries([...ACTION_IDS.map(id=>`btn-${id}`),'ss-more-divider','btn-more','btn-copy','btn-cancel','btn-ocr-diag','ss-action-menu','ss-customize'].map(id=>[id,new Element(id)]));
    nodes['btn-ocr-diag'].hidden=true;nodes['ss-action-menu'].hidden=true;
    const group=new Element('output'),toolbar=new Element('toolbar');
    for(const id of ['btn-ocr','btn-ocr-diag',...ACTION_IDS.filter(id=>id!=='ocr').map(id=>`btn-${id}`),'ss-more-divider','btn-more','btn-copy','btn-cancel'])group.append(nodes[id]);
    nodes['ss-action-menu'].append(nodes['ss-customize']);
    Object.defineProperty(toolbar,'offsetWidth',{get:()=>120+group.children.filter(node=>!node.hidden&&!node.classList.contains('hidden')).reduce((sum,node)=>sum+(node.id==='ss-more-divider'?6:32),0)});
    const ss={toolbar,editorSession:{source:'screenshot'},isAnnotating:true};
    screenshotState.toolbar = toolbar;
    const frames = [], observers = [], events = {};
    globalThis.window = {addEventListener: (name, run) => {events[name] = run;}};
    globalThis.MutationObserver = class { constructor(run) { observers.push(run); } observe() {} };
    globalThis.requestAnimationFrame = run => frames.push(run);
    globalThis.document={getElementById:id=>nodes[id],querySelector:()=>group,createElement:()=>new Element(),addEventListener(){}};
    globalThis.menuHarness={ss,ACTION_IDS,normalizeLayout,fitLayout,applyFloatingUiScale,findDisplayCssAt,placeActionMenu};
    const source=(await readFile(new URL('./ss-action-menu.js',import.meta.url),'utf8')).replace(/^import[^\n]*\n/gm,'');
    const prefix=`const {ss,ACTION_IDS,normalizeLayout,fitLayout,applyFloatingUiScale,findDisplayCssAt,placeActionMenu}=globalThis.menuHarness;
        const t=key=>key, applyI18nFromConfig=()=>Promise.resolve();
        const listen=()=>Promise.resolve(),EVENTS={},openLayoutEditor=()=>{},closeLayoutEditor=()=>{};`;
    const module=await import('data:text/javascript;base64,'+Buffer.from(prefix+source).toString('base64')+`#${++harnessId}`);
    module.bindActionMenu();
    screenshotState._closeActionMenu = module.closeActionMenu;
    return {nodes, group, toolbar, module, frames, observers, events};
}
test('actual menu layout keeps canonical nodes, default order and fixed completion actions across overflow',async()=>{
    const {nodes,group,module}=await harness();
    const ids=element=>element.children.map(node=>node.id);
    assert.deepEqual(ids(group),['btn-ocr-diag','btn-translate','btn-scroll','btn-gif','btn-save','btn-ask-ai','btn-pin','ss-more-divider','btn-more','btn-copy','btn-cancel']);
    assert.deepEqual(ids(nodes['ss-action-menu']),['btn-ocr','btn-qr','btn-translate-pin','ss-customize']);
    module.refreshActionLayout(288);
    assert.deepEqual(ids(group).slice(-6),['btn-translate','btn-scroll','ss-more-divider','btn-more','btn-copy','btn-cancel']);
    assert.deepEqual(ids(nodes['ss-action-menu']),['btn-gif','btn-save','btn-ask-ai','btn-pin','btn-ocr','btn-qr','btn-translate-pin','ss-customize']);
    assert.equal(nodes['btn-ocr'],nodes['ss-action-menu'].children[4]);
    module.refreshActionLayout(1000);
    assert.equal(group.children.length,11);
    assert.deepEqual(ids(group).slice(-4),['ss-more-divider','btn-more','btn-copy','btn-cancel']);
    assert.equal(nodes['ss-more-divider'].hidden,false);
    assert(module.refreshActionLayout(150)<1,'extremely narrow screens use a bounded visual fallback');
    assert.equal(nodes['ss-more-divider'].hidden,true);
    delete globalThis.menuHarness;delete globalThis.document;
});

test('letters including E leave menu focus unchanged; navigation and action dispatch remain available', async () => {
    const {nodes} = await harness();
    window.innerWidth = 1600; window.innerHeight = 1000;
    delete window.__blinkScreenMeta; invalidateDisplaysCache();
    Object.assign(nodes['ss-action-menu'], {offsetWidth: 200, offsetHeight: 160});
    let executed = 0;
    registerScreenshotAction('ocr', () => {
        assert.equal(nodes['ss-action-menu'].hidden, true, 'close before executing the action');
        executed++;
    });
    const press = (key, extra = {}) => {
        const event = {key, target: document.activeElement, preventDefault() { this.defaultPrevented = true; }, stopPropagation() {}, ...extra};
        nodes['ss-action-menu'].listeners.keydown[0](event);
        return event;
    };
    nodes['btn-more'].onclick({stopPropagation() {}});
    nodes['btn-qr'].focus();
    press('x'); assert.equal(document.activeElement, nodes['btn-qr']);
    assert.equal(press('e').defaultPrevented, undefined);
    assert.equal(press('E').defaultPrevented, undefined);
    assert.equal(document.activeElement, nodes['btn-qr']);
    press('ArrowDown'); assert.equal(document.activeElement, nodes['btn-translate-pin']);
    assert.equal(press('Enter').defaultPrevented, undefined, 'native button activation stays available');
    press('e', {ctrlKey: true}); press('e', {isComposing: true});
    press('e', {target: {tagName: 'INPUT'}}); press('e', {repeat: true});
    assert.equal(executed, 0);
    assert.equal(nodes['ss-action-menu'].hidden, false);
    assert.equal(runScreenshotAction('ocr'), true);
    assert.equal(executed, 1);
    nodes['btn-more'].onclick({stopPropagation() {}});
    nodes['btn-ocr'].disabled = true;
    assert.equal(runScreenshotAction('ocr'), false);
    assert.equal(executed, 1);
    assert.equal(nodes['ss-action-menu'].hidden, false);
    press('Escape'); assert.equal(document.activeElement, nodes['btn-more']);
});

test('result anchors follow actual layout and toolbar monitor, including hidden overflow buttons', async () => {
    const {nodes, toolbar, module, frames, observers, events} = await harness();
    window.innerWidth = 2400; window.innerHeight = 900;
    window.__blinkScreenMeta = {vx: -1200, vy: 0, renderScaleX: 1, renderScaleY: 1,
        physicalDisplays: [{x: -1200, y: 0, w: 1200, h: 900, dpi: 96}, {x: 0, y: 0, w: 1200, h: 900, dpi: 144}]};
    invalidateDisplaysCache(); toolbar.dataset.uiScale = '1.5';
    const more = {left: 1800, right: 1848, top: 300, bottom: 348};
    nodes['btn-more'].getBoundingClientRect = () => more;
    nodes['btn-ocr'].getBoundingClientRect = () => ({left: 0, right: 0, top: 0, bottom: 0});
    assert.deepEqual(getActionAnchorRect('ocr'), more);
    screenshotState.selCss = {x: 50, y: 80, w: 200, h: 100}; // Selection belongs to another monitor.
    const panel = new Element('ocr-panel'); panel.offsetWidth = 300; panel.offsetHeight = 250;
    panel._reposition = () => positionResultPanel(panel, 'ocr');
    nodes['ocr-panel'] = panel;
    panel._reposition();
    assert.equal(panel.style.left, '1800px'); assert.equal(panel.style.top, '352px');
    nodes['btn-ocr'].getBoundingClientRect = () => ({left: 1450, right: 1498, top: 300, bottom: 348});
    module.applyActionLayout({toolbar: ['ocr'], more: ACTION_IDS.filter(id => id !== 'ocr')});
    assert.equal(frames.length, 1); frames.shift()();
    assert.equal(panel.style.left, '1450px', 'open result follows saved layout immediately');
    module.applyActionLayout(); frames.shift()();
    assert.equal(panel.style.left, '1800px', 'moving back to more restores visible anchor');
    panel.dataset.userMoved = 'true';
    panel.getBoundingClientRect = () => ({left: 1300, right: 1750, top: 100, bottom: 475});
    observers[0](); events.resize(); frames.shift()();
    assert.equal(panel.style.left, '1300px'); assert.equal(panel.style.top, '100px');
});

test('layout editor may overlap selection while result panels still avoid it', async () => {
    const {nodes, module} = await harness();
    window.innerWidth = 1600; window.innerHeight = 1000;
    delete window.__blinkScreenMeta; invalidateDisplaysCache();
    screenshotState.toolbar.dataset.uiScale = '1';
    const anchor = {left: 600, right: 632, top: 300, bottom: 332};
    nodes['btn-more'].getBoundingClientRect = () => anchor;
    screenshotState.selCss = {x: 0, y: 336, w: 1500, h: 200};
    const editor = new Element('screenshot-layout-editor'); editor.offsetWidth = 380; editor.offsetHeight = 250;
    module.positionActionPanel(editor);
    assert.equal(editor.style.top, '336px', 'customization stays below its trigger despite selection');
    const result = new Element('qr-panel'); result.offsetWidth = 300; result.offsetHeight = 250;
    positionResultPanel(result, 'qr');
    assert.equal(result.style.top, '46px', 'QR result keeps selection avoidance');
});

test('action panels use toolbar compensation and visual monitor bounds across mixed DPI and small screens', async () => {
    const {nodes, toolbar, module} = await harness();
    for (const renderScale of [1, 1.25, 1.5, 2]) for (const dpi of [96, 120, 144, 192]) for (const width of [320, 800]) {
        window.devicePixelRatio = 3; // Deliberately differs from both render scale and monitor DPI.
        window.__blinkScreenMeta = {vx: -1920, vy: -200, renderScaleX: renderScale, renderScaleY: renderScale,
            physicalDisplays: [{x: -1920, y: -200, w: 1920, h: 1080, dpi: 96}, {x: 0, y: 0, w: width, h: 600, dpi}]};
        invalidateDisplaysCache();
        const anchor = {left: (1920 + width - 120) / renderScale, right: (1920 + width - 88) / renderScale, top: 720 / renderScale, bottom: 752 / renderScale};
        nodes['btn-more'].getBoundingClientRect = () => anchor;
        const scale = uiScaleAtCss(anchor.left, anchor.top, window.__blinkScreenMeta);
        toolbar.dataset.uiScale = String(scale);
        for (const panel of [nodes['ss-action-menu'], new Element('screenshot-layout-editor'), new Element('ocr-panel'), new Element('qr-panel')]) {
            Object.defineProperty(panel, 'offsetWidth', {configurable: true, get: () => Math.min(380, parseFloat(panel.style.maxWidth))});
            Object.defineProperty(panel, 'offsetHeight', {configurable: true, get: () => Math.min(440, parseFloat(panel.style.maxHeight))});
            if (['ocr-panel', 'qr-panel'].includes(panel.id)) positionResultPanel(panel, panel.id === 'ocr-panel' ? 'ocr' : 'qr');
            else module.positionActionPanel(panel);
            assert.equal(panel.style.transform, `scale(${dpi / 96 / renderScale})`);
            const mon = findDisplayCssAt(anchor.left, anchor.top);
            const left = parseFloat(panel.style.left), top = parseFloat(panel.style.top);
            assert(left >= mon.x + 8 && top >= mon.y + 8);
            assert(left + panel.offsetWidth * scale <= mon.x + mon.w - 8 + 0.001);
            assert(top + panel.offsetHeight * scale <= mon.y + mon.h - 8 + 0.001);
        }
    }
});

test('open layout editor follows toolbar changes and resize without resurrecting a closed panel', async () => {
    const {nodes, frames, observers, events} = await harness();
    let repositioned = 0;
    nodes['screenshot-layout-editor'] = {_reposition: () => repositioned++};
    observers[0](); observers[0](); events.resize();
    assert.equal(frames.length, 1, 'coalesce geometry changes until final toolbar transform');
    frames.shift()(); assert.equal(repositioned, 1);
    events.resize(); delete nodes['screenshot-layout-editor'];
    frames.shift()(); assert.equal(repositioned, 1);
    observers[0](); assert.equal(frames.length, 0);
});
