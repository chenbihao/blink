//! 键盘交互：结果导航、激活、ESC 隐藏、修饰键默认行为屏蔽。
//! Tab / ArrowRight 拦截接受 ghost 文本补全（视配置 autosuggest_tab_key）。
//! Alt 状态由后端事件驱动，不再轮询。chord 触发门禁用 `inputState.isAltDown() || e.altKey`。

import { hideWindow, triggerChord, getAwarenessText } from "../shared/api.js";
import { activateItem } from "./actions.js";
import * as results from "./results.js";
import * as ghost from "./ghost.js";
import * as chord from "./chord.js";
import * as autosuggestConfig from "./autosuggest-config.js";
import * as aiMode from "./ai-mode.js";
import * as cmdMode from "./command-mode.js";
import * as inputState from "./input-state.js";
import { queryEl, aiQueryEl, appEl } from "./dom.js";

/** 绑定全部键盘监听 + 滚轮翻页。 */
export function init() {
  // 0.8.5 Chord：Alt+字母触发（捕获阶段，最优先；独立于 onNavigation，不依赖 hasItems）
  document.addEventListener("keydown", onChordTrigger, true);
  document.addEventListener("keydown", onAutosuggestAccept, true); // 捕获阶段，优先其他 handler
  document.addEventListener("keydown", onNavigation);
  document.addEventListener("keydown", onEscape);
  document.addEventListener("keydown", onBlockModifiers, true);
  // 滚轮翻页：向上滚 = PageUp，向下滚 = PageDown（整页翻，用鼠标就不用手移到方向键了）
  // 监听 appEl 而非 resultsEl：window-size.js 的 maxHeight 机制会让窗口在末页
  // 保持满页高度，#results 下方可能有空白区域（属于 #app），监听 appEl 可覆盖
  // 全窗口，滚轮不溢出到背后窗口。
  appEl.addEventListener("wheel", (e) => {
    // 0.18.0: AI 模式放行默认滚动（让 #ai-display 可滚），搜索模式仍 preventDefault + 翻页
    if (aiMode.isActive()) return;
    // 0.18.6: 命令模式无结果可翻页，放行默认行为
    if (cmdMode.isActive()) return;
    e.preventDefault(); // 阻止默认滚动（列表本来就不滚动）
    if (!results.hasItems()) return;
    if (e.deltaY < 0) {
      results.pageUp(); // 向上滚 → 上一页（等价于 PageUp）
    } else {
      results.pageDown(); // 向下滚 → 下一页（等价于 PageDown）
    }
  });
}

// ── Autosuggestion Tab 接受 ────────────────────────────────────────

/**
 * Tab / ArrowRight（视配置）+ 有活跃 ghost → 接受补全。
 * 捕获阶段拦截，抢在 onNavigation 之前——ArrowRight 场景下必须先处理，
 * 否则会被 results 的方向键导航吞掉（虽然当前 ArrowRight 无导航语义，仍为将来预留）。
 * AiMode 下抑制 Tab Ghost 接受（AI 模式无 ghost）。
 */
function onAutosuggestAccept(e) {
  // AiMode 下抑制 Tab Ghost 接受
  if (aiMode.isActive()) return;
  // 0.18.6: 命令模式下抑制 Tab Ghost 接受
  if (cmdMode.isActive()) return;
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
  // 0.18.6: 命令模式 — 只处理 Enter（执行命令），其余导航全部抑制
  if (cmdMode.isActive()) {
    if (e.key === "Enter" && !e.isComposing) {
      e.preventDefault();
      cmdMode.execute();
    }
    return;
  }

  // 0.17.6: AiMode 下 Enter 发送追问（或确认卡片），不触发结果导航
  if (aiMode.isActive()) {
    if (e.key === "Enter" && !e.isComposing) {
      e.preventDefault();
      // 确认卡片显示时，Enter 确认操作（不触发追问）
      if (aiMode.isAwaitingConfirm()) {
        aiMode.confirmCurrentAction();
      } else {
        const text = aiQueryEl.value.trim();
        if (text) {
          aiQueryEl.value = "";
          aiMode.askFollowup(text);
        }
      }
    }
    return;
  }

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
  } else if ((inputState.isAltDown() || e.altKey) && /^[1-9]$/.test(e.key)) {
    // Alt+1~9：直接激活第 N 个候选
    // 用后端 Alt 快照 || e.altKey，抵抗 WebView synthetic keyup
    e.preventDefault();
    activateItem(results.getNth(parseInt(e.key, 10)));
  }
}

// ── ESC 隐藏 ──────────────────────────────────────────────────────────────────

function onEscape(e) {
  if (e.key === "Escape") {
    e.preventDefault();
    // AiMode 下 ESC 退出 AI 模式（不 hide 窗口）
    if (aiMode.isActive()) {
      aiMode.exitAiMode();
      return;
    }
    ghost.clear();
    hideWindow();
  }
}

// ── 屏蔽修饰键/功能键系统默认行为 ─────────────────────────────────────────────
// 防 Alt 激活宿主窗口系统菜单导致 WebView2 消息泵冻结（与 settings.js 录制同理）。
// 不阻止字母数字/方向键/Enter；Alt+数字选候选不受 preventDefault 影响。

function onBlockModifiers(e) {
  const altDown = inputState.isAltDown() || e.altKey;
  if (
    e.key === "Alt" ||
    e.key === "Meta" ||
    /^F\d{1,2}$/.test(e.key) ||
    (altDown && (e.key === " " || e.code === "Space"))
  ) {
    e.preventDefault();
  }
}

// ── Chord 触发：Alt+字母 → trigger_chord ──────────────────────────────
// onNavigation 的 altKey 分支只覆盖 Alt+1~9（选候选，且依赖 hasItems）。
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
// 触发键集合从 chord.getTapKeys() 动态获取（用户可配置）。
// E/S 键做 contextual 解析——active item 文本 payload > 非空 query > Awareness 选区 > 空白。
// 用 `inputState.isAltDown() || e.altKey` 代替纯 `e.altKey`，
// 后端快照抵抗 WebView synthetic keyup，事件自带 altKey 覆盖状态事件尚未到达的即时边沿。
async function fireChord(key) {
  console.log(`[chord] Alt+${key.toUpperCase()} triggered`);
  let inputText = queryEl.value;
  let originRef = null;

  // E/S 需要 contextual 解析
  if (key === "e" || key === "s") {
    const ctx = await resolveContextualContent();
    inputText = ctx.text;
    originRef = ctx.hitId;
  }

  triggerChord(key, inputText, originRef).catch((e) => console.warn("[chord] trigger_chord 失败", e));
}

/**
 * 为 chord E/S 解析上下文内容。
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
      return { text: firstAction.payload, hitId: firstAction.hitId || null };
    }
  }

  // 2. 非空 query
  const queryText = queryEl.value.trim();
  if (queryText) {
    return { text: queryEl.value, hitId: null };
  }

  // 3. 空闲态 Awareness 选区（仅当 query 为空且无结果时）
  if (!results.hasItems()) {
    try {
      const selectionText = await getAwarenessText();
      if (selectionText && selectionText.trim()) {
        return { text: selectionText, hitId: null };
      }
    } catch (e) {
      // 后端未就绪或无选区——静默降级到空白
    }
  }

  // 4. 空白
  return { text: "", hitId: null };
}

// chord 独占模式下的兜底触发路径。
//
// **正常路径**：chord exclusive 激活时 native LL hook 吞掉字母 keydown 并直接 emit
// `ChordTriggered` → 后端 `trigger_chord`，前端 keydown 收不到事件。
//
// **此函数的用途**：当后端 exclusive 尚未建立（如首次唤起竞态）时，
// hook 未吞键，前端 keydown 兜底走此路径触发 chord。两条路径互斥——不会双触发。
// 维护者注意：不要误以为是双触发 bug 而删除此函数。
function onChordTrigger(e) {
  // 用后端 Alt 快照 || e.altKey，抵抗 WebView synthetic keyup
  if (!(inputState.isAltDown() || e.altKey)) return;
  if (e.isComposing || e.keyCode === 229) return; // IME 组字放行
  const key = e.key.toLowerCase();
  // 用动态 tap 键集合（从 chord 配置派生）
  if (!chord.getTapKeys().has(key)) return;
  // 门禁不再检查 queryEmpty--getTapKeys() 已按 query 是否为空动态过滤。
  // 非空 query 时 getTapKeys 只返回 requires_input=true 的键，此处的 key 已通过过滤。
  e.preventDefault(); // 不进输入框
  e.stopPropagation();
  // AiMode 下 Alt+Q 触发临时对话提升，不走常规 chord 路径
  if (aiMode.isActive() && key === "q") {
    aiMode.promoteToChat();
    return;
  }
  fireChord(key);
}
