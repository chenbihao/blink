import assert from "node:assert/strict";
import {readFile} from "node:fs/promises";

import {
    clampAIHardTimeoutMs,
    DEFAULT_AI_HARD_TIMEOUT_MS,
    effectiveAIHardTimeoutMs,
    formatModelContextWindow,
    memoryExpertVisibility,
} from "./semantics.js";

assert.equal(DEFAULT_AI_HARD_TIMEOUT_MS, 20_000);
assert.equal(effectiveAIHardTimeoutMs(null), 20_000);
assert.equal(effectiveAIHardTimeoutMs(45_000), 45_000);
assert.equal(clampAIHardTimeoutMs("100"), 500);
assert.equal(clampAIHardTimeoutMs("200000"), 30_000);
assert.equal(clampAIHardTimeoutMs("invalid"), null);

assert.deepEqual(memoryExpertVisibility("token_aware", true), {
    fixedCount: false,
    tokenAware: true,
    recallTopK: true,
});
assert.deepEqual(memoryExpertVisibility("fixed_count", false), {
    fixedCount: true,
    tokenAware: false,
    recallTopK: false,
});

assert.equal(formatModelContextWindow(128_000), "128000 tokens");
assert.equal(formatModelContextWindow(null), null);

const settingsHtml = await readFile(new URL("../../../../settings.html", import.meta.url), "utf8");
const coreSource = await readFile(new URL("./core.js", import.meta.url), "utf8");
const providerSource = await readFile(new URL("./provider.js", import.meta.url), "utf8");

for (const panel of ["ai-providers", "ai-chat", "ai-extensions"]) {
    assert.match(settingsHtml, new RegExp(`id="${panel}"`));
}
for (const removedControl of [
    "ai-streaming",
    "ai-direct-safe",
    "ai-tool-feedback",
    "ai-allow-routing",
    "ai-require-whitespace",
    "ai-exclude-pure-numeric",
    "ai-respect-awareness-url-path",
]) {
    assert.doesNotMatch(settingsHtml, new RegExp(`id="${removedControl}"`));
    assert.doesNotMatch(coreSource, new RegExp(`\\$\\("${removedControl}"\\)`));
}
assert.match(settingsHtml, /id="ai-timeout-ms"[^>]*value="20000"/);
assert.match(settingsHtml, /name="ai-agent-mode"[^>]*value="full"/);
assert.match(settingsHtml, /name="ai-agent-mode"[^>]*value="main_chat_only"/);
assert.match(settingsHtml, /name="ai-agent-mode"[^>]*value="pure_chat"/);
assert.match(settingsHtml, /id="ai-main-window-model-mode"/);
assert.match(settingsHtml, /id="ai-tier-ultra-light"/);
// 对话窗口命名模型（LLM 自动命名用，默认超轻档）
assert.match(settingsHtml, /id="ai-chat-title-model-mode"/);
assert.match(settingsHtml, /id="ai-chat-title-custom-model"/);
assert.match(coreSource, /cfg\.chat_config\.title_model/);
// 模型与档位卡片 + 档位降级 banner 从供应商页移入 AI 对话设置
assert.match(settingsHtml, /data-i18n="ai\.tiers\.card"/);
assert.match(settingsHtml, /id="ai-tier-banner"/);
assert.doesNotMatch(settingsHtml, /data-i18n="ai\.tiers\.section"/);
assert.doesNotMatch(settingsHtml, /data-i18n="ai\.filter\.section"/);
assert.match(coreSource, /cfg\.chat_config\.agent_mode = e\.target\.value/);
assert.match(coreSource, /cfg\.chat_config\.main_window_model/);
assert.doesNotMatch(coreSource, /allow_intent_routing/);
assert.doesNotMatch(coreSource, /ctxWindow\s*\*\s*0\.6/);
assert.doesNotMatch(coreSource, /≈.*轮/);

// AI 弹窗按钮必须带基础 btn 类，避免仅使用变体类导致尺寸级联不一致。
for (const buttonId of [
    "ai-modal-cancel",
    "ai-modal-save",
    "ai-model-edit-cancel",
    "ai-model-edit-continue",
    "ai-model-edit-save",
]) {
    assert.match(settingsHtml, new RegExp(`class="[^"]*\\bbtn\\b[^"]*"[^>]*id="${buttonId}"`));
}

// i18n 文案占位符是 {model}；删除确认必须传同名参数。
assert.match(providerSource, /t\("ai\.model\.delete\.confirm", \{model:/);

console.log("AI settings semantics tests passed");
