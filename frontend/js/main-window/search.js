//! 搜索输入：防抖 + IME 感知 + 调后端 + 交给 results 渲染。
//! 0.8.3：解 SearchResponse { entries, suggestion } 并联动 ghost overlay。
//! 空 query 也走 search 接口（0.8.2 的 fetchContextSuggestions 已废弃）——
//! Context Suggestion 走 Ghost + Tab 采纳,不再抢首屏。

import { queryEl } from "./dom.js";
import { searchApps } from "../shared/api.js";
import { listen } from "../shared/tauri.js";
import { EVENTS } from "../shared/event-names.js";
import * as results from "./results.js";
import * as ghost from "./ghost.js";
import * as cmdMode from "./command-mode.js";

/** 防抖间隔。后端搜索实测 ~1ms（纯内存），仅用于合并极快连打。 */
const DEBOUNCE_MS = 40;

let timer = null;
/** 请求序号：每发起一次搜索 +1，响应回来时只接受最新序号，丢弃过期结果（防竞态）。 */
let seq = 0;
/** IME 组字中标记（compositionstart 置 true，compositionend 置 false）。
 * 中文输入法按拼音时会产生大量中间态（如 "ne'o'" / "呢OK"），这些都不应触发搜索。
 */
let isComposing = false;

/** 绑定输入监听 + async 增量结果监听（blink://results）。 */
export function init() {
  queryEl.addEventListener("input", onInput);
  // IME 组字感知：避免中文拼音中间态触发无效搜索
  queryEl.addEventListener("compositionstart", () => { isComposing = true; });
  queryEl.addEventListener("compositionupdate", onCompositionUpdate);
  queryEl.addEventListener("compositionend", () => {
    isComposing = false;
    // 组字结束：恢复 ghost-typed 为最终 query（compositionupdate 可能残留中间态）
    ghost.syncTypedText(queryEl.value);
    onInput(); // 组字结束后立即触发一次完整输入搜索
  });
  // async lane 慢引擎完成后推送增量；校验 seq（防过期）。
  // 0.8.2：不再拦"空 query"——Context 触发（选区/剪贴板感知）会在空 query 下产生
  // 合法的插件增量结果（如翻译）。原护栏靠 seq/reset 已足够防隐藏后回填。
  listen(EVENTS.RESULTS, (event) => {
    const payload = event.payload;
    if (!payload || payload.seq !== seq) return;
    results.merge(payload.items, payload.seq);
    // 增量事件不带 suggestion（同步首次返回已给过），此处不动 ghost。
  });

  // 0.17.6: 主窗口 AI 改走 ChatService（CHAT_STREAM / CHAT_CONFIRM_ACTION），
  // 由 ai-mode.js 负责监听和处理，search.js 不再参与 AI 事件。
}

/** 复位：取消防抖 + 作废在途请求（生命周期 shown/hidden 调用）。 */
export function reset() {
  clearTimeout(timer);
  seq++;
  isComposing = false;
  ghost.clear();
}

/** 取当前 seq(0.17.6: trigger_ai 已删除，此函数保留供未来扩展复用)。 */
export function getSeq() {
  return seq;
}

/**
 * 用当前输入重新触发一次搜索。
 *
 * - 非空 query：走 onInput（debounce + render，不 clear）
 * - 空 query：直接 fetchContextSuggestions（不 clear 旧结果，避免闪烁）
 *
 * **与 onInput 空 query 路径的关键区别**：
 * onInput 在用户退格清空时会 `results.clear()` + `ghost.clear()` 再
 * `fetchContextSuggestions()`——因为用户主动清空意味着旧结果已不相关。
 * 而 retrigger 用于 awareness-updated（选区就绪 / 剪贴板变化）和右键菜单
 * 重置后刷新——此时旧结果仍相关，应等新响应到达后由 results.render
 * 自动替换（ensureSeq seq 变化 → allItems 重置 → renderPage 重建），
 * 而非先清空再重绘，避免「旧结果消失 → 新结果到达」之间的视觉闪烁。
 *
 * 用于：
 * - lifecycle `blink://awareness-updated` → retrigger
 * - contextmenu `resetItemHistory` 后 retrigger
 */
export function retrigger() {
  if (queryEl.value.trim()) {
    onInput();
  } else {
    // 不走 onInput 的 clear 路径——fetchContextSuggestions 内部 ++seq
    // 会作废在途请求，新响应到达后 results.render 自动替换旧结果。
    fetchContextSuggestions();
  }
}

/**
 * shown 事件后拉一次空 query 结果（0.8.3 §4.13 P0-1）。
 *
 * 0.8.2 → 0.8.3 变化：原 `fetchContextSuggestions` 拉的是 Context 召回的 AppEntry
 * （翻译 Priority 置顶等），0.8.3 §4.13 P0-3 已把 Context 从 route() 里挪走——
 * 空 query 不再产 candidate。这个函数保留是为了：唤起瞬间**发一次空 query 到后端**
 * 拿 `response.suggestion`（Context Suggestion 走这条路径出 Ghost）。
 *
 * 相较 `onInput` 的空 query 分支：绕过 40ms 防抖，让 shown 到 Ghost 出现无感延迟。
 */
export async function fetchContextSuggestions() {
  const mySeq = ++seq;
  try {
    const resp = await searchApps("", mySeq);
    // 竞态防护：用户已开始输入或已再次隐藏 → 丢弃
    if (mySeq !== seq) return;
    // 只有仍是空 query 时才应用结果，避免和用户输入的结果打架
    if (queryEl.value.trim()) return;
    // entries 通常为空（Context 已不产 candidate），但仍走 render 走清理路径
    results.render(resp.entries || [], mySeq);
    // Ghost：空 query 场景后端产 Context Suggestion,此处消费
    ghost.update("", resp.suggestion);
  } catch (e) {
    console.error("fetchContextSuggestions failed:", e);
  }
}

/**
 * IME compositionupdate 事件处理：拼音/笔画每变化一次就触发。
 * 把当前 IME 文字同步到 ghost-typed（透明占位），让 ghost-suggest 跟随后移。
 * 示例：用户输入 "ni" → "ni'h" → "ni'hao"，ghost-typed 实时更新，suggest 不重叠。
 */
function onCompositionUpdate() {
  if (!isComposing) return;
  // composition 期间不触发搜索（等 compositionend），只同步 ghost 位置
  ghost.syncTypedText(queryEl.value);
}

function onInput() {
  clearTimeout(timer);

  // 0.18.6: `> ` 前缀 → 命令模式，跳过搜索逻辑（清结果、停 ghost、显示 hint）
  if (cmdMode.handleInput(queryEl.value)) {
    return;
  }

  const q = queryEl.value.trim();
  if (!q) {
    // 空 query（用户退格清空）：作废在途请求 + 清结果 + 重新拉 Context 建议。
    // 此处 clear 是正确的——用户主动清空意味着旧结果已不相关。
    // 注意：awareness-updated 走 retrigger() 而非 onInput()，不会经过此
    // clear 路径，避免「旧结果消失 → 新结果到达」的闪烁（见 retrigger 注释）。
    seq++;
    results.clear();
    ghost.clear();
    fetchContextSuggestions();
    return;
  }
  // IME 组字中：不触发搜索，等 compositionend 再发
  if (isComposing) return;
  const mySeq = ++seq;
  timer = setTimeout(async () => {
    try {
      // 用 raw value（保留末尾空格）给后端算 suggestion —— "fanyi " 与 "fanyi" 语义不同
      const rawQuery = queryEl.value;
      const resp = await searchApps(rawQuery, mySeq);
      // 丢弃过期响应：用户已输入新 query 或已复位
      if (mySeq !== seq) return;
      results.render(resp.entries || [], mySeq);
      ghost.update(rawQuery, resp.suggestion);
    } catch (e) {
      console.error("search_apps failed:", e);
    }
  }, DEBOUNCE_MS);
}
