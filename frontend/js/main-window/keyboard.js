//! 键盘交互：结果导航、激活、ESC 隐藏、修饰键默认行为屏蔽。
//! 0.8.1：Tab / ArrowRight 拦截接受 ghost text 补全（视配置 autosuggest_tab_key）。

import { hideWindow, triggerChord, isAltDown, setChordMode, getAwarenessText } from "../shared/api.js";
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

// 触发后端 trigger_chord（Alt+A 截图 / Alt+C 剪贴板等 tap 语义动作）。
// 注意：Alt+Space 语音输入（hold 语义）不走此路径——由 native hotkey hold 状态机直接处理。
// 0.10.7：触发键集合从 chord.getTapKeys() 动态获取（用户可配置），不再硬编码。
// 0.16.9：E/S 键做 contextual 解析——active item 文本 payload > 非空 query > Awareness 选区 > 空白。
async function fireChord(key) {
  console.log(`[chord] Alt+${key.toUpperCase()} triggered`);
  let inputText = queryEl.value;

  // 0.16.9：E/S 需要 contextual 解析
  if (key === "e" || key === "s") {
    inputText = await resolveContextualContent();
  }

  triggerChord(key, inputText).catch((e) => console.warn("[chord] trigger_chord 失败", e));
}

/**
 * 0.16.9：为 chord E/S 解析上下文内容。
 *
 * 解析顺序（§3.13）：
 * 1. active item 的文本 payload（copy action 的 payload）
 * 2. 非空 query
 * 3. 空闲态 Awareness 选区（仅当 query 为空且无结果时）
 * 4. 空白
 */
async function resolveContextualContent() {
  // 1. active item 的文本 payload
  const active = results.getActive();
  if (active && !active.isError) {
    const firstAction = active.actions?.[0];
    if (firstAction?.kind === "copy" && firstAction.payload) {
      return firstAction.payload;
    }
  }

  // 2. 非空 query
  const queryText = queryEl.value.trim();
  if (queryText) {
    return queryEl.value;
  }

  // 3. 空闲态 Awareness 选区（仅当 query 为空且无结果时）
  if (!results.hasItems()) {
    try {
      const selectionText = await getAwarenessText();
      if (selectionText && selectionText.trim()) {
        return selectionText;
      }
    } catch (e) {
      // 后端未就绪或无选区——静默降级到空白
    }
  }

  // 4. 空白
  return "";
}

// chord 独占模式（0.10.7）下的兜底触发路径。
//
// **正常路径**：chord mode 激活时 native LL hook 吞掉字母 keydown 并直接 emit
// `HotkeyEvent::Chord` → 后端 `trigger_chord`，前端 keydown 收不到事件。
//
// **此函数的用途**：当 `set_chord_mode` IPC 调用失败（如后端未就绪）时，
// hook 未吞键，前端 keydown 兜底走此路径触发 chord。两条路径互斥——不会双触发。
// 维护者注意：不要误以为是双触发 bug 而删除此函数。
function onChordTrigger(e) {
  if (!e.altKey) return;
  if (e.isComposing || e.keyCode === 229) return; // IME 组字放行
  const key = e.key.toLowerCase();
  // 0.10.7：用动态 tap 键集合（从 chord 配置派生），不再用硬编码 CHORD_KEYS
  if (!chord.getTapKeys().has(key)) return;
  // 0.16.2：门禁不再检查 queryEmpty--getTapKeys() 已按 query 是否为空动态过滤。
  // 非空 query 时 getTapKeys 只返回 requires_input=true 的键，此处的 key 已通过过滤。
  e.preventDefault(); // 不进输入框
  e.stopPropagation();
  fireChord(key);
}

/** Chord 触发/显示的统一门禁条件。
 *  0.16.2：只检查总开关。query 是否为空由 getTapKeys() 动态过滤（空 query 全部 tap 键，
 *  非空 query 只返回 requires_input 的键），结果列表是否为空不再阻断 chord 显示--
 *  有搜索结果时用户仍可能想用 Alt+Q 把文本带入对话。
 */
function chordEligible() {
  return chord.isEnabled();
}

// ── 按住 Alt 显示数字角标 ─────────────────────────────────────────────────────
// body.alt-active 由 CSS 控制角标显隐。需多重兜底清除，避免 Alt+Tab 切走后状态残留。

function setAlt(on) {
  // 0.8.5 §6.4：菜单可见性与触发资格同源——只有 chordEligible() 满足才允许展示。
  // 0.16.2：chordEligible 不再检查 queryEmpty--非空 query 时 chord 仍可显示
  // （chord 提示走 statusbar 副行，不遮 Ghost / results）。
  const eligible = chordEligible();
  const showChord = on && eligible;
  const prevChordVisible = document.body.classList.contains("chord-visible");
  document.body.classList.toggle("alt-active", on);
  document.body.classList.toggle("chord-visible", showChord);
  // 0.10.7：chord 独占模式联动——showChord 时进独占（LL hook 吞 chord keydown），
  // 退出时还原。异步调用，失败仅告警（不影响主流程）。
  // 0.14：showChord=true 时传入前端已派生的 tapKeys，跳过后端 3 次 DB 查询。
  // 0.16.2：hook 独占模式只在空 query 时进--非空 query 时 chord 触发走前端 keydown
  // 兜底路径（fireChord 带 inputText），hook 不吞键以避免后端拿不到输入框文本。
  const hookExclusive = showChord && !queryEl.value.trim();
  setChordMode(hookExclusive, hookExclusive ? [...chord.getTapKeys()] : null).catch((e) =>
    console.warn("[chord] set_chord_mode 失败", e),
  );
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
// 轮询、hidden 停止，每 50ms 查逻辑 hold 态，状态变化才 setAlt（避免无谓 resize）。
//
// 0.11.10：后端 `is_alt_down()` 改读 `ALT_LOGICALLY_HELD`（LL hook 过滤 injected 事件的
// 逻辑态），已免疫 SetForegroundWindow 合成 keyup 污染。故前端原来的宽限期兜底
// （altPollGraceUntil / 首次 tick 前 altLast=true）不再需要，poll 直接跟随后端读数。
// 0.14：轮询间隔从 100ms 降到 50ms——Alt 松开后最多 50ms 内检测到并退出 chord 模式。
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
    // Guard: stopAltPoll() 可能在 await 期间被调用（如截图触发时 hide_for_screenshot
    // emit blink://hidden → 前端 stopAltPoll 清了 altLast=false）。若不挡，这个
    // in-flight tick 会拿到 down=true（Alt 还按着）→ setAlt(true) → setChordMode(true)，
    // 把 CHORD_MODE 重新打开。此后 alt-poll 已停、没人再清，chord 状态泄露到
    // 截图退出后——桌面按 Alt+A 会误触发一次截图。
    if (!altPollTimer) return;
    if (down !== altLast) {
      altLast = down;
      setAlt(down);
    }
  };
  tick();
  altPollTimer = window.setInterval(tick, 50);
}

export function stopAltPoll() {
  if (altPollTimer) window.clearInterval(altPollTimer);
  altPollTimer = null;
  if (altLast) {
    altLast = false;
    setAlt(false);
  }
}

/**
 * chord 配置就绪后补检 Alt 态（lifecycle.js chord.refresh() 完成后调）。
 *
 * 修首次唤起竞态：shown 时 chord.refresh()（async 未 await）和 startAltPoll() 同时启动，
 * 若 config IPC 慢于首个 poll tick（首启冷启动常见），用户正按着 Alt 但 chordEnabled
 * 还是 false → chord-visible 不设置 → 用户松手后 config 才到 → 错过。
 * refresh 完成后调此函数，用已知 altLast 重判一次即可（不重新查物理态，避免异步开销）。
 */
export function recheckAlt() {
  if (altLast) {
    setAlt(true);
  }
}
