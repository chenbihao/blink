/**
 * 共享消息渲染组件（0.17.6 B2.6）。
 *
 * 从 chat/components.js 抽取的工具状态卡、确认卡和 rAF 节流渲染器，
 * 供主窗口 AI 模式和对话窗口共用。
 *
 * 无 bundler 铁则：不 import vendor 脚本，通过 window.* 访问。
 */

import { t } from "../i18n/index.js";

/**
 * 渲染 typing 指示器（三点跳动动画）。
 * @returns {string} HTML 字符串
 */
export function renderTypingIndicator() {
  return '<div class="ai-typing"><span></span><span></span><span></span></div>';
}

/**
 * 渲染工具状态行（单行，替换式）。
 * 主窗口 #ai-tool-line 用：每次 tool_call chunk 替换内容，不积累历史。
 *
 * @param {string} toolName 工具名称
 * @param {string} [args] 工具参数 JSON 字符串（可选）
 * @returns {string} HTML 字符串
 */
export function renderToolLine(toolName, args) {
  let argsPreview = "";
  if (args && args.trim() && args.trim() !== "{}") {
    try {
      const parsed = JSON.parse(args);
      const entries = Object.entries(parsed);
      if (entries.length > 0) {
        const parts = entries.slice(0, 3).map(([k, v]) => {
          const val = typeof v === "string" ? v : JSON.stringify(v);
          const truncated = val.length > 40 ? val.slice(0, 40) + "…" : val;
          return `${k}: ${truncated}`;
        });
        argsPreview = ` <span class="ai-tool-args">${escapeText(parts.join(" · "))}</span>`;
      }
    } catch {
      // 非 JSON 参数，直接显示
      const truncated = args.length > 60 ? args.slice(0, 60) + "…" : args;
      argsPreview = ` <span class="ai-tool-args">${escapeText(truncated)}</span>`;
    }
  }
  const spinner = '<span class="ai-tool-spinner"></span>';
  return `${spinner}<span class="ai-tool-name">${escapeText(toolName)}</span>${argsPreview}`;
}

/**
 * 渲染工具结果摘要（替换工具行的 spinner 为完成状态）。
 *
 * @param {string} toolName
 * @param {boolean} success
 * @param {string} [summary] 结果摘要
 * @returns {string} HTML 字符串
 */
export function renderToolResultLine(toolName, success, summary) {
  const icon = success ? "✓" : "✕";
  const cls = success ? "ai-tool-done" : "ai-tool-fail";
  let summaryHtml = "";
  if (summary) {
    const truncated = summary.length > 80 ? summary.slice(0, 80) + "…" : summary;
    summaryHtml = ` <span class="ai-tool-summary">${escapeText(truncated)}</span>`;
  }
  return `<span class="${cls}">${icon}</span><span class="ai-tool-name">${escapeText(toolName)}</span>${summaryHtml}`;
}

/**
 * 渲染危险操作确认卡片。
 *
 * @param {{confirm_id: number, tool_name: string, tool_type: string, danger_class: string}} payload
 * @param {(confirmId: number, approved: boolean) => void} onConfirm
 * @returns {HTMLElement} 卡片元素
 */
export function renderConfirmCard(payload, onConfirm) {
  const el = document.createElement("div");
  el.className = "ai-confirm-card";
  el.innerHTML = `
    <div class="ai-confirm-card-title">${escapeText(t("ai.confirm_title"))}</div>
    <div class="ai-confirm-card-tool">
      ${escapeText(payload.tool_type || "")}: <strong>${escapeText(payload.tool_name)}</strong>
    </div>
    <div class="ai-confirm-card-actions">
      <button class="ai-confirm-btn ai-confirm-btn-reject" data-action="reject">${escapeText(t("ai.confirm_reject"))}</button>
      <button class="ai-confirm-btn ai-confirm-btn-approve" data-action="approve">${escapeText(t("ai.confirm_approve"))}</button>
    </div>
  `;
  el.querySelector("[data-action='reject']").addEventListener("click", () => {
    onConfirm(payload.confirm_id, false);
    el.querySelector(".ai-confirm-card-actions").innerHTML =
      `<span class="ai-confirm-status ai-confirm-status-rejected">${escapeText(t("ai.confirm_rejected"))}</span>`;
  });
  el.querySelector("[data-action='approve']").addEventListener("click", () => {
    onConfirm(payload.confirm_id, true);
    el.querySelector(".ai-confirm-card-actions").innerHTML =
      `<span class="ai-confirm-status ai-confirm-status-approved">${escapeText(t("ai.confirm_approved"))}</span>`;
  });
  return el;
}

/**
 * 创建 rAF 节流渲染器。
 *
 * 多次调用 schedule() 只在下一帧执行一次 updateFn，
 * 避免每个流式 chunk 都触发 DOM 重绘。
 *
 * @param {(text: string) => void} updateFn 接收累积文本的更新函数
 * @returns {{ schedule: (text: string) => void, cancel: () => void }}
 */
export function createThrottledRenderer(updateFn) {
  let rafId = null;
  let pendingText = "";

  function schedule(text) {
    pendingText = text;
    if (rafId !== null) return;
    rafId = requestAnimationFrame(() => {
      rafId = null;
      updateFn(pendingText);
    });
  }

  function cancel() {
    if (rafId !== null) {
      cancelAnimationFrame(rafId);
      rafId = null;
    }
  }

  return { schedule, cancel };
}

/**
 * HTML 特殊字符转义。
 * @param {string} text
 * @returns {string}
 */
function escapeText(text) {
  const div = document.createElement("div");
  div.textContent = String(text ?? "");
  return div.innerHTML;
}
