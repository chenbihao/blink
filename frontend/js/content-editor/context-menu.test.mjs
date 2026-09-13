import test from "node:test";
import assert from "node:assert/strict";

globalThis.window = globalThis;
const {editorContextItems, clampEditorMenuPosition, bindEditorContextMenu} = await import("./context-menu.js");

test("editor right-click only offers tidy for an editable selection with idle AI", () => {
    const base = {hasSelection: true, hasMeaningfulSelection: true, hasText: true, editable: true, aiAvailable: true, transformIdle: true};
    assert.deepEqual(editorContextItems(base), ["cut", "copy", "paste", "selectAll", "separator", "tidySelection"]);
    assert.ok(!editorContextItems({...base, hasSelection: false, hasMeaningfulSelection: false}).includes("tidySelection"));
    assert.deepEqual(editorContextItems({...base, hasMeaningfulSelection: false}), ["cut", "copy", "paste", "selectAll"]);
    assert.ok(!editorContextItems({...base, aiAvailable: false}).includes("tidySelection"));
    assert.ok(!editorContextItems({...base, transformIdle: false}).includes("tidySelection"));
    assert.deepEqual(editorContextItems({...base, editable: false}), ["copy", "selectAll"]);
});

test("editor right-click keeps the menu inside the editor at all four edges", () => {
    assert.deepEqual(clampEditorMenuPosition(300, 200, 180, 120, 400, 300), {x: 216, y: 176});
    assert.deepEqual(clampEditorMenuPosition(-20, -10, 180, 120, 400, 300), {x: 4, y: 4});
});

test("editor selection right-click enters the existing transform flow without changing text", () => {
    class Element {
        constructor(tag = "div") {
            this.tag = tag;
            this.children = [];
            this.handlers = new Map();
            this.style = {};
            this.attributes = {};
        }
        addEventListener(name, fn) { this.handlers.set(name, fn); }
        dispatch(name, event) { this.handlers.get(name)?.(event); }
        appendChild(child) { this.children.push(child); child.parent = this; }
        remove() { this.parent.children = this.parent.children.filter((child) => child !== this); }
        contains(target) { return this === target || this.children.some((child) => child.contains(target)); }
        setAttribute(name, value) { this.attributes[name] = value; }
        getBoundingClientRect() { return {left: 0, top: 0, width: 500, height: 300}; }
        querySelectorAll(tag) { return this.children.filter((child) => child.tag === tag); }
        querySelector(tag) { return this.querySelectorAll(tag)[0]; }
    }
    const root = new Element();
    const source = new Element("textarea");
    source.selectionStart = 2;
    source.selectionEnd = 4;
    const doc = new Element();
    doc.createElement = (tag) => new Element(tag);
    doc.querySelector = () => null;
    globalThis.document = doc;
    window.addEventListener = () => {};
    const text = "abcdef";
    const calls = [];
    const adapter = {
        view: "source", revision: 1,
        getText: () => text,
        getSelectionText: () => text.slice(source.selectionStart, source.selectionEnd),
        isEditable: () => true,
    };
    bindEditorContextMenu({
        root, source, markdown: null, adapter,
        session: {isActive: true, sessionRef: "s1", generation: 1},
        transform: {aiAvailable: true, isBusy: false, candidate: null, start: (scope) => calls.push(scope)},
        setStatus: () => {},
    });
    root.dispatch("contextmenu", {
        target: source, clientX: 20, clientY: 20, button: 2,
        preventDefault() {},
    });
    const menu = root.children[0];
    assert.equal(menu.attributes.role, "menu");
    menu.children.at(-1).dispatch("click", {});
    assert.deepEqual(calls, ["selection"]);
    assert.equal(text, "abcdef");
    assert.equal(root.children.length, 0);
});
