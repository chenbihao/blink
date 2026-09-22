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

/** 从引擎状态中提取仍在进行的安装/修复 operation id。终态和 idle 不绑定。 */
export function activeOperationId(statusDto) {
    const operation = statusDto?.status?.operation;
    if (!operation || operation.kind === "idle" || !operation.operation_id) return null;
    return classifyInstallStage(operation.stage) === "active"
        ? String(operation.operation_id)
        : null;
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

// ── Chord toggles 竞态防护（纯函数，供测试）─────────────────────────────────────

/**
 * 判断某次异步保存的响应是否仍然有效（revision 匹配当前最新）。
 *
 * 快速连续切换 chord 开关时，旧请求可能比新请求晚完成。
 * 旧请求的响应必须被丢弃，否则会覆盖用户最后一次操作。
 *
 * @param {number} requestRevision - 发起请求时的 revision
 * @param {number} currentRevision - 当前最新 revision
 * @returns {boolean} true = 仍然有效，可以提交；false = 已过期，丢弃
 */
export function isChordToggleRevisionValid(requestRevision, currentRevision) {
    return Number(requestRevision) === Number(currentRevision);
}

/**
 * 构造失败回滚后的 toggle 值。
 *
 * @param {object} confirmed - 最后一次后端已确认的值 { chord_enabled, chord_hint_visible }
 * @returns {{chord_enabled: boolean, chord_hint_visible: boolean}}
 */
export function rollbackChordToggles(confirmed) {
    return {
        chord_enabled: confirmed?.chord_enabled === true,
        chord_hint_visible: confirmed?.chord_hint_visible !== false,
    };
}

// ── Chord 全局快捷键开关（纯函数，供测试）─────────────────────────────────────

/**
 * 返回开启/关闭某 chord 动作全局快捷键后的 chord_bindings 新对象（不改原对象）。
 *
 * 与设置页 chord tab 同一契约（0.22.12）：开启 = 写入 `{mode:"follow_chord"}`
 * （跟随触发键，零配置生效）；关闭 = 删除 global 字段，保留 key/modifiers 等
 * 其他字段。动作条目不存在时创建空触发键条目（后端按 default_key 解析生效键）。
 *
 * @param {object|null} bindings - get_config 返回的 chord_bindings（id → binding）
 * @param {string} id - chord 动作 id（如 "chat"）
 * @param {boolean} enabled
 * @returns {object} 新的 chord_bindings（顶层与目标条目均为浅拷贝）
 */
export function applyChordGlobalToBindings(bindings, id, enabled) {
    const next = {...(bindings || {})};
    const entry = {...(next[id] ?? {key: "", modifiers: ["alt"]})};
    if (enabled) {
        entry.global = {mode: "follow_chord"};
    } else if (entry.global) {
        delete entry.global;
    }
    next[id] = entry;
    return next;
}

// ── 安装进度事件 operation_id 隔离（纯函数，供测试）───────────────────────────

/**
 * 判断一条安装进度/阶段事件是否应该被当前操作接受。
 *
 * **铁则：operation_id 不从事件绑定**——operation_id 必须从后端命令返回值
 * 或 get_local_engine_status 主动获取，不允许从首条事件猜测绑定
 * （首事件可能来自旧操作的迟到推送）。
 *
 * 接受规则：
 * - 已绑定 operation_id 时，事件 operation_id 必须匹配（否则丢弃）。
 * - 未绑定 operation_id 时拒绝事件，直到状态查询建立可信绑定。
 * - 事件无 operation_id 时拒绝，不能绕过身份隔离。
 *
 * @param {string|null} currentOpId - 当前已从后端获取的 operation_id
 * @param {string|null|undefined} eventOpId - 事件携带的 operation_id
 * @returns {{accept: boolean, newOpId: string|null}}
 *   accept = 是否接受此事件；newOpId = 始终等于 currentOpId（不从事件绑定）
 */
export function shouldAcceptInstallEvent(currentOpId, eventOpId) {
    // 无可信绑定时，任何事件都无法证明属于当前操作；宁可暂时不显示早期
    // 进度，也不能让上一操作的迟到事件污染新操作 UI。
    if (!currentOpId || !eventOpId) {
        return {accept: false, newOpId: currentOpId ?? null};
    }
    return {
        accept: String(eventOpId) === String(currentOpId),
        newOpId: currentOpId,
    };
}
