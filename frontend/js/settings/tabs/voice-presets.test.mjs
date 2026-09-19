// 0.23.17 语音策略预设测试：参数与 Rust preset_matrix 对齐 + 匹配逻辑
import assert from "node:assert/strict";
import {readFile} from "node:fs/promises";

import {VOICE_PRESETS, getPreset, matchPreset} from "./voice-presets.js";
import {RECOGNITION_DEFAULTS, RECOGNITION_RANGE} from "./voice-recognition.js";
import {VAD_DEFAULTS} from "./voice-vad.js";

// 三个预设 + 唯一 id
assert.deepEqual(VOICE_PRESETS.map((preset) => preset.id), ["default", "fast", "long30"]);

// default = 现行默认值（不动点）
assert.deepEqual(VOICE_PRESETS[0].vad, VAD_DEFAULTS);
assert.deepEqual(VOICE_PRESETS[0].recognition, RECOGNITION_DEFAULTS);

// fast：更短切分 + 固定冻结节奏开启（min_sentence 与未提交上限保持默认——
// 前者削弱噪声门会让纯静音误出字，后者收窄过载余量会让连续长语音过载）
{
    const fast = getPreset("fast");
    assert.equal(fast.vad.min_sentence_ms, 800);
    assert.equal(fast.vad.soft_window_s, 5);
    assert.equal(fast.vad.hard_window_s, 8);
    assert.equal(fast.vad.max_uncommitted_s, 12);
    assert.equal(fast.recognition.preview_window_ms, 2500);
    assert.equal(fast.recognition.preview_refresh_ms, 500);
    assert.equal(fast.recognition.draft_min_s, 3);
    assert.equal(fast.recognition.strong_pause_ms, 500);
    assert.equal(fast.recognition.long_pause_ms, 1000);
    assert.equal(fast.recognition.phrase_freeze_interval_ms, 800);
    // 0.23.17：三预设都开启目标窗口。fast 取 5±2 —— floor =
    // max(draft_min 3, 5−2) = 3，落字时机与开启前一致
    assert.equal(fast.recognition.draft_target_s, 5);
    assert.equal(fast.recognition.draft_target_tolerance_s, 2);
    // 预设本身必须是合法组合：归一化后不变
    const {normalizeRecognitionConfig} = await import("./voice-recognition.js");
    const {normalizeVadWindows} = await import("./voice-vad.js");
    const vad = {...fast.vad};
    const recognition = {...fast.recognition};
    assert.equal(normalizeVadWindows(vad), false, "fast 预设 vad 应为不动点");
    assert.equal(normalizeRecognitionConfig(recognition, vad.max_uncommitted_s), false,
        "fast 预设 recognition 应为不动点");
}

// long30：30s 上限 + 目标窗口 24±6
{
    const long30 = getPreset("long30");
    assert.equal(long30.vad.soft_window_s, 20);
    assert.equal(long30.vad.hard_window_s, 26);
    assert.equal(long30.vad.max_uncommitted_s, 30);
    assert.equal(long30.recognition.preview_window_ms, 4000);
    assert.equal(long30.recognition.draft_min_s, 10);
    assert.equal(long30.recognition.draft_target_s, 24);
    assert.equal(long30.recognition.draft_target_tolerance_s, 6);
    assert.equal(long30.recognition.phrase_freeze_interval_ms, 0);
    const {normalizeRecognitionConfig} = await import("./voice-recognition.js");
    const recognition = {...long30.recognition};
    assert.equal(normalizeRecognitionConfig(recognition, 30), false,
        "long30 预设应为不动点（target+tol <= cap）");
}

// 0.23.17：三个预设都开启目标窗口，且渐进上屏保留窗口不随预设变化
{
    for (const preset of VOICE_PRESETS) {
        assert.ok(preset.recognition.draft_target_s > 0,
            `${preset.id} 预设必须默认开启目标窗口`);
        assert.equal(preset.recognition.g2_retention_segments, 0,
            `${preset.id} 的 G2 保留窗口固定为 0（定稿即上屏）`);
        assert.equal(preset.recognition.editor_retention_segments, 1,
            `${preset.id} 的 Editor 保留窗口固定为 1`);
    }
}

// 匹配逻辑：命中 / 自定义
{
    const preset = getPreset("fast");
    assert.equal(matchPreset({...preset.vad}, {...preset.recognition}), "fast");
    const drifted = {...preset.recognition, strong_pause_ms: 600};
    assert.equal(matchPreset({...preset.vad}, drifted), "custom");
    assert.equal(matchPreset({...VAD_DEFAULTS}, {...RECOGNITION_DEFAULTS}), "default");
    assert.equal(getPreset("nope"), undefined);
}

// 与 Rust 侧 preset_matrix 三处同步：数值必须逐一出现在 tests.rs
const rustSource = await readFile(
    new URL("../../../../src/app/local_engine/funasr/tests.rs", import.meta.url),
    "utf8",
);
for (const marker of [
    "soft_window_s: 5",
    "hard_window_s: 8",
    "max_uncommitted_s: 12",
    "preview_window_ms: 2_500",
    "preview_refresh_ms: 500",
    "draft_min_s: 3",
    "strong_pause_ms: 500",
    "long_pause_ms: 1_000",
    "phrase_freeze_interval_ms: 800",
    "draft_target_s: 5",
    "soft_window_s: 20",
    "hard_window_s: 26",
    "max_uncommitted_s: 30",
    "preview_window_ms: 4_000",
    "draft_min_s: 10",
    "draft_target_s: 24",
    "draft_target_tolerance_s: 6",
    // 0.23.17：渐进上屏保留窗口随预设下发（三个预设同值）
    "g2_retention_segments: 0",
    "editor_retention_segments: 1",
]) {
    assert.ok(rustSource.includes(marker), `Rust preset_matrix 缺少参数 ${marker}`);
}
// default 预设直接委托 Rust 默认值（目标窗口默认开启 10s 由 Rust 侧单测锁定）
assert.ok(rustSource.includes("RecognitionConfig::default()"),
    "default 预设必须委托 RecognitionConfig::default()");

// 渐进上屏保留窗口：Rust 侧边界必须与 JS RECOGNITION_RANGE 同值
{
    const rustConfig = await readFile(
        new URL("../../../../src/domain/config/stt_config.rs", import.meta.url),
        "utf8",
    );
    for (const marker of [
        "pub g2_retention_segments: u32",
        "pub editor_retention_segments: u32",
        `RECOGNITION_RETENTION_MIN_SEGMENTS: u32 = ${RECOGNITION_RANGE.g2_retention_segments.min}`,
        `RECOGNITION_RETENTION_MAX_SEGMENTS: u32 = ${RECOGNITION_RANGE.g2_retention_segments.max}`,
    ]) {
        assert.ok(rustConfig.includes(marker), `Rust RecognitionConfig 缺少边界 ${marker}`);
    }
}

console.log("voice-presets.test.mjs: all assertions passed");
