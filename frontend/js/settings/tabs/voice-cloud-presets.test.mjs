// 0.29.1 云端 STT 供应商预设测试：预设完备性 + 回显匹配（含 base_url 缺省回退）
import assert from "node:assert/strict";

import {CLOUD_STT_PRESETS, KIND_DEFAULT_BASE_URL, matchCloudPreset} from "./voice-cloud-presets.js";

// 每个预设的 kind 都是合法值域；预设 base_url 与该 kind 的默认 URL 一致
const KINDS = ["openai", "groq", "mimo", "mimo_plan", "custom"];
for (const [id, preset] of Object.entries(CLOUD_STT_PRESETS)) {
    if (id === "custom") {
        assert.equal(preset, null, "custom 预设必须为 null（全手填）");
        continue;
    }
    assert.ok(KINDS.includes(preset.kind), `${id} 预设 kind 非法`);
    assert.equal(
        preset.base_url.replace(/\/$/, ""),
        KIND_DEFAULT_BASE_URL[preset.kind].replace(/\/$/, ""),
        `${id} 预设 base_url 应与 kind 默认值一致`,
    );
}

// 0.29.1 整理点：mimo_plan 预设补齐（此前只有迁移逻辑里有、UI 无入口）
assert.ok(CLOUD_STT_PRESETS.mimo_plan, "mimo_plan 预设应存在");
assert.equal(CLOUD_STT_PRESETS.mimo_plan.kind, "mimo_plan");

// ── 回显匹配 ──

// 完整显式配置 → 命中对应预设
assert.equal(
    matchCloudPreset({kind: "openai", base_url: "https://api.openai.com/v1", model_id: "whisper-1"}),
    "openai",
);

// 回显 bug 修复：base_url=null（未显式保存，后端用 kind 默认值）也应命中预设
assert.equal(matchCloudPreset({kind: "openai", base_url: null, model_id: "whisper-1"}), "openai");
assert.equal(matchCloudPreset({kind: "groq", base_url: null, model_id: "whisper-large-v3"}), "groq");

// 尾斜杠差异不影响命中
assert.equal(
    matchCloudPreset({kind: "mimo", base_url: "https://api.xiaomimimo.com/v1/", model_id: ""}),
    "mimo",
);

// 自部署端点（funasr）→ 自定义
assert.equal(
    matchCloudPreset({kind: "openai", base_url: "http://192.168.31.77:38040/v1", model_id: "sensevoice-small"}),
    "custom",
);

// kind=custom → 自定义（后端将其按 openai 协议 + 用户 URL 处理）
assert.equal(matchCloudPreset({kind: "custom", base_url: "http://host:9000/v1", model_id: "m"}), "custom");

// 未配置 → 自定义
assert.equal(matchCloudPreset(null), "custom");

// 缺 kind（旧配置）→ 按 openai 兜底参与匹配
assert.equal(matchCloudPreset({base_url: "https://api.openai.com/v1", model_id: "x"}), "openai");

console.log("voice-cloud-presets.test.mjs: all assertions passed");
