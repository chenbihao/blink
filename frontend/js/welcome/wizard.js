/**
 * 0.22.13 引导向导纯逻辑（无 DOM / 无副作用，供 welcome.js 与测试共用）。
 *
 * 步骤状态机 + OCR 引导编排的纯函数部分。
 * DOM 渲染与 invoke 调用一律留在 welcome.js 入口模块。
 */

// ── 步骤状态机 ────────────────────────────────────────────────────────────────

export const STEP_COUNT = 4;

/** 步骤下标合法范围 [0, STEP_COUNT)。 */
export function clampStep(step) {
    const n = Number(step);
    if (!Number.isFinite(n)) return 0;
    return Math.min(Math.max(Math.trunc(n), 0), STEP_COUNT - 1);
}

export function canGoBack(step) {
    return clampStep(step) > 0;
}

export function canGoNext(step) {
    return clampStep(step) < STEP_COUNT - 1;
}

export function isLastStep(step) {
    return clampStep(step) === STEP_COUNT - 1;
}

export function nextStep(step) {
    return clampStep(step + 1);
}

export function prevStep(step) {
    return clampStep(step - 1);
}

// ── OCR 引导编排 ──────────────────────────────────────────────────────────────

/** OCR 增强引擎（稳定 engine id，与后端 PADDLEOCR_ENGINE_ID 对应）。 */
export const OCR_ENGINE_ID = "paddleocr";

/**
 * 从 `get_local_engine_status` 的返回列表中取目标引擎项。
 */
export function pickEngineStatus(list, engineId) {
    if (!Array.isArray(list)) return null;
    return list.find((s) => s && s.engine_id === engineId) ?? null;
}

/**
 * 引擎级状态 → OCR 是否已就绪可用。
 *
 * PP-OCR 的 ORT DLL 与模型在同一个安装事务内联合提交（0.22 §3.9），
 * `environment === "ready"` 即已安装可用。模型目录（list_engine_models）
 * 只注册了 FunASR，OCR 查出来恒为空数组，不能作为判定源。
 */
export function isOcrReady(statusDto) {
    const wire = statusDto && typeof statusDto === "object" ? statusDto.status : null;
    return Boolean(wire) && wire.environment === "ready";
}

/**
 * install-stage 事件 stage 值分类。
 *
 * @returns {"active"|"done"|"failed"} active=进行中（继续显示进度），
 *   done=成功终态，failed=失败/取消终态（回到可重试态）。
 */
export function classifyInstallStage(stage) {
    switch (stage) {
        case "completed":
            return "done";
        case "failed":
        case "cancelled":
            return "failed";
        default:
            // pending/preparing/downloading/verifying/promoting/switching/validating 及未知值
            // 一律按进行中处理（未知 stage 不误报失败）
            return "active";
    }
}

/** stage 显示文案的 i18n key（复用引擎页既有 key，缺 key 时 t() 回退 key 本身）。 */
export function installStageTextKey(stage) {
    return `local_engine.operation.stage.${stage}`;
}

// 注：下载进度纯函数（样本窗口/ETA/格式化）已提取到 ../shared/download-progress.js，
// 由欢迎页与设置页引擎页共用（0.22.14）。
