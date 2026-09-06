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

// ── 下载进度（0.22.14）───────────────────────────────────────────────────────

/** 进度样本窗口上限（约覆盖 2-3 秒，按 200ms 节流的事件频率）。 */
const PROGRESS_MAX_SAMPLES = 12;
/** ETA 估算要求的样本时间跨度下限（ms）——太短速度噪声大。 */
const PROGRESS_ETA_MIN_SPAN_MS = 800;
/** ETA 超过该值（ms）视为不可信（网络停滞/速度骤降），不展示。 */
const PROGRESS_ETA_MAX_MS = 30 * 60 * 1000;

/**
 * 追加一个进度样本到有界窗口（纯函数）。
 *
 * 多文件安装（PP-OCR 的 ORT zip + 3 个模型）逐文件重置 downloaded——
 * 检测到字节回退时清空窗口重新累积，避免窗口内算出负速度。
 *
 * @param {Array<{t:number, bytes:number}>} samples 已有样本（时间升序）
 * @param {number} tMs 事件时间戳（Date.now()）
 * @param {number} bytes 累计已下载字节数
 * @returns {Array<{t:number, bytes:number}>} 新窗口（旧样本超限淘汰）
 */
export function pushProgressSample(samples, tMs, bytes) {
    const arr = Array.isArray(samples) ? samples : [];
    if (arr.length > 0 && bytes < arr[arr.length - 1].bytes) {
        return [{t: tMs, bytes}];
    }
    const next = [...arr, {t: tMs, bytes}];
    return next.length > PROGRESS_MAX_SAMPLES
        ? next.slice(next.length - PROGRESS_MAX_SAMPLES)
        : next;
}

/**
 * 由样本窗口估算剩余毫秒（纯函数）。
 *
 * 窗口首尾差商求平均速度；样本不足、跨度太短、速度为零、总量未知、
 * 已下完或估计超过 30 分钟时返回 null（前端不展示 ETA）。
 *
 * @param {Array<{t:number, bytes:number}>} samples
 * @param {number|null} totalBytes 文件总大小（null = 未知）
 * @returns {number|null} 剩余毫秒；不可估为 null
 */
export function estimateEtaMs(samples, totalBytes) {
    if (!Array.isArray(samples) || samples.length < 2) return null;
    if (!Number.isFinite(totalBytes) || totalBytes <= 0) return null;
    const first = samples[0];
    const last = samples[samples.length - 1];
    const dt = last.t - first.t;
    if (dt < PROGRESS_ETA_MIN_SPAN_MS) return null;
    const rate = (last.bytes - first.bytes) / dt; // bytes/ms
    if (rate <= 0) return null;
    const remaining = totalBytes - last.bytes;
    if (remaining <= 0) return 0;
    const eta = remaining / rate;
    return eta > PROGRESS_ETA_MAX_MS ? null : eta;
}

/** ETA 桶化 + i18n key/params（分钟粒度——ETA 本身是估计值，秒级跳动无意义）。 */
export function etaTextKeyAndParams(etaMs) {
    if (!Number.isFinite(etaMs) || etaMs < 0) return null;
    const totalSec = Math.max(1, Math.ceil(etaMs / 1000));
    if (totalSec < 60) return {key: "welcome.step3.ocr.eta.sec", params: {sec: totalSec}};
    const totalMin = Math.ceil(totalSec / 60);
    if (totalMin < 60) return {key: "welcome.step3.ocr.eta.min", params: {min: totalMin}};
    return {key: "welcome.step3.ocr.eta.hour", params: {hour: Math.ceil(totalMin / 60)}};
}

/**
 * 字节数格式化为人类可读（纯函数，单位通用不做 i18n）。
 * 1024 进制；<100 保留 1 位小数，≥100 取整。
 */
export function formatBytes(bytes) {
    if (!Number.isFinite(bytes) || bytes <= 0) return "0 B";
    if (bytes < 1024) return `${Math.floor(bytes)} B`;
    const kb = bytes / 1024;
    if (kb < 1024) return `${kb.toFixed(kb < 100 ? 1 : 0)} KB`;
    const mb = kb / 1024;
    if (mb < 1024) return `${mb.toFixed(mb < 100 ? 1 : 0)} MB`;
    return `${(mb / 1024).toFixed(1)} GB`;
}

/**
 * 下载百分比（0-100 整数）；总量未知/非法返回 null。
 * 已下载数可能短暂超过 Content-Length（chunk 边界），钳制到 100。
 */
export function progressPercent(downloaded, total) {
    if (!Number.isFinite(total) || total <= 0) return null;
    if (!Number.isFinite(downloaded) || downloaded <= 0) return 0;
    return Math.min(100, Math.floor((downloaded / total) * 100));
}
