//! 底部提示栏：显示当前选中项的动作提示（左）+ 翻页提示（右）。
//! 无结果时整条隐藏（不占窗口高度）。
//!
//! **0.8.1 优化**：接入 ghost 状态——有 hint 时左侧显示"[Tab] 接受补全 → fanyi"，
//! 视觉与 overlay 的"影子灰字"分开：overlay 只画 ghost text，键帽 chip 走 statusbar。
//! 这也是 Raycast / Warp 的做法（active UI hint 归 statusbar，passive suggestion 归 inline overlay）。
//!
//! **文案键帽化**：所有含键位的提示（"↑↓ 选择" / "Enter 打开" / "PgUp/PgDn 翻页"）
//! 全部改用 i18n 模板 `{{key:X}}` 占位符 + `renderHint` 渲染出 <kbd> DOM。

import { actionHint } from "./hints.js";
import { t } from "./i18n.js";
import * as ghost from "./ghost.js";
import * as autosuggestConfig from "./autosuggest-config.js";
import { renderKey, renderHint } from "./kbd.js";

const el = document.getElementById("statusbar");

/** 缓存最近一次 update() 的入参，供 ghost onChange 回调时重绘用。 */
let lastActive = null;
let lastPaging = { page: 1, pageCount: 1 };

/** 初始化：订阅 ghost 状态。main.js 在 ghost.init() 之后调一次。 */
export function init() {
  ghost.onChange(() => render());
}

/**
 * 刷新提示栏。
 * @param {{action?: {kind: string, hint?: string}}|null} active 当前选中项
 * @param {{page: number, pageCount: number}} paging 翻页信息
 */
export function update(active, paging) {
  lastActive = active;
  lastPaging = paging || { page: 1, pageCount: 1 };
  render();
}

function render() {
  // 隐藏条件：无候选 AND 无 ghost hint。有 ghost hint 时即使无候选也要显示
  // （用户输入 `fy` 时可能还没有本地结果，仍要提示可 Tab 接受）。
  const hasHint = ghost.hasHint();
  if (!lastActive && !hasHint) {
    el.classList.remove("visible");
    el.replaceChildren();
    return;
  }

  el.replaceChildren();
  el.appendChild(buildLeft(lastActive, hasHint));
  const right = buildRight(lastPaging);
  if (right) el.appendChild(right);
  el.classList.add("visible");
}

/** 左侧：ghost 提示 优先；否则显示"导航 · Alt+数字 · 当前项动作"。 */
function buildLeft(active, hasHint) {
  const left = document.createElement("span");
  left.className = "hint-left";

  if (hasHint) {
    // {key} 传入 kbd Element，模板里 "按 {key} 接受补全 → {target}" 会自动内嵌。
    const display = ghost.currentDisplay();
    const params = { key: renderKey(autosuggestConfig.getTabKey()) };
    if (display) {
      left.appendChild(renderHint(t("statusbar.autosuggest_accept"),
        { ...params, target: display }));
    } else {
      left.appendChild(renderHint(t("statusbar.autosuggest_enter"), params));
    }
    // 0.8.3 §4.9：Context Suggestion 追加 origin 提示（· 来自划词 / · 来自剪贴板）,
    // Keyword 无 origin → currentOrigin() 返回 null,不追加。
    const origin = ghost.currentOrigin();
    if (origin) {
      const originKey = `suggestion.origin.${origin}`;
      const originText = t(originKey);
      // 降级保护：t() 未命中会返回 key 本身,不显示"suggestion.origin.selection"这种字面串
      if (originText && originText !== originKey) {
        left.appendChild(document.createTextNode(" · "));
        const originEl = document.createElement("span");
        originEl.className = "hint-origin";
        originEl.textContent = originText;
        left.appendChild(originEl);
      }
    }
    return left;
  }

  // 常规态：导航 · Alt+数字 · 动作提示——三段用 · 分隔，各段都走 renderHint 支持键帽。
  if (active) {
    left.appendChild(renderHint(t("hint.navigate")));
    left.appendChild(document.createTextNode(" · "));
    left.appendChild(renderHint(t("hint.alt_number")));
    left.appendChild(document.createTextNode(" · "));
    const { template, params } = actionHint(active.action);
    left.appendChild(renderHint(template, params));
  }
  return left;
}

/** 右侧：多于一屏才显示翻页提示。 */
function buildRight(paging) {
  if (paging.pageCount <= 1) return null;
  const right = document.createElement("span");
  right.className = "hint-right";
  right.appendChild(renderHint(t("statusbar.paging"), {
    page: paging.page,
    pageCount: paging.pageCount,
  }));
  return right;
}
