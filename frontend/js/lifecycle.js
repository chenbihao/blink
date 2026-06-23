//! 窗口生命周期：响应后端 blink://shown / blink://hidden，复位输入与列表。

import { listen } from "./tauri.js";
import { queryEl } from "./dom.js";
import * as results from "./results.js";
import * as search from "./search.js";
import { clearAlt } from "./keyboard.js";

/** 注册生命周期事件监听。 */
export function init() {
  listen("blink://shown", () => {
    queryEl.value = "";
    search.reset(); // 作废在途搜索请求
    results.clear();
    clearAlt(); // 清 Alt 角标残留（上次按住 Alt 激活后可能未收到 keyup）
    queryEl.focus();
  });

  listen("blink://hidden", () => {
    queryEl.value = "";
    search.reset();
    results.clear();
    clearAlt();
  });
}
