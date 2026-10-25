import assert from "node:assert/strict";
import {test} from "node:test";
import {readFileSync} from "node:fs";
import {createTurnRetryStore, resolvePersistedTurn} from "./turn-retry.js";

test("retry cache is bounded, expires and never persists the reference in message state", () => {
    let time = 0;
    const store = createTurnRetryStore({limit: 2, ttl: 100, now: () => time});
    const attachments = [{kind: "image", ref: "rref_original", display: "a.png"}];
    const badges = [{kind: "image", name: "a.png"}];
    store.remember("a", attachments);
    attachments[0].ref = "rref_mutated";
    assert.equal(store.get("a", badges)[0].ref, "rref_original");
    store.remember("b", attachments);
    store.remember("c", attachments);
    assert.equal(store.get("a", badges), null);
    time = 100;
    assert.equal(store.get("c", badges), null);
    store.clear();
    assert.equal(store.get("b", badges), null);
    assert.deepEqual(store.get("text-only", []), []);
});

const messages = [
    {id: "image", role: "user", content: "（图片附件）", attachments: [{kind: "image", name: "a.png"}]},
    {id: "reply", role: "assistant", content: "reply"},
    {id: "text", role: "user", content: "edit me"},
];
const snapshots = [
    {message_id: 10, role: "user", text: "（图片附件）"},
    {message_id: 11, role: "assistant", text: "", tool_name: "inspect"},
    // DB row 12 is a tool-result delivery and is omitted by the projection.
    {message_id: 13, role: "assistant", text: "reply"},
    {message_id: 14, role: "user", text: "edit me"},
];
test("attachment-only users and hidden tool rows resolve to the exact DB user id", () => {
    assert.equal(resolvePersistedTurn(messages, "text", snapshots), 14);
    assert.equal(resolvePersistedTurn(messages, "image", snapshots), 10);
    assert.throws(() => resolvePersistedTurn(messages, "missing", snapshots));
    assert.throws(() => resolvePersistedTurn(messages, "text", [...snapshots, {role: "user", text: "external"}]));
    assert.throws(() => resolvePersistedTurn(messages.map(m => m.id === "text" ? {...m, messageId: 99} : m), "text", snapshots));
});

// 执行 main 的真实协调函数，用受控 IPC 验证先校验后截断、完整 payload 重发。
const main = readFileSync(new URL("./main.js", import.meta.url), "utf8").replace(/\r\n/g, "\n");
const editSource = main.slice(main.indexOf("async function handleEditMessage("), main.indexOf("/** 气泡直接携带"));
const retrySource = main.slice(main.indexOf("async function handleRetry("), main.indexOf("// ── 停止"));
function harness({expired = false, missingSnapshot = false} = {}) {
    const state = {conversationId: "c1", messages: structuredClone(messages), isStreaming: false, ephemeralMode: false};
    const turns = createTurnRetryStore();
    if (!missingSnapshot) turns.remember("image", [{kind: "image", ref: "rref_original", display: "a.png"}]);
    const calls = [];
    const ipc = {
        attachmentListCandidates: async () => ({sessionRefs: expired ? [] : [{ref: "rref_original"}]}),
        getChatMessages: async () => snapshots,
        truncateMessages: async (...args) => {calls.push(["truncate", ...args]);},
    };
    const components = {
        clearMessages() {}, finalizeAssistantMessage() {}, createAssistantMessage() {},
        removeSummarySeparator() {}, updateContextIndicator() {},
        renderSignal() {}, renderErrorMessage: text => calls.push(["error", text]),
    };
    const create = new Function("state", "turnRetries", "ipc", "components", "resolvePersistedTurn", "currentModelSupportsVision", "setInputValue", "focusInput", "t", "handleStop", "renderUserEntry", "addAttachments", "handleSend", "invalidateComposerBarCache", `let editingTurn = false; ${editSource}\n${retrySource}; return {handleEditMessage,handleRetry};`);
    const functions = create(state, turns, ipc, components, resolvePersistedTurn, () => true,
        text => calls.push(["draft", text]), () => {}, key => key, async () => {}, () => {},
        attachments => calls.push(["add", attachments]), async payload => calls.push(["send", payload]), () => {});
    return {state, calls, ...functions};
}

test("actual edit path reuses image refs and truncates the image user row", async () => {
    const h = harness();
    await h.handleEditMessage("image", "analyze again");
    assert.deepEqual(h.calls[0], ["truncate", "c1", 0, 10, null]);
    assert.deepEqual(h.calls.find(c => c[0] === "send")[1], {
        text: "analyze again", attachments: [{kind: "image", ref: "rref_original", display: "a.png"}],
    });
    assert.equal(h.state.messages.length, 0);
});

test("expired and history-only attachments preserve history and request reattachment", async () => {
    for (const options of [{expired: true}, {missingSnapshot: true}]) {
        const h = harness(options);
        await h.handleEditMessage("image", "retry text");
        assert.equal(h.state.messages.length, 3);
        assert.ok(h.calls.some(c => c[0] === "error"));
        assert.ok(h.calls.some(c => c[0] === "draft" && c[1] === "retry text"));
        assert.ok(!h.calls.some(c => c[0] === "truncate" || c[0] === "send"));
    }
});

test("actual retry path uses the anchor's user id; stale anchors never fall back", async () => {
    const h = harness();
    await h.handleRetry({detail: {msgEl: {dataset: {userMessageId: "missing"}}}});
    assert.equal(h.calls.length, 0);
    await h.handleRetry({detail: {msgEl: {dataset: {userMessageId: "text"}}}});
    assert.deepEqual(h.calls[0], ["truncate", "c1", 0, 14, null]);
    assert.equal(h.calls.find(c => c[0] === "send")[1].text, "edit me");
});

const sendSource = main.slice(main.indexOf("async function handleSend("), main.indexOf("// ── 消息编辑重发"));
function sendHarness({failed = false, reject = false} = {}) {
    const messages = failed ? [{id: "failed-user", role: "user", content: "retry", failed: true, attachments: []}] : [];
    const calls = [];
    const state = {
        messages, conversationId: "c1", currentGroupId: null, ephemeralMode: false, supportsEffort: false,
        addMessage(msg) { msg.id = "new-user"; messages.push(msg); return msg; },
        setStreaming() {}, resetStreamBuffer() {}, setActiveRequestId() {},
    };
    const components = {renderErrorMessage() {}, removeEmptyState() {}, createAssistantMessage() { return null; }};
    const ports = {
        state, components, turnRetries: createTurnRetryStore(), currentModelSupportsVision: () => true,
        renderUserEntry() {}, forceScrollToBottom() {}, setStreamingMode() {}, effectiveThinkingEnabled: () => true,
        earlyStreamBuffer: {begin() {}, resolve: () => [], clear() {}},
        lastUserId: () => messages.findLast(m => m.role === "user")?.id,
        ipc: {
            async chatPrompt(...args) { calls.push(args); if (reject) throw new Error("start rejected"); return 123; },
            async getChatMessages() { return []; },
            async renameChatConversation() {}, async generateConversationTitle() {},
        },
        dispatchStreamPayload() {}, updateBreadcrumb() {}, refreshSidebar() {}, clearInput() {},
        clearSentAttachments() {}, setActiveConversation() {}, finishStreaming() {}, t: key => key,
        document: {querySelectorAll: () => [], getElementById: () => null}, resolvePersistedTurn,
        console: {error() {}, warn() {}},
    };
    const send = new Function(...Object.keys(ports), `let currentAssistantEl = null; ${sendSource}; return handleSend;`)(...Object.values(ports));
    return {send, calls, state};
}

test("confirming a prefilled failed turn reuses its bubble and includes the new attachments", async () => {
    const h = sendHarness({failed: true});
    const attachments = [{kind: "image", ref: "rref_replacement", display: "replacement.png"}];
    await h.send({text: "retry", attachments});
    assert.equal(h.state.messages.length, 1);
    assert.equal(h.state.messages[0].id, "failed-user");
    assert.equal(h.state.messages[0].failed, false);
    assert.deepEqual(h.state.messages[0].attachments, [{kind: "image", name: "replacement.png"}]);
    assert.deepEqual(h.calls[0][3].attachments, attachments);
    assert.ok(!JSON.stringify(h.state.messages).includes("rref_"));
});

test("a rejected start with no DB prewrite rolls back the optimistic user", async () => {
    const h = sendHarness({reject: true});
    await h.send("retry");
    assert.equal(h.state.messages.length, 0);
});

test("stream errors mark the failed user and retain a partial assistant in state", () => {
    const start = main.indexOf('        case "error": {');
    const end = main.indexOf("\n    }\n}", start);
    const errorCase = main.slice(start, end);
    for (const partial of ["", "partial reply"]) {
        const state = {messages: [{role: "user", content: "retry"}], streamBuffer: partial, thinkingBuffer: "",
            addMessage(msg) { this.messages.push(msg); }};
        const dispatch = new Function("state", "components", "finishStreaming", "chunk", `let currentAssistantEl = {remove() {}}; switch (chunk.type) {${errorCase}}`);
        dispatch(state, {finalizeAssistantMessage() {}, renderErrorMessage() {}}, () => {}, {type: "error", message: "failed"});
        assert.equal(state.messages[0].failed, true);
        assert.equal(state.messages.length, partial ? 2 : 1);
        if (partial) assert.equal(state.messages[1].content, partial);
    }
});
