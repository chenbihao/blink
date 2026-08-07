//! 窗口生命周期：响应后端 blink://shown / blink://hidden，复位输入与列表。

import { listen } from "../shared/tauri.js";
import { EVENTS } from "../shared/event-names.js";
import { queryEl, aiQueryEl } from "./dom.js";
import * as results from "./results.js";
import * as search from "./search.js";
import * as chord from "./chord.js";
import * as ghost from "./ghost.js";
import * as aiMode from "./ai-mode.js";
import * as cmdMode from "./command-mode.js";
import * as inputState from "./input-state.js";
import { applyThemeFromConfig, applyGlassOpacityFromConfig } from "../shared/theme.js";
import { applyI18nFromConfig, t } from "../i18n/index.js";

/** 注册生命周期事件监听。 */
export function init() {
  listen(EVENTS.SHOWN, () => {
    // 0.17.6: AiMode 下 SHOWN 只 focus AI 输入框，不重置搜索状态
    if (aiMode.isActive()) {
      aiQueryEl.focus();
      inputState.onShown();
      return;
    }
    queryEl.value = "";
    // 先解冻 ghost（上次录音可能残留 frozen），再 reset 让 ghost.clear 正常清 DOM
    ghost.unfreeze();
    document.body.classList.remove("voice-active");
    search.reset(); // 作废在途搜索请求
    results.clear();
    cmdMode.reset(); // 0.18.6: 复位命令模式
    const vi = document.getElementById("voice-indicator");
    if (vi) vi.classList.add("hidden");
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
    // chord 配置刷新后重新投影 chord-visible
    chord.refresh().then(() => inputState.reevaluate());
    // query 被清空后上报 context（programmatic value= 不触发 input 事件）
    inputState.onShown();
  });

  listen(EVENTS.HIDDEN, () => {
    // Alt/Chord 状态由后端 INPUT_STATE_CHANGED 事件驱动，不在 HIDDEN 补偿清理。
    // 0.17.6: AiMode 下 HIDDEN 也清理 AI 状态
    if (aiMode.isActive()) {
      aiMode.exitAiMode();
    }
    queryEl.value = "";
    search.reset();
    results.clear();
    cmdMode.reset(); // 0.18.6: 复位命令模式
  });

  // 配置变更即时响应（设置页切换主题/语言等，无需关闭再打开主窗口）
  listen(EVENTS.CONFIG_CHANGED, () => {
    applyThemeFromConfig();
    applyGlassOpacityFromConfig(); // 毛玻璃透明度即时生效
    applyI18nFromConfig();
    results.refreshMaxResults();
    chord.refresh().then(() => inputState.reevaluate()); // Chord 开关/可见性改动即时生效；刷新后重投影
  });

  // 0.8.5 §6.4：Chord Alt+C 剪贴板改走 fill-query——后端 ClipboardHistoryAction
  // execute 里 window::invoke + emit "剪贴板 " → 前端填搜索框 + 触发 ClipboardEngine 召回。
  listen(EVENTS.CHORD_FILL_QUERY, (event) => {
    queryEl.value = String(event.payload ?? "");
    queryEl.dispatchEvent(new Event("input", { bubbles: true }));
  });

  // 0.10 语音录音开始 → G1 隐藏 Ghost overlay + 显示语音指示器
  // 注意：不清空 ghost-chord 文本内容——CSS body.voice-active 已隐藏它，
  // 录音结束后移除 voice-active 即可恢复显示。清空 textContent 会导致
  // chord.refresh() 未重新渲染前 :not(:empty) 不匹配，chord 提示永久消失。
  listen(EVENTS.VOICE_RECORDING_START, (event) => {
    const { target } = event.payload ?? {};
    if (target !== "g1") return;
    document.body.classList.add("voice-active");
    ghost.freeze(); // voice-partial 独占 overlay，search 不覆写
    // 显示语音指示器
    if (voiceIndicator) {
      voiceIndicator.classList.remove("hidden");
      // 录音开始：波形切回绿色（移除加载态蓝色 + 错误态红色）
      voiceIndicator.querySelector(".voice-wave")?.classList.remove("voice-loading", "voice-error");
    }
  });

  // 0.10 语音状态提示（模型加载中等，非错误性质）
  // 注意：不设 voice-active——只有真正录音（voice-recording-start）才设，
  // 避免模型加载中隐藏 Chord 提示。
  listen(EVENTS.VOICE_STATUS, (event) => {
    const { message, target } = event.payload ?? {};
    if (target !== "g1" || !message) return;
    if (voiceIndicator) {
      voiceIndicator.classList.remove("hidden");
      const label = voiceIndicator.querySelector(".voice-label");
      if (label) label.textContent = message;
      // 模型加载中：波形转蓝色（清除可能残留的错误态红色）
      voiceIndicator.querySelector(".voice-wave")?.classList.remove("voice-error");
      voiceIndicator.querySelector(".voice-wave")?.classList.add("voice-loading");
    }
  });

  // 0.10 语音输入:G1 流式 partial 文字实时更新 #query
  // (G2 的 partial 由 mini overlay 窗口处理,主窗口不可见时不接收)
  // 0.10.4: 支持 confirmed/preview 双字段（伪流式引擎）
  // 0.10.5: G1 应用 G2 的预上屏双色渲染——confirmed 填 #query，preview 走 Ghost overlay
  // 0.10.6: 同步更新 .ghost-typed 为 confirmed 文本——freeze 后 renderToDom 不执行，
  //         .ghost-typed 保持空白导致 preview 影子出现在输入框最左边而非 confirmed 之后
  // 0.10.7: 录音期间不再 dispatch input 事件——伪流式引擎第一句时 confirmed 为空，
  //         dispatch input 会触发 onInput → fetchContextSuggestions → search_apps("")
  //         产生大量无意义空 query 搜索（每个音频 chunk 一次，~70ms 内 6 次）。
  //         搜索结果在 freeze 期间 ghost.update 不写 DOM，纯无用功。
  //         录音结束后由 chord-fill-query（正常结束）填入 final_text 并 dispatch input
  //         触发一次完整搜索，取消时 ESC 隐藏窗口自动清空，无需录音中触发。
  listen(EVENTS.VOICE_PARTIAL, (event) => {
    const payload = event.payload ?? {};
    if (payload.target !== "g1") return;

    // 优先使用 confirmed + preview 双字段（伪流式）
    if (payload.confirmed !== undefined || payload.preview !== undefined) {
      const confirmed = payload.confirmed || "";
      const preview = payload.preview || "";
      // confirmed 填入输入框（已定稿文本）
      queryEl.value = confirmed;
      // 光标移到末尾 → 浏览器自动滚动 input 到文本末尾（超长时关键）
      queryEl.setSelectionRange(confirmed.length, confirmed.length);
      // preview 走 Ghost overlay（灰色半透明，与 G2 预上屏视觉效果一致）
      // ghost 已 freeze，search 的 ghost.update 不会覆写此处
      const ghostTyped = document.querySelector("#ghost-overlay .ghost-typed");
      const ghostSuggest = document.querySelector("#ghost-overlay .ghost-suggest");
      if (ghostTyped) {
        // 透明占位：与 #query 内容等宽，让 .ghost-suggest 的 preview 跟随到 confirmed 之后
        ghostTyped.textContent = confirmed;
      }
      if (ghostSuggest) {
        ghostSuggest.textContent = preview ? ` ${preview}` : "";
        ghostSuggest.classList.add("voice-preview-text");
        ghostSuggest.classList.remove("ghost-context");
      }
      // 0.10.6: 超长文本时确保滚动到文本末尾——setSelectionRange 的原生 scroll
      // 可能在下一帧才生效，用 rAF 确保读取到正确的 scrollWidth 后主动滚到最右端。
      requestAnimationFrame(() => ghost.scrollWithMargin());
      // 不 dispatch input —— 录音期间不触发搜索（见上方 0.10.7 注释）
    } else if (payload.text) {
      // 兼容旧格式（真流式 / 非流式引擎）
      queryEl.value = payload.text;
      // 不 dispatch input —— 同上，录音结束后由 chord-fill-query 触发搜索
    }
  });

  // 0.10 G1 录音音量波动条
  const voiceIndicator = document.getElementById("voice-indicator");
  const vwBars = voiceIndicator?.querySelectorAll(".vw-bar") ?? [];
  listen(EVENTS.VOICE_LEVEL, (event) => {
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

  // 0.10 录音结束 → 隐藏 G1 指示器 + 解冻 Ghost overlay（恢复 search 建议）
  listen(EVENTS.VOICE_RECORDING_END, () => {
    document.body.classList.remove("voice-active");
    ghost.unfreeze(); // 恢复 ghost.update DOM 写入 + 清除 voice-preview-text + 重绘当前 suggestion
    if (voiceIndicator) {
      voiceIndicator.classList.add("hidden");
      vwBars.forEach((bar) => (bar.style.height = "4px"));
      // 恢复语音指示器标签默认文案 + 清除加载态/错误态
      voiceIndicator.querySelector(".voice-wave")?.classList.remove("voice-loading", "voice-error");
      const label = voiceIndicator.querySelector(".voice-label");
      if (label) label.textContent = t("voice.indicator.recording");
    }
  });

  // 0.10 语音错误提示（STT 未配置 / 服务未启动等）
  // 设计铁则：所有语音状态统一在波形动画区域展示——
  // 绿色=录音中 · 蓝色=加载中 · 红色=错误。错误信息显示在 .voice-label 文本上。
  listen(EVENTS.VOICE_ERROR, (event) => {
    const { message, target } = event.payload ?? {};
    if (target !== "g1" || !message) return;
    document.body.classList.remove("voice-active");
    // 添加 voice-error 标记——隐藏 chord 提示，避免错误文案与 chord 键帽重叠
    document.body.classList.add("voice-error");
    ghost.unfreeze(); // 确保解冻（错误可能发生在录音中）
    // 在语音指示器上显示错误信息 + 红色波形
    if (voiceIndicator) {
      voiceIndicator.classList.remove("hidden");
      const label = voiceIndicator.querySelector(".voice-label");
      if (label) label.textContent = message;
      const wave = voiceIndicator.querySelector(".voice-wave");
      if (wave) {
        wave.classList.remove("voice-loading"); // 清除可能残留的加载态
        wave.classList.add("voice-error");
      }
    }
    // 3s 后隐藏指示器 + 恢复默认文案 + 移除 voice-error 标记
    setTimeout(() => {
      document.body.classList.remove("voice-error");
      if (voiceIndicator) {
        voiceIndicator.classList.add("hidden");
        voiceIndicator.querySelector(".voice-wave")?.classList.remove("voice-error");
        const label = voiceIndicator.querySelector(".voice-label");
        if (label) label.textContent = t("voice.indicator.recording");
      }
    }, 3000);
  });

  // 0.9.2.1：剪贴板变化 / 选区就绪 → AwarenessSnapshot 已局部刷新 → 用当前
  // query 重跑一次让 Context Ghost / AI 四筛子读到新值。retrigger 内部区分空/非空
  // query 分别走 fetchContextSuggestions / onInput。
  // **0.x 闪烁修复**：retrigger 在空 query 时直接调 fetchContextSuggestions，
  // 不先 clear results/ghost——避免「旧结果消失 → 新结果到达」的视觉闪烁。
  // 后端只在主窗口可见时才 emit，前端无需再判可见。
  listen(EVENTS.AWARENESS_UPDATED, () => {
    search.retrigger();
  });
}
