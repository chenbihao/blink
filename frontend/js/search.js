//! 搜索输入：防抖 + IME 感知 + 调后端 + 交给 results 渲染。

import { queryEl } from "./dom.js";
import { searchApps } from "./api.js";
import { listen } from "./tauri.js";
import * as results from "./results.js";

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
  });
}

/** 复位：取消防抖 + 作废在途请求（生命周期 shown/hidden 调用）。 */
export function reset() {
  clearTimeout(timer);
  seq++;
  isComposing = false;
}

/** 用当前输入重新触发一次搜索（右键菜单重置记录后刷新结果用）。 */
export function retrigger() {
  onInput();
}

function onInput() {
  clearTimeout(timer);
  const q = queryEl.value.trim();
  if (!q) {
    seq++; // 作废在途请求，避免旧响应回填空输入态
    results.clear();
    return;
  }
  // IME 组字中：不触发搜索，等 compositionend 再发
  if (isComposing) return;
  const mySeq = ++seq;
  timer = setTimeout(async () => {
    try {
      const apps = await searchApps(q, mySeq);
      // 丢弃过期响应：用户已输入新 query 或已复位
      if (mySeq !== seq) return;
      results.render(apps, mySeq);
    } catch (e) {
      console.error("search_apps failed:", e);
    }
  }, DEBOUNCE_MS);
}
