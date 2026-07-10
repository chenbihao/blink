/**
 * 通用设置 Tab 模块
 * 包含：主题、语言、自动启动、搜索历史、结果数等
 */

import { applyTheme } from "../../theme.js";
import { t, applyI18n, setLang } from "../../i18n/index.js";
import { saveConfig } from "../../config-keys.js";
import { getCurrentConfig } from "../shared/state.js";
import { loadConfig } from "../shared/ipc.js";

/**
 * 初始化通用设置 Tab
 * @param {Object} cfg - 初始配置
 */
export function initGeneralTab(cfg) {
  // 配置回填到表单（0.9.5 拆分时丢失的 applyConfigToUI 链路，0.9.5.1 补回）
  applyGeneralConfig(cfg);

  // 自动启动
  const autoStartCheckbox = document.getElementById("auto-start");
  if (autoStartCheckbox) {
    autoStartCheckbox.addEventListener("change", async (e) => {
      try {
        await saveConfig("auto_start", e.target.checked);
        const currentConfig = getCurrentConfig();
        if (currentConfig) currentConfig.auto_start = e.target.checked;
      } catch (err) {
        console.error("update_auto_start failed:", err);
      }
    });
  }

  // 语言切换
  const languageSelect = document.getElementById("language");
  if (languageSelect) {
    languageSelect.addEventListener("change", async (e) => {
      const lang = e.target.value;
      try {
        await saveConfig("language", lang);
        const currentConfig = getCurrentConfig();
        if (currentConfig) currentConfig.language = lang;
        // 即时切换整页语言
        setLang(lang);
        applyI18n();
        // JS 动态生成的本地化文本（如 engines 状态徽章）由各 tab 通过
        // i18n.onLangChange 自行订阅刷新，无需在此跨 tab 调用。
      } catch (err) {
        console.error("update_language failed:", err);
      }
    });
  }

  // 主题切换
  const themeSelect = document.getElementById("theme");
  if (themeSelect) {
    themeSelect.addEventListener("change", async (e) => {
      const mode = e.target.value;
      applyTheme(mode); // 即时预览
      try {
        const g = readGeneral();
        await saveConfig("general_config", g);
        const currentConfig = getCurrentConfig();
        if (currentConfig) currentConfig.theme = mode;
      } catch (err) {
        console.error("update_general_config (theme) failed:", err);
      }
    });

    // 滚轮切换主题
    themeSelect.addEventListener("wheel", (e) => {
      e.preventDefault();
      const options = themeSelect.options;
      const currentIndex = themeSelect.selectedIndex;
      let newIndex;

      if (e.deltaY > 0) {
        newIndex = Math.min(currentIndex + 1, options.length - 1);
      } else {
        newIndex = Math.max(currentIndex - 1, 0);
      }

      if (newIndex === currentIndex) return;
      themeSelect.selectedIndex = newIndex;
      themeSelect.dispatchEvent(new Event("change"));
    });
  }

  // 窗口透明度
  const opacitySlider = document.getElementById("window-opacity");
  const opacityValue = document.getElementById("window-opacity-value");
  if (opacitySlider) {
    opacitySlider.addEventListener("input", (e) => {
      // 实时更新显示值
      if (opacityValue) opacityValue.textContent = `${e.target.value}%`;
    });
    opacitySlider.addEventListener("change", async (e) => {
      const opacity = parseInt(e.target.value, 10) / 100;
      try {
        await saveConfig("window_opacity", opacity);
        const currentConfig = getCurrentConfig();
        if (currentConfig) currentConfig.window_opacity = opacity;
      } catch (err) {
        console.error("update_window_opacity failed:", err);
      }
    });
  }

  // 搜索历史开关
  const shEnabledCheckbox = document.getElementById("search-history-enabled");
  if (shEnabledCheckbox) {
    shEnabledCheckbox.addEventListener("change", async (e) => {
      try {
        const g = readGeneral();
        await saveConfig("general_config", g);
        const currentConfig = getCurrentConfig();
        if (currentConfig) currentConfig.search_history_enabled = e.target.checked;
      } catch (err) {
        console.error("update_general_config (history enabled) failed:", err);
      }
    });
  }

  // 搜索历史天数
  const shDaysInput = document.getElementById("search-history-days");
  if (shDaysInput) {
    shDaysInput.addEventListener("change", async () => {
      try {
        const g = readGeneral();
        await saveConfig("general_config", g);
        const currentConfig = getCurrentConfig();
        if (currentConfig) currentConfig.search_history_days = g.search_history_days;
      } catch (err) {
        console.error("update_general_config (history days) failed:", err);
      }
    });
  }

  // 最大结果数
  const maxResultsInput = document.getElementById("max-results");
  if (maxResultsInput) {
    maxResultsInput.addEventListener("change", async () => {
      try {
        const g = readGeneral();
        await saveConfig("general_config", g);
        const currentConfig = getCurrentConfig();
        if (currentConfig) currentConfig.max_results = g.max_results;
      } catch (err) {
        console.error("update_general_config (max results) failed:", err);
      }
    });
  }

  // 每页结果数
  const pageSizeInput = document.getElementById("page-size");
  if (pageSizeInput) {
    pageSizeInput.addEventListener("change", async () => {
      try {
        const g = readGeneral();
        await saveConfig("general_config", g);
        const currentConfig = getCurrentConfig();
        if (currentConfig) currentConfig.page_size = g.page_size;
      } catch (err) {
        console.error("update_general_config (page size) failed:", err);
      }
    });
  }

  // Autosuggestion
  initAutosuggestion();

  // Chord 总控
  initChordToggles();
}

/**
 * 读取通用配置字段
 * @returns {Object} 通用配置对象
 */
function readGeneral() {
  const val = (id, fb) => (document.getElementById(id)?.value ?? fb);
  const checked = (id, fb) => (document.getElementById(id)?.checked ?? fb);
  return {
    theme: val("theme", "auto"),
    search_history_enabled: checked("search-history-enabled", true),
    search_history_days: parseInt(val("search-history-days", "30"), 10) || 0,
    max_results: parseInt(val("max-results", "50"), 10) || 50,
    page_size: parseInt(val("page-size", "9"), 10) || 9,
  };
}

/**
 * 初始化 Autosuggestion 设置
 */
function initAutosuggestion() {
  async function saveAutosuggest() {
    const enabled = document.getElementById("autosuggest-enabled")?.checked !== false;
    const scoreRaw = document.getElementById("autosuggest-min-score")?.value ?? "0.7";
    const minScore = Math.min(0.95, Math.max(0.5, parseFloat(scoreRaw) || 0.7));
    const tabKey = document.getElementById("autosuggest-tab-key")?.value || "Tab";
    try {
      await saveConfig("autosuggest", { enabled, minScore, tabKey });
      const currentConfig = getCurrentConfig();
      if (currentConfig) {
        currentConfig.autosuggest_enabled = enabled;
        currentConfig.autosuggest_min_score = minScore;
        currentConfig.autosuggest_tab_key = tabKey;
      }
    } catch (err) {
      console.error("update_autosuggest_config failed:", err);
    }
  }

  const autosuggestEnabledEl = document.getElementById("autosuggest-enabled");
  if (autosuggestEnabledEl) autosuggestEnabledEl.addEventListener("change", saveAutosuggest);
  const autosuggestMinScoreEl = document.getElementById("autosuggest-min-score");
  if (autosuggestMinScoreEl) autosuggestMinScoreEl.addEventListener("change", saveAutosuggest);
  const autosuggestTabKeyEl = document.getElementById("autosuggest-tab-key");
  if (autosuggestTabKeyEl) autosuggestTabKeyEl.addEventListener("change", saveAutosuggest);
}

/**
 * 初始化 Chord 总控设置
 */
function initChordToggles() {
  async function saveChordToggles() {
    const chordEnabled = document.getElementById("chord-enabled")?.checked === true;
    const chordHintVisible = document.getElementById("chord-hint-visible")?.checked === true;
    try {
      await saveConfig("chord_toggles", { chordEnabled, chordHintVisible });
      const currentConfig = getCurrentConfig();
      if (currentConfig) {
        currentConfig.chord_enabled = chordEnabled;
        currentConfig.chord_hint_visible = chordHintVisible;
      }
    } catch (err) {
      console.error("update_chord_toggles failed:", err);
    }
  }

  async function saveClipboardEnabled() {
    const enabled = document.getElementById("clipboard-enabled")?.checked !== false;
    try {
      await saveConfig("clipboard_enabled", enabled);
      const currentConfig = getCurrentConfig();
      if (currentConfig?.clipboard) {
        currentConfig.clipboard.enabled = enabled;
      }
    } catch (err) {
      console.error("update_clipboard_enabled failed:", err);
    }
  }

  const chordEnabledEl = document.getElementById("chord-enabled");
  if (chordEnabledEl) chordEnabledEl.addEventListener("change", saveChordToggles);
  const chordHintVisibleEl = document.getElementById("chord-hint-visible");
  if (chordHintVisibleEl) chordHintVisibleEl.addEventListener("change", saveChordToggles);
}

// 〔日志设置归 debug.js、存储设置归 storage.js。此 4 函数原为 0.9.5 拆分时误塞进
//   general.js，且引用了未 import 的 invoke/confirmDialog，导致 initGeneralTab 抛
//   ReferenceError、中断 index.js init() 后续所有 tab 初始化（hotkey/engines/plugins/
//   network/context/storage/debug/about/ai/chord 全部没跑，插件页空/CHORD 空/AI 坏/
//   调试坏/文件搜索坏的共同根因）。0.9.5.1 移除，由各自归属 tab 承接。〕

/**
 * 把后端配置回填到通用设置表单
 * （拆自原 settings.js applyConfigToUI 的 general 字段段；setLang/applyTheme 由 index.js 统一处理）
 * @param {Object} cfg - get_config 返回的配置对象
 */
function applyGeneralConfig(cfg) {
  if (!cfg) return;

  // 主题 / 语言 / 日志级别
  const themeSel = document.getElementById("theme");
  if (themeSel && cfg.theme) themeSel.value = cfg.theme;
  const languageSel = document.getElementById("language");
  if (languageSel && cfg.language) languageSel.value = cfg.language;
  const logLevel = document.getElementById("log-level");
  if (logLevel && cfg.log_level) logLevel.value = cfg.log_level;

  // 自动启动（false 也是有效值，用 !== undefined 守卫）
  const autoStart = document.getElementById("auto-start");
  if (autoStart && cfg.auto_start !== undefined) autoStart.checked = cfg.auto_start;

  // 搜索历史
  const shEnabled = document.getElementById("search-history-enabled");
  if (shEnabled && cfg.search_history_enabled !== undefined) shEnabled.checked = cfg.search_history_enabled;
  const shDays = document.getElementById("search-history-days");
  if (shDays && cfg.search_history_days !== undefined) shDays.value = cfg.search_history_days;

  // 结果数
  const maxResults = document.getElementById("max-results");
  if (maxResults && cfg.max_results !== undefined) maxResults.value = cfg.max_results;
  const pageSize = document.getElementById("page-size");
  if (pageSize && cfg.page_size !== undefined) pageSize.value = cfg.page_size;

  // 窗口透明度
  const opacitySlider = document.getElementById("window-opacity");
  const opacityValue = document.getElementById("window-opacity-value");
  if (opacitySlider && cfg.window_opacity !== undefined) {
    const percent = Math.round(cfg.window_opacity * 100);
    opacitySlider.value = percent;
    if (opacityValue) opacityValue.textContent = `${percent}%`;
  }

  // Autosuggestion（默认开：!== false）
  const autoEnabled = document.getElementById("autosuggest-enabled");
  if (autoEnabled) autoEnabled.checked = cfg.autosuggest_enabled !== false;
  const autoScore = document.getElementById("autosuggest-min-score");
  if (autoScore && typeof cfg.autosuggest_min_score === "number") {
    autoScore.value = cfg.autosuggest_min_score.toFixed(2);
  }
  const autoTabKey = document.getElementById("autosuggest-tab-key");
  if (autoTabKey && typeof cfg.autosuggest_tab_key === "string") autoTabKey.value = cfg.autosuggest_tab_key;

  // Chord（默认关：=== true；hint 默认开：!== false）
  const chordEnabled = document.getElementById("chord-enabled");
  if (chordEnabled) chordEnabled.checked = cfg.chord_enabled === true;
  const chordHint = document.getElementById("chord-hint-visible");
  if (chordHint) chordHint.checked = cfg.chord_hint_visible !== false;
}
