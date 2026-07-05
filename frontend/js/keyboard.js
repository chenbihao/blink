//! 键盘交互：结果导航、激活、ESC 隐藏、修饰键默认行为屏蔽。
//! 0.8.1：Tab / ArrowRight 拦截接受 ghost text 补全（视配置 autosuggest_tab_key）。

import { hideWindow, triggerChord, isAltDown } from "./api.js";
import { activateItem } from "./actions.js";
import * as results from "./results.js";
import * as ghost from "./ghost.js";
import * as chord from "./chord.js";
import * as autosuggestConfig from "./autosuggest-config.js";
import { queryEl, resultsEl } from "./dom.js";

/** 绑定全部键盘监听 + 滚轮翻页。 */
export function init() {
  // 0.8.5 Chord：Alt+字母触发（捕获阶段，最优先；独立于 onNavigation，不依赖 hasItems）
  document.addEventListener("keydown", onChordTrigger, true);
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
  // alt-active 状态由轮询驱动（startAltPoll/stopAltPoll），lifecycle shown/hidden 控制
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

// ── Chord 触发（0.8.5）：Alt+字母 → trigger_chord ──────────────────────────────
// 现状 onNavigation 的 altKey 分支只覆盖 Alt+1~9（选候选，且依赖 hasItems）。
// Chord 触发不依赖搜索结果，故独立 handler + 捕获阶段优先。
// 时序自洽（§6.2）：Alt+Q 的 Q 是 hook 状态机的「异键」→ aborted → Alt keyup 判 hold
// → 不发 Tap → 不 toggle hide；前端 preventDefault 保 Q 不进输入框。
//
// **门禁**（§6.4 修正）：Chord 只在「用户还没开始交互」时触发——
//   query 为空 **且** 结果列表为空。任一非空说明用户已在打字/浏览结果，
//   此时 Alt+字母应正常进搜索框，别被 Chord 吞掉。
//   典型场景：用户输入"剪贴板"后按 Alt+C 想输入 C——不该触发 Chord，字母正常入框。

const CHORD_KEYS = new Set(["a", "q", "c"]);

// 触发后端 trigger_chord（Alt+Q 划词会在后端显示 chord-ball 悬浮窗）。
function fireChord(key) {
  console.log(`[chord] Alt+${key.toUpperCase()} triggered`);
  triggerChord(key).catch((e) => console.warn("[chord] trigger_chord 失败", e));
}

function onChordTrigger(e) {
  if (!e.altKey) return;
  if (e.isComposing || e.keyCode === 229) return; // IME 组字放行
  const key = e.key.toLowerCase();
  if (!CHORD_KEYS.has(key)) return;
  // 门禁：query 空 + 结果为空才触发（用户还没开始交互）。
  // trim 是防"只有空格"也被判非空。同 setAlt 里的可见性门禁一致——
  // 触发与显示门禁共用同一条件，避免"菜单不该显示但按 Alt+C 生效"或反之。
  if (!chordEligible()) return;
  e.preventDefault(); // 不进输入框
  e.stopPropagation();
  fireChord(key);
}

/** Chord 触发/显示的统一门禁条件。
 *  三重与门:
 *  - Chord 总开关未关(chord_enabled,读自 chord.js 快照)
 *  - query 空
 *  - 结果列表空
 */
function chordEligible() {
  return (
    chord.isEnabled() &&
    queryEl.value.trim() === "" &&
    !results.hasItems()
  );
}

// ── 按住 Alt 显示数字角标 ─────────────────────────────────────────────────────
// body.alt-active 由 CSS 控制角标显隐。需多重兜底清除，避免 Alt+Tab 切走后状态残留。

function setAlt(on) {
  // 0.8.5 §6.4：菜单可见性与触发资格同源——只有 chordEligible() 满足才允许展示。
  // 用户输入后按 Alt 不该弹出菜单遮 Ghost / results（触发路径也会被 onChordTrigger 门禁挡住，
  // 双闸保一致）。alt-active 仍标记物理 Alt 态供其他 UI（如 results 上的 Alt+1~9 角标）用。
  const showChord = on && chordEligible();
  const prevChordVisible = document.body.classList.contains("chord-visible");
  document.body.classList.toggle("alt-active", on);
  document.body.classList.toggle("chord-visible", showChord);
  // Chord 提示在 ghost overlay（无 Ghost 时）或 statusbar（有 Ghost 时），
  // 状态变化需通知 statusbar 重绘。
  if (showChord !== prevChordVisible) {
    chord.notifyVisibilityChange();
  }
}

/** 清除 Alt 角标态（供生命周期 shown/hidden 兜底调用）。 */
export function clearAlt() {
  setAlt(false);
}

// alt-active 轮询（0.8.5 §6.1）：WebView2 不转发 Alt 键自身的 keydown 到 JS
// （系统键被 Windows 用于菜单激活），keydown 监听不可靠。改由 lifecycle shown 启动
// 轮询、hidden 停止，每 100ms 查物理态，状态变化才 setAlt（避免无谓 resize）。
let altPollTimer = null;
let altLast = false;

export function startAltPoll() {
  stopAltPoll();
  const tick = async () => {
    let down = false;
    try {
      down = await isAltDown();
    } catch (e) {
      console.warn("[alt] is_alt_down 失败", e);
    }
    if (down !== altLast) {
      altLast = down;
      setAlt(down);
    }
  };
  tick();
  altPollTimer = window.setInterval(tick, 100);
}

export function stopAltPoll() {
  if (altPollTimer) window.clearInterval(altPollTimer);
  altPollTimer = null;
  if (altLast) {
    altLast = false;
    setAlt(false);
  }
}
