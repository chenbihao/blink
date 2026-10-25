import assert from "node:assert/strict";

// ── Tauri / window 桩（须在动态 import 前就位）──────────────────────────────
// composer.js → ipc.js → tauri.js（模块顶层写 window.alert 等）

const invokeCalls = [];
const removedRefs = [];

globalThis.window = globalThis.window || {};
globalThis.window.__TAURI__ = {
    core: {
        invoke: async (cmd, args) => {
            invokeCalls.push({cmd, args});
            if (cmd === "remove_chat_attachment") {
                removedRefs.push(args.attachmentRef);
                return true;
            }
            return {};
        },
    },
    event: {
        listen: async () => ({unlisten: () => {}}),
    },
};

// ── 最小 DOM 桩（composer / attachment-palette 用到的面）──────────────────

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
    // renderAttachments 以 innerHTML = "" 清空容器——赋空值时同步清 children
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
register("chat-attach-btn", makeElement("button"));
const attachmentsEl = register("chat-audio-attachments", makeElement("div"));
register("chat-attachment-palette", makeElement("div"));

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
const REF_TXT = `rref_${"t".repeat(32)}`;

const sent = [];

composer.initComposer({
    onSend: (payload) => sent.push(payload),
    onStop: () => {},
    getConversationId: () => "conv-1",
});

// ── 行为 1：addAttachments 类型化添加 + 去重 + 非法 ref 拒绝 ───────────────

composer.addAttachments([
    {kind: "text_file", ref: REF_TXT, display: "笔记.txt", truncated: true},
    {kind: "audio", ref: REF_A, display: "录音.wav"},
    {kind: "audio", ref: REF_A, display: "录音.wav"}, // 同 ref 去重
    {kind: "text_file", ref: "bearer-token", display: "坏.ref"}, // 非 rref_ 拒绝
]);
assert.equal(composer.getAttachments().length, 2, "同 ref 去重、非法 ref 拒绝");
assert.equal(composer.getAttachments()[0].kind, "text_file");
assert.equal(attachmentsEl.children.length, 2, "应渲染两个附件 chip");

// ── 行为 2：发送负载 = 可见正文 + 结构化附件（模型输入分离由后端组装）──────

textareaEl.value = "总结这个文件";
sendBtnEl.click();
await flush();

assert.equal(sent.length, 1, "应触发一次发送");
assert.equal(sent[0].text, "总结这个文件", "气泡/标题/历史用的是可见正文");
assert.deepEqual(
    sent[0].attachments,
    [
        {kind: "text_file", ref: REF_TXT, display: "笔记.txt"},
        {kind: "audio", ref: REF_A, display: "录音.wav"},
    ],
    "附件以 {kind, ref, display} 结构化元数据随本轮请求发送",
);

// ── 行为 3：纯附件发送（空正文）→ 人类可读摘要，无 rref_ ───────────────────

composer.clearAttachments();
sent.length = 0;
composer.addAttachments([{kind: "text_file", ref: REF_TXT, display: "笔记.txt"}]);
textareaEl.value = "";
sendBtnEl.click();
await flush();

assert.equal(sent.length, 1);
assert.equal(sent[0].text, "（文件附件：笔记.txt）", "空正文时摘要可读且不含技术提示");
assert.ok(!sent[0].text.includes("rref_"));

// ── 行为 4：会话切换清空（UI 态，不触发 revoke）────────────────────────────

removedRefs.length = 0;
composer.clearAttachments();
assert.equal(composer.getAttachments().length, 0);
assert.equal(attachmentsEl.children.length, 0);
assert.deepEqual(removedRefs, [], "clearAttachments 只清 UI，撤销由按 owner 批量命令负责");

// ── 行为 5：发送后已发送附件离开输入框（不撤销 ref，新附文件不受影响）──────

const REF_C = `rref_${"c".repeat(32)}`;
composer.addAttachments([{kind: "text_file", ref: REF_A, display: "已发送.txt"}]);
composer.addAttachments([{kind: "text_file", ref: REF_C, display: "留着的.txt"}]);
assert.equal(composer.getAttachments().length, 2);
removedRefs.length = 0;

// main.js 发送成功路径的调用形态：只清本次发出的那条（按 ref 匹配）
composer.clearSentAttachments([{ref: REF_A}]);
assert.equal(composer.getAttachments().length, 1, "已发送的附件应离开输入框");
assert.equal(composer.getAttachments()[0].ref, REF_C, "发送间隙新附的文件应保留");
assert.equal(attachmentsEl.children.length, 1);
assert.deepEqual(removedRefs, [], "发送后移除 chip 不撤销后端 ref（本轮 agent 仍要消费）");

// 空入参 no-op
composer.clearSentAttachments([]);
assert.equal(composer.getAttachments().length, 1);

// ── 行为 6：单轮上限 8 项（超出丢弃）───────────────────────────────────────

composer.clearAttachments();
const many = Array.from({length: 10}, (_, i) => ({
    kind: "text_file",
    ref: `rref_${String(i).padStart(32, "0")}`,
    display: `f${i}.txt`,
}));
composer.addAttachments(many);
assert.equal(composer.getAttachments().length, 8, "超出单轮上限的尾部被丢弃");
assert.deepEqual(composer.addAttachments(many.slice(0, 8)).rejected, [], "满载时幂等重放不能被当作超限失败");

console.log("Chat composer attach tests passed");
