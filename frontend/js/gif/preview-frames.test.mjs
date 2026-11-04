import test from 'node:test';
import assert from 'node:assert/strict';
import {PreviewFrames, readyPreview, decodeFrame} from './preview-frames.js';
const flush = async()=>{for(let n=0;n<5;n++)await new Promise(r=>setImmediate(r));};

test('30fps playhead keeps presenting completed frames even when decoding takes more than a frame interval', async()=>{
    const s={epoch:1,sessionId:'one',previewWidth:640,previewCache:new Map(),previewBytes:0,playing:true,current:{srcIdx:0},view:{frameCount:1000}};
    const pending=[],shown=[];let maximum=0;
    const decoder=new PreviewFrames({state:s,invoke:(_command,args)=>new Promise(resolve=>{pending.push({args,resolve});maximum=Math.max(maximum,decoder.active);}),
        decode:async id=>({id,width:640,height:360,close(){}}),evict(){},
        onReady(){shown.push(readyPreview(s.previewCache,s.current.srcIdx,true)?.id);},onError:assert.fail});
    decoder.request(Array.from({length:16},(_,i)=>i));
    assert.equal(pending.length,2);
    // 两个解码请求都晚于 33ms 播放间隔；完成时播放头已走过它们。
    s.current.srcIdx=3;decoder.request([3],true);
    pending[0].resolve(0);await flush();
    assert.equal(shown.at(-1),0,'不能因为当前帧已变成3而丢弃迟到的第0帧');
    assert.equal(pending[2].args.index,3,'当前目标先于旧的预取队列');
    pending[1].resolve(1);await flush();assert.equal(shown.at(-1),1);
    s.current.srcIdx=6;decoder.request([6],true);
    pending[2].resolve(3);await flush();assert.equal(shown.at(-1),3,'解码持续落后时画面仍前进');
    assert.ok(maximum<=2);assert.ok(decoder.queue.length<=32);
    s.epoch++;for(const request of pending)request.resolve(0);await flush();
    assert.equal(decoder.active,0);
});

test('precise paused seek never substitutes a different frame; obsolete bitmap is closed', async()=>{
    const cache=new Map([[1,{id:1}],[9,{id:9}]]);
    assert.equal(readyPreview(cache,5,false),null);
    assert.equal(readyPreview(cache,5,true).id,1);
    assert.equal(readyPreview(cache,1,false).id,1);
    const s={epoch:1,sessionId:'one',previewWidth:640,previewCache:new Map(),previewBytes:0,view:{frameCount:2}};
    let finish,closed=0;
    const decoder=new PreviewFrames({state:s,invoke:async()=>0,decode:()=>new Promise(r=>finish=r),evict(){},onReady:assert.fail});
    decoder.request([0]);await flush();
    s.epoch++;finish({width:1,height:1,close(){closed++;}});await flush();
    assert.equal(closed,1);assert.equal(s.previewCache.size,0);assert.equal(s.previewBytes,0);
});

test('binary frame validation rejects malformed payloads before allocating a bitmap', async()=>{
    await assert.rejects(decodeFrame(new ArrayBuffer(4)));
    const invalid=new ArrayBuffer(12),header=new DataView(invalid);header.setUint32(0,5000,true);header.setUint32(4,5000,true);
    await assert.rejects(decodeFrame(invalid));
});

test('binary frame preserves exact RGBA pixels and dimensions without PNG or Base64', async()=>{
    const originalImageData=globalThis.ImageData, originalBitmap=globalThis.createImageBitmap;
    globalThis.ImageData=class { constructor(data,width,height){Object.assign(this,{data,width,height});} };
    globalThis.createImageBitmap=async image=>image;
    try {
        const bytes=new Uint8Array([2,0,0,0,1,0,0,0,30,20,10,255,60,50,40,255]);
        const image=await decodeFrame(bytes);
        assert.equal(image.width,2);assert.equal(image.height,1);
        assert.deepEqual([...image.data],[30,20,10,255,60,50,40,255]);
    } finally {globalThis.ImageData=originalImageData;globalThis.createImageBitmap=originalBitmap;}
});
