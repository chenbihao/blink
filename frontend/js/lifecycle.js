//! 窗口生命周期：响应后端 blink://shown / blink://hidden，复位输入与列表。

import { listen } from "./tauri.js";
import { queryEl } from "./dom.js";
import * as results from "./results.js";
import * as search from "./search.js";
import * as chord from "./chord.js";
import { clearAlt, startAltPoll, stopAltPoll } from "./keyboard.js";
import { applyThemeFromConfig } from "./theme.js";
import { applyI18nFromConfig } from "./i18n.js";

/** 注册生命周期事件监听。 */
export function init() {
  listen("blink://shown", () => {
    queryEl.value = "";
    search.reset(); // 作废在途搜索请求
    results.clear();
    clearAlt(); // 清 Alt 角标残留（上次按住 Alt 激活后可能未收到 keyup）
    queryEl.focus();
    // 异步刷新主题（设置页可能改了 theme）；不 await，不阻塞 focus
    applyThemeFromConfig();
    // 刷新界面语言（设置页可能改了 language）；不 await，不阻塞 focus
    applyI18nFromConfig();
    // 刷新最大结果数（设置页可能改了 max_results）
    results.refreshMaxResults();
    // 0.8.3 §4.13 P0-1：唤起瞬间发一次空 query,拉后端产的 Context Suggestion（Ghost）。
    // 0.8.2 此调用是拉 Context 召回条目（AppEntry）;0.8.3 契约变更后 Context 不产
    // candidate,该调用现在的作用是拿 `response.suggestion` 走 Ghost 通道——函数名保留,
    // 内部实现在 search.js 已重写。
    search.fetchContextSuggestions();
    chord.refresh(); // 0.8.5：拉 Chord 动作列表渲染增强菜单
    startAltPoll(); // 0.8.5：轮询 Alt 物理态驱动 alt-active（WebView2 不转发 Alt keydown）
  });

  listen("blink://hidden", () => {
    stopAltPoll(); // 0.8.5：停 Alt 轮询
    queryEl.value = "";
    search.reset();
    results.clear();
    clearAlt();
  });

  // 配置变更即时响应（设置页切换主题/语言等，无需关闭再打开主窗口）
  listen("blink://config-changed", () => {
    applyThemeFromConfig();
    applyI18nFromConfig();
    results.refreshMaxResults();
    chord.refresh(); // 0.8.5.1 §6.6：Chord 开关/可见性改动即时生效
  });

  // 0.8.5：Chord 划词确认 → 填搜索框「翻译 {text}」触发翻译插件
  listen("blink://chord-translate", (event) => {
    queryEl.value = `翻译 ${event.payload}`;
    queryEl.dispatchEvent(new Event("input", { bubbles: true }));
  });

  // 0.8.5 §6.4：Chord Alt+C 剪贴板改走 fill-query——后端 ClipboardHistoryAction
  // execute 里 window::invoke + emit "剪贴板 " → 前端填搜索框 + 触发 ClipboardEngine 召回。
  listen("blink://chord-fill-query", (event) => {
    queryEl.value = String(event.payload ?? "");
    queryEl.dispatchEvent(new Event("input", { bubbles: true }));
  });
}
