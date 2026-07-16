//! 窗口生命周期：响应后端 blink://shown / blink://hidden，复位输入与列表。

import { listen } from "./tauri.js";
import { queryEl } from "./dom.js";
import * as results from "./results.js";
import * as search from "./search.js";
import * as chord from "./chord.js";
import { clearAlt, startAltPoll, stopAltPoll, recheckAlt } from "./keyboard.js";
import { applyThemeFromConfig, applyGlassOpacityFromConfig } from "./theme.js";
import { applyI18nFromConfig } from "./i18n/index.js";

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
    applyGlassOpacityFromConfig(); // 刷新毛玻璃透明度
    // 刷新界面语言（设置页可能改了 language）；不 await，不阻塞 focus
    applyI18nFromConfig();
    // 刷新最大结果数（设置页可能改了 max_results）
    results.refreshMaxResults();
    // 0.8.3 §4.13 P0-1：唤起瞬间发一次空 query,拉后端产的 Context Suggestion（Ghost）。
    // 0.8.2 此调用是拉 Context 召回条目（AppEntry）;0.8.3 契约变更后 Context 不产
    // candidate,该调用现在的作用是拿 `response.suggestion` 走 Ghost 通道——函数名保留,
    // 内部实现在 search.js 已重写。
    search.fetchContextSuggestions();
    chord.refresh().then(recheckAlt); // 0.8.5：拉 Chord 动作列表渲染增强菜单；就绪后补检 Alt 态（修首次唤起竞态）
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
    applyGlassOpacityFromConfig(); // 毛玻璃透明度即时生效
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

  // 0.10 语音输入:G1 流式 partial 文字实时更新 #query
  // (G2 的 partial 由 mini overlay 窗口处理,主窗口不可见时不接收)
  // 0.10.4: 支持 confirmed/preview 双字段（伪流式引擎）
  listen("blink://voice-partial", (event) => {
    const payload = event.payload ?? {};
    if (payload.target !== "g1") return;

    // 优先使用 confirmed + preview 双字段（伪流式）
    if (payload.confirmed !== undefined || payload.preview !== undefined) {
      const confirmed = payload.confirmed || "";
      const preview = payload.preview || "";
      const combined = confirmed + preview;
      if (combined) {
        queryEl.value = combined;
        queryEl.dispatchEvent(new Event("input", { bubbles: true }));
      }
    } else if (payload.text) {
      // 兼容旧格式（真流式 / 非流式引擎）
      queryEl.value = payload.text;
      queryEl.dispatchEvent(new Event("input", { bubbles: true }));
    }
  });

  // 0.10 G1 录音音量波动条
  const voiceIndicator = document.getElementById("voice-indicator");
  const vwBars = voiceIndicator?.querySelectorAll(".vw-bar") ?? [];
  listen("blink://voice-level", (event) => {
    const { level, target } = event.payload ?? {};
    if (target !== "g1") return;
    if (voiceIndicator?.classList.contains("hidden")) {
      voiceIndicator?.classList.remove("hidden");
    }
    const lv = Math.max(0, Math.min(1, level || 0));
    vwBars.forEach((bar, i) => {
      const factor = [0.6, 0.85, 1.0, 0.85, 0.6][i] || 0.7;
      // jitter 独立于 lv：即使安静时也有微妙呼吸感
      const jitter = (Math.sin(Date.now() / 80 + i * 1.3) + 1) * 0.08;
      const h = Math.max(4, (lv * factor + jitter) * 20);
      bar.style.height = h + "px";
    });
  });

  // 0.10 录音结束 → 隐藏 G1 指示器
  listen("blink://voice-recording-end", () => {
    if (voiceIndicator) {
      voiceIndicator.classList.add("hidden");
      vwBars.forEach((bar) => (bar.style.height = "4px"));
    }
  });

  // 0.10 语音错误提示（服务未启动等）
  listen("blink://voice-error", (event) => {
    const { message, target } = event.payload ?? {};
    if (target !== "g1" || !message) return;
    // 在搜索框中显示错误提示，用户输入时自动清除
    queryEl.value = "";
    queryEl.placeholder = message;
    queryEl.dispatchEvent(new Event("input", { bubbles: true }));
    // 3s 后恢复原 placeholder
    setTimeout(() => {
      queryEl.placeholder = "";
    }, 3000);
  });

  // 0.9.2.1：剪贴板变化 → AwarenessSnapshot 已局部刷新 → 用当前 query 重跑
  // 一次让 Context Ghost / AI 四筛子读到新剪贴板。retrigger 内部会区分空/非空
  // query 分别走 fetchContextSuggestions / onInput，Ghost 通道天然覆盖。
  // 后端只在主窗口可见时才 emit，前端无需再判可见。
  listen("blink://awareness-updated", () => {
    search.retrigger();
  });
}
