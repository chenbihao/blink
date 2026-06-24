//! 搜索输入：防抖 + 调后端 + 交给 results 渲染。

import { queryEl } from "./dom.js";
import { searchApps } from "./api.js";
import { listen } from "./tauri.js";
import * as results from "./results.js";

/** 防抖间隔。后端搜索实测 ~1ms（纯内存），仅用于合并极快连打/IME 组合输入。 */
const DEBOUNCE_MS = 40;

let timer = null;
/** 请求序号：每发起一次搜索 +1，响应回来时只接受最新序号，丢弃过期结果（防竞态）。 */
let seq = 0;

/** 绑定输入监听 + async 增量结果监听（blink://results）。 */
export function init() {
  queryEl.addEventListener("input", onInput);
  // async lane 慢引擎完成后推送增量；校验 seq（防过期）+ 非空 query（防 reset 后回填）。
  listen("blink://results", (event) => {
    const payload = event.payload;
    if (!payload || payload.seq !== seq) return;
    if (!queryEl.value.trim()) return;
    results.merge(payload.items);
  });
}

/** 复位：取消防抖 + 作废在途请求（生命周期 shown/hidden 调用）。 */
export function reset() {
  clearTimeout(timer);
  seq++;
}

function onInput() {
  clearTimeout(timer);
  const q = queryEl.value.trim();
  if (!q) {
    seq++; // 作废在途请求，避免旧响应回填空输入态
    results.clear();
    return;
  }
  const mySeq = ++seq;
  timer = setTimeout(async () => {
    try {
      const apps = await searchApps(q, mySeq);
      // 丢弃过期响应：用户已输入新 query 或已复位
      if (mySeq !== seq) return;
      results.render(apps);
    } catch (e) {
      console.error("search_apps failed:", e);
    }
  }, DEBOUNCE_MS);
}
