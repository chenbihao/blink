/**
 * 本地引擎日志展示格式。
 *
 * wire contract 保留 RFC 3339，展示层转换为用户本地时间，避免直接显示
 * `2026-08-29T10:48:54.615+00:00` 这类冗长前缀。
 */

function pad(value, width = 2) {
    return String(value).padStart(width, "0");
}

/** 将 RFC 3339 时间转换为本地 `MM-DD HH:mm:ss.SSS`。 */
export function formatLocalLogTimestamp(timestamp, dateFactory = (value) => new Date(value)) {
    if (!timestamp) return "—";
    const date = dateFactory(timestamp);
    if (!date || Number.isNaN(date.getTime())) return String(timestamp);
    return `${pad(date.getMonth() + 1)}-${pad(date.getDate())} ` +
        `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}.` +
        `${pad(date.getMilliseconds(), 3)}`;
}

/** 生成与界面一致、可直接粘贴排查的单行日志文本。 */
export function formatLogLine(log) {
    const timestamp = formatLocalLogTimestamp(log?.timestamp);
    const level = log?.level || "info";
    const text = log?.text || "";
    return `[${timestamp}] [${level}] ${text}`;
}
