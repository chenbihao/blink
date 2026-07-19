/**
 * 插件 Tab 模块
 * 包含：内置动作列表、插件管理（schema 渲染 + 触发词 + 拖拽排序）
 *
 * 插件 schema 渲染主体搬自原 settings.js 797–1560（0.9.5 拆分时被残缺重写，
 * 命令名 list_plugins 与容器 plugins-list 均不存在导致页面空白；0.9.5.1 还原原版
 * get_plugins + plugins-container + 整套 schema/触发词/拖拽）。
 */
import { invoke } from "../../tauri.js";
import { t, onLangChange } from "../../i18n/index.js";
import { iconHTML } from "../../icon.js";
import { saveConfig } from "../../config-keys.js";
import { clearUnsaved, markUnsaved } from "../shared/ui.js";

/** 内置动作图标映射（0.10.8：emoji → Lucide 图标名） */
const BUILTIN_ACTION_ICONS = {
  open_settings: "settings",
  lock: "lock",
  shutdown: "power",
  restart: "refresh-cw",
  sleep: "moon",
  clear_history: "eraser",
  exit_blink: "log-out",
  open_logs: "file-text",
  open_data_dir: "folder-open",
  open_url: "external-link",
  open_path: "folder",
  reveal_in_explorer: "folder-search",
};

/** 插件图标映射（0.10.8：emoji → Lucide 图标名；builtin.echo 保留 volume-2 声波语义） */
const PLUGIN_ICONS = {
  "builtin.ip": "globe",
  "builtin.echo": "volume-2",
  "builtin.ai": "sparkles",
  "builtin.translate": "languages",
  "builtin.weather": "cloud-sun",
};

/** 防止重复注册 onLangChange */
let _langChangeRegistered = false;

/**
 * 初始化插件 Tab
 */
export function initPluginsTab() {
  loadBuiltinActions();
  loadPlugins();
  initNumberSpinner();
  initExternalLinkDelegate();

  // 语言切换时重新渲染（toggle 状态已自动保存；插件配置需重新加载）
  if (!_langChangeRegistered) {
    _langChangeRegistered = true;
    onLangChange(() => {
      loadBuiltinActions();
      loadPlugins();
    });
  }
}

// ── 内置动作列表 ──────────────────────────────────────────────────────────────

/**
 * 加载内置动作列表
 */
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
    if (!list.dataset.eventsBound) {
      bindBuiltinActionEvents(list);
      list.dataset.eventsBound = "1";
    }
  } catch (e) {
    console.error("loadBuiltinActions failed:", e);
    list.innerHTML = `<p class="hint msg-error" style="padding: 12px 0;">${escapeHtml(String(e))}</p>`;
  }
}

/**
 * 渲染单个内置动作行
 */
function renderBuiltinActionRow(a) {
  // 0.10.8：Lucide 图标名 → iconHTML；未知 id 兜底显示占位符
  const iconName = BUILTIN_ACTION_ICONS[a.id];
  const iconMarkup = iconName ? iconHTML(iconName) : "•";
  const keywords = (a.keywords || []).join(" / ");
  const meta = [
    `<span>${t("engine.builtin_actions.keywords_label")}: ${escapeHtml(keywords)}</span>`,
    a.trigger_desc ? `<span>${escapeHtml(a.trigger_desc)}</span>` : "",
    a.param_desc
      ? `<span>${t("engine.builtin_actions.param_label")}: ${escapeHtml(a.param_desc)}</span>`
      : "",
  ].filter(Boolean).join(" · ");

  return `<div class="builtin-action-row" data-action-id="${escapeAttr(a.id)}">
    <div class="builtin-action-icon">${iconMarkup}</div>
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

/**
 * 绑定内置动作事件
 */
function bindBuiltinActionEvents(list) {
  list.addEventListener("change", async (e) => {
    if (!e.target.classList.contains("builtin-action-toggle")) return;
    const disabled = [];
    list.querySelectorAll(".builtin-action-row").forEach((row) => {
      const toggle = row.querySelector(".builtin-action-toggle");
      if (toggle && !toggle.checked) disabled.push(row.dataset.actionId);
    });

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
      loadBuiltinActions();
    }
  });
}

// ── 插件列表（搬自原 settings.js）─────────────────────────────────────────────

/**
 * 加载插件列表
 */
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

  if (!Array.isArray(plugins) || plugins.length === 0) {
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

/**
 * 渲染单个插件卡片（头部总开关 + 触发词标签嵌入描述行 + 配置区分组）
 */
function renderPluginCard(plugin) {
  // 0.10.8：Lucide 图标名 → iconHTML；未知插件用通用 zap 图标兜底
  const iconName = PLUGIN_ICONS[plugin.id] || "zap";
  const icon = iconHTML(iconName);
  const desc = plugin.description || t("plugin.desc_default");
  const enabled = plugin.enabled !== false;
  const schema = plugin.settings_schema || [];
  const settings = plugin.settings || {};
  const hasFields = schema.length > 0;

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

/**
 * 渲染触发关键字标签行（嵌入描述栏，极简设计）
 */
function renderTriggersTags(plugin) {
  const defaultTriggers = plugin.triggers || [];
  const customTriggers = plugin.custom_triggers || [];
  const disabledDefaults = plugin.disabled_default_triggers || [];
  const hasTriggers = defaultTriggers.length > 0 || customTriggers.length > 0;

  if (!hasTriggers) {
    return `<span class="plugin-triggers-row">
      <span class="trigger-label">${t("plugin.trigger_label")}</span>
      <button class="trigger-add-inline-btn" title="${t("plugin.trigger_add")}">
        ${t("plugin.trigger_add_label")}
      </button>
      <input type="text" class="trigger-add-inline-input" style="display:none;" placeholder="${t("plugin.trigger_placeholder")}" />
    </span>`;
  }

  const triggersHtml = [
    `<span class="trigger-label">${t("plugin.trigger_label")}</span>`,
    ...defaultTriggers.map((kw) => {
      const isDisabled = disabledDefaults.includes(kw);
      return `<span class="trigger-tag ${isDisabled ? "trigger-tag-disabled" : ""}" data-keyword="${escapeAttr(kw)}" data-type="default">
        <span class="trigger-tag-text">${escapeHtml(kw)}</span>
        <button class="trigger-tag-btn" title="${isDisabled ? t("plugin.trigger_restore") : t("plugin.trigger_disable")}" data-keyword="${escapeAttr(kw)}">
          ${isDisabled ? "↻" : "×"}
        </button>
      </span>`;
    }),
    ...customTriggers.map((trigger, i) => `<span class="trigger-tag trigger-tag-custom ${trigger.enabled ? "" : "trigger-tag-disabled"}" data-keyword="${escapeAttr(trigger.keyword)}" data-idx="${i}">
      <span class="trigger-tag-text">${escapeHtml(trigger.keyword)}</span>
      <button class="trigger-tag-btn trigger-tag-btn-delete" title="${t("plugin.trigger_delete")}" data-keyword="${escapeAttr(trigger.keyword)}" data-idx="${i}">
        ×
      </button>
    </span>`),
    `<button class="trigger-add-tag-btn" title="${t("plugin.trigger_add")}">+</button>
    <input type="text" class="trigger-add-inline-input" style="display:none;" placeholder="${t("plugin.trigger_placeholder")}" />`,
  ].join("");

  return `<span class="plugin-triggers-row">${triggersHtml}</span>`;
}

// ── 配置项渲染 ────────────────────────────────────────────────────────────────

/**
 * 渲染单个配置项控件
 * boolean→switch / enum→下拉 / number→带 spinner 输入 / sortable_list→可拖动列表 / string→输入框
 */
function renderSettingField(field, value, useSettingRow = false) {
  const val = value !== undefined ? value : field.default;
  let control;
  switch (field.type) {
    case "boolean":
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
      const items = Array.isArray(val) ? val : (field.default || []);
      const optionsMap = {};
      (field.options || []).forEach((o) => { optionsMap[o.value] = o.label; });
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
  const descIcon = field.description
    ? `<span class="field-hint-icon" title="${escapeAttr(field.description)}">ⓘ</span>`
    : "";

  if (useSettingRow) {
    return `
      <div class="setting-row">
        <label class="setting-label">${escapeHtml(field.title)}${descIcon}</label>
        ${control}
      </div>`;
  }

  return `
    <div class="plugin-field-row">
      <div class="field-head">
        <span class="field-title">${escapeHtml(field.title)}${descIcon}</span>
        ${control}
      </div>
    </div>`;
}

/**
 * 渲染可拖动排序列表
 */
function renderSortableList(key, items, optionsMap) {
  const listId = `sortable-${key}`;
  const itemsHtml = items.map((val) => {
    const label = optionsMap[val] || val;
    return `<div class="sortable-item" data-value="${escapeAttr(val)}" draggable="true">
      <span class="sortable-handle">⠿</span>
      <span class="sortable-label">${escapeHtml(label)}</span>
    </div>`;
  }).join("");

  const hiddenValue = JSON.stringify(items);

  return `<div class="sortable-list" id="${listId}" data-key="${key}">
    ${itemsHtml}
  </div>
  <input type="hidden" class="sortable-value plugin-field" data-key="${key}" value="${escapeAttr(hiddenValue)}" />`;
}

/**
 * 初始化可拖动列表事件（事件委托，同时支持 HTML5 drag 和鼠标 fallback；_bound 守卫只绑一次）
 */
function initSortableLists() {
  if (initSortableLists._bound) return;
  initSortableLists._bound = true;

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
    document.querySelectorAll(".sortable-item.drag-over").forEach((i) => i.classList.remove("drag-over"));
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

  // 鼠标 fallback（WebView2 drag API 有时不触发）
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

    const list = _mouseDrag.closest(".sortable-list");
    if (!list) return;
    const items = [...list.querySelectorAll(".sortable-item")];
    items.forEach((i) => i.classList.remove("drag-over"));
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
      items.forEach((i) => i.classList.remove("drag-over"));
      updateSortableValue(list);
    }

    _mouseDrag.classList.remove("dragging");
    if (_mouseClone) { _mouseClone.remove(); _mouseClone = null; }
    _mouseDrag = null;
  });
}

/**
 * 更新可拖动列表的值到隐藏 input
 * 手动派发 change 事件，让上层 markUnsaved 生效（否则 hidden input 赋值不冒泡）
 */
function updateSortableValue(list) {
  const key = list.dataset.key;
  const values = [...list.querySelectorAll(".sortable-item")].map((i) => i.dataset.value);
  let input = list.parentElement.querySelector(`input.sortable-value[data-key="${key}"]`);
  if (!input) {
    input = document.createElement("input");
    input.type = "hidden";
    input.className = "sortable-value plugin-field";
    input.dataset.key = key;
    list.parentElement.appendChild(input);
  }
  input.value = JSON.stringify(values);
  input.dispatchEvent(new Event("change", { bubbles: true }));
}

/**
 * 收集 sortable_list 的值
 */
function collectSortableValue(card, key) {
  const input = card.querySelector(`input.sortable-value[data-key="${key}"]`);
  if (input) {
    try {
      return JSON.parse(input.value);
    } catch (e) {
      console.error("Failed to parse sortable value:", e);
    }
  }
  const list = card.querySelector(`.sortable-list[data-key="${key}"]`);
  if (list) {
    return [...list.querySelectorAll(".sortable-item")].map((i) => i.dataset.value);
  }
  return [];
}

// ── 配置区公用渲染 ────────────────────────────────────────────────────────────

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
 */
function renderConfigSection(title, schema, values, opts = {}) {
  const groups = {};
  const ungrouped = [];
  for (const f of schema) {
    if (f.group) {
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

  const ungroupedHtml = ungrouped.map((f) => renderSettingField(f, values[f.key])).join("");

  const groupedHtml = Object.entries(groups).map(([, group]) => {
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

  if (opts.flat) {
    return `<div class="plugin-section-title">${escapeHtml(title)}</div>
       ${ungroupedHtml}
       ${groupedHtml}
       ${saveRow}`;
  }

  if (opts.collapsible) {
    const collapsed = opts.collapsed !== false;
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

/**
 * 绑定单个插件卡片事件（enabled 开关 + 保存 settings + 触发词增删禁用）
 */
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
    if (!ok) e.target.checked = !e.target.checked;
  });

  // 字段变更 → 挂 unsaved 徽章（提示用户点保存）
  card.querySelectorAll(".plugin-field").forEach((el) => {
    el.addEventListener("input", () => markUnsaved(card));
    el.addEventListener("change", () => markUnsaved(card));
  });

  card.querySelector(".plugin-save")?.addEventListener("click", async () => {
    const ok = await save();
    if (ok) {
      clearUnsaved(card);
      flash(card, t("plugin.saved_msg"));
    }
  });

  // 默认触发词的 ban/恢复按钮
  card.querySelectorAll(".trigger-tag-btn:not(.trigger-tag-btn-delete)").forEach((btn) => {
    btn.addEventListener("click", async (e) => {
      e.stopPropagation();
      const keyword = btn.dataset.keyword;
      const tag = btn.closest(".trigger-tag");
      if (!tag || !keyword) return;
      const isDisabled = tag.classList.contains("trigger-tag-disabled");

      const originalContent = btn.innerHTML;
      btn.innerHTML = "⋯";
      btn.disabled = true;

      try {
        await invoke("toggle_default_trigger", { pluginId: id, keyword, disabled: !isDisabled });
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
  card.querySelectorAll(".trigger-tag-btn-delete").forEach((btn) => {
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

  // 内联添加触发词
  const addBtnInline = card.querySelector(".trigger-add-tag-btn");
  const addBtnText = card.querySelector(".trigger-add-inline-btn");
  const addInputInline = card.querySelector(".trigger-add-inline-input");
  const triggersRow = card.querySelector(".plugin-triggers-row");

  [addBtnInline, addBtnText].filter(Boolean).forEach((btn) => {
    btn.addEventListener("click", () => {
      if (addInputInline) {
        addInputInline.style.display = "inline-block";
        addInputInline.focus();
        btn.style.display = "none";
      }
    });
  });

  /** 插入一个自定义触发词标签并绑删除 */
  function appendCustomTag(kw) {
    const newTag = document.createElement("span");
    newTag.className = "trigger-tag trigger-tag-custom";
    newTag.innerHTML = `
      <span class="trigger-tag-text">${escapeHtml(kw)}</span>
      <button class="trigger-tag-btn trigger-tag-btn-delete" title="${t("plugin.trigger_delete")}" data-keyword="${escapeAttr(kw)}">×</button>`;
    const addBtn = triggersRow?.querySelector(".trigger-add-tag-btn, .trigger-add-inline-btn");
    if (addBtn && triggersRow) triggersRow.insertBefore(newTag, addBtn);
    else if (triggersRow) triggersRow.appendChild(newTag);
    newTag.querySelector(".trigger-tag-btn-delete")?.addEventListener("click", async (e) => {
      e.stopPropagation();
      try {
        await invoke("delete_custom_trigger", { pluginId: id, keyword: kw });
        newTag.remove();
      } catch (err) {
        console.error("delete_custom_trigger failed:", err);
      }
    });
  }

  async function commitAdd(inputEl) {
    const kw = (inputEl.value || "").trim();
    if (!kw) {
      inputEl.style.display = "none";
      if (addBtnInline) addBtnInline.style.display = "inline-flex";
      if (addBtnText) addBtnText.style.display = "inline-block";
      return;
    }
    try {
      await invoke("add_custom_trigger", { pluginId: id, keyword: kw });
      appendCustomTag(kw);
    } catch (err) {
      console.error("add_custom_trigger failed:", err);
    }
    inputEl.value = "";
    inputEl.style.display = "none";
    if (addBtnInline) addBtnInline.style.display = "inline-flex";
    if (addBtnText) addBtnText.style.display = "inline-block";
  }

  addInputInline?.addEventListener("keydown", async (e) => {
    if (e.key === "Enter") {
      e.preventDefault();
      await commitAdd(e.target);
    } else if (e.key === "Escape") {
      e.target.value = "";
      e.target.blur();
    }
  });

  addInputInline?.addEventListener("blur", async (e) => {
    await commitAdd(e.target);
  });
}

/**
 * 从卡片控件收集 settings 对象（按 schema type 转换类型）
 */
function collectSettings(card, schema) {
  const settings = {};
  for (const f of schema) {
    if (f.type === "sortable_list") {
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
      default:
        settings[f.key] = el.value;
    }
  }
  return settings;
}

// ── 全局事件委托（数字 spinner / 外链）─────────────────────────────────────────

/**
 * 数字输入框增减按钮（事件委托，支持动态生成的插件数字输入；_bound 守卫）
 */
function initNumberSpinner() {
  if (initNumberSpinner._bound) return;
  initNumberSpinner._bound = true;
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

    const stepStr = input.step || "1";
    const decimals = stepStr.includes(".") ? stepStr.split(".")[1].length : 0;

    if (btn.classList.contains("spinner-up")) {
      value = Math.min(value + step, isNaN(max) ? Infinity : max);
    } else {
      value = Math.max(value - step, isNaN(min) ? -Infinity : min);
    }

    input.value = decimals > 0 ? value.toFixed(decimals) : value;
    input.dispatchEvent(new Event("change", { bubbles: true }));
  });
}

/**
 * 外链点击委托（替代原 onclick="openExternalUrl(...)"——ES module 下内联 onclick 取不到模块作用域函数）
 */
function initExternalLinkDelegate() {
  if (initExternalLinkDelegate._bound) return;
  initExternalLinkDelegate._bound = true;
  document.addEventListener("click", (e) => {
    const link = e.target.closest(".external-link");
    if (!link) return;
    e.preventDefault();
    openExternalUrl(link.dataset.url);
  });
}

// ── helper ────────────────────────────────────────────────────────────────────

/** HTML 转义（防 settings/title 注入） */
function escapeHtml(s) {
  return String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]));
}

/** 属性转义 */
function escapeAttr(s) {
  return escapeHtml(s);
}

/** 把文本中的 URL 转换成可点击链接（data-url + 事件委托打开） */
function linkify(text) {
  const escaped = escapeHtml(text);
  return escaped.replace(
    /(https?:\/\/[^\s<>"']+)/g,
    '<a href="#" class="external-link" data-url="$1">$1</a>',
  );
}

/** 在外部浏览器打开 URL */
async function openExternalUrl(url) {
  try {
    await invoke("open_url", { url });
  } catch (e) {
    console.error("openExternalUrl failed:", e);
    window.open(url, "_blank");
  }
}

/** 卡片内显示一行反馈（2s 后清除） */
function flash(card, msg, isError) {
  const el = card.querySelector(".plugin-save-msg");
  if (!el) return;
  el.textContent = msg;
  el.className = `plugin-save-msg ${isError ? "msg-error" : "msg-success"}`;
  clearTimeout(el._t);
  el._t = setTimeout(() => { el.textContent = ""; el.className = "plugin-save-msg"; }, 2000);
}
