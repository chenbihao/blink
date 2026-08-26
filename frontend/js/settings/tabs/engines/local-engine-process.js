/**
 * 进程状态渲染纯函数（0.22.5 H1）。
 *
 * 从 local-engine-card.js 提取，使其可在 Node.js 测试环境中测试，
 * 不依赖 DOM / icon / i18n 模块。
 *
 * 使用 ProcessStateDto shape：`{ state, pid?, reason? }`。
 *
 * @module local-engine-process
 */

/**
 * 进程状态显示文案。
 *
 * 使用 ProcessStateDto shape：`{ state, pid?, reason? }`。
 * 前端按 `state` 字段分支，`pid` / `reason` 是可选字段。
 * 未知 shape → fail closed 显示 "unknown"。
 *
 * **安全铁则**：pid/reason 使用 textContent 渲染（由调用方保证），
 * 此函数只返回字符串。
 *
 * @param {Object|null} process - ProcessStateDto
 * @returns {string}
 */
export function processDisplay(process) {
    if (!process || typeof process !== "object") return "unknown";
    const state = process.state;
    if (typeof state !== "string") return "unknown";
    switch (state) {
        case "stopped":
            return "stopped";
        case "starting":
            return "starting";
        case "running":
            return typeof process.pid === "number" ? `running (pid=${process.pid})` : "running";
        case "stopping":
            return "stopping";
        case "exited":
            return "exited";
        default:
            return "unknown";
    }
}

/**
 * 进程状态 CSS class。
 *
 * 使用 ProcessStateDto shape：`{ state, pid?, reason? }`。
 * 未知 shape → fail closed 返回 "status-unknown"。
 *
 * @param {Object|null} process - ProcessStateDto
 * @returns {string}
 */
export function processClass(process) {
    if (!process || typeof process !== "object") return "status-unknown";
    const state = process.state;
    if (typeof state !== "string") return "status-unknown";
    switch (state) {
        case "stopped":
            return "status-unknown";
        case "running":
            return "status-available";
        case "starting":
        case "stopping":
            return "status-warning";
        case "exited":
            return "status-unavailable";
        default:
            return "status-unknown";
    }
}
