/**
 * 语音高级输入策略预设（0.23.17）。
 *
 * 三个预设覆盖主要使用场景（0.23.17 起三个预设都默认开启定稿目标窗口，
 * 设置页不再要求用户手动选择）：
 * - default：均衡（现行默认值；目标 10s±2s，优选窗 [8, 12]）
 * - fast：快速反应——更快出字、更短定稿、短语固定节奏开；目标取
 *   5s±2s，floor = max(draft_min 3, 3) = 3s，落字时机与开启前一致
 * - long30：超长定稿——30s 上限 + 目标窗口 24s±6s，追求长上下文精度
 *
 * 渐进上屏保留窗口（段数）不随预设变化：G2=0 / Editor=1 是产品语义
 * （0.23.13），预设只覆盖识别节奏。
 *
 * 与 Rust 侧 `preset_matrix`（src/app/local_engine/funasr/tests.rs）
 * 三处同步铁则：Rust / JS / phase 文档。调参必须同步两侧并重跑矩阵。
 */

import {RECOGNITION_DEFAULTS} from "./voice-recognition.js";
import {VAD_DEFAULTS} from "./voice-vad.js";

export const VOICE_PRESETS = [
    {
        id: "default",
        vad: {...VAD_DEFAULTS},
        recognition: {...RECOGNITION_DEFAULTS},
    },
    {
        id: "fast",
        vad: {
            ...VAD_DEFAULTS,
            soft_window_s: 5,
            hard_window_s: 8,
            // 未提交上限保持默认 12：快切分（hard=8）已保证段落短，
            // 缩小上限会收窄过载余量（连续长语音时推理落后于喂入）。
            max_uncommitted_s: 12,
        },
        recognition: {
            ...RECOGNITION_DEFAULTS,
            preview_window_ms: 2500,
            preview_refresh_ms: 500,
            draft_min_s: 3,
            strong_pause_ms: 500,
            long_pause_ms: 1000,
            phrase_freeze_interval_ms: 800,
            draft_target_s: 5,
            draft_target_tolerance_s: 2,
        },
    },
    {
        id: "long30",
        vad: {
            ...VAD_DEFAULTS,
            soft_window_s: 20,
            hard_window_s: 26,
            max_uncommitted_s: 30,
        },
        recognition: {
            ...RECOGNITION_DEFAULTS,
            preview_window_ms: 4000,
            draft_min_s: 10,
            draft_target_s: 24,
            draft_target_tolerance_s: 6,
        },
    },
];

/**
 * 当前参数命中的预设 id；任一字段偏离返回 "custom"。
 * 只比较两卡实际暴露的字段（VAD 全字段 + recognition 全字段）。
 */
export function matchPreset(vad, recognition) {
    for (const preset of VOICE_PRESETS) {
        const vadMatched = Object.entries(preset.vad).every(
            ([key, value]) => vad[key] === value,
        );
        const recognitionMatched = Object.entries(preset.recognition).every(
            ([key, value]) => recognition[key] === value,
        );
        if (vadMatched && recognitionMatched) return preset.id;
    }
    return "custom";
}

/** 按 id 取预设；未知 id 返回 undefined。 */
export function getPreset(id) {
    return VOICE_PRESETS.find((preset) => preset.id === id);
}
