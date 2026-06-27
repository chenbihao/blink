//! 底部提示栏：显示当前选中项的动作提示（左）+ 翻页提示（右）。
//! 无结果时整条隐藏（不占窗口高度）。未来可承载智能提示/更新提示。

import { actionHint } from "./hints.js";
import { t } from "./i18n.js";

const el = document.getElementById("statusbar");

/**
 * 刷新提示栏。
 * @param {{action?: {kind: string, hint?: string}}|null} active 当前选中项
 * @param {{page: number, pageCount: number}} paging 翻页信息
 */
export function update(active, paging) {
  if (!active) {
    el.classList.remove("visible");
    el.innerHTML = "";
    return;
  }

  const left = actionHint(active.action);
  // 多于一屏才显示翻页提示
  const right =
    paging.pageCount > 1
      ? t("statusbar.paging", { page: paging.page, pageCount: paging.pageCount })
      : "";

  el.innerHTML = "";
  el.appendChild(span("hint-left", left));
  if (right) el.appendChild(span("hint-right", right));
  el.classList.add("visible");
}

function span(cls, text) {
  const s = document.createElement("span");
  s.className = cls;
  s.textContent = text;
  return s;
}
