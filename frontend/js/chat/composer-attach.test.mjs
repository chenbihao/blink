import assert from "node:assert/strict";

// ── Tauri / window 桩（须在动态 import 前就位）──────────────────────────────
// composer.js → ipc.js → tauri.js（模块顶层写 window.alert 等）

const invokeCalls = [];
const removedRefs = [];
/** picker 返回控制：函数 → 值/Promise；null → 用户取消 */
let pickHandler = null;

globalThis.window = globalThis.window || {};
globalThis.window.__TAURI__ = {
    core: {
        invoke: async (cmd, args) => {
            invokeCalls.push({cmd, args});
            if (cmd === "pick_chat_audio_attachment") {
                return pickHandler ? pickHandler(args) : null;
            }
            if (cmd === "remove_chat_audio_attachment") {
                removedRefs.push(args.audioRef);
                return true;
            }
            return {};
        },
    },
    event: {
        listen: async () => ({unlisten: () => {}}),
    },
};

// ── 最小 DOM 桩（composer 用到的面）────────────────────────────────────────

function makeElement(tag = "div") {
    const listeners = {};
    const el = {
        tagName: tag.toUpperCase(),
        children: [],
        listeners,
        className: "",
        title: "",
        textContent: "",
        disabled: false,
        hidden: false,
        value: "",
        readOnly: false,
        scrollHeight: 0,
        type: "",
        style: {},
        dataset: {},
        classList: {
            _set: new Set(),
            add(c) {
                this._set.add(c);
            },
            remove(c) {
                this._set.delete(c);
            },
            contains(c) {
                return this._set.has(c);
            },
            toggle(c, force) {
                const want = force === undefined ? !this._set.has(c) : !!force;
                if (want) this._set.add(c);
                else this._set.delete(c);
                return want;
            },
        },
        addEventListener(type, fn) {
            (listeners[type] ||= []).push(fn);
        },
        removeEventListener() {},
        setAttribute() {},
        append(...nodes) {
            el.children.push(...nodes);
        },
        appendChild(node) {
            el.children.push(node);
            return node;
        },
        focus() {},
        setSelectionRange() {},
        querySelectorAll() {
            return [];
        },
        click() {
            for (const fn of listeners.click || []) {
                fn({preventDefault() {}, stopPropagation() {}});
            }
        },
    };
    // renderAudioAttachments 以 innerHTML = "" 清空容器——赋空值时同步清 children
    let innerHTMLValue = "";
    Object.defineProperty(el, "innerHTML", {
        get: () => innerHTMLValue,
        set: (v) => {
            innerHTMLValue = String(v);
            if (innerHTMLValue === "") el.children.length = 0;
        },
        configurable: true,
    });
    return el;
}

const elements = new Map();
const register = (id, el) => (elements.set(id, el), el);

const textareaEl = register("chat-input", makeElement("textarea"));
const sendBtnEl = register("chat-send-btn", makeElement("button"));
register("chat-thinking-btn", makeElement("button"));
const attachBtnEl = register("chat-attach-btn", makeElement("button"));
const attachmentsEl = register("chat-audio-attachments", makeElement("div"));

globalThis.document = {
    getElementById: (id) => elements.get(id) ?? null,
    createElement: (tag) => makeElement(tag),
    addEventListener() {},
    removeEventListener() {},
};

async function flush() {
    for (let i = 0; i < 5; i++) {
        await new Promise((resolve) => setImmediate(resolve));
    }
}

// ── 被测模块（mock 就位后动态 import）──────────────────────────────────────

const composer = await import("./composer.js");

const REF_A = `rref_${"a".repeat(32)}`;
const REF_B = `rref_${"b".repeat(32)}`;

let currentConv = "conv-1";
const sent = [];

composer.initComposer({
    onSend: (payload) => sent.push(payload),
    onStop: () => {},
    // 0.23.14：会话 id 由 main.js 注入——composer 不再依赖未导入的全局绑定
    getConversationId: () => currentConv,
});

// ── 行为 1：正常 attach ────────────────────────────────────────────────────

pickHandler = async (args) => {
    assert.equal(args.conversationId, "conv-1", "picker 应收到发起时的会话 id");
    return {audioRef: REF_A, displayName: "录音.wav"};
};

attachBtnEl.click();
await flush();

assert.equal(composer.getAudioAttachments().length, 1, "正常 attach 应进入附件列表");
assert.equal(composer.getAudioAttachments()[0].displayName, "录音.wav");
assert.equal(attachmentsEl.children.length, 1, "应渲染一个附件 chip");
assert.equal(attachBtnEl.disabled, false, "attach 完成后按钮应恢复可用");
assert.deepEqual(removedRefs, [], "正常 attach 不应触发撤销");

// ── 行为 2：发送负载 = 可见正文 + 结构化附件（模型输入分离由后端组装）──────

textareaEl.value = "转写这个附件";
sendBtnEl.click();
await flush();

assert.equal(sent.length, 1, "应触发一次发送");
assert.equal(sent[0].text, "转写这个附件", "气泡/标题/历史用的是可见正文");
assert.deepEqual(
    sent[0].attachments,
    [{audioRef: REF_A, displayName: "录音.wav"}],
    "附件以结构化元数据随本轮请求发送",
);

// ── 行为 3：纯附件发送（空正文）→ 人类可读摘要，无 rref_ ───────────────────

textareaEl.value = "";
sendBtnEl.click();
await flush();

assert.equal(sent.length, 2);
assert.equal(sent[1].text, "（音频附件：录音.wav）", "空正文时摘要可读且不含技术提示");
assert.ok(!sent[1].text.includes("rref_"));

// ── 行为 4：picker 返回前切换会话 → 撤销刚签发的 ref，附件不落入新会话 ─────

composer.clearAudioAttachments();
removedRefs.length = 0;
sent.length = 0;

let resolvePick;
pickHandler = () => new Promise((resolve) => {
    resolvePick = resolve;
});

attachBtnEl.click();
await flush(); // 让 handleAttachAudio 停在 await picker 上

currentConv = "conv-2"; // picker 打开期间用户切换了会话
resolvePick({audioRef: REF_B, displayName: "旧会话.wav"});
await flush();

assert.equal(composer.getAudioAttachments().length, 0, "切换会话后旧附件不得落入新会话");
assert.equal(attachmentsEl.children.length, 0, "不应渲染旧会话的 chip");
assert.deepEqual(removedRefs, [REF_B], "picker 返回后会话已变，应立即撤销刚签发的 ref");
assert.equal(sent.length, 0);

// ── 行为 5：用户取消 picker → 无附件、无撤销 ───────────────────────────────

pickHandler = () => null;
attachBtnEl.click();
await flush();

assert.equal(composer.getAudioAttachments().length, 0);
assert.deepEqual(removedRefs, [REF_B], "取消选择不应产生新的撤销调用");

// ── 行为 6：发送后已发送附件离开输入框（不撤销 ref，新附文件不受影响）──────

const REF_C = `rref_${"c".repeat(32)}`;
pickHandler = () => ({audioRef: REF_A, displayName: "已发送.wav"});
attachBtnEl.click();
await flush();
pickHandler = () => ({audioRef: REF_C, displayName: "留着的.wav"});
attachBtnEl.click();
await flush();

assert.equal(composer.getAudioAttachments().length, 2);
removedRefs.length = 0;

// main.js 发送成功路径的调用形态：只清本次发出的那条
composer.clearSentAttachments([{audioRef: REF_A}]);

assert.equal(composer.getAudioAttachments().length, 1, "已发送的附件应离开输入框");
assert.equal(composer.getAudioAttachments()[0].audioRef, REF_C, "发送间隙新附的文件应保留");
assert.equal(attachmentsEl.children.length, 1);
assert.deepEqual(removedRefs, [], "发送后移除 chip 不撤销后端 ref（本轮 agent 仍要消费）");

// 空入参 no-op
composer.clearSentAttachments([]);
assert.equal(composer.getAudioAttachments().length, 1);

console.log("Chat composer attach tests passed");
