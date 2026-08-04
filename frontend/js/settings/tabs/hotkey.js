/**
 * 快捷键设置 Tab 模块
 * 包含：快捷键录制、热键配置、滑块配置（tap阈值、grace期）
 */

import { invoke } from "../../shared/tauri.js";
import { t, onLangChange } from "../../i18n/index.js";
import { renderKey, renderCombo } from "../../shared/kbd.js";
import { saveConfig } from "../../shared/config-keys.js";
import { getCurrentConfig } from "../shared/state.js";

/**
 * 初始化快捷键设置 Tab
 * @param {Object} cfg - 初始配置
 */
export function initHotkeyTab(cfg) {
  // 配置回填（0.9.5 拆分时丢失的 applyConfigToUI 链路，0.9.5.1 补回）
  applyHotkeyConfig(cfg);
  initHotkeyRecording();
  initSliders();

  // 0.17.3：渲染 chord 快捷键速查表
  renderCheatsheet(cfg);

  // 语言切换时刷新滑块值标签（带参数的 {value}ms 无法用 data-i18n 处理）
  onLangChange(() => {
    const tapSlider = document.getElementById("tap-threshold");
    const tapValue = document.getElementById("tap-threshold-value");
    if (tapSlider && tapValue) {
      tapValue.textContent = t("hotkey.unit.ms", { value: tapSlider.value });
    }
    const graceSlider = document.getElementById("grace-ms");
    const graceValue = document.getElementById("grace-ms-value");
    if (graceSlider && graceValue) {
      graceValue.textContent = t("hotkey.unit.ms", { value: graceSlider.value });
    }
    // 语言切换时刷新速查表标签
    const currentConfig = getCurrentConfig();
    if (currentConfig) renderCheatsheet(currentConfig);
  });
}

/**
 * 初始化快捷键录制
 */
function initHotkeyRecording() {
  const hotkeyRecordBtn = document.getElementById("hotkey-record");
  const hotkeyResetBtn = document.getElementById("hotkey-reset");

  if (hotkeyRecordBtn) {
    hotkeyRecordBtn.addEventListener("click", async (e) => {
      e.stopPropagation();
      await startRecording();
    });
  }

  if (hotkeyResetBtn) {
    hotkeyResetBtn.addEventListener("click", async (e) => {
      e.stopPropagation();
      // 从后端拿真·默认值（HotkeyConfig::default()），避免前端硬编码字面量与后端漂移。
      // 历史 bug：曾把 RightAlt 当默认值，与后端 Alt+Space 不一致。
      let defaultHotkey;
      try {
        defaultHotkey = await invoke("get_default_hotkey");
      } catch (err) {
        console.error("[reset] get_default_hotkey failed:", err);
        return;
      }
      await saveConfig("hotkey", {
        modifiers: defaultHotkey.modifiers,
        key: defaultHotkey.key,
        display: defaultHotkey.display,
      });
      renderHotkeyInto(hotkeyRecordBtn, defaultHotkey.display);
      const currentConfig = getCurrentConfig();
      if (currentConfig) currentConfig.hotkey = defaultHotkey;
    });
  }
}

/**
 * 开始录制快捷键
 */
async function startRecording() {
  const hotkeyRecordBtn = document.getElementById("hotkey-record");
  const hotkeyResetBtn = document.getElementById("hotkey-reset");

  hotkeyRecordBtn.disabled = true;
  hotkeyResetBtn.disabled = true;
  hotkeyRecordBtn.classList.add("recording");
  hotkeyRecordBtn.textContent = t("hotkey.recording");

  // 录制期间吞掉所有键盘事件的默认行为
  const suppress = (e) => e.preventDefault();
  document.addEventListener("keydown", suppress, true);

  try {
    const result = await invoke("record_hotkey");
    console.log("[startRecording] record_hotkey resolved:", JSON.stringify(result));

    await saveConfig("hotkey", {
      modifiers: result.modifiers,
      key: result.key,
      display: result.display,
    });

    renderHotkeyInto(hotkeyRecordBtn, result.display);

    const currentConfig = getCurrentConfig();
    if (currentConfig) {
      currentConfig.hotkey = {
        modifiers: result.modifiers,
        key: result.key,
        display: result.display,
      };
    }
  } catch (e) {
    console.error("[startRecording] failed:", e);
    const currentConfig = getCurrentConfig();
    renderHotkeyInto(hotkeyRecordBtn, currentConfig?.hotkey?.display || "Alt+Space");
  } finally {
    document.removeEventListener("keydown", suppress, true);
    hotkeyRecordBtn.classList.remove("recording");
    hotkeyRecordBtn.disabled = false;
    hotkeyResetBtn.disabled = false;
  }
}

/**
 * 渲染快捷键到按钮
 * @param {HTMLElement} btn - 按钮元素
 * @param {string} display - 显示字符串
 */
function renderHotkeyInto(btn, display) {
  if (!btn) return;
  btn.innerHTML = "";
  const keys = display.split("+");
  keys.forEach((k, i) => {
    if (i > 0) {
      const plus = document.createElement("span");
      plus.textContent = "+";
      plus.className = "hotkey-plus";
      btn.appendChild(plus);
    }
    const kbd = document.createElement("kbd");
    kbd.textContent = k.trim();
    btn.appendChild(kbd);
  });
}

/**
 * 初始化滑块配置
 */
function initSliders() {
  // Tap 阈值滑块
  const tapSlider = document.getElementById("tap-threshold");
  const tapValue = document.getElementById("tap-threshold-value");

  if (tapSlider) {
    tapSlider.addEventListener("input", (e) => {
      tapValue.textContent = t("hotkey.unit.ms", { value: e.target.value });
    });

    tapSlider.addEventListener("change", async (e) => {
      const value = parseInt(e.target.value);
      try {
        await saveConfig("tap_threshold", value);
        const currentConfig = getCurrentConfig();
        if (currentConfig) currentConfig.tap_threshold = value;
      } catch (err) {
        console.error("update_tap_threshold failed:", err);
      }
    });
  }

  // Grace 期滑块
  const graceSlider = document.getElementById("grace-ms");
  const graceValue = document.getElementById("grace-ms-value");

  if (graceSlider) {
    graceSlider.addEventListener("input", (e) => {
      graceValue.textContent = t("hotkey.unit.ms", { value: e.target.value });
    });

    graceSlider.addEventListener("change", async (e) => {
      const value = parseInt(e.target.value);
      try {
        await saveConfig("grace_period", value);
        const currentConfig = getCurrentConfig();
        if (currentConfig) currentConfig.grace_period = value;
      } catch (err) {
        console.error("update_grace_period failed:", err);
      }
    });
  }
}

// ── 0.17.3：Chord 快捷键速查表 ─────────────────────────────────────────────

/**
 * 6 个 chord action 的元数据。
 * - id：action 标识符，对应 ChordBindings 的字段名
 * - labelKey：i18n key，功能名
 * - defaultKey：默认触发字母
 * - customizable：是否可在设置页改绑
 */
const CHEATSHEET_ACTIONS = [
  { id: "voice_input", labelKey: "hotkey.cheatsheet.voice_input", defaultKey: " ", customizable: false },
  { id: "chat", labelKey: "hotkey.cheatsheet.chat", defaultKey: "q", customizable: true },
  { id: "screenshot", labelKey: "hotkey.cheatsheet.screenshot", defaultKey: "a", customizable: true },
  { id: "clipboard_history", labelKey: "hotkey.cheatsheet.clipboard_history", defaultKey: "c", customizable: true },
  { id: "edit", labelKey: "hotkey.cheatsheet.edit", defaultKey: "e", customizable: false },
  { id: "sticky", labelKey: "hotkey.cheatsheet.sticky", defaultKey: "s", customizable: false },
];

/**
 * 计算 action 的生效组合键字符串。
 * - voice_input：使用全局 hotkey.display（如 "Alt+Space"）
 * - 其他："Alt+" + effective_key（binding.key 非空用 binding，否则用 defaultKey）
 */
function getEffectiveCombo(cfg, action) {
  if (action.id === "voice_input") {
    return cfg?.hotkey?.display || "Alt+Space";
  }
  const binding = cfg?.chord_bindings?.[action.id];
  const key = binding?.key && binding.key.length > 0 ? binding.key : action.defaultKey;
  // key 是单字符（如 "q"），转成组合键显示（如 "Alt+Q"）
  const upperKey = key.length === 1 ? key.toUpperCase() : key;
  return `Alt+${upperKey}`;
}

/**
 * 渲染 chord 快捷键速查表。
 * @param {Object} cfg - get_config 返回的配置对象
 */
function renderCheatsheet(cfg) {
  const tbody = document.getElementById("cheatsheet-body");
  if (!tbody) return;

  tbody.innerHTML = "";
  for (const action of CHEATSHEET_ACTIONS) {
    const tr = document.createElement("tr");

    // 功能名
    const nameTd = document.createElement("td");
    nameTd.className = "cheatsheet-name";
    nameTd.textContent = t(action.labelKey);
    tr.appendChild(nameTd);

    // 快捷键（用 renderCombo 渲染键帽）
    const keyTd = document.createElement("td");
    keyTd.className = "cheatsheet-key";
    const combo = getEffectiveCombo(cfg, action);
    keyTd.appendChild(renderCombo(combo));
    tr.appendChild(keyTd);

    // 状态（可改绑 + 修改链接 / 固定）
    const statusTd = document.createElement("td");
    statusTd.className = "cheatsheet-status";
    if (action.customizable) {
      const badge = document.createElement("span");
      badge.className = "cheatsheet-badge editable";
      badge.textContent = t("hotkey.cheatsheet.editable");
      statusTd.appendChild(badge);

      const modifyLink = document.createElement("a");
      modifyLink.className = "cheatsheet-modify";
      modifyLink.textContent = t("hotkey.cheatsheet.modify");
      modifyLink.href = "#";
      modifyLink.addEventListener("click", (e) => {
        e.preventDefault();
        // 在 settings 窗口内切到 chord tab
        const chordTab = document.querySelector('.tab[data-tab="chord"]');
        if (chordTab) chordTab.click();
      });
      statusTd.appendChild(modifyLink);
    } else {
      const badge = document.createElement("span");
      badge.className = "cheatsheet-badge locked";
      badge.textContent = t("hotkey.cheatsheet.locked");
      statusTd.appendChild(badge);
    }
    tr.appendChild(statusTd);

    tbody.appendChild(tr);
  }
}

/**
 * 把后端配置回填到快捷键表单
 * （拆自原 settings.js applyConfigToUI 的 hotkey 段）
 * @param {Object} cfg - get_config 返回的配置对象
 */
function applyHotkeyConfig(cfg) {
  if (!cfg) return;

  // 快捷键显示
  const hotkeyBtn = document.getElementById("hotkey-record");
  if (hotkeyBtn && cfg.hotkey) {
    renderHotkeyInto(hotkeyBtn, cfg.hotkey.display || "Alt+Space");
  }

  // tap 阈值
  const tapSlider = document.getElementById("tap-threshold");
  const tapValue = document.getElementById("tap-threshold-value");
  if (tapSlider && cfg.tap_threshold) {
    tapSlider.value = cfg.tap_threshold;
    tapValue.textContent = t("hotkey.unit.ms", { value: cfg.tap_threshold });
  }

  // grace period
  const graceSlider = document.getElementById("grace-ms");
  const graceValue = document.getElementById("grace-ms-value");
  if (graceSlider && cfg.grace_period) {
    graceSlider.value = cfg.grace_period;
    graceValue.textContent = t("hotkey.unit.ms", { value: cfg.grace_period });
  }
}
