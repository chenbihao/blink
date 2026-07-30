/**
 * 上下文感知 Tab 模块
 * 包含：剪贴板、选中文本、敏感应用配置、Context 触发规则
 */

import { invoke } from "../../tauri.js";
import { t, onLangChange } from "../../i18n/index.js";
import { iconHTML } from "../../icon.js";
import { saveConfig } from "../../config-keys.js";
import { getCurrentConfig } from "../shared/state.js";

/** 防止重复注册 onLangChange（initContextTab 可能被多次调用） */
let _langChangeRegistered = false;

/**
 * 初始化上下文感知 Tab
 * @param {Object} cfg - 初始配置
 */
export function initContextTab(cfg) {
  loadContextConfig();
  loadContextBindings();

  // 语言切换时重新渲染（所有变更都是自动保存的，重新加载不会丢失状态）
  if (!_langChangeRegistered) {
    _langChangeRegistered = true;
    onLangChange(() => {
      loadContextConfig();
      loadContextBindings();
    });
  }
}

/**
 * 加载上下文配置
 */
async function loadContextConfig() {
  const captureContainer = document.getElementById("context-container");
  const filterContainer = document.getElementById("context-filter-container");
  if (!captureContainer || !filterContainer) return;

  let cfg = { enabled: true, clipboardEnabled: true, selectionEnabled: true, sensitiveApps: [] };
  try {
    const data = await invoke("get_context_config");
    if (data) cfg = data;
  } catch (e) {
    console.error("load context config failed:", e);
  }

  // 剪贴板历史录入的初值
  let clipboardHistEnabled = true;
  try {
    const fullCfg = await invoke("get_config");
    if (fullCfg && fullCfg.clipboard) {
      clipboardHistEnabled = fullCfg.clipboard.enabled !== false;
    }
  } catch (e) {
    console.error("load clipboard enabled failed:", e);
  }

  // 本地状态（敏感应用列表）
  let sensitiveApps = [...(cfg.sensitiveApps || [])];

  // 采集卡
  captureContainer.innerHTML = renderCaptureCard(cfg, clipboardHistEnabled);

  // 过滤卡
  filterContainer.innerHTML = renderFilterCard();

  // 渲染敏感应用列表
  renderSensitiveList(filterContainer, sensitiveApps, save);

  // 绑定事件
  bindCaptureEvents(captureContainer, save, saveClipboardEnabled);
  bindFilterEvents(filterContainer, sensitiveApps, save);

  // 自动保存函数
  async function save() {
    const enabled = captureContainer.querySelector(".context-enabled").checked;
    const clipboardEnabled = captureContainer.querySelector('.plugin-field[data-key="clipboard_enabled"]')?.checked ?? true;
    const selectionEnabled = captureContainer.querySelector('.plugin-field[data-key="selection_enabled"]')?.checked ?? true;
    try {
      await saveConfig("context_config", { enabled, clipboardEnabled, selectionEnabled, sensitiveApps: [...sensitiveApps] });
    } catch (e) {
      console.error("save context config failed:", e);
    }
  }

  async function saveClipboardEnabled() {
    const enabled = captureContainer.querySelector("#clipboard-enabled")?.checked !== false;
    try {
      await saveConfig("clipboard_enabled", enabled);
      const currentConfig = getCurrentConfig();
      if (currentConfig?.clipboard) {
        currentConfig.clipboard.enabled = enabled;
      }
    } catch (e) {
      console.error("save clipboard enabled failed:", e);
    }
  }
}

/**
 * 渲染采集卡
 * @param {Object} cfg - 上下文配置
 * @param {boolean} clipboardHistEnabled - 剪贴板历史录入开关
 * @returns {string} HTML 字符串
 */
function renderCaptureCard(cfg, clipboardHistEnabled) {
  return `
    <div class="extension-card" data-autosave>
      <div class="extension-header">
        <div class="extension-icon">${iconHTML("spotlight")}</div>
        <div class="extension-info">
          <h3>${t("context.title")}</h3>
          <p class="extension-desc">${t("context.desc")}</p>
        </div>
        <label class="switch">
          <input type="checkbox" class="context-enabled" ${cfg.enabled ? "checked" : ""} />
          <span class="slider"></span>
        </label>
      </div>
      <div class="extension-body">
        <div class="setting-row">
          <label class="setting-label">${t("context.clipboard")}</label>
          <label class="switch switch-sm">
            <input type="checkbox" class="plugin-field" data-key="clipboard_enabled" ${cfg.clipboardEnabled ? "checked" : ""} />
            <span class="slider"></span>
          </label>
        </div>
        <div class="setting-row">
          <label class="setting-label">
            ${t("context.selection")}
            <span class="field-hint-icon" title="${t("context.selection.hint")}">ⓘ</span>
          </label>
          <label class="switch switch-sm">
            <input type="checkbox" class="plugin-field" data-key="selection_enabled" ${cfg.selectionEnabled ? "checked" : ""} />
            <span class="slider"></span>
          </label>
        </div>
        <div class="setting-row">
          <label class="setting-label">
            ${t("chord.clipboard.enabled.label")}
            <span class="field-hint-icon" title="${t("chord.clipboard.enabled.hint")}">ⓘ</span>
          </label>
          <label class="switch switch-sm">
            <input type="checkbox" id="clipboard-enabled" ${clipboardHistEnabled ? "checked" : ""} />
            <span class="slider"></span>
          </label>
        </div>
      </div>
    </div>
  `;
}

/**
 * 渲染过滤卡
 * @returns {string} HTML 字符串
 */
function renderFilterCard() {
  return `
    <div class="extension-card" data-autosave>
      <div class="extension-header">
        <div class="extension-icon">${iconHTML("shield")}</div>
        <div class="extension-info">
          <h3>${t("context.filter.title")}</h3>
          <p class="extension-desc">${t("context.filter.desc")}</p>
        </div>
      </div>
      <div class="extension-body">
        <div class="context-sensitive-list"></div>
        <div class="context-sensitive-actions">
          <button class="btn-small context-add-btn">${t("context.add_app")}</button>
        </div>
      </div>
    </div>
  `;
}

/**
 * 渲染敏感应用列表
 * @param {HTMLElement} container - 容器元素
 * @param {Array} sensitiveApps - 敏感应用列表
 * @param {Function} save - 保存函数
 */
function renderSensitiveList(container, sensitiveApps, save) {
  const listEl = container.querySelector(".context-sensitive-list");
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

  // 绑定移除事件
  listEl.querySelectorAll(".context-chip-remove").forEach((el) => {
    el.addEventListener("click", async (e) => {
      e.stopPropagation();
      const idx = parseInt(el.dataset.idx, 10);
      sensitiveApps.splice(idx, 1);
      renderSensitiveList(container, sensitiveApps, save);
      await save();
    });
  });
}

/**
 * 绑定采集卡事件
 * @param {HTMLElement} container - 容器元素
 * @param {Function} save - 保存函数
 * @param {Function} saveClipboardEnabled - 保存剪贴板历史录入函数
 */
function bindCaptureEvents(container, save, saveClipboardEnabled) {
  container.querySelector(".context-enabled")?.addEventListener("change", save);
  container.querySelector('.plugin-field[data-key="clipboard_enabled"]')?.addEventListener("change", save);
  container.querySelector('.plugin-field[data-key="selection_enabled"]')?.addEventListener("change", save);
  container.querySelector("#clipboard-enabled")?.addEventListener("change", saveClipboardEnabled);
}

/**
 * 绑定过滤卡事件
 * @param {HTMLElement} container - 容器元素
 * @param {Array} sensitiveApps - 敏感应用列表（外层持有的引用，须原地 mutate 让 save() 读到）
 * @param {Function} save - 保存函数
 */
function bindFilterEvents(container, sensitiveApps, save) {
  container.querySelector(".context-add-btn")?.addEventListener("click", async () => {
    const picked = await openProcessPicker(sensitiveApps);
    if (!picked || picked.length === 0) return;
    // 去重合入
    const seen = new Set(sensitiveApps.map((n) => n.toLowerCase()));
    for (const name of picked) {
      if (!seen.has(name.toLowerCase())) {
        sensitiveApps.push(name);
        seen.add(name.toLowerCase());
      }
    }
    renderSensitiveList(container, sensitiveApps, save);
    await save();
  });
}

/**
 * 打开进程选择器 modal，返回用户点击添加的进程名数组（取消 → []）。
 * 每次调用即时创建 DOM，关闭时移除——避免依赖 settings.html 里预置结构。
 * @param {Array<string>} existing - 当前已配置的敏感应用（用于灰化"已添加"）
 * @returns {Promise<Array<string>>}
 */
async function openProcessPicker(existing) {
  // 加载运行中的进程列表
  let processes = [];
  try {
    processes = await invoke("list_running_processes");
  } catch (e) {
    console.error("list_running_processes failed:", e);
  }

  // 按 process_name 去重（同名多窗口只留一个）
  const dedup = new Map();
  for (const p of processes) {
    const key = (p.process_name || "").toLowerCase();
    if (!key) continue;
    if (!dedup.has(key)) dedup.set(key, p);
  }
  const list = Array.from(dedup.values()).sort((a, b) =>
    a.process_name.localeCompare(b.process_name),
  );

  const existingSet = new Set((existing || []).map((n) => n.toLowerCase()));
  const added = [];

  return new Promise((resolve) => {
    const overlay = document.createElement("div");
    overlay.className = "modal-overlay";
    overlay.innerHTML = `
      <div class="modal">
        <div class="modal-header">
          <h3>${escapeHtml(t("context.modal.title"))}</h3>
          <button type="button" class="modal-close" title="×">×</button>
        </div>
        <div class="modal-search">
          <input type="text" placeholder="${escapeAttr(t("context.modal.search_ph"))}" />
        </div>
        <p class="modal-hint" style="padding:0 16px;">${escapeHtml(t("context.modal.hint"))}</p>
        <div class="modal-body"></div>
        <div class="modal-footer">
          <button type="button" class="btn-primary btn-small modal-done">${escapeHtml(t("context.modal.done"))}</button>
        </div>
      </div>
    `;
    document.body.appendChild(overlay);

    const bodyEl = overlay.querySelector(".modal-body");
    const searchInput = overlay.querySelector(".modal-search input");

    function renderList(filter) {
      const q = (filter || "").trim().toLowerCase();
      const filtered = list.filter(
        (p) =>
          !q ||
          p.process_name.toLowerCase().includes(q) ||
          (p.window_title || "").toLowerCase().includes(q),
      );
      if (filtered.length === 0) {
        bodyEl.innerHTML = `<div class="modal-item" style="cursor:default;color:var(--text-faint);">${escapeHtml(t("context.modal.empty"))}</div>`;
        return;
      }
      bodyEl.innerHTML = filtered
        .map((p) => {
          const already = existingSet.has(p.process_name.toLowerCase());
          const title = p.window_title ? ` <span style="color:var(--text-faint);font-size:11px;">${escapeHtml(p.window_title)}</span>` : "";
          const tag = already
            ? `<span style="color:var(--text-faint);font-size:11px;margin-left:auto;">${escapeHtml(t("context.modal.added"))}</span>`
            : "";
          return `<div class="modal-item" data-name="${escapeAttr(p.process_name)}" ${already ? 'style="opacity:0.5;cursor:default;"' : ""}>
            <span class="modal-item-name">${escapeHtml(p.process_name)}${title}</span>
            ${tag}
          </div>`;
        })
        .join("");
      bodyEl.querySelectorAll(".modal-item[data-name]").forEach((el) => {
        const name = el.dataset.name;
        if (existingSet.has(name.toLowerCase())) return; // 已添加，不响应
        el.addEventListener("click", () => {
          added.push(name);
          existingSet.add(name.toLowerCase());
          // 就地把该行标灰
          el.style.opacity = "0.5";
          el.style.cursor = "default";
          const tag = document.createElement("span");
          tag.style.cssText = "color:var(--text-faint);font-size:11px;margin-left:auto;";
          tag.textContent = t("context.modal.added");
          el.appendChild(tag);
          // 移除该行的 click 监听（重新替换元素）
          const clone = el.cloneNode(true);
          el.replaceWith(clone);
        });
      });
    }

    function close() {
      overlay.remove();
      resolve(added);
    }

    overlay.addEventListener("mousedown", (e) => {
      if (e.target === overlay) close();
    });
    overlay.querySelector(".modal-close").addEventListener("click", close);
    overlay.querySelector(".modal-done").addEventListener("click", close);
    searchInput.addEventListener("input", () => renderList(searchInput.value));
    document.addEventListener("keydown", function onKey(e) {
      if (e.key === "Escape" && document.body.contains(overlay)) {
        document.removeEventListener("keydown", onKey);
        close();
      }
    });

    renderList("");
    setTimeout(() => searchInput.focus(), 40);
  });
}

/**
 * 加载上下文触发规则
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

  // 渲染绑定列表
  if (bindings.length === 0) {
    container.innerHTML = `<p class="hint" style="padding: 12px 0;">${t("context.bindings.empty")}</p>`;
  } else {
    container.innerHTML = bindings.map(renderBindingRow).join("");
  }

  // 从当前渲染态推算 disabled 列表 + 同步总开关
  const collectDisabled = () =>
    Array.from(container.querySelectorAll(".context-binding-row"))
      .filter((row) => !row.querySelector(".binding-toggle")?.checked)
      .map((row) => row.dataset.key)
      .filter(Boolean);

  const syncMasterFromChildren = () => {
    // 只要还有一条启用 → 总开关亮；全禁 → 总开关灭
    const anyEnabled = Array.from(container.querySelectorAll(".binding-toggle")).some(
      (el) => el.checked,
    );
    masterToggle.checked = anyEnabled;
  };

  const persist = async (disabled) => {
    try {
      await saveConfig("disabled_context_bindings", disabled);
    } catch (e) {
      console.error("saveConfig disabled_context_bindings failed:", e);
    }
  };

  // 初始化总开关：只要有任一 binding 未禁用就点亮
  syncMasterFromChildren();

  // 单条 toggle
  container.querySelectorAll(".binding-toggle").forEach((toggle) => {
    toggle.addEventListener("change", async () => {
      syncMasterFromChildren();
      await persist(collectDisabled());
    });
  });

  // 总开关：一键启用/禁用全部（避免与既有事件叠加，先克隆掉旧监听）
  const cloned = masterToggle.cloneNode(true);
  masterToggle.replaceWith(cloned);
  cloned.addEventListener("change", async () => {
    const enabled = cloned.checked;
    container.querySelectorAll(".binding-toggle").forEach((el) => {
      el.checked = enabled;
    });
    await persist(collectDisabled());
  });
}

/**
 * 渲染单个绑定行
 *
 * 0.11.9 修复：0.9.5 重构时误引用了后端不存在的字段 b.name / b.description
 * （后端 list_context_bindings 实际返回 trigger_key + target_label），导致
 * 规则项名称渲染成 undefined、描述为空。改用后端实际字段，并通过 i18n key
 * 翻译 trigger（文案在 zh.js/en.js 的 context.trigger.* 项）。
 *
 * 复用 `.action-list-row` 紧凑单行组件（CSS 类 settings.css:844 起本就声明
 * 「Chord 动作、Context 触发规则共用」；原 .context-binding-* 类全树零定义，
 * 故不再使用）。
 *
 * @param {Object} b - 后端 binding 对象（{ key, trigger_key, target_label, enabled }）
 * @returns {string} HTML 字符串
 */
function renderBindingRow(b) {
  const triggerLabel = t(`context.trigger.${b.trigger_key}`) || b.trigger_key || "";
  const targetLabel = b.target_label || b.target_id || "";
  return `
    <div class="action-list-row context-binding-row" data-key="${escapeAttr(b.key)}">
      <div class="action-icon" aria-hidden="true">${iconHTML("ghost")}</div>
      <div class="action-info">
        <div class="action-title">${escapeHtml(triggerLabel)} → ${escapeHtml(targetLabel)}</div>
      </div>
      <label class="switch action-toggle">
        <input type="checkbox" class="binding-toggle" ${b.enabled ? "checked" : ""} />
        <span class="slider"></span>
      </label>
    </div>
  `;
}

/**
 * HTML 转义
 * @param {string} str - 原始字符串
 * @returns {string} 转义后的字符串
 */
function escapeHtml(str) {
  const div = document.createElement("div");
  div.textContent = str;
  return div.innerHTML;
}

/**
 * 属性转义
 * @param {string} str - 原始字符串
 * @returns {string} 转义后的字符串
 */
function escapeAttr(str) {
  return str.replace(/"/g, "&quot;").replace(/'/g, "&#39;");
}
