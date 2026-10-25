import assert from "node:assert/strict";
import {test} from "node:test";

const requests = [];
globalThis.window = {__TAURI__: {core: {invoke: (cmd, args) => {
    const response = Promise.withResolvers();
    requests.push({cmd, args, ...response});
    return response.promise;
}}}};
const state = await import("./state.js");
const {setThinkingModelId, applyThinkingEffort} = await import("./thinking-selector.js");

test("a response from model A cannot change model B, including A→B→A", async () => {
    setThinkingModelId("A");
    const save = applyThinkingEffort("high");
    assert.equal(requests.at(-1).args.selectionId, "A");
    setThinkingModelId("B");
    state.setThinkingEffort("medium");
    setThinkingModelId("A");
    state.setThinkingEffort("low");
    requests.at(-1).resolve(true);
    await save;
    assert.equal(state.thinkingEffort, "low");
});

test("out-of-order consecutive saves only apply the latest intent", async () => {
    setThinkingModelId("B");
    const first = applyThinkingEffort("low");
    const r1 = requests.at(-1);
    const second = applyThinkingEffort("high");
    requests.at(-1).resolve(true);
    await second;
    assert.equal(state.thinkingEffort, "high");
    r1.resolve(true);
    await first;
    assert.equal(state.thinkingEffort, "high");
});
