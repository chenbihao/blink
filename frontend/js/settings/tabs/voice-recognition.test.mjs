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
    draft_target_s: 0,
    draft_target_tolerance_s: 2,
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
        draft_target_s: 0,
        draft_target_tolerance_s: 2,
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
        draft_target_s: 0,
        draft_target_tolerance_s: 2,
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

const voiceSource = await readFile(new URL("./voice.js", import.meta.url), "utf8");
for (const id of [
    "voice-recognition-preview-window-ms",
    "voice-recognition-preview-refresh-ms",
    "voice-recognition-draft-min-s",
    "voice-recognition-strong-pause-ms",
    "voice-recognition-phrase-freeze-interval-ms",
    "voice-recognition-draft-target-s",
    "voice-recognition-draft-target-tolerance-s",
    "voice-recognition-reset-btn",
]) {
    assert.match(voiceSource, new RegExp(`getElementById\\("${id}"\\)`), `voice.js 应接线 ${id}`);
}
assert.match(voiceSource, /initRecognitionConfig\(config\)/);
assert.match(voiceSource, /recognitionCard\.classList\.toggle\("hidden"/);
assert.match(voiceSource, /normalizeRecognitionConfig\(recognition/);
assert.match(voiceSource, /setAttribute\("aria-label", t\(control\.ariaKey\)\)/);

const html = await readFile(new URL("../../../settings.html", import.meta.url), "utf8");
assert.match(html, /id="voice-recognition-card"/);
assert.match(html, /class="voice-recognition-card voice-vad-card hidden"/);
const htmlRanges = [
    [/id="voice-recognition-preview-window-ms" max="4000" min="2000" step="100"/, "Preview window range"],
    [/id="voice-recognition-preview-refresh-ms" max="1000" min="500" step="50"/, "Preview refresh range"],
    [/id="voice-recognition-draft-min-s" max="10" min="3" step="1"/, "Draft minimum range"],
    [/id="voice-recognition-strong-pause-ms" max="1500" min="500" step="50"/, "Strong pause range"],
    [/id="voice-recognition-long-pause-ms" max="2000" min="800" step="50"/, "Long pause range"],
    [/id="voice-recognition-phrase-freeze-interval-ms" max="3000" min="0" step="100"/, "Phrase freeze range"],
    [/id="voice-recognition-draft-target-s" max="30" min="0" step="1"/, "Draft target range"],
    [/id="voice-recognition-draft-target-tolerance-s" max="10" min="1" step="1"/, "Draft tolerance range"],
];
for (const [pattern, label] of htmlRanges) assert.match(html, pattern, label);
assert.match(html, /data-i18n="voice\.local\.vad\.min_sentence_ms\.label">候选最短语音/);

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
];
for (const lang of ["zh", "en"]) {
    const source = await readFile(new URL(`../../i18n/${lang}.js`, import.meta.url), "utf8");
    for (const key of i18nKeys) {
        assert.match(source, new RegExp(`"${key.replaceAll(".", "\\.")}":`), `${lang}.js 缺少词条 ${key}`);
    }
}

console.log("voice-recognition.test.mjs: all assertions passed");
