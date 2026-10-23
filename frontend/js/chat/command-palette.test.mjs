import assert from "node:assert/strict";

// command-palette.js → ipc.js/i18n → tauri.js 在模块顶层写 window，node 环境
// 须先备好 window 桩再动态 import（与旧 composer-skill.test.mjs 同模式）。
globalThis.window = globalThis.window || {};
globalThis.window.__TAURI__ = {
    core: {invoke: async () => ({})},
    event: {listen: async () => ({unlisten: () => {}})},
};

const {splitSlashQuery, filterSlashCommands, filterActiveSkills, SLASH_COMMANDS} =
    await import("./command-palette.js");

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

console.log("Chat command-palette tests passed");
