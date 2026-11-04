import test from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
import {readFileSync} from 'node:fs';
import {TimelinePages} from './timeline-pages.js';
import {commands, formatFrameDur, formatSeconds} from './timeline-view.js';
import {clampPreviewLeft, formatTimecode, keyboardStep, playbackOffset, pointerRatio, ratioToTime} from './seek-bar.js';

const source=readFileSync(new URL('./editor.js',import.meta.url),'utf8').replace(/^import\s[\s\S]*?;\r?$/gm,'');
const flush=async()=>{for(let i=0;i<5;i++)await new Promise(r=>setImmediate(r));};
function node(tagName='DIV') {
    const classes=new Set(),children=[],queries=new Map();
    return {tagName,style:{},dataset:{},children,handlers:{},clientWidth:800,width:0,height:0,scrollLeft:0,
        classList:{add:c=>classes.add(c),remove:c=>classes.delete(c),contains:c=>classes.has(c),toggle(c,on){if(on)classes.add(c);else classes.delete(c);}},
        addEventListener(e,h){this.handlers[e]=h;},removeEventListener(){},setAttribute(){},removeAttribute(){},focus(){},remove(){},
        appendChild(n){children.push(n);return n;},
        querySelector(q){if(q==='img')return children.find(n=>n.tagName==='IMG')||node('IMG');if(q==='.frame-tag')return children.find(n=>n.className==='frame-tag')||node();if(!queries.has(q))queries.set(q,node());return queries.get(q);},
        querySelectorAll(){return [node(),node()];},getBoundingClientRect(){return {left:0,width:800};},
        getContext(){return {drawImage(){},clearRect(){}};},setPointerCapture(){},releasePointerCapture(){},
    };
}
async function fixture() {
    const nodes=new Map(),calls=[];let revision=1,effective=100000;
    const get=id=>{if(!nodes.has(id))nodes.set(id,node());return nodes.get(id);};
    const summary=()=>({revision,frameCount:100000,effectiveCount:effective,totalMs:effective*100,speed:1,mix:[],canUndo:revision>1,canRedo:false,meta:{width:1280,height:720,fps:10},audio:{enabled:[false,false]}});
    const invoke=async(command,args={})=>{
        calls.push({command,args});
        if(command==='gif_editor_current')return 'one';
        if(command==='gif_get_session')return summary();
        if(command==='gif_get_frames')return args.indices.map(index=>({index,pngBase64:'',width:80,height:45}));
        if(command==='gif_timeline_window') {
            if(!effective && args.sourceOffset==null)return {revision,frames:[]};
            if(args.sourceOffset!=null)return {revision,frames:Array.from({length:Math.min(256,100000-args.sourceOffset)},(_,i)=>({srcIdx:args.sourceOffset+i,inTrim:true,deleted:false,baseMs:100,durMs:100}))};
            const first=Math.min(99999,args.play ?? args.source ?? Math.floor(args.timeMs/100));
            return {revision,frames:Array.from({length:Math.min(128,100000-first)},(_,i)=>({playIdx:first+i,srcIdx:first+i,startMs:(first+i)*100,durMs:100,sourceMs:(first+i)*100,soundMs:100,silenceMs:0}))};
        }
        if(command==='gif_edit'){revision++;if(JSON.stringify(args.command.ranges)==='[[0,99999]]' && args.command.operation==='delete')effective=0;return summary();}
        return null;
    };
    const context=vm.createContext({document:{getElementById:get,createElement:t=>node(t.toUpperCase()),addEventListener(){},body:node(),activeElement:null},
        window:{addEventListener(){}},devicePixelRatio:1,performance:{now:()=>100},console,Map,Set,
        setTimeout:()=>1,clearTimeout(){},cancelAnimationFrame(){},
        createAudioPlayback:()=>({clear(){},pause(){},load(){},sync(){},dispose(){}}),
        createExportController:()=>({setSession(){},setSource(){},invalidate(){},dispose(){}}),
        createRecentList:()=>({bind(){},refresh(){}}),ensureSpriteLoaded(){},onLangChange(){},
        bindEditorSelects:()=>()=>{},applyThemeFromConfig:async()=>{},
        applyI18nFromConfig:async()=>{},getCurrentWindow:()=>null,confirmDialog:async()=>true,
        invoke,listen:async()=>()=>{},normalizeError:e=>e,t:key=>key,EVENTS:{},
        fetch:async()=>({blob:async()=>({})}),createImageBitmap:async()=>({width:80,height:45,close(){}}),
        TimelinePages,commands,formatFrameDur,formatSeconds,clampPreviewLeft,formatTimecode,keyboardStep,playbackOffset,pointerRatio,ratioToTime,
    });
    vm.runInContext(`${source}\nglobalThis.fixture={state,locate,handleThumbClick,editSelection,selectionRanges,endSeekDrag,pages};`,context);
    await flush();return {...context.fixture,calls};
}

test('editor loads bounded metadata/visible images and can seek directly to the end of a long session',async()=>{
    const f=await fixture();
    assert.equal(f.state.current.srcIdx,0);
    assert.equal(f.state.view.frames,undefined);
    assert.ok(f.state.liveThumbs.size<=30);
    const decoded=f.calls.filter(c=>c.command==='gif_get_frames');
    assert.ok(decoded.every(c=>c.args.indices.length<=24));
    assert.ok(decoded.reduce((n,c)=>n+c.args.indices.length,0)<100);
    await f.locate({timeMs:9999999});await flush();
    assert.equal(f.state.current.srcIdx,99999);
    assert.equal(f.state.seekInFrameMs,99);
});

test('shift selection and batch edit cross old frame limits without expanding an IPC frame array',async()=>{
    const f=await fixture();
    f.handleThumbClick(1,{});await flush();f.handleThumbClick(99999,{shiftKey:true});
    assert.equal(f.state.selectedSet.size,0);
    await f.editSelection('delete');await flush();
    const command=f.calls.find(c=>c.command==='gif_edit').args;
    assert.equal(command.incremental,true);assert.equal(command.revision,1);
    assert.equal(JSON.stringify(command.command.ranges),'[[1,99999]]');
    assert.equal(command.command.frames,undefined);
    assert.equal(f.state.view.revision,2);
});


test('ending a paused seek waits for a metadata slot and commits the latest target',async()=>{
    const f=await fixture();
    let release;
    const pending=new Promise(r=>{release=r;});
    for(let n=0;n<4;n++) f.pages.inflight.add(pending.then(()=>{}));
    pending.then(()=>f.pages.inflight.clear());
    f.state.seekDragging=true;f.state.seekRequestedMs=9999999;
    const end=f.endSeekDrag(true);
    await flush();assert.equal(f.state.current.srcIdx,0);
    release();await end;
    assert.equal(f.state.current.srcIdx,99999);
    assert.equal(f.state.playing,false);
});


test('deleting the whole effective timeline clears the playback frame',async()=>{
    const f=await fixture();
    f.state.selectionRange=[0,99999];
    await f.editSelection('delete');await flush();
    assert.equal(f.state.view.effectiveCount,0);
    assert.equal(f.state.current,null);
    assert.equal(f.state.playing,false);
});
