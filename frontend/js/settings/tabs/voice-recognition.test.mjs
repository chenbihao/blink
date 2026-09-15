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
});
assert.deepEqual(RECOGNITION_RANGE, {
    preview_window_ms: {min: 2000, max: 4000},
    preview_refresh_ms: {min: 500, max: 1000},
    draft_min_s: {min: 3, max: 10},
    strong_pause_ms: {min: 500, max: 1500},
});
assert.deepEqual(RECOGNITION_KEYS, [
    "preview_window_ms",
    "preview_refresh_ms",
    "draft_min_s",
    "strong_pause_ms",
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
    };
    assert.equal(normalizeRecognitionConfig(recognition, 6), true);
    assert.deepEqual(recognition, {
        preview_window_ms: 2000,
        preview_refresh_ms: 1000,
        draft_min_s: 6,
        strong_pause_ms: 500,
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
    });
}

{
    const recognition = ensureRecognitionFields([], 12);
    assert.deepEqual(recognition, RECOGNITION_DEFAULTS);
}

const voiceSource = await readFile(new URL("./voice.js", import.meta.url), "utf8");
for (const id of [
    "voice-recognition-preview-window-ms",
    "voice-recognition-preview-refresh-ms",
    "voice-recognition-draft-min-s",
    "voice-recognition-strong-pause-ms",
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
];
for (const lang of ["zh", "en"]) {
    const source = await readFile(new URL(`../../i18n/${lang}.js`, import.meta.url), "utf8");
    for (const key of i18nKeys) {
        assert.match(source, new RegExp(`"${key.replaceAll(".", "\\.")}":`), `${lang}.js 缺少词条 ${key}`);
    }
}

console.log("voice-recognition.test.mjs: all assertions passed");
