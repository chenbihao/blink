import assert from "node:assert/strict";

// ── Tauri / window 桩（components.js → state.js/api.js → tauri.js）──────────

globalThis.window = globalThis.window || {};
globalThis.window.__TAURI__ = {
    core: {invoke: async () => ({})},
    event: {listen: async () => ({unlisten: () => {}})},
};
if (!globalThis.requestAnimationFrame) {
    globalThis.requestAnimationFrame = (fn) => fn();
}

// ── 最小 DOM 桩 ────────────────────────────────────────────────────────────

function makeElement(tag = "div") {
    const listeners = {};
    const el = {
        tagName: tag.toUpperCase(),
        _children: [],
        listeners,
        className: "",
        title: "",
        textContent: "",
        innerHTML: "",
        type: "",
        dataset: {},
        style: {},
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
            el._children.push(...nodes);
        },
        appendChild(child) {
            el._children.push(child);
            return child;
        },
        insertBefore(child, ref) {
            if (!ref) el._children.unshift(child);
            else {
                const idx = el._children.indexOf(ref);
                if (idx < 0) el._children.unshift(child);
                else el._children.splice(idx, 0, child);
            }
            return child;
        },
        querySelectorAll(sel) {
            if (!sel.startsWith(".")) return [];
            const cls = sel.slice(1);
            const matches = [];
            const walk = (node) => {
                if (String(node.className).split(" ").includes(cls)) matches.push(node);
                (node._children || []).forEach(walk);
            };
            el._children.forEach(walk);
            return matches;
        },
        focus() {},
        remove() {},
    };
    return el;
}

const messagesEl = makeElement("div");
globalThis.document = {
    getElementById: (id) => (id === "chat-messages" ? messagesEl : null),
    createElement: (tag) => makeElement(tag),
    addEventListener() {},
    removeEventListener() {},
    dispatchEvent() {},
};

const components = await import("./components.js");

components.initComponents({onEditMessage: () => {}});

// ── 用例 0：middleEllipsis——中段省略，保住尾部扩展名 ──────────────────────

const {middleEllipsis} = components;
assert.equal(middleEllipsis("a.wav"), "a.wav", "短名不截断");
const longName = "我的超长会议录音文件-20260917-143000-final-version.wav";
const truncated = middleEllipsis(longName);
assert.ok(truncated.includes("…"), "超长名应含省略号");
assert.ok(longName.endsWith(truncated.slice(truncated.indexOf("…") + 1)), "结尾（扩展名）应保留");
assert.ok(truncated.startsWith(longName.slice(0, 1)), "开头应保留");
assert.ok(truncated.length <= 24, `截断后应不超过预算: ${truncated.length}`);
assert.notEqual(truncated, longName);
// 预算参数化
assert.equal(middleEllipsis("abcdef", 3), "a…f");

// ── 用例 1：附件独立成卡片气泡（每个文件一张，右对齐静音卡）────────────────

messagesEl._children.length = 0;
components.renderUserAttachmentCards(["录音.wav", "会议.wav"]);

assert.equal(messagesEl._children.length, 2, "每个附件一张卡片");
for (const card of messagesEl._children) {
    assert.equal(card.className, "chat-attachment-card");
    // 关键约束：不带 .chat-msg 类——startEditMessage 以 .chat-msg 集合定位消息下标，
    // 附件卡片混入会使编辑/重试索引错位
    assert.ok(
        !card.className.split(" ").includes("chat-msg"),
        "附件卡片不得参与 .chat-msg 编辑索引",
    );
    const clip = card.querySelectorAll(".chat-attachment-card-clip");
    assert.equal(clip.length, 1, "前缀 paperclip 图标应存在");
    const typeIcon = card.querySelectorAll(".chat-attachment-card-icon");
    assert.equal(typeIcon.length, 1, "类型图标应存在");
    const label = card.querySelectorAll(".chat-attachment-card-name")[0];
    assert.ok(label, "卡片应含文件名节点");
    assert.ok(card.title.includes("音频附件"), "tooltip 应标识附件类型");
}
assert.equal(
    messagesEl._children[0].querySelectorAll(".chat-attachment-card-name")[0].textContent,
    "录音.wav",
);
assert.equal(
    messagesEl._children[1].querySelectorAll(".chat-attachment-card-name")[0].textContent,
    "会议.wav",
);
const allText = JSON.stringify(messagesEl._children, (key, value) =>
    key === "listeners" ? undefined : value,
);
assert.ok(!allText.includes("rref_"), "卡片不得把 bearer token 带进 DOM");

// ── 用例 2：超长文件名走中段省略，tooltip 保留全名 ──────────────────────────

messagesEl._children.length = 0;
components.renderUserAttachmentCards([longName]);
const longLabel = messagesEl._children[0].querySelectorAll(".chat-attachment-card-name")[0];
assert.equal(longLabel.textContent, truncated, "超长名应中段省略");
assert.ok(
    messagesEl._children[0].title.startsWith(longName),
    "tooltip 应保留完整文件名",
);

// ── 用例 2：正文气泡回到纯文本（附件不再内嵌）───────────────────────────────

messagesEl._children.length = 0;
const bubble = components.renderUserMessage("转录然后整理一下文本");
assert.ok(bubble.className.split(" ").includes("chat-msg-user"));
assert.equal(bubble.textContent, "转录然后整理一下文本");
assert.equal(bubble.querySelectorAll(".chat-attachment-card").length, 0);
assert.equal(bubble.querySelectorAll(".chat-msg-attachments").length, 0, "旧徽标行应已移除");

console.log("Chat user attachment card tests passed");
