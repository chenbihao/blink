/**
 * 设置页入口模块
 * 协调各 Tab 模块，管理全局生命周期
 *
 * 0.9.5 前端架构重整：从 settings.js（4169行）拆分为模块化结构
 */

import { invoke } from "../tauri.js";
import { applyTheme } from "../theme.js";
import { t, applyI18n, setLang } from "../i18n/index.js";
import { ensureSpriteLoaded } from "../icon.js";
import { loadConfig, hideSettingsWindow } from "./shared/ipc.js";
import { setCurrentConfig } from "./shared/state.js";
import { initGeneralTab } from "./tabs/general.js";
import { initHotkeyTab } from "./tabs/hotkey.js";
import { initEnginesTab } from "./tabs/engines.js";
import { initPluginsTab } from "./tabs/plugins.js";
import { initNetworkTab } from "./tabs/network.js";
import { initContextTab } from "./tabs/context.js";
import { initStorageTab } from "./tabs/storage.js";
import { initDebugTab } from "./tabs/debug.js";
import { initAboutTab } from "./tabs/about.js";
import { initAITab } from "./tabs/ai.js";
import { initVoiceTab } from "./tabs/voice.js";
import { initChordTab } from "./tabs/chord.js";
import { initMcPTab } from "./tabs/mcp.js";
import { initMcpServerSection } from "./tabs/mcp-server.js";

// ── Tab 切换 ─────────────────────────────────────────────────────────────────

document.querySelectorAll(".tab").forEach((btn) => {
  btn.addEventListener("click", () => {
    document.querySelectorAll(".tab").forEach((t) => t.classList.remove("active"));
    document.querySelectorAll(".panel").forEach((p) => p.classList.remove("active"));
    btn.classList.add("active");
    document.getElementById(btn.dataset.tab).classList.add("active");
  });
});

// ── 配置加载与初始化 ─────────────────────────────────────────────────────────

/**
 * 应用配置到 UI
 * @param {Object} cfg - 配置对象
 */
function applyConfigToUI(cfg) {
  // 各 Tab 模块内部处理各自的 UI 更新
  // 这里只处理全局状态
  setCurrentConfig(cfg);
}

/**
 * 初始化设置页
 */
async function init() {
  try {
    // 图标 sprite 先注入（await 保证首屏无 FOUC —— tab 初始化时 innerHTML 拼图标就能立即用）
    await ensureSpriteLoaded();

    // 加载配置
    const cfg = await loadConfig();
    applyConfigToUI(cfg);

    // 应用主题
    applyTheme(cfg.theme || "auto");

    // 应用语言
    if (cfg.language) {
      setLang(cfg.language);
    }
    applyI18n();

    // 初始化各 Tab
    initGeneralTab(cfg);
    initHotkeyTab(cfg);
    initEnginesTab(cfg);
    initPluginsTab(cfg);
    initNetworkTab(cfg);
    initContextTab(cfg);
    initStorageTab(cfg);
    initDebugTab(cfg);
    initAboutTab(cfg);
  initAITab();
  initVoiceTab();
    initChordTab();
    initMcPTab();
initMcpServerSection();

    console.log("Settings initialized");
  } catch (e) {
    console.error("Failed to initialize settings:", e);
  }
}

// ── 事件监听 ─────────────────────────────────────────────────────────────────

// 窗口关闭按钮
document.getElementById("close-btn")?.addEventListener("click", hideSettingsWindow);

// ESC 隐藏窗口（与主窗口一致）
// 优先级降级：
//   1. 有可见 modal（AI provider / model edit / context picker 等）→ 交给 modal 内部处理器关闭
//   2. 正在录制热键 → 录制流程会 preventDefault 吞键，此处不处理
//   3. 否则调用 hide_settings_window
document.addEventListener("keydown", (e) => {
  if (e.key !== "Escape") return;
  // 有 modal 打开：让 modal 内部的 Escape 处理器负责关闭
  const modalOpen = Array.from(document.querySelectorAll(".modal-overlay")).some(
    (el) => !el.classList.contains("hidden"),
  );
  if (modalOpen) return;
  // 正在录制热键：交给录制流程
  if (document.querySelector(".hotkey-btn.recording")) return;
  e.preventDefault();
  hideSettingsWindow();
});

// 窗口 shown 事件刷新配置
window.addEventListener("focus", async () => {
  try {
    const cfg = await loadConfig();
    applyConfigToUI(cfg);
  } catch (e) {
    console.error("Failed to refresh config on focus:", e);
  }
});

// ── 启动 ─────────────────────────────────────────────────────────────────────

init();
