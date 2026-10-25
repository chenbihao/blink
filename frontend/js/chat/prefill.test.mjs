import assert from "node:assert/strict";
import {test} from "node:test";
import {createPrefillConsumer} from "./prefill.js";

const deferred = () => Promise.withResolvers();
function setup(overrides = {}) {
    const calls = {new: 0, text: [], adopt: 0, add: 0};
    const view = {id: "old"};
    const consume = createPrefillConsumer({
        getConversationId: () => view.id,
        newConversation: async () => { calls.new++; view.id = "new"; },
        insertText: text => calls.text.push(text),
        adopt: async refs => { calls.adopt++; return refs.length; },
        add: () => { calls.add++; return {rejected: []}; },
        ...overrides,
    });
    return {consume, calls, view};
}
const payload = {text: "draft", attachments: [{kind: "image", ref: "rref_a", display: "a.png"}], newConversation: true};

test("event/take share the in-flight revision, including new-conversation await", async () => {
    const gate = deferred();
    const {consume, calls} = setup({newConversation: () => gate.promise});
    const event = consume(payload, 1);
    const take = consume(payload, 1);
    assert.equal(event, take);
    gate.resolve();
    assert.equal(await event, true);
    assert.equal(await take, true);
    assert.deepEqual(calls.text, ["draft"]);
    assert.equal(calls.adopt, 1);
    assert.equal(await consume(payload, 1), true);
    assert.equal(calls.adopt, 1);
});

test("partial/failed adoption never acknowledges; replay retries without inserting text", async () => {
    let attempts = 0;
    const {consume, calls} = setup({adopt: async () => ++attempts === 1 ? 0 : 1});
    assert.equal(await consume(payload, 2), false);
    assert.equal(calls.add, 0);
    assert.equal(await consume(payload, 2), true);
    assert.equal(attempts, 2);
    assert.equal(calls.new, 1);
    assert.deepEqual(calls.text, ["draft"]);
});

test("replay cannot move a failed draft into a different conversation", async () => {
    const {consume, calls, view} = setup({adopt: async () => 0});
    assert.equal(await consume(payload, 3), false);
    view.id = "other";
    assert.equal(await consume(payload, 3), false);
    assert.deepEqual(calls.text, ["draft"]);
    assert.equal(calls.new, 1);
});

test("different revisions serialize and late older events cannot reinsert", async () => {
    const gate = deferred();
    let attempts = 0;
    const {consume, calls} = setup({adopt: async () => { if (++attempts === 1) await gate.promise; return 1; }});
    const a = consume(payload, 4);
    const b = consume({...payload, text: "next"}, 5);
    await Promise.resolve();
    await Promise.resolve();
    assert.deepEqual(calls.text, ["draft"]);
    gate.resolve();
    assert.deepEqual(await Promise.all([a, b]), [true, true]);
    assert.equal(await consume(payload, 4), true);
    assert.deepEqual(calls.text, ["draft", "next"]);
});

test("navigation during new-conversation preparation cannot receive the text or attachments", async () => {
    const gate = deferred();
    const h = setup({newConversation: () => gate.promise});
    const result = h.consume(payload, 6);
    await Promise.resolve();
    h.view.id = "user-selected";
    gate.resolve();
    assert.equal(await result, false);
    assert.deepEqual(h.calls.text, []);
    assert.equal(h.calls.adopt, 0);
});

test("composer overflow is retryable even when resources were already adopted", async () => {
    let attempts = 0;
    const h = setup({add: () => ({rejected: ++attempts === 1 ? payload.attachments : []})});
    assert.equal(await h.consume(payload, 7), false);
    assert.equal(await h.consume(payload, 7), true);
    assert.deepEqual(h.calls.text, ["draft"]);
    assert.equal(h.calls.adopt, 2);
});
