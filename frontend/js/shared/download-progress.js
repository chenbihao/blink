/**
 * 下载进度纯函数（0.22.14）——欢迎页引导与设置页引擎页共用。
 *
 * 输入是 install-progress 事件的字节进度（{downloaded, total}），输出
 * 进度条渲染所需的可展示数据：样本窗口维护、ETA 估算、字节/百分比格式化。
 *
 * 纯模块铁则：无 DOM、无 Tauri、无 i18n 依赖（etaTextKeyAndParams 只产出
 * key + params，由调用方经 t() 取词）。
 *
 * 多文件安装（PP-OCR 的 ORT zip + 3 个模型；FunASR GGUF 多分片）逐文件
 * 重置 downloaded——pushProgressSample 检测到字节回退时清空窗口重新累积。
 */

// ── 内部常量 ─────────────────────────────────────────────────────────────────

/** 进度样本窗口上限（约覆盖 2-3 秒，按 200ms 节流的事件频率）。 */
const PROGRESS_MAX_SAMPLES = 12;
/** ETA 估算要求的样本时间跨度下限（ms）——太短速度噪声大。 */
const PROGRESS_ETA_MIN_SPAN_MS = 800;
/** ETA 超过该值（ms）视为不可信（网络停滞/速度骤降），不展示。 */
const PROGRESS_ETA_MAX_MS = 30 * 60 * 1000;

/**
 * 追加一个进度样本到有界窗口（纯函数）。
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

/**
 * ETA 桶化 + i18n key/params（分钟粒度——ETA 本身是估计值，秒级跳动无意义）。
 *
 * @param {number|null} etaMs estimateEtaMs 产物
 * @returns {{key: string, params: Object}|null} t() 可消费的取词参数；不可展示为 null
 */
export function etaTextKeyAndParams(etaMs) {
    if (!Number.isFinite(etaMs) || etaMs < 0) return null;
    const totalSec = Math.max(1, Math.ceil(etaMs / 1000));
    if (totalSec < 60) return {key: "local_engine.progress.eta.sec", params: {sec: totalSec}};
    const totalMin = Math.ceil(totalSec / 60);
    if (totalMin < 60) return {key: "local_engine.progress.eta.min", params: {min: totalMin}};
    return {key: "local_engine.progress.eta.hour", params: {hour: Math.ceil(totalMin / 60)}};
}

/**
 * 字节数格式化为人类可读（纯函数，单位通用不做 i18n）。
 * 1024 进制；<100 保留 1 位小数，≥100 取整。
 *
 * @param {number} bytes
 * @returns {string}
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
 *
 * @param {number} downloaded
 * @param {number|null} total
 * @returns {number|null}
 */
export function progressPercent(downloaded, total) {
    if (!Number.isFinite(total) || total <= 0) return null;
    if (!Number.isFinite(downloaded) || downloaded <= 0) return 0;
    return Math.min(100, Math.floor((downloaded / total) * 100));
}
