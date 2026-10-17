import assert from "node:assert/strict";
import {test} from "node:test";
import {TestElement} from "../settings/tabs/weather-settings-test-helpers.mjs";

globalThis.window ??= {};
const {fitImage, zoomImage, panImage, createClipboardImageViewer} = await import("./clipboard-image-viewer.js");

test("fit, cursor-centered zoom and pan retain visible bounds for wide and long images", () => {
    for (const [width, height] of [[1200,800], [2459,12448], [40,20]]) {
        const fit = fitImage(width,height,300,200);
        assert.ok(width * fit.scale <= 300 && height * fit.scale <= 200);
        assert.ok(fit.scale <= 1);
        const zoom = zoomImage(fit,2,150,100);
        assert.ok(Math.abs((150 - fit.x) / fit.scale - (150 - zoom.x) / zoom.scale) < 1e-8);
        assert.ok(Math.abs((100 - fit.y) / fit.scale - (100 - zoom.y) / zoom.scale) < 1e-8);
        const maximum = zoomImage(fit,1e6,150,100);
        assert.equal(maximum.scale,4);
        const moved = panImage(maximum,1e6,-1e6);
        assert.ok(moved.x <= Math.max(0,(300 - width * moved.scale) / 2));
        assert.ok(moved.y >= Math.min(0,200 - height * moved.scale));
        assert.deepEqual(zoomImage(maximum,1e-6,150,100),fit);
    }
});

test("viewer keeps the original image, captures drags and releases on cancellation or disposal", async t => {
    const previousDocument = globalThis.document;
    const capture = new Set();
    const element = () => {
        const node = new TestElement();
        node.style = {};
        node.getBoundingClientRect = () => ({left:0,top:0,width:300,height:200});
        node.setPointerCapture = id => capture.add(id);
        node.hasPointerCapture = id => capture.has(id);
        node.releasePointerCapture = id => capture.delete(id);
        return node;
    };
    globalThis.document = {createElement: element};
    t.after(() => {globalThis.document = previousDocument;});
    const image = element();
    image.naturalWidth = 1200;
    image.naturalHeight = 800;
    image.src = "original.gif";
    let started = 0, finished = 0;
    const viewer = createClipboardImageViewer(image,{width:300,maxHeight:200,
        onDragStart: () => started++, onDragEnd: () => finished++});
    const [viewport, caption] = viewer.root.children;
    const fitTransform = image.style.transform;
    await viewport.fire("pointerdown",{button:0,pointerId:1,clientX:150,clientY:100});
    assert.equal(started,0,"fitted images do not start a meaningless drag");
    let prevented = false, stopped = false;
    await viewport.fire("wheel",{deltaY:-350,deltaMode:0,clientX:150,clientY:100,
        preventDefault() {prevented = true;}, stopPropagation() {stopped = true;}});
    assert.ok(prevented && stopped,"zoom does not scroll/navigate the result list");
    assert.notEqual(image.style.transform,fitTransform);
    await viewport.fire("pointerdown",{button:0,pointerId:1,clientX:150,clientY:100});
    assert.equal(started,1);
    assert.ok(capture.has(1));
    await viewport.fire("pointermove",{pointerId:1,clientX:400,clientY:400});
    await viewport.fire("pointercancel",{pointerId:1});
    assert.equal(finished,1);
    assert.equal(capture.size,0);
    await viewport.fire("pointerdown",{button:0,pointerId:2,clientX:100,clientY:100});
    viewer.dispose();
    viewer.dispose();
    assert.equal(finished,2,"disposal releases an active gesture exactly once");
    await viewport.fire("dblclick");
    assert.equal(image.style.transform,fitTransform);
    viewer.setFileSize(946084);
    assert.ok(caption.textContent.includes("924 KB"));
    assert.equal(image.src,"original.gif","zoom and reset never reload or flatten animation");
    assert.equal(viewport.children[0],image);
});
