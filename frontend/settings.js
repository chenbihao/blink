import { invoke, confirmDialog, messageDialog } from "./js/tauri.js";
import { applyTheme } from "./js/theme.js";
import { t, applyI18n, setLang } from "./js/i18n.js";
import { renderKey } from "./js/kbd.js";
import { saveConfig } from "./js/config-keys.js";

// WebView2 下按 Alt 会激活宿主窗口的系统菜单、进入菜单模态，webview 消息泵随之
// 暂停——后端返回的 invoke 响应会堆在队列里无法分发，表现就是「录制时按钮卡住、
// 非得点一下鼠标才恢复」。设置页本身不需要 Alt 菜单，这里在捕获阶段吞掉 Alt 的
// 默认行为以规避。若仍无效，需改用 Win32 拦截 SC_KEYMENU（更底层）。
document.addEventListener(
  "keydown",
  (e) => {
    if (e.key === "Alt") e.preventDefault();
  },
  true
);

// ESC 键隐藏设置窗口
document.addEventListener("keydown", async (e) => {
  if (e.key === "Escape") {
    try {
      // 设置窗口调用专门的隐藏命令（不影响主窗口）
      await invoke("hide_settings_window");
    } catch (err) {
      console.error("hide_settings_window failed:", err);
    }
  }
});

// ── 主题：复用主窗口 js/theme.js（设置页改 module 后 import，消除原内联重复）──
// 注意：theme.js 的 applyTheme 在 auto 模式会挂系统主题监听，比原内联版更完整。

// ── Tab 切换 ─────────────────────────────────────────────────────────────────

document.querySelectorAll(".tab").forEach((btn) => {
  btn.addEventListener("click", () => {
    document.querySelectorAll(".tab").forEach((t) => t.classList.remove("active"));
    document.querySelectorAll(".panel").forEach((p) => p.classList.remove("active"));
    btn.classList.add("active");
    document.getElementById(btn.dataset.tab).classList.add("active");
  });
});

// ── 配置加载与保存 ───────────────────────────────────────────────────────────

let currentConfig = null;

/**
 * 把 hotkey display 字符串（如 "RightAlt" / "Ctrl+Shift+Space"）渲染成按钮内的键帽 DOM。
 * 用于快捷键录制按钮：非录制态显示 <kbd> 键帽 + "+" 连接符；录制态由调用点直接
 * `textContent = t("hotkey.recording")` 覆写为纯文字。
 *
 * @param {HTMLElement} btn 目标按钮
 * @param {string} display 后端返回的 hotkey.display（"+"分隔）
 */
function renderHotkeyInto(btn, display) {
  btn.replaceChildren();
  const parts = String(display || "").split("+").filter(Boolean);
  parts.forEach((p, i) => {
    if (i > 0) {
      const plus = document.createElement("span");
      plus.className = "kbd-plus";
      plus.textContent = "+";
      btn.appendChild(plus);
    }
    btn.appendChild(renderKey(p));
  });
}

async function loadConfig() {
  try {
    currentConfig = await invoke("get_config");
    applyConfigToUI(currentConfig);
  } catch (e) {
    console.error("loadConfig failed:", e);
  }
}

function applyConfigToUI(config) {
  // 快捷键显示
  const hotkeyBtn = document.getElementById("hotkey-record");
  if (hotkeyBtn && config.hotkey) {
    renderHotkeyInto(hotkeyBtn, config.hotkey.display || "RightAlt");
  }

  // tap 阈值
  const tapSlider = document.getElementById("tap-threshold");
  const tapValue = document.getElementById("tap-threshold-value");
  if (tapSlider && config.tap_threshold) {
    tapSlider.value = config.tap_threshold;
    tapValue.textContent = t("hotkey.unit.ms", { value: config.tap_threshold });
  }

  // grace period
  const graceSlider = document.getElementById("grace-ms");
  const graceValue = document.getElementById("grace-ms-value");
  if (graceSlider && config.grace_period) {
    graceSlider.value = config.grace_period;
    graceValue.textContent = t("hotkey.unit.ms", { value: config.grace_period });
  }

  // 开机自启
  const autoStart = document.getElementById("auto-start");
  if (autoStart && config.auto_start !== undefined) {
    autoStart.checked = config.auto_start;
  }

  // 语言
  const language = document.getElementById("language");
  if (language && config.language) {
    language.value = config.language;
  }

  // 应用界面语言（静态文本；动态区在各自 load 函数里用 t() 渲染）
  setLang(config.language || "zh");
  applyI18n();

  // 日志级别
  const logLevel = document.getElementById("log-level");
  if (logLevel && config.log_level) {
    logLevel.value = config.log_level;
  }

  // 主题
  const themeSel = document.getElementById("theme");
  if (themeSel && config.theme) {
    themeSel.value = config.theme;
  }

  // 搜索历史开关
  const shEnabled = document.getElementById("search-history-enabled");
  if (shEnabled && config.search_history_enabled !== undefined) {
    shEnabled.checked = config.search_history_enabled;
  }

  // 历史保留天数
  const shDays = document.getElementById("search-history-days");
  if (shDays && config.search_history_days !== undefined) {
    shDays.value = config.search_history_days;
  }

  // 最大结果数
  const maxResultsEl = document.getElementById("max-results");
  if (maxResultsEl && config.max_results !== undefined) {
    maxResultsEl.value = config.max_results;
  }

  // 每页显示条数
  const pageSizeEl = document.getElementById("page-size");
  if (pageSizeEl && config.page_size !== undefined) {
    pageSizeEl.value = config.page_size;
  }

  // Autosuggestion（0.8.1 §2.8）
  const autoEnabledEl = document.getElementById("autosuggest-enabled");
  if (autoEnabledEl) autoEnabledEl.checked = config.autosuggest_enabled !== false;
  const autoScoreEl = document.getElementById("autosuggest-min-score");
  if (autoScoreEl && typeof config.autosuggest_min_score === "number") {
    autoScoreEl.value = config.autosuggest_min_score.toFixed(2);
  }
  const autoTabKeyEl = document.getElementById("autosuggest-tab-key");
  if (autoTabKeyEl && typeof config.autosuggest_tab_key === "string") {
    autoTabKeyEl.value = config.autosuggest_tab_key;
  }

  // Chord 交互（0.8.5 §6.6 · 0.8.7 起默认关闭：新用户 opt-in）
  const chordEnabledEl = document.getElementById("chord-enabled");
  if (chordEnabledEl) chordEnabledEl.checked = config.chord_enabled === true;
  const chordHintEl = document.getElementById("chord-hint-visible");
  if (chordHintEl) chordHintEl.checked = config.chord_hint_visible !== false;
  // #clipboard-enabled 由 loadContextConfig 从 currentConfig.clipboard.enabled 初始化(卡异步渲染)

  // 应用主题（设置页本身即时正确显示）
  applyTheme(config.theme || "auto");

  // 应用配置加载完成后，再加载引擎配置
  loadEngineConfig();
}

// 加载引擎配置（应用搜索 / 文件搜索 / 计算器）
async function loadEngineConfig() {
  // 加载应用搜索配置
  try {
    const startMenu = await invoke("get_start_menu_config");
    const enabledEl = document.getElementById("start-menu-enabled");
    const depthEl = document.getElementById("start-menu-scan-depth");
    const includeUwpEl = document.getElementById("start-menu-include-uwp");
    if (enabledEl) enabledEl.checked = startMenu.enabled !== false;
    if (depthEl) depthEl.value = startMenu.scan_depth || 3;
    if (includeUwpEl) includeUwpEl.checked = startMenu.include_uwp !== false;
  } catch (e) {
    console.error("loadStartMenuConfig failed:", e);
  }

  // 加载文件搜索配置
  try {
    const fileSearch = await invoke("get_engine_config", { engineId: "file_search" });
    const enabled = fileSearch.enabled !== false;
    const dataSource = fileSearch.data_source || "auto";
    const port = fileSearch.everything_port || 80;
    const maxResults = fileSearch.max_results || 20;

    const enabledEl = document.getElementById("file-search-enabled");
    const dataSourceEl = document.getElementById("file-search-data-source");
    const portEl = document.getElementById("everything-port");
    const maxResultsEl = document.getElementById("everything-max-results");

    if (enabledEl) enabledEl.checked = enabled;
    if (dataSourceEl) dataSourceEl.value = dataSource;
    if (portEl) portEl.value = port;
    if (maxResultsEl) maxResultsEl.value = maxResults;

    // 根据数据源显示/隐藏 Everything 配置
    updateEverythingConfigVisibility(dataSource);

    // 页面加载后自动探测一次（非 local 模式）
    if (dataSource !== "local") {
      setTimeout(probeEverythingStatus, 500);
    }
  } catch (e) {
    console.error("loadFileSearchConfig failed:", e);
    document.getElementById("everything-port").value = 80;
  }

  // 加载计算器配置
  try {
    const calc = await invoke("get_calc_config");
    const enabledEl = document.getElementById("calc-enabled");
    if (enabledEl) enabledEl.checked = calc.enabled !== false;
  } catch (e) {
    console.error("loadCalcConfig failed:", e);
  }

  // 加载内置动作列表（0.8.0 §1.3）
  loadBuiltinActions();

  // 加载插件列表
  loadPlugins();
}

// ── 内置动作面板（0.8.0 §1.3）─────────────────────────────────────────────────

/** 按 id 映射的图标（emoji）。后端不下发图标，前端集中管理。 */
const BUILTIN_ACTION_ICONS = {
  open_settings: "⚙️",
  lock: "🔒",
  shutdown: "⏻",
  restart: "🔁",
  sleep: "🌙",
  clear_history: "🧹",
  exit_blink: "🚪",
  open_logs: "📄",
  open_data_dir: "🗂️",
  // 0.8.0 §1.3 参数化动作
  open_url: "🔗",
  open_path: "📁",
  reveal_in_explorer: "🔍",
};

/** 拉取动作列表并渲染。 */
async function loadBuiltinActions() {
  const list = document.getElementById("builtin-actions-list");
  if (!list) return;
  try {
    const actions = await invoke("list_builtin_actions");
    if (!Array.isArray(actions) || actions.length === 0) {
      list.innerHTML = `<p class="hint" style="padding: 12px 0;">${t("engine.builtin_actions.empty")}</p>`;
      return;
    }
    list.innerHTML = actions.map(renderBuiltinActionRow).join("");
    // 事件委托只绑定一次（loadBuiltinActions 可能被多次调用，如失败回滚）
    if (!list.dataset.eventsBound) {
      bindBuiltinActionEvents(list);
      list.dataset.eventsBound = "1";
    }
  } catch (e) {
    console.error("loadBuiltinActions failed:", e);
    list.innerHTML = `<p class="hint msg-error" style="padding: 12px 0;">${escapeHtml(String(e))}</p>`;
  }
}

/** 单个动作行：图标 + 标题/副标题/触发方式/参数来源 + disable 开关。 */
function renderBuiltinActionRow(a) {
  const icon = BUILTIN_ACTION_ICONS[a.id] || "•";
  const keywords = (a.keywords || []).join(" / ");
  // 参数来源与触发方式合成副行第二段（都是元信息，视觉密度小）
  // trigger_desc 为空 = 纯 keyword 触发，前面 keywords 一行已表达清楚，无需重复
  const meta = [
    `<span>${t("engine.builtin_actions.keywords_label")}: ${escapeHtml(keywords)}</span>`,
    a.trigger_desc ? `<span>${escapeHtml(a.trigger_desc)}</span>` : "",
    a.param_desc
      ? `<span>${t("engine.builtin_actions.param_label")}: ${escapeHtml(a.param_desc)}</span>`
      : "",
  ]
    .filter(Boolean)
    .join(" · ");

  return `<div class="builtin-action-row" data-action-id="${escapeAttr(a.id)}">
    <div class="builtin-action-icon">${icon}</div>
    <div class="builtin-action-info">
      <div class="builtin-action-title">${escapeHtml(a.title)}</div>
      <div class="builtin-action-subtitle">${escapeHtml(a.subtitle)}</div>
      <div class="builtin-action-meta">${meta}</div>
    </div>
    <label class="switch">
      <input type="checkbox" class="builtin-action-toggle" ${a.enabled ? "checked" : ""} />
      <span class="slider"></span>
    </label>
  </div>`;
}

/** 事件委托：任一开关变化 → 收集所有 disable id → 一次性写回后端。 */
function bindBuiltinActionEvents(list) {
  list.addEventListener("change", async (e) => {
    if (!e.target.classList.contains("builtin-action-toggle")) return;
    const disabled = [];
    list.querySelectorAll(".builtin-action-row").forEach((row) => {
      const toggle = row.querySelector(".builtin-action-toggle");
      if (toggle && !toggle.checked) disabled.push(row.dataset.actionId);
    });
    // 0.8.7 UX 统一:自动保存静默成功,只保留失败态红字。
    const msg = document.getElementById("builtin-actions-save-msg");
    if (msg) msg.textContent = "";
    try {
      await saveConfig("disabled_builtin_actions", disabled);
    } catch (err) {
      console.error("set_disabled_builtin_actions failed:", err);
      if (msg) {
        msg.textContent = `${t("engine.builtin_actions.save_failed")}: ${err}`;
        msg.className = "plugin-save-msg msg-error";
      }
      // 回滚 UI 状态：重新加载列表
      loadBuiltinActions();
    }
  });
}

// 根据数据源模式显示/隐藏 Everything 配置
function updateEverythingConfigVisibility(dataSource) {
  const everythingConfig = document.getElementById("everything-config");
  if (everythingConfig) {
    everythingConfig.style.display = dataSource === "local" ? "none" : "";
  }
}

// 插件图标映射（按 manifest.id 匹配）
const PLUGIN_ICONS = {
  "builtin.ip": "🌐",
  "builtin.echo": "🔊",
  "builtin.ai": "🤖",
  "builtin.translate": "📝",
  "builtin.weather": "🌤️",
};

// 加载并渲染网络配置（全局代理，0.5.1：独立 Tab，本体/插件共用）
async function loadNetworkConfig() {
  const container = document.getElementById("network-container");
  if (!container) return;

  let proxyConfig = { http: "", https: "" };
  try {
    const cfg = await invoke("get_engine_config", { engineId: "_global_proxy" });
    if (cfg) {
      proxyConfig = { http: cfg.http || "", https: cfg.https || "" };
    }
  } catch (e) {
    console.error("load proxy config failed:", e);
  }

  const PROXY_SCHEMA = [
    textField("http_proxy", t("network.http.label"), { placeholder: t("network.http.ph") }),
    textField("https_proxy", t("network.https.label"), { placeholder: t("network.https.ph") }),
  ];

  container.innerHTML = renderExtensionCard({
    icon: "🌐",
    title: t("network.title"),
    desc: t("network.desc"),
    body: renderConfigSection(t("network.section"), PROXY_SCHEMA, proxyConfig, { saveLabel: t("network.save"), flat: true }),
  });

  // 绑定保存事件
  const btn = container.querySelector(".plugin-save");
  const msg = container.querySelector(".plugin-save-msg");
  if (!btn) return;
  btn.addEventListener("click", async () => {
    const http = container.querySelector('.plugin-field[data-key="http_proxy"]')?.value || "";
    const https = container.querySelector('.plugin-field[data-key="https_proxy"]')?.value || "";
    try {
      await saveConfig("global_proxy", { http, https });
      // 网络代理留长句"已保存,下次查询自动生效"——用户需要知道生效时机(不必重启)
      if (msg) { msg.textContent = t("network.saved_msg"); msg.className = "plugin-save-msg msg-success"; setTimeout(() => { if (msg) { msg.textContent = ""; msg.className = "plugin-save-msg"; } }, 3000); }
      clearUnsaved(container);
    } catch (e) {
      console.error("save proxy failed:", e);
      if (msg) { msg.textContent = t("network.save_failed"); msg.className = "plugin-save-msg msg-error"; }
    }
  });
}

// 加载并渲染上下文配置（0.8.9 UX：拆成两卡 — 采集卡 + 过滤卡 —— 敏感应用独立）
async function loadContextConfig() {
  const captureContainer = document.getElementById("context-container");
  const filterContainer = document.getElementById("context-filter-container");
  if (!captureContainer || !filterContainer) return;

  let cfg = { enabled: true, clipboardEnabled: true, selectionEnabled: true, sensitive_apps: [] };
  try {
    const data = await invoke("get_context_config");
    if (data) cfg = data;
  } catch (e) {
    console.error("load context config failed:", e);
  }

  // 剪贴板历史录入的初值不能靠 currentConfig(可能还没加载),直接单独拉一次 get_config
  let clipboardHistEnabled = true;
  try {
    const fullCfg = await invoke("get_config");
    if (fullCfg && fullCfg.clipboard) {
      clipboardHistEnabled = fullCfg.clipboard.enabled !== false;
    }
  } catch (e) {
    console.error("load clipboard enabled failed:", e);
  }

  // ── 本地状态（敏感应用列表在两卡间共享，走同一次 save）──
  let sensitiveApps = [...(cfg.sensitive_apps || [])];

  // ── ① 采集卡（三个即时采集开关 + 总开关）──
  const CLIPBOARD_FIELD = booleanField("clipboard_enabled", t("context.clipboard"));
  const SELECTION_FIELD = booleanField("selection_enabled", t("context.selection"), {
    description: t("context.selection.hint"),
  });
  const enableSwitch = `<label class="switch"><input type="checkbox" class="context-enabled" ${cfg.enabled ? "checked" : ""} /><span class="slider"></span></label>`;

  // 剪贴板历史录入开关(0.8.7 UX：从 Chord tab 迁至此处,与"采集剪贴板文本"同框)
  // 走独立命令 update_clipboard_enabled,不属于 update_context_config payload
  // ——语义分离:context.clipboard_enabled 是"即时读",clipboard.enabled 是"历史录入"
  const clipboardHistoryFieldHtml = `<div class="setting-row">
      <label class="setting-label">${t("chord.clipboard.enabled.label")}<span class="field-hint-icon" title="${escapeAttr(t("chord.clipboard.enabled.hint"))}">ⓘ</span></label>
      <label class="switch switch-sm"><input type="checkbox" id="clipboard-enabled" ${clipboardHistEnabled ? "checked" : ""} /><span class="slider"></span></label>
    </div>`;

  captureContainer.innerHTML = renderExtensionCard({
    icon: "🌍",
    title: t("context.title"),
    desc: t("context.desc"),
    headerRight: enableSwitch,
    attrs: "data-autosave",
    body: `${renderSettingField(CLIPBOARD_FIELD, cfg.clipboardEnabled, true)}
        ${renderSettingField(SELECTION_FIELD, cfg.selectionEnabled, true)}
        ${clipboardHistoryFieldHtml}`,
  });

  // ── ② 过滤卡（敏感应用独立）──
  filterContainer.innerHTML = renderExtensionCard({
    icon: "🛡",
    title: t("context.filter.title"),
    desc: t("context.filter.desc"),
    attrs: "data-autosave",
    body: `<div class="context-sensitive-list"></div>
        <div class="context-sensitive-actions">
          <button class="btn-small context-add-btn">${t("context.add_app")}</button>
        </div>`,
  });

  // ── 渲染敏感应用列表（chip 样式 + × 移除）──
  function renderSensitiveList() {
    const listEl = filterContainer.querySelector(".context-sensitive-list");
    if (!listEl) return;
    if (sensitiveApps.length === 0) {
      listEl.innerHTML = `<div class="hint" style="padding: 4px 0;">${t("context.empty")}</div>`;
      return;
    }
    listEl.innerHTML = sensitiveApps
      .map(
        (name, i) =>
          `<span class="context-chip" data-idx="${i}">
            ${escapeHtml(name)}
            <span class="context-chip-remove" data-idx="${i}" title="${t("context.remove.title")}">×</span>
          </span>`
      )
      .join("");
    // 绑定移除
    listEl.querySelectorAll(".context-chip-remove").forEach((el) => {
      el.addEventListener("click", async (e) => {
        e.stopPropagation();
        const idx = parseInt(el.dataset.idx, 10);
        sensitiveApps.splice(idx, 1);
        renderSensitiveList();
        await save();
      });
    });
  }
  renderSensitiveList();

  // ── 自动保存 ──
  // 设计约定(0.8.7 UX 统一):自动保存"静默成功、喧哗失败"——UI 状态变更本身就是反馈,
  // 不再显示"✓ 已自动保存",避免噪音;失败保留红字并回滚由调用侧处理。
  async function save() {
    const enabled = captureContainer.querySelector(".context-enabled").checked;
    const clipboardEnabled = captureContainer.querySelector('.plugin-field[data-key="clipboard_enabled"]')?.checked ?? true;
    const selectionEnabled = captureContainer.querySelector('.plugin-field[data-key="selection_enabled"]')?.checked ?? true;
    try {
      await saveConfig("context_config", { enabled, clipboardEnabled, selectionEnabled, sensitive_apps: [...sensitiveApps] });
    } catch (e) {
      console.error("save context config failed:", e);
    }
  }

  // 总开关 + 剪贴板采集开关 + 划词开关 → change 自动保存
  captureContainer.querySelector(".context-enabled")?.addEventListener("change", save);
  captureContainer.querySelector('.plugin-field[data-key="clipboard_enabled"]')?.addEventListener("change", save);
  captureContainer.querySelector('.plugin-field[data-key="selection_enabled"]')?.addEventListener("change", save);
  // 剪贴板历史录入(0.8.7 从 Chord tab 迁入,走独立命令 update_clipboard_enabled)
  captureContainer.querySelector("#clipboard-enabled")?.addEventListener("change", saveClipboardEnabled);

  // ── 添加应用弹窗（敏感应用在过滤卡内）──
  filterContainer.querySelector(".context-add-btn")?.addEventListener("click", async () => {
    await showAddProcessModal(filterContainer, sensitiveApps, async (added) => {
      sensitiveApps.push(...added);
      // 去重
      sensitiveApps = [...new Set(sensitiveApps)];
      renderSensitiveList();
      await save();
    });
  });
}

/**
 * 加载并渲染 Context 触发规则列表（0.8.9 UX：卡头总开关 + 只读能力清单）。
 *
 * 从后端 `list_context_bindings` 拉所有已注册 binding + enabled 状态。
 * 卡头总开关驱动全部规则：
 *   - 关 → 传所有 key 到 `set_disabled_context_bindings`（全禁）
 *   - 开 → 传空数组（全启）
 * 初值宽容:任一 binding 启用即视为"总开关开",覆盖历史"部分禁用"态。
 */
async function loadContextBindings() {
  const card = document.getElementById("context-triggers-card");
  const container = document.getElementById("context-bindings-container");
  const masterToggle = document.getElementById("context-triggers-enabled");
  if (!container || !card || !masterToggle) return;

  let bindings = [];
  try {
    bindings = await invoke("list_context_bindings");
  } catch (e) {
    console.error("list_context_bindings failed:", e);
  }

  if (!Array.isArray(bindings) || bindings.length === 0) {
    container.innerHTML = `<div class="action-list-empty">${t("context.bindings.empty")}</div>`;
    masterToggle.checked = false;
    masterToggle.disabled = true;
    card.classList.add("is-disabled");
    return;
  }
  masterToggle.disabled = false;

  // trigger_key → 图标(0.8.7 UX 重排:动作行加视觉锚点,识别度对齐引擎/插件卡)
  const TRIGGER_ICONS = {
    text_is_non_target_lang: "🌐",
    clipboard_is_url: "🔗",
    clipboard_is_file_path: "📂",
    selection_non_empty: "✂️",
  };

  // 只读能力清单：一行 icon + trigger → target,不带单项开关
  container.innerHTML = bindings
    .map((b) => {
      const icon = TRIGGER_ICONS[b.trigger_key] || "•";
      const triggerI18nKey = `context.trigger.${b.trigger_key}`;
      const triggerLabel = t(triggerI18nKey) || b.trigger_key;
      const targetLabel = escapeHtml(b.target_label || b.target_id);
      return `<div class="action-list-row" data-binding-key="${escapeHtml(b.key)}">
        <div class="action-icon">${icon}</div>
        <div class="action-info">
          <div class="action-title">${escapeHtml(triggerLabel)} → ${targetLabel}</div>
        </div>
      </div>`;
    })
    .join("");

  // 初值:只要有任一 binding 启用即"总开关开"(宽容历史部分禁用状态)
  const anyEnabled = bindings.some((b) => b.enabled);
  masterToggle.checked = anyEnabled;
  card.classList.toggle("is-disabled", !anyEnabled);

  // 总开关持久化:关 → 全部 key 加入 disabled;开 → 空 disabled
  const allKeys = bindings.map((b) => b.key);
  masterToggle.addEventListener("change", async () => {
    const enabled = masterToggle.checked;
    card.classList.toggle("is-disabled", !enabled);
    try {
      await saveConfig("disabled_context_bindings", enabled ? [] : allKeys);
    } catch (e) {
      console.error("set_disabled_context_bindings failed:", e);
      // 回滚 UI 状态
      masterToggle.checked = !enabled;
      card.classList.toggle("is-disabled", enabled);
    }
  });
}

// ── Chord 动作列表（0.8.5 §6.6）─────────────────────────────────────────
/**
 * 加载并渲染 Chord 动作开关列表。
 * 每条动作一个 setting-row + 开关；取消勾选后经 `set_disabled_chord_actions` 写回。
 * 注:Context bindings 已在 0.8.9 UX 改为"卡头总开关 + 只读清单",此处仍保留逐条开关
 *    (Chord 每条动作有不同快捷键需求,单项控制价值更高)。
 */
async function loadChordActions() {
  const container = document.getElementById("chord-actions-container");
  if (!container) return;

  let actions = [];
  try {
    // list_chord_actions 只返 enabled 的,拿 disabled 列表另外调
    // 但这里我们要展示所有动作（含被禁用的）,直接读所有并交叉比对
    actions = await invoke("list_all_chord_actions");
  } catch (e) {
    console.error("list_all_chord_actions failed:", e);
    return;
  }

  if (!Array.isArray(actions) || actions.length === 0) {
    container.innerHTML = `<div class="action-list-empty">${t("chord.actions.empty")}</div>`;
    return;
  }

  // Chord id → 图标 + 副标题(0.8.7 UX 重排:一眼看懂每个 Chord 做啥)
  const CHORD_META = {
    screenshot: { icon: "🖼", subtitle: t("chord.action.screenshot.subtitle") },
    selection: { icon: "✂️", subtitle: t("chord.action.selection.subtitle") },
    clipboard_history: { icon: "📋", subtitle: t("chord.action.clipboard_history.subtitle") },
  };

  container.innerHTML = actions
    .map((a) => {
      const meta = CHORD_META[a.id] || { icon: "•", subtitle: "" };
      const combo = `Alt + ${a.key.toUpperCase()}`;
      const rowClass = a.enabled ? "" : "is-disabled";
      const subtitleHtml = meta.subtitle
        ? `<div class="action-subtitle">${escapeHtml(meta.subtitle)}</div>`
        : "";
      return `<div class="action-list-row ${rowClass}" data-chord-id="${escapeHtml(a.id)}">
        <div class="action-icon">${meta.icon}</div>
        <div class="action-kbd">${combo}</div>
        <div class="action-info">
          <div class="action-title">${escapeHtml(a.label)}</div>
          ${subtitleHtml}
        </div>
        <label class="switch action-toggle">
          <input type="checkbox" class="chord-action-toggle" data-id="${escapeHtml(a.id)}" ${a.enabled ? "checked" : ""} />
          <span class="slider"></span>
        </label>
      </div>`;
    })
    .join("");

  async function save() {
    const disabled = Array.from(
      container.querySelectorAll(".chord-action-toggle"),
    )
      .filter((el) => !el.checked)
      .map((el) => el.dataset.id);
    try {
      await saveConfig("disabled_chord_actions", disabled);
    } catch (e) {
      console.error("set_disabled_chord_actions failed:", e);
    }
  }

  container.querySelectorAll(".chord-action-toggle").forEach((el) => {
    el.addEventListener("change", (e) => {
      const row = e.target.closest(".action-list-row");
      if (row) row.classList.toggle("is-disabled", !e.target.checked);
      save();
    });
  });
}

/** 弹窗：从运行中的进程里选择敏感应用 */
async function showAddProcessModal(container, existing, onAdd) {
  // 加载运行中进程
  let processes = [];
  try {
    processes = await invoke("list_running_processes");
  } catch (e) {
    console.error("list_running_processes failed:", e);
  }

  // 构建弹窗
  const overlay = document.createElement("div");
  overlay.className = "modal-overlay";
  overlay.innerHTML = `
    <div class="modal">
      <div class="modal-header">
        <h3>${t("context.modal.title")}</h3>
        <button class="modal-close">×</button>
      </div>
      <input type="text" class="modal-search" placeholder="${t("context.modal.search_ph")}" />
      <div class="modal-list"></div>
      <div class="modal-footer">
        <span class="hint">${t("context.modal.hint")}</span>
        <button class="btn-small modal-done">${t("context.modal.done")}</button>
      </div>
    </div>`;
  document.body.appendChild(overlay);

  const modal = overlay.querySelector(".modal");
  const searchInput = overlay.querySelector(".modal-search");
  const listEl = overlay.querySelector(".modal-list");
  const selected = new Set();
  const existingSet = new Set(existing.map((s) => s.toLowerCase()));

  function renderList(filter = "") {
    const flt = filter.toLowerCase();
    const filtered = processes.filter(
      (p) =>
        p.process_name.toLowerCase().includes(flt) ||
        p.window_title.toLowerCase().includes(flt)
    );
    if (filtered.length === 0) {
      listEl.innerHTML = `<div class="modal-empty">${t("context.modal.empty")}</div>`;
      return;
    }
    listEl.innerHTML = filtered
      .map((p) => {
        const isExisting = existingSet.has(p.process_name.toLowerCase());
        const isSelected = selected.has(p.process_name);
        const disabled = isExisting ? "disabled" : "";
        const checked = isSelected ? "checked" : "";
        const label = isExisting ? t("context.modal.added") : "";
        return `<label class="modal-item ${isExisting ? "modal-item-existing" : ""}" data-name="${escapeHtml(p.process_name)}">
          <label class="checkbox">
            <input type="checkbox" ${checked} ${disabled} />
            <span class="checkmark"></span>
          </label>
          <span class="modal-item-name">${escapeHtml(p.process_name)}</span>
          <span class="modal-item-title">${escapeHtml(p.window_title)}</span>
          ${label ? `<span class="modal-item-label">${label}</span>` : ""}
        </label>`;
      })
      .join("");

    // 绑定 checkbox
    listEl.querySelectorAll("input[type=checkbox]").forEach((cb) => {
      cb.addEventListener("change", () => {
        const name = cb.closest(".modal-item").dataset.name;
        if (cb.checked) selected.add(name);
        else selected.delete(name);
      });
    });
  }
  renderList();

  // 搜索过滤
  searchInput.addEventListener("input", () => renderList(searchInput.value));
  searchInput.focus();

  // 关闭
  function close() {
    overlay.remove();
  }
  overlay.querySelector(".modal-close").addEventListener("click", close);
  // mousedown + mouseup 双重命中才判定"点空白",避免从 input 里划词拖出边界误关
  let downOnOverlay = false;
  overlay.addEventListener("mousedown", (e) => {
    downOnOverlay = e.target === overlay;
  });
  overlay.addEventListener("mouseup", (e) => {
    if (downOnOverlay && e.target === overlay) close();
    downOnOverlay = false;
  });
  document.addEventListener(
    "keydown",
    function esc(e) {
      if (e.key === "Escape") {
        close();
        document.removeEventListener("keydown", esc);
      }
    },
    { once: true }
  );

  // 完成 → 回调
  overlay.querySelector(".modal-done").addEventListener("click", () => {
    if (selected.size > 0) onAdd([...selected]);
    close();
  });
}

// 加载并渲染插件列表（0.5.1：只含插件配置，网络已拆到独立 Tab）
async function loadPlugins() {
  const container = document.getElementById("plugins-container");
  if (!container) return;

  let plugins;
  try {
    plugins = await invoke("get_plugins");
  } catch (e) {
    console.error("loadPlugins failed:", e);
    container.innerHTML = `<p class="msg-error" style="padding: 20px;">${t("plugin.load_failed")}</p>`;
    return;
  }

  if (plugins.length === 0) {
    container.innerHTML = `<p class="msg-muted" style="padding: 20px;">${t("plugin.empty")}</p>`;
    return;
  }

  container.innerHTML = plugins.map(renderPluginCard).join("");

  // 绑定每个插件卡片事件（用 data-plugin-id 定位）
  for (const plugin of plugins) {
    bindPluginCardEvents(plugin);
  }

  // 初始化可拖动排序列表
  initSortableLists();
}

// 渲染单个插件卡片（0.5.1：头部总开关 + 触发词标签嵌入描述行 + 配置区分组）
function renderPluginCard(plugin) {
  const icon = PLUGIN_ICONS[plugin.id] || "🔌";
  const desc = plugin.description || t("plugin.desc_default");
  const enabled = plugin.enabled !== false;
  const schema = plugin.settings_schema || [];
  const settings = plugin.settings || {};
  const hasFields = schema.length > 0;

  // 触发关键字标签（直接嵌入头部描述栏）
  const triggersTags = renderTriggersTags(plugin);

  const configSection = hasFields
    ? renderConfigSection(t("plugin.section"), schema, settings, { saveLabel: t("plugin.save"), collapsible: true, collapsed: true })
    : `<div class="plugin-no-config">${t("plugin.no_config")}</div>`;

  const headerRight = `<div class="plugin-master-toggle">
      <label class="switch" title="${t("plugin.toggle.title")}">
        <input type="checkbox" class="plugin-enabled" ${enabled ? "checked" : ""} />
        <span class="slider"></span>
      </label>
    </div>`;

  return renderExtensionCard({
    icon,
    title: `${escapeHtml(plugin.name || plugin.id)}<span class="version-badge">v${escapeHtml(plugin.version || "1.0.0")}</span>${triggersTags}`,
    desc: escapeHtml(desc),
    headerRight,
    attrs: `data-plugin-id="${plugin.id}"`,
    classes: enabled ? "" : "is-disabled",
    body: configSection,
  });
}

// 渲染触发关键字标签行（嵌入描述栏，极简设计）
function renderTriggersTags(plugin) {
  const defaultTriggers = plugin.triggers || [];
  const customTriggers = plugin.custom_triggers || [];
  const disabledDefaults = plugin.disabled_default_triggers || [];
  const hasTriggers = defaultTriggers.length > 0 || customTriggers.length > 0;

  if (!hasTriggers) {
    // 没有触发词的情况：只显示添加按钮
    return `<span class="plugin-triggers-row">
      <span class="trigger-label">${t("plugin.trigger_label")}</span>
      <button class="trigger-add-inline-btn" title="${t("plugin.trigger_add")}">
        ${t("plugin.trigger_add_label")}
      </button>
      <input type="text" class="trigger-add-inline-input" style="display:none;" placeholder="${t("plugin.trigger_placeholder")}" />
    </span>`;
  }

  const triggersHtml = [
    // 标签前缀
    `<span class="trigger-label">${t("plugin.trigger_label")}</span>`,
    // 默认触发词标签
    ...defaultTriggers.map(kw => {
      const isDisabled = disabledDefaults.includes(kw);
      return `<span class="trigger-tag ${isDisabled ? "trigger-tag-disabled" : ""}" data-keyword="${escapeAttr(kw)}" data-type="default">
        <span class="trigger-tag-text">${escapeHtml(kw)}</span>
        <button class="trigger-tag-btn" title="${isDisabled ? t("plugin.trigger_restore") : t("plugin.trigger_disable")}" data-keyword="${escapeAttr(kw)}">
          ${isDisabled ? "↻" : "×"}
        </button>
      </span>`;
    }),
    // 自定义触发词标签
    ...customTriggers.map((trigger, i) => `<span class="trigger-tag trigger-tag-custom ${trigger.enabled ? "" : "trigger-tag-disabled"}" data-keyword="${escapeAttr(trigger.keyword)}" data-idx="${i}">
      <span class="trigger-tag-text">${escapeHtml(trigger.keyword)}</span>
      <button class="trigger-tag-btn trigger-tag-btn-delete" title="${t("plugin.trigger_delete")}" data-keyword="${escapeAttr(trigger.keyword)}" data-idx="${i}">
        ×
      </button>
    </span>`),
    // 添加按钮（小号）
    `<button class="trigger-add-tag-btn" title="${t("plugin.trigger_add")}">+</button>
    <input type="text" class="trigger-add-inline-input" style="display:none;" placeholder="${t("plugin.trigger_placeholder")}" />`
  ].join("");

  return `<span class="plugin-triggers-row">${triggersHtml}</span>`;
}

// 渲染单个配置项控件（boolean→checkbox 方框, enum→下拉, number/string→输入框, sortable_list→可拖动列表）
function renderSettingField(field, value, useSettingRow = false) {
  const val = value !== undefined ? value : field.default;
  let control;
  switch (field.type) {
    case "boolean":
      // 配置项用小号 switch(区别于卡片头部的标准 switch)
      control = `<label class="switch switch-sm"><input type="checkbox" class="plugin-field" data-key="${field.key}" ${val === true ? "checked" : ""} /><span class="slider"></span></label>`;
      break;
    case "enum": {
      const opts = (field.options || [])
        .map((o) => `<option value="${escapeAttr(o.value)}" ${String(val) === String(o.value) ? "selected" : ""}>${escapeHtml(o.label)}</option>`)
        .join("");
      control = `<select class="plugin-field" data-key="${field.key}">${opts}</select>`;
      break;
    }
    case "sortable_list": {
      // 可拖动排序列表
      const items = Array.isArray(val) ? val : (field.default || []);
      const optionsMap = {};
      (field.options || []).forEach(o => { optionsMap[o.value] = o.label; });
      control = renderSortableList(field.key, items, optionsMap);
      break;
    }
    case "number":
      control = `<div class="number-input-wrapper"><input type="number" class="plugin-field" data-key="${field.key}" value="${escapeAttr(val ?? "")}" ${field.min != null ? `min="${field.min}"` : ""} ${field.max != null ? `max="${field.max}"` : ""} /><div class="number-spinner"><button type="button" class="spinner-up" aria-label="${t("spinner.increase")}">▲</button><button type="button" class="spinner-down" aria-label="${t("spinner.decrease")}">▼</button></div></div>`;
      break;
    case "string":
    default:
      control = `<input type="text" class="plugin-field" data-key="${field.key}" value="${escapeAttr(val ?? "")}" />`;
      break;
  }
  // 描述文本：有则显示为标题后的感叹号图标 tooltip
  const descIcon = field.description
    ? `<span class="field-hint-icon" title="${escapeAttr(field.description)}">ⓘ</span>`
    : "";

  // 上下文感知页面使用 setting-row 结构，与设置页其他部分统一
  if (useSettingRow) {
    return `
      <div class="setting-row">
        <label class="setting-label">${escapeHtml(field.title)}${descIcon}</label>
        ${control}
      </div>
    `;
  }

  // 插件配置页面使用 plugin-field-row 结构
  return `
    <div class="plugin-field-row">
      <div class="field-head">
        <span class="field-title">${escapeHtml(field.title)}${descIcon}</span>
        ${control}
      </div>
    </div>
  `;
}

// 渲染可拖动排序列表
function renderSortableList(key, items, optionsMap) {
  const listId = `sortable-${key}`;
  const itemsHtml = items.map((val, idx) => {
    const label = optionsMap[val] || val;
    return `<div class="sortable-item" data-value="${escapeAttr(val)}" draggable="true">
      <span class="sortable-handle">⠿</span>
      <span class="sortable-label">${escapeHtml(label)}</span>
    </div>`;
  }).join("");

  // 隐藏 input 存储值（初始就创建，方便 collectSettings 读取）
  const hiddenValue = JSON.stringify(items);

  return `<div class="sortable-list" id="${listId}" data-key="${key}">
    ${itemsHtml}
  </div>
  <input type="hidden" class="sortable-value plugin-field" data-key="${key}" value="${escapeAttr(hiddenValue)}" />`;
}

// 初始化可拖动列表事件（事件委托，同时支持 HTML5 drag 和鼠标 fallback）
function initSortableLists() {
  if (initSortableLists._bound) return;
  initSortableLists._bound = true;

  // ── HTML5 Drag API ──
  let _dragItem = null;

  document.addEventListener("dragstart", (e) => {
    const item = e.target.closest(".sortable-item");
    if (!item) return;
    _dragItem = item;
    item.classList.add("dragging");
    e.dataTransfer.effectAllowed = "move";
    e.dataTransfer.setData("text/plain", item.dataset.value);
  });

  document.addEventListener("dragend", (e) => {
    const item = e.target.closest(".sortable-item");
    if (!item) return;
    item.classList.remove("dragging");
    document.querySelectorAll(".sortable-item.drag-over").forEach(i => i.classList.remove("drag-over"));
    const list = item.closest(".sortable-list");
    if (list) updateSortableValue(list);
    _dragItem = null;
  });

  document.addEventListener("dragover", (e) => {
    const item = e.target.closest(".sortable-item");
    if (!item || !_dragItem || item === _dragItem) return;
    e.preventDefault();
    e.dataTransfer.dropEffect = "move";
    item.classList.add("drag-over");
  });

  document.addEventListener("dragleave", (e) => {
    const item = e.target.closest(".sortable-item");
    if (item) item.classList.remove("drag-over");
  });

  document.addEventListener("drop", (e) => {
    const item = e.target.closest(".sortable-item");
    if (!item || !_dragItem || item === _dragItem) return;
    e.preventDefault();
    item.classList.remove("drag-over");
    const list = item.closest(".sortable-list");
    if (!list) return;
    const allItems = [...list.querySelectorAll(".sortable-item")];
    const dragIdx = allItems.indexOf(_dragItem);
    const dropIdx = allItems.indexOf(item);
    if (dragIdx < dropIdx) item.after(_dragItem);
    else item.before(_dragItem);
  });

  // ── 鼠标 fallback（WebView2 drag API 有时不触发）──
  let _mouseDrag = null;
  let _mouseClone = null;
  let _mouseStartY = 0;

  document.addEventListener("mousedown", (e) => {
    const handle = e.target.closest(".sortable-handle");
    if (!handle) return;
    const item = handle.closest(".sortable-item");
    if (!item) return;
    e.preventDefault();

    _mouseDrag = item;
    _mouseStartY = e.clientY;
    item.classList.add("dragging");

    // 创建拖拽预览
    _mouseClone = item.cloneNode(true);
    _mouseClone.style.position = "fixed";
    _mouseClone.style.pointerEvents = "none";
    _mouseClone.style.zIndex = "99999";
    _mouseClone.style.width = item.offsetWidth + "px";
    _mouseClone.style.opacity = "0.8";
    _mouseClone.style.boxShadow = "0 4px 12px rgba(0,0,0,0.3)";
    const rect = item.getBoundingClientRect();
    _mouseClone.style.left = rect.left + "px";
    _mouseClone.style.top = rect.top + "px";
    document.body.appendChild(_mouseClone);
  });

  document.addEventListener("mousemove", (e) => {
    if (!_mouseDrag || !_mouseClone) return;
    e.preventDefault();
    const rect = _mouseDrag.getBoundingClientRect();
    _mouseClone.style.top = (rect.top + (e.clientY - _mouseStartY)) + "px";

    // 查找鼠标下方的 sortable-item
    const list = _mouseDrag.closest(".sortable-list");
    if (!list) return;
    const items = [...list.querySelectorAll(".sortable-item")];
    items.forEach(i => i.classList.remove("drag-over"));
    for (const item of items) {
      if (item === _mouseDrag) continue;
      const r = item.getBoundingClientRect();
      if (e.clientY >= r.top && e.clientY <= r.bottom) {
        item.classList.add("drag-over");
        break;
      }
    }
  });

  document.addEventListener("mouseup", (e) => {
    if (!_mouseDrag) return;
    const list = _mouseDrag.closest(".sortable-list");

    // 找到 drop 目标并插入
    if (list) {
      const items = [...list.querySelectorAll(".sortable-item")];
      for (const item of items) {
        if (item === _mouseDrag) continue;
        const r = item.getBoundingClientRect();
        if (e.clientY >= r.top && e.clientY <= r.bottom) {
          const allItems = [...list.querySelectorAll(".sortable-item")];
          const dragIdx = allItems.indexOf(_mouseDrag);
          const dropIdx = allItems.indexOf(item);
          if (dragIdx < dropIdx) item.after(_mouseDrag);
          else item.before(_mouseDrag);
          break;
        }
      }
      items.forEach(i => i.classList.remove("drag-over"));
      updateSortableValue(list);
    }

    _mouseDrag.classList.remove("dragging");
    if (_mouseClone) { _mouseClone.remove(); _mouseClone = null; }
    _mouseDrag = null;
  });
}

// 更新可拖动列表的值到隐藏 input
function updateSortableValue(list) {
  const key = list.dataset.key;
  const values = [...list.querySelectorAll(".sortable-item")].map(i => i.dataset.value);
  // 查找或创建隐藏 input 存储值
  let input = list.parentElement.querySelector(`input.sortable-value[data-key="${key}"]`);
  if (!input) {
    input = document.createElement("input");
    input.type = "hidden";
    input.className = "sortable-value plugin-field";
    input.dataset.key = key;
    list.parentElement.appendChild(input);
  }
  input.value = JSON.stringify(values);
}

// 收集 sortable_list 的值
function collectSortableValue(card, key) {
  const input = card.querySelector(`input.sortable-value[data-key="${key}"]`);
  if (input) {
    try {
      return JSON.parse(input.value);
    } catch (e) {
      console.error("Failed to parse sortable value:", e);
    }
  }
  // fallback: 从 DOM 顺序收集
  const list = card.querySelector(`.sortable-list[data-key="${key}"]`);
  if (list) {
    return [...list.querySelectorAll(".sortable-item")].map(i => i.dataset.value);
  }
  return [];
}

// ── 配置区公用渲染函数 ──────────────────────────────────────────────────────

/** 快捷构建文本字段 schema */
function textField(key, title, opts = {}) {
  return { type: "string", key, title, ...opts };
}

/** 快捷构建布尔字段 schema */
function booleanField(key, title, opts = {}) {
  return { type: "boolean", key, title, ...opts };
}

/**
 * 渲染配置区（section 容器 + 标题 + 字段列表 + 可选保存行）
 * @param {string} title - 分组标题
 * @param {Array} schema - 字段 schema 数组（每项需 type/key/title，可选 description/default/options/min/max）
 * @param {Object} values - 当前值 { key: value }
 * @param {Object} [opts]
 * @param {string} [opts.saveLabel] - 保存按钮文字，省略则不渲染保存行
 * @param {boolean} [opts.collapsible] - 是否可折叠（默认 false）
 * @param {boolean} [opts.collapsed] - 初始是否收起（默认 true，仅 collapsible=true 时生效）
 * @returns {string} HTML
 */
function renderConfigSection(title, schema, values, opts = {}) {
  // 按 group 分组（支持字符串或对象格式）
  const groups = {}; // key -> { title, description, fields }
  const ungrouped = [];
  for (const f of schema) {
    if (f.group) {
      // group 支持字符串或对象 { title, description }
      const groupKey = typeof f.group === "string" ? f.group : f.group.title;
      if (!groups[groupKey]) {
        groups[groupKey] = {
          title: groupKey,
          description: typeof f.group === "object" ? f.group.description : "",
          fields: [],
        };
      }
      groups[groupKey].fields.push(f);
    } else {
      ungrouped.push(f);
    }
  }

  // 渲染无分组字段
  const ungroupedHtml = ungrouped.map((f) => renderSettingField(f, values[f.key])).join("");

  // 渲染各分组（每个分组可独立折叠）
  const groupedHtml = Object.entries(groups).map(([key, group]) => {
    const fieldsHtml = group.fields.map((f) => renderSettingField(f, values[f.key])).join("");
    const descHtml = group.description
      ? `<span class="plugin-group-desc">${linkify(group.description)}</span>`
      : "";
    return `<details class="plugin-group" open>
      <summary class="plugin-group-title">
        <span>${escapeHtml(group.title)}</span>
        ${descHtml}
      </summary>
      <div class="plugin-group-body">${fieldsHtml}</div>
    </details>`;
  }).join("");

  const saveRow = opts.saveLabel
    ? `<div class="plugin-save-row">
         <button class="btn-small plugin-save">${escapeHtml(opts.saveLabel)}</button>
         <span class="plugin-save-msg"></span>
       </div>`
    : "";

  // 扁平模式（非插件页）：不使用 .plugin-config-section 嵌套
  if (opts.flat) {
    return `<div class="plugin-section-title">${escapeHtml(title)}</div>
       ${ungroupedHtml}
       ${groupedHtml}
       ${saveRow}`;
  }

  // 整个配置区可折叠
  if (opts.collapsible) {
    const collapsed = opts.collapsed !== false; // 默认收起
    return `<details class="plugin-config-section" ${collapsed ? "" : "open"}>
       <summary class="plugin-section-title">${escapeHtml(title)}</summary>
       <div class="plugin-config-body">
         ${ungroupedHtml}
         ${groupedHtml}
         ${saveRow}
       </div>
     </details>`;
  }

  return `<div class="plugin-config-section">
     <div class="plugin-section-title">${escapeHtml(title)}</div>
     ${ungroupedHtml}
     ${groupedHtml}
     ${saveRow}
   </div>`;
}

/**
 * 渲染扩展卡片壳（icon + title + desc + 可选 headerRight + body）
 * @param {Object} card
 * @param {string} card.icon - emoji 图标
 * @param {string} card.title - 标题
 * @param {string} card.desc - 描述
 * @param {string} [card.headerRight] - 头部右侧 HTML（如 switch），默认空
 * @param {string} [card.attrs] - 卡片根元素额外属性（如 data-plugin-id），默认空
 * @param {string} [card.classes] - 卡片根元素额外 class，默认空
 * @param {string} card.body - body 区 HTML
 * @returns {string} HTML
 */
function renderExtensionCard({ icon, title, desc, headerRight = "", attrs = "", classes = "", body }) {
  return `<div class="extension-card ${classes}" ${attrs}>
      <div class="extension-header">
        <div class="extension-icon">${icon}</div>
        <div class="extension-info">
          <h3>${title}</h3>
          <p class="extension-desc">${desc}</p>
        </div>
        ${headerRight}
      </div>
      <div class="extension-body">
        ${body}
      </div>
    </div>`;
}

// 绑定单个插件卡片的事件（enabled 开关 + 保存 settings + triggers 配置）
function bindPluginCardEvents(plugin) {
  const card = document.querySelector(`.extension-card[data-plugin-id="${plugin.id}"]`);
  if (!card) return;
  const id = plugin.id;
  const schema = plugin.settings_schema || [];

  const save = async (enabledOverride) => {
    const settings = collectSettings(card, schema);
    const enabled = enabledOverride !== undefined ? enabledOverride : card.querySelector(".plugin-enabled").checked;
    try {
      await saveConfig("plugin_config", { pluginId: id, enabled, settings });
      return true;
    } catch (err) {
      console.error("update_plugin_config failed:", err);
      flash(card, t("common.save_failed_msg", { err }), true);
      return false;
    }
  };

  card.querySelector(".plugin-enabled")?.addEventListener("change", async (e) => {
    const ok = await save(e.target.checked);
    // 0.8.7 UX 统一:自动保存静默成功——toggle 状态本身即反馈,不再 flash"已启用/已禁用"。
    // 失败态由 save() 内部 flash 报错并在这里回滚。
    if (!ok) e.target.checked = !e.target.checked;
  });

  card.querySelector(".plugin-save")?.addEventListener("click", async () => {
    const ok = await save();
    if (ok) {
      clearUnsaved(card);
      flash(card, t("plugin.saved_msg"));
    }
  });

  // ==== 触发关键字相关事件（极简标签设计） ====

  // 默认触发词的 ban/恢复按钮
  card.querySelectorAll(".trigger-tag-btn:not(.trigger-tag-btn-delete)").forEach(btn => {
    btn.addEventListener("click", async (e) => {
      e.stopPropagation();
      const keyword = btn.dataset.keyword;
      const tag = btn.closest(".trigger-tag");
      if (!tag || !keyword) return; // 防御性检查
      const isDisabled = tag.classList.contains("trigger-tag-disabled");

      // 加载态反馈
      const originalContent = btn.innerHTML;
      btn.innerHTML = "⋯";
      btn.disabled = true;

      try {
        await invoke("toggle_default_trigger", {
          pluginId: id,
          keyword,
          disabled: !isDisabled,
        });
        // 更新 UI
        tag.classList.toggle("trigger-tag-disabled", !isDisabled);
        btn.innerHTML = !isDisabled ? "↻" : "×";
        btn.title = !isDisabled ? t("plugin.trigger_restore") : t("plugin.trigger_disable");
      } catch (err) {
        console.error("toggle_default_trigger failed:", err);
        btn.innerHTML = originalContent;
      } finally {
        btn.disabled = false;
      }
    });
  });

  // 自定义触发词的删除按钮
  card.querySelectorAll(".trigger-tag-btn-delete").forEach(btn => {
    btn.addEventListener("click", async (e) => {
      e.stopPropagation();
      const tag = btn.closest(".trigger-tag");
      const keyword = btn.dataset.keyword;
      if (!tag || !keyword) return;

      try {
        await invoke("delete_custom_trigger", { pluginId: id, keyword });
        tag.remove();
      } catch (err) {
        console.error("delete_custom_trigger failed:", err);
      }
    });
  });

  // 内联添加触发词（点击 + 按钮显示输入框）
  const addBtnInline = card.querySelector(".trigger-add-tag-btn");
  const addBtnText = card.querySelector(".trigger-add-inline-btn");
  const addInputInline = card.querySelector(".trigger-add-inline-input");
  const triggersRow = card.querySelector(".plugin-triggers-row");

  // 处理两种添加按钮：标签型 + 文本型
  [addBtnInline, addBtnText].filter(Boolean).forEach(btn => {
    btn.addEventListener("click", () => {
      if (addInputInline) {
        addInputInline.style.display = "inline-block";
        addInputInline.focus();
        btn.style.display = "none";
      }
    });
  });

  // 回车添加
  addInputInline?.addEventListener("keydown", async (e) => {
    if (e.key !== "Enter") return;
    const kw = (e.target.value || "").trim();
    if (!kw) return;

    try {
      await invoke("add_custom_trigger", { pluginId: id, keyword: kw });

      // 插入新标签
      const newTag = document.createElement("span");
      newTag.className = "trigger-tag trigger-tag-custom";
      newTag.innerHTML = `
        <span class="trigger-tag-text">${escapeHtml(kw)}</span>
        <button class="trigger-tag-btn trigger-tag-btn-delete" title="${t("plugin.trigger_delete")}" data-keyword="${escapeAttr(kw)}">
          ×
        </button>
      `;

      // 插入到添加按钮前面
      const addBtn = triggersRow?.querySelector(".trigger-add-tag-btn, .trigger-add-inline-btn");
      if (addBtn && triggersRow) {
        triggersRow.insertBefore(newTag, addBtn);
      } else if (triggersRow) {
        triggersRow.appendChild(newTag);
      }

      // 绑定删除事件
      newTag.querySelector(".trigger-tag-btn-delete")?.addEventListener("click", async (e) => {
        e.stopPropagation();
        try {
          await invoke("delete_custom_trigger", { pluginId: id, keyword: kw });
          newTag.remove();
        } catch (err) {
          console.error("delete_custom_trigger failed:", err);
        }
      });

      // 重置输入框
      e.target.value = "";
      e.target.style.display = "none";
      if (addBtnInline) addBtnInline.style.display = "inline-flex";
      if (addBtnText) addBtnText.style.display = "inline-block";
    } catch (err) {
      console.error("add_custom_trigger failed:", err);
    }
  });

  // 点击其他地方：有内容则保存，无内容则取消
  addInputInline?.addEventListener("blur", async (e) => {
    const kw = (e.target.value || "").trim();
    if (!kw) {
      // 无内容，直接隐藏
      e.target.style.display = "none";
      if (addBtnInline) addBtnInline.style.display = "inline-flex";
      if (addBtnText) addBtnText.style.display = "inline-block";
      return;
    }

    // 有内容，保存
    try {
      await invoke("add_custom_trigger", { pluginId: id, keyword: kw });

      // 插入新标签
      const newTag = document.createElement("span");
      newTag.className = "trigger-tag trigger-tag-custom";
      newTag.innerHTML = `
        <span class="trigger-tag-text">${escapeHtml(kw)}</span>
        <button class="trigger-tag-btn trigger-tag-btn-delete" title="${t("plugin.trigger_delete")}" data-keyword="${escapeAttr(kw)}">
          ×
        </button>
      `;

      // 插入到添加按钮前面
      const addBtn = triggersRow?.querySelector(".trigger-add-tag-btn, .trigger-add-inline-btn");
      if (addBtn && triggersRow) {
        triggersRow.insertBefore(newTag, addBtn);
      } else if (triggersRow) {
        triggersRow.appendChild(newTag);
      }

      // 绑定删除事件
      newTag.querySelector(".trigger-tag-btn-delete")?.addEventListener("click", async (e) => {
        e.stopPropagation();
        try {
          await invoke("delete_custom_trigger", { pluginId: id, keyword: kw });
          newTag.remove();
        } catch (err) {
          console.error("delete_custom_trigger failed:", err);
        }
      });

      // 重置输入框
      e.target.value = "";
      e.target.style.display = "none";
      if (addBtnInline) addBtnInline.style.display = "inline-flex";
      if (addBtnText) addBtnText.style.display = "inline-block";
    } catch (err) {
      console.error("add_custom_trigger failed:", err);
    }
  });

  addInputInline?.addEventListener("keydown", (e) => {
    if (e.key === "Escape") {
      e.target.value = "";
      e.target.blur();
    }
  });
}

// 从卡片控件收集 settings 对象（按 schema type 转换类型）
function collectSettings(card, schema) {
  const settings = {};
  for (const f of schema) {
    if (f.type === "sortable_list") {
      // sortable_list 从隐藏 input 或 DOM 顺序收集
      settings[f.key] = collectSortableValue(card, f.key);
      continue;
    }
    const el = card.querySelector(`.plugin-field[data-key="${f.key}"]`);
    if (!el) continue;
    switch (f.type) {
      case "boolean":
        settings[f.key] = el.checked;
        break;
      case "number":
        settings[f.key] = el.value === "" ? 0 : Number(el.value);
        break;
      default: // string / enum
        settings[f.key] = el.value;
    }
  }
  return settings;
}

// HTML 转义（防 settings/title 注入 HTML）
function escapeHtml(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}
function escapeAttr(s) {
  return escapeHtml(s);
}

// 把文本中的 URL 转换成可点击链接（使用 Tauri invoke 在外部浏览器打开）
function linkify(text) {
  const escaped = escapeHtml(text);
  return escaped.replace(
    /(https?:\/\/[^\s<>"']+)/g,
    '<a href="#" class="external-link" onclick="openExternalUrl(\'$1\'); return false;">$1</a>'
  );
}

// 在外部浏览器打开 URL
async function openExternalUrl(url) {
  try {
    await invoke("open_url", { url });
  } catch (e) {
    console.error("openExternalUrl failed:", e);
    // fallback
    window.open(url, "_blank");
  }
}

// 卡片内显示一行反馈（2s 后清除）
function flash(card, msg, isError) {
  const el = card.querySelector(".plugin-save-msg");
  if (!el) return;
  el.textContent = msg;
  el.className = `plugin-save-msg ${isError ? "msg-error" : "msg-success"}`;
  clearTimeout(el._t);
  el._t = setTimeout(() => { el.textContent = ""; el.className = "plugin-save-msg"; }, 2000);
}

// ── 待保存提示 ─────────────────────────────────────────────────────────────────


/** 标记字段为"待保存"（在 field-title 内部末尾追加红色 badge） */
function markUnsaved(fieldEl) {
  const head = fieldEl.closest(".field-head");
  if (!head) return;
  const title = head.querySelector(".field-title");
  if (!title) return;
  // 已有就跳过
  if (title.querySelector(".unsaved-badge")) return;
  const badge = document.createElement("span");
  badge.className = "unsaved-badge";
  badge.textContent = t("plugin.unsaved");
  title.appendChild(badge);
}

/** 清除卡片内所有待保存 badge */
function clearUnsaved(container) {
  container.querySelectorAll(".unsaved-badge").forEach((el) => el.remove());
}

// 事件委托：plugin-field 变化时标记待保存（input 实时，change 最终）
// 例外：容器带 [data-autosave] 的自动保存卡片跳过——它们 change 即 save，无需 badge。
document.addEventListener("input", (e) => {
  const el = e.target.closest(".plugin-field");
  if (!el || el.type === "checkbox") return;
  if (el.closest("[data-autosave]")) return;
  markUnsaved(el);
});
document.addEventListener("change", (e) => {
  const el = e.target.closest(".plugin-field");
  if (!el) return;
  if (el.closest("[data-autosave]")) return;
  markUnsaved(el);
});

// 文件搜索引擎的输入框（不是 plugin-field，单独处理）
document.addEventListener("input", (e) => {
  const id = e.target.id;
  if (["everything-port", "everything-max-results", "start-menu-scan-depth"].includes(id)) {
    const row = e.target.closest(".setting-row");
    if (row) {
      const label = row.querySelector("label");
      if (label && !label.querySelector(".unsaved-badge")) {
        const badge = document.createElement("span");
        badge.className = "unsaved-badge";
        badge.textContent = t("plugin.unsaved");
        label.appendChild(badge);
      }
    }
  }
});

// 数据源选择变化时标记未保存
document.getElementById("file-search-data-source")?.addEventListener("change", (e) => {
  const row = e.target.closest(".setting-row");
  if (row) {
    const label = row.querySelector("label");
    if (label && !label.querySelector(".unsaved-badge")) {
      const badge = document.createElement("span");
      badge.className = "unsaved-badge";
      badge.textContent = t("plugin.unsaved");
      label.appendChild(badge);
    }
  }
});

// 应用搜索——扫描深度 spinner / UWP 开关变化时标记未保存
// （spinner 的 click handler dispatch change 事件而非 input，原 input 监听覆盖不到）
["start-menu-scan-depth", "start-menu-include-uwp"].forEach((id) => {
  document.getElementById(id)?.addEventListener("change", (e) => {
    const row = e.target.closest(".setting-row");
    if (row) {
      const label = row.querySelector("label");
      if (label && !label.querySelector(".unsaved-badge")) {
        const badge = document.createElement("span");
        badge.className = "unsaved-badge";
        badge.textContent = t("plugin.unsaved");
        label.appendChild(badge);
      }
    }
  });
});

// 应用搜索开关变化时标记未保存
document.getElementById("start-menu-enabled")?.addEventListener("change", () => {
  const card = document.getElementById("save-start-menu")?.closest(".extension-card");
  if (!card) return;
  const desc = card.querySelector(".extension-desc");
  if (desc && !desc.nextElementSibling?.classList?.contains("unsaved-badge")) {
    const badge = document.createElement("span");
    badge.className = "unsaved-badge";
    badge.textContent = t("plugin.unsaved");
    desc.after(badge);
  }
});

// 文件搜索总开关（在卡片头部，单独处理）
document.getElementById("file-search-enabled")?.addEventListener("change", () => {
  const card = document.getElementById("save-file-search")?.closest(".extension-card");
  if (!card) return;
  // 在扩展描述后显示 badge（如果还没有）
  const desc = card.querySelector(".extension-desc");
  if (desc && !desc.nextElementSibling?.classList?.contains("unsaved-badge")) {
    const badge = document.createElement("span");
    badge.className = "unsaved-badge";
    badge.textContent = t("plugin.unsaved");
    desc.after(badge);
  }
});

// ── 快捷键录制（使用后端 rdev）─────────────────────────────────────────────────

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
    const defaultHotkey = { modifiers: [], key: "ralt", display: "RightAlt" };
    await saveConfig("hotkey", {
      modifiers: defaultHotkey.modifiers,
      key: defaultHotkey.key,
      display: defaultHotkey.display,
    });
    renderHotkeyInto(hotkeyRecordBtn, "RightAlt");
    if (currentConfig) currentConfig.hotkey = defaultHotkey;
  });
}

async function startRecording() {
  hotkeyRecordBtn.disabled = true;
  hotkeyResetBtn.disabled = true;
  hotkeyRecordBtn.classList.add("recording");
  // 录制态：直接纯文字覆写（.recording class 会切成 mono 字体 + 脉动色）
  hotkeyRecordBtn.textContent = t("hotkey.recording");

  // 录制期间吞掉所有键盘事件的默认行为：Alt / Alt+Space / F10 等会激活宿主窗口
  // 系统菜单、冻结 WebView2 消息泵（导致按钮卡住、需点鼠标才恢复）。录制由后端
  // ll_proc 处理，前端不需要任何键盘输入，全量拦截是安全的。
  const suppress = (e) => e.preventDefault();
  document.addEventListener("keydown", suppress, true);

  try {
    // 调用后端录制
    const result = await invoke("record_hotkey");
    console.log("[startRecording] record_hotkey resolved:", JSON.stringify(result));

    // 保存录制结果
    await saveConfig("hotkey", {
      modifiers: result.modifiers,
      key: result.key,
      display: result.display,
    });

    // 更新显示（键帽 DOM）
    renderHotkeyInto(hotkeyRecordBtn, result.display);

    // 更新本地配置缓存
    if (currentConfig) {
      currentConfig.hotkey = {
        modifiers: result.modifiers,
        key: result.key,
        display: result.display,
      };
    }
  } catch (e) {
    console.error("[startRecording] failed:", e);
    // 恢复原显示（键帽 DOM）
    renderHotkeyInto(hotkeyRecordBtn, currentConfig?.hotkey?.display || "RightAlt");
  } finally {
    document.removeEventListener("keydown", suppress, true);
    hotkeyRecordBtn.classList.remove("recording");
    hotkeyRecordBtn.disabled = false;
    hotkeyResetBtn.disabled = false;
  }
}

// ── 滑块配置 ─────────────────────────────────────────────────────────────────

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
      if (currentConfig) currentConfig.tap_threshold = value;
    } catch (err) {
      console.error("update_tap_threshold failed:", err);
    }
  });
}

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
      if (currentConfig) currentConfig.grace_period = value;
    } catch (err) {
      console.error("update_grace_period failed:", err);
    }
  });
}

// ── 通用设置 ─────────────────────────────────────────────────────────────────

const autoStartCheckbox = document.getElementById("auto-start");

if (autoStartCheckbox) {
  autoStartCheckbox.addEventListener("change", async (e) => {
    try {
      await saveConfig("auto_start", e.target.checked);
      if (currentConfig) currentConfig.auto_start = e.target.checked;
    } catch (err) {
      console.error("update_auto_start failed:", err);
    }
  });
}

const languageSelect = document.getElementById("language");

if (languageSelect) {
  languageSelect.addEventListener("change", async (e) => {
    const lang = e.target.value;
    try {
      await saveConfig("language", lang);
      if (currentConfig) currentConfig.language = lang;
      // 即时切换整页语言：静态文本 + 动态渲染区 + 计量/徽章
      setLang(lang);
      applyI18n();
      loadNetworkConfig();
      loadContextConfig();
      loadContextBindings();
      loadPlugins();
      loadStorageInfo();
      refreshEverythingBadgeText();
      refreshInterpreterBadgeText("python");
      refreshInterpreterBadgeText("node");
    } catch (err) {
      console.error("update_language failed:", err);
    }
  });
}

// ── 通用配置（主题 / 搜索历史 / 结果数，聚合 update_general_config）──────────

// 读取当前通用字段（合并 DOM 现值），保证聚合更新不丢字段
function readGeneral() {
  const val = (id, fb) => (document.getElementById(id)?.value ?? fb);
  const checked = (id, fb) => (document.getElementById(id)?.checked ?? fb);
  return {
    theme: val("theme", "auto"),
    searchHistoryEnabled: checked("search-history-enabled", true),
    searchHistoryDays: parseInt(val("search-history-days", "30"), 10) || 0,
    maxResults: parseInt(val("max-results", "50"), 10) || 50,
    pageSize: parseInt(val("page-size", "9"), 10) || 9,
  };
}

const themeSelect = document.getElementById("theme");
if (themeSelect) {
  themeSelect.addEventListener("change", async (e) => {
    const mode = e.target.value;
    applyTheme(mode); // 即时预览
    try {
      const g = readGeneral();
      await saveConfig("general_config", g);
      if (currentConfig) currentConfig.theme = mode;
    } catch (err) {
      console.error("update_general_config (theme) failed:", err);
    }
  });

  // 滚轮切换主题：鼠标悬停时滚轮可快速预览主题
  themeSelect.addEventListener("wheel", (e) => {
    e.preventDefault(); // 阻止页面滚动
    const options = themeSelect.options;
    const currentIndex = themeSelect.selectedIndex;
    let newIndex;

    if (e.deltaY > 0) {
      // 向下滚动：下一个主题（到末尾停止）
      newIndex = Math.min(currentIndex + 1, options.length - 1);
    } else {
      // 向上滚动：上一个主题（到开头停止）
      newIndex = Math.max(currentIndex - 1, 0);
    }

    if (newIndex === currentIndex) return; // 已到边界，不触发 change
    themeSelect.selectedIndex = newIndex;
    // 触发 change 事件以应用主题
    themeSelect.dispatchEvent(new Event("change"));
  });
}

const shEnabledCheckbox = document.getElementById("search-history-enabled");
if (shEnabledCheckbox) {
  shEnabledCheckbox.addEventListener("change", async (e) => {
    try {
      const g = readGeneral();
      await saveConfig("general_config", g);
      if (currentConfig) currentConfig.search_history_enabled = e.target.checked;
    } catch (err) {
      console.error("update_general_config (history enabled) failed:", err);
    }
  });
}

const shDaysInput = document.getElementById("search-history-days");
if (shDaysInput) {
  shDaysInput.addEventListener("change", async () => {
    try {
      const g = readGeneral();
      await saveConfig("general_config", g);
      if (currentConfig) currentConfig.search_history_days = g.searchHistoryDays;
    } catch (err) {
      console.error("update_general_config (history days) failed:", err);
    }
  });
}

const maxResultsInput = document.getElementById("max-results");
if (maxResultsInput) {
  maxResultsInput.addEventListener("change", async () => {
    try {
      const g = readGeneral();
      await saveConfig("general_config", g);
      if (currentConfig) currentConfig.max_results = g.maxResults;
    } catch (err) {
      console.error("update_general_config (max results) failed:", err);
    }
  });
}

const pageSizeInput = document.getElementById("page-size");
if (pageSizeInput) {
  pageSizeInput.addEventListener("change", async () => {
    try {
      const g = readGeneral();
      await saveConfig("general_config", g);
      if (currentConfig) currentConfig.page_size = g.pageSize;
    } catch (err) {
      console.error("update_general_config (page size) failed:", err);
    }
  });
}

// ── Autosuggestion（0.8.1 §2.8）─────────────────────────────────────────────

async function saveAutosuggest() {
  const enabled = document.getElementById("autosuggest-enabled")?.checked !== false;
  const scoreRaw = document.getElementById("autosuggest-min-score")?.value ?? "0.7";
  const minScore = Math.min(0.95, Math.max(0.5, parseFloat(scoreRaw) || 0.7));
  const tabKey = document.getElementById("autosuggest-tab-key")?.value || "Tab";
  try {
    await saveConfig("autosuggest", { enabled, minScore, tabKey });
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

// ── Chord 总控 + 剪贴板开关（0.8.5 §6.6）──────────────────────────────────

async function saveChordToggles() {
  // checkbox.checked 是纯 boolean; 用 === true 精确读取(0.8.7:chord 默认关,不再兜底 true)
  const chordEnabled = document.getElementById("chord-enabled")?.checked === true;
  const chordHintVisible = document.getElementById("chord-hint-visible")?.checked === true;
  try {
    await saveConfig("chord_toggles", { chordEnabled, chordHintVisible });
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
// #clipboard-enabled 已随 0.8.7 UX 重排迁至上下文卡的动态渲染 body,绑定在 loadContextConfig 内完成


// ── 日志 ─────────────────────────────────────────────────────────────────────

const logLevelSelect = document.getElementById("log-level");
if (logLevelSelect) {
  logLevelSelect.addEventListener("change", async (e) => {
    try {
      await saveConfig("log_level", e.target.value);
      if (currentConfig) currentConfig.log_level = e.target.value;
    } catch (err) {
      console.error("update_log_level failed:", err);
    }
  });
}

document.getElementById("open-log-file")?.addEventListener("click", async () => {
  try {
    await invoke("open_log_file");
  } catch (e) {
    console.error("open_log_file failed:", e);
  }
});

document.getElementById("open-log-dir")?.addEventListener("click", async () => {
  try {
    await invoke("open_log_dir");
  } catch (e) {
    console.error("open_log_dir failed:", e);
  }
});

async function loadLogInfo() {
  try {
    const info = await invoke("get_log_info");
    const el = document.getElementById("log-file-path");
    if (el) el.textContent = info.current_file || "-";
  } catch (e) {
    console.error("loadLogInfo failed:", e);
  }
}

// ── 存储面板 ─────────────────────────────────────────────────────────────────

async function loadStorageInfo() {
  try {
    const info = await invoke("get_storage_info");
    document.getElementById("history-count").textContent = t("storage.history_count", { count: info.history_count });
    document.getElementById("db-path").textContent = info.db_path;
  } catch (e) {
    console.error("loadStorageInfo failed:", e);
  }
}

document.getElementById("clear-history")?.addEventListener("click", async () => {
  const ok = await confirmDialog(t("storage.clear.confirm"), {
    title: t("common.confirm"),
    kind: "warning",
  });
  if (!ok) return;
  await invoke("clear_history");
  loadStorageInfo();
});

// ── 扩展 Tab：搜索引擎配置 ──────────────────────────────────────────────────

// 应用搜索配置保存
document.getElementById("save-start-menu")?.addEventListener("click", async () => {
  const enabled = document.getElementById("start-menu-enabled").checked;
  const scanDepth = parseInt(document.getElementById("start-menu-scan-depth").value, 10) || 3;
  const includeUwp = document.getElementById("start-menu-include-uwp")?.checked ?? true;

  try {
    await saveConfig("start_menu_config", { enabled, scanDepth, includeUwp });
    const msgEl = document.getElementById("start-menu-save-msg");
    if (msgEl) {
      msgEl.textContent = t("plugin.saved_msg");
      msgEl.className = "plugin-save-msg msg-success";
      setTimeout(() => { msgEl.textContent = ""; msgEl.className = "plugin-save-msg"; }, 2000);
    }
    const card = document.getElementById("save-start-menu")?.closest(".extension-card");
    if (card) clearUnsaved(card);
  } catch (e) {
    console.error("update_start_menu_config failed:", e);
    messageDialog(t("common.save_failed_msg", { err: e }), { title: t("common.error"), kind: "error" });
  }
});

// 计算器配置保存（开关变化即时保存）
document.getElementById("calc-enabled")?.addEventListener("change", async (e) => {
  try {
    await saveConfig("calc_config", { enabled: e.target.checked });
  } catch (err) {
    console.error("update_calc_config failed:", err);
    e.target.checked = !e.target.checked; // 回滚
  }
});

// 数据源选择变化时显示/隐藏 Everything 配置
document.getElementById("file-search-data-source")?.addEventListener("change", (e) => {
  updateEverythingConfigVisibility(e.target.value);
});

async function probeEverythingStatus() {
  const statusEl = document.getElementById("everything-status");
  const portInput = document.getElementById("everything-port");
  const port = parseInt(portInput?.value || "80", 10);

  statusEl.textContent = t("engine.status.probing");
  statusEl.className = "status-badge status-unknown";
  statusEl.dataset.badgeState = "probing";

  try {
    const available = await invoke("probe_everything", { port });
    if (available) {
      statusEl.textContent = t("engine.status.available");
      statusEl.className = "status-badge status-available";
      statusEl.dataset.badgeState = "available";
    } else {
      statusEl.textContent = t("engine.status.unavailable");
      statusEl.className = "status-badge status-unavailable";
      statusEl.dataset.badgeState = "unavailable";
    }
  } catch (e) {
    statusEl.textContent = t("engine.status.failed");
    statusEl.className = "status-badge status-unavailable";
    statusEl.dataset.badgeState = "failed";
    console.error("probe_everything failed:", e);
  }
}

// 语言切换时按当前探测状态重写徽章文本（不重新发起探测请求）
function refreshEverythingBadgeText() {
  const statusEl = document.getElementById("everything-status");
  if (!statusEl) return;
  const key =
    statusEl.dataset.badgeState === "available" ? "engine.status.available" :
    statusEl.dataset.badgeState === "unavailable" ? "engine.status.unavailable" :
    statusEl.dataset.badgeState === "failed" ? "engine.status.failed" :
    "engine.status.probing";
  statusEl.textContent = t(key);
}

// 语言切换时按当前探测状态重写脚本解释器徽章文本（不重新发起探测请求）
function refreshInterpreterBadgeText(type) {
  const statusEl = document.getElementById(`${type}-status`);
  if (!statusEl) return;
  const key =
    statusEl.dataset.badgeState === "available" ? "engine.status.available" :
    statusEl.dataset.badgeState === "version_low" ? "engine.status.version_low" :
    statusEl.dataset.badgeState === "not_found" ? "engine.status.not_found" :
    statusEl.dataset.badgeState === "failed" ? "engine.status.failed" :
    "engine.status.probing";
  statusEl.textContent = t(key);
}


document.getElementById("probe-everything")?.addEventListener("click", probeEverythingStatus);

document.getElementById("save-file-search")?.addEventListener("click", async () => {
  console.log("保存文件搜索配置 - 开始");
  const enabled = document.getElementById("file-search-enabled").checked;
  const dataSource = document.getElementById("file-search-data-source").value;
  const port = parseInt(document.getElementById("everything-port").value, 10);
  const maxResults = parseInt(document.getElementById("everything-max-results").value, 10) || 20;

  // 端口范围校验（u16 最大值 65535）
  if (port < 1 || port > 65535) {
    const msgEl = document.getElementById("file-search-save-msg");
    if (msgEl) {
      msgEl.textContent = t("error.port_range");
      msgEl.className = "plugin-save-msg msg-error";
      setTimeout(() => { msgEl.textContent = ""; msgEl.className = "plugin-save-msg"; }, 3000);
    }
    return;
  }

  console.log("保存文件搜索配置 - 参数:", { enabled, dataSource, everythingPort: port, maxResults });

  try {
    await saveConfig("file_search", {
      enabled,
      dataSource,
      everythingPort: port,
      maxResults,
    });
    console.log("保存文件搜索配置 - 成功");
    // 跟插件保存一致的 flash 提示样式
    const msgEl = document.getElementById("file-search-save-msg");
    if (msgEl) {
      msgEl.textContent = t("plugin.saved_msg");
      msgEl.className = "plugin-save-msg msg-success";
      setTimeout(() => { msgEl.textContent = ""; msgEl.className = "plugin-save-msg"; }, 2000);
    }
    // 清除待保存提示
    const fileSearchCard = document.getElementById("save-file-search")?.closest(".extension-card");
    if (fileSearchCard) clearUnsaved(fileSearchCard);
    // 重新探测（非 local 模式）
    if (dataSource !== "local") {
      probeEverythingStatus();
    }
  } catch (e) {
    console.error("update_file_search failed:", e);
    messageDialog(t("common.save_failed_msg", { err: e }), { title: t("common.error"), kind: "error" });
  }
});

// ── 初始化 ───────────────────────────────────────────────────────────────────

loadConfig();
loadStorageInfo();
loadLogInfo();
loadNetworkConfig();
loadContextConfig();
loadContextBindings();
loadChordActions();
loadPerfStats();
loadAboutInfo();

// 关于面板：应用元信息从后端 Cargo.toml 编译期注入的字段读取
async function loadAboutInfo() {
  try {
    const info = await invoke("get_app_info");
    const versionEl = document.getElementById("about-version");
    if (versionEl) versionEl.textContent = info.version || "—";
    const licenseEl = document.getElementById("about-license");
    if (licenseEl) licenseEl.textContent = info.license || "—";
    const repoEl = document.getElementById("about-repository");
    if (repoEl) {
      const url = info.repository || "";
      if (url) {
        repoEl.textContent = url;
        repoEl.href = url;
        repoEl.addEventListener("click", (e) => {
          e.preventDefault();
          openExternalUrl(url);
        });
      } else {
        repoEl.textContent = "—";
        repoEl.removeAttribute("href");
      }
    }
  } catch (e) {
    console.error("loadAboutInfo failed:", e);
  }

  // 检查更新按钮
  const checkBtn = document.getElementById("about-check-update");
  if (checkBtn) {
    checkBtn.addEventListener("click", () => checkForUpdate(checkBtn));
  }
}

async function checkForUpdate(btn) {
  const el = document.getElementById("about-update");
  if (!el) return;

  // 检查中
  btn.disabled = true;
  btn.textContent = t("about.update.checking");
  el.hidden = true;

  try {
    const result = await invoke("check_update");
    if (result.has_update) {
      el.hidden = false;
      el.innerHTML = `${t("about.update.available", { version: result.latest_version })} · <a href="#" class="about-update-link" data-external>${t("about.update.download")}</a>`;
      el.querySelector(".about-update-link").addEventListener("click", (e) => {
        e.preventDefault();
        openExternalUrl(result.release_url);
      });
      btn.textContent = t("about.update.check");
    } else {
      el.hidden = false;
      el.textContent = t("about.update.latest");
      btn.textContent = t("about.update.check");
    }
  } catch (e) {
    console.error("checkForUpdate failed:", e);
    el.hidden = false;
    el.textContent = t("about.update.failed");
  } finally {
    btn.disabled = false;
  }
}

// ── 性能统计（调试 Tab）──────────────────────────────────────────────────────

// 加载性能统计数据
async function loadPerfStats() {
  try {
    const overview = await invoke("get_perf_overview");
    renderPerfStats(overview);
  } catch (e) {
    console.error("loadPerfStats failed:", e);
    showPerfError();
  }
}

// 渲染性能统计
function renderPerfStats(overview) {
  renderPercentileCard("perf-startup-total", overview.startup);
  renderPercentileCard("perf-hotkey-show", overview.hotkey);
  renderPercentileCard("perf-search-total", overview.search);

  // 采样数：取三项中最大的 count
  const countEl = document.getElementById("perf-total-count");
  if (countEl) {
    const counts = [overview.startup, overview.hotkey, overview.search]
      .filter(d => d && d.count > 0)
      .map(d => d.count);
    countEl.textContent = counts.length > 0 ? Math.max(...counts) : t("debug.perf.no_data");
    countEl.className = counts.length > 0 ? "debug-value" : "debug-value no-data";
  }

  // 慢查询日志（合并 hotkey + search）
  renderSlowQueries([...(overview.slow_hotkey || []), ...(overview.slow_search || [])]);
}

// 渲染单个百分位数卡片
function renderPercentileCard(elementId, data) {
  const el = document.getElementById(elementId);
  if (!el) return;

  if (!data || data.count === 0) {
    el.textContent = t("debug.perf.no_data");
    el.className = "debug-value no-data";
    return;
  }

  // 显示 P50 作为主值，P90/P99 作为提示
  const p50 = data.p50 || "-";
  el.textContent = `${p50} ${t("debug.perf.unit.ms")}`;
  el.className = "debug-value";
  el.title = [
    `${t("debug.perf.stats.count")}: ${data.count}`,
    `${t("debug.perf.stats.p50")}: ${data.p50} ms`,
    `${t("debug.perf.stats.p90")}: ${data.p90} ms`,
    `${t("debug.perf.stats.p99")}: ${data.p99} ms`,
    `${t("debug.perf.stats.min")}: ${data.min} ms`,
    `${t("debug.perf.stats.max")}: ${data.max} ms`,
    `${t("debug.perf.stats.avg")}: ${data.avg} ms`,
  ].join("\n");
}

// 渲染慢查询日志（合并列表）
function renderSlowQueries(slowItems) {
  const el = document.getElementById("perf-slow-list");
  if (!el) return;

  if (!slowItems || slowItems.length === 0) {
    el.innerHTML = `<div class="perf-slow-empty">${t("debug.perf.slow.empty")}</div>`;
    return;
  }

  // 按耗时降序
  slowItems.sort((a, b) => b.value_ms - a.value_ms);

  el.innerHTML = slowItems.map(m => `
    <div class="perf-slow-item">
      <span class="perf-slow-cat">${escapeHtml(m.category)}</span>
      <span class="perf-slow-name">${escapeHtml(m.name)}</span>
      <span class="perf-slow-time">${m.value_ms.toFixed(1)} ms</span>
      <span class="perf-slow-meta">${escapeHtml(m.metadata || "")}</span>
    </div>
  `).join("");
}

// 显示性能统计错误状态
function showPerfError() {
  ["perf-startup-total", "perf-hotkey-show", "perf-search-total", "perf-total-count"].forEach(id => {
    const el = document.getElementById(id);
    if (el) {
      el.textContent = "-";
      el.className = "debug-value error";
    }
  });
}

// 刷新按钮
document.getElementById("perf-refresh")?.addEventListener("click", () => {
  loadPerfStats();
});

// 导出报告按钮
document.getElementById("perf-export")?.addEventListener("click", async () => {
  try {
    const path = await invoke("export_perf_report");
    if (!path) {
      return; // 用户取消了
    }

    // 显示提示（带保存路径）
    const btn = document.getElementById("perf-export");
    if (btn) {
      const original = btn.textContent;
      btn.textContent = t("debug.perf.exported");
      setTimeout(() => { btn.textContent = original; }, 2000);
    }

    console.log("性能报告已保存到:", path);
  } catch (e) {
    console.error("export_perf_report failed:", e);
  }
});

// 清除记录按钮
document.getElementById("perf-clear")?.addEventListener("click", async () => {
  const ok = await confirmDialog(t("debug.perf.clear.confirm"), {
    title: t("common.confirm"),
    kind: "warning",
  });
  if (!ok) return;
  try {
    await invoke("clear_perf_data");
    loadPerfStats();
    const btn = document.getElementById("perf-clear");
    if (btn) {
      const original = btn.textContent;
      btn.textContent = t("debug.perf.cleared");
      setTimeout(() => { btn.textContent = original; }, 2000);
    }
  } catch (e) {
    console.error("clear_perf_data failed:", e);
  }
});

// ── 脚本解释器探测（Phase 0.6） ─────────────────────────────────────────────

function updateInterpreterUI(type, status) {
  const statusEl = document.getElementById(`${type}-status`);
  const pathEl = document.getElementById(`${type}-path`);
  const browseBtn = document.getElementById(`${type}-browse`);

  if (!statusEl) return;

  if (status.found) {
    if (status.version_ok) {
      // 合并显示：版本号 + 可用状态
      const versionText = status.version ? `${status.version} ` : "";
      statusEl.textContent = `${versionText}${t("engine.status.available")}`;
      statusEl.className = "status-badge status-available";
      statusEl.dataset.badgeState = "available";
    } else {
      // 版本过低也合并显示
      const versionText = status.version ? `${status.version} ` : "";
      statusEl.textContent = `${versionText}${t("engine.status.version_low")}`;
      statusEl.className = "status-badge status-warning";
      statusEl.dataset.badgeState = "version_low";
    }
    pathEl.value = status.path || "";
  } else {
    statusEl.textContent = t("engine.status.not_found");
    statusEl.className = "status-badge status-unavailable";
    statusEl.dataset.badgeState = "not_found";
    pathEl.value = status.error || t("engine.status.not_found");
  }

}

// 打开文件选择器选择解释器路径
async function browseInterpreter(kind) {
  try {
    const selected = await invoke("open_file_dialog", {
      title: t(`file_dialog.${kind}_title`),
      filters: [
        {
          name: t("file_dialog.exe_filter"),
          extensions: ["exe"]
        }
      ]
    });
    if (selected) {
      const pathEl = document.getElementById(`${kind}-path`);
      if (pathEl) pathEl.value = selected;
      // TODO: 验证选中的文件版本
    }
  } catch (e) {
    console.error("browseInterpreter failed:", e);
  }
}

// 探测单个解释器（跟文件搜索对齐，一次只探测一种）
async function probeSingleInterpreter(type) {
  const statusEl = document.getElementById(`${type}-status`);
  if (!statusEl) return;

  // 显示探测中状态（只更新对应解释器）
  statusEl.textContent = t("engine.status.probing");
  statusEl.className = "status-badge status-unknown";
  statusEl.dataset.badgeState = "probing";

  try {
    const status = await invoke("probe_interpreters");
    updateInterpreterUI(type, status[type]);
  } catch (e) {
    console.error(`probeInterpreter ${type} failed:`, e);
    statusEl.textContent = t("engine.status.failed");
    statusEl.className = "status-badge status-unavailable";
    statusEl.dataset.badgeState = "failed";
  }
}

// 探测全部解释器（只在首次启动两个都为空时自动调用）
async function probeAllInterpreters() {
  const pythonPath = document.getElementById("python-path")?.value;
  const nodePath = document.getElementById("node-path")?.value;

  // 已有值就不自动探测了（避免覆盖用户手动配置）
  if (pythonPath && nodePath) return;

  // 显示探测中状态
  ["python", "node"].forEach((type) => {
    const statusEl = document.getElementById(`${type}-status`);
    if (statusEl) {
      statusEl.textContent = t("engine.status.probing");
      statusEl.className = "status-badge status-unknown";
      statusEl.dataset.badgeState = "probing";
    }
  });

  try {
    const status = await invoke("probe_interpreters");
    updateInterpreterUI("python", status.python);
    updateInterpreterUI("node", status.node);
  } catch (e) {
    console.error("probeInterpreters failed:", e);
  }
}

// 绑定事件
document.getElementById("python-probe")?.addEventListener("click", () => probeSingleInterpreter("python"));
document.getElementById("node-probe")?.addEventListener("click", () => probeSingleInterpreter("node"));
document.getElementById("python-browse")?.addEventListener("click", () => browseInterpreter("python"));
document.getElementById("node-browse")?.addEventListener("click", () => browseInterpreter("node"));

// 首次启动：两个路径都为空时才自动探测全部
// 避免用户手动选择后又被覆盖
setTimeout(() => {
  const pythonPath = document.getElementById("python-path")?.value;
  const nodePath = document.getElementById("node-path")?.value;
  if (!pythonPath && !nodePath) {
    probeAllInterpreters();
  }
}, 100);

// ── 数字输入框增减按钮（事件委托，支持动态生成的插件数字输入）──────────────

document.addEventListener("click", (e) => {
  const btn = e.target.closest(".number-spinner button");
  if (!btn) return;

  const wrapper = btn.closest(".number-input-wrapper");
  const input = wrapper?.querySelector("input[type='number']");
  if (!input) return;

  const min = parseFloat(input.min);
  const max = parseFloat(input.max);
  const step = parseFloat(input.step) || 1;
  let value = parseFloat(input.value) || 0;

  // 确定小数位数（基于 step），避免浮点精度问题
  const stepStr = input.step || "1";
  const decimals = stepStr.includes(".") ? stepStr.split(".")[1].length : 0;

  if (btn.classList.contains("spinner-up")) {
    value = Math.min(value + step, isNaN(max) ? Infinity : max);
  } else {
    value = Math.max(value - step, isNaN(min) ? -Infinity : min);
  }

  // 固定小数位数，避免 0.7000000000000001 这样的问题
  input.value = decimals > 0 ? value.toFixed(decimals) : value;
  // 触发 change 事件，让绑定的事件处理函数生效
  input.dispatchEvent(new Event("change", { bubbles: true }));
});

// ── AI Tab（0.9.1 Phase 6）───────────────────────────────────────────────────
//
// 数据流：
//   loadAIConfig() 拉后端 → 渲染 UI + 记 currentAIConfig
//   用户改字段 → 写 currentAIConfig → saveAIConfig() → invoke set_config('ai_config')
//   添加供应商 → modal → save_ai_secret(先写 CM)→ 更新 currentAIConfig.providers →
//     saveAIConfig() → 弹 toast 询问总开关（§5.3 严格 opt-in）
//   删除供应商 → 确认 → delete_ai_secret（幂等）→ 移除 provider entry →
//     若引用了 tier 则同时清 tier → saveAIConfig()
//
// **不发密钥回前端**：has_ai_secret 只返 bool，用户想改 Key 必须"清空重填"（§5.2）

let currentAIConfig = null;
let hasSecretMap = new Map(); // provider_id → boolean

const AI_KIND_LABEL = {
  openai_compatible: "OpenAI Compatible",
  anthropic_messages: "Anthropic",
  gemini_generate_content: "Gemini",
};

async function loadAIConfig() {
  // 读第 7 分片 —— 老用户拿到 default（enabled=false, providers=[]）
  try {
    const cfg = await invoke("get_config_section", { key: "app.ai" });
    // null → 空配置（首次运行；后端返 Value::Null）
    currentAIConfig = cfg && typeof cfg === "object" ? cfg : defaultAIConfig();
  } catch (e) {
    console.error("get_config_section app.ai failed:", e);
    currentAIConfig = defaultAIConfig();
  }
  // 密钥存在性并行查询
  hasSecretMap = new Map();
  const providers = currentAIConfig.providers || [];
  await Promise.all(
    providers.map(async (p) => {
      try {
        const has = await invoke("has_ai_secret", { providerId: p.id });
        hasSecretMap.set(p.id, !!has);
      } catch {
        hasSecretMap.set(p.id, false);
      }
    })
  );
  applyAIConfigToUI();
  bindAIEvents();
}

function defaultAIConfig() {
  return {
    enabled: false,
    allow_intent_routing: false,
    min_query_len: 4,
    require_whitespace: true,
    exclude_pure_numeric: true,
    respect_awareness_url_path: true,
    providers: [],
    tier_router: null,
    tier_light: null,
    tier_main: null,
    direct_execute_safe_actions: false,
    slo_hard_timeout_ms: null,
  };
}

function applyAIConfigToUI() {
  const c = currentAIConfig;
  const $ = (id) => document.getElementById(id);
  $("ai-enabled").checked = !!c.enabled;
  $("ai-allow-routing").checked = !!c.allow_intent_routing;
  $("ai-min-query-len").value = c.min_query_len ?? 4;
  $("ai-require-whitespace").checked = c.require_whitespace !== false;
  $("ai-exclude-pure-numeric").checked = c.exclude_pure_numeric !== false;
  $("ai-respect-awareness-url-path").checked = c.respect_awareness_url_path !== false;
  $("ai-direct-safe").checked = !!c.direct_execute_safe_actions;
  $("ai-timeout-ms").value = c.slo_hard_timeout_ms ?? 2500;

  renderAIProviders();
  renderAITierSelects();
  renderAITierBanner();
}

function renderAIProviders() {
  const container = document.getElementById("ai-providers-container");
  const providers = currentAIConfig.providers || [];
  if (providers.length === 0) {
    container.innerHTML =
      `<div class="ai-providers-empty">${escapeHtml(t("ai.providers.empty"))}</div>
       <button class="ai-providers-add" id="ai-add-provider">${escapeHtml(t("ai.providers.add"))}</button>`;
    document.getElementById("ai-add-provider").addEventListener("click", openAIProviderModal);
    return;
  }
  const cards = providers
    .map((p) => {
      const kindLabel = AI_KIND_LABEL[p.kind] || p.kind;
      const models = p.models || [];
      const modelSummary = models.map((m) => m.id).join(", ");
      const hasKey = hasSecretMap.get(p.id) === true;
      const statusCls = hasKey ? "" : "no-key";
      const statusText = hasKey ? t("ai.provider.configured") : t("ai.provider.not_configured");
      // 0.9.4:tint monogram 方块——按 kind + base_url 反查 preset,fallback ink 色
      const presetKey = guessPresetForProvider(p.kind, p.base_url);
      const preset = AI_PRESET_CATALOG[presetKey] || AI_PRESET_CATALOG.custom;
      const monogram = preset.monogram || "?";
      const isCJK = /[一-鿿]/.test(monogram);
      const monoCjkCls = isCJK ? " ai-provider-mono--cjk" : "";
      // 0.9.4:可编辑模型表格(展开时显示)——colgroup 固定列宽,长 id 溢出走 ellipsis
      const modelsTable = models.length > 0
        ? `<table class="ai-models-table">
            <colgroup>
              <col class="col-id" />
              <col class="col-name" />
              <col class="col-enabled" />
              <col class="col-actions" />
            </colgroup>
            <thead><tr><th>Model ID</th><th>显示名</th><th>启用</th><th>操作</th></tr></thead>
            <tbody>${models.map((m) => {
              const enabled = m.enabled !== false;
              // 0.9.4 Step 1:有 temperature / max_tokens / custom_parameters 的模型显示 · 参数徽章
              const hasParams = m.temperature != null || m.max_tokens != null || (m.custom_parameters && m.custom_parameters.length > 0);
              const paramsBadge = hasParams
                ? '<span class="ai-model-params-badge" title="已配置调用参数">·参数</span>'
                : '';
              return `<tr data-model-id="${escapeAttr(m.id)}" data-provider-id="${escapeAttr(p.id)}">
                <td class="ai-models-table-id" title="${escapeAttr(m.id)}">${escapeHtml(m.id)}${paramsBadge}</td>
                <td title="${escapeAttr(m.display_name || m.id)}">${escapeHtml(m.display_name || m.id)}</td>
                <td>
                  <label class="switch switch-sm">
                    <input type="checkbox" class="ai-model-toggle" data-model-id="${escapeAttr(m.id)}" data-provider-id="${escapeAttr(p.id)}" ${enabled ? "checked" : ""} />
                    <span class="slider"></span>
                  </label>
                </td>
                <td class="ai-models-table-actions">
                  <button class="ai-model-edit-btn" data-model-id="${escapeAttr(m.id)}" data-provider-id="${escapeAttr(p.id)}" title="${escapeAttr(t("ai.model.edit"))}">✎</button>
                  <button class="ai-model-delete" data-model-id="${escapeAttr(m.id)}" data-provider-id="${escapeAttr(p.id)}" title="${escapeAttr(t("ai.model.delete"))}">✕</button>
                </td>
              </tr>`;
            }).join("")}</tbody>
          </table>
          <button class="ai-model-add-inline" data-provider-id="${escapeAttr(p.id)}">${escapeHtml(t("ai.model.add"))}</button>`
        : `<div class="ai-models-table-empty">${escapeHtml(t("ai.model.empty"))}</div>
           <button class="ai-model-add-inline" data-provider-id="${escapeAttr(p.id)}">${escapeHtml(t("ai.model.add"))}</button>`;
      return `
        <div class="ai-provider-card" data-provider-id="${escapeAttr(p.id)}">
          <div class="ai-provider-header" data-provider-id="${escapeAttr(p.id)}">
            <span class="ai-provider-mono${monoCjkCls}" data-tint="${preset.tint}">${escapeHtml(monogram)}</span>
            <div class="ai-provider-info">
              <div class="ai-provider-title">${escapeHtml(p.display_name)}</div>
              <div class="ai-provider-meta">${escapeHtml(kindLabel)} · ${escapeHtml(modelSummary || "(no model)")}</div>
            </div>
            <span class="ai-provider-status ${statusCls}">${escapeHtml(statusText)}</span>
            <span class="ai-provider-chevron">▸</span>
            <button class="ai-provider-edit" data-provider-id="${escapeAttr(p.id)}" title="${escapeAttr(t("ai.provider.edit"))}">✎</button>
            <button class="ai-provider-delete" data-provider-id="${escapeAttr(p.id)}" title="${escapeAttr(t("ai.provider.delete"))}">✕</button>
          </div>
          <div class="ai-provider-models" style="display:none;">${modelsTable}</div>
        </div>`;
    })
    .join("");
  container.innerHTML =
    cards + `<button class="ai-providers-add" id="ai-add-provider">${escapeHtml(t("ai.providers.add"))}</button>`;

  document.getElementById("ai-add-provider").addEventListener("click", openAIProviderModal);
  // 0.9.4:accordion 展开/折叠
  container.querySelectorAll(".ai-provider-header").forEach((header) => {
    header.addEventListener("click", (e) => {
      if (e.target.closest(".ai-provider-edit") || e.target.closest(".ai-provider-delete")) return;
      const card = header.closest(".ai-provider-card");
      const modelsDiv = card.querySelector(".ai-provider-models");
      const chevron = header.querySelector(".ai-provider-chevron");
      const isOpen = modelsDiv.style.display !== "none";
      modelsDiv.style.display = isOpen ? "none" : "";
      chevron.textContent = isOpen ? "▸" : "▾";
    });
  });
  container.querySelectorAll(".ai-provider-edit").forEach((btn) => {
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      openAIProviderModal(btn.dataset.providerId);
    });
  });
  container.querySelectorAll(".ai-provider-delete").forEach((btn) => {
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      deleteAIProvider(btn.dataset.providerId);
    });
  });
  // 0.9.4:模型启用/禁用开关
  container.querySelectorAll(".ai-model-toggle").forEach((toggle) => {
    toggle.addEventListener("change", (e) => {
      e.stopPropagation();
      const { providerId, modelId } = toggle.dataset;
      toggleModelEnabled(providerId, modelId, toggle.checked);
    });
  });
  // 0.9.4:模型删除按钮
  container.querySelectorAll(".ai-model-delete").forEach((btn) => {
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      const { providerId, modelId } = btn.dataset;
      deleteModelFromProvider(providerId, modelId);
    });
  });
  // 0.9.4 Step 1:模型编辑(打开独立模型 modal)
  container.querySelectorAll(".ai-model-edit-btn").forEach((btn) => {
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      const { providerId, modelId } = btn.dataset;
      openAIModelEditModal(providerId, modelId);
    });
  });
  // 0.9.4 Step 1:表格底部添加模型(新增模式)
  container.querySelectorAll(".ai-model-add-inline").forEach((btn) => {
    btn.addEventListener("click", (e) => {
      e.stopPropagation();
      openAIModelEditModal(btn.dataset.providerId, null);
    });
  });
}

/** 切换模型启用状态(0.9.4)。 */
function toggleModelEnabled(providerId, modelId, enabled) {
  const providers = currentAIConfig.providers || [];
  const provider = providers.find((p) => p.id === providerId);
  if (!provider) return;
  const model = (provider.models || []).find((m) => m.id === modelId);
  if (!model) return;
  model.enabled = enabled;
  saveAIConfig()
    .then(() => {
      // 0.9.4:禁用可能让某个 tier 变悬空,tier select 里也要同步 disabled 状态 + banner 重新算
      renderAITierSelects();
      renderAITierBanner();
    })
    .catch((e) => console.error("[ai] toggle model enabled failed:", e));
}

/**
 * 保存 provider 后引导用户添加模型(0.9.4)。
 *
 * **触发场景**:只有"新增 provider"时调,编辑时不动(避免打扰)。
 *
 * **行为**:
 * 1. 展开对应卡片的 accordion(models 区显现)
 * 2. 高亮"+ 添加模型"按钮,3 秒后淡出
 * 3. `scrollIntoView` 让按钮进入视口
 *
 * **失败静默**:找不到 DOM(还没渲染完)时 setTimeout 一次;仍失败就放弃。
 */
function guideAddModelForProvider(providerId) {
  const tryGuide = (retries) => {
    const card = document.querySelector(`.ai-provider-card[data-provider-id="${CSS.escape(providerId)}"]`);
    if (!card) {
      if (retries > 0) setTimeout(() => tryGuide(retries - 1), 80);
      return;
    }
    // 展开 accordion
    const modelsDiv = card.querySelector(".ai-provider-models");
    const chevron = card.querySelector(".ai-provider-chevron");
    if (modelsDiv && modelsDiv.style.display === "none") {
      modelsDiv.style.display = "";
      if (chevron) chevron.textContent = "▾";
    }
    // 高亮"+ 添加模型"按钮
    const addBtn = card.querySelector(".ai-model-add-inline");
    if (addBtn) {
      addBtn.classList.add("ai-model-add-inline--pulse");
      addBtn.scrollIntoView({ behavior: "smooth", block: "center" });
      // 3s 后收回高亮(与 CSS animation 时长一致)
      setTimeout(() => addBtn.classList.remove("ai-model-add-inline--pulse"), 3000);
    }
  };
  // 首帧未必渲染完,重试最多 3 次
  setTimeout(() => tryGuide(3), 20);
}

/** 从 provider 中删除单个模型(0.9.4)。始终 confirm,tier 引用时提示更严重后果。 */
async function deleteModelFromProvider(providerId, modelId) {
  // 检查是否被 tier 引用——决定 confirm 提示的严重程度
  const refs = [];
  ["router", "light", "main"].forEach((tier) => {
    const a = currentAIConfig[`tier_${tier}`];
    if (a && a.provider_id === providerId && a.model_id === modelId) refs.push(t(`ai.tier.${tier}`));
  });
  // 无论有无 tier 引用,删除都需要 confirm——避免误点 ✕ 秒删
  const msg = refs.length > 0
    ? t("ai.model.delete.referenced", { model: modelId, tiers: refs.join("、") })
    : t("ai.model.delete.confirm", { model: modelId });
  const ok = await confirmDialog(msg, {
    title: t("ai.model.delete"),
    kind: refs.length > 0 ? "warning" : "info",
  });
  if (!ok) return;

  const providers = currentAIConfig.providers || [];
  const provider = providers.find((p) => p.id === providerId);
  if (!provider) return;
  provider.models = (provider.models || []).filter((m) => m.id !== modelId);
  // 清理悬空 tier 引用
  ["router", "light", "main"].forEach((tier) => {
    const a = currentAIConfig[`tier_${tier}`];
    if (a && a.provider_id === providerId && a.model_id === modelId) {
      currentAIConfig[`tier_${tier}`] = null;
    }
  });
  saveAIConfig()
    .then(() => {
      renderAIProviders();
      renderAITierSelects();
      renderAITierBanner();
    })
    .catch((e) => console.error("[ai] delete model failed:", e));
}

// ── 模型编辑 modal(0.9.4 Step 1)────────────────────────────────────────
//
// **两种模式**:
// - 新增(modelId=null):表格底部 `+ 添加模型` 触发,允许编辑 Model ID
// - 编辑(modelId=非空):行 ✎ 触发,Model ID 只读(动它意味着删旧建新,场景复杂,留 Step 2)
//
// **数据流**:modal 内改动落到 _modelEditDraft 草稿;点保存 → 落回
// currentAIConfig.providers[i].models[j] → saveAIConfig() → set_config('ai_config')
// → registry.reload(热切换,复用 fingerprint 不变的实例)。
//
// **参数覆盖开关联动**:temperature/max_tokens 各有一个 toggle;关闭时对应
// slider/input disabled,保存时字段落成 null。开启时用 body 里最新值。

/** modal 当前编辑的 provider + model 引用(草稿) */
let _modelEditProviderId = null;
let _modelEditOriginalId = null; // 编辑模式下原 model id(判定改名)
let _modelEditDraft = null;      // { id, display_name, enabled, temperature, max_tokens, custom_parameters }

/** 打开模型编辑 modal(0.9.4 Step 1)。modelId 为 null 时是"新增"模式。 */
function openAIModelEditModal(providerId, modelId) {
  const provider = (currentAIConfig.providers || []).find((p) => p.id === providerId);
  if (!provider) return;
  const isEdit = modelId != null;
  const existing = isEdit ? (provider.models || []).find((m) => m.id === modelId) : null;
  if (isEdit && !existing) return; // model 被并发删除,静默不动

  _modelEditProviderId = providerId;
  _modelEditOriginalId = isEdit ? existing.id : null;
  _modelEditDraft = {
    id: existing?.id ?? "",
    display_name: existing?.display_name ?? "",
    enabled: existing?.enabled !== false,
    temperature: existing?.temperature ?? null,
    max_tokens: existing?.max_tokens ?? null,
    custom_parameters: (existing?.custom_parameters || []).map((cp) => ({ ...cp })),
  };

  const $ = (id) => document.getElementById(id);
  // 标题 + 副标题
  $("ai-model-edit-title").textContent = t(isEdit ? "ai.model_modal.title.edit" : "ai.model_modal.title.add");
  const info = $("ai-model-edit-provider-info");
  info.textContent = t("ai.model_modal.provider_info", { name: provider.display_name });

  // 基础字段
  const idInput = $("ai-model-edit-id");
  idInput.value = _modelEditDraft.id;
  idInput.readOnly = isEdit; // 编辑模式下 ID 不可改;新增模式可自由输入
  idInput.classList.toggle("input-readonly", isEdit);
  $("ai-model-edit-display-name").value = _modelEditDraft.display_name;

  // 拉取按钮:新增模式显示,编辑模式隐藏(ID 只读没必要拉)
  const fetchBtn = $("ai-model-edit-fetch");
  if (fetchBtn) fetchBtn.style.display = isEdit ? "none" : "";
  // popover 每次打开时清空隐藏
  closeModelFetchPopover();
  _modelFetchCache = null; // 换 provider 后作废缓存

  // 参数(覆盖开关联动)
  setupModelParamRow("temperature", _modelEditDraft.temperature, 0.7);
  setupModelParamRow("max-tokens", _modelEditDraft.max_tokens, 4096);

  // 自定义参数列表
  renderCustomParams();

  // 错误行清空
  $("ai-model-edit-error").textContent = "";
  // 显示 overlay
  const overlay = $("ai-model-edit-overlay");
  overlay.style.display = "flex";
  setTimeout(() => idInput.focus(), 40);
}

/** 参数行(temperature / max-tokens)通用设置:同步覆盖 toggle 与两个联动 input。 */
function setupModelParamRow(key, currentValue, fallbackValue) {
  const $ = (id) => document.getElementById(id);
  const toggle = $(`ai-model-edit-${key}-toggle`);
  const body = $(`ai-model-param-${key}-body`);
  const range = $(`ai-model-edit-${key}-range`);
  const num = $(`ai-model-edit-${key}-num`);
  const enabled = currentValue != null;
  toggle.checked = enabled;
  body.style.display = enabled ? "" : "none";
  const shown = enabled ? currentValue : fallbackValue;
  range.value = shown;
  num.value = shown;
}

/** 渲染自定义参数键值对列表。 */
function renderCustomParams() {
  const container = document.getElementById("ai-model-edit-custom-params");
  if (!container) return;
  const params = _modelEditDraft.custom_parameters || [];
  if (params.length === 0) {
    container.innerHTML = `<div class="ai-model-custom-params-empty">${escapeHtml(t("ai.model_modal.custom_params.empty"))}</div>`;
    return;
  }
  container.innerHTML = params.map((p, idx) => {
    // value 用 JSON.stringify 展示原始类型(数字/布尔/字符串引号);用户编辑时输入 raw 文本
    const valDisplay = p.value == null
      ? ""
      : (typeof p.value === "string" ? p.value : JSON.stringify(p.value));
    return `<div class="ai-model-custom-param-row" data-idx="${idx}">
      <input type="text" class="ai-model-custom-param-key" data-idx="${idx}" value="${escapeAttr(p.key || "")}" placeholder="${escapeAttr(t("ai.model_modal.custom_params.key.ph"))}" />
      <input type="text" class="ai-model-custom-param-val" data-idx="${idx}" value="${escapeAttr(valDisplay)}" placeholder="${escapeAttr(t("ai.model_modal.custom_params.val.ph"))}" />
      <button type="button" class="ai-model-custom-param-del" data-idx="${idx}" title="${escapeAttr(t("common.delete"))}">✕</button>
    </div>`;
  }).join("");
  // 绑定输入 → 落草稿
  container.querySelectorAll(".ai-model-custom-param-key").forEach((el) => {
    el.addEventListener("input", () => {
      const i = Number(el.dataset.idx);
      _modelEditDraft.custom_parameters[i].key = el.value;
    });
  });
  container.querySelectorAll(".ai-model-custom-param-val").forEach((el) => {
    el.addEventListener("input", () => {
      const i = Number(el.dataset.idx);
      _modelEditDraft.custom_parameters[i].value = coerceCustomParamValue(el.value);
    });
  });
  container.querySelectorAll(".ai-model-custom-param-del").forEach((btn) => {
    btn.addEventListener("click", () => {
      const i = Number(btn.dataset.idx);
      _modelEditDraft.custom_parameters.splice(i, 1);
      renderCustomParams();
    });
  });
}

/**
 * value 自动推断类型:数字 / 布尔 / JSON / 字符串。
 * - "true" / "false" → bool
 * - "" → 空字符串保留(用户可能就想传空)
 * - 纯数字 → number
 * - `{...}` / `[...]` / `"..."` 能解析为 JSON → JSON 值
 * - 其它 → string(原样)
 */
function coerceCustomParamValue(raw) {
  if (typeof raw !== "string") return raw;
  const trimmed = raw.trim();
  if (trimmed === "") return "";
  if (trimmed === "true") return true;
  if (trimmed === "false") return false;
  if (trimmed === "null") return null;
  // 数字(整数或小数,允许负号)
  if (/^-?\d+(\.\d+)?$/.test(trimmed)) {
    const n = Number(trimmed);
    if (Number.isFinite(n)) return n;
  }
  // JSON 对象 / 数组 / 引号字符串
  if (/^[[{"]/.test(trimmed)) {
    try {
      return JSON.parse(trimmed);
    } catch {
      // JSON 解析失败,回落字符串
    }
  }
  return raw;
}

/** 关闭模型编辑 modal。 */
function closeAIModelEditModal() {
  const overlay = document.getElementById("ai-model-edit-overlay");
  if (overlay) overlay.style.display = "none";
  closeModelFetchPopover();
  _modelEditProviderId = null;
  _modelEditOriginalId = null;
  _modelEditDraft = null;
  _modelFetchCache = null;
}

// ── 拉取模型 popover(0.9.4 Step 1 增强)─────────────────────────────
//
// 当前 provider 的 API 拉一次模型列表,让用户点选替代手打。
// 只在新增模式(ID 输入可写)显示;编辑模式隐藏按钮。
//
// **缓存**:一次 provider 打开期内只拉一次,close 时清。避免用户开关 popover 反复请求。

/** 打开一次期间的 provider 模型缓存;{ models, error, loading } 或 null。 */
let _modelFetchCache = null;

/** 点击 🔍 触发——首次拉取,后续切换开关。 */
async function toggleModelFetchPopover() {
  const popover = document.getElementById("ai-model-edit-fetch-popover");
  if (!popover) return;
  const isOpen = popover.style.display !== "none";
  if (isOpen) {
    closeModelFetchPopover();
    return;
  }
  popover.style.display = "";
  // 首次打开 → 拉取
  if (!_modelFetchCache) {
    await performModelFetch();
  }
  renderModelFetchList("");
  // 聚焦搜索框
  const filter = document.getElementById("ai-model-edit-fetch-filter");
  if (filter) { filter.value = ""; setTimeout(() => filter.focus(), 30); }
}

/** 关闭 popover 但保留缓存(下次打开秒开)。 */
function closeModelFetchPopover() {
  const popover = document.getElementById("ai-model-edit-fetch-popover");
  if (popover) popover.style.display = "none";
}

/** 执行拉取——从当前 modal 关联的 provider 抓 model 列表。 */
async function performModelFetch() {
  const providerId = _modelEditProviderId;
  const provider = (currentAIConfig.providers || []).find((p) => p.id === providerId);
  if (!provider) {
    _modelFetchCache = { models: [], error: t("ai.model_modal.err.provider_gone"), loading: false };
    return;
  }
  _modelFetchCache = { models: [], error: null, loading: true };
  renderModelFetchList("");
  try {
    const models = await fetchAvailableModelsFor(provider.kind, provider.base_url, providerId);
    _modelFetchCache = { models: models || [], error: null, loading: false };
  } catch (e) {
    _modelFetchCache = { models: [], error: String(e.message || e), loading: false };
  }
}

/** 渲染 popover 列表——按 filter 过滤;已存在的 model 灰化"已添加"。 */
function renderModelFetchList(filter) {
  const list = document.getElementById("ai-model-edit-fetch-list");
  if (!list) return;
  const cache = _modelFetchCache;
  if (!cache || cache.loading) {
    list.innerHTML = `<div class="ai-model-dropdown-empty"><span class="ai-spinner"></span> ${escapeHtml(t("ai.model_modal.fetch.loading"))}</div>`;
    return;
  }
  if (cache.error) {
    list.innerHTML = `<div class="ai-model-dropdown-empty">${escapeHtml(t("ai.model_modal.fetch.failed", { err: cache.error }))}</div>`;
    return;
  }
  const provider = (currentAIConfig.providers || []).find((p) => p.id === _modelEditProviderId);
  const existingIds = new Set((provider?.models || []).map((m) => m.id));
  const q = (filter || "").trim().toLowerCase();
  const filtered = cache.models
    .filter((m) => (q ? m.toLowerCase().includes(q) : true))
    .slice(0, 100);
  if (filtered.length === 0) {
    const msg = cache.models.length === 0
      ? t("ai.model_modal.fetch.empty")
      : t("ai.model_modal.fetch.no_match");
    list.innerHTML = `<div class="ai-model-dropdown-empty">${escapeHtml(msg)}</div>`;
    return;
  }
  list.innerHTML = filtered.map((m) => {
    const isExisting = existingIds.has(m);
    return `<div class="ai-model-dropdown-item${isExisting ? " is-added" : ""}" data-model-id="${escapeAttr(m)}">
      <span>${escapeHtml(m)}</span>
      ${isExisting ? `<span class="added-tag">${escapeHtml(t("ai.modal.badge.added"))}</span>` : ""}
    </div>`;
  }).join("");
  list.querySelectorAll(".ai-model-dropdown-item:not(.is-added)").forEach((item) => {
    item.addEventListener("click", () => {
      const id = item.dataset.modelId;
      // 填入 Model ID 输入框,若显示名为空则自动填一份
      const idInput = document.getElementById("ai-model-edit-id");
      const dispInput = document.getElementById("ai-model-edit-display-name");
      if (idInput) idInput.value = id;
      if (dispInput && !dispInput.value.trim()) dispInput.value = id;
      closeModelFetchPopover();
      if (idInput) idInput.focus();
    });
  });
}

/**
 * 保存模型编辑——校验 + 落回 currentAIConfig + saveAIConfig。
 *
 * **校验**:
 * - Model ID 非空
 * - 新增模式下 ID 不能与本 provider 下已有 model 重复
 * - custom_parameters 里 key 空的自动忽略(不报错)
 */
async function saveModelEdit() {
  const $ = (id) => document.getElementById(id);
  const errorEl = $("ai-model-edit-error");
  errorEl.textContent = "";

  const providerId = _modelEditProviderId;
  const provider = (currentAIConfig.providers || []).find((p) => p.id === providerId);
  if (!provider) {
    errorEl.textContent = t("ai.model_modal.err.provider_gone");
    return;
  }
  const isEdit = _modelEditOriginalId != null;

  // 读表单
  const id = $("ai-model-edit-id").value.trim();
  const displayName = $("ai-model-edit-display-name").value.trim();
  const tempToggle = $("ai-model-edit-temperature-toggle").checked;
  const tempVal = Number($("ai-model-edit-temperature-num").value);
  const maxToggle = $("ai-model-edit-max-tokens-toggle").checked;
  const maxVal = Number($("ai-model-edit-max-tokens-num").value);

  // 校验
  if (!id) {
    errorEl.textContent = t("ai.model_modal.err.empty_id");
    return;
  }
  if (!isEdit) {
    if ((provider.models || []).some((m) => m.id === id)) {
      errorEl.textContent = t("ai.model_modal.err.duplicate_id");
      return;
    }
  }
  if (tempToggle && (!Number.isFinite(tempVal) || tempVal < 0 || tempVal > 2)) {
    errorEl.textContent = t("ai.model_modal.err.temperature_range");
    return;
  }
  if (maxToggle && (!Number.isFinite(maxVal) || maxVal < 1)) {
    errorEl.textContent = t("ai.model_modal.err.max_tokens_range");
    return;
  }

  // 清洗 custom_parameters:去掉空 key 行
  const cleanedCustom = (_modelEditDraft.custom_parameters || []).filter((cp) => (cp.key || "").trim().length > 0);

  // 落回 provider.models
  const newModel = {
    id,
    display_name: displayName || id,
    enabled: isEdit ? _modelEditDraft.enabled : true,
    context_window: null,
    input_price_per_million: null,
    output_price_per_million: null,
    temperature: tempToggle ? tempVal : null,
    max_tokens: maxToggle ? Math.floor(maxVal) : null,
    custom_parameters: cleanedCustom,
  };

  if (isEdit) {
    const idx = (provider.models || []).findIndex((m) => m.id === _modelEditOriginalId);
    if (idx < 0) {
      errorEl.textContent = t("ai.model_modal.err.model_gone");
      return;
    }
    // 保留 enabled / context_window / 价格字段(未来 Step 2 若加)
    const old = provider.models[idx];
    newModel.enabled = old.enabled !== false;
    newModel.context_window = old.context_window ?? null;
    newModel.input_price_per_million = old.input_price_per_million ?? null;
    newModel.output_price_per_million = old.output_price_per_million ?? null;
    provider.models[idx] = newModel;
  } else {
    provider.models = provider.models || [];
    provider.models.push(newModel);
  }

  try {
    await saveAIConfig();
  } catch (e) {
    errorEl.textContent = t("ai.error.save_failed", { err: String(e) });
    return;
  }
  closeAIModelEditModal();
  renderAIProviders();
  renderAITierSelects();
  renderAITierBanner();
}

/** 模型 modal 事件绑定——由 init 阶段调用一次。 */
function bindAIModelEditModalEvents() {
  const $ = (id) => document.getElementById(id);
  const overlay = $("ai-model-edit-overlay");
  if (!overlay) return;

  // 参数覆盖 toggle 联动 body 显隐 + slider 与 number 双向同步
  ["temperature", "max-tokens"].forEach((key) => {
    const toggle = $(`ai-model-edit-${key}-toggle`);
    const body = $(`ai-model-param-${key}-body`);
    const range = $(`ai-model-edit-${key}-range`);
    const num = $(`ai-model-edit-${key}-num`);
    if (!toggle || !body || !range || !num) return;
    toggle.addEventListener("change", () => {
      body.style.display = toggle.checked ? "" : "none";
    });
    range.addEventListener("input", () => { num.value = range.value; });
    num.addEventListener("input", () => {
      const v = Number(num.value);
      if (Number.isFinite(v)) range.value = v;
    });
  });

  // 添加自定义参数
  $("ai-model-edit-custom-params-add").addEventListener("click", () => {
    if (!_modelEditDraft) return;
    _modelEditDraft.custom_parameters.push({ key: "", value: "" });
    renderCustomParams();
    // 焦点到新增行的 key input
    setTimeout(() => {
      const rows = document.querySelectorAll(".ai-model-custom-param-key");
      const last = rows[rows.length - 1];
      if (last) last.focus();
    }, 20);
  });

  // 取消 / 保存
  $("ai-model-edit-cancel").addEventListener("click", closeAIModelEditModal);
  $("ai-model-edit-save").addEventListener("click", saveModelEdit);

  // 0.9.4 Step 1:拉取模型 popover
  const fetchBtn = $("ai-model-edit-fetch");
  if (fetchBtn) {
    fetchBtn.addEventListener("click", (e) => {
      e.stopPropagation();
      toggleModelFetchPopover();
    });
  }
  const fetchClose = $("ai-model-edit-fetch-close");
  if (fetchClose) fetchClose.addEventListener("click", closeModelFetchPopover);
  const fetchFilter = $("ai-model-edit-fetch-filter");
  if (fetchFilter) {
    fetchFilter.addEventListener("input", () => renderModelFetchList(fetchFilter.value));
    fetchFilter.addEventListener("keydown", (e) => {
      if (e.key === "Escape") closeModelFetchPopover();
    });
  }
  // 点 popover 外关闭(限本 modal 内;overlay 不算外)
  overlay.addEventListener("click", (e) => {
    const popover = $("ai-model-edit-fetch-popover");
    if (!popover || popover.style.display === "none") return;
    if (e.target.closest("#ai-model-edit-fetch-popover")) return;
    if (e.target.id === "ai-model-edit-fetch") return; // 让 toggle 自己处理
    closeModelFetchPopover();
  });

  // 点空白关闭(mousedown/mouseup 都落在 overlay 上,与 provider modal 一致)
  let downOnOverlay = false;
  overlay.addEventListener("mousedown", (e) => {
    downOnOverlay = e.target.id === "ai-model-edit-overlay";
  });
  overlay.addEventListener("mouseup", (e) => {
    if (downOnOverlay && e.target.id === "ai-model-edit-overlay") closeAIModelEditModal();
    downOnOverlay = false;
  });

  // ESC 关闭
  document.addEventListener("keydown", (e) => {
    if (e.key === "Escape" && overlay.style.display !== "none") {
      closeAIModelEditModal();
    }
  });
}

function renderAITierSelects() {
  const providers = currentAIConfig.providers || [];
  const options = [`<option value="">${escapeHtml(t("ai.tier.unassigned"))}</option>`];
  providers.forEach((p) => {
    (p.models || []).forEach((m) => {
      const val = `${p.id}::${m.id}`;
      // 0.9.4:禁用的 model 显示但 disabled,并挂"(已禁用)"后缀——避免误选
      const isEnabled = m.enabled !== false;
      const label = isEnabled
        ? `${p.display_name} / ${m.id}`
        : `${p.display_name} / ${m.id} ${t("ai.tier.model_disabled")}`;
      const disabledAttr = isEnabled ? "" : " disabled";
      options.push(`<option value="${escapeAttr(val)}"${disabledAttr}>${escapeHtml(label)}</option>`);
    });
  });
  const html = options.join("");
  ["router", "light", "main"].forEach((tier) => {
    const sel = document.getElementById(`ai-tier-${tier}`);
    sel.innerHTML = html;
    const assign = currentAIConfig[`tier_${tier}`];
    sel.value = assign ? `${assign.provider_id}::${assign.model_id}` : "";
  });
  renderAITierDegrade();
}

function renderAITierDegrade() {
  const cfg = currentAIConfig;
  // 简单版：如果 tier_x 空，展示"→ 降级到 xxx"提示
  // Router 空 → 降到 Light 或 Main；Light 空 → 降到 Main；Main 空 → 全部失效
  const chain = { router: ["light", "main"], light: ["main"], main: [] };
  // 0.9.4:enabled=false 视同悬空——与后端 find_provider_model 一致
  const isUsable = (a) => {
    if (!a) return false;
    const provider = (cfg.providers || []).find((p) => p.id === a.provider_id);
    if (!provider) return false;
    const model = (provider.models || []).find((m) => m.id === a.model_id);
    return !!model && model.enabled !== false;
  };
  const findAssignmentDetail = (a) => {
    const provider = (cfg.providers || []).find((p) => p.id === a.provider_id);
    const model = provider && (provider.models || []).find((m) => m.id === a.model_id);
    return { provider, model };
  };
  ["router", "light", "main"].forEach((tier) => {
    const el = document.getElementById(`ai-tier-${tier}-degrade`);
    if (!el) return;
    const assign = cfg[`tier_${tier}`];
    if (assign) {
      const { provider, model } = findAssignmentDetail(assign);
      if (!provider || !model) {
        // 悬空:model / provider 被删过
        el.textContent = t("ai.tier.no_provider");
        el.className = "ai-tier-degrade error";
      } else if (model.enabled === false) {
        // 0.9.4:model 存在但被禁用——最容易被用户忽视的坑,单独 warn
        el.textContent = t("ai.tier.model_disabled_warn", { model: model.id });
        el.className = "ai-tier-degrade error";
      } else {
        el.textContent = "";
        el.className = "ai-tier-degrade";
      }
      return;
    }
    // 空档 —— 找降级目标(必须是可用的,即启用且非悬空)
    let target = null;
    for (const next of chain[tier]) {
      const a = cfg[`tier_${next}`];
      if (!isUsable(a)) continue;
      const provider = (cfg.providers || []).find((p) => p.id === a.provider_id);
      target = { tier: next, label: provider.display_name };
      break;
    }
    if (target) {
      el.textContent = t("ai.tier.degrade_to", { tier: t(`ai.tier.${target.tier}`) });
      el.className = "ai-tier-degrade warning";
    } else {
      // 主档也空 → 整个 AI 意图辅助不生效
      el.textContent = tier === "main" && cfg.enabled ? t("ai.tier.no_provider") : "";
      el.className = tier === "main" && cfg.enabled ? "ai-tier-degrade error" : "ai-tier-degrade";
    }
  });
}

function renderAITierBanner() {
  const banner = document.getElementById("ai-tier-banner");
  const cfg = currentAIConfig;
  if (!cfg.enabled) {
    banner.style.display = "none";
    return;
  }
  // 展开 §6.4:任一档降级/悬空/model 已禁用则显示 banner
  const hasIssue = ["router", "light", "main"].some((tier) => {
    const assign = cfg[`tier_${tier}`];
    if (!assign) return tier !== "main"; // 主档空 = 严重(也算 issue)
    const provider = (cfg.providers || []).find((p) => p.id === assign.provider_id);
    const model = provider && (provider.models || []).find((m) => m.id === assign.model_id);
    return !provider || !model || model.enabled === false;
  });
  // tier_main 悬空 或 model 被禁用 → 视同缺主档
  const mainAssign = cfg.tier_main;
  const mainMissing = !mainAssign || !(cfg.providers || []).some((p) =>
    p.id === mainAssign.provider_id &&
    (p.models || []).some((m) => m.id === mainAssign.model_id && m.enabled !== false)
  );
  if (!hasIssue && !mainMissing) {
    banner.style.display = "none";
    return;
  }
  banner.style.display = "block";
  banner.textContent = mainMissing
    ? t("ai.tier.no_provider").replace(/^→\s*/, "⚠️ ")
    : "⚠️ " + t("ai.tier.degrade_to", { tier: t("ai.tier.main") });
}

function bindAIEvents() {
  // 幂等——多次 loadAIConfig 时避免重复绑定
  const root = document.getElementById("ai");
  if (root.dataset.eventsBound === "1") return;
  root.dataset.eventsBound = "1";

  const $ = (id) => document.getElementById(id);

  $("ai-enabled").addEventListener("change", (e) => {
    currentAIConfig.enabled = e.target.checked;
    renderAITierBanner();
    saveAIConfig();
  });
  $("ai-allow-routing").addEventListener("change", (e) => {
    currentAIConfig.allow_intent_routing = e.target.checked;
    saveAIConfig();
  });
  $("ai-min-query-len").addEventListener("change", (e) => {
    const v = parseInt(e.target.value, 10);
    currentAIConfig.min_query_len = isNaN(v) ? 4 : Math.max(1, Math.min(20, v));
    e.target.value = currentAIConfig.min_query_len;
    saveAIConfig();
  });
  $("ai-require-whitespace").addEventListener("change", (e) => {
    currentAIConfig.require_whitespace = e.target.checked;
    saveAIConfig();
  });
  $("ai-exclude-pure-numeric").addEventListener("change", (e) => {
    currentAIConfig.exclude_pure_numeric = e.target.checked;
    saveAIConfig();
  });
  $("ai-respect-awareness-url-path").addEventListener("change", (e) => {
    currentAIConfig.respect_awareness_url_path = e.target.checked;
    saveAIConfig();
  });
  $("ai-direct-safe").addEventListener("change", (e) => {
    currentAIConfig.direct_execute_safe_actions = e.target.checked;
    saveAIConfig();
  });
  $("ai-timeout-ms").addEventListener("change", (e) => {
    const v = parseInt(e.target.value, 10);
    currentAIConfig.slo_hard_timeout_ms = isNaN(v) ? null : Math.max(500, Math.min(30000, v));
    e.target.value = currentAIConfig.slo_hard_timeout_ms ?? 2500;
    saveAIConfig();
  });

  ["router", "light", "main"].forEach((tier) => {
    $(`ai-tier-${tier}`).addEventListener("change", (e) => {
      const val = e.target.value;
      if (!val) {
        currentAIConfig[`tier_${tier}`] = null;
      } else {
        // 只切第一个 "::":provider_id 是 UUID 不含 "::",但 model_id 可能含
        // "::"(代理/网关 model 名带命名空间);indexOf 保证剩余整体归 model_id
        const sep = val.indexOf("::");
        const providerId = val.slice(0, sep);
        const modelId = val.slice(sep + 2);
        currentAIConfig[`tier_${tier}`] = { provider_id: providerId, model_id: modelId };
      }
      renderAITierDegrade();
      renderAITierBanner();
      saveAIConfig();
    });
  });

  // Modal 事件
  $("ai-modal-cancel").addEventListener("click", closeAIProviderModal);
  $("ai-modal-save").addEventListener("click", saveNewProviderFromModal);
  // 点击 overlay 空白关 modal:必须 mousedown + mouseup 都在 overlay 上才算——
  // 否则从 textarea 里划词拖出边界时,click 的 target 会被算成 overlay(mousedown
  // 与 mouseup 的最近共同祖先),导致「选个字就把 modal 关了」。
  {
    let downOnOverlay = false;
    const overlayEl = $("ai-modal-overlay");
    overlayEl.addEventListener("mousedown", (e) => {
      downOnOverlay = e.target.id === "ai-modal-overlay";
    });
    overlayEl.addEventListener("mouseup", (e) => {
      if (downOnOverlay && e.target.id === "ai-modal-overlay") {
        closeAIProviderModal();
      }
      downOnOverlay = false;
    });
  }
  // 0.9.4:Preset 由网格 tile 点击驱动(见 renderPresetGrid),不再需要 select change
  // Kind 手改:回退 preset 为 custom(避免 preset 显示与实际不一致)
  $("ai-modal-kind").addEventListener("change", () => {
    $("ai-modal-preset").value = "custom";
  });
  // Base URL 手改:同上回退
  $("ai-modal-base-url").addEventListener("input", () => {
    const bu = $("ai-modal-base-url").value.trim();
    const kind = $("ai-modal-kind").value;
    $("ai-modal-preset").value = guessPresetForProvider(kind, bu);
  });

  // 0.9.4:测试连接按钮
  $("ai-modal-test").addEventListener("click", async () => {
    const $ = (id) => document.getElementById(id);
    const btn = $("ai-modal-test");
    const resultEl = $("ai-modal-test-result");
    const kind = $("ai-modal-kind").value;
    const baseUrl = $("ai-modal-base-url").value.trim() || null;
    const apiKey = $("ai-modal-api-key").value.trim();
    const overlay = $("ai-modal-overlay");
    const providerId = overlay.dataset.editProviderId || null;

    // 没填 key 且不是编辑模式 → 提示填写
    if (!apiKey && !providerId) {
      resultEl.textContent = "请先填写 API Key";
      resultEl.className = "ai-test-result error";
      resultEl.style.display = "";
      return;
    }

    btn.classList.add("testing");
    btn.textContent = "测试中…";
    resultEl.style.display = "none";
    try {
      // 编辑模式 + 没填新 key → 后端从 CM 读已有密钥测试
      const msg = await invoke("test_ai_provider", {
        kind, baseUrl, apiKey: apiKey || "", providerId: providerId || null,
      });
      resultEl.textContent = `✅ ${msg}`;
      resultEl.className = "ai-test-result success";
      resultEl.style.display = "";
    } catch (e) {
      resultEl.textContent = `❌ ${e}`;
      resultEl.className = "ai-test-result error";
      resultEl.style.display = "";
    } finally {
      btn.classList.remove("testing");
      btn.textContent = t("ai.modal.test");
    }
  });


  // Toast
  $("ai-toast-enable").addEventListener("click", () => {
    currentAIConfig.enabled = true;
    $("ai-enabled").checked = true;
    hideAIEnableToast();
    renderAITierBanner();
    saveAIConfig();
  });
  $("ai-toast-later").addEventListener("click", hideAIEnableToast);

  // 0.9.4 Step 1:模型编辑 modal(独立于 provider modal)
  bindAIModelEditModalEvents();
}

/**
 * AI 供应商预设目录(0.9.4 升级)——按厂商展开成"协议 + base_url + 视觉信息"。
 *
 * 用户在 modal 里点网格 tile 就一键填 kind + base_url,不用去查每家 API 文档;
 * 选"自定义"清空所有字段,回归手填模式。
 *
 * **base_url = null**:该协议不需要用户填(Anthropic / Gemini 走 rig 默认)。
 * **kind 必须与后端 ProviderKind serde rename 值一致**(见 src/app/ai_config.rs)。
 * **category**:分类标签(main=国际主流 / cn=国内 / gw=网关 / local=本地)。
 * **monogram**:tile 上显示的 2-3 字母缩写。
 * **tint**:tile 图标背景色(对应 CSS data-tint 属性)。
 */
const AI_PRESET_CATALOG = {
  openai: {
    kind: "openai_compatible",
    base_url: "https://api.openai.com/v1",
    display_name_default: "OpenAI",
    monogram: "OA",
    tint: "green",
    category: "main",
  },
  anthropic: {
    kind: "anthropic_messages",
    base_url: null,
    display_name_default: "Anthropic",
    monogram: "An",
    tint: "amber",
    category: "main",
  },
  gemini: {
    kind: "gemini_generate_content",
    base_url: null,
    display_name_default: "Google Gemini",
    monogram: "Ge",
    tint: "teal",
    category: "main",
  },
  deepseek: {
    kind: "openai_compatible",
    base_url: "https://api.deepseek.com/v1",
    display_name_default: "DeepSeek",
    monogram: "深度",
    tint: "blue",
    category: "cn",
  },
  siliconflow: {
    kind: "openai_compatible",
    base_url: "https://api.siliconflow.cn/v1",
    display_name_default: "SiliconFlow",
    monogram: "硅基",
    tint: "blue",
    category: "cn",
  },
  moonshot: {
    kind: "openai_compatible",
    base_url: "https://api.moonshot.cn/v1",
    display_name_default: "Moonshot",
    monogram: "Ki",
    tint: "purple",
    category: "cn",
  },
  groq: {
    kind: "openai_compatible",
    base_url: "https://api.groq.com/openai/v1",
    display_name_default: "Groq",
    monogram: "Gq",
    tint: "orange",
    category: "gw",
  },
  openrouter: {
    kind: "openai_compatible",
    base_url: "https://openrouter.ai/api/v1",
    display_name_default: "OpenRouter",
    monogram: "OR",
    tint: "purple",
    category: "gw",
  },
  ollama: {
    kind: "openai_compatible",
    base_url: "http://localhost:11434/v1",
    display_name_default: "Ollama",
    monogram: "Ol",
    tint: "slate",
    category: "local",
  },
  custom: {
    kind: null,
    base_url: null,
    display_name_default: null,
    monogram: "+",
    tint: "ink",
    category: "custom",
  },
};

/** tile 渲染顺序(按分类分组)。 */
const AI_PRESET_ORDER = [
  "openai", "anthropic", "gemini",
  "deepseek", "siliconflow", "moonshot",
  "groq", "openrouter",
  "ollama",
  "custom",
];

/** 猜测 provider 编辑时该回填到哪个 preset。完全匹配 kind + base_url → 命中;否则回落到 "custom"。 */
function guessPresetForProvider(kind, baseUrl) {
  const bu = (baseUrl || "").trim().replace(/\/$/, "");
  for (const [key, preset] of Object.entries(AI_PRESET_CATALOG)) {
    if (key === "custom") continue;
    if (preset.kind !== kind) continue;
    const presetBu = (preset.base_url || "").replace(/\/$/, "");
    if (presetBu === bu) return key;
  }
  return "custom";
}

/**
 * 渲染预设列表(0.9.4 修正:平铺按钮布局)。
 *
 * **每个按钮**:小 tint monogram + 名称;已添加时按钮右上角 accent 小圆点。
 * **flex-wrap 自适应换行,信息密度高,一眼扫完。
 * **custom**:独占最后一行,虚线边框区分。
 *
 * **CJK monogram**(`深度`/`硅基`) 自动降字号,保持视觉一致。
 * 选中高亮,点击触发 applyAIPresetToModal。
 */
function renderPresetList(selectedKey, isEdit) {
  const list = document.getElementById("ai-preset-list");
  if (!list) return;
  // 计算"已添加":同 kind + 同 base_url 视作已配一次(guessPresetForProvider 有归一化)。
  const addedKinds = new Set(
    (currentAIConfig.providers || []).map((p) => {
      const key = guessPresetForProvider(p.kind, p.base_url);
      return key !== "custom" ? key : null;
    }).filter(Boolean),
  );
  list.innerHTML = AI_PRESET_ORDER.map((key) => {
    const preset = AI_PRESET_CATALOG[key];
    const isSelected = key === selectedKey;
    const isCustom = key === "custom";
    const name = preset.display_name_default || t("ai.modal.preset.custom");
    const monogram = preset.monogram || "?";
    // CJK 缩字号——2 个汉字比 2 个字母宽,需要小一号才不挤
    const isCJK = /[一-鿿]/.test(monogram);
    const cjkCls = isCJK ? " ai-preset-item-mono--cjk" : "";
    const customCls = isCustom ? " ai-preset-item--custom" : "";
    const selectedCls = isSelected ? " selected" : "";
    const addedAttr = addedKinds.has(key) ? ' data-added="1"' : "";
    const addedTitle = addedKinds.has(key) ? ` title="${escapeAttr(t("ai.modal.badge.added"))}"` : "";
    return `<button type="button" class="ai-preset-item${selectedCls}${customCls}" data-preset="${key}"${addedAttr}${addedTitle}>
      <span class="ai-preset-item-mono${cjkCls}" data-tint="${preset.tint}">${escapeHtml(monogram)}</span>
      <span class="ai-preset-item-name">${escapeHtml(name)}</span>
    </button>`;
  }).join("");
  list.querySelectorAll(".ai-preset-item").forEach((item) => {
    item.addEventListener("click", () => {
      const key = item.dataset.preset;
      list.querySelectorAll(".ai-preset-item").forEach((i) => i.classList.remove("selected"));
      item.classList.add("selected");
      document.getElementById("ai-modal-preset").value = key;
      applyAIPresetToModal(key, isEdit);
    });
  });
}

/**
 * 应用 preset 到 modal 字段——kind / base_url / display_name(仅新增时空 name 才填)。
 * `custom` 走特殊分支:不动 kind/base_url,让用户自己填。
 */
function applyAIPresetToModal(presetKey, isEdit) {
  const $ = (id) => document.getElementById(id);
  const preset = AI_PRESET_CATALOG[presetKey];
  if (!preset || presetKey === "custom") return;
  if (preset.kind) $("ai-modal-kind").value = preset.kind;
  $("ai-modal-base-url").value = preset.base_url || "";
  if (!isEdit && preset.display_name_default && !$("ai-modal-display-name").value.trim()) {
    $("ai-modal-display-name").value = uniqueDisplayName(preset.display_name_default);
  }
  // 清空测试结果
  const testResult = $("ai-modal-test-result");
  if (testResult) { testResult.style.display = "none"; testResult.textContent = ""; }
}

/**
 * 拉取可用模型列表(0.9.4)。
 * **不硬编码任何模型**——全部通过 API 获取。
 *
 * - OpenAI 兼容:GET {base_url}/models
 * - Gemini:GET {base_url}/v1beta/models?key={apiKey}
 * - Anthropic:不暴露模型列表,抛异常提示手动输入
 */
async function fetchAvailableModels(kind, baseUrl, apiKey) {
  if (kind === "openai_compatible") {
    const base = (baseUrl || "").trim().replace(/\/$/, "");
    if (!base) throw new Error("Base URL 不能为空");
    const urls = base.endsWith("/v1")
      ? [`${base}/models`, `${base.replace(/\/v1$/, "")}/models`]
      : [`${base}/models`, `${base}/v1/models`];
    let lastErr = null;
    for (const url of urls) {
      try {
        const resp = await fetch(url, {
          headers: { "Authorization": `Bearer ${apiKey}`, "Accept": "application/json" },
        });
        if (!resp.ok) {
          lastErr = `HTTP ${resp.status}`;
          if (resp.status === 401 || resp.status === 403) {
            throw new Error("认证失败，请检查 API Key");
          }
          continue;
        }
        const json = await resp.json();
        const models = (json.data || json.models || []).map((m) => m.id || m.name).filter(Boolean);
        if (models.length > 0) return [...new Set(models)].sort();
        lastErr = "返回空列表";
      } catch (e) {
        if (e.message.includes("认证失败")) throw e;
        lastErr = String(e);
      }
    }
    throw new Error(lastErr || "无法获取模型列表");
  }
  if (kind === "anthropic_messages") {
    // Anthropic 没有公开的模型列表端点
    throw new Error("Anthropic 不支持自动获取模型列表，请手动输入 model id");
  }
  if (kind === "gemini_generate_content") {
    const base = (baseUrl || "https://generativelanguage.googleapis.com").trim().replace(/\/$/, "");
    const url = `${base}/v1beta/models?key=${apiKey}`;
    try {
      const resp = await fetch(url, { headers: { "Accept": "application/json" } });
      if (!resp.ok) {
        if (resp.status === 401 || resp.status === 403) {
          throw new Error("认证失败，请检查 API Key");
        }
        throw new Error(`HTTP ${resp.status}`);
      }
      const json = await resp.json();
      const models = (json.models || [])
        .map((m) => (m.name || "").replace(/^models\//, ""))
        .filter((n) => n.toLowerCase().includes("gemini"));
      if (models.length > 0) return models.sort();
      throw new Error("返回空列表");
    } catch (e) {
      throw new Error(`Gemini 模型获取失败: ${e.message}`);
    }
  }
  throw new Error("未知协议");
}

/**
 * 生成不与现有 provider 重名的 display_name——重名时追加 " (2)" / " (3)" 后缀。
 *
 * 消除"添加两个 OpenAI 官方后列表全是同名卡片"的肉眼歧义。
 * 底层 UUID 独立,只是 UI 显示层加区分。
 */
function uniqueDisplayName(base) {
  const existing = new Set(
    (currentAIConfig.providers || []).map((p) => (p.display_name || "").trim()),
  );
  if (!existing.has(base)) return base;
  for (let i = 2; i < 100; i++) {
    const candidate = `${base} (${i})`;
    if (!existing.has(candidate)) return candidate;
  }
  return base; // 兜底(几乎不可能触达)
}

/**
 * 打开 AI 供应商 modal。
 *
 * @param {string} [editProviderId] 传 provider id 进入编辑模式;不传/传 undefined 进入新增模式。
 *
 * **编辑模式差异**:
 * - 标题变"编辑 AI 供应商"
 * - kind 选择行隐藏(kind 不可改;想改 kind = 删了重加)
 * - display_name / base_url / models 预填
 * - api_key 输入框留空,placeholder 提示"留空 = 保留原密钥"
 * - hint 变"填新值将覆盖旧密钥"
 * - 保存时 apiKey 空 → 跳过 save_ai_secret,仅更新 provider entry
 *
 * modal 状态通过 overlay 的 dataset.editProviderId 传递,close 时清除。
 */
function openAIProviderModal(editProviderId) {
  const $ = (id) => document.getElementById(id);
  const overlay = $("ai-modal-overlay");
  const isEdit = typeof editProviderId === "string" && editProviderId.length > 0;

  if (isEdit) {
    const p = (currentAIConfig.providers || []).find((x) => x.id === editProviderId);
    if (!p) {
      console.warn("[ai] 编辑模式找不到 provider", editProviderId);
      return;
    }
    overlay.dataset.editProviderId = editProviderId;
    $("ai-modal-title").textContent = t("ai.modal.title.edit");
    $("ai-modal-kind-row").style.display = "none";
    $("ai-modal-preset-row").style.display = "none";
    $("ai-modal-kind").value = p.kind;
    $("ai-modal-preset").value = guessPresetForProvider(p.kind, p.base_url);
    $("ai-modal-display-name").value = p.display_name || "";
    $("ai-modal-base-url").value = p.base_url || "";
    $("ai-modal-api-key").value = "";
    $("ai-modal-api-key").placeholder = t("ai.modal.api_key.ph.edit");
    $("ai-modal-api-key-hint").textContent = t("ai.modal.api_key.hint.edit");
  } else {
    delete overlay.dataset.editProviderId;
    $("ai-modal-title").textContent = t("ai.modal.title");
    $("ai-modal-kind-row").style.display = "";
    $("ai-modal-preset-row").style.display = "";
    $("ai-modal-preset").value = "openai";
    $("ai-modal-kind").value = "openai_compatible";
    $("ai-modal-display-name").value = "";
    $("ai-modal-base-url").value = "";
    $("ai-modal-api-key").value = "";
    $("ai-modal-api-key").placeholder = t("ai.modal.api_key.ph");
    $("ai-modal-api-key-hint").textContent = t("ai.modal.api_key.hint");
    renderPresetList("openai", false);
    applyAIPresetToModal("openai", false);
  }
  const testResult = $("ai-modal-test-result");
  if (testResult) { testResult.style.display = "none"; testResult.textContent = ""; testResult.className = "ai-test-result"; }
  $("ai-modal-error").textContent = "";
  overlay.style.display = "flex";
  setTimeout(() => $("ai-modal-display-name").focus(), 50);
}

/**
 * 拉取可用模型列表(Model modal 复用)。
 *
 * **两种调用路径**:
 * - 已存 provider(有 providerId + 无 apiKey):后端从 CM 读密钥,前端不接触明文
 * - 未存 provider(无 providerId + 有 apiKey):前端拿明文直接 fetch(用于新建 provider 场景;
 *   0.9.4 Step 1 之后 Model modal 只在已存 provider 上加模型,此分支保留兼容)
 *
 * **返回**:model id 数组;抛错时向上传播错误信息(前端展示为 popover 空态)。
 */
async function fetchAvailableModelsFor(kind, baseUrl, providerId) {
  if (providerId) {
    // 已保存 provider → 后端 command(密钥不出 CM)
    const models = await invoke("fetch_ai_models", { providerId, kind, baseUrl: baseUrl || null });
    return models || [];
  }
  // 未保存 provider → 前端明文(仅新建 provider 模型区被砍前的旧路径,Model modal 用不到)
  const apiKey = document.getElementById("ai-modal-api-key")?.value?.trim();
  if (!apiKey) throw new Error("请先填写 API Key");
  return await fetchAvailableModels(kind, baseUrl, apiKey);
}

function closeAIProviderModal() {
  const overlay = document.getElementById("ai-modal-overlay");
  overlay.style.display = "none";
  // 清空 Key 输入框，防止残留在 DOM
  document.getElementById("ai-modal-api-key").value = "";
  // 清编辑状态
  delete overlay.dataset.editProviderId;
}

async function saveNewProviderFromModal() {
  const $ = (id) => document.getElementById(id);
  const errEl = $("ai-modal-error");
  errEl.textContent = "";

  const overlay = $("ai-modal-overlay");
  const editingId = overlay.dataset.editProviderId || null;
  const isEdit = !!editingId;

  const kind = $("ai-modal-kind").value;
  const displayName = $("ai-modal-display-name").value.trim();
  const baseUrl = $("ai-modal-base-url").value.trim();
  const apiKey = $("ai-modal-api-key").value.trim();

  if (!displayName) {
    errEl.textContent = t("ai.modal.save.empty_display");
    return;
  }
  if (kind === "openai_compatible" && !baseUrl) {
    errEl.textContent = t("ai.modal.save.empty_base_url");
    return;
  }
  if (!isEdit && !apiKey) {
    errEl.textContent = t("ai.modal.save.empty_key");
    return;
  }

  if (isEdit) {
    await saveEditedProvider(editingId, { kind, displayName, baseUrl, apiKey, errEl });
  } else {
    await saveNewProvider({ kind, displayName, baseUrl, apiKey, errEl });
  }
}

/** 新增 provider(0.9.4 简化:models 空数组,由用户后续在卡片里逐个添加)。 */
async function saveNewProvider({ kind, displayName, baseUrl, apiKey, errEl }) {
  const providerId = (crypto.randomUUID && crypto.randomUUID()) || `p-${Date.now()}-${Math.random().toString(36).slice(2, 8)}`;

  // 1. 先写密钥到 CM —— 失败则整个操作失败
  try {
    await invoke("save_ai_secret", { providerId, secret: apiKey });
  } catch (e) {
    errEl.textContent = t("ai.error.save_failed", { err: String(e) });
    return;
  }

  // 2. 构造 provider entry(models 空——用户保存后由卡片里"+ 添加模型"引导)
  const newProvider = {
    id: providerId,
    display_name: displayName,
    kind: kind,
    base_url: baseUrl || null,
    secret_ref: `blink/${providerId}/key`,
    models: [],
    created_at: Math.floor(Date.now() / 1000),
  };

  // 3. 追加到配置 + 保存
  currentAIConfig.providers = [...(currentAIConfig.providers || []), newProvider];
  hasSecretMap.set(providerId, true);

  try {
    await saveAIConfig();
  } catch (e) {
    // 保存 config 失败 → 回滚 CM
    try {
      await invoke("delete_ai_secret", { providerId });
    } catch {}
    currentAIConfig.providers = currentAIConfig.providers.filter((p) => p.id !== providerId);
    hasSecretMap.delete(providerId);
    errEl.textContent = t("ai.error.save_failed", { err: String(e) });
    return;
  }

  // 4. 关 modal，重渲染 UI
  closeAIProviderModal();
  renderAIProviders();
  renderAITierSelects();

  // 5. 0.9.4:新增 provider 后自动展开卡片 + 引导用户添加模型
  guideAddModelForProvider(providerId);

  // 6. §5.3 严格 opt-in：如果总开关还关着，弹 toast 询问
  if (!currentAIConfig.enabled) {
    showAIEnableToast();
  }
}

/**
 * 编辑既有 provider(0.9.4 简化:models 保持不动,只更 display_name/base_url/key)。
 *
 * 与新增的差异:
 * - **kind 不变**:即使用户在 modal 里改了,也用原 provider 的 kind(kind 行已隐藏)
 * - **id 保持**:同一 provider 保 secret_ref / tier_* 引用不失效
 * - **models 保持**:模型增删改统一走独立 Model modal,provider modal 不动 models
 * - **密钥 apiKey 空 → 跳过 save_ai_secret**:保留原密钥
 */
async function saveEditedProvider(providerId, { displayName, baseUrl, apiKey, errEl }) {
  const idx = (currentAIConfig.providers || []).findIndex((p) => p.id === providerId);
  if (idx < 0) {
    errEl.textContent = t("ai.error.save_failed", { err: "provider not found" });
    return;
  }
  const old = currentAIConfig.providers[idx];

  // 1. 若填了新密钥 → 覆写 CM
  const changingKey = apiKey.length > 0;
  if (changingKey) {
    try {
      await invoke("save_ai_secret", { providerId, secret: apiKey });
    } catch (e) {
      errEl.textContent = t("ai.error.save_failed", { err: String(e) });
      return;
    }
  }

  // 2. 构造更新后的 provider entry(kind + id + created_at + models 全保持)
  const updated = {
    ...old,
    display_name: displayName,
    base_url: baseUrl || null,
  };
  currentAIConfig.providers = [
    ...currentAIConfig.providers.slice(0, idx),
    updated,
    ...currentAIConfig.providers.slice(idx + 1),
  ];
  if (changingKey) hasSecretMap.set(providerId, true);

  // 3. 保存到后端 —— 触发 registry.reload 增量热更新
  try {
    await saveAIConfig();
  } catch (e) {
    // 回退元数据(密钥若已覆写就保留新的——旧明文不在手上无法恢复)
    currentAIConfig.providers[idx] = old;
    errEl.textContent = t("ai.error.save_failed", { err: String(e) });
    return;
  }

  // 4. 检查 tier 引用是否受影响(model 未动,不会新增悬空)
  closeAIProviderModal();
  renderAIProviders();
  renderAITierSelects();
  renderAITierBanner();
}

async function deleteAIProvider(providerId) {
  const provider = (currentAIConfig.providers || []).find((p) => p.id === providerId);
  if (!provider) return;

  // §6.4 UX:删除前提示 tier 引用 + 附带 model 数量,避免误删多模型 provider
  const referenced = [];
  ["router", "light", "main"].forEach((tier) => {
    const a = currentAIConfig[`tier_${tier}`];
    if (a && a.provider_id === providerId) referenced.push(t(`ai.tier.${tier}`));
  });
  const modelCount = (provider.models || []).length;
  const providerName = provider.display_name || provider.id;
  let msg = t("ai.provider.delete.confirm", { name: providerName, models: modelCount });
  if (referenced.length > 0) {
    msg = `${t("ai.provider.referenced", { tiers: referenced.join("、") })}\n\n${msg}`;
  }
  const ok = await confirmDialog(msg, {
    title: t("ai.provider.delete"),
    kind: referenced.length > 0 ? "warning" : "info",
  });
  if (!ok) return;

  // 1. CM 密钥删除（幂等）
  try {
    await invoke("delete_ai_secret", { providerId });
  } catch (e) {
    console.error("delete_ai_secret failed:", e);
    // 继续走——config 侧也要清理
  }

  // 2. 清 config
  currentAIConfig.providers = (currentAIConfig.providers || []).filter((p) => p.id !== providerId);
  hasSecretMap.delete(providerId);
  ["router", "light", "main"].forEach((tier) => {
    const a = currentAIConfig[`tier_${tier}`];
    if (a && a.provider_id === providerId) {
      currentAIConfig[`tier_${tier}`] = null;
    }
  });

  // 3. §5.3：删除后若已无 provider，自动关总开关
  if (currentAIConfig.providers.length === 0 && currentAIConfig.enabled) {
    currentAIConfig.enabled = false;
    document.getElementById("ai-enabled").checked = false;
  }

  await saveAIConfig();
  renderAIProviders();
  renderAITierSelects();
  renderAITierBanner();
}

async function saveAIConfig() {
  try {
    await saveConfig("ai_config", currentAIConfig);
  } catch (e) {
    console.error("save ai_config failed:", e);
    throw e;
  }
}

function showAIEnableToast() {
  const toast = document.getElementById("ai-enable-toast");
  toast.style.display = "flex";
  // 8 秒后自动隐藏——如果用户不理会视为"稍后"
  clearTimeout(showAIEnableToast._t);
  showAIEnableToast._t = setTimeout(hideAIEnableToast, 8000);
}
function hideAIEnableToast() {
  document.getElementById("ai-enable-toast").style.display = "none";
  clearTimeout(showAIEnableToast._t);
}

// 页面初始化时加载一次
loadAIConfig();
