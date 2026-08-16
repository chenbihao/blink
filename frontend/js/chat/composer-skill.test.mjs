import assert from "node:assert/strict";

import {filterActiveSkills} from "./composer.js";

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
