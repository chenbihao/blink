//! 0.26.17：args.hints 引导项的 Route 动作——回写输入框、光标置尾、滚动到
//! 末尾（spec-frontend §5.8）并派发 input 走一轮新搜索。与建议 RouteQuery 采纳
//! （suggestion-accept.js）同语义，此处只测结果项激活入口。

import assert from 'node:assert/strict';
import {test} from 'node:test';

globalThis.window ??= {};

const inputEvents = [];
const queryEl = {
    value: "",
    selection: null,
    scrollWidth: 240,
    clientWidth: 120,
    scrollLeft: 0,
    setSelectionRange(start, end) { this.selection = [start, end]; },
    dispatchEvent(ev) { inputEvents.push({type: ev.type, value: this.value}); },
};
globalThis.document = {getElementById: (id) => (id === "query" ? queryEl : null)};

const {activateItem} = await import("./actions.js");

test("route 动作回写目标查询、光标置尾、滚动到末尾并派发 input", async () => {
    await activateItem({actions: [{kind: "route", payload: "ip refresh "}]});
    assert.equal(queryEl.value, "ip refresh ");
    // 光标在末尾 + 显式滚动（§5.8：裸 value= 重置 scrollLeft，浏览器不跟随光标）
    assert.deepEqual(queryEl.selection, ["ip refresh ".length, "ip refresh ".length]);
    assert.equal(queryEl.scrollLeft, queryEl.scrollWidth);
    const last = inputEvents.at(-1);
    assert.equal(last.type, "input");
    assert.equal(last.value, "ip refresh ");
});

test("route 缺 payload 静默不派发（防御：后端契约保证，前端不重算）", async () => {
    const before = inputEvents.length;
    queryEl.value = "unchanged";
    queryEl.scrollLeft = 7;
    await activateItem({actions: [{kind: "route"}]});
    assert.equal(inputEvents.length, before);
    assert.equal(queryEl.value, "unchanged");
    assert.equal(queryEl.scrollLeft, 7);
});

test("错误项不可执行（route 亦不例外）", async () => {
    const before = inputEvents.length;
    await activateItem({isError: true, actions: [{kind: "route", payload: "ip refresh "}]});
    assert.equal(inputEvents.length, before);
});
