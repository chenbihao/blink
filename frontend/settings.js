import { invoke } from "./js/tauri.js";
import { applyTheme } from "./js/theme.js";
import { t, applyI18n, setLang } from "./js/i18n.js";

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
    hotkeyBtn.textContent = config.hotkey.display || "RightAlt";
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

  // 应用主题（设置页本身即时正确显示）
  applyTheme(config.theme || "auto");

  // 应用配置加载完成后，再加载引擎配置
  loadEngineConfig();
}

// 加载引擎配置（文件搜索等内置引擎）
async function loadEngineConfig() {
  try {
    const fileSearch = await invoke("get_engine_config", { engineId: "file_search" });
    const enabled = fileSearch.enabled !== false;
    const port = fileSearch.everything_port || 80;
    const depth = fileSearch.local_scan_depth || 3;
    const maxResults = fileSearch.max_results || 20;

    const enabledEl = document.getElementById("file-search-enabled");
    const portEl = document.getElementById("everything-port");
    const depthEl = document.getElementById("local-scan-depth");
    const maxResultsEl = document.getElementById("everything-max-results");

    if (enabledEl) enabledEl.checked = enabled;
    if (portEl) portEl.value = port;
    if (depthEl) depthEl.value = depth;
    if (maxResultsEl) maxResultsEl.value = maxResults;

    // 页面加载后自动探测一次
    setTimeout(probeEverythingStatus, 500);
  } catch (e) {
    console.error("loadEngineConfig failed:", e);
    // 降级使用默认值
    document.getElementById("everything-port").value = 80;
  }

  // 加载插件列表
  loadPlugins();
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
    body: renderConfigSection(t("network.section"), PROXY_SCHEMA, proxyConfig, { saveLabel: t("network.save") }),
  });

  // 绑定保存事件
  const btn = container.querySelector(".plugin-save");
  const msg = container.querySelector(".plugin-save-msg");
  if (!btn) return;
  btn.addEventListener("click", async () => {
    const http = container.querySelector('.plugin-field[data-key="http_proxy"]')?.value || "";
    const https = container.querySelector('.plugin-field[data-key="https_proxy"]')?.value || "";
    try {
      await invoke("update_global_proxy", { http, https });
      if (msg) { msg.textContent = t("network.saved_msg"); msg.style.color = "#a6e3a1"; }
    } catch (e) {
      console.error("save proxy failed:", e);
      if (msg) { msg.textContent = t("network.save_failed"); msg.style.color = "#f38ba8"; }
    }
  });
}

// 加载并渲染上下文配置（0.5.2：环境感知采集控制 + 敏感应用列表化选择器 + 自动保存）
async function loadContextConfig() {
  const container = document.getElementById("context-container");
  if (!container) return;

  let cfg = { enabled: true, clipboard_enabled: true, sensitive_apps: [] };
  try {
    const data = await invoke("get_context_config");
    if (data) cfg = data;
  } catch (e) {
    console.error("load context config failed:", e);
  }

  // ── 渲染卡片 ──
  const CLIPBOARD_FIELD = booleanField("clipboard_enabled", t("context.clipboard"));
  const enableSwitch = `<label class="switch"><input type="checkbox" class="context-enabled" ${cfg.enabled ? "checked" : ""} /><span class="slider"></span></label>`;

  container.innerHTML = renderExtensionCard({
    icon: "🌍",
    title: t("context.title"),
    desc: t("context.desc"),
    headerRight: enableSwitch,
    body: `<div class="plugin-config-section" style="padding-top: 0;">
        ${renderSettingField(CLIPBOARD_FIELD, cfg.clipboard_enabled)}
        <div class="plugin-field-row">
          <div class="field-head">
            <span class="field-title">${t("context.sensitive.title")}</span>
            <span class="hint">${t("context.sensitive.hint")}</span>
          </div>
          <div class="context-sensitive-list"></div>
          <button class="btn-small context-add-btn" style="margin-top:8px;">${t("context.add_app")}</button>
        </div>
        <div class="context-save-msg"></div>
      </div>`,
  });

  // 本地状态
  let sensitiveApps = [...(cfg.sensitive_apps || [])];

  // ── 渲染敏感应用列表（chip 样式 + × 移除）──
  function renderSensitiveList() {
    const listEl = container.querySelector(".context-sensitive-list");
    if (!listEl) return;
    if (sensitiveApps.length === 0) {
      listEl.innerHTML = `<div class="context-empty-hint">${t("context.empty")}</div>`;
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
  async function save() {
    const enabled = container.querySelector(".context-enabled").checked;
    const clipboard_enabled = container.querySelector('.plugin-field[data-key="clipboard_enabled"]')?.checked ?? true;
    const msg = container.querySelector(".context-save-msg");
    try {
      await invoke("update_context_config", {
        config: { enabled, clipboard_enabled, sensitive_apps: [...sensitiveApps] },
      });
      if (msg) {
        msg.textContent = t("context.auto_saved");
        msg.style.color = "#a6e3a1";
        setTimeout(() => { if (msg) msg.textContent = ""; }, 2000);
      }
    } catch (e) {
      console.error("save context config failed:", e);
      if (msg) {
        msg.textContent = t("context.save_failed");
        msg.style.color = "#f38ba8";
      }
    }
  }

  // 总开关 + 剪贴板开关 → change 自动保存
  container.querySelector(".context-enabled")?.addEventListener("change", save);
  container.querySelector('.plugin-field[data-key="clipboard_enabled"]')?.addEventListener("change", save);

  // ── 添加应用弹窗 ──
  container.querySelector(".context-add-btn")?.addEventListener("click", async () => {
    await showAddProcessModal(container, sensitiveApps, async (added) => {
      sensitiveApps.push(...added);
      // 去重
      sensitiveApps = [...new Set(sensitiveApps)];
      renderSensitiveList();
      await save();
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
        <span class="modal-hint">${t("context.modal.hint")}</span>
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
          <input type="checkbox" ${checked} ${disabled} />
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
    container.innerHTML = `<p style="color: #f38ba8; padding: 20px;">${t("plugin.load_failed")}</p>`;
    return;
  }

  if (plugins.length === 0) {
    container.innerHTML = `<p style="color: #6c7086; padding: 20px;">${t("plugin.empty")}</p>`;
    return;
  }

  container.innerHTML = plugins.map(renderPluginCard).join("");

  // 绑定每个插件卡片事件（用 data-plugin-id 定位）
  for (const plugin of plugins) {
    bindPluginCardEvents(plugin);
  }
}

// 渲染单个插件卡片（0.5.1：头部总开关 + 配置区分组；boolean 用 checkbox 区别于总开关）
function renderPluginCard(plugin) {
  const icon = PLUGIN_ICONS[plugin.id] || "🔌";
  const triggers = plugin.triggers && plugin.triggers.length > 0
    ? t("plugin.trigger", { kw: plugin.triggers.join(" / ") })
    : t("plugin.no_trigger");
  const desc = plugin.description || t("plugin.desc_default");
  const enabled = plugin.enabled !== false;
  const schema = plugin.settings_schema || [];
  const settings = plugin.settings || {};
  const hasFields = schema.length > 0;

  const configSection = hasFields
    ? renderConfigSection(t("plugin.section"), schema, settings, { saveLabel: t("plugin.save") })
    : `<div class="plugin-no-config">${t("plugin.no_config")}</div>`;

  const headerRight = `<div class="plugin-master-toggle">
      <span class="toggle-label">${enabled ? t("plugin.enabled") : t("plugin.disabled")}</span>
      <label class="switch" title="${t("plugin.toggle.title")}">
        <input type="checkbox" class="plugin-enabled" ${enabled ? "checked" : ""} />
        <span class="slider"></span>
      </label>
    </div>`;

  return renderExtensionCard({
    icon,
    title: `${escapeHtml(plugin.name || plugin.id)}<span class="version-badge">v${escapeHtml(plugin.version || "1.0.0")}</span>`,
    desc: escapeHtml(triggers),
    headerRight,
    attrs: `data-plugin-id="${plugin.id}"`,
    classes: enabled ? "" : "is-disabled",
    body: `<div class="plugin-desc-line">${escapeHtml(desc)}</div>${configSection}`,
  });
}

// 渲染单个配置项控件（boolean→checkbox 方框, enum→下拉, number/string→输入框）
function renderSettingField(field, value) {
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
    case "number":
      control = `<input type="number" class="plugin-field" data-key="${field.key}" value="${escapeAttr(val ?? "")}" ${field.min != null ? `min="${field.min}"` : ""} ${field.max != null ? `max="${field.max}"` : ""} />`;
      break;
    case "string":
    default:
      control = `<input type="text" class="plugin-field" data-key="${field.key}" value="${escapeAttr(val ?? "")}" />`;
      break;
  }
  const desc = field.description ? `<div class="field-desc">${escapeHtml(field.description)}</div>` : "";
  return `
    <div class="plugin-field-row">
      <div class="field-head">
        <span class="field-title">${escapeHtml(field.title)}</span>
        ${control}
      </div>
      ${desc}
    </div>
  `;
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
 * @returns {string} HTML
 */
function renderConfigSection(title, schema, values, opts = {}) {
  const fieldsHtml = schema.map((f) => renderSettingField(f, values[f.key])).join("");
  const saveRow = opts.saveLabel
    ? `<div class="plugin-save-row">
         <button class="btn-small plugin-save">${escapeHtml(opts.saveLabel)}</button>
         <span class="plugin-save-msg"></span>
       </div>`
    : "";
  return `<div class="plugin-config-section">
     <div class="plugin-section-title">${escapeHtml(title)}</div>
     ${fieldsHtml}
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

// 绑定单个插件卡片的事件（enabled 开关 + 保存 settings）
function bindPluginCardEvents(plugin) {
  const card = document.querySelector(`.extension-card[data-plugin-id="${plugin.id}"]`);
  if (!card) return;
  const id = plugin.id;
  const schema = plugin.settings_schema || [];

  const save = async (enabledOverride) => {
    const settings = collectSettings(card, schema);
    const enabled = enabledOverride !== undefined ? enabledOverride : card.querySelector(".plugin-enabled").checked;
    try {
      await invoke("update_plugin_config", { pluginId: id, enabled, settings });
      return true;
    } catch (err) {
      console.error("update_plugin_config failed:", err);
      flash(card, t("common.save_failed_msg", { err }), true);
      return false;
    }
  };

  card.querySelector(".plugin-enabled")?.addEventListener("change", async (e) => {
    const ok = await save(e.target.checked);
    if (ok) flash(card, e.target.checked ? t("plugin.enabled") : t("plugin.disabled"));
    else e.target.checked = !e.target.checked; // 回滚
  });

  card.querySelector(".plugin-save")?.addEventListener("click", async () => {
    const ok = await save();
    if (ok) flash(card, t("plugin.saved_msg"));
  });
}

// 从卡片控件收集 settings 对象（按 schema type 转换类型）
function collectSettings(card, schema) {
  const settings = {};
  for (const f of schema) {
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

// 卡片内显示一行反馈（2s 后清除）
function flash(card, msg, isError) {
  const el = card.querySelector(".plugin-save-msg");
  if (!el) return;
  el.textContent = msg;
  el.style.color = isError ? "#f38ba8" : "#a6e3a1";
  clearTimeout(el._t);
  el._t = setTimeout(() => { el.textContent = ""; }, 2000);
}

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
    await invoke("update_hotkey", {
      modifiers: defaultHotkey.modifiers,
      key: defaultHotkey.key,
      display: defaultHotkey.display,
    });
    hotkeyRecordBtn.textContent = "RightAlt";
    if (currentConfig) currentConfig.hotkey = defaultHotkey;
  });
}

async function startRecording() {
  hotkeyRecordBtn.disabled = true;
  hotkeyResetBtn.disabled = true;
  hotkeyRecordBtn.classList.add("recording");
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
    await invoke("update_hotkey", {
      modifiers: result.modifiers,
      key: result.key,
      display: result.display,
    });

    // 更新显示
    hotkeyRecordBtn.textContent = result.display;

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
    // 恢复原显示
    if (currentConfig?.hotkey?.display) {
      hotkeyRecordBtn.textContent = currentConfig.hotkey.display;
    } else {
      hotkeyRecordBtn.textContent = "RightAlt";
    }
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
      await invoke("update_tap_threshold", { threshold: value });
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
      await invoke("update_grace_period", { period: value });
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
      await invoke("update_auto_start", { autoStart: e.target.checked });
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
      await invoke("update_language", { language: lang });
      if (currentConfig) currentConfig.language = lang;
      // 即时切换整页语言：静态文本 + 动态渲染区 + 计量/徽章
      setLang(lang);
      applyI18n();
      loadNetworkConfig();
      loadContextConfig();
      loadPlugins();
      loadStorageInfo();
      refreshEverythingBadgeText();
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
  };
}

const themeSelect = document.getElementById("theme");
if (themeSelect) {
  themeSelect.addEventListener("change", async (e) => {
    const mode = e.target.value;
    applyTheme(mode); // 即时预览
    try {
      const g = readGeneral();
      await invoke("update_general_config", g);
      if (currentConfig) currentConfig.theme = mode;
    } catch (err) {
      console.error("update_general_config (theme) failed:", err);
    }
  });
}

const shEnabledCheckbox = document.getElementById("search-history-enabled");
if (shEnabledCheckbox) {
  shEnabledCheckbox.addEventListener("change", async (e) => {
    try {
      const g = readGeneral();
      await invoke("update_general_config", g);
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
      await invoke("update_general_config", g);
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
      await invoke("update_general_config", g);
      if (currentConfig) currentConfig.max_results = g.maxResults;
    } catch (err) {
      console.error("update_general_config (max results) failed:", err);
    }
  });
}

// ── 日志 ─────────────────────────────────────────────────────────────────────

const logLevelSelect = document.getElementById("log-level");
if (logLevelSelect) {
  logLevelSelect.addEventListener("change", async (e) => {
    try {
      await invoke("update_log_level", { level: e.target.value });
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

// ── 扩展 Tab：文件搜索 ──────────────────────────────────────────────────────

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


document.getElementById("probe-everything")?.addEventListener("click", probeEverythingStatus);

document.getElementById("save-file-search")?.addEventListener("click", async () => {
  const enabled = document.getElementById("file-search-enabled").checked;
  const port = parseInt(document.getElementById("everything-port").value, 10);
  // 本地扫描深度配置暂隐藏，使用默认值 3
  const depthEl = document.getElementById("local-scan-depth");
  const depth = depthEl ? parseInt(depthEl.value, 10) : 3;
  const maxResults = parseInt(document.getElementById("everything-max-results").value, 10) || 20;

  try {
    await invoke("update_file_search", {
      enabled,
      everythingPort: port,
      localScanDepth: depth,
      maxResults,
    });
    // 跟插件保存一致的 flash 提示样式
    const msgEl = document.getElementById("file-search-save-msg");
    if (msgEl) {
      msgEl.textContent = t("plugin.saved_msg");
      msgEl.style.color = "#a6e3a1";
      setTimeout(() => { msgEl.textContent = ""; }, 2000);
    }
    // 重新探测
    probeEverythingStatus();
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

// ── 脚本解释器探测（Phase 0.6） ─────────────────────────────────────────────

function updateInterpreterUI(type, status) {
  const statusEl = document.getElementById(`${type}-status`);
  const versionEl = document.getElementById(`${type}-version`);
  const pathEl = document.getElementById(`${type}-path`);
  const browseBtn = document.getElementById(`${type}-browse`);

  if (!statusEl) return;

  if (status.found) {
    if (status.version_ok) {
      statusEl.textContent = t("engine.status.available");
      statusEl.className = "status-badge status-available";
    } else {
      statusEl.textContent = t("engine.status.version_low");
      statusEl.className = "status-badge status-warning";
    }
    versionEl.textContent = status.version || "";
    versionEl.style.display = status.version ? "inline" : "none";
    pathEl.value = status.path || "";
  } else {
    statusEl.textContent = t("engine.status.not_found");
    statusEl.className = "status-badge status-unavailable";
    versionEl.style.display = "none";
    pathEl.value = status.error || t("engine.status.not_found");
  }

}

// 打开文件选择器选择解释器路径
async function browseInterpreter(kind) {
  try {
    const selected = await invoke("open_file_dialog", {
      title: `选择 ${kind} 可执行文件`,
      filters: [
        {
          name: "可执行文件",
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

  try {
    const status = await invoke("probe_interpreters");
    updateInterpreterUI(type, status[type]);
  } catch (e) {
    console.error(`probeInterpreter ${type} failed:`, e);
    statusEl.textContent = t("engine.status.failed");
    statusEl.className = "status-badge status-unavailable";
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
