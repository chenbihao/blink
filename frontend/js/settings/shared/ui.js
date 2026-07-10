/**
 * UI 工具模块
 * 设置页共享的 DOM 工具函数
 */

import { t } from "../../i18n/index.js";

/**
 * 清除容器内所有"待保存" badge
 * @param {HTMLElement} container - 容器元素
 */
export function clearUnsaved(container) {
  container.querySelectorAll(".unsaved-badge").forEach((el) => el.remove());
}

/**
 * 在容器内 `.plugin-save` 按钮后挂一个"待保存"徽章（幂等：已存在则不重复插）。
 * 用于手动保存的卡片（插件卡 / 网络卡），字段 change 时提示用户点保存。
 * @param {HTMLElement} container - 容器元素（extension-card 或包含 .plugin-save 的父）
 */
export function markUnsaved(container) {
  if (!container) return;
  const saveBtn = container.querySelector(".plugin-save");
  if (!saveBtn) return;
  const row = saveBtn.parentElement;
  if (!row || row.querySelector(".unsaved-badge")) return;
  const badge = document.createElement("span");
  badge.className = "unsaved-badge";
  badge.textContent = t("plugin.unsaved");
  saveBtn.after(badge);
}
