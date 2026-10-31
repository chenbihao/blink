import {test} from 'node:test';
import assert from 'node:assert/strict';
import {QrSession, createLatestQrResolver, qrActionAllowed} from './qr-session.js';
import {registerScreenshotAction, runScreenshotAction} from './ss-actions.js';

test('closing, rescanning, changing selection and resetting invalidate old QR results/errors', () => {
    const session=new QrSession();
    const first=session.begin('image1:selection1');
    assert(session.current(first,'image1:selection1'));
    session.cache={key:first.key,results:['kept']};
    session.close();
    assert(!session.current(first,first.key));
    assert.equal(session.cache.results[0],'kept');
    const second=session.begin(first.key);
    assert(!session.current(first,first.key));
    assert(session.current(second,first.key));
    assert(!session.current(second,'image1:selection2'));
    session.reset();
    assert(!session.current(second,first.key));assert.equal(session.cache,null);
});

test('rapid target changes admit one active and only the latest pending probe', async () => {
    const calls=[], release=[];
    const resolve=createLatestQrResolver(text => { calls.push(text);return new Promise(done=>release.push(done)); });
    const first=resolve('first',()=>true), second=resolve('second',()=>true), third=resolve('third',()=>true);
    assert.deepEqual(calls,['first']);assert.equal(await second,null);
    release.shift()({kind:'text'});await first;await Promise.resolve();
    assert.deepEqual(calls,['first','third']);release.shift()({kind:'text'});await third;
    assert.equal(await resolve('closed',()=>false),null);assert.deepEqual(calls,['first','third']);
});
test('frontend action whitelist rejects unsafe protocols, disguised hosts and network/device paths',()=>{
    assert(qrActionAllowed({kind:'web',target:'https://user@example.com/a',host:'example.com'}));
    assert(qrActionAllowed({kind:'directory',target:'C:\\中文\\a % b'}));
    for(const target of [
        {kind:'web',target:'ftp://example.com',host:'example.com'},
        {kind:'web',target:'https://evil.example',host:'example.com'},
        {kind:'file',target:'\\\\server\\share\\file'},
        {kind:'file',target:'\\\\?\\C:\\file'},
        {kind:'file',target:'C:\\file:stream'},
        {kind:'text',target:'C:\\file'},
    ]) assert.equal(qrActionAllowed(target),false);
});
test('all action entry points honor the same canonical button gate', () => {
    let calls=0;
    const button={disabled:false,hidden:false,classList:{contains:()=>false}};
    globalThis.document={getElementById:id=>id==='btn-ocr'?button:null};
    registerScreenshotAction('ocr',()=>calls++);
    assert.equal(runScreenshotAction('ocr'),true);assert.equal(calls,1);
    button.disabled=true;assert.equal(runScreenshotAction('ocr'),false);
    button.disabled=false;button.hidden=true;assert.equal(runScreenshotAction('ocr'),false);
    assert.equal(runScreenshotAction('missing'),false);assert.equal(calls,1);
    delete globalThis.document;
});
