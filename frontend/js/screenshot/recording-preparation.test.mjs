import test from 'node:test';
import assert from 'node:assert/strict';
import {createPreparationLease} from './recording-preparation.js';
const flush=async()=>{for(let n=0;n<4;n++)await new Promise(r=>setImmediate(r));};

test('closing while preparation is pending releases only the original lease before opening the next',async()=>{
    const calls=[];let finish;
    const lease=createPreparationLease(async(command,args)=>{
        calls.push({command,args});
        if(args.active && calls.length===1) return new Promise(r=>finish=r);
        return args.active ? 2 : null;
    });
    const first=lease.open();await flush();
    const closing=lease.close(),next=lease.open();
    assert.equal(calls.length,1);
    finish(1);await Promise.all([first,closing,next]);
    assert.deepEqual(calls.map(c=>c.args),[{active:true},{active:false,preparationId:1},{active:true}]);
    assert.equal(await lease.ready(),2);
    await lease.close();
    assert.deepEqual(calls.at(-1).args,{active:false,preparationId:2});
    assert.ok(calls.every(c=>c.command==='gif_overlay_prepare'),'准备阶段不启动素材采集');
});

test('a failed preparation does not poison a later attempt or release an unrelated lease',async()=>{
    let attempts=0;const closed=[];
    const lease=createPreparationLease(async(_command,args)=>{
        if(!args.active) {closed.push(args.preparationId);return null;}
        if(++attempts===1) throw new Error('window unavailable');
        return 7;
    });
    await assert.rejects(lease.open());await lease.close();
    assert.equal(await lease.open(),7);await lease.close();
    assert.deepEqual(closed,[7]);
});
