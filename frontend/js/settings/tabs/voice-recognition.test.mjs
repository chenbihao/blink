// 0.23.9 Preview / Draft 配置测试：纯逻辑归一化 + 设置页契约静态对齐
import assert from "node:assert/strict";
import {readFile} from "node:fs/promises";

import {
    RECOGNITION_DEFAULTS,
    RECOGNITION_KEYS,
    RECOGNITION_RANGE,
    ensureRecognitionFields,
    normalizeRecognitionConfig,
} from "./voice-recognition.js";

assert.deepEqual(RECOGNITION_DEFAULTS, {
    preview_window_ms: 3000,
    preview_refresh_ms: 700,
    draft_min_s: 5,
    strong_pause_ms: 700,
    long_pause_ms: 1100,
    phrase_freeze_interval_ms: 0,
    // 0.23.17：目标窗口默认开启（10s ± 2s，stock 上限 12 下恰好可达）
    draft_target_s: 10,
    draft_target_tolerance_s: 2,
    // 0.23.17：渐进上屏保留窗口（G2 定稿即上屏 / Editor 停留一句）
    g2_retention_segments: 0,
    editor_retention_segments: 1,
});
assert.deepEqual(RECOGNITION_RANGE, {
    preview_window_ms: {min: 2000, max: 4000},
    preview_refresh_ms: {min: 500, max: 1000},
    draft_min_s: {min: 3, max: 10},
    strong_pause_ms: {min: 500, max: 1500},
    long_pause_ms: {min: 800, max: 2000},
    phrase_freeze_interval_ms: {min: 0, max: 3000},
    draft_target_s: {min: 0, max: 30},
    draft_target_tolerance_s: {min: 1, max: 10},
    g2_retention_segments: {min: 0, max: 5},
    editor_retention_segments: {min: 0, max: 5},
});
assert.deepEqual(RECOGNITION_KEYS, [
    "preview_window_ms",
    "preview_refresh_ms",
    "draft_min_s",
    "strong_pause_ms",
    "long_pause_ms",
    "phrase_freeze_interval_ms",
    "draft_target_s",
    "draft_target_tolerance_s",
    "g2_retention_segments",
    "editor_retention_segments",
]);

{
    const recognition = {...RECOGNITION_DEFAULTS};
    assert.equal(normalizeRecognitionConfig(recognition, 12), false);
    assert.deepEqual(recognition, RECOGNITION_DEFAULTS);
}

{
    const recognition = {
        preview_window_ms: 1,
        preview_refresh_ms: 9999,
        draft_min_s: 99,
        strong_pause_ms: 1,
        long_pause_ms: 99,
    };
    assert.equal(normalizeRecognitionConfig(recognition, 6), true);
    assert.deepEqual(recognition, {
        preview_window_ms: 2000,
        preview_refresh_ms: 1000,
        draft_min_s: 6,
        strong_pause_ms: 500,
        long_pause_ms: 800,
        phrase_freeze_interval_ms: 0,
        // 0.23.17：缺字段补默认目标 10s，再按未提交上限 6s 收敛到 6−2=4s
        draft_target_s: 4,
        draft_target_tolerance_s: 2,
        g2_retention_segments: 0,
        editor_retention_segments: 1,
    });
    assert.ok(recognition.preview_refresh_ms < recognition.preview_window_ms);
    assert.ok(recognition.draft_min_s <= 6);
}

{
    const recognition = {};
    assert.equal(ensureRecognitionFields(recognition, 4), recognition);
    assert.deepEqual(recognition, {
        preview_window_ms: 3000,
        preview_refresh_ms: 700,
        draft_min_s: 4,
        strong_pause_ms: 700,
        long_pause_ms: 1100,
        phrase_freeze_interval_ms: 0,
        // 0.23.17：上限 4s 放不下目标下限（4−2=2 < 4）→ 目标被关闭，
        // 与后端 sanitize 的"不可达即关闭"一致
        draft_target_s: 0,
        draft_target_tolerance_s: 2,
        g2_retention_segments: 0,
        editor_retention_segments: 1,
    });
}

{
    const recognition = ensureRecognitionFields([], 12);
    assert.deepEqual(recognition, RECOGNITION_DEFAULTS);
}

// 0.23.14 长静音不得低于强停顿：越界收敛后保持关系约束
{
    const recognition = {...RECOGNITION_DEFAULTS, strong_pause_ms: 1200, long_pause_ms: 900};
    assert.equal(normalizeRecognitionConfig(recognition, 12), true);
    assert.equal(recognition.long_pause_ms, 1250);
    assert.ok(recognition.long_pause_ms > recognition.strong_pause_ms);
}

// 0.23.14.6 相等门槛必须被拉开至少一个滑块步长（50ms）——相等会让
// 长静音分支遮蔽强停顿的 2s/1.2s 保护
{
    const recognition = {...RECOGNITION_DEFAULTS, strong_pause_ms: 1200, long_pause_ms: 1200};
    assert.equal(normalizeRecognitionConfig(recognition, 12), true);
    assert.equal(recognition.long_pause_ms, 1250);
    assert.equal(recognition.strong_pause_ms, 1200);
}

// 已合法间隔（> 步长）不调整
{
    const recognition = {...RECOGNITION_DEFAULTS, strong_pause_ms: 1200, long_pause_ms: 1300};
    assert.equal(normalizeRecognitionConfig(recognition, 12), false);
    assert.equal(recognition.long_pause_ms, 1300);
}

// 0.23.16.5：固定节奏与目标窗口归一化（与后端 sanitize 三处同步）
{
    // 固定节奏：0 保持关闭；500 收敛 800；4000 收敛 3000。
    const freeze = {...RECOGNITION_DEFAULTS, phrase_freeze_interval_ms: 500};
    assert.equal(normalizeRecognitionConfig(freeze, 12), true);
    assert.equal(freeze.phrase_freeze_interval_ms, 800);
    freeze.phrase_freeze_interval_ms = 4000;
    normalizeRecognitionConfig(freeze, 12);
    assert.equal(freeze.phrase_freeze_interval_ms, 3000);
    const off = {...RECOGNITION_DEFAULTS, phrase_freeze_interval_ms: 0};
    assert.equal(normalizeRecognitionConfig(off, 12), false, "0 = 关闭，不得被抬升");

    // 目标窗口：10 + 2 在 max_uncommitted=12 内恰好可达（不动）。
    const fit = {...RECOGNITION_DEFAULTS, draft_target_s: 10, draft_target_tolerance_s: 2};
    assert.equal(normalizeRecognitionConfig(fit, 12), false);
    // 超上限：target 收敛到 max − tolerance。
    const overshoot = {...RECOGNITION_DEFAULTS, draft_target_s: 25, draft_target_tolerance_s: 3};
    assert.equal(normalizeRecognitionConfig(overshoot, 12), true);
    assert.equal(overshoot.draft_target_s, 9);
    // 不可达（cap − tolerance < 4）：关闭。
    const impossible = {...RECOGNITION_DEFAULTS, draft_target_s: 10, draft_target_tolerance_s: 10};
    assert.equal(normalizeRecognitionConfig(impossible, 12), true);
    assert.equal(impossible.draft_target_s, 0);
    assert.equal(impossible.draft_target_tolerance_s, 10);
    // 关闭状态宽容越界仍收敛。
    const toleranceOnly = {...RECOGNITION_DEFAULTS, draft_target_tolerance_s: 99};
    assert.equal(normalizeRecognitionConfig(toleranceOnly, 12), true);
    assert.equal(toleranceOnly.draft_target_tolerance_s, 10);
}

// 0.23.17 静态契约：时序条 + 预设行重设计后的接线对齐
const voiceSource = await readFile(new URL("./voice.js", import.meta.url), "utf8");
assert.match(voiceSource, /initAdvancedVoiceControls\(config/);
const advancedUiSource = await readFile(new URL("./voice-advanced-ui.js", import.meta.url), "utf8");
for (const id of [
    "voice-ts-preview",
    "voice-ts-draft",
    "voice-draft-target-toggle",
    "voice-recognition-reset-btn",
    // 0.23.17：选区带下方的"实际生效区间 + 上限约束"提示
    "voice-draft-target-effective",
]) {
    assert.match(advancedUiSource, new RegExp(`getElementById\\("${id}"\\)`), `voice-advanced-ui.js 应接线 ${id}`);
}
// 0.23.17：渐进上屏保留窗口滑杆走 bindRangeControl({id: "…"}) 统一接线
for (const id of ["voice-g2-retention", "voice-editor-retention"]) {
    assert.match(advancedUiSource, new RegExp(`id: "${id}"`), `voice-advanced-ui.js 应接线 ${id}`);
}
// 0.23.17：独立滑杆必须写 --fill-pct（否则高亮恒定停在 CSS 兜底的 50%）
assert.match(advancedUiSource, /setProperty\("--fill-pct"/);
// 卡2 恢复默认覆盖全部 recognition 字段；提交统一走归一化
assert.match(advancedUiSource, /Object\.assign\(recognition, RECOGNITION_DEFAULTS\)/);
assert.match(advancedUiSource, /normalizeRecognitionConfig\(recognition, currentMaxUncommittedS\(\)\)/);

const html = await readFile(new URL("../../../settings.html", import.meta.url), "utf8");
assert.match(html, /id="voice-recognition-card"/);
assert.match(html, /class="voice-recognition-card voice-vad-card hidden"/);
for (const id of [
    "voice-ts-preview",
    "voice-ts-draft",
    "voice-draft-target-toggle",
    "voice-preset-row",
    "voice-g2-retention",
    "voice-editor-retention",
    "voice-draft-target-effective",
]) {
    assert.match(html, new RegExp(`id="${id}"`), `settings.html 应包含 ${id}`);
}
assert.match(html, /data-i18n="voice\.local\.vad\.min_sentence_ms\.label">候选最短语音/);
// 0.23.17 版式修正：预设行提到两张参数卡之前（紧跟流式识别）
{
    const streamingAt = html.indexOf('id="voice-streaming-field"');
    const presetAt = html.indexOf('id="voice-preset-row"');
    const vadCardAt = html.indexOf('id="voice-ts-pauses"');
    assert.ok(streamingAt > 0 && presetAt > streamingAt && vadCardAt > presetAt,
        "预设行必须位于流式识别之后、参数卡之前");
    // 候选最短语音必须在停顿检测时序条之前
    const minSentenceAt = html.indexOf('id="voice-vad-min-sentence-ms"');
    assert.ok(minSentenceAt > 0 && minSentenceAt < vadCardAt,
        "候选最短语音必须排在停顿检测时序条之前");
}
// 0.23.17：组合预览泳道默认收起（<details> 不带 open）
assert.match(html, /<details class="voice-vad-debug-collapsible" id="voice-vad-debug-composite-details">/,
    "组合预览泳道必须默认收起");
// 0.23.17：面向用户的文案不再出现 P / D 缩写
assert.doesNotMatch(html, /预览 P|定稿 D|预览（P）|定稿（D）/,
    "HTML 文案不得再使用 P / D 缩写");

const i18nKeys = [
    "voice.local.recognition.title",
    "voice.local.recognition.desc",
    "voice.local.recognition.hint",
    "voice.local.recognition.reset",
    "voice.local.recognition.preview_window_ms.label",
    "voice.local.recognition.preview_window_ms.hint",
    "voice.local.recognition.preview_refresh_ms.label",
    "voice.local.recognition.preview_refresh_ms.hint",
    "voice.local.recognition.draft_min_s.label",
    "voice.local.recognition.draft_min_s.hint",
    "voice.local.recognition.strong_pause_ms.label",
    "voice.local.recognition.strong_pause_ms.hint",
    "voice.local.recognition.long_pause_ms.label",
    "voice.local.recognition.long_pause_ms.hint",
    "voice.local.recognition.phrase_freeze_interval_ms.label",
    "voice.local.recognition.phrase_freeze_interval_ms.hint",
    "voice.local.recognition.draft_target_s.label",
    "voice.local.recognition.draft_target_s.hint",
    "voice.local.recognition.draft_target_tolerance_s.label",
    "voice.local.recognition.draft_target_tolerance_s.hint",
    // 0.23.17：渐进上屏保留窗口 + 目标窗口生效区间提示
    "voice.local.recognition.delivery_group.title",
    "voice.local.recognition.g2_retention_segments.label",
    "voice.local.recognition.g2_retention_segments.hint",
    "voice.local.recognition.editor_retention_segments.label",
    "voice.local.recognition.editor_retention_segments.hint",
    "voice.local.recognition.retention.immediate",
    "voice.local.recognition.retention.immediate_editor",
    "voice.local.recognition.retention.segments",
    "voice.local.recognition.draft_target.off_hint",
    "voice.local.recognition.draft_target.effective",
    "voice.local.recognition.draft_target.limited",
    "voice.local.recognition.draft_target.unreachable",
    "voice.local.preset.label",
];
for (const lang of ["zh", "en"]) {
    const source = await readFile(new URL(`../../i18n/${lang}.js`, import.meta.url), "utf8");
    for (const key of i18nKeys) {
        assert.match(source, new RegExp(`"${key.replaceAll(".", "\\.")}":`), `${lang}.js 缺少词条 ${key}`);
    }
    // 0.23.17：面向用户的文案不出现 P / D 缩写（缩写只留在代码与 phase 文档）
    assert.doesNotMatch(source, /预览 P|定稿 D|预览（P）|定稿（D）/,
        `${lang}.js 文案不得再使用 P / D 缩写`);
}

console.log("voice-recognition.test.mjs: all assertions passed");
