import assert from "node:assert/strict";
import {
    markSettingsTabActivation,
    navigateSettings,
    resetSettingsContentScroll,
} from "./navigation.js";

function makeDocument({target = null, focusTarget = null} = {}) {
    const content = {scrollTop: 640};
    const tab = {
        dataset: {tab: "engines"},
        clickCount: 0,
        click() {
            this.clickCount++;
            markSettingsTabActivation();
        },
    };
    return {
        body: {},
        content,
        tab,
        querySelectorAll(selector) {
            return selector === ".tab" ? [tab] : [];
        },
        querySelector(selector) {
            if (selector === ".content") return content;
            if (selector === "#target") return target;
            if (selector === "#focus") return focusTarget;
            return null;
        },
    };
}

{
    const documentRef = makeDocument();
    resetSettingsContentScroll(documentRef);
    assert.equal(documentRef.content.scrollTop, 0);
}

{
    const calls = [];
    const target = {
        scrollIntoView(options) {
            calls.push(["scroll", options]);
        },
    };
    const focusTarget = {
        focus(options) {
            calls.push(["focus", options]);
        },
    };
    const documentRef = makeDocument({target, focusTarget});

    const result = await navigateSettings({
        tabId: "engines",
        target: "#target",
        focusTarget: "#focus",
        prepare: () => calls.push(["prepare"]),
        documentRef,
    });

    assert.equal(result.activated, true);
    assert.equal(documentRef.tab.clickCount, 1);
    assert.equal(documentRef.content.scrollTop, 0);
    assert.deepEqual(calls, [
        ["prepare"],
        ["scroll", {behavior: "smooth", block: "start"}],
        ["focus", {preventScroll: true}],
    ]);
}

{
    const documentRef = makeDocument();
    const result = await navigateSettings({tabId: "missing", documentRef});
    assert.deepEqual(result, {activated: false, target: null});
}

{
    const calls = [];
    let finishPrepare;
    const documentRef = makeDocument({
        target: {scrollIntoView: () => calls.push("scroll")},
    });
    const navigation = navigateSettings({
        tabId: "engines",
        target: "#target",
        prepare: () => new Promise((resolve) => {
            finishPrepare = resolve;
        }),
        documentRef,
    });

    markSettingsTabActivation(); // 模拟用户在异步准备期间切到另一 Tab
    finishPrepare();
    const result = await navigation;
    assert.equal(result.stale, true);
    assert.deepEqual(calls, [], "过期导航不得滚动已经切走的 panel");
}

{
    let target = null;
    let observerCallback = null;
    let timerCleared = false;
    class FakeMutationObserver {
        constructor(callback) {
            observerCallback = callback;
        }
        observe() {}
        disconnect() {}
    }
    const documentRef = makeDocument();
    documentRef.defaultView = {
        MutationObserver: FakeMutationObserver,
        setTimeout: () => 7,
        clearTimeout: (timer) => {
            assert.equal(timer, 7);
            timerCleared = true;
        },
    };
    documentRef.querySelector = (selector) => {
        if (selector === ".content") return documentRef.content;
        if (selector === "#late-target") return target;
        return null;
    };

    const navigation = navigateSettings({
        tabId: "engines",
        target: "#late-target",
        documentRef,
    });
    target = {scrollIntoView() {}};
    observerCallback();

    const result = await navigation;
    assert.equal(result.target, target, "异步渲染目标出现后应由 MutationObserver 完成定位");
    assert.equal(timerCleared, true, "目标出现后应清理超时兜底");
}

console.log("settings navigation tests passed");
