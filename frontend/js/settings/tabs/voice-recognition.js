/**
 * Preview / Draft 识别协调参数的纯逻辑（0.23.9）。
 *
 * 默认值与 Rust 侧 `RecognitionConfig` 一一对应；`normalizeRecognitionConfig`
 * 与 `RecognitionConfig::sanitize` 保持同样的 clamp / 关系约束，供设置页
 * 回显和保存前即时归一化使用。
 */

export const RECOGNITION_DEFAULTS = {
    preview_window_ms: 3000,
    preview_refresh_ms: 700,
    draft_min_s: 5,
    strong_pause_ms: 700,
    long_pause_ms: 1100,
    // 0.23.16.5：0 = 关闭（沿用现状）。
    phrase_freeze_interval_ms: 0,
    draft_target_s: 0,
    draft_target_tolerance_s: 2,
};

export const RECOGNITION_KEYS = [
    "preview_window_ms",
    "preview_refresh_ms",
    "draft_min_s",
    "strong_pause_ms",
    "long_pause_ms",
    "phrase_freeze_interval_ms",
    "draft_target_s",
    "draft_target_tolerance_s",
];

export const RECOGNITION_RANGE = {
    preview_window_ms: {min: 2000, max: 4000},
    preview_refresh_ms: {min: 500, max: 1000},
    draft_min_s: {min: 3, max: 10},
    strong_pause_ms: {min: 500, max: 1500},
    long_pause_ms: {min: 800, max: 2000},
    phrase_freeze_interval_ms: {min: 0, max: 3000},
    draft_target_s: {min: 0, max: 30},
    draft_target_tolerance_s: {min: 1, max: 10},
};

/** 0.23.16.5：固定节奏设值时的安全边界（0=关闭；设值 800～3000ms）。 */
export const PHRASE_FREEZE_ACTIVE_MIN_MS = 800;
/** 0.23.16.5：目标窗口中心设值时的安全边界（0=关闭；设值 4～30s）。 */
export const DRAFT_TARGET_ACTIVE_MIN_S = 4;

/**
 * long_pause_ms 必须严格大于 strong_pause_ms 的最小间隔（毫秒）。
 * 与后端 RECOGNITION_PAUSE_MIN_GAP_MS（滑块步长 50ms）一致：相等会让
 * 长静音分支（约 300ms 有声即定稿）遮蔽强停顿的 2s/1.2s 保护。
 */
export const RECOGNITION_PAUSE_MIN_GAP_MS = 50;

/**
 * 归一化 Preview / Draft 配置。
 *
 * `maxUncommittedS` 是已归一化 VAD 的未提交上限；缺省时只应用字段自身
 * 范围，便于旧配置回显。就地修改并返回是否发生调整。
 */
export function normalizeRecognitionConfig(recognition, maxUncommittedS = Infinity) {
    let changed = false;
    for (const key of RECOGNITION_KEYS) {
        const range = RECOGNITION_RANGE[key];
        const current = recognition[key];
        const value = typeof current === "number" && Number.isFinite(current)
            ? current
            : RECOGNITION_DEFAULTS[key];
        const clamped = Math.min(range.max, Math.max(range.min, value));
        if (clamped !== current) {
            recognition[key] = clamped;
            changed = true;
        }
    }

    // 当前安全范围本身保证 refresh < window；保留显式关系约束，避免
    // 将来调整范围时前端回显与后端 sanitize 分叉。
    if (recognition.preview_refresh_ms >= recognition.preview_window_ms) {
        const refresh = Math.min(
            RECOGNITION_RANGE.preview_refresh_ms.max,
            Math.max(
                RECOGNITION_RANGE.preview_refresh_ms.min,
                recognition.preview_window_ms - 1,
            ),
        );
        if (refresh !== recognition.preview_refresh_ms) {
            recognition.preview_refresh_ms = refresh;
            changed = true;
        }
    }

    const numericMax = Number.isFinite(maxUncommittedS)
        ? Math.max(0, maxUncommittedS)
        : Infinity;
    const draftMin = Math.min(RECOGNITION_RANGE.draft_min_s.min, numericMax);
    const draftMax = Math.min(RECOGNITION_RANGE.draft_min_s.max, numericMax);
    const draft = Math.min(draftMax, Math.max(draftMin, recognition.draft_min_s));
    if (draft !== recognition.draft_min_s) {
        recognition.draft_min_s = draft;
        changed = true;
    }

    // 长静音终结必须严格晚于强停顿（至少一个滑块步长），否则强停顿规则
    // 在 [strong, long) 空区间内永远不可达（与后端 sanitize 同一约束）。
    if (recognition.long_pause_ms <= recognition.strong_pause_ms) {
        const floor = Math.min(
            RECOGNITION_RANGE.long_pause_ms.max,
            recognition.strong_pause_ms + RECOGNITION_PAUSE_MIN_GAP_MS,
        );
        if (recognition.long_pause_ms < floor) {
            recognition.long_pause_ms = floor;
            changed = true;
        }
    }

    // 0.23.16.5：固定节奏 0=关闭；设值收敛 [800, 3000]（与后端 sanitize
    // 同一约束，三处同步铁则）。
    if (recognition.phrase_freeze_interval_ms !== 0) {
        const freeze = Math.min(
            RECOGNITION_RANGE.phrase_freeze_interval_ms.max,
            Math.max(PHRASE_FREEZE_ACTIVE_MIN_MS, recognition.phrase_freeze_interval_ms),
        );
        if (freeze !== recognition.phrase_freeze_interval_ms) {
            recognition.phrase_freeze_interval_ms = freeze;
            changed = true;
        }
    }

    // 0.23.16.5：目标窗口宽容收敛；target 设值时中心收敛并保证
    // target + tolerance ≤ max_uncommitted（不可达时关闭，与后端一致）。
    const tolerance = Math.min(
        RECOGNITION_RANGE.draft_target_tolerance_s.max,
        Math.max(RECOGNITION_RANGE.draft_target_tolerance_s.min, recognition.draft_target_tolerance_s),
    );
    if (tolerance !== recognition.draft_target_tolerance_s) {
        recognition.draft_target_tolerance_s = tolerance;
        changed = true;
    }
    if (recognition.draft_target_s !== 0) {
        const target = Math.min(
            RECOGNITION_RANGE.draft_target_s.max,
            Math.max(DRAFT_TARGET_ACTIVE_MIN_S, recognition.draft_target_s),
        );
        let next = target;
        if (Number.isFinite(numericMax)) {
            const ceilingCap = Math.max(0, numericMax - tolerance);
            if (ceilingCap < target) {
                next = ceilingCap >= DRAFT_TARGET_ACTIVE_MIN_S ? ceilingCap : 0;
            }
        }
        if (next !== recognition.draft_target_s) {
            recognition.draft_target_s = next;
            changed = true;
        }
    }

    return changed;
}

/**
 * 补齐旧配置缺失的 recognition 子对象/字段并归一化非法值。
 */
export function ensureRecognitionFields(recognition, maxUncommittedS = Infinity) {
    if (!recognition || typeof recognition !== "object" || Array.isArray(recognition)) {
        return {...RECOGNITION_DEFAULTS};
    }
    for (const key of RECOGNITION_KEYS) {
        if (typeof recognition[key] !== "number" || !Number.isFinite(recognition[key])) {
            recognition[key] = RECOGNITION_DEFAULTS[key];
        }
    }
    normalizeRecognitionConfig(recognition, maxUncommittedS);
    return recognition;
}
