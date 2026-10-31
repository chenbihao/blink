import test from 'node:test';
import assert from 'node:assert/strict';
const nodes = new Map(), events = new Map(), windowEvents = new Map(), calls = [];
function node(id) {
    if (!nodes.has(id)) {
        const classes = new Set();
        nodes.set(id, {value:'', textContent:'', disabled:false, open:false, options:[{},{},{}], handlers:{},
            classList:{add:c=>classes.add(c), remove:c=>classes.delete(c), toggle(c,on){if(on)classes.add(c);else classes.delete(c);}, contains:c=>classes.has(c)},
            addEventListener(e,h){this.handlers[e]=h;}, removeEventListener(e){delete this.handlers[e];},
            showModal(){this.open=true;}, close(){this.open=false;}});
    }
    return nodes.get(id);
}
let runtime = {state:'missing'}, status = null, beginResolve = null;
globalThis.document = {getElementById:node};
globalThis.window = {
    setTimeout:()=>1, addEventListener:(e,h)=>windowEvents.set(e,h), removeEventListener:e=>windowEvents.delete(e),
    __TAURI__:{core:{invoke:async(command,args)=>{
        calls.push({command,args});
        if(command==='gif_export_availability')return runtime;
        if(command==='gif_export_status')return status;
        if(command==='gif_begin_export')return new Promise(resolve=>{beginResolve=resolve;});
        return null;
    }},event:{listen:async(e,h)=>{events.set(e,h);return()=>events.delete(e);}}},
};
const {createExportController} = await import('./export-controller.js');
const flush = () => new Promise(resolve=>setImmediate(resolve));
const click = id => node(id).handlers.click();
const emit = payload => events.get('blink://gif-export-progress')({payload});
const job = (id, sequence, stage='encoding', sessionId='one')=>({jobId:id, sequence, stage, sessionId, format:'mp4',framesDone:15,framesTotal:30});

test('缺引擎安装引导保留路径，安装后不自动导出；开始事件丢失由终态查询恢复', async()=>{
    node('export-format').value='gif'; node('export-quality').value='medium';
    const controller=createExportController(); controller.setSession('one'); click('btn-export'); await flush();
    node('export-path').value='C:\\中文 空格\\片段.gif'; node('export-format').value='mp4';
    node('export-format').handlers.change(); await flush();
    assert.equal(node('export-path').value,'C:\\中文 空格\\片段.mp4');
    assert.equal(node('btn-export-confirm').disabled,true);
    await click('btn-export-install'); assert.deepEqual(calls.at(-1),{command:'open_settings_tab',args:{tab:'media'}});
    runtime={state:'ready'}; click('btn-export'); await flush(); windowEvents.get('focus')(); await flush();
    assert.equal(node('btn-export-confirm').disabled,false);
    assert.equal(calls.filter(c=>c.command==='gif_begin_export').length,0);
    const started=click('btn-export-confirm'); await flush();
    click('btn-export-confirm'); assert.equal(calls.filter(c=>c.command==='gif_begin_export').length,1);
    emit(job('job-1',2));
    status={...job('job-1',8,'done'),stats:{width:66,height:68,writtenFrames:30,durationMs:1000,bytes:1024,outputPath:'C:\\out.mp4'}};
    beginResolve('job-1'); await started; await flush();
    assert.match(node('export-progress-text').textContent,/完成：MP4/);
    assert.equal(node('export-progress').value,100);
    emit(job('job-1',9,'cancelled')); assert.match(node('export-progress-text').textContent,/完成：MP4/);
    controller.dispose();
});
test('切换会话后迟到开始/进度无效，新任务取消携带准确 job id',async()=>{
    status=null; node('export-format').value='mp4';
    const controller=createExportController(); controller.setSession('one'); click('btn-export'); await flush();
    node('export-path').value='C:\\old.mp4'; const oldStart=click('btn-export-confirm'); await flush();
    const oldResolve=beginResolve; controller.setSession('two'); await flush();
    oldResolve('old-job'); await oldStart; emit(job('old-job',8,'done')); await flush();
    assert.equal(node('export-progress-row').classList.contains('hidden'),true);
    node('export-path').value='C:\\new.mp4'; const newStart=click('btn-export-confirm'); await flush();
    status=job('new-job',2,'encoding','two'); beginResolve('new-job'); await newStart; await flush();
    assert.equal(node('btn-export-cancel-req').disabled,false);
    click('btn-export-close'); assert.equal(node('export-dialog').open,false);
    click('btn-export-cancel-req'); await flush();
    assert.deepEqual(calls.findLast(c=>c.command==='gif_cancel_export_job').args,{sessionId:'two',jobId:'new-job'});
    emit({...job('new-job',3,'cancelled','two')});
    assert.equal(node('btn-export-confirm').disabled,false);
    controller.dispose();
});
