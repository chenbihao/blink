/**
 * 引擎卡片渲染辅助（0.22.6 从 local-engine-card.js 拆出）。
 *
 * 纯渲染辅助：i18n 取词、字节格式化、badge/label/value 构造、
 * 剪贴板复制反馈。不含业务规则——状态→CSS class 的映射是纯视觉投影。
 *
 * @module local-engine-card-utils
 */

import {copyToClipboard} from "../../../shared/api.js";
import {t} from "../../../i18n/index.js";

/**
 * i18n 取词：优先调用方注入的 i18n 对象，未命中回落到默认 t() 与 fallback。
 * @param {Object|null} i18n - 可含 t(key, params) 的注入对象
 * @param {string} key
 * @param {string} fallback
 * @param {Object} [params]
 * @returns {string}
 */
export function tt(i18n, key, fallback, params) {
    if (i18n && typeof i18n.t === "function") return i18n.t(key, params) || fallback;
    const raw = t(key, params);
    return raw !== key ? raw : fallback;
}

/**
 * 状态 wire value → 视觉 class（环境/服务/模型共用投影）。
 * @param {string|null} value
 * @returns {string}
 */
export function statusClass(value) {
    if (!value) return "status-unknown";
    const map = {
        missing: "status-unavailable",
        ready: "status-available",
        broken: "status-unavailable",
        needs_rebuild: "status-warning",
        unknown: "status-unknown",
        unreachable: "status-unavailable",
        healthy: "status-available",
        degraded: "status-warning",
        not_loaded: "status-unknown",
        downloading: "status-warning",
        loading: "status-warning",
        failed: "status-unavailable",
    };
    return map[value] || "status-unknown";
}

/**
 * badge 徽章。
 * @param {string} text
 * @param {string} cls
 * @returns {HTMLElement}
 */
export function makeBadge(text, cls) {
    const badge = document.createElement("span");
    badge.className = `le-badge ${cls} status-badge`;
    badge.textContent = text;
    return badge;
}

/**
 * 信息行标签。
 * @param {string} text
 * @returns {HTMLElement}
 */
export function makeLabel(text) {
    const span = document.createElement("span");
    span.className = "le-info-label";
    span.textContent = text;
    return span;
}

/**
 * 信息行值。
 * @param {string} text
 * @returns {HTMLElement}
 */
export function makeValue(text) {
    const span = document.createElement("span");
    span.className = "le-info-value";
    span.textContent = text;
    return span;
}

/**
 * 字节数格式化（B/MB/GB）。
 * @param {number} bytes
 * @returns {string}
 */
export function formatBytes(bytes) {
    if (!bytes || bytes === 0) return "0 B";
    const mb = bytes / (1024 * 1024);
    if (mb < 1024) return `${Math.round(mb)} MB`;
    return `${(mb / 1024).toFixed(1)} GB`;
}

/**
 * MB 数值格式化（MB/GB）。
 * @param {number|null} mb
 * @returns {string}
 */
export function formatMB(mb) {
    if (mb == null) return "—";
    if (mb < 1024) return `${mb} MB`;
    return `${(mb / 1024).toFixed(1)} GB`;
}

/**
 * CSS 选择器转义（engine id 只允许字母数字下划线中划线，其余转义）。
 * @param {string} s
 * @returns {string}
 */
export function cssEscape(s) {
    return String(s).replace(/[^a-zA-Z0-9_-]/g, (c) => `\\${c}`);
}

/**
 * 通过后端剪贴板 command 复制文本并给按钮反馈。
 * WebView 中 navigator.clipboard 可能因权限/焦点被静默拒绝，不能作为可靠路径。
 * @param {HTMLElement} button
 * @param {string} text
 * @param {Object|null} i18n
 */
export async function copyTextWithFeedback(button, text, i18n) {
    if (!text || button.dataset.copying === "true") return;
    const original = button.textContent;
    button.dataset.copying = "true";
    button.disabled = true;
    try {
        await copyToClipboard(text);
        button.textContent = tt(i18n, "local_engine.log.copied", "已复制");
    } catch (error) {
        console.error("[local-engine] copy failed:", error);
        button.textContent = tt(i18n, "local_engine.log.copy_failed", "复制失败");
    } finally {
        window.setTimeout(() => {
            button.textContent = original;
            button.disabled = false;
            button.dataset.copying = "false";
        }, 1200);
    }
}
