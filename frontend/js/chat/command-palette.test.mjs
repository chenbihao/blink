import assert from "node:assert/strict";

// command-palette.js → ipc.js/i18n → tauri.js 在模块顶层写 window，node 环境
// 须先备好 window 桩再动态 import（与旧 composer-skill.test.mjs 同模式）。
globalThis.window = globalThis.window || {};
globalThis.window.__TAURI__ = {
    core: {invoke: async () => ({})},
    event: {listen: async () => ({unlisten: () => {}})},
};

const cp = await import("./command-palette.js");
const {splitSlashQuery, filterSlashCommands, filterActiveSkills, SLASH_COMMANDS} = cp;

// ── 触发契约（phase §8.3：仅首个非空白字符为 / 且单行时触发） ──────────────

assert.deepEqual(splitSlashQuery("/"), {name: "", args: ""}, "仅 / 触发（列全部命令）");
assert.deepEqual(splitSlashQuery("/ne"), {name: "ne", args: ""}, "/ne 触发并用于过滤");
assert.deepEqual(splitSlashQuery("  /model"), {name: "model", args: ""}, "前导空白不阻止触发");
assert.deepEqual(
    splitSlashQuery("/skill rust 帮我调试"),
    {name: "skill", args: "rust 帮我调试"},
    "/skill 带参仍触发（skill 子视图过滤）",
);
assert.equal(splitSlashQuery("hello /new"), null, "非 / 开头不触发");
assert.equal(splitSlashQuery("/new\n第二行"), null, "多行输入不触发");
assert.equal(splitSlashQuery("\n/new"), null, "前导换行不触发");
assert.equal(splitSlashQuery(""), null, "空输入不触发");
assert.equal(splitSlashQuery(null), null, "非字符串输入不触发");

// ── 命令模糊过滤（前缀优先 → 子串次之） ────────────────────────────────────

assert.deepEqual(
    filterSlashCommands(SLASH_COMMANDS, "").map((c) => c.name),
    SLASH_COMMANDS.map((c) => c.name),
    "空前缀列全部命令",
);
assert.deepEqual(
    filterSlashCommands(SLASH_COMMANDS, "m").map((c) => c.name),
    ["model", "mcp", "compact", "prompt"],
    "前缀命中优先（model/mcp 先于 compact/prompt 的子串命中）",
);
assert.deepEqual(
    filterSlashCommands(SLASH_COMMANDS, "co").map((c) => c.name),
    ["compact"],
    "前缀命中 compact",
);
assert.deepEqual(
    filterSlashCommands(SLASH_COMMANDS, "NE").map((c) => c.name),
    ["new"],
    "过滤大小写不敏感",
);
assert.deepEqual(filterSlashCommands(SLASH_COMMANDS, "zzz"), [], "无命中返回空（未知命令）");

// ── Skill 过滤（沿用旧 skill-hint 语义：disabled 不进提示） ────────────────

const skills = [
    {name: "rust-debug", disabled: false},
    {name: "rust-review", disabled: true},
    {name: "translate", disabled: false},
];
assert.deepEqual(
    filterActiveSkills(skills).map((skill) => skill.name),
    ["rust-debug", "translate"],
    "不得展示已禁用 Skill",
);
assert.deepEqual(
    filterActiveSkills(skills, "RUST").map((skill) => skill.name),
    ["rust-debug"],
    "过滤忽略大小写且继续排除 disabled Skill",
);
assert.deepEqual(filterActiveSkills(null), []);

// ── /prompt 命令采纳 → 条目采纳集成链路（实机验收发现的静默无操作回归）──────
//
// 命令名 "prompt" 与条目 kind 曾撞名：Enter 采纳 /prompt 命令时误入
// promptEntries.find("") 分支 → 面板关闭、无插入、无报错。条目 kind 现为
// "prompt_entry"，本组用例钉住两个命名空间不再互扰。

function makePaletteEl() {
    return {
        _listeners: {},
        innerHTML: "",
        hidden: false,
        value: "",
        selectionStart: 0,
        selectionEnd: 0,
        scrollTop: 0,
        scrollHeight: 100,
        classList: {toggle() {}, add() {}, remove() {}, contains: () => false},
        contains: () => false,
        addEventListener(type, fn) {
            (this._listeners[type] ||= []).push(fn);
        },
        dispatchEvent(ev) {
            (this._listeners[ev.type] || []).forEach((fn) => fn(ev));
            return true;
        },
        querySelectorAll: () => [],
        querySelector: () => null,
        setSelectionRange() {},
        focus() {},
    };
}

{
    const paletteEl = makePaletteEl();
    const inputEl = makePaletteEl();
    const els = {"chat-command-palette": paletteEl, "chat-input": inputEl};
    // escapeText 走 createElement("div") 的 textContent→innerHTML 浏览器语义
    const prevDocument = globalThis.document;
    globalThis.document = {
        getElementById: (id) => els[id] || null,
        addEventListener() {},
        createElement: () => {
            const el = makePaletteEl();
            let text = "";
            Object.defineProperty(el, "textContent", {
                set(v) {
                    text = String(v ?? "");
                },
                get() {
                    return text;
                },
            });
            Object.defineProperty(el, "innerHTML", {
                get() {
                    return text
                        .replace(/&/g, "&amp;")
                        .replace(/</g, "&lt;")
                        .replace(/>/g, "&gt;");
                },
                set() {},
            });
            return el;
        },
    };
    const prevRaf = globalThis.requestAnimationFrame;
    globalThis.requestAnimationFrame = (fn) => fn();
    const prevInvoke = globalThis.window.__TAURI__.core.invoke;
    globalThis.window.__TAURI__.core.invoke = async (cmd) =>
        cmd === "prompt_library_list"
            ? [{id: "p1", name: "测试提示词", effectiveContent: "你好，请帮我", builtin: false, hidden: false}]
            : {};

    try {
        const inserted = [];
        let revision = 0;
        await cp.initCommandPalette({
            onCommand: (name) => {
                if (name === "prompt") cp.openPromptPalette();
            },
            getInputRevision: () => revision,
            insertTextAtCursor: (text) => inserted.push(text),
        });

        // 1. /prompt → 命令面板；Enter 采纳命令 → PROMPT 视图应打开
        cp.onComposerInput("/prompt");
        assert.ok(cp.isPaletteOpen(), "命令面板应打开");
        assert.equal(
            cp.handlePaletteKeydown({key: "Enter", preventDefault() {}, stopPropagation() {}}),
            true,
            "命令面板 Enter 应被面板处理",
        );
        await new Promise((r) => setTimeout(r, 30));
        assert.ok(cp.isPaletteOpen(), "PROMPT 视图应打开（命令采纳后）");
        assert.ok(
            paletteEl.innerHTML.includes("测试提示词"),
            "PROMPT 列表应渲染条目",
        );

        // 2. PROMPT 视图 Enter 采纳条目 → insertTextAtCursor 应收到正文
        assert.equal(
            cp.handlePaletteKeydown({key: "Enter", preventDefault() {}, stopPropagation() {}}),
            true,
            "PROMPT 面板 Enter 应被处理",
        );
        assert.equal(inserted.length, 1, `insertTextAtCursor 应被调用一次，实际 ${inserted.length} 次`);
        assert.equal(inserted[0], "你好，请帮我");
        assert.ok(!cp.isPaletteOpen(), "条目采纳后面板应关闭");
    } finally {
        globalThis.document = prevDocument;
        globalThis.requestAnimationFrame = prevRaf;
        globalThis.window.__TAURI__.core.invoke = prevInvoke;
    }
}

console.log("Chat command-palette tests passed");
