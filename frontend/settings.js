const TAU = window.__TAURI__;
const invoke = TAU?.core?.invoke ?? TAU?.invoke;

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
    tapValue.textContent = `${config.tap_threshold}ms`;
  }

  // grace period
  const graceSlider = document.getElementById("grace-ms");
  const graceValue = document.getElementById("grace-ms-value");
  if (graceSlider && config.grace_period) {
    graceSlider.value = config.grace_period;
    graceValue.textContent = `${config.grace_period}ms`;
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

  // 日志级别
  const logLevel = document.getElementById("log-level");
  if (logLevel && config.log_level) {
    logLevel.value = config.log_level;
  }

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

    const enabledEl = document.getElementById("file-search-enabled");
    const portEl = document.getElementById("everything-port");
    const depthEl = document.getElementById("local-scan-depth");

    if (enabledEl) enabledEl.checked = enabled;
    if (portEl) portEl.value = port;
    if (depthEl) depthEl.value = depth;

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
      proxyConfig = {
        http: cfg.http || "",
        https: cfg.https || "",
      };
    }
  } catch (e) {
    console.error("load proxy config failed:", e);
  }

  container.innerHTML = `
    <div class="extension-card">
      <div class="extension-header">
        <div class="extension-icon">🌐</div>
        <div class="extension-info">
          <h3>全局网络代理</h3>
          <p class="extension-desc">本体、网络插件共用，修改后需重启 Blink 生效</p>
        </div>
      </div>
      <div class="extension-body">
        <div class="plugin-config-section" style="padding-top: 0;">
          <div class="plugin-field-row">
            <div class="field-head">
              <span class="field-title">HTTP 代理</span>
              <input type="text" class="plugin-field" data-key="http_proxy" value="${escapeHtml(proxyConfig.http)}" placeholder="http://127.0.0.1:7890" />
            </div>
          </div>
          <div class="plugin-field-row">
            <div class="field-head">
              <span class="field-title">HTTPS 代理</span>
              <input type="text" class="plugin-field" data-key="https_proxy" value="${escapeHtml(proxyConfig.https)}" placeholder="http://127.0.0.1:7890" />
            </div>
          </div>
          <div class="plugin-save-row">
            <button class="btn-small network-proxy-save">保存配置</button>
            <span class="plugin-save-msg"></span>
          </div>
        </div>
      </div>
    </div>`;

  // 绑定保存事件
  const btn = document.querySelector(".network-proxy-save");
  if (!btn) return;
  btn.addEventListener("click", async () => {
    const http = document.querySelector('.plugin-field[data-key="http_proxy"]')?.value || "";
    const https = document.querySelector('.plugin-field[data-key="https_proxy"]')?.value || "";
    const msg = btn.parentElement.querySelector(".plugin-save-msg");
    try {
      await invoke("update_global_proxy", { http, https });
      if (msg) {
        msg.textContent = "已保存，下次查询自动生效";
        msg.style.color = "#a6e3a1";
      }
    } catch (e) {
      console.error("save proxy failed:", e);
      if (msg) {
        msg.textContent = "保存失败";
        msg.style.color = "#f38ba8";
      }
    }
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
    container.innerHTML = '<p style="color: #f38ba8; padding: 20px;">加载插件列表失败</p>';
    return;
  }

  if (plugins.length === 0) {
    container.innerHTML = '<p style="color: #6c7086; padding: 20px;">暂无已加载插件</p>';
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
    ? `触发: ${plugin.triggers.join(" / ")}`
    : "无触发关键词";
  const desc = plugin.description || "暂无描述";
  const enabled = plugin.enabled !== false;
  const schema = plugin.settings_schema || [];
  const settings = plugin.settings || {};
  const hasFields = schema.length > 0;

  const fieldsHtml = hasFields
    ? schema.map((f) => renderSettingField(f, settings[f.key])).join("")
    : "";

  // 配置区:有字段时分组(标题+字段+保存);否则提示无可配置项
  const configSection = hasFields
    ? `<div class="plugin-config-section">
         <div class="plugin-section-title">配置</div>
         ${fieldsHtml}
         <div class="plugin-save-row">
           <button class="btn-small plugin-save">保存配置</button>
           <span class="plugin-save-msg"></span>
         </div>
       </div>`
    : '<div class="plugin-no-config">（该插件无可配置项）</div>';

  return `
    <div class="extension-card ${enabled ? "" : "is-disabled"}" data-plugin-id="${plugin.id}">
      <div class="extension-header">
        <div class="extension-icon">${icon}</div>
        <div class="extension-info">
          <h3>${escapeHtml(plugin.name || plugin.id)}<span class="version-badge">v${escapeHtml(plugin.version || "1.0.0")}</span></h3>
          <p class="extension-desc">${escapeHtml(triggers)}</p>
        </div>
        <div class="plugin-master-toggle">
          <span class="toggle-label">${enabled ? "已启用" : "已禁用"}</span>
          <label class="switch" title="启用/禁用插件">
            <input type="checkbox" class="plugin-enabled" ${enabled ? "checked" : ""} />
            <span class="slider"></span>
          </label>
        </div>
      </div>
      <div class="extension-body">
        <div class="plugin-desc-line">${escapeHtml(desc)}</div>
        ${configSection}
      </div>
    </div>
  `;
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
      flash(card, "保存失败: " + err, true);
      return false;
    }
  };

  card.querySelector(".plugin-enabled")?.addEventListener("change", async (e) => {
    const ok = await save(e.target.checked);
    if (ok) flash(card, e.target.checked ? "已启用" : "已禁用");
    else e.target.checked = !e.target.checked; // 回滚
  });

  card.querySelector(".plugin-save")?.addEventListener("click", async () => {
    const ok = await save();
    if (ok) flash(card, "已保存");
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
  hotkeyRecordBtn.textContent = "请按下快捷键...（10秒超时）";

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
    tapValue.textContent = `${e.target.value}ms`;
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
    graceValue.textContent = `${e.target.value}ms`;
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
    try {
      await invoke("update_language", { language: e.target.value });
      if (currentConfig) currentConfig.language = e.target.value;
    } catch (err) {
      console.error("update_language failed:", err);
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
    document.getElementById("history-count").textContent = `${info.history_count} 条记录`;
    document.getElementById("db-path").textContent = info.db_path;
  } catch (e) {
    console.error("loadStorageInfo failed:", e);
  }
}

document.getElementById("clear-history")?.addEventListener("click", async () => {
  if (confirm("确定清空所有历史记录？")) {
    await invoke("clear_history");
    loadStorageInfo();
  }
});

// ── 扩展 Tab：文件搜索 ──────────────────────────────────────────────────────

async function probeEverythingStatus() {
  const statusEl = document.getElementById("everything-status");
  const portInput = document.getElementById("everything-port");
  const port = parseInt(portInput?.value || "80", 10);

  statusEl.textContent = "探测中…";
  statusEl.className = "status-badge status-unknown";

  try {
    const available = await invoke("probe_everything", { port });
    if (available) {
      statusEl.textContent = "可用 ✓";
      statusEl.className = "status-badge status-available";
    } else {
      statusEl.textContent = "不可用 ✗";
      statusEl.className = "status-badge status-unavailable";
    }
  } catch (e) {
    statusEl.textContent = "探测失败";
    statusEl.className = "status-badge status-unavailable";
    console.error("probe_everything failed:", e);
  }
}


document.getElementById("probe-everything")?.addEventListener("click", probeEverythingStatus);

document.getElementById("save-file-search")?.addEventListener("click", async () => {
  const enabled = document.getElementById("file-search-enabled").checked;
  const port = parseInt(document.getElementById("everything-port").value, 10);
  // 本地扫描深度配置暂隐藏，使用默认值 3
  const depthEl = document.getElementById("local-scan-depth");
  const depth = depthEl ? parseInt(depthEl.value, 10) : 3;

  try {
    await invoke("update_file_search", {
      enabled,
      everythingPort: port,
      localScanDepth: depth,
    });
    alert("文件搜索配置已保存");
    // 重新探测
    probeEverythingStatus();
  } catch (e) {
    console.error("update_file_search failed:", e);
    alert("保存失败: " + e);
  }
});

// ── 初始化 ───────────────────────────────────────────────────────────────────

loadConfig();
loadStorageInfo();
loadLogInfo();
loadNetworkConfig();
