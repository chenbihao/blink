import test from 'node:test';
import assert from 'node:assert/strict';
import vm from 'node:vm';
import {readFileSync} from 'node:fs';
import {TimelinePages} from './timeline-pages.js';
import {PreviewFrames, readyPreview} from './preview-frames.js';
import {commands, formatFrameDur, formatSeconds} from './timeline-view.js';
import {clampPreviewLeft, formatTimecode, keyboardStep, playbackOffset, pointerRatio, ratioToTime} from './seek-bar.js';

const source=readFileSync(new URL('./editor.js',import.meta.url),'utf8').replace(/^import\s[\s\S]*?;\r?$/gm,'');
const flush=async()=>{for(let i=0;i<5;i++)await new Promise(r=>setImmediate(r));};
function node(tagName='DIV') {
    const classes=new Set(),children=[],queries=new Map();
    return {tagName,style:{},dataset:{},children,handlers:{},clientWidth:800,width:0,height:0,scrollLeft:0,
        classList:{add:c=>classes.add(c),remove:c=>classes.delete(c),contains:c=>classes.has(c),toggle(c,on){if(on)classes.add(c);else classes.delete(c);}},
        addEventListener(e,h){this.handlers[e]=h;},removeEventListener(){},setAttribute(){},removeAttribute(){},focus(){},remove(){},closest(){return null;},
        appendChild(n){children.push(n);return n;},
        querySelector(q){if(q==='img')return children.find(n=>n.tagName==='IMG')||node('IMG');if(q==='.frame-tag')return children.find(n=>n.className==='frame-tag')||node();if(!queries.has(q))queries.set(q,node());return queries.get(q);},
        querySelectorAll(){return [node(),node()];},getBoundingClientRect(){return {left:0,width:800};},
        getContext(){return {drawImage(){},clearRect(){}};},setPointerCapture(){},releasePointerCapture(){},
    };
}
async function fixture() {
    const nodes=new Map(),calls=[],audioCalls=[];let revision=1,effective=100000,speed=1,now=100,holds=[];
    const mix=[{gain:1,muted:false},{gain:1,muted:false}];
    const get=id=>{if(!nodes.has(id))nodes.set(id,node());return nodes.get(id);};
    const summary=()=>({revision,frameCount:100000,effectiveCount:effective,totalMs:effective*100/speed,speed,mix:mix.map(m=>({...m})),canUndo:revision>1,canRedo:false,meta:{width:1280,height:720,fps:10},audio:{enabled:[true,true]}});
    const invoke=async(command,args={})=>{
        calls.push({command,args});
        if(command==='gif_editor_current')return 'one';
        if(command==='gif_get_session')return summary();
        if(command==='gif_get_frames')return args.indices.map(index=>({index,pngBase64:'',width:80,height:45}));
        if(command==='gif_get_preview_frame') {
            const data = new ArrayBuffer(8 + 80 * 45 * 4), header = new DataView(data);
            header.setUint32(0,80,true);header.setUint32(4,45,true);return data;
        }
        if(command==='gif_timeline_window') {
            if(args.selection) {
                const single=args.selection.length===1 && args.selection[0][0]===args.selection[0][1];
                const held=holds.find(h=>h[0]<=args.selection[0][0] && args.selection[0][0]<=h[1]);
                return {revision,hasHold:holds.some(h=>args.selection.some(([a,b])=>h[0]<=b && a<=h[1])),durationMs:single?(held?.[2]??100):null};
            }
            if(!effective && args.sourceOffset==null)return {revision,frames:[]};
            if(args.sourceOffset!=null)return {revision,frames:Array.from({length:Math.min(256,100000-args.sourceOffset)},(_,i)=>({srcIdx:args.sourceOffset+i,inTrim:true,deleted:false,baseMs:100,durMs:100}))};
            const first=Math.min(99999,args.play ?? args.source ?? Math.floor(args.timeMs/(100/speed)));
            return {revision,frames:Array.from({length:Math.min(128,100000-first)},(_,i)=>({playIdx:first+i,srcIdx:first+i,startMs:(first+i)*100/speed,durMs:100/speed,sourceMs:(first+i)*100,speed,soundMs:100,silenceMs:0}))};
        }
        if(command==='gif_edit'){
            revision++;
            if(JSON.stringify(args.command.ranges)==='[[0,99999]]' && args.command.operation==='delete')effective=0;
            if(args.command.type==='set_speed')speed=args.command.factor;
            if(args.command.type==='set_audio')mix[args.command.source]={gain:args.command.gain,muted:args.command.muted};
            if(args.command.operation==='hold')holds=args.command.ms?args.command.ranges.map(([a,b])=>[a,b,args.command.ms]):[];
            return summary();
        }
        return null;
    };
    const document={getElementById:get,createElement:t=>node(t.toUpperCase()),handlers:{},addEventListener(e,h,capture){this.handlers[e]={h,capture};},body:node(),activeElement:null};
    const context=vm.createContext({document,
        window:{addEventListener(){}},devicePixelRatio:1,performance:{now:()=>now},console,Map,Set,
        setTimeout:()=>1,clearTimeout(){},cancelAnimationFrame(){},
        createAudioPlayback:()=>({clear(){},pause(){audioCalls.push('pause');},load(){},sync(...args){audioCalls.push(args);},dispose(){}}),
        createExportController:()=>({setSession(){},setSource(){},invalidate(){},dispose(){}}),
        createRecentList:()=>({bind(){},refresh(){}}),ensureSpriteLoaded(){},onLangChange(){},
        bindEditorSelects:()=>()=>{},applyThemeFromConfig:async()=>{},
        applyI18nFromConfig:async()=>{},getCurrentWindow:()=>null,confirmDialog:async()=>true,
        invoke,listen:async()=>()=>{},normalizeError:e=>e,t:key=>key,EVENTS:{},
        fetch:async()=>({blob:async()=>({})}),createImageBitmap:async()=>({width:80,height:45,close(){}}),
        PreviewFrames: class extends PreviewFrames { constructor(options) { super({...options, decode: async()=>({width:80,height:45,close(){}})}); } }, readyPreview,
        TimelinePages,commands,formatFrameDur,formatSeconds,clampPreviewLeft,formatTimecode,keyboardStep,playbackOffset,pointerRatio,ratioToTime,
    });
    vm.runInContext(`${source}\nglobalThis.fixture={state,locate,handleThumbClick,editSelection,selectionRanges,endSeekDrag,pages,setPlaying,edit,positionMs,renderControls};`,context);
    await flush();return {...context.fixture,calls,audioCalls,nodes,get,document,setNow:n=>{now=n;}};
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

test('preview sound toggle never submits an edit while track mute does', async()=>{
    const f = await fixture();
    const before = f.calls.filter(c=>c.command==='gif_edit').length;
    f.get('preview-audio-enabled').handlers.change({target:{checked:false}});
    await flush();
    assert.equal(f.state.previewSound,false);
    assert.equal(f.calls.filter(c=>c.command==='gif_edit').length,before);
    const row=f.get('audio-mix-0');
    row.querySelector('input[type="range"]').value='150';
    row.querySelector('input[type="checkbox"]').checked=true;
    await row.handlers.change();await flush();
    const edit=f.calls.filter(c=>c.command==='gif_edit').at(-1).args.command;
    assert.equal(edit.type,'set_audio');assert.equal(edit.gain,1.5);assert.equal(edit.muted,true);
});

test('space remains transport after switches, ranges and closed select triggers; typing and open menus retain keys', async()=>{
    const f=await fixture(), key=f.document.handlers.keydown;
    assert.equal(key.capture,true);
    const space=(repeat=false)=>({code:'Space',repeat,preventDefault(){this.prevented=true;},stopPropagation(){this.stopped=true;}});
    for(const type of ['checkbox','range']) {
        f.document.activeElement={...node('INPUT'),type};
        const event=space();key.h(event);
        assert.equal(event.prevented,true);assert.equal(event.stopped,true);assert.equal(f.state.playing,true);
        key.h(space(true));assert.equal(f.state.playing,true,'held space must not toggle repeatedly');
        key.h(space());assert.equal(f.state.playing,false);
    }
    f.document.activeElement={...node('BUTTON'),closest:selector=>selector==='.editor-select-trigger'?{}:null};
    key.h(space());assert.equal(f.state.playing,true);
    for(const target of [{...node('INPUT'),type:'number'}, {...node(),isContentEditable:true},
        {...node('BUTTON'),closest:selector=>selector==='.editor-select-menu'?{}:null}]) {
        f.document.activeElement=target;
        const event=space();key.h(event);assert.equal(event.prevented,undefined);assert.equal(f.state.playing,true);
    }
});

test('gain dragging and mute edits keep preview clock and apply gain before commit',async()=>{
    const f=await fixture();f.setPlaying(true);await flush();f.setNow(250);
    const clock=f.state.playClockAt, row=f.get('audio-mix-0');
    row.querySelector('input[type="range"]').value='150';
    row.handlers.input();
    assert.equal(f.audioCalls.at(-1)[3][0].gain,1.5);
    assert.equal(f.state.view.mix[0].gain,1,'drag preview does not commit an export edit until change');
    row.querySelector('input[type="checkbox"]').checked=true;
    await row.handlers.change();await flush();
    assert.equal(f.state.playing,true);assert.equal(f.state.playClockAt,clock);
    assert.equal(f.state.view.mix[0].gain,1.5);assert.equal(f.state.view.mix[0].muted,true);
    assert.ok(!f.audioCalls.includes('pause'));
    assert.equal(f.state.previewMix,null);
});

test('preview sound can be turned off and back on during playback without suspending transport',async()=>{
    const f=await fixture();f.setPlaying(true);await flush();
    const clock=f.state.playClockAt, toggle=f.get('preview-audio-enabled').handlers.change;
    for(const checked of [false,true,false,true]) {
        toggle({target:{checked}});
        assert.equal(f.audioCalls.at(-1)[1],checked);
        assert.equal(f.state.playing,true);assert.equal(f.state.playClockAt,clock);
    }
    assert.ok(!f.audioCalls.includes('pause'));
});

test('changing speed while playing preserves source frame and fractional position',async()=>{
    const f=await fixture();f.setPlaying(true);await flush();f.setNow(250);
    await f.edit(commands.setSpeed(2));await flush();
    assert.equal(f.state.playing,true);
    assert.equal(f.state.current.srcIdx,1);
    assert.equal(f.state.current.durMs,50);
    assert.equal(f.state.seekInFrameMs,25);
    assert.equal(f.positionMs(),75);
    assert.equal(f.state.retiming,false);
    assert.ok(!f.audioCalls.includes('pause'));
});

test('duration reset is available only for held selections and restores the automatic value',async()=>{
    const f=await fixture();f.handleThumbClick(1,{});await flush();
    assert.equal(f.get('inp-dur').value,'100');assert.equal(f.get('btn-dur-clear').disabled,true);
    f.get('inp-dur').value='800';f.get('inp-dur').handlers.change();await flush();
    assert.equal(f.get('inp-dur').value,'800');assert.equal(f.get('btn-dur-clear').disabled,false);
    f.state.selectionRange=[0,99999];f.state.selectedSet.clear();f.renderControls();await flush();
    assert.equal(f.get('btn-dur-clear').disabled,false,'a held frame outside the viewport still enables range reset');
    assert.ok(f.calls.filter(c=>c.command==='gif_timeline_window' && c.args.selection).every(c=>c.args.selection.length<=1));
    f.state.selectionRange=null;f.state.selectedSet.add(1);f.renderControls();await flush();
    await f.get('btn-dur-clear').handlers.click();await flush();
    assert.equal(f.get('inp-dur').value,'100');assert.equal(f.get('btn-dur-clear').disabled,true);
    const edits=f.calls.filter(c=>c.command==='gif_edit').length;
    f.get('inp-dur').value='';f.get('inp-dur').handlers.change();await flush();
    assert.equal(f.calls.filter(c=>c.command==='gif_edit').length,edits,'clearing the field must not set an accidental 10ms hold');
});
