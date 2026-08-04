/**
 * 0.17.3 首次启动引导窗口
 *
 * 展示 6 个快捷键 + "开始使用"按钮。
 * 点击"开始使用"或关闭窗口 -> first_run=false，后续启动不再弹引导。
 */

import { invoke, getCurrentWindow } from "./shared/tauri.js";
import { applyI18nFromConfig, t, onLangChange } from "./i18n/index.js";
import { renderCombo } from "./shared/kbd.js";

// ── 快捷键数据 ────────────────────────────────────────────────────────────────

const SHORTCUTS = [
  { combo: "Alt+Space", labelKey: "welcome.shortcut.voice_input" },
  { combo: "Alt+Q", labelKey: "welcome.shortcut.chat" },
  { combo: "Alt+A", labelKey: "welcome.shortcut.screenshot" },
  { combo: "Alt+C", labelKey: "welcome.shortcut.clipboard_history" },
  { combo: "Alt+E", labelKey: "welcome.shortcut.edit" },
  { combo: "Alt+S", labelKey: "welcome.shortcut.sticky" },
];

// ── 渲染快捷键列表 ────────────────────────────────────────────────────────────

function renderShortcuts() {
  const container = document.getElementById("shortcut-list");
  if (!container) return;

  container.innerHTML = "";
  for (const { combo, labelKey } of SHORTCUTS) {
    const row = document.createElement("div");
    row.className = "welcome-shortcut-row";

    const label = document.createElement("span");
    label.className = "welcome-shortcut-label";
    label.textContent = t(labelKey);

    const keys = document.createElement("span");
    keys.className = "welcome-shortcut-keys";
    keys.appendChild(renderCombo(combo));

    row.appendChild(label);
    row.appendChild(keys);
    container.appendChild(row);
  }
}

// ── 初始化 ────────────────────────────────────────────────────────────────────

async function init() {
  // 加载语言配置 + 应用 i18n
  await applyI18nFromConfig();

  // 渲染快捷键列表
  renderShortcuts();

  // 语言切换时刷新快捷键标签
  onLangChange(() => renderShortcuts());

  // "开始使用"按钮
  const startBtn = document.getElementById("start-btn");
  if (startBtn) {
    startBtn.addEventListener("click", async () => {
      try {
        await invoke("set_config", { key: "first_run", value: false });
      } catch (e) {
        console.error("welcome: set_config first_run failed:", e);
      }
      getCurrentWindow()?.close();
    });
  }
}

init().catch((e) => console.error("welcome init failed:", e));
