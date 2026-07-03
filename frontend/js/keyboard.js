//! 键盘交互：结果导航、激活、ESC 隐藏、修饰键默认行为屏蔽。
//! 0.8.1：Tab / ArrowRight 拦截接受 ghost text 补全（视配置 autosuggest_tab_key）。

import { hideWindow } from "./api.js";
import { activateItem } from "./actions.js";
import * as results from "./results.js";
import * as ghost from "./ghost.js";
import * as autosuggestConfig from "./autosuggest-config.js";
import { resultsEl } from "./dom.js";

/** 绑定全部键盘监听 + 滚轮翻页。 */
export function init() {
  document.addEventListener("keydown", onAutosuggestAccept, true); // 捕获阶段，优先其他 handler
  document.addEventListener("keydown", onNavigation);
  document.addEventListener("keydown", onEscape);
  document.addEventListener("keydown", onBlockModifiers, true);
  // 滚轮翻页：向上滚 = PageUp，向下滚 = PageDown（整页翻，用鼠标就不用手移到方向键了）
  resultsEl.addEventListener("wheel", (e) => {
    e.preventDefault(); // 阻止默认滚动（列表本来就不滚动）
    if (!results.hasItems()) return;
    if (e.deltaY < 0) {
      results.pageUp(); // 向上滚 → 上一页（等价于 PageUp）
    } else {
      results.pageDown(); // 向下滚 → 下一页（等价于 PageDown）
    }
  });
  initAltBadges();
}

// ── Autosuggestion Tab 接受（0.8.1）────────────────────────────────────────

/**
 * Tab / ArrowRight（视配置）+ 有活跃 ghost → 接受补全。
 * 捕获阶段拦截，抢在 onNavigation 之前——ArrowRight 场景下必须先处理，
 * 否则会被 results 的方向键导航吞掉（虽然当前 ArrowRight 无导航语义，仍为将来预留）。
 */
function onAutosuggestAccept(e) {
  // IME 组字期间放行——部分中日韩输入法用 Tab 切候选词，不能被 ghost 吞掉。
  // `isComposing` 是现代 DOM 标准，`keyCode === 229` 是老浏览器兜底。
  if (e.isComposing || e.keyCode === 229) return;
  const tabKey = autosuggestConfig.getTabKey();
  if (e.key !== tabKey) return;
  if (!ghost.hasHint()) return;
  if (ghost.acceptCurrent()) {
    e.preventDefault();
    e.stopPropagation();
  }
}

// ── 导航 / 激活 ───────────────────────────────────────────────────────────────

function onNavigation(e) {
  if (!results.hasItems()) return;

  if (e.key === "ArrowDown") {
    e.preventDefault();
    results.move(1);
  } else if (e.key === "ArrowUp") {
    e.preventDefault();
    results.move(-1);
  } else if (e.key === "PageDown") {
    e.preventDefault();
    results.pageDown();
  } else if (e.key === "PageUp") {
    e.preventDefault();
    results.pageUp();
  } else if (e.key === "Enter") {
    e.preventDefault();
    activateItem(results.getActive());
  } else if (e.altKey && /^[1-9]$/.test(e.key)) {
    // Alt+1~9：直接激活第 N 个候选
    e.preventDefault();
    activateItem(results.getNth(parseInt(e.key, 10)));
  }
}

// ── ESC 隐藏 ──────────────────────────────────────────────────────────────────

function onEscape(e) {
  if (e.key === "Escape") {
    e.preventDefault();
    ghost.clear();
    hideWindow();
  }
}

// ── 屏蔽修饰键/功能键系统默认行为 ─────────────────────────────────────────────
// 防 Alt 激活宿主窗口系统菜单导致 WebView2 消息泵冻结（与 settings.js 录制同理）。
// 不阻止字母数字/方向键/Enter；Alt+数字选候选不受 preventDefault 影响。

function onBlockModifiers(e) {
  if (
    e.key === "Alt" ||
    e.key === "Meta" ||
    /^F\d{1,2}$/.test(e.key) ||
    (e.altKey && (e.key === " " || e.code === "Space"))
  ) {
    e.preventDefault();
  }
}

// ── 按住 Alt 显示数字角标 ─────────────────────────────────────────────────────
// body.alt-active 由 CSS 控制角标显隐。需多重兜底清除，避免 Alt+Tab 切走后状态残留。

function setAlt(on) {
  document.body.classList.toggle("alt-active", on);
}

/** 清除 Alt 角标态（供生命周期 shown/hidden 兜底调用）。 */
export function clearAlt() {
  setAlt(false);
}

function initAltBadges() {
  document.addEventListener("keydown", (e) => {
    if (e.key === "Alt") setAlt(true);
  });
  document.addEventListener("keyup", (e) => {
    if (e.key === "Alt") setAlt(false);
  });
  // 失焦/隐藏兜底：按住 Alt 时窗口失焦（如 Alt+Tab）收不到 keyup，强制清除
  window.addEventListener("blur", () => setAlt(false));
}
