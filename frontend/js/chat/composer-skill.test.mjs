import assert from "node:assert/strict";

// composer.js → ipc.js → tauri.js 在模块顶层写 window（alert/confirm/prompt 拦截），
// node 环境须先备好 window 桩再动态 import（与 composer-bar-popup.test.mjs 同模式）。
globalThis.window = globalThis.window || {};
globalThis.window.__TAURI__ = {
    core: {invoke: async () => ({})},
    event: {listen: async () => ({unlisten: () => {}})},
};

const {filterActiveSkills} = await import("./composer.js");

const skills = [
    {name: "rust-debug", disabled: false},
    {name: "rust-review", disabled: true},
    {name: "translate", disabled: false},
];

assert.deepEqual(
    filterActiveSkills(skills).map((skill) => skill.name),
    ["rust-debug", "translate"],
    "对话提示不得展示已禁用 Skill",
);
assert.deepEqual(
    filterActiveSkills(skills, "RUST").map((skill) => skill.name),
    ["rust-debug"],
    "过滤应忽略大小写且继续排除 disabled Skill",
);
assert.deepEqual(filterActiveSkills(null), []);

console.log("Chat composer Skill tests passed");
