import { invoke } from "./js/tauri.js";
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
  overlay.addEventListener("click", (e) => {
    if (e.target === overlay) close();
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
  if (confirm(t("storage.clear.confirm"))) {
    await invoke("clear_history");
    loadStorageInfo();
  }
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
    alert(t("common.save_failed_msg", { err: e }));
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
    alert(t("common.save_failed_msg", { err: e }));
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
  if (!confirm(t("debug.perf.clear.confirm"))) return;
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
