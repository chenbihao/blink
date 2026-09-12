import test from "node:test";
import assert from "node:assert/strict";
import {readFileSync} from "node:fs";

const editorHtml = readFileSync(new URL("../../content-editor.html", import.meta.url), "utf8");
const overlayHtml = readFileSync(new URL("../../voice-overlay.html", import.meta.url), "utf8");
const editorCss = readFileSync(new URL("../../css/views/content-editor.css", import.meta.url), "utf8");
const mainJs = readFileSync(new URL("./main.js", import.meta.url), "utf8");

test("editor: view switch exposes tab semantics and keyboard navigation", () => {
    assert.match(editorHtml, /role="tablist"/);
    assert.equal((editorHtml.match(/role="tab"/g) ?? []).length, 2);
    assert.match(editorHtml, /aria-selected="true"/);
    assert.match(mainJs, /"ArrowLeft", "ArrowRight", "Home", "End"/);
    assert.match(mainJs, /setAttribute\("aria-selected"/);
});

test("editor: icon-only controls and transient status have accessible names", () => {
    for (const id of ["titlebar-minimize", "titlebar-maximize", "titlebar-close", "btn-more", "btn-mic"]) {
        const tag = editorHtml.match(new RegExp(`<button[^>]*id="${id}"[^>]*>`))?.[0] ?? "";
        assert.match(tag, /(?:aria-label|data-i18n-aria-label)=/, `${id} should have an accessible name`);
    }
    assert.match(editorHtml, /id="editor-status"[^>]*role="status"/);
    assert.match(overlayHtml, /aria-live="polite"[^>]*id="partial-text"[^>]*role="status"/);
});

test("editor: keyboard users receive visible focus and menu state", () => {
    assert.match(editorCss, /\.editor-more-btn:focus-visible/);
    assert.match(editorCss, /outline:\s*2px solid var\(--accent\)/);
    assert.match(mainJs, /bindMenuTrigger\(moreBtn\)/);
    assert.match(mainJs, /closeMenu\(\{restoreFocus: true\}\)/);
});
