/**
 * 设置页入口模块
 * 协调各 Tab 模块，管理全局生命周期
 *
 * 0.9.5 前端架构重整：从 settings.js（4169行）拆分为模块化结构
 */

import { invoke } from "../tauri.js";
import { applyTheme } from "../theme.js";
import { t, applyI18n, setLang } from "../i18n/index.js";
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
import { initChordTab } from "./tabs/chord.js";

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
    initChordTab();

    console.log("Settings initialized");
  } catch (e) {
    console.error("Failed to initialize settings:", e);
  }
}

// ── 事件监听 ─────────────────────────────────────────────────────────────────

// 窗口关闭按钮
document.getElementById("close-btn")?.addEventListener("click", hideSettingsWindow);

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
