import test from 'node:test';
import assert from 'node:assert/strict';
globalThis.window ||= {};
const {placeRecordingPanel, formatRecordingBytes} = await import('./ss-recording-ui.js');

test('settings stay six visual pixels from their anchor, regardless of the capture region', () => {
    const monitor = {x:0, y:0, w:1920, h:1080};
    assert.deepEqual(placeRecordingPanel({left:300,top:400,bottom:432},112,180,monitor),{left:300,top:438});
    assert.deepEqual(placeRecordingPanel({left:300,top:950,bottom:982},112,180,monitor),{left:300,top:764});
});

test('menus clamp to negative-coordinate monitors and use scaled dimensions at mixed DPI', () => {
    const monitor = {x:-1920,y:-120,w:1920,h:1080};
    for (const scale of [1,1.25,1.5,2]) {
        const width=160*scale,height=200*scale;
        const result=placeRecordingPanel({left:-40,top:450,bottom:450+32*scale},width,height,monitor,6*scale);
        assert.ok(result.left>=monitor.x+8 && result.left+width<=-8);
        assert.ok(result.top>=monitor.y+8 && result.top+height<=monitor.y+monitor.h-8);
    }
});

test('space information distinguishes unavailable values from a full disk', () => {
    assert.equal(formatRecordingBytes(undefined),'—');
    assert.equal(formatRecordingBytes(0),'0.0 MB');
    assert.equal(formatRecordingBytes(2.5*1024**3),'2.5 GB');
});
