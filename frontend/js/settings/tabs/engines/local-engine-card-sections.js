/**
 * 引擎卡片状态行渲染（0.22.6 从 local-engine-card.js 拆出，中密度重设计收敛）。
 *
 * 只做视觉投影，不做业务推导：
 * - keyline：环境 / 模型 / 服务 / 生命周期策略 四枚 badge。
 *   进程状态不再与环境/服务/模型同权重常驻——瞬态进综合摘要，PID 进诊断。
 *
 * 状态→label/class 的投影来自 local-engine-summary.js 纯函数；
 * 内部存储概念（generation/install_id/provider/refcount/transaction slot）
 * 不出现在任何展示文案中。
 *
 * @module local-engine-card-sections
 */

import {tt} from "./local-engine-card-utils.js";
import {computeKeyline} from "./local-engine-summary.js";

// ── 关键状态行 ─────────────────────────────────────────────────────────────────

/**
 * 更新 keyline（环境/模型/服务/策略）。
 * 签名脏检查：高频日志/下载事件不重建 badge。
 *
 * @param {HTMLElement} el - .le-keyline 容器
 * @param {Object} entry - EngineStateEntry
 * @param {Object} i18n
 */
export function updateKeyline(el, entry, i18n) {
    const items = computeKeyline(entry, (key, params) => {
        const value = tt(i18n, key, key, params);
        return value !== key ? value : undefined;
    });

    const sig = items.map((item) => `${item.label}=${item.value}:${item.cls}`).join("|");
    if (el.dataset.renderSig === sig) return;
    el.dataset.renderSig = sig;

    el.textContent = "";
    for (const item of items) {
        if (!item.value) continue;
        const cell = document.createElement("span");
        cell.className = "le-keyline-item";

        const label = document.createElement("span");
        label.className = "le-keyline-label";
        label.textContent = item.label;

        const value = document.createElement("span");
        value.className = `le-keyline-value status-badge ${item.cls}`;
        value.textContent = item.value;

        cell.appendChild(label);
        cell.appendChild(value);
        el.appendChild(cell);
    }
}
