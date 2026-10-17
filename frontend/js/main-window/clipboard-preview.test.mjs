import assert from "node:assert/strict";
import {test} from "node:test";
import {TestElement} from "../settings/tabs/weather-settings-test-helpers.mjs";

globalThis.window ??= {};
const {bindClipboardPreview, bindErrorPreview, hideClipboardPreview, getClipboardPreviewContext, holdClipboardPreview, previewHeight} = await import("./clipboard-preview.js");

test("image preview height adapts to the viewport while text stays compact", () => {
    for (const [viewport, expected] of [[400,300], [640,480], [1000,750], [200,184], [16,1]]) {
        assert.equal(previewHeight(viewport, true), expected);
        assert.equal(previewHeight(viewport), Math.max(1, Math.min(260, viewport - 16)));
    }
});

test("clipboard hover lazily shows full text and rejects stale entry responses", async (t) => {
    t.mock.timers.enable({apis:["setTimeout"]});
    const oldDocument = globalThis.document;
    const oldTauri = window.__TAURI__;
    const oldAddListener = window.addEventListener;
    const oldGetSelection = window.getSelection;
    const body = new TestElement();
    const documentListeners = new Map();
    const windowListeners = new Map();
    let selectedPanel;
    let textSelection;
    const query = new TestElement();
    query.focus = () => {document.activeElement = query;};
    const createdImages = [];
    const previewImageForTest = () => createdImages.at(-1);
    const element = (tag) => {
        const view = new TestElement();
        view.style = {setProperty(name, value) {this[name] = value;}};
        view.replaceChildren = (...children) => {view.children = children; view.textContent = "";};
        view.getBoundingClientRect = () => ({left:10,top:10,bottom:35,height:80,width:300});
        view.focus = () => {document.activeElement = view;};
        view.setPointerCapture = () => {};
        if (tag === "img") {
            view.naturalWidth = 1200;
            view.naturalHeight = 800;
            createdImages.push(view);
        }
        return view;
    };
    globalThis.document = {
        body, createElement: element, getElementById: id => id === "query" ? query : null,
        addEventListener(name, listener) {documentListeners.set(name, listener);},
    };
    window.getSelection = () => textSelection ?? ({selectAllChildren(view) {selectedPanel = view;}});
    window.addEventListener = (name, listener) => windowListeners.set(name, listener);
    window.innerWidth = 600;
    window.innerHeight = 400;
    const pending = new Map();
    window.__TAURI__ = {core:{invoke:async (command,{id}) => {
        assert.equal(command,"get_clipboard_text");
        return new Promise(resolve => pending.set(id,resolve));
    }}};
    t.after(() => {
        hideClipboardPreview();
        globalThis.document = oldDocument;
        window.__TAURI__ = oldTauri;
        window.addEventListener = oldAddListener;
        window.getSelection = oldGetSelection;
    });
    const first = element();
    const second = element();
    bindClipboardPreview(first,{kind:"text",id:"first"});
    bindClipboardPreview(second,{kind:"text",id:"second"});
    await first.fire("pointerenter", {clientX:250});
    t.mock.timers.tick(350);
    assert.equal(pending.size,0,"hovering the right two thirds keeps list operations unobstructed");
    await first.fire("pointermove", {clientX:111});
    t.mock.timers.tick(350);
    assert.equal(pending.size,0,"trigger threshold follows the actual row bounds");
    await first.fire("pointermove", {clientX:110});
    t.mock.timers.tick(349);
    assert.equal(pending.size,0,"no full text read in the search or initial hover path");
    t.mock.timers.tick(1);
    assert.ok(pending.has("first"));
    await second.fire("pointerenter", {clientX:50});
    t.mock.timers.tick(350);
    const full = "line one\n" + "完整文本 ".repeat(120);
    pending.get("second")(full);
    for (let i=0;i<5;i++) await Promise.resolve();
    const panel = body.children[0];
    assert.equal(panel.textContent,full,"preserves newlines and all text beyond the row summary");
    assert.equal(panel.style.left, "10px");
    assert.equal(panel.style.width, "300px", "both edges align with the row, not a fixed preview width");
    pending.get("first")("stale text");
    for (let i=0;i<5;i++) await Promise.resolve();
    assert.equal(panel.textContent,full);
    await second.fire("pointerleave");
    t.mock.timers.tick(50);
    await first.fire("pointerenter", {clientX:50});
    assert.equal(panel.classList.contains("hidden"),false,"crossing another row does not bypass the exit buffer");
    t.mock.timers.tick(50);
    await panel.fire("pointerenter");
    t.mock.timers.tick(500);
    assert.equal(panel.classList.contains("hidden"),false,"entering the preview cancels both closing and crossed-row hover");
    assert.equal(panel.textContent,full,"a crossed row never replaces the panel being read");
    hideClipboardPreview();
    assert.equal(panel.textContent,"");
    assert.equal(panel.classList.contains("hidden"),true);
    const errorRow = element();
    const error = "插件返回错误 (invalid_arguments): " + "完整错误信息\n".repeat(80);
    bindErrorPreview(errorRow,error);
    await errorRow.fire("pointerenter", {clientX:50});
    t.mock.timers.tick(350);
    for (let i=0;i<5;i++) await Promise.resolve();
    assert.equal(panel.textContent,error);
    assert.equal(panel.dataset.errorText,error,"right click can target the same full error snapshot");
    await panel.fire("pointerenter");
    await panel.fire("pointerdown", {button:0});
    await errorRow.fire("pointerleave");
    await panel.fire("pointerleave");
    await first.fire("pointermove", {clientX:50,buttons:1});
    t.mock.timers.tick(1000);
    assert.equal(panel.classList.contains("hidden"),false,"text dragging outside the panel keeps its selection alive");
    documentListeners.get("pointerup")();
    t.mock.timers.tick(149);
    assert.equal(panel.classList.contains("hidden"),false,"exit delay starts after the selection gesture ends");
    await panel.fire("pointerenter");
    const keydown = async (key, ctrlKey = false) => {
        let stopped = false, prevented = false;
        await panel.fire("keydown", {
            key, ctrlKey, stopPropagation() {stopped = true;}, preventDefault() {prevented = true;},
        });
        if (!stopped) documentListeners.get("keydown")();
        return {stopped, prevented};
    };
    assert.deepEqual(await keydown("Control"), {stopped:true,prevented:false});
    assert.deepEqual(await keydown("c", true), {stopped:true,prevented:false},"Ctrl+C keeps native text copy and bypasses row shortcuts");
    assert.equal(panel.classList.contains("hidden"),false);
    assert.deepEqual(await keydown("a", true), {stopped:true,prevented:true});
    assert.equal(selectedPanel,panel,"Ctrl+A selects only preview text");
    await keydown("Escape");
    assert.equal(panel.classList.contains("hidden"),true);
    assert.equal(document.activeElement,query,"Escape restores the main input after selecting text");
    await errorRow.fire("pointerenter", {clientX:50});
    t.mock.timers.tick(350);
    for (let i=0;i<5;i++) await Promise.resolve();
    await errorRow.fire("pointermove", {clientX:250});
    t.mock.timers.tick(149);
    assert.equal(panel.classList.contains("hidden"),false,"the exit buffer lasts 150ms, matching existing tooltips");
    await errorRow.fire("pointermove", {clientX:260});
    t.mock.timers.tick(1);
    assert.equal(panel.classList.contains("hidden"),true,"continued movement outside the trigger does not restart the exit delay");
    await first.fire("pointermove", {clientX:50});
    t.mock.timers.tick(300);
    await first.fire("pointermove", {clientX:250});
    t.mock.timers.tick(400);
    assert.equal(panel.classList.contains("hidden"),true,"leaving the trigger before the delay never opens a panel");
    await first.fire("pointermove", {clientX:50,buttons:1});
    t.mock.timers.tick(350);
    assert.equal(panel.classList.contains("hidden"),true,"dragging in the trigger region does not open a preview");
    hideClipboardPreview();
    const imageRow = element(), thumbnail = element();
    thumbnail.alt = "image";
    imageRow.selectors.set("img.clip-thumb", thumbnail);
    bindClipboardPreview(imageRow, {kind: "image", id: "picture"});
    await imageRow.fire("pointerenter", {clientX: 250, clientY: 30});
    t.mock.timers.tick(350);
    assert.equal(panel.classList.contains("hidden"), true, "image right side does not trigger preview");
    await imageRow.fire("pointermove", {clientX: 50, clientY: 30});
    t.mock.timers.tick(349);
    assert.equal(panel.classList.contains("hidden"), true, "left third still waits for hover intent");
    t.mock.timers.tick(1);
    assert.equal(panel.classList.contains("hidden"), false, "image shares the text left-third trigger");
    const image = previewImageForTest();
    assert.equal(image.src, "http://blink-clipimg.localhost/preview/picture?v=native", "原图预览不命中旧的缩小图及预算拒绝缓存");
    assert.equal(image.alt, "image", "preview image keeps the thumbnail alt");
    const staleLoad = image.onload;
    image.naturalHeight = 2400;
    image.onload();
    assert.equal(panel.children[0].children[0].children[0], image);
    assert.equal(panel.classList.contains("is-image-preview"), true);
    assert.equal(panel.style.maxHeight, "300px");
    assert.equal(panel.children[0].children[0].style.height, "248px", "long images use the adaptive height including footer space");
    await imageRow.fire("pointerleave");
    t.mock.timers.tick(100);
    await panel.fire("pointerenter");
    t.mock.timers.tick(200);
    assert.equal(panel.classList.contains("hidden"), false, "image preview shares the hover buffer");
    await first.fire("pointermove", {clientX: 50});
    t.mock.timers.tick(350);
    staleLoad();
    assert.equal(panel.classList.contains("is-image-preview"), false, "late image cannot replace a text preview");
    hideClipboardPreview();
    assert.equal(image.src, undefined, "closing releases the loaded image URL");
    hideClipboardPreview();
    assert.equal(panel.dataset.errorText,undefined);

    await first.fire("pointermove", {clientX:50});
    t.mock.timers.tick(350);
    pending.get("first")("preview text");
    for (let i=0;i<5;i++) await Promise.resolve();
    const startNode = {}, endNode = {};
    panel.contains = node => node === startNode || node === endNode;
    const previewTarget = {closest: () => panel};
    textSelection = {rangeCount:1, isCollapsed:false, anchorNode:startNode, focusNode:endNode, toString: () => "selected text"};
    const context = getClipboardPreviewContext(previewTarget);
    assert.equal(context.anchor,first);
    assert.equal(context.selectedText,"selected text");
    textSelection.focusNode = {};
    assert.equal(getClipboardPreviewContext(previewTarget).selectedText,"","cross-boundary selection is not owned by the preview");
    assert.equal(getClipboardPreviewContext({closest: () => null}),null);
    assert.equal(context.selectedText,"selected text","selection snapshot survives later focus changes");

    await first.fire("pointerleave");
    const releaseOld = holdClipboardPreview(context);
    windowListeners.get("blur")();
    await second.fire("pointermove", {clientX:50});
    documentListeners.get("keydown")();
    t.mock.timers.tick(1000);
    assert.equal(panel.textContent,"preview text","popup focus and crossing another row cannot replace the held preview");
    const releaseNew = holdClipboardPreview(context);
    releaseOld();
    t.mock.timers.tick(1000);
    assert.equal(panel.classList.contains("hidden"),false,"old menu release cannot dismiss the current menu's preview");
    releaseNew();
    t.mock.timers.tick(149);
    assert.equal(panel.classList.contains("hidden"),false,"menu dismissal restores the 150ms buffer");
    t.mock.timers.tick(1);
    assert.equal(panel.classList.contains("hidden"),true);
    assert.equal(context.isCurrent(),false,"closing invalidates the menu's anchor snapshot");
    assert.equal(getClipboardPreviewContext(previewTarget),null);

    let fileStatus = "animation_budget";
    const filePath = "C:\\pictures\\large.jpg";
    window.__TAURI__ = {core:{invoke:async (command) => {
        if (command === "get_clipboard_file_entry") return {paths:[filePath]};
        assert.equal(command,"get_clipboard_file_preview_status");
        return fileStatus;
    }}};
    const fileRow = element();
    bindClipboardPreview(fileRow,{kind:"file",id:"large-file"});
    let finishMetadata;
    window.__TAURI__.core.invoke = async (command, args) => {
        assert.equal(command,"get_clipboard_file_entry");
        if (args.includeMetadata) return new Promise(resolve => {finishMetadata = resolve;});
        return {paths:[filePath]};
    };
    await fileRow.fire("pointermove",{clientX:50});
    t.mock.timers.tick(350);
    for (let i=0;i<5;i++) await Promise.resolve();
    assert.equal(finishMetadata,undefined,"size probing does not delay first image paint");
    previewImageForTest().onload();
    const fileCaption = panel.children[0].children[1];
    finishMetadata({size_bytes:4117749});
    for (let i=0;i<5;i++) await Promise.resolve();
    assert.ok(fileCaption.textContent.includes("3.9 MB"));
    hideClipboardPreview();

    await fileRow.fire("pointermove",{clientX:50});
    t.mock.timers.tick(350);
    for (let i=0;i<5;i++) await Promise.resolve();
    previewImageForTest().onload();
    const staleCaption = panel.children[0].children[1];
    hideClipboardPreview();
    finishMetadata({size_bytes:4117749});
    for (let i=0;i<5;i++) await Promise.resolve();
    assert.ok(!staleCaption.textContent.includes("MB"),"late metadata cannot update a closed viewer");
    window.__TAURI__.core.invoke = async command => command === "get_clipboard_file_entry"
        ? {paths:[filePath]} : fileStatus;
    const openFilePreview = async () => {
        hideClipboardPreview();
        await fileRow.fire("pointermove",{clientX:50});
        t.mock.timers.tick(350);
        for (let i=0;i<5;i++) await Promise.resolve();
        previewImageForTest().onerror();
        for (let i=0;i<5;i++) await Promise.resolve();
    };
    await openFilePreview();
    assert.ok(panel.textContent.startsWith(filePath));
    assert.ok(panel.textContent.includes("200"),"backend animation rejection has a specific explanation");
    fileStatus = "ok";
    await openFilePreview();
    assert.ok(panel.textContent.includes("解码失败"),"WebView rejection after successful preflight is not reported as ok");
    fileStatus = "not_image";
    await openFilePreview();
    assert.equal(panel.textContent, filePath, "ordinary unsupported files show only their path, without an image-format error");
    for (const [reason, hint] of [["missing", "不存在"], ["unc", "网络路径"], ["decode_failed", "解码失败"]]) {
        fileStatus = reason;
        await openFilePreview();
        assert.ok(panel.textContent.startsWith(filePath));
        assert.ok(panel.textContent.includes(hint), "specific failures remain visible: " + reason);
    }

    // A delayed failure explanation must not replace a different preview.
    let resolveStatus;
    window.__TAURI__.core.invoke = async (command) => command === "get_clipboard_file_entry"
        ? {paths:[filePath]} : new Promise(resolve => {resolveStatus = resolve;});
    await openFilePreview();
    hideClipboardPreview();
    const newErrorRow = element();
    bindErrorPreview(newErrorRow,"new preview");
    await newErrorRow.fire("pointermove",{clientX:50});
    t.mock.timers.tick(350);
    for (let i=0;i<5;i++) await Promise.resolve();
    resolveStatus("animation_budget");
    for (let i=0;i<5;i++) await Promise.resolve();
    assert.equal(panel.textContent,"new preview");

    hideClipboardPreview();
    await imageRow.fire("pointermove",{clientX:50});
    t.mock.timers.tick(350);
    previewImageForTest().onload();
    const viewport = panel.children[0].children[0];
    await viewport.fire("wheel",{deltaY:-400,clientX:150,clientY:50});
    await viewport.fire("pointerdown",{button:0,pointerId:1,clientX:150,clientY:50});
    await imageRow.fire("pointerleave");
    await panel.fire("pointerleave");
    await first.fire("pointermove",{clientX:50,buttons:1});
    t.mock.timers.tick(500);
    assert.ok(!panel.classList.contains("hidden"),"dragging outside the preview retains the image and blocks crossed rows");
    await viewport.fire("pointercancel",{pointerId:1});
    t.mock.timers.tick(149);
    assert.ok(!panel.classList.contains("hidden"));
    t.mock.timers.tick(1);
    assert.ok(panel.classList.contains("hidden"),"cancellation restores the normal exit buffer");
});
