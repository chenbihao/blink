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

for (const panel of ["ai-providers", "ai-chat", "ai-extensions"]) {
    assert.match(settingsHtml, new RegExp(`id="${panel}"`));
}
for (const removedControl of ["ai-streaming", "ai-direct-safe", "ai-tool-feedback"]) {
    assert.doesNotMatch(settingsHtml, new RegExp(`id="${removedControl}"`));
    assert.doesNotMatch(coreSource, new RegExp(`\\$\\("${removedControl}"\\)`));
}
assert.match(settingsHtml, /id="ai-timeout-ms"[^>]*value="20000"/);
assert.match(settingsHtml, /name="ai-agent-mode"[^>]*value="full"/);
assert.match(settingsHtml, /name="ai-agent-mode"[^>]*value="main_chat_only"/);
assert.match(settingsHtml, /name="ai-agent-mode"[^>]*value="pure_chat"/);
assert.match(settingsHtml, /id="ai-main-window-model-mode"/);
assert.match(settingsHtml, /id="ai-tier-ultra-light"/);
assert.match(coreSource, /cfg\.chat_config\.agent_mode = e\.target\.value/);
assert.match(coreSource, /cfg\.chat_config\.main_window_model/);
assert.doesNotMatch(coreSource, /ctxWindow\s*\*\s*0\.6/);
assert.doesNotMatch(coreSource, /≈.*轮/);

console.log("AI settings semantics tests passed");
