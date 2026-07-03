//! 搜索输入：防抖 + IME 感知 + 调后端 + 交给 results 渲染。
//! 0.8.1：解 SearchResponse { entries, completionHint } 并联动 ghost overlay。

import { queryEl } from "./dom.js";
import { searchApps } from "./api.js";
import { listen } from "./tauri.js";
import * as results from "./results.js";
import * as ghost from "./ghost.js";

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
  queryEl.addEventListener("compositionend", () => {
    isComposing = false;
    onInput(); // 组字结束后立即触发一次完整输入搜索
  });
  // async lane 慢引擎完成后推送增量；校验 seq（防过期）+ 非空 query（防 reset 后回填）。
  listen("blink://results", (event) => {
    const payload = event.payload;
    if (!payload || payload.seq !== seq) return;
    if (!queryEl.value.trim()) return;
    results.merge(payload.items, payload.seq);
    // 增量事件不带 hint（同步首次返回已给过），此处不动 ghost。
  });
}

/** 复位：取消防抖 + 作废在途请求（生命周期 shown/hidden 调用）。 */
export function reset() {
  clearTimeout(timer);
  seq++;
  isComposing = false;
  ghost.clear();
}

/** 用当前输入重新触发一次搜索（右键菜单重置记录后刷新结果用）。 */
export function retrigger() {
  onInput();
}

/**
 * 拉取空 query 的 Context 建议动作（0.8.0 §1.3）。
 *
 * 空 query 场景：`onInput` 一律短路不发请求，而 shown 事件后我们**需要**主动查后端
 * 拿一批依 Context（剪贴板/选区）命中的内置动作。此函数专给 lifecycle 调，绕过 onInput
 * 的空 query 短路，仍走完整的 seq/竞态防护路径。
 */
export async function fetchContextSuggestions() {
  const mySeq = ++seq;
  try {
    const resp = await searchApps("", mySeq);
    // 竞态防护：用户已开始输入或已再次隐藏 → 丢弃
    if (mySeq !== seq) return;
    // 只有仍是空 query 时才渲染，避免和用户输入的结果打架
    if (queryEl.value.trim()) return;
    results.render(resp.entries || [], mySeq);
    // 空 query 恒无 hint（后端保证），但保险起见清一次
    ghost.clear();
  } catch (e) {
    console.error("fetchContextSuggestions failed:", e);
  }
}

function onInput() {
  clearTimeout(timer);
  const q = queryEl.value.trim();
  if (!q) {
    seq++; // 作废在途请求，避免旧响应回填空输入态
    results.clear();
    ghost.clear();
    return;
  }
  // IME 组字中：不触发搜索，等 compositionend 再发
  if (isComposing) return;
  const mySeq = ++seq;
  timer = setTimeout(async () => {
    try {
      // 用 raw value（保留末尾空格）给后端算 hint —— "fanyi " 与 "fanyi" 语义不同
      const rawQuery = queryEl.value;
      const resp = await searchApps(rawQuery, mySeq);
      // 丢弃过期响应：用户已输入新 query 或已复位
      if (mySeq !== seq) return;
      results.render(resp.entries || [], mySeq);
      ghost.update(rawQuery, resp.completionHint);
    } catch (e) {
      console.error("search_apps failed:", e);
    }
  }, DEBOUNCE_MS);
}
