import {test} from 'node:test';
import assert from 'node:assert/strict';
import {DEFAULT_LAYOUT} from './screenshot-layout.js';

// Small DOM seam: verifies save/cancel/lifecycle decisions, not visual layout.
class Element {
    constructor(tag) { this.tagName=tag;this.children=[];this.dataset={};this.style={};this.disabled=false; }
    append(...nodes) { for (const node of nodes) { node.parent=this;this.children.push(node); } }
    replaceChildren(...nodes) { this.children=[];this.append(...nodes); }
    remove() { if(this.parent)this.parent.children=this.parent.children.filter(node=>node!==this); }
    setAttribute() {}
    addEventListener() {}
    focus() { document.activeElement=this; }
    querySelector(selector) {
        const match=selector.match(/data-action="([^"]+)".*data-control="([^"]+)"/);
        return this.all().find(node=>match && node.dataset.action===match[1] && String(node.dataset.control)===match[2]) || null;
    }
    querySelectorAll() { return this.all().filter(node=>node.tagName==='button'&&!node.disabled); }
    all() { return this.children.flatMap(node=>[node,...node.all()]); }
}
globalThis.window={};
globalThis.document={body:new Element('body'),createElement:tag=>new Element(tag),getElementById:id=>document.body.all().find(node=>node.id===id)};
let read=async()=>({toolbarLayout:structuredClone(DEFAULT_LAYOUT)}), write=async()=>{}, writes=[];
window.__TAURI__={core:{invoke:async(cmd,args)=>{
    if(cmd==='get_config_section')return read();
    if(cmd==='set_config'){writes.push(args);return write();}
    throw new Error(cmd);
}}};
const {openLayoutEditor,closeLayoutEditor}=await import('./screenshot-layout-editor.js');
const panel=()=>document.getElementById('screenshot-layout-editor');

test('cancel and restore defaults are local until save; conflict retains the draft and expected value',async()=>{
    await openLayoutEditor();
    const first=panel();
    first.querySelector('[data-action="qr"][data-control="2"]').onclick();
    first.children[3].children[1].onclick();
    assert.equal(panel(),undefined);assert.equal(writes.length,0);

    await openLayoutEditor();
    const current=panel();
    current.querySelector('[data-action="qr"][data-control="2"]').onclick();
    write=async()=>{throw {code:'config_conflict',message:'conflict'};};
    await current.children[3].children[2].onclick();
    assert.equal(panel(),current);
    assert(writes[0].value.toolbar.includes('qr'));
    assert.deepEqual(writes[0].expected,DEFAULT_LAYOUT);
    assert.equal(current.children[3].children[2].disabled,false);
    assert(current.children[2].textContent.includes('其他窗口'));
    current.children[3].children[0].onclick();
    write=async()=>{};
    await current.children[3].children[2].onclick();
    assert.deepEqual(writes[1].value,DEFAULT_LAYOUT);assert.equal(panel(),undefined);
});
test('closed load and in-flight save never resurrect or update a replacement session',async()=>{
    let finishRead;
    read=()=>new Promise(resolve=>{finishRead=resolve;});
    const loading=openLayoutEditor();
    closeLayoutEditor();finishRead({toolbarLayout:DEFAULT_LAYOUT});await loading;
    assert.equal(panel(),undefined);
    read=async()=>({toolbarLayout:DEFAULT_LAYOUT});
    let finishWrite,applied=0;
    write=()=>new Promise(resolve=>{finishWrite=resolve;});
    await openLayoutEditor({onSaved:()=>applied++});
    const saving=panel().children[3].children[2].onclick();
    assert.equal(closeLayoutEditor(),false);
    assert.equal(closeLayoutEditor({force:true}),true);
    finishWrite();await saving;
    assert.equal(panel(),undefined);assert.equal(applied,0);
});
