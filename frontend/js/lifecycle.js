//! 窗口生命周期：响应后端 blink://shown / blink://hidden，复位输入与列表。

import { listen } from "./tauri.js";
import { queryEl } from "./dom.js";
import * as results from "./results.js";
import * as search from "./search.js";
import { clearAlt } from "./keyboard.js";
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
  });

  listen("blink://hidden", () => {
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
  });
}
